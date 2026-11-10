//! serve.rs — `brain serve`: the production HTTP surface for the wiki.
//!
//! `brain web` and `brain serve` render the same page and answer the same four
//! JSON routes. Everything *around* them differs, and the differences are the
//! reason this file exists:
//!
//! |                     | `brain web` (rustex1 devserver)              | `brain serve` (this)                  |
//! |---------------------|----------------------------------------------|---------------------------------------|
//! | filesystem watcher  | yes — rebuilds the wasm on a source edit      | none                                   |
//! | boot-time build     | rebuilds when `web/pkg` is stale              | none; the image ships the bundle       |
//! | `POST /api/restart` | **yes — re-execs the process**                | absent                                 |
//! | concurrency         | one OS thread per connection                  | tokio tasks                            |
//! | SIGTERM             | default disposition, no drain                 | stop accepting, drain, exit 0          |
//! | listen socket       | opened after the build                        | opened before the database             |
//! | `/hot.mjs`          | the kit's client: socket, flash, overlay      | a production stub: none of those       |
//! | `/api/status`+`/api/builds` | the dev server's own telemetry        | answered honestly as "no build here"   |
//!
//! A server that recompiles Rust on request and can be told to replace its own
//! process by an HTTP call is the right tool at a desk and the wrong one behind
//! a corporate gateway. The moment it is reachable from a network, `POST
//! /api/restart` is a denial-of-service primitive and a cold `web/pkg` is a
//! compiler failure on the request path.
//!
//! ## What the browser actually requires
//!
//! `kit/web/app.mjs` boots by fetching `GET /api/config` to learn the wasm
//! artifact URLs, importing them, and calling the module's `boot()`. That route
//! is therefore not optional — answer it wrongly and the page sits on its
//! splash screen forever.
//!
//! HMR is switched off on **both** ends, not just the server one:
//!
//! * *Server*: no watcher, no boot-time rebuild, no `POST /api/restart`, and
//!   `GET /__m_hmr` refuses the upgrade with 426 rather than pretending.
//! * *Client*: `/hot.mjs` is served from `deploy/web-prod/hot.mjs`, compiled in
//!   with `include_str!`, instead of falling through to the kit's dev client. A
//!   stub is needed because the dev client cannot be left in place: it retries
//!   its socket every 4 s forever (`hot.mjs`'s `close` handler re-`connect()`s
//!   unconditionally) and `kit/src/live.rs` derives the status chip from
//!   `__HMR__.connected`, so a healthy pod renders a red OFFLINE badge. It is
//!   compiled in rather than dropped in `web/` because `web/` is shared with
//!   `brain web`, and shadowing the client there would kill hot reload in the
//!   one place it is actually wanted. The kit offers no opt-out —
//!   `api::spawn_all()` is unconditional at `kit/src/lib.rs:204` — and
//!   `vendor/rustex1` is a submodule, so an upstream patch would not survive
//!   `git submodule update`.
//! * *Telemetry*: `/api/status` and `/api/builds` answer with what a production
//!   server actually knows (uptime, request count, no builds) instead of 404ing,
//!   because a 404 without a JSON body is routed by the kit into `fail()`, which
//!   writes `session.error` and `session.online = false`.
//!
//! ## Why the socket binds before the database does
//!
//! `Ctx::open` needs PostgreSQL: it loads the T-box and runs `ensure_schema()`.
//! Opening it first would mean the process has no listening socket while the
//! database is still coming up, so a kubelet liveness probe would kill a pod
//! that was merely waiting for a neighbour — and in a pod where Postgres is a
//! sibling container, "waiting for a neighbour" is the normal case, not the
//! pathological one. So this binds immediately, answers `/health` from the
//! first instant, and opens the store on a background task with backoff.
//! Until that task lands, `/api/*` returns `503` and `/health` reports
//! `"ready": false`, which is exactly what a readiness probe is for.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

use crate::cli::Global;
use crate::commands::Ctx;
use crate::web::{handle, Owned, Route};

/// Defaults chosen for the container, not for a desk: a production port, and
/// loopback still the default bind so an unauthenticated wiki never becomes
/// network-reachable by accident. The image passes `--host 0.0.0.0` explicitly,
/// and the platform's proxy remains the only way in.
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_WEB: &str = "web";
const DEFAULT_NAME: &str = "brain";
const DEFAULT_PKG_JS: &str = "/pkg/brain_app.js";
const DEFAULT_PKG_WASM: &str = "/pkg/brain_app_bg.wasm";

