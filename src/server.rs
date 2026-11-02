//! server.rs — the optional long-running process.
//!
//! One server per brain, owning the connection pool and the loaded T-box, so a
//! CLI invocation becomes a unix-socket round trip instead of a fresh pool and
//! a fresh schema parse. It is optional on purpose: every command works without
//! one, because the database is now a real server of its own (`brain pg`) and
//! there is no embedded engine left to be the single owner of.
//!
//! The protocol is one JSON object per line: `{"op":"…", …}` in,
//! `{"ok":true,…}` or `{"ok":false,"error":"…"}` out. During `reindex` and
//! `export` the whole server is held, so a query waits rather than reading a
//! half-built index — a stall beats a wrong answer.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;

use crate::cli::{Args, Global};
use crate::commands::Ctx;
use crate::config::{self, Brain};
use crate::index;
use crate::query;

pub async fn run(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let sub = a.positional.first().map(|s| s.as_str()).unwrap_or("status");
    let brain = config::resolve(g.db.as_deref())?;
    match sub {
        "start" => serve(g).await,
        "stop" => {
            let v = call(&brain, json!({"op": "stop"})).await?;
            println!("{}", v["message"].as_str().unwrap_or("stopped"));
            Ok(0)
        }
        "status" => {
            match call(&brain, json!({"op": "status"})).await {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(0)
                }
                Err(_) => {
                    // No server: report the index itself, which is what the
                    // question is really about.
                    let ctx = Ctx::open(g).await?;
                    let st = ctx.store.stats().await?;
                    println!("server      not running (optional)");
                    println!("brain       {}  ({})", ctx.brain.alias, ctx.brain.db_root.display());
                    println!("pg schema   {}", ctx.brain.pg_schema);
                    println!("entities    {}", st.entities);
                    println!("edges       {}", st.links);
                    println!("classes     {}", st.classes);
                    println!("relations   {}", st.relations);
                    println!("vectors     {}", st.embedded);
                    println!("components  {}", st.components);
                    println!("indexed at  {}", if st.indexed_at.is_empty() { "never" } else { &st.indexed_at });
                    Ok(0)
                }
            }
        }
        "reindex" | "export" | "vacuum" | "components" => {
            match call(&brain, json!({"op": sub})).await {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(0)
                }
                Err(_) => {
                    // Standalone: do it here. The maintenance ops are the same
                    // work either way; the server only serializes them.
                    let ctx = Ctx::open(g).await?;
                    let v = maintenance(&ctx, sub).await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(0)
                }
            }
        }
        other => bail!("unknown `brain server` subcommand: {other} (see `brain help server`)"),
    }
}

async fn maintenance(ctx: &Ctx, op: &str) -> Result<Value> {
    Ok(match op {
        "reindex" => {
            let r = index::reindex(&ctx.brain, &ctx.tbox, &ctx.store, ctx.embedder.as_ref(), true).await?;
            index::record_disk_fingerprint(&ctx.brain, &ctx.store).await?;
            json!({"entities": r.entities, "links": r.links, "ms": r.total_ms})
        }
        "export" => {
            let (w, p) = index::export(&ctx.brain, &ctx.store, false).await?;
            json!({"written": w, "pruned": p})
        }
        "vacuum" => {
            ctx.store.vacuum().await?;
            let st = ctx.store.refresh_stats().await?;
            json!({"entities": st.entities, "links": st.links})
        }
        "components" => {
            let n = ctx.store.label_components().await?;
            ctx.store.refresh_stats().await?;
            json!({"components": n})
        }
        other => bail!("unknown maintenance op: {other}"),
    })
}

/// Send one request to a running server.
pub async fn call(brain: &Brain, req: Value) -> Result<Value> {
    let path = brain.socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("no server listening on {}", path.display()))?;
    stream.write_all(format!("{req}\n").as_bytes()).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let v: Value = serde_json::from_str(line.trim()).context("server returned non-JSON")?;
    if v["ok"].as_bool() == Some(false) {
        bail!("{}", v["error"].as_str().unwrap_or("server error"));
    }
    Ok(v)
}

struct State {
    ctx: Ctx,
    started: std::time::Instant,
}

