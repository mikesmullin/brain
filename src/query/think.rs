//! think.rs — retrieve, then one grounded synthesis.
//!
//! `search` finds rows; `think` reads them and writes an answer with citations
//! and an explicit list of gaps. The gaps matter more than they look: a model
//! given eight records will always produce prose, and the only way to tell a
//! complete answer from a confident fragment is to make it say which part of
//! the question its sources did not cover.

use anyhow::Result;
use serde::Serialize;

use crate::abox::render_fields;
use crate::llm::{Chat, ChatMessage, Embedder};
use crate::store::Store;

const SYSTEM: &str = "\
You answer questions from a knowledge graph. You are given the records that a \
retrieval step returned; they are your only evidence.

Rules:
1) Answer the question directly, in prose, as someone who knows the answer.
2) Cite the slug of every record you used, inline, like [Person/jdoe].
3) After the answer, on its own line, write `gaps:` followed by what the \
question asked that these records do not settle. Write `gaps: none` only when \
the records fully answer it.
4) Never state a fact that is not in the records. If the records disagree, say so.";

#[derive(Debug, Clone, Serialize)]
pub struct ThinkResult {
    pub question: String,
    pub answer: String,
    pub sources: Vec<String>,
    pub ms: u128,
}

pub async fn run(
    store: &Store,
    embedder: Option<&Embedder>,
    chat: &Chat,
    question: &str,
    limit: i64,
) -> Result<ThinkResult> {
    let t0 = std::time::Instant::now();
    let hits = super::search::run(
        store,
        embedder,
        question,
        &super::search::SearchOpts { limit, ..Default::default() },
    )
    .await?;

    let slugs: Vec<String> = hits.hits.iter().map(|h| h.slug.clone()).collect();
    let mut ctx = String::new();
    for slug in &slugs {
        if let Some((e, incoming)) = crate::store::hydrate(store, slug).await? {
            ctx.push_str(&format!("\n### {} — {}\n", e.slug(), e.label()));
            ctx.push_str(&render_fields(&e, 1500));
            for (rel, edges) in &e.relations {
                ctx.push_str(&format!(
                    "{rel}: {}\n",
                    edges.iter().map(|x| x.to.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            for l in incoming.iter().take(12) {
                ctx.push_str(&format!("<- {} {}\n", l.rel, l.from));
            }
            if !e.body.trim().is_empty() {
                ctx.push_str(&format!("{}\n", e.body.trim().chars().take(800).collect::<String>()));
            }
        }
    }
    if ctx.trim().is_empty() {
        return Ok(ThinkResult {
            question: question.to_string(),
            answer: format!("Nothing in this brain matches \"{question}\".\ngaps: the whole question — retrieval returned no records."),
            sources: vec![],
            ms: t0.elapsed().as_millis(),
        });
    }

    let answer = chat
        .complete(
            &[
                ChatMessage::system(SYSTEM),
                ChatMessage::user(format!("QUESTION: {question}\n\nRECORDS:{ctx}")),
            ],
            0.1,
        )
        .await?;

    Ok(ThinkResult {
        question: question.to_string(),
        answer,
        sources: slugs,
        ms: t0.elapsed().as_millis(),
    })
}
