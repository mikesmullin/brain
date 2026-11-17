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

use anyhow::{bail, Context, Result};
use rustex1_devserver::{args_to_config, Config, Request, Response, Server, Stream};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use crate::brains;
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

    // Which brains this process serves: a `corpora.yaml` if one was named,
    // otherwise the one the CLI resolved to, anything named with `--brain`, and
    // every brain registered on this machine.
    let corpora = match crate::corpus::from_argv(&argv) {
        Some(Ok(c)) => Some(Arc::new(c)),
        Some(Err(e)) => {
            eprintln!("error: {e}");
            return 1;
        }
        None => None,
    };
    let explicit = brains::explicit(&argv);
    let set = match brains::discover(global, &explicit, corpora.as_deref()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    // One crawl at a time for the whole process, not per brain: the model back
    // end is a single GPU, so two brains walking at once would interleave for
    // no throughput and make each walk's decision count a window on someone
    // else's work.
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let waiting = Arc::new(AtomicUsize::new(0));

    let multi = Multi::new(set.default.clone());
    for brain in set.list {
        let alias = brain.alias.clone();
        // Provenance comes from the corpora entry when there is one: the repo and
        // ref are configuration, and the SHA is whatever is checked out now.
        let corpus = corpora.as_ref().and_then(|c| c.get(&alias));
        match rt.block_on(Ctx::open_brain(brain)) {
            Ok(mut ctx) => {
                ctx.crawl_gate = Arc::clone(&gate);
                ctx.crawl_waiting = Arc::clone(&waiting);
                let prov = match corpus {
                    Some(c) => Provenance::of(&ctx.brain, &c.repo, &c.git_ref),
                    None => Provenance::of(&ctx.brain, "", ""),
                };
                multi.insert(ctx, prov);
            }
            // One brain that cannot open (Postgres down, bad schema) must not
            // cost the others their pages: the dropdown just will not offer it.
            Err(e) => eprintln!("warning: brain '{alias}' unavailable: {e}"),
        }
    }
    if multi.is_empty() {
        eprintln!("error: no brain could be opened — is postgres running? (`brain pg start`)");
        return 1;
    }

    // Shared from here on. The map inside has its own lock because a corpus
    // refresh swaps brains while requests are in flight; the `Arc` itself never
    // changes.
    let multi = Arc::new(multi);

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

    for (db, ctx, prov) in multi.snapshot() {
        let st = rt.block_on(ctx.store.stats()).unwrap_or_default();
        println!(
            "brain web  {db} — {} entities / {} edges  (pg schema {}{})",
            st.entities,
            st.links,
            ctx.brain.pg_schema,
            if prov.sha.is_empty() { String::new() } else { format!(", git {}{}", prov.git_ref, prov.sha) }
        );
    }
    println!(
        "  {} brain(s) served at /<db>/…  ·  default {}  ·  the search bar runs `crawl`",
        multi.len(),
        multi.default_db()
    );
    if let Some(c) = &corpora {
        println!(
            "  corpora {}  ·  refresh: POST /api/corpus/refresh",
            c.file.display()
        );
    }

    let mut server = Server::new(config);
    for (path, kind) in [
        ("/api/meta", Route::Meta),
        ("/api/entity", Route::Entity),
        ("/api/class", Route::Class),
        ("/api/query", Route::Query),
    ] {
        let multi = Arc::clone(&multi);
        let rt = Arc::clone(&rt);
        server = server.api(path, move |_state, req| {
            let req = clone_req(req);
            match rt.block_on(answer(&multi, kind, &req)) {
                Ok(v) => Response::ok(v),
                Err(e) => Response::server_error(format!("{e}")),
            }
        });
    }
    {
        // The dropdown's inventory. One route, read once per boot: the list is
        // the server's own configuration, so it does not change while the
        // process is up.
        let multi = Arc::clone(&multi);
        let rt = Arc::clone(&rt);
        server = server.api("/api/brains", move |_state, _req| {
            Response::ok(rt.block_on(multi.list()))
        });
    }
    if let Some(corpora) = corpora.clone() {
        // The corpus webhook, on the dev server too: a refresh that can only be
        // exercised against production is a refresh nobody tests. Same handler
        // as `serve` — the auth, the single-flight gate and the report are all in
        // `corpus_refresh`.
        let multi = Arc::clone(&multi);
        let rt = Arc::clone(&rt);
        server = server.api("/api/corpus/refresh", move |_state, req| {
            let only: Vec<String> = req
                .param("alias")
                .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            let answer = rt.block_on(corpus_refresh(
                &multi,
                Some(&corpora),
                &only,
                &req.body,
                req.header("authorization"),
                req.header("x-hub-signature-256"),
            ));
            Response::new(answer.status, answer.body)
        });
    }
    {
        // The crawl's progress feed. `serve` has always had this; the dev server
        // did not, which meant the one place `web` and `serve` differ in what a
        // reader sees was also the place nobody could see while writing it — a
        // crawl here rendered "walking the graph…" with no steps, no decisions
        // and no clock, because the client fell back to the plain JSON request.
        //
        // A `raw` route rather than an `api` route because the response is not
        // one buffer: headers now, frames for as long as the walk runs. Raw
        // routes are matched after the exact `api` routes, so this cannot
        // shadow anything.
        let multi = Arc::clone(&multi);
        let rt = Arc::clone(&rt);
        server = server.raw("/api/stream", move |_state, req, out| {
            stream_answer(&multi, &rt, req, out);
        });
    }
    if rebuild {
        server = server.rebuild_on_start();
    }
    server.run()
}

