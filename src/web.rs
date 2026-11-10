//! web.rs — browse the brain as a wiki, on the rustex1 stack.
//!
//! One process serves the app, watches the sources, compiles the frontend from
//! Rust to wasm and pushes the result into the browser over a single websocket.
//! The three reload tiers all apply here:
//!
//! | you edit              | what happens |
//! |-----------------------|--------------|
//! | `app/**.rs`           | cargo + wasm-bindgen, then `__M_BOOT__(t)` — no page reload, stores kept |
//! | `web/styles.css`      | the `<link>` is cache-busted in place, zero redraws |
//! | `src/**.rs` (this crate) | cargo build, then the process re-execs itself and the client reconnects |
//!
//! Everything the browser runs is Rust except the kit's two bridge files. The
//! JSON below is the entire server-side surface, and `crawl` is the default
//! mode for any search — which is the one design decision in this file that is
//! about the product rather than the plumbing.
//!
//! ## Why the handlers block
//!
//! `rustex1-devserver` is synchronous: a connection gets a thread, and a
//! handler is a plain `Fn`. The index is `tokio-postgres`, which is not. So the
//! runtime is created here, owned here, and each handler calls `block_on` from
//! its connection thread — which is legal precisely because that thread is not
//! a runtime worker. This is also why `brain web` is dispatched before the
//! CLI's runtime starts: nesting one runtime inside another is the failure this
//! layout avoids rather than works around.

use anyhow::{Context, Result};
use rustex1_devserver::{args_to_config, Config, Request, Response, Server};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::Global;
use crate::commands::Ctx;
use crate::query;

/// The dev server owns the process, so this is an entry point rather than a
/// subcommand body: it is called before any tokio runtime exists.
pub fn main(argv: Vec<String>, global: &Global) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let ctx = match rt.block_on(Ctx::open(global)) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("error: {e}");
            for cause in e.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            return 1;
        }
    };

    let root = match find_root(&argv) {
        Some(r) => r,
        None => {
            eprintln!(
                "error: `brain web` serves this repository's frontend, and could not find \
                 rustex1.json.\n\
                 run it from the brain checkout, or pass --root <path-to-checkout>."
            );
            return 1;
        }
    };
    let (config, rebuild) = args_to_config(&argv, Config::discover(&root));

    let st = rt.block_on(ctx.store.stats()).unwrap_or_default();
    println!(
        "brain web  {} — {} entities / {} edges  (pg schema {})",
        ctx.tbox.ontology, st.entities, st.links, ctx.brain.pg_schema
    );
    println!("  the search bar runs `crawl`; the selector beside it picks another mode");

    let mut server = Server::new(config);
    for (path, kind) in [
        ("/api/meta", Route::Meta),
        ("/api/entity", Route::Entity),
        ("/api/class", Route::Class),
        ("/api/query", Route::Query),
    ] {
        let ctx = ctx.clone();
        let rt = rt.clone();
        server = server.api(path, move |_state, req| {
            let ctx = ctx.clone();
            let req = clone_req(req);
            match rt.block_on(handle(&ctx, kind, &req)) {
                Ok(v) => Response::ok(v),
                Err(e) => Response::server_error(format!("{e}")),
            }
        });
    }
    if rebuild {
        server = server.rebuild_on_start();
    }
    server.run()
}