/// The production hot client, compiled into the binary. Routed at `/hot.mjs`
/// ahead of the static handler, so `--assets` can keep pointing at the kit's
/// `web/` for everything else. Why this is an embedded artifact instead of a
/// file under `web/`: `web/` is also `brain web`'s root, and shadowing the kit's
/// client there would disable hot reload for development. See the file itself.
const PROD_HOT: &str = include_str!("../deploy/web-prod/hot.mjs");

/// Read ceilings. The wiki sends no bodies, so 1 MiB of header+request is
/// generous; over it the connection is closed rather than truncated, because a
/// half-parsed request line is a worse symptom than a dropped one.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to keep trying `Ctx::open` before giving up. Long enough that a
/// slow Postgres startup is a delay rather than a crash-loop.
const STORE_OPEN_TIMEOUT: Duration = Duration::from_secs(180);
/// Drain budget on SIGTERM. Envoy's default upstream drain is longer than this,
/// so in-flight requests finish before the cluster tears the listener down.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on one `/api/stream` response. The walk inside it is already bounded
/// by [`crawl::CRAWL_DEADLINE_MS`], but the payload also runs a search and may run
/// an ontology fallback after that budget is spent, and this task holds a socket
/// plus an `in_flight` slot while it does -- so the stream gets its own backstop
/// rather than inheriting whatever the slowest leg happens to take.
const STREAM_TIMEOUT: Duration = Duration::from_secs(45);

pub struct Args {
    pub host: IpAddr,
    pub port: u16,
    /// The app's own static root: `index.html` and the built `pkg/`.
    pub web: PathBuf,
    /// Extra roots searched after `web`, in order — in practice the rustex1
    /// kit's `web/` (app.mjs, hot.mjs, kit.css, the icon font). Same ordering
    /// rule as the dev server: the app's root is searched first, so an app may
    /// shadow a kit asset by putting its own copy in front of it.
    pub assets: Vec<PathBuf>,
    pub name: String,
    pub pkg_js: String,
    pub pkg_wasm: String,
    pub log: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            web: PathBuf::from(DEFAULT_WEB),
            assets: Vec::new(),
            name: DEFAULT_NAME.to_string(),
            pkg_js: DEFAULT_PKG_JS.to_string(),
            pkg_wasm: DEFAULT_PKG_WASM.to_string(),
            log: true,
        }
    }
}

/// The shared state every connection task reads.
struct Server {
    args: Args,
    /// Empty until the background opener lands; then `Some` forever after.
    ctx: RwLock<Option<Arc<Ctx>>>,
    boot: Instant,
    boot_id: String,
    requests: AtomicU64,
    in_flight: AtomicU64,
}