/// The set of brains a wiki serves, and the one a URL without a database means.
///
/// Both transports own one of these: `brain web` (threads, `block_on`) and
/// `brain serve` (tasks). Keeping it here rather than in either server means the
/// routing rule — "one path segment names the brain" — is written once.
pub(crate) struct Multi {
    default: String,
    /// Interior mutability, because a corpus refresh replaces brains *at
    /// runtime*. A pulled corpus means a T-box that may have changed, so the
    /// `Ctx` is rebuilt rather than patched, and swapping the map entry is the
    /// commit point: a request either sees the old brain or the new one, never
    /// half of each.
    by: std::sync::RwLock<BTreeMap<String, Slot>>,
}

struct Slot {
    ctx: Arc<Ctx>,
    /// Where this brain came from. Empty for a brain with no configured corpus
    /// (a laptop checkout), because there is no repository to name.
    provenance: Provenance,
}

/// Enough to answer "which corpus am I actually serving?" — the question the
/// baked-image design could not answer, and the reason the webhook is only safe
/// with this data attached to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Provenance {
    pub repo: String,
    pub git_ref: String,
    pub sha: String,
}

impl Provenance {
    pub(crate) fn of(brain: &crate::config::Brain, repo: &str, git_ref: &str) -> Provenance {
        Provenance {
            repo: repo.to_string(),
            git_ref: git_ref.to_string(),
            sha: crate::corpus::head(&brain.project_root),
        }
    }
}

impl Multi {
    pub(crate) fn new(default: impl Into<String>) -> Multi {
        Multi { default: default.into(), by: std::sync::RwLock::new(BTreeMap::new()) }
    }

    /// Add or replace a brain. The provenance carries the corpus it came from,
    /// so a refresh that changes the SHA changes what `/api/brains` reports.
    pub(crate) fn insert(&self, ctx: Ctx, provenance: Provenance) {
        let alias = ctx.brain.alias.clone();
        if let Ok(mut by) = self.by.write() {
            by.insert(alias, Slot { ctx: Arc::new(ctx), provenance });
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by.read().map(|b| b.is_empty()).unwrap_or(true)
    }

    pub(crate) fn len(&self) -> usize {
        self.by.read().map(|b| b.len()).unwrap_or(0)
    }

    /// The brain a bare `/` means. Falls back to the first one that opened, so a
    /// named default that failed to connect does not become a dead homepage.
    pub(crate) fn default_db(&self) -> String {
        let Ok(by) = self.by.read() else { return String::new() };
        if by.contains_key(&self.default) {
            return self.default.clone();
        }
        by.keys().next().cloned().unwrap_or_default()
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.by.read().map(|b| b.keys().cloned().collect()).unwrap_or_default()
    }

    /// The brain named by a request, or the default when it names none.
    ///
    /// `Err` carries the *requested* name: an unknown database is the client's
    /// mistake, and answering it with the default's data would make a broken
    /// link look like a working one.
    pub(crate) fn ctx(&self, db: Option<&str>) -> std::result::Result<Arc<Ctx>, String> {
        let db = db
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.default_db());
        self.by
            .read()
            .ok()
            .and_then(|by| by.get(&db).map(|s| Arc::clone(&s.ctx)))
            .ok_or(db)
    }

