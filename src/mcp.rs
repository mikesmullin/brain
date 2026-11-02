//! mcp.rs — Model Context Protocol over stdio.
//!
//! A thin adapter over the same query surface the CLI uses: one tool per mode,
//! the same arguments, the same results. Nothing here can see the graph except
//! through the engine, so an agent driving brain over MCP has exactly the same
//! guarantee a human at the CLI does — a slug in a result is a slug in the
//! index.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::cli::Global;
use crate::commands::Ctx;
use crate::query;

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    json!([
      {"name":"crawl","description":"Answer a natural-language question by walking the graph, one typed decision per hop. The default and usually the right choice.",
       "inputSchema":{"type":"object","properties":{"question":{"type":"string"},"kind":{"type":"string","description":"force the answer's class"}},"required":["question"]}},
      {"name":"search","description":"Hybrid keyword+vector search over entities.",
       "inputSchema":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"},"strategy":{"type":"string","enum":["hybrid","keyword","vector"]}},"required":["query"]}},
      {"name":"think","description":"Retrieve, then synthesise a grounded answer with citations and gaps.",
       "inputSchema":{"type":"object","properties":{"question":{"type":"string"},"limit":{"type":"integer"}},"required":["question"]}},
      {"name":"ontology","description":"Agentic typed traversal for multi-hop relational questions.",
       "inputSchema":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}},
      {"name":"graph","description":"Deterministic structural match. Mermaid-ish: 'Team -->|SUPPORTS| Product', '*', '**', '*N', --shortest, --undirected.",
       "inputSchema":{"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}},
      {"name":"graphql","description":"Deterministic projection from a known root: 'Person/jdoe { identity { name }, REPORTS_TO { identity { name } } }'.",
       "inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}},
      {"name":"get_entity","description":"Read one entity and its edges.",
       "inputSchema":{"type":"object","properties":{"slug":{"type":"string"},"links":{"type":"boolean"}},"required":["slug"]}},
      {"name":"put_entity","description":"Create or update one entity. Components are lowercase aliases; relations are ALL_CAPS.",
       "inputSchema":{"type":"object","properties":{"slug":{"type":"string"},"components":{"type":"object"},"relations":{"type":"object"},"dry_run":{"type":"boolean"}},"required":["slug"]}},
      {"name":"delete_entity","description":"Remove one entity.",
       "inputSchema":{"type":"object","properties":{"slug":{"type":"string"}},"required":["slug"]}},
      {"name":"schema","description":"The T-box: classes, components, relations, and the OWL axioms on them.",
       "inputSchema":{"type":"object","properties":{"format":{"type":"string","enum":["json","turtle","tsv"]}}}},
      {"name":"validate","description":"Schema + lint findings for every entity.",
       "inputSchema":{"type":"object","properties":{"errors_only":{"type":"boolean"}}}}
    ])
}

pub async fn run(g: &Global) -> Result<i32> {
    let ctx = Ctx::open(g).await?;
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                reply(&mut out, json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("{e}")}})).await?;
                continue;
            }
        };
        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or("").to_string();
        // notifications carry no id and expect no reply
        let is_notification = id.is_null() && method.starts_with("notifications/");
        let result = handle(&ctx, &method, &req["params"]).await;
        if is_notification {
            continue;
        }
        let msg = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":format!("{e}")}}),
        };
        reply(&mut out, msg).await?;
    }
    Ok(0)
}