async fn serve(g: &Global) -> Result<i32> {
    let ctx = Ctx::open(g).await?;
    let sock = ctx.brain.socket_path();
    let pidfile = ctx.brain.pid_path();
    // A stale socket from a killed server must not stop a new one.
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            bail!("a server is already listening on {}", sock.display());
        }
        std::fs::remove_file(&sock).ok();
    }
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    std::fs::write(&pidfile, std::process::id().to_string()).ok();
    let alias = ctx.brain.alias.clone();
    let st = ctx.store.stats().await?;
    eprintln!(
        "brain server  {alias}  {} entities / {} edges  socket {}",
        st.entities,
        st.links,
        sock.display()
    );
    let state = Arc::new(RwLock::new(State { ctx, started: std::time::Instant::now() }));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);

    loop {
        tokio::select! {
            _ = rx.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let state = state.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, state, tx).await {
                        eprintln!("client error: {e}");
                    }
                });
            }
        }
    }
    std::fs::remove_file(&sock).ok();
    std::fs::remove_file(&pidfile).ok();
    eprintln!("brain server stopped");
    Ok(0)
}

async fn handle(
    stream: UnixStream,
    state: Arc<RwLock<State>>,
    shutdown: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write.write_all(format!("{}\n", json!({"ok": false, "error": format!("{e}")})).as_bytes()).await?;
                continue;
            }
        };
        let op = req["op"].as_str().unwrap_or("").to_string();
        if op == "stop" {
            write
                .write_all(format!("{}\n", json!({"ok": true, "message": "stopping"})).as_bytes())
                .await?;
            let _ = shutdown.send(()).await;
            return Ok(());
        }
        let res = dispatch_op(&state, &op, &req).await;
        let out = match res {
            Ok(mut v) => {
                v["ok"] = json!(true);
                v
            }
            Err(e) => json!({"ok": false, "error": format!("{e}")}),
        };
        write.write_all(format!("{out}\n").as_bytes()).await?;
    }
    Ok(())
}

async fn dispatch_op(state: &Arc<RwLock<State>>, op: &str, req: &Value) -> Result<Value> {
    // Maintenance takes the write lock: queries wait rather than read a
    // half-built index.
    if matches!(op, "reindex" | "export" | "vacuum" | "components") {
        let s = state.write().await;
        return maintenance(&s.ctx, op).await;
    }
    let s = state.read().await;
    let ctx = &s.ctx;
    Ok(match op {
        "status" => {
            let st = ctx.store.stats().await?;
            json!({
                "pid": std::process::id(),
                "brain": ctx.brain.alias,
                "db": ctx.brain.db_root.display().to_string(),
                "pg_schema": ctx.brain.pg_schema,
                "uptime_s": s.started.elapsed().as_secs(),
                "entities": st.entities, "links": st.links,
                "classes": st.classes, "relations": st.relations,
                "vectors": st.embedded, "components": st.components,
                "indexed_at": st.indexed_at,
            })
        }
        "search" => {
            let q = req["q"].as_str().unwrap_or("");
            let r = query::search::run(
                &ctx.store,
                ctx.embedder.as_ref(),
                q,
                &query::search::SearchOpts {
                    limit: req["limit"].as_i64().unwrap_or(10),
                    snippets: req["snippets"].as_bool().unwrap_or(false),
                    ..Default::default()
                },
            )
            .await?;
            serde_json::to_value(r)?
        }
        "crawl" => {
            let q = req["q"].as_str().unwrap_or("");
            let r = query::crawl::run(
                &ctx.store,
                &ctx.tbox,
                &ctx.slot,
                Some(&ctx.chat),
                ctx.embedder.as_ref(),
                q,
                &query::crawl::CrawlOpts::default(),
            )
            .await?;
            serde_json::to_value(r)?
        }
        "graph" => serde_json::to_value(
            query::graph::run(&ctx.store, &ctx.tbox, req["pattern"].as_str().unwrap_or("")).await?,
        )?,
        "graphql" => query::graphql::run(&ctx.store, req["query"].as_str().unwrap_or("")).await?,
        "think" => serde_json::to_value(
            query::think::run(
                &ctx.store,
                ctx.embedder.as_ref(),
                &ctx.chat,
                req["q"].as_str().unwrap_or(""),
                req["limit"].as_i64().unwrap_or(8),
            )
            .await?,
        )?,
        "ontology" => serde_json::to_value(
            query::ontology::run(&ctx.store, &ctx.tbox, &ctx.chat, req["q"].as_str().unwrap_or(""), None).await?,
        )?,
        "get" => {
            let slug = req["slug"].as_str().unwrap_or("");
            match crate::store::hydrate(&ctx.store, &ctx.resolve_slug(slug).await?).await? {
                None => json!({"error": "not found"}),
                Some((e, incoming)) => json!({
                    "slug": e.slug(), "label": e.label(), "markdown": e.to_markdown(),
                    "incoming": incoming.iter().map(|l| json!({"rel": l.rel, "from": l.from})).collect::<Vec<_>>(),
                }),
            }
        }
        other => bail!("unknown op: {other}"),
    })
}