    /// Every brain and its provenance, cloned. Used by the startup log and by a
    /// refresh, both of which must not hold the lock across an await.
    pub(crate) fn snapshot(&self) -> Vec<(String, Arc<Ctx>, Provenance)> {
        self.by
            .read()
            .map(|by| by.iter().map(|(k, s)| (k.clone(), Arc::clone(&s.ctx), s.provenance.clone())).collect())
            .unwrap_or_default()
    }

    /// `GET /api/brains`: what the selector shows.
    ///
    /// Counts are included because they are the cheapest honest signal that a
    /// brain has an index — a brain whose schema resolved but whose entities did
    /// not index reads as `0` here instead of silently answering nothing. The
    /// provenance (`repo`/`ref`/`sha`/`indexedAt`) is what makes a refresh
    /// auditable: without it, "which corpus is serving me?" has no answer.
    /// The default is first: that is the order the control should offer.
    pub(crate) async fn list(&self) -> Value {
        let default = self.default_db();
        let mut rows: Vec<(String, Value)> = Vec::new();
        for (db, ctx, prov) in self.snapshot() {
            let st = ctx.store.stats().await.unwrap_or_default();
            rows.push((
                db.clone(),
                json!({
                    "db": db,
                    "ontology": ctx.tbox.ontology,
                    "entities": st.entities,
                    "edges": st.links,
                    "repo": prov.repo,
                    "ref": prov.git_ref,
                    "sha": prov.sha,
                    "indexedAt": st.indexed_at,
                }),
            ));
        }
        rows.sort_by(|a, b| (a.0 != default).cmp(&(b.0 != default)).then_with(|| a.0.cmp(&b.0)));
        json!({
            "default": default,
            "brains": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        })
    }
}

/// Pull the corpora and swap the brains in, once at a time.
///
/// The pull itself is [`corpus::pull`] — the same code as `brain corpus pull`,
/// so a webhook cannot refresh differently than a boot does. What this adds is
/// the part only a running server can do: rebuilding each changed brain's `Ctx`
/// so a T-box that moved in `schema.yaml` is re-read, and publishing the new
/// brain before releasing the caller.
///
/// Single-flight by construction: two concurrent refreshes would fetch into the
/// same checkout, and one of them would reindex a half-updated tree.
pub(crate) async fn refresh(
    multi: &Multi,
    corpora: &crate::corpus::Corpora,
    only: &[String],
) -> Result<Value> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let gate = GATE.get_or_init(|| tokio::sync::Mutex::new(()));
    let Ok(_permit) = gate.try_lock() else {
        return Err(anyhow::Error::new(Busy));
    };

    let reports = crate::corpus::pull(corpora, only, true, true).await?;
    let mut swapped: Vec<String> = Vec::new();
    for r in &reports {
        // Only a corpus that actually reindexed needs a new `Ctx`; the rest are
        // unchanged on disk, and rebuilding them would drop a warm pool for
        // nothing.
        if r.reindexed.is_none() {
            continue;
        }
        let Some(c) = corpora.get(&r.db) else { continue };
        match crate::config::from_root_named(&c.brain_dir(), Some(&c.alias))
            .and_then(|b| Ok(b))
        {
            Ok(brain) => match Ctx::open_brain(brain).await {
                Ok(ctx) => {
                    let prov = Provenance::of(&ctx.brain, &c.repo, &c.git_ref);
                    multi.insert(ctx, prov);
                    swapped.push(r.db.clone());
                }
                Err(e) => log_refresh(&format!("{}: reopened brain failed: {e}", r.db)),
            },
            Err(e) => log_refresh(&format!("{}: {} ", r.db, e)),
        }
    }
    Ok(json!({
        "ok": !reports.iter().any(|r| r.status == "failed"),
        "swapped": swapped,
        "corpora": reports,
    }))
}

fn log_refresh(msg: &str) {
    eprintln!("corpus: {msg}");
}