/// `rustex1.json` sits at the repository root. Look next to the running binary
/// first (`target/release/brain` → two levels up), then the working directory,
/// so the command works both from a checkout and from an installed binary that
/// still has its sources beside it.
fn find_root(argv: &[String]) -> Option<PathBuf> {
    if let Some(i) = argv.iter().position(|a| a == "--root") {
        if let Some(v) = argv.get(i + 1) {
            return Some(PathBuf::from(v));
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(exe);
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("x"));
    }
    for c in candidates {
        let mut dir = c.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if d.join("rustex1.json").is_file() {
                return Some(d);
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    None
}

#[derive(Clone, Copy)]
pub(crate) enum Route {
    Meta,
    Entity,
    Class,
    Query,
}

/// The devserver hands a borrowed request to a handler; `block_on` wants an
/// owned one it can move into the future.
///
/// `pub(crate)` because `serve.rs` answers the same four routes over a different
/// transport: the handlers are the shared half, the server is not.
pub(crate) struct Owned {
    params: BTreeMap<String, String>,
}

/// The `brain web` side: keep only the parameters the handlers read, so a
/// stray query key cannot reach a handler and be mistaken for an argument.
fn clone_req(req: &Request) -> Owned {
    let mut params = BTreeMap::new();
    for key in ["slug", "name", "offset", "limit", "q", "mode"] {
        if let Some(v) = req.param(key) {
            params.insert(key.to_string(), v);
        }
    }
    Owned { params }
}

impl Owned {
    /// The `brain serve` side: a query string already parsed and decoded.
    pub(crate) fn from_query(params: BTreeMap<String, String>) -> Owned {
        Owned { params }
    }

    fn s(&self, k: &str) -> String {
        self.params.get(k).cloned().unwrap_or_default()
    }

    fn n(&self, k: &str, default: i64) -> i64 {
        self.params.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
    }
}

/// Owned by `web`, called by both transports: `brain web` reaches it through
/// `clone_req`, `brain serve` through `Owned::from_query`.
pub(crate) async fn handle(ctx: &Ctx, route: Route, req: &Owned) -> Result<Value> {
    match route {
        Route::Meta => meta(ctx).await,
        Route::Entity => entity(ctx, &req.s("slug")).await,
        Route::Class => class(ctx, &req.s("name"), req.n("offset", 0), req.n("limit", 200)).await,
        Route::Query => {
            let mode = req.params.get("mode").cloned().unwrap_or_else(|| "crawl".into());
            query_mode(ctx, &req.s("q"), &mode, req.n("limit", 20)).await
        }
    }
}

async fn meta(ctx: &Ctx) -> Result<Value> {
    let st = ctx.store.stats().await?;
    let counts: BTreeMap<String, i64> = ctx.store.class_counts().await?.into_iter().collect();
    let classes: Vec<Value> = ctx
        .tbox
        .classes
        .iter()
        .map(|(name, c)| {
            json!({
                "name": name, "top": c.top, "parent": c.parent,
                "count": counts.get(name).copied().unwrap_or(0),
            })
        })
        .collect();
    let relations: Vec<Value> = ctx
        .tbox
        .relations
        .iter()
        .map(|(name, r)| {
            json!({
                "name": name, "domain": r.domain, "range": r.range,
                "cardinality": r.cardinality.as_str(),
                "transitive": r.transitive, "symmetric": r.symmetric,
                "inverseOf": r.inverse_of, "chain": r.chain,
            })
        })
        .collect();
    Ok(json!({
        "ontology": ctx.tbox.ontology,
        "brain": ctx.brain.alias,
        "entities": st.entities, "links": st.links, "vectors": st.embedded,
        "indexedAt": st.indexed_at,
        "classes": classes, "relations": relations,
    }))
}

async fn entity(ctx: &Ctx, slug: &str) -> Result<Value> {
    if slug.trim().is_empty() {
        return Ok(json!({"error": "no slug"}));
    }
    // A `[[wiki-link]]` carries a label, not a slug, so fall back to a label
    // lookup before reporting nothing: the two are the same door.
    let canonical = match ctx.resolve_slug(slug).await {
        Ok(c) => c,
        Err(_) => match ctx.store.by_label(slug, 1).await?.into_iter().next() {
            Some(r) => r.slug,
            None => return Ok(json!({"error": format!("no such entity: {slug}"), "slug": slug})),
        },
    };
    let Some((e, incoming)) = crate::store::hydrate(&ctx.store, &canonical).await? else {
        return Ok(json!({"error": "not found", "slug": slug}));
    };
    // One batched label lookup for every neighbour, so a hub page costs two
    // queries rather than one per link.
    let mut targets: Vec<String> = e.relations.values().flatten().map(|x| x.to.clone()).collect();
    targets.extend(incoming.iter().map(|l| l.from.clone()));
    targets.sort();
    targets.dedup();
    let labels = ctx.store.labels_of(&targets).await?;
    let lbl = |s: &str| -> String {
        labels
            .get(s)
            .cloned()
            .filter(|x| !x.trim().is_empty())
            .unwrap_or_else(|| crate::abox::pretty_slug(s.split('/').next_back().unwrap_or(s)))
    };
    let components: Vec<Value> = e
        .components
        .iter()
        .map(|(alias, f)| {
            json!({
                "alias": alias,
                "fields": f.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>()
            })
        })
        .collect();
    let out: Vec<Value> = e
        .relations
        .iter()
        .map(|(rel, edges)| {
            json!({"rel": rel, "dir": "out", "targets": edges.iter().map(|x| json!({
                "slug": x.to, "label": lbl(&x.to),
                "qualifiers": x.qualifiers.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>()
            })).collect::<Vec<_>>()})
        })
        .collect();
    let mut by_rel: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for l in &incoming {
        by_rel
            .entry(l.rel.clone())
            .or_default()
            .push(json!({"slug": l.from, "label": lbl(&l.from), "qualifiers": []}));
    }
    let inn: Vec<Value> = by_rel
        .into_iter()
        .map(|(rel, targets)| json!({"rel": rel, "dir": "in", "targets": targets}))
        .collect();
    Ok(json!({
        "slug": e.slug(), "class": e.cls, "id": e.id, "label": e.label(),
        "components": components, "out": out, "in": inn,
        "body": e.body, "markdown": e.to_markdown(),
        "component": ctx.store.component_of(&canonical).await.ok().flatten(),
    }))
}

async fn class(ctx: &Ctx, name: &str, offset: i64, limit: i64) -> Result<Value> {
    let resolved = ctx.tbox.resolve_class(name).unwrap_or(name).to_string();
    let rows = ctx.store.list_class(Some(&resolved), limit.clamp(1, 1000), offset).await?;
    Ok(json!({
        "class": resolved, "offset": offset,
        "entities": rows.into_iter().map(|(slug, _, label)| json!({"slug": slug, "label": label})).collect::<Vec<_>>(),
    }))
}

/// How many crawls may wait for the gate before new arrivals are shed to the
/// cheap path instead of joining the queue.
///
/// One walk costs up to [`query::crawl::CRAWL_DEADLINE_MS`] plus its own finalise,
/// so a queue of N means the last one waits roughly N half-minutes for an answer
/// it may not get. Past this depth the right answer is "here is the ontology
/// result now", not "hold this socket and hope" -- which is also what keeps a
/// burst of traffic from pinning connections and tasks behind a busy GPU.
pub const CRAWL_QUEUE_CAP: usize = 8;

/// Ceiling on one browser search, end to end -- queue wait, walk, ranked rows
/// and any ontology fallback included.
///
/// The walk already has its own six-second budget, but that number bounds only
/// the walk. Everything around it was unbounded: waiting for the gate had no
/// timeout at all, so a search could sit behind a stalled walker indefinitely and
/// the user saw a spinner forever. Twenty seconds is the point at which "still
/// thinking" has become "not coming back" from a reader's point of view.
///
/// Overridable with `BRAIN_SEARCH_DEADLINE_MS`, and clamped well below the 600 s
/// the old HTTP client would have tolerated.
pub const SEARCH_DEADLINE_MS: u64 = 20_000;

pub fn search_deadline() -> std::time::Duration {
    static ONCE: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        let ms = std::env::var("BRAIN_SEARCH_DEADLINE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(SEARCH_DEADLINE_MS);
        std::time::Duration::from_millis(ms.clamp(500, 120_000))
    })
}

/// Restores the queue depth however the walk ends -- returned, errored or panicked.
///
/// A plain decrement after the call would strand the count on the error paths, and
/// a permanently inflated `crawl_waiting` means the server eventually refuses every
/// crawl it is asked to run. That is the kind of fault that survives a restart-free
/// deploy and looks like a mysterious, permanent mode switch.
struct QueueGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The ranked rows, or `Null`. Best-effort by design: the rows are context for an
/// answer, and a failure here must never be the reason a search fails.
async fn rows(ctx: &Ctx, q: &str, limit: i64) -> Value {
    match query::search::run(
        &ctx.store,
        ctx.embedder.as_ref(),
        q,
        &query::search::SearchOpts { limit, snippets: true, ..Default::default() },
    )
    .await
    {
        Ok(h) => serde_json::to_value(h).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// An ontology answer, with the crawl's own account of why it is not here.
async fn ontology_payload(
    ctx: &Ctx,
    q: &str,
    fallback: Value,
    search: Value,
    hard_deadline: std::time::Instant,
) -> Result<Value> {
    // The fallback is model work too, and it runs when the model has ALREADY
    // failed to answer in time. So it gets only what the ceiling leaves, and if
    // there is nothing left the search degrades to its ranked rows -- which are
    // BM25+RRF from Postgres, need no GPU at all, and were measured at 8 ms. A
    // dead model must never be able to cost a user both the answer and the rows.
    let left = hard_deadline.saturating_duration_since(std::time::Instant::now());
    let o = match tokio::time::timeout(left, query::ontology::run(&ctx.store, &ctx.tbox, &ctx.chat, q, None)).await {
        Ok(Ok(o)) => o,
        // Timed out or failed: still deliver everything that did not need the GPU.
        _ => {
            return Ok(json!({
                "mode": "search",
                "search": search,
                "fallback": fallback,
            }))
        }
    };
    Ok(json!({
        "mode": "ontology",
        "ontology": serde_json::to_value(o)?,
        "fallback": fallback,
        "search": search,
    }))
}

/// The crawl, as JSON -- shared by `/api/query` and `/api/stream` so the two
/// cannot drift into different behaviours.
///
/// `progress` is the SSE sink; `None` gives the plain JSON request exactly the
/// walk it had before. The budget and the ontology fallback live HERE rather than
/// in the caller, because "the browser path is the one sharing a GPU" is true of
/// both endpoints, and a second call site quietly re-inheriting an unbounded walk
/// is precisely the bug this whole change is about.
pub async fn crawl_payload(
    ctx: &Ctx,
    q: &str,
    limit: i64,
    progress: Option<tokio::sync::mpsc::UnboundedSender<query::crawl::Progress>>,
) -> Result<Value> {
    // Set once, at entry, and honoured by EVERY await below -- including the
    // fallbacks. A ceiling that bounds only the walk is not a ceiling: measured
    // against a model endpoint that accepts connections and never answers, a 6 s
    // budget still took 8.05 s, because the ontology fallback ran afterwards with
    // no limit of its own. The promise this function makes to a reader watching a
    // spinner is "you will have an answer, or something, by this instant".
    let started = std::time::Instant::now();
    let hard_deadline = started + search_deadline();
    let remaining = || hard_deadline.saturating_duration_since(std::time::Instant::now());
    // ---- admission: one walk at a time ---------------------------------------
    // Shed first, then queue. Overloaded requests get the ontology answer
    // immediately rather than holding a socket for minutes: it is the same
    // graceful path a deadline takes, and `reason` says which one it was.
    //
    // The gate covers the WALK only. The ontology fallback is short and bounded
    // (a handful of turns), and the saturated path reaches it without a permit by
    // necessity -- a queue that could not answer while full would be a deadlock
    // with extra steps.
    if ctx.crawl_waiting.load(std::sync::atomic::Ordering::Relaxed) >= CRAWL_QUEUE_CAP {
        return ontology_payload(
            ctx,
            q,
            json!({
                "from": "crawl",
                "reason": "saturated",
                "queueCap": CRAWL_QUEUE_CAP,
                "budgetMs": query::crawl::deadline_ms().unwrap_or(query::crawl::CRAWL_DEADLINE_MS),
            }),
            rows(ctx, q, limit).await,
            hard_deadline,
        )
        .await;
    }
    ctx.crawl_waiting.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _queue = QueueGuard(std::sync::Arc::clone(&ctx.crawl_waiting));

    // Waiting for the gate is now bounded too. A stalled walker used to make
    // everyone behind it wait ten minutes on a request that was never coming;
    // past the deadline we answer from the ontology and say so.
    let Ok(Ok(permit)) =
        tokio::time::timeout(remaining(), std::sync::Arc::clone(&ctx.crawl_gate).acquire_owned()).await
    else {
        return ontology_payload(
            ctx,
            q,
            json!({
                "from": "crawl",
                "reason": if started.elapsed() >= search_deadline() { "queue-timeout" } else { "gate-closed" },
                "waitedMs": started.elapsed().as_millis() as u64,
                "deadlineMs": search_deadline().as_millis() as u64,
            }),
            rows(ctx, q, limit).await,
            hard_deadline,
        )
        .await;
    };

    // The walk itself, still inside the same ceiling. On expiry we release the
    // gate BEFORE answering from the ontology: holding the only permit while
    // running a fallback would make the next searcher wait for work that is
    // already known to be too slow. Dropping the walk's future also drops its
    // in-flight request, so the GPU stops paying for an answer nobody is waiting
    // for -- the cancellation the SSE stream could not otherwise provide.
    let r = match tokio::time::timeout(remaining(), query::crawl::run(
        &ctx.store,
        &ctx.tbox,
        &ctx.slot,
        Some(&ctx.chat),
        ctx.embedder.as_ref(),
        q,
        &query::crawl::CrawlOpts {
            deadline_ms: query::crawl::deadline_ms(),
            progress,
            ..Default::default()
        },
    ))
    .await
    {
        Ok(r) => r.context("crawl")?,
        Err(_) => {
            drop(permit);
            return ontology_payload(
                ctx,
                q,
                json!({
                    "from": "crawl",
                    "reason": "search-timeout",
                    "deadlineMs": search_deadline().as_millis() as u64,
                    "elapsedMs": started.elapsed().as_millis() as u64,
                }),
                rows(ctx, q, limit).await,
                hard_deadline,
            )
            .await;
        }
    };
    // The walk is done; the GPU is free for the next searcher. What remains is a
    // BM25 pass and possibly one short ontology answer, neither of which should
    // keep a queue behind us.
    drop(permit);

    let search = rows(ctx, q, limit).await;

    // Out of budget -> answer the same question the other way round, and
    // SAY that is what happened.
    //
    // `ontology` reads the T-box and reasons over the same corpus without
    // sweeping the graph one candidate at a time, so it is the useful answer for
    // exactly the questions the walk wanders on. It is gated on
    // `ended == "deadline"` alone and NOT on `no_answer` or `cancelled`: falling
    // back on a walk that finished and genuinely found nothing would dress up a
    // real miss as a success, and a cancelled walk has nobody left to answer.
    // The crawl's own partial result is kept in `fallback` so the UI can say "the
    // walk ran out of budget after N steps" rather than silently switching modes
    // behind the user -- and so the log line and the response agree.
    if r.ended == "deadline" {
        return ontology_payload(
            ctx,
            q,
            json!({
                "from": "crawl",
                "reason": "deadline",
                "budgetMs": query::crawl::deadline_ms().unwrap_or(query::crawl::CRAWL_DEADLINE_MS),
                "steps": r.steps.len(),
                "calls": r.calls,
                "ms": r.ms,
                "collected": r.collected.len(),
            }),
            search,
            hard_deadline,
        )
        .await;
    }

    Ok(json!({
        "mode": "crawl",
        "crawl": {
            "answer": r.answer,
            "slugs": r.collected, "labels": r.labels,
            "kind": r.kind, "crawl_mode": r.mode,
            "steps": r.steps.len(), "calls": r.calls, "ms": r.ms,
            "path": r.path, "ended": r.ended,
        },
        "search": search,
    }))
}

/// One endpoint, six modes.
async fn query_mode(ctx: &Ctx, q: &str, mode: &str, limit: i64) -> Result<Value> {
    if q.trim().is_empty() {
        return Ok(json!({"error": "no query"}));
    }
    let limit = limit.clamp(1, 200);
    Ok(match mode {
        "search" => {
            let r = query::search::run(
                &ctx.store,
                ctx.embedder.as_ref(),
                q,
                &query::search::SearchOpts { limit, snippets: true, ..Default::default() },
            )
            .await
            .context("search")?;
            json!({"mode": "search", "search": serde_json::to_value(r)?})
        }
        "graph" => json!({"mode": "graph", "graph": serde_json::to_value(
            query::graph::run(&ctx.store, &ctx.tbox, q).await.context("graph")?)?}),
        "graphql" => json!({"mode": "graphql", "graphql":
            query::graphql::run(&ctx.store, q).await.context("graphql")?}),
        "think" => json!({"mode": "think", "think": serde_json::to_value(
            query::think::run(&ctx.store, ctx.embedder.as_ref(), &ctx.chat, q, 8).await.context("think")?)?}),
        "ontology" => json!({"mode": "ontology", "ontology": serde_json::to_value(
            query::ontology::run(&ctx.store, &ctx.tbox, &ctx.chat, q, None).await.context("ontology")?)?}),
        _ => crawl_payload(ctx, q, limit, None).await?,
    })
}