pub async fn run(argv: &[String], global: &Global) -> Result<i32> {
    let args = parse(argv)?;

    if !args.web.join("index.html").is_file() {
        bail!(
            "no index.html under {} — `brain serve` needs the built frontend \
             (index.html, the kit's app.mjs/hot.mjs/kit.css, and pkg/*). Build the \
             image, or run `brain web` for development.",
            args.web.display()
        );
    }

    let srv = Arc::new(Server {
        args,
        ctx: RwLock::new(None),
        boot: Instant::now(),
        boot_id: format!("{:x}", std::process::id()),
        requests: AtomicU64::new(0),
        in_flight: AtomicU64::new(0),
    });

    // 1 · the socket, before anything that can block.
    let listener = TcpListener::bind((srv.args.host, srv.args.port))
        .await
        .with_context(|| format!("cannot bind {}:{}", srv.args.host, srv.args.port))?;
    let bound = listener.local_addr()?;

    println!(
        "brain serve  {} · http://{}:{}  ·  web={}  ·  pid={}",
        srv.args.name, bound.ip(), bound.port(), srv.args.web.display(), std::process::id()
    );
    log::line("api", "GET /health");
    log::line("api", "GET /api/config");
    for route in ["/api/meta", "/api/entity", "/api/class", "/api/query"] {
        log::line("api", &format!("GET {route}"));
    }

    // 2 · the database, in the background, with backoff.
    let global = global.clone();
    let opener = {
        let srv = srv.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + STORE_OPEN_TIMEOUT;
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match Ctx::open(&global).await {
                    Ok(ctx) => {
                        let ctx = Arc::new(ctx);
                        log::line(
                            "pg",
                            &format!(
                                "connected after {attempt} attempt(s) — ontology {}",
                                ctx.tbox.ontology
                            ),
                        );
                        *srv.ctx.write().await = Some(ctx);
                        return;
                    }
                    Err(e) => {
                        if Instant::now() >= deadline {
                            // Deliberate: a pod that can never reach its database
                            // should be visible as a crash, not as a 503 forever.
                            log::line("pg", &format!("giving up after {attempt} attempts: {e}"));
                            std::process::exit(1);
                        }
                        let backoff =
                            Duration::from_millis(
                                u64::from((500 * 2u32.pow(attempt.min(5))).min(8_000)),
                            );
                        log::line(
                            "pg",
                            &format!(
                                "attempt {attempt} failed ({}), retrying in {}ms",
                                first_line(&e.to_string()),
                                backoff.as_millis()
                            ),
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        })
    };

    // 3 · accept until SIGTERM/SIGINT, then drain.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let accept = {
        let srv = srv.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let srv = srv.clone();                            // Bounding tasks-per-process matters more than the
                            // count itself: a stalled client must not be able to
                            // pin unbounded sockets. 512 is far above any real
                            // wiki usage and far below any fd limit.
                            if srv.in_flight.fetch_add(1, Ordering::Relaxed) > 512 {
                                srv.in_flight.fetch_sub(1, Ordering::Relaxed);
                                log::line("accept", &format!("{peer}: at capacity, refused"));
                                drop(stream);
                                continue;
                            }
                            // The guard, not a trailing decrement: a task that
                            // panics mid-request must still give its slot back,
                            // or the cap that stops a stalled client from
                            // pinning sockets would slowly strangle the server.
                            let slot = Slot(Arc::clone(&srv));
                            tokio::spawn(async move {
                                serve_connection(srv, stream).await;
                                drop(slot);
                            });
                        }
                        Err(e) => {
                            log::line("accept", &format!("{e}"));
                            // A transient accept error must not end the server.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        })
    };

    wait_for_signal().await;
    log::line(
        "shutdown",
        &format!(
            "signal received; draining {} in-flight request(s), {}s budget",
            srv.in_flight.load(Ordering::Relaxed),
            DRAIN_TIMEOUT.as_secs()
        ),
    );
    let _ = shutdown_tx.send(true);
    accept.abort();

    let drained = Instant::now();
    while srv.in_flight.load(Ordering::Relaxed) > 0 && drained.elapsed() < DRAIN_TIMEOUT {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if srv.in_flight.load(Ordering::Relaxed) > 0 {
        log::line("shutdown", "drain budget expired; closing anyway");
    }
    opener.abort();
    log::line("shutdown", "bye");
    Ok(0)
}

/// One admitted connection's claim on `in_flight`.
struct Slot(Arc<Server>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One connection, many requests.
///
/// Keep-alive is on by default because the upstream is a proxy: without it,
/// every request pays a TCP handshake to the pod. A client asking for `close`
/// gets exactly that, and any malformed request ends the *connection* rather
/// than the process.
async fn serve_connection(srv: Arc<Server>, mut stream: TcpStream) {
    stream
        .set_nodelay(true)
        .ok(); // latency beats buffering for a request/response protocol

    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8 * 1024];

    loop {
        // Read until a full header block. No body is ever required: every route
        // here is GET/HEAD, so stopping at \r\n\r\n is complete, not partial.
        let head_end = loop {
            if let Some(i) = find_double_crlf(&buf) {
                break i;
            }
            if buf.len() >= MAX_HEADER_BYTES {
                let _ = write_all(&mut stream, b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n").await;
                return;
            }
            match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => return, // client closed
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(_)) => return,
                Err(_) => return, // slow client: drop it, do not hold the task
            }
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let Some((method, target, version)) = parse_head(&head) else {
            let _ = write_all(&mut stream, b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
            return;
        };
        // Keep whatever the client pipelined after the headers.
        buf.drain(..head_end + 4);

        // ---- server-sent events: its own path, by necessity ------------------
        //
        // Everything below assumes a response is one buffer built and written
        // once. A progress stream is the opposite shape -- headers now, body in
        // pieces, for as long as the walk runs -- so it cannot go through
        // `route`/`framed`. Catch it here, hand it the socket, and end the
        // connection when it finishes: SSE clients reconnect on their own
        // (`EventSource` does it for you), so a closed stream is a normal
        // outcome rather than an error.
        let (rpath, rquery) = target.split_once('?').unwrap_or((target.as_str(), ""));
        if method == "GET" && rpath == "/api/stream" {
            let started = Instant::now();
            let res = serve_stream(&srv, &mut stream, rquery).await;
            if srv.args.log {
                srv.requests.fetch_add(1, Ordering::Relaxed);
                log::line(
                    "http",
                    &format!(
                        "GET /api/stream {} {}ms",
                        if res.is_ok() { "200" } else { "err" },
                        started.elapsed().as_millis()
                    ),
                );
            }
            return;
        }

        let keep_alive = keep_alive(&method, &version, &head);
        let started = Instant::now();
        let response = route(&srv, &method, &target).await;

        // Borrowed, not moved: the access log below still needs the status line
        // out of the same buffer it writes to the socket.
        let (head_bytes, _head_only) = response.parts();
        let out = framed(head_bytes, keep_alive);
        let sent = out.len();
        if write_all(&mut stream, &out).await.is_err() {
            return;
        }

        let (path, _) = target.split_once('?').unwrap_or((target.as_str(), ""));
        if srv.args.log && path != "/health" {
            srv.requests.fetch_add(1, Ordering::Relaxed);
            log::line(
                "http",
                &format!(
                    "{method} {path} {} {sent}B {}ms",
                    response.code(),
                    started.elapsed().as_millis()
                ),
            );
        }

        if !keep_alive {
            return;
        }
    }
}

// ── routing ────────────────────────────────────────────────────────────────

enum Response {
    Full(Vec<u8>),
    HeadOnly(Vec<u8>),
}

impl Response {
    /// The rendered head, and whether the body was suppressed for HEAD.
    fn parts(&self) -> (&[u8], bool) {
        match self {
            Response::Full(b) => (b, false),
            Response::HeadOnly(b) => (b, true),
        }
    }

    /// The status code, read back out of the rendered first line rather than
    /// carried alongside it — one source of truth, so a response cannot log a
    /// status it did not send. Named `code`, not `status`, because the
    /// `WithStatus::status` setter that stamps it takes the same receiver.
    fn code(&self) -> u16 {
        let head = String::from_utf8_lossy(self.parts().0);
        head.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
}

async fn route(srv: &Arc<Server>, method: &str, target: &str) -> Response {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let head_only = method == "HEAD";

    match (method, path) {
        ("GET" | "HEAD", "/health") => json_response(
            &json!({
                "ok": true,
                "app": srv.args.name,
                "ready": srv.ctx.read().await.is_some(),
                "uptimeS": srv.boot.elapsed().as_secs(),
                "requests": srv.requests.load(Ordering::Relaxed),
                "bootId": srv.boot_id,
            }),
            head_only,
        ),

        ("GET" | "HEAD", "/api/config") => json_response(
            // The exact shape kit/web/app.mjs destructures. `hmrPath` stays for
            // compatibility with anything that reads the dev contract, but
            // `dev: false` is the honest signal: the client `serve` hands out
            // (PROD_HOT, below) never opens that socket.
            &json!({
                "app": srv.args.name,
                "pkgJs": srv.args.pkg_js,
                "pkgWasm": srv.args.pkg_wasm,
                "hmrPath": "/__m_hmr",
                "bootId": srv.boot_id,
                "dev": false,
            }),
            head_only,
        ),

        ("GET" | "HEAD", "/api/meta") => api(srv, Route::Meta, query, head_only).await,
        ("GET", "/api/entity") => api(srv, Route::Entity, query, head_only).await,
        ("GET", "/api/class") => api(srv, Route::Class, query, head_only).await,
        ("GET", "/api/query") => api(srv, Route::Query, query, head_only).await,

        // The dev server's control surface, answered honestly instead of left
        // to 404 through the static handler: these mutate the process, and a
        // production server has no process to mutate.
        ("POST", "/api/rebuild") | ("POST", "/api/restart") => json_response(
            &json!({
                "error": "unavailable",
                "detail": "brain serve does not build or restart; use `brain web` for development"
            }),
            head_only,
        )
        .status(405),

        ("GET" | "HEAD", "/__m_hmr") => json_response(
            &json!({"error": "no HMR in this server"}),
            head_only,
        )
        .status(426),

        // The production hot client, from the binary. Routed *before*
        // `serve_static` so the kit's `web/hot.mjs` under `--assets` is never
        // reachable from this server: left in place it reconnects a dead socket
        // every 4 s per tab and parks a red OFFLINE chip on a healthy pod.
        ("GET" | "HEAD", "/hot.mjs") => inline_static(
            PROD_HOT.as_bytes(),
            "text/javascript; charset=utf-8",
            // Short rather than immutable: it is 2 KB, and picking a fix up on
            // reload is worth more than a year of cache.
            "public, max-age=60",
            head_only,
        ),

        // ---- the kit's telemetry, answered rather than 404'd ----------------
        //
        // `kit/src/api.rs`'s `spawn_all()` polls these every 2 s and 3 s, with
        // no production mode to stop it (it is called unconditionally at
        // `kit/src/lib.rs:204`). Falling through to the static handler returns a
        // 404 whose JSON body the kit's `send()` cannot parse as success, so it
        // lands in `fail()` — which writes `session.error` and
        // `session.online = false`. The result was a permanently red chip and a
        // console line every half second, on a server that is fine.
        //
        // Field names are the kit's contract, not inventions: `kit/src/ui.rs`
        // binds `$store.session.api.uptimeS`, `.bootId`, `.requests`, `.clients`.
        ("GET" | "HEAD", "/api/status") => json_response(
            &json!({
                "app": srv.args.name,
                "ready": srv.ctx.read().await.is_some(),
                "uptimeS": srv.boot.elapsed().as_secs(),
                "requests": srv.requests.load(Ordering::Relaxed),
                // Always 0: this server accepts no HMR sockets, so there is no
                // client set to count. `fetch_status` only needs the key.
                "clients": 0,
                "bootId": srv.boot_id,
                "dev": false,
            }),
            head_only,
        ),

        // `fetch_builds()` reads `.builds`, returns early on an empty list, and
        // leaves `buildLabel`/`buildTone` at the store's own defaults ("—" and
        // "idle") — which is the truth, since this process has never run cargo.
        ("GET" | "HEAD", "/api/builds") => json_response(
            &json!({"builds": [], "dev": false}),
            head_only,
        ),

        ("GET" | "HEAD", _) => {
            // The client owns every non-`/api/` route that is not a file: the
            // router puts the query and mode into the path
            // (`/search/<question>/crawl`, `/entity/<slug>`), so a refresh or a
            // shared link asks the server for a path that exists only in the
            // browser. Serving the app shell is the whole SPA contract -- and
            // `web/index.html` references its assets absolutely (`/app.mjs`,
            // `/kit.css`, `/styles.css`), so the shell boots at any depth.
            //
            // Two exclusions, both deliberate:
            //
            // * `/api/...` keeps 404-ing as JSON. Falling through here would
            //   answer a mistyped API call with an HTML page, which a `fetch().
            //   json()` caller reads as a parse failure -- far worse to debug
            //   than a 404 with a name in it.
            // * A missing ASSET keeps 404-ing. `styles.css` coming back as
            //   `text/html` unstyled-the-app is a silent, confusing failure; a
            //   404 points straight at the file that is not there.
            if requires_exact_file(path) {
                match serve_static(&srv.args, path) {
                    Some(bytes) => static_response(bytes, head_only),
                    None => not_found(head_only),
                }
            } else {
                match serve_static(&srv.args, path).or_else(|| serve_shell(&srv.args)) {
                    Some(bytes) => static_response(bytes, head_only),
                    None => not_found(head_only),
                }
            }
        },

        _ => not_found(head_only),
    }
}

async fn api(srv: &Arc<Server>, kind: Route, query: &str, head_only: bool) -> Response {
    let Some(ctx) = srv.ctx.read().await.clone() else {
        return json_response(
            &json!({"error": "unavailable", "detail": "database not ready"}),
            head_only,
        )
        .status(503);
    };
    let params = query_params(query);
    match handle(&ctx, kind, &Owned::from_query(params)).await {
        Ok(v) => json_response(&v, head_only),
        Err(e) => json_response(
            // The dev server returned the same text; what changes is that the
            // status is now an error status, so a browser or monitor can tell a
            // failed answer from an empty one.
            &json!({"error": "internal", "detail": e.to_string()}),
            head_only,
        )
        .status(500),
    }
}

/// Wrap bytes built by [`render_static`] for the method that asked.
///
/// `render_static` always appends a body, and the write path ignores
/// `Response::parts()`'s head-only flag -- suppression happens at construction,
/// the way [`json_response`] does it. Handing HEAD a `Full` therefore sent 902
/// bytes nobody asked for: `Content-Length` stayed correct, so curl and Chrome
/// (which opens a separate connection for HEAD) never noticed, but a strict
/// keep-alive client reads those bytes as the start of the NEXT response and
/// desynchronises the connection. RFC 9110 is explicit: HEAD is GET without a
/// body. Keep the head -- it already carries the right status, type, length and
/// cache policy -- and drop everything after the header terminator.
fn static_response(bytes: Vec<u8>, head_only: bool) -> Response {
    if !head_only {
        return Response::Full(bytes);
    }
    match bytes.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => Response::HeadOnly(bytes[..i + 4].to_vec()),
        None => Response::HeadOnly(bytes),
    }
}

/// Should this path be served strictly from disk -- 404 if the file is not there
/// -- rather than answered by the app shell?
///
/// Decided on the last segment's extension, from a closed list -- NOT on "does
/// it contain a dot". DST slugs and questions are full of dots (`info.crafting`,
/// "What is Mr. Skitts?"), and treating those as assets would 404 a route the
/// browser can happily render. Anything without a recognised extension is a
/// route, and routes belong to the shell.
fn requires_exact_file(path: &str) -> bool {
    if path.starts_with("/api/") || path == "/api" {
        return true;
    }
    let decoded = percent_decode(path);
    let last = decoded.rsplit('/').next().unwrap_or("");
    let Some((_, ext)) = last.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "css" | "js" | "mjs" | "json" | "wasm" | "html" | "htm" | "svg" | "png" | "jpg"
            | "jpeg" | "gif" | "webp" | "ico" | "woff" | "woff2" | "ttf" | "map" | "txt"
    )
}

/// The app shell, for a route only the client-side router understands.
///
/// Same bytes and same headers as `/`, including `no-cache` (a deploy must be
/// visible immediately, not after the browser gives up on its copy). Serves the
/// one file the caller already knows about, so a deep link can never widen what
/// the static handler is able to read.
fn serve_shell(args: &Args) -> Option<Vec<u8>> {
    let app_root = args.web.canonicalize().ok()?;
    let index = args.web.join("index.html").canonicalize().ok()?;
    if !index.starts_with(&app_root) || !index.is_file() {
        return None;
    }
    let bytes = std::fs::read(&index).ok()?;
    Some(render_static(&index, &bytes, &app_root))
}

/// Path resolution, done without a filesystem walk that can escape.
///
/// Roots are searched in order — the app's first, so it shadows the kit — and
/// two rules hold for every one of them: the joined path must still sit under
/// *that* root after canonicalization (which resolves symlinks, so a planted
/// link cannot be a way out), and a missing file is not an error, just the next
/// root. `/` is index.html; nothing is ever listed.
fn serve_static(args: &Args, url_path: &str) -> Option<Vec<u8>> {
    let decoded = percent_decode(url_path);
    if decoded.contains('\0') {
        return None;
    }
    let rel = decoded.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    let app_root = args.web.canonicalize().ok()?;
    for root in std::iter::once(args.web.as_path()).chain(args.assets.iter().map(PathBuf::as_path)) {
        let Ok(canonical_root) = root.canonicalize() else {
            continue; // a missing --assets dir is a typo, not a failure
        };
        let candidate = match canonical_root.join(rel).canonicalize() {
            Ok(c) if c.starts_with(&canonical_root) && c.is_file() => c,
            _ => continue,
        };
        // A synchronous read is deliberate: these are small, already-warm build
        // artifacts, and `std::fs` keeps this function total (Option, not Result)
        // and free of an await point in the middle of routing.
        let bytes = std::fs::read(&candidate).ok()?;
        return Some(render_static(&candidate, &bytes, &app_root));
    }
    None
}

fn render_static(path: &Path, bytes: &[u8], app_root: &Path) -> Vec<u8> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = mime_of(&ext);

    // Cache policy by category, not by extension. The wasm bundle and the font
    // are content-addressed by the build and cache-busted by `?t=` in app.mjs,
    // so they are safe to hold a long time; index.html must never be cached, or
    // a deploy is invisible until the browser gives up on its copy.
    let cache = match ext.as_str() {
        "html" => "no-cache",
        "wasm" | "js" | "mjs" | "css" | "woff2" | "woff" | "ttf" => {
            if path.starts_with(app_root.join("pkg")) {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=300"
            }
        }
        _ => "public, max-age=3600",
    };

    let mut out = Vec::with_capacity(bytes.len() + 256);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(format!("Content-Type: {mime}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", bytes.len()).as_bytes());
    out.extend_from_slice(format!("Cache-Control: {cache}\r\n").as_bytes());
    out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
    out.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    out.extend_from_slice(bytes);
    out
}