/// A refresh that arrived while another is running. A distinct type rather than
/// a string match, because the transport has to answer it with `409` and every
/// other error with `500`.
#[derive(Debug)]
pub(crate) struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a corpus refresh is already running")
    }
}

impl std::error::Error for Busy {}

/// A status and a body, so both transports answer the webhook identically
/// without either of them owning the policy.
pub(crate) struct HttpAnswer {
    pub status: u16,
    pub body: Value,
}

impl HttpAnswer {
    fn error(status: u16, message: impl Into<String>) -> HttpAnswer {
        HttpAnswer { status, body: json!({"ok": false, "error": message.into()}) }
    }
}

/// `POST /api/corpus/refresh` — the whole endpoint, minus the socket.
///
/// Refuses three ways before it does any work, and each refusal is a different
/// fact worth distinguishing: no corpora configured (`501` — nothing to pull),
/// no secret configured (`403` — this server will not be an open trigger), and a
/// bad or missing credential (`401`). Then one refusal during: another refresh
/// already holds the gate (`409`).
pub(crate) async fn corpus_refresh(
    multi: &Multi,
    corpora: Option<&crate::corpus::Corpora>,
    only: &[String],
    body: &[u8],
    authorization: Option<&str>,
    signature: Option<&str>,
) -> HttpAnswer {
    let Some(corpora) = corpora else {
        return HttpAnswer::error(
            501,
            "no corpora configured — start the server with `--corpora <file>` or BRAIN_CORPORA",
        );
    };
    let secret = std::env::var("BRAIN_WEBHOOK_SECRET").unwrap_or_default();
    let secret = secret.trim();
    if secret.is_empty() {
        return HttpAnswer::error(
            403,
            "BRAIN_WEBHOOK_SECRET is not set; refusing to expose an unauthenticated refresh trigger",
        );
    }
    if !authorised(secret, authorization, signature, body) {
        return HttpAnswer::error(
            401,
            "unauthorized: send `Authorization: Bearer <secret>` or a valid `X-Hub-Signature-256`",
        );
    }
    match refresh(multi, corpora, only).await {
        Ok(v) => HttpAnswer { status: 200, body: v },
        Err(e) if e.downcast_ref::<Busy>().is_some() => HttpAnswer::error(409, e.to_string()),
        Err(e) => HttpAnswer::error(500, format!("refresh failed: {e}")),
    }
}