async fn reply(out: &mut tokio::io::Stdout, v: Value) -> Result<()> {
    out.write_all(format!("{v}\n").as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

fn text(v: impl serde::Serialize) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text": serde_json::to_string_pretty(&v)?}]}))
}

async fn handle(ctx: &Ctx, method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "brain", "version": crate::cli::VERSION},
            "instructions": format!(
                "A knowledge graph of {} classes. Prefer `crawl` for a question in words; \
`graph`/`graphql` when you already know the slug or the pattern. Slugs are always Class/id.",
                ctx.tbox.classes.len())
        })),
        "tools/list" => Ok(json!({"tools": tools()})),
        "ping" => Ok(json!({})),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let a = &params["arguments"];
            match name {
                "crawl" => {
                    let r = query::crawl::run(
                        &ctx.store, &ctx.tbox, &ctx.slot, Some(&ctx.chat), ctx.embedder.as_ref(),
                        a["question"].as_str().unwrap_or(""),
                        &query::crawl::CrawlOpts {
                            kind: a["kind"].as_str().map(|s| s.to_string()),
                            ..Default::default()
                        },
                    ).await?;
                    text(json!({
                        "answer": r.answer, "slugs": r.collected, "labels": r.labels,
                        "kind": r.kind, "mode": r.mode, "steps": r.steps.len(), "ms": r.ms
                    }))
                }
                "search" => {
                    let strategy = a["strategy"].as_str().and_then(query::search::Strategy::parse)
                        .unwrap_or(query::search::Strategy::Hybrid);
                    let r = query::search::run(&ctx.store, ctx.embedder.as_ref(),
                        a["query"].as_str().unwrap_or(""),
                        &query::search::SearchOpts {
                            limit: a["limit"].as_i64().unwrap_or(10), strategy, snippets: true,
                            ..Default::default()
                        }).await?;
                    text(r)
                }
                "think" => text(query::think::run(&ctx.store, ctx.embedder.as_ref(), &ctx.chat,
                    a["question"].as_str().unwrap_or(""), a["limit"].as_i64().unwrap_or(8)).await?),
                "ontology" => text(query::ontology::run(&ctx.store, &ctx.tbox, &ctx.chat,
                    a["question"].as_str().unwrap_or(""), None).await?),
                "graph" => text(query::graph::run(&ctx.store, &ctx.tbox, a["pattern"].as_str().unwrap_or("")).await?),
                "graphql" => text(query::graphql::run(&ctx.store, a["query"].as_str().unwrap_or("")).await?),
                "get_entity" => {
                    let slug = ctx.resolve_slug(a["slug"].as_str().unwrap_or("")).await?;
                    match crate::store::hydrate(&ctx.store, &slug).await? {
                        None => text(json!({"error": "not found"})),
                        Some((e, incoming)) => text(json!({
                            "slug": e.slug(), "label": e.label(), "markdown": e.to_markdown(),
                            "incoming": if a["links"].as_bool().unwrap_or(false) {
                                json!(incoming.iter().map(|l| json!({"rel": l.rel, "from": l.from})).collect::<Vec<_>>())
                            } else { Value::Null }
                        })),
                    }
                }
                "put_entity" => {
                    let payload = json!({"entities": [{
                        "slug": a["slug"], "components": a["components"], "relations": a["relations"]
                    }]});
                    if a["dry_run"].as_bool().unwrap_or(false) {
                        // Report the class shape rather than writing, so a caller
                        // can see what a valid entity of this class looks like.
                        let s = crate::slug::parse(a["slug"].as_str().unwrap_or(""))?;
                        let shape = ctx.tbox.class_fields(&s.cls);
                        return text(json!({"dry_run": true, "class": s.cls,
                            "components": shape.iter().map(|(alias,(comp,f))| json!({
                                "alias": alias, "component": comp,
                                "fields": f.keys().cloned().collect::<Vec<_>>()
                            })).collect::<Vec<_>>()}));
                    }
                    let n = crate::convert::write_entities_json(ctx, &payload).await?;
                    text(json!({"written": n}))
                }
                "delete_entity" => {
                    let slug = ctx.resolve_slug(a["slug"].as_str().unwrap_or("")).await?;
                    let gone = ctx.store.delete(&slug).await?;
                    let s = crate::slug::parse(&slug)?;
                    std::fs::remove_file(ctx.brain.db_root.join(&s.cls).join(format!("{}.md", s.id))).ok();
                    text(json!({"deleted": gone, "slug": slug}))
                }
                "schema" => match a["format"].as_str().unwrap_or("json") {
                    "turtle" => Ok(json!({"content":[{"type":"text","text": ctx.tbox.to_turtle()}]})),
                    "tsv" => Ok(json!({"content":[{"type":"text","text": ctx.tbox.to_tsv()}]})),
                    _ => text(json!({
                        "ontology": ctx.tbox.ontology,
                        "classes": ctx.tbox.classes.keys().collect::<Vec<_>>(),
                        "components": ctx.tbox.components.iter().map(|(k,v)| json!({
                            "name": k, "fields": v.fields.keys().collect::<Vec<_>>()})).collect::<Vec<_>>(),
                        "relations": ctx.tbox.relations.iter().map(|(k,v)| json!({
                            "name": k, "domain": v.domain, "range": v.range,
                            "cardinality": v.cardinality.as_str(),
                            "transitive": v.transitive, "symmetric": v.symmetric,
                            "inverseOf": v.inverse_of, "chain": v.chain})).collect::<Vec<_>>(),
                    })),
                },
                "validate" => {
                    let f = crate::index::validate(&ctx.brain, &ctx.tbox)?;
                    let errors_only = a["errors_only"].as_bool().unwrap_or(false);
                    text(json!({
                        "errors": f.iter().filter(|x| x.severity == crate::index::Severity::Error).count(),
                        "findings": f.iter()
                            .filter(|x| !errors_only || x.severity == crate::index::Severity::Error)
                            .take(500).map(|x| format!("{x}")).collect::<Vec<_>>()
                    }))
                }
                other => anyhow::bail!("unknown tool: {other}"),
            }
        }
        other => anyhow::bail!("unknown method: {other}"),
    }
}