/// Serve bytes that live in the binary rather than on disk, emitting the same
/// headers the static path does — so a compiled-in shim and a real file are
/// indistinguishable to the browser, and one cache/security policy governs both.
fn inline_static(bytes: &[u8], mime: &str, cache: &str, head_only: bool) -> Response {
    let mut out = Vec::with_capacity(bytes.len() + 256);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(format!("Content-Type: {mime}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", bytes.len()).as_bytes());
    out.extend_from_slice(format!("Cache-Control: {cache}\r\n").as_bytes());
    out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
    out.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    // HEAD keeps an accurate Content-Length and sends no body, exactly as the
    // on-disk path does for the responses it suppresses.
    if !head_only {
        out.extend_from_slice(bytes);
    }
    if head_only {
        Response::HeadOnly(out)
    } else {
        Response::Full(out)
    }
}

fn json_response(value: &Value, head_only: bool) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let mut out = head.into_bytes();
    if !head_only {
        out.extend_from_slice(&body);
    }
    if head_only {
        Response::HeadOnly(out)
    } else {
        Response::Full(out)
    }
}

/// Re-stamp a rendered response with another status. Only ever used on the
/// synthetic error responses, so rewriting the first line is safe.
trait WithStatus {
    fn status(self, code: u16) -> Response;
}

