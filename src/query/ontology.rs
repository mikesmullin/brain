//! ontology.rs — an agent walks typed edges through a deterministic tool.
//!
//! The model never names an entity that does not exist, because the only thing
//! it gets to write is a tool call, and every tool reads the index. It is the
//! older and more general of the two model-driven traversals; `crawl` is the
//! narrower one that gives up free-form tool choice in exchange for a typed
//! decision per hop. Keep both: `ontology` wins when the shape of the question
//! is unknown, `crawl` wins when the answer is a node or a set of nodes.

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use crate::llm::{Chat, ChatMessage};
use crate::store::Store;
use crate::tbox::TBox;

const MAX_TURNS: usize = 20;

fn system(tbox: &TBox) -> String {
    let classes: Vec<String> = tbox
        .classes
        .iter()
        .map(|(n, c)| if c.top { format!("{n}*") } else { n.clone() })
        .collect();
    let rels: Vec<String> = tbox
        .relations
        .iter()
        .map(|(n, r)| {
            format!(
                "{n}: {} -> {}",
                r.domain.clone().unwrap_or_else(|| "*".into()),
                r.range.clone().unwrap_or_else(|| "*".into())
            )
        })
        .collect();
    format!(
        "You answer questions by walking a typed knowledge graph. You cannot see the graph; \
you must call tools.

CLASSES (a * marks a good starting class): {}
RELATIONS:
{}

Reply with ONE JSON object per turn, and nothing else:
  {{\"tool\":\"search\",\"q\":\"…\",\"limit\":10}}        find entities by text
  {{\"tool\":\"get\",\"slug\":\"Class/id\"}}               read one entity + its edges
  {{\"tool\":\"paths\",\"from\":\"Class/id\",\"rel\":\"REL\",\"hops\":2}}   walk out of a node
  {{\"tool\":\"list\",\"cls\":\"Class\",\"limit\":50}}     list a class
  {{\"tool\":\"answer\",\"text\":\"…\",\"slugs\":[\"…\"]}} finish

Rules: never invent a slug — only use slugs a tool returned. `rel` and `hops` are \
optional on `paths`. Stop as soon as the evidence settles the question, and say so \
plainly in `answer`.",
        classes.join(", "),
        rels.join("\n")
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct OntologyResult {
    pub question: String,
    pub answer: String,
    pub slugs: Vec<String>,
    pub turns: usize,
    pub calls: Vec<Value>,
    pub ms: u128,
}

pub async fn run(
    store: &Store,
    tbox: &TBox,
    chat: &Chat,
    question: &str,
    max_turns: Option<usize>,
) -> Result<OntologyResult> {
    let t0 = std::time::Instant::now();
    let budget = max_turns.unwrap_or_else(|| {
        std::env::var("BRAIN_MAX_TURNS").ok().and_then(|v| v.parse().ok()).unwrap_or(MAX_TURNS)
    });
    let mut msgs = vec![
        ChatMessage::system(system(tbox)),
        ChatMessage::user(format!("QUESTION: {question}")),
    ];
    let mut calls: Vec<Value> = Vec::new();

    for turn in 1..=budget {
        let v = match chat.complete_json(&msgs).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(OntologyResult {
                    question: question.into(),
                    answer: format!("the traversal agent stopped: {e}"),
                    slugs: vec![],
                    turns: turn,
                    calls,
                    ms: t0.elapsed().as_millis(),
                })
            }
        };
        calls.push(v.clone());
        let tool = v["tool"].as_str().unwrap_or("");
        if tool == "answer" {
            let slugs: Vec<String> = v["slugs"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            return Ok(OntologyResult {
                question: question.into(),
                answer: v["text"].as_str().unwrap_or("").to_string(),
                slugs,
                turns: turn,
                calls,
                ms: t0.elapsed().as_millis(),
            });
        }
        let observation = match tool {
            "search" => {
                let q = v["q"].as_str().unwrap_or("");
                let limit = v["limit"].as_i64().unwrap_or(10).clamp(1, 50);
                let hits = store.keyword(q, limit).await?;
                let slugs: Vec<String> = hits.iter().map(|(s, _)| s.clone()).collect();
                let labels = store.labels_of(&slugs).await?;
                json!(slugs
                    .iter()
                    .map(|s| json!({ "slug": s, "label": labels.get(s).cloned().unwrap_or_default() }))
                    .collect::<Vec<_>>())
            }
            "get" => {
                let slug = v["slug"].as_str().unwrap_or("");
                match store.resolve(slug).await? {
                    None => json!({ "error": format!("{slug} does not exist") }),
                    Some(c) => match crate::store::hydrate(store, &c).await? {
                        None => json!({ "error": format!("{slug} does not exist") }),
                        Some((e, incoming)) => json!({
                            "slug": e.slug(),
                            "label": e.label(),
                            "fields": crate::abox::render_fields(&e, 1200),
                            "out": e.relations.iter().map(|(r, es)| json!({
                                "rel": r,
                                "to": es.iter().map(|x| x.to.clone()).collect::<Vec<_>>()
                            })).collect::<Vec<_>>(),
                            "in": incoming.iter().take(40).map(|l| json!({ "rel": l.rel, "from": l.from }))
                                 .collect::<Vec<_>>(),
                        }),
                    },
                }
            }
            "paths" => {
                let from = v["from"].as_str().unwrap_or("");
                let rel = v["rel"].as_str().map(|r| format!("-->|{r}|")).unwrap_or_else(|| "-->".into());
                let hops = v["hops"].as_u64().unwrap_or(1).clamp(1, 6);
                let pattern = format!("{from} *{hops}{rel} *");
                let res = super::graph::run(store, tbox, &pattern).await?;
                json!({
                    "paths": res.paths.iter().take(60).map(|p| json!({
                        "nodes": p.nodes, "via": p.via
                    })).collect::<Vec<_>>(),
                    "capped": res.capped,
                })
            }
            "list" => {
                let cls = v["cls"].as_str().unwrap_or("");
                let limit = v["limit"].as_i64().unwrap_or(50).clamp(1, 500);
                let resolved = tbox.resolve_class(cls).unwrap_or(cls);
                let rows = store.list_class(Some(resolved), limit, 0).await?;
                json!(rows
                    .into_iter()
                    .map(|(s, _, l)| json!({ "slug": s, "label": l }))
                    .collect::<Vec<_>>())
            }
            other => json!({ "error": format!("unknown tool '{other}'") }),
        };
        msgs.push(ChatMessage::assistant(serde_json::to_string(&v)?));
        msgs.push(ChatMessage::user(format!(
            "OBSERVATION:\n{}",
            serde_json::to_string(&observation)?.chars().take(12000).collect::<String>()
        )));
    }

    Ok(OntologyResult {
        question: question.into(),
        answer: format!("the traversal ran out of turns after {budget} (raise BRAIN_MAX_TURNS)"),
        slugs: vec![],
        turns: budget,
        calls,
        ms: t0.elapsed().as_millis(),
    })
}