/// Bearer token, or an HMAC-SHA256 signature over the body.
///
/// The signature is the shape a git host sends (`X-Hub-Signature-256:
/// sha256=<hex>`), so a real webhook can be pointed at this without a proxy
/// rewriting headers; the bearer token is what a `curl` from a runbook uses.
fn authorised(secret: &str, authorization: Option<&str>, signature: Option<&str>, body: &[u8]) -> bool {
    use hmac::{Hmac, Mac};

    if let Some(auth) = authorization {
        let token = auth.trim();
        let token = token.strip_prefix("Bearer ").or_else(|| token.strip_prefix("bearer "));
        if let Some(token) = token {
            if constant_time_eq(token.trim().as_bytes(), secret.as_bytes()) {
                return true;
            }
        }
    }
    if let Some(sig) = signature {
        let hex_sig = sig.trim().trim_start_matches("sha256=");
        if let Ok(bytes) = hex::decode(hex_sig) {
            let mut mac = match Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) {
                Ok(m) => m,
                Err(_) => return false,
            };
            mac.update(body);
            // `verify_slice` is constant-time; comparing hex strings would leak
            // the prefix length through the timing.
            return mac.verify_slice(&bytes).is_ok();
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// One API request: find the brain it names, then answer it.
async fn answer(multi: &Multi, route: Route, req: &Owned) -> Result<Value> {
    let ctx = match multi.ctx(req.db.as_deref()) {
        Ok(c) => c,
        Err(db) => {
            return Ok(json!({
                "error": format!("no such brain: {db}"),
                "brains": multi.names(),
            }))
        }
    };
    handle(&ctx, route, req).await
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
    /// The database this request names (already decoded). `None` means the
    /// server's default, and is resolved where the set of brains lives — not by
    /// the handlers, which only ever see one brain's data.
    pub(crate) db: Option<String>,
}

/// The `brain web` side: keep only the parameters the handlers read, so a
/// stray query key cannot reach a handler and be mistaken for an argument.
fn clone_req(req: &Request) -> Owned {
    let mut params = BTreeMap::new();
    for key in ["db", "slug", "name", "offset", "limit", "q", "mode"] {
        if let Some(v) = req.param(key) {
            params.insert(key.to_string(), v);
        }
    }
    let db = params.get("db").cloned();
    Owned { params, db }
}

impl Owned {
    /// The `brain serve` side: a query string already parsed and decoded.
    pub(crate) fn from_query(params: BTreeMap<String, String>) -> Owned {
        let db = params.get("db").cloned();
        Owned { params, db }
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
        "db": ctx.brain.alias,
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
    let rows = ctx
        .store
        .list_class_rows(Some(&resolved), limit.clamp(1, 1000), offset)
        .await?;
    Ok(json!({
        "class": resolved, "offset": offset,
        "entities": rows.into_iter().map(|row| json!({
            // A listing row is a badge plus the note's opening — the same shape
            // a search result has, so the two pages read the same way. The
            // budget is the row's, not the note's: 220 characters is about two
            // lines at the page's measure.
            "summary": crate::abox::summary(&row.to_entity(), 220),
            "slug": row.slug,
            "label": row.label,
        })).collect::<Vec<_>>(),
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

// ── the stream ──────────────────────────────────────────────────────────────

/// Ceiling on one `/api/stream` response: the search ceiling plus enough slack
/// to deliver a final `result` or `error` event, so a client is never left
/// holding an open stream that has already been abandoned upstream.
///
/// It used to be a 45 s guess in `serve`, which meant the browser waited 45 s to
/// learn that the walk was gone -- and then re-queued a second crawl behind it.
pub(crate) const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// Where the frames go, per transport.
///
/// The event contract lives in [`crawl_events`], not here: `brain serve` writes
/// to a tokio socket, `brain web` to the devserver's chunked [`Stream`], and both
/// must emit the same bytes in the same order. Keeping only the *write* in this
/// trait is what stops the dev progress feed and the production one from
/// drifting into two dialects.
#[allow(async_fn_in_trait)] // pub(crate): not a published API surface
pub(crate) trait EventSink {
    /// Write one complete SSE frame: `event: <kind>`, `data: <json>`, blank line.
    async fn frame(&mut self, kind: &str, data: &str) -> Result<()>;

    /// Has the reader gone away since the last frame?
    ///
    /// A transport whose write fails on a dead socket needs no check (that
    /// failure IS the signal); one that records EOF instead does, which is why
    /// this exists rather than being folded into `frame`. It is a *shortcut*,
    /// never the guarantee: the walk also stops when a frame cannot be written.
    fn gone(&mut self) -> bool;
}

/// The walk's task, aborted when this is dropped.
///
/// An abandoned stream must stop costing the shared GPU. Two things end it
/// early -- a failed write (the `?` on `frame`) and a vanished reader (the
/// tick) -- and both return from `crawl_events`. Dropping a `JoinHandle` does
/// *not* cancel its task (it detaches), so without this guard the walk would
/// run to its own deadline for a reader nobody has.
struct Job(tokio::task::JoinHandle<Result<Value>>);

impl Drop for Job {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One crawl as `text/event-stream`, shared by both transports.
///
/// Events: `event: step` per walk step (tiny -- step, stage, action, arg, calls,
/// ms), then exactly one `event: result` carrying the same JSON `/api/query`
/// would have returned, or `event: error`. Keeping the result identical to the
/// JSON endpoint is what lets the browser treat a dropped stream as "fall back
/// to the polling request" without special-casing anything.
///
/// Why SSE and not a websocket, since the next reader will ask: the traffic is
/// one-way, the payload is a few small JSON lines, and `EventSource` already
/// handles reconnection. If a follow-up chat feature ever needs the server to
/// react mid-generation, that is the moment to upgrade, and it is a transport
/// swap behind this function rather than a rewrite.
pub(crate) async fn crawl_events<S: EventSink>(
    ctx: Arc<Ctx>,
    q: &str,
    limit: i64,
    sink: &mut S,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<query::crawl::Progress>();
    let query = q.to_string();
    let mut job = Job(tokio::spawn(async move {
        crawl_payload(&ctx, &query, limit, Some(tx)).await
    }));

    // Once the crawl returns, its sink drops and `rx.recv()` yields None
    // forever -- which would spin this loop, and worse, would make it believe
    // the stream was over while the search/ontology half of the payload is
    // still running. So the receive arm retires itself on the first None and
    // the job arm stays the only way out.
    let mut listening = true;
    // A clock that asks exactly one question -- "is anyone still there?" -- so a
    // walk that has gone quiet (a slow model turn can be seconds long) notices
    // an abandoned tab before its next step does. Best-effort by design: the
    // authoritative signal is still a failed write, and a transport whose socket
    // reports that on its own answers `false` here.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if sink.gone() {
                    bail!("the reader disconnected");
                }
            }
            ev = rx.recv(), if listening => match ev {
                Some(p) => {
                    let body = serde_json::to_string(&p)?;
                    sink.frame("step", &body).await?;
                }
                None => listening = false,
            },
            // Bounded, because this task holds a connection and an in_flight
            // slot: the walk has its own budget, but a hung search or ontology
            // leg behind it must not pin a socket indefinitely.
            r = tokio::time::timeout(STREAM_TIMEOUT, &mut job.0) => {
                let (kind, body) = match r {
                    Err(_) => ("error", r#"{"error":"stream timeout"}"#.to_string()),
                    Ok(Err(_)) => ("error", r#"{"error":"crawl panicked"}"#.to_string()),
                    Ok(Ok(Err(e))) => ("error", json!({"error": e.to_string()}).to_string()),
                    Ok(Ok(Ok(v))) => serde_json::to_string(&v)
                        .map(|s| ("result", s))
                        .unwrap_or_else(|_| ("error", r#"{"error":"serialise failed"}"#.to_string())),
                };
                sink.frame(kind, &body).await?;
                return Ok(());
            }
        }
    }
}

/// The dev server's side: frames through the kit's chunked [`Stream`].
///
/// Runs on the connection thread (`rustex1-devserver` is one thread per
/// connection), so `block_on` is legal here for the same reason it is in the
/// JSON handlers: this thread is not a runtime worker.
fn stream_answer(multi: &Multi, rt: &tokio::runtime::Runtime, req: &Request, out: &mut Stream) {
    // A raw route is matched before the dev server's method gate, so the method
    // is ours to check. `EventSource` only ever sends GET; anything else is a
    // client mistake, and answering it with a stream would be a body on a HEAD.
    if req.method != "GET" {
        let _ = out.json(405, &json!({"error": "method not allowed"}));
        return;
    }
    let q = req.param("q").unwrap_or_default();
    if q.trim().is_empty() {
        let _ = out.json(400, &json!({"error": "no query"}));
        return;
    }
    let Ok(ctx) = multi.ctx(req.param("db").as_deref()) else {
        let db = req.param("db").unwrap_or_default();
        let _ = out.json(404, &json!({"error": format!("no such brain: {db}")}));
        return;
    };
    let limit = req
        .param("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(20);

    // Headers before any work, and the same ones `serve` sends: a client that is
    // watching progress needs the stream open, not merely buffered, and
    // `x-accel-buffering: no` is the conventional hint that no proxy on the path
    // may hold the body.
    let _ = out.start(
        200,
        &[
            ("content-type", "text/event-stream; charset=utf-8"),
            ("cache-control", "no-store"),
            ("x-accel-buffering", "no"),
        ],
    );
    let mut sink = ChunkSink { out };
    // The dispatcher calls `finish()` after this returns; a write error or a
    // vanished reader has already ended the conversation by then.
    let _ = rt.block_on(crawl_events(ctx, &q, limit, &mut sink));
}

struct ChunkSink<'a, 'b> {
    out: &'a mut Stream<'b>,
}

impl EventSink for ChunkSink<'_, '_> {
    async fn frame(&mut self, kind: &str, data: &str) -> Result<()> {
        let body = format!("event: {kind}\ndata: {data}\n\n");
        self.out
            .chunk(body.as_bytes())
            .map_err(|e| anyhow::anyhow!("writing an SSE frame: {e}"))
    }

    fn gone(&mut self) -> bool {
        self.out.disconnected()
    }
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