impl WithStatus for Response {
    fn status(self, code: u16) -> Response {
        let reason = match code {
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            426 => "Upgrade Required",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "OK",
        };
        // Destructure once: taking the bytes out of `self` and then matching on
        // `self` again would be a use-after-move.
        match self {
            Response::Full(bytes) => Response::Full(restamp(bytes, code, reason)),
            Response::HeadOnly(bytes) => Response::HeadOnly(restamp(bytes, code, reason)),
        }
    }
}

/// Swap the status line, leaving headers and body alone. Only ever applied to
/// the synthetic error responses this module builds itself, so the first `\r`
/// is reliably the end of the status line.
fn restamp(bytes: Vec<u8>, code: u16, reason: &str) -> Vec<u8> {
    match bytes.iter().position(|b| *b == b'\r') {
        Some(i) => {
            let mut out = format!("HTTP/1.1 {code} {reason}").into_bytes();
            out.extend_from_slice(&bytes[i..]);
            out
        }
        None => bytes,
    }
}

fn not_found(head_only: bool) -> Response {
    json_response(&json!({"error": "not_found"}), head_only).status(404)
}

// ── HTTP parsing ───────────────────────────────────────────────────────────

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_head(head: &str) -> Option<(String, String, String)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.0").to_string();
    if !target.starts_with('/') && !target.starts_with("http") {
        return None; // not origin-form; a proxy mistake, not our problem
    }
    Some((method, target, version))
}

/// `GET /api/stream?q=…` -- one crawl as `text/event-stream`.
///
/// Why SSE and not a websocket, stated here because the next reader will ask:
/// the traffic is one-way, the payload is a few small JSON lines, and
/// `EventSource` already handles reconnection. A websocket would buy nothing
/// except a second protocol to proxy, authenticate and -- given what the kit's
/// dev client taught us -- reconnect without inventing a 4-second retry loop
/// that never stops. If the follow-up chat feature ever needs the server to
/// react mid-generation, that is the moment to upgrade; it is a transport swap
/// behind this function, not a rewrite.
///
/// Events: `event: step` per walk step (tiny -- step, stage, action, arg, calls,
/// ms), then exactly one `event: result` carrying the same JSON `/api/query`
/// would have returned, or `event: error`. Keeping the result identical to the
/// JSON endpoint means the browser can treat a dropped stream as "fall back to
/// the polling request" without special-casing anything.
///
/// Cancellation is a property of this design rather than an extra: writes go
/// straight to the socket, so a client that vanished makes `write_all` fail, the
/// sink's receiver drops, and `crawl::run` sees its send fail and stops -- the
/// abandoned search stops costing the shared GPU anything.
async fn serve_stream(srv: &Arc<Server>, stream: &mut TcpStream, query: &str) -> std::io::Result<()> {
    let params = query_params(query);
    let q = params.get("q").cloned().unwrap_or_default();
    if q.trim().is_empty() {
        return write_all(
            stream,
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json; charset=utf-8\r\n\
              Content-Length: 24\r\nConnection: close\r\n\r\n{\"error\":\"no query\"}\n",
        )
        .await;
    }
    let Some(ctx) = srv.ctx.read().await.clone() else {
        return write_all(
            stream,
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json; charset=utf-8\r\n\
              Content-Length: 27\r\nConnection: close\r\n\r\n{\"error\":\"not ready\"}\n",
        )
        .await;
    };

    // Headers immediately, before any work: a client that is watching progress
    // needs the stream open, not merely unbuffered. `X-Accel-Buffering: no` is
    // the conventional hint that no proxy on the path may hold the body.
    write_all(
        stream,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
          Cache-Control: no-store\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n",
    )
    .await?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::query::crawl::Progress>();
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(20);
    let mut job = tokio::spawn(async move { crate::web::crawl_payload(&ctx, &q, limit, Some(tx)).await });

    // Once the crawl returns, its sink drops and `rx.recv()` starts yielding
    // None forever -- which would spin this loop, and worse, would make it
    // believe the stream was over while the search/ontology half of the payload
    // is still running. So the receive arm retires itself on the first None and
    // the job arm stays the only way out.
    let mut listening = true;
    loop {
        tokio::select! {
            ev = rx.recv(), if listening => match ev {
                Some(p) => {
                    let Ok(body) = serde_json::to_string(&p) else { continue };
                    write_all(stream, format!("event: step\ndata: {body}\n\n").as_bytes()).await?;
                }
                None => listening = false,
            },
            // Bounded, because this task holds a connection and an in_flight
            // slot: the walk has its own 6 s budget, but a hung search or
            // ontology leg behind it should not pin a socket indefinitely.
            r = tokio::time::timeout(STREAM_TIMEOUT, &mut job) => {
                let (kind, body) = match r {
                    Err(_) => ("error", r#"{"error":"stream timeout"}"#.to_string()),
                    Ok(Err(_)) => ("error", r#"{"error":"crawl panicked"}"#.to_string()),
                    Ok(Ok(Err(e))) => ("error", json!({"error": e.to_string()}).to_string()),
                    Ok(Ok(Ok(v))) => serde_json::to_string(&v)
                        .map(|s| ("result", s))
                        .unwrap_or_else(|_| ("error", r#"{"error":"serialise failed"}"#.to_string())),
                };
                write_all(stream, format!("event: {kind}\ndata: {body}\n\n").as_bytes()).await?;
                break;
            }
        }
    }
    Ok(())
}

fn keep_alive(method: &str, version: &str, head: &str) -> bool {
    let lower = head.to_ascii_lowercase();
    if method == "CONNECT" {
        return false;
    }
    if lower.contains("connection: close") {
        return false;
    }
    if version == "HTTP/1.0" {
        // 1.0 keep-alive is opt-in, per its spec.
        return lower.contains("connection: keep-alive");
    }
    true
}

/// `a=1&b=2` → map, with `+` and `%xx` handled. Unknown keys are kept; the
/// handlers pick the ones they want, so adding a parameter is not a server
/// change.
fn query_params(query: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

/// Percent-decoding, `+`-as-space, and — deliberately — no decoding of `/`.
/// A path must not be able to acquire a separator from an escape sequence.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &bytes[i + 1..i + 3];
                match u8::from_str_radix(&String::from_utf8_lossy(hex), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mime_of(ext: &str) -> &'static str {
    match ext {
        // wasm without the right type is not a module, and the browser's error
        // for that ("failed to compile wasm module: Invalid binary magic") is
        // famously unhelpful, so this mapping is not decoration.
        "wasm" => "application/wasm",
        "mjs" | "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ── plumbing ───────────────────────────────────────────────────────────────

async fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, async {
        stream.write_all(bytes).await?;
        stream.flush().await
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut, e))?
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        let Ok(mut int) = signal(SignalKind::interrupt()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

mod log {
    pub fn line(kind: &str, msg: &str) {
        // stdout, not stderr: in a container both reach the same log stream and
        // interleaving is less confusing from one writer.
        println!("{kind:<9} {msg}");
    }
}

/// Already fully framed by `json_response`; this only rewrites `Connection` so
/// a client that asked to close is not told to keep the socket open.
fn framed(body: &[u8], keep_alive: bool) -> Vec<u8> {
    if keep_alive {
        return body.to_vec();
    }
    let text = String::from_utf8_lossy(body).replace("Connection: keep-alive", "Connection: close");
    text.into_bytes()
}

/// Parse `brain serve`'s own flags. Global flags (`--db`, `--use`) are peeled by
/// the caller, so anything unrecognised here is a mistake worth surfacing.
fn parse(argv: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let mut value = || -> Result<String> {
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match flag {
            "--port" | "-p" => a.port = value()?.parse().context("--port wants a number")?,
            "--host" => {
                let h = value()?;
                a.host = h
                    .parse()
                    .with_context(|| format!("--host wants an IP address, got {h}"))?
            }
            "--web" => a.web = PathBuf::from(value()?),
            // Repeatable: `--assets kit/web --assets extra`.
            "--assets" => a.assets.push(PathBuf::from(value()?)),
            "--name" => a.name = value()?,
            "--pkg-js" => a.pkg_js = value()?,
            "--pkg-wasm" => a.pkg_wasm = value()?,
            "--quiet" => a.log = false,
            "-h" | "--help" => bail!("usage: brain serve [--host H] [--port N] [--web DIR] [--name N]"),
            other if other.starts_with("--port=") => {
                a.port = other["--port=".len()..].parse().context("--port wants a number")?
            }
            other if other.starts_with("--host=") => {
                a.host = other["--host=".len()..]
                    .parse()
                    .with_context(|| "bad --host".to_string())?
            }
            other if other.starts_with("--web=") => a.web = PathBuf::from(&other["--web=".len()..]),
            other if other.starts_with("--assets=") => {
                a.assets.push(PathBuf::from(&other["--assets=".len()..]))
            }
            other => bail!("unknown flag for `brain serve`: {other}"),
        }
        i += 1;
    }
    Ok(a)
}
