//! The stores: everything a page reads, and the fetches that fill them.
//!
//! A store lives in m-js's bucket on `window`, so a wasm swap merges new
//! methods and **keeps the values** — which is why the fetched entity, the
//! search results and the typed query all survive editing a template. Anything
//! held in a Rust thread-local instead would reset on every rebuild.
//!
//! Nothing here deserializes. The server answers JSON, `api::get` hands back a
//! `JsValue`, and that value goes straight into the store for m-js to read with
//! `x-for` / `x-text`. A `serde` round trip on the wasm side would buy nothing
//! and cost the whole schema in generated code.

use rustex1_kit::{api, js, lifecycle, mjs, mjs::Def};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use wasm_bindgen::prelude::*;

/// Every answer carries the id of the request that asked for it.
///
/// Without it the store is last-writer-wins, and the browser's arrival order is
/// not the user's intent order: search A, get impatient, refresh, search B, and
/// A's slow response lands on top of B's screen. The guard in [stale] makes the
/// screen's expectation the only thing a callback is allowed to satisfy.
static NEXT_REQ: AtomicU32 = AtomicU32::new(1);

/// The live stream, if one is open, plus the closures wired to it.
///
/// The closures are held rather than `forget()`ten because a session can run
/// hundreds of searches: leaking three listeners each is a slow but genuine leak,
/// and the only thing keeping them alive is this cell. Replacing the cell drops
/// the old set, which is exactly when the old `EventSource` is closed too.
struct Live {
    source: JsValue,
    _step: Closure<dyn FnMut(JsValue)>,
    _result: Closure<dyn FnMut(JsValue)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

thread_local! {
    static STREAM: RefCell<Option<Live>> = const { RefCell::new(None) };
}

/// Has the screen moved on since `id` was issued?
fn stale(id: u32) -> bool {
    js::get_num(&mjs::store_ref("serp"), "reqId") != f64::from(id)
}

/// The modes the search bar offers, in the order it offers them. `crawl` is
/// first because it is the default for any UI search.
pub const MODES: &[&str] = &["crawl", "search", "think", "ontology", "graph", "graphql"];

pub fn register() {
    meta();
    nav();
    page();
    serp();
}

// ── the graph's own identity: ontology, counts, schema ──────────────────────

fn meta() {
    let mut d = Def::new();
    d.bool("loaded", false)
        .str("ontology", "brain")
        .str("brain", "")
        .num("entities", 0.0)
        .num("links", 0.0)
        .num("vectors", 0.0)
        .str("indexedAt", "")
        .val("classes", &js::new_array().into())
        .val("relations", &js::new_array().into())
        .str("error", "");
    mjs::store("meta", &d.finish());
}

/// Fetched once per boot. Cheap, and every page's sidebar reads it.
pub fn load_meta() {
    let s = mjs::store_ref("meta");
    if js::get_bool(&s, "loaded") {
        return;
    }
    api::get("/api/meta", None, move |v| {
        let s = mjs::store_ref("meta");
        for k in ["ontology", "brain", "indexedAt"] {
            js::set_str(&s, k, &js::get_str(&v, k));
        }
        for k in ["entities", "links", "vectors"] {
            js::set_num(&s, k, js::get_num(&v, k));
        }
        for k in ["classes", "relations"] {
            js::set(&s, k, &js::get(&v, k));
        }
        js::set_bool(&s, "loaded", true);
        document_title(&format!("{} — brain", js::get_str(&v, "ontology")));
        mjs::redraw();
    });
}

// ── navigation ──────────────────────────────────────────────────────────────

fn nav() {
    let mut d = Def::new();
    d
        // One handler for every internal link on the page, attached to a
        // container rather than to each anchor: a page that lists a hub's
        // 36,000 neighbours should not install 36,000 listeners.
        .method("link", |_this, args| {
            let ev = js::arg(&args, 0);
            if js::get_bool(&ev, "ctrlKey") || js::get_bool(&ev, "metaKey") || js::get_bool(&ev, "shiftKey") {
                return JsValue::UNDEFINED;
            }
            let target = js::get(&ev, "target");
            let anchor = js::call(&target, "closest", &[JsValue::from_str("a")]);
            if anchor.is_falsy() {
                return JsValue::UNDEFINED;
            }
            let href = js::call_string(&anchor, "getAttribute", &[JsValue::from_str("href")]);
            if !href.starts_with('/') {
                return JsValue::UNDEFINED;
            }
            js::call(&ev, "preventDefault", &[]);
            mjs::router_set(&href);
            JsValue::UNDEFINED
        })
        .method("home", |_this, args| {
            let ev = js::arg(&args, 0);
            js::call(&ev, "preventDefault", &[]);
            mjs::router_set("/");
            JsValue::UNDEFINED
        });
    mjs::store("nav", &d.finish());
}

// ── one entity, or one class listing ────────────────────────────────────────

fn page() {
    let mut d = Def::new();
    d.bool("loading", false)
        .str("error", "")
        .str("slug", "")
        .val("entity", &JsValue::NULL)
        .val("listing", &JsValue::NULL)
        .num("offset", 0.0);
    mjs::store("page", &d.finish());
}

pub fn load_entity(cls: &str, id: &str) {
    let s = mjs::store_ref("page");
    let slug = format!("{cls}/{id}");
    js::set_bool(&s, "loading", true);
    js::set_str(&s, "error", "");
    js::set_str(&s, "slug", &slug);
    js::set(&s, "listing", &JsValue::NULL);
    api::get(&format!("/api/entity?slug={}", encode(&slug)), None, move |v| {
        let s = mjs::store_ref("page");
        js::set_bool(&s, "loading", false);
        let err = js::get_str(&v, "error");
        if err.is_empty() {
            js::set(&s, "entity", &v);
            document_title(&format!("{} — brain", js::get_str(&v, "label")));
        } else {
            js::set(&s, "entity", &JsValue::NULL);
            js::set_str(&s, "error", &err);
        }
        mjs::redraw();
    });
}

pub fn load_class(cls: &str) {
    let s = mjs::store_ref("page");
    js::set_bool(&s, "loading", true);
    js::set_str(&s, "error", "");
    js::set_str(&s, "slug", cls);
    js::set(&s, "entity", &JsValue::NULL);
    let title = cls.to_string();
    let cls_q = cls.to_string();
    api::get(&format!("/api/class?name={}", encode(&cls_q)), None, move |v| {
        let s = mjs::store_ref("page");
        js::set_bool(&s, "loading", false);
        js::set(&s, "listing", &v);
        document_title(&format!("{title} — brain"));
        mjs::redraw();
    });
}

// ── the SERP ────────────────────────────────────────────────────────────────

fn serp() {
    let mut d = Def::new();
    d.str("q", "")
        .str("mode", "crawl")
        .bool("loading", false)
        .str("error", "")
        .val("crawl", &JsValue::NULL)
        .val("think", &JsValue::NULL)
        .val("ontology", &JsValue::NULL)
        .val("graph", &JsValue::NULL)
        .val("graphql", &JsValue::NULL)
        .val("hits", &js::new_array().into())
        // Live-request bookkeeping. `reqId` is the guard above; `startedAt` seeds
        // the stopwatch; `steps` is the crawl's progress feed, appended to one
        // entry per SSE event; `fallback` explains a mode switch the user did not
        // ask for -- without it an ontology answer under a "crawl" heading reads
        // like a bug in the heading.
        .num("reqId", 0.0)
        .num("startedAt", 0.0)
        .val("steps", &js::new_array().into())
        // Two scalars the step handler maintains alongside `steps`, so the
        // template reads a property instead of reaching for the last array
        // element -- markup that has to index a live list is markup that
        // mis-renders the moment the list is empty.
        .num("liveCalls", 0.0)
        .str("liveStage", "")
        .val("fallback", &JsValue::NULL)
        // The bar is one input and one selector, and both write straight back
        // here so the typed query survives a swap.
        .method("setQ", |_this, args| {
            let ev = js::arg(&args, 0);
            let v = js::get_str(&js::get(&ev, "target"), "value");
            js::set_str(&mjs::store_ref("serp"), "q", &v);
            JsValue::UNDEFINED
        })
        .method("setMode", |_this, args| {
            let ev = js::arg(&args, 0);
            let v = js::get_str(&js::get(&ev, "target"), "value");
            js::set_str(&mjs::store_ref("serp"), "mode", &v);
            JsValue::UNDEFINED
        })
        .method("submit", |_this, args| {
            let ev = js::arg(&args, 0);
            if !ev.is_falsy() {
                js::call(&ev, "preventDefault", &[]);
            }
            let s = mjs::store_ref("serp");
            let q = js::get_str(&s, "q").trim().to_string();
            if q.is_empty() {
                return JsValue::UNDEFINED;
            }
            let mode = js::get_str(&s, "mode");
            // m-js's Router matches on the PATHNAME and drops a query string,
            // so the search state rides in the path. `encodeURIComponent`
            // escapes `/`, so a question with a slash in it is still one
            // segment.
            mjs::router_set(&format!("/search/{}/{mode}", encode(&q)));
            JsValue::UNDEFINED
        })
        .method("rerun", |_this, args| {
            let mode = js::arg_str(&args, 0);
            let s = mjs::store_ref("serp");
            let q = js::get_str(&s, "q");
            if q.trim().is_empty() {
                return JsValue::UNDEFINED;
            }
            mjs::router_set(&format!("/search/{}/{mode}", encode(&q)));
            JsValue::UNDEFINED
        });
    mjs::store("serp", &d.finish());
}

/// Run the query named in the address bar. The URL is the state: a pasted link,
/// an edited query string and a Back press all land here and all produce the
/// same page.
pub fn run_query(q: &str, mode: &str) {
    let s = mjs::store_ref("serp");
    let id = NEXT_REQ.fetch_add(1, Ordering::Relaxed);

    // Abandon whatever the previous search was still doing. Closing the stream is
    // not tidiness: the server's next write to us fails, its progress sink fails,
    // and the walk stops -- so an abandoned search stops costing the shared GPU.
    STREAM.with(|c| {
        if let Some(live) = c.borrow_mut().take() {
            js::call(&live.source, "close", &[]);
        }
    });

    js::set_str(&s, "q", q);
    js::set_str(&s, "mode", mode);
    js::set_bool(&s, "loading", true);
    js::set_str(&s, "error", "");
    js::set_num(&s, "reqId", f64::from(id));
    js::set_num(&s, "startedAt", js_sys::Date::now());
    for k in ["crawl", "think", "ontology", "graph", "graphql", "fallback"] {
        js::set(&s, k, &JsValue::NULL);
    }
    js::set(&s, "hits", &js::new_array().into());
    js::set(&s, "steps", &js::new_array().into());
    js::set_num(&s, "liveCalls", 0.0);
    js::set_str(&s, "liveStage", "");
    document_title(&format!("{q} — brain"));

    // The rows first, always. BM25 + RRF contacts no model and comes back in
    // about a tenth of a second, so nothing justifies a blank page while a crawl
    // thinks -- and a user who can see the candidates can judge the answer the
    // moment it lands.
    {
        let path = format!("/api/query?q={}&mode=search", encode(q));
        api::get(&path, None, move |v| {
            if stale(id) {
                return;
            }
            let s = mjs::store_ref("serp");
            let search = js::get(&v, "search");
            if !js::is_missing(&search) && !search.is_null() {
                js::set(&s, "hits", &js::get(&search, "hits"));
            }
            mjs::redraw();
        });
    }

    if mode == "crawl" {
        stream_crawl(q, id);
    } else {
        let path = format!("/api/query?q={}&mode={}", encode(q), encode(mode));
        api::get(&path, None, move |v| apply_answer(id, v));
    }
}

/// Paint one mode's answer -- from the stream or from JSON, they are the same
/// shape deliberately, so this function does not care which delivered it.
fn apply_answer(id: u32, v: JsValue) {
    // The screen moved on. Dropping it here is the whole point of the id.
    if stale(id) {
        return;
    }
    let s = mjs::store_ref("serp");
    js::set_bool(&s, "loading", false);
    let err = js::get_str(&v, "error");
    if !err.is_empty() {
        js::set_str(&s, "error", &err);
        mjs::redraw();
        return;
    }
    for k in ["crawl", "think", "ontology", "graph", "graphql", "fallback"] {
        let got = js::get(&v, k);
        if !js::is_missing(&got) {
            js::set(&s, k, &got);
        }
    }
    // Prefer the rows already painted by the fast fetch; take these only if the
    // first one has not landed (or failed), so a slow rows request can be
    // overtaken by the answer carrying its own copy rather than staying empty.
    let search = js::get(&v, "search");
    let have = js::get_num(&js::get(&s, "hits"), "length");
    if have == 0.0 && !js::is_missing(&search) && !search.is_null() {
        js::set(&s, "hits", &js::get(&search, "hits"));
    }
    mjs::redraw();
}

/// Follow one crawl as it happens. Falls back to the plain JSON request if the
/// stream is unavailable or breaks: the final `result` event is byte-identical to
/// what `/api/query` returns, so losing the progress feed never has to lose the
/// answer.
fn stream_crawl(q: &str, id: u32) {
    let json_only = |q: String, id: u32| {
        let path = format!("/api/query?q={}&mode=crawl", encode(&q));
        api::get(&path, None, move |v| apply_answer(id, v));
    };

    let ctor = js::get(&js::global(), "EventSource");
    if js::is_missing(&ctor) || ctor.is_null() {
        json_only(q.to_string(), id);
        return;
    }
    let args = js::new_array();
    js::call(&args, "push", &[JsValue::from_str(&format!("/api/stream?q={}", encode(q)))]);
    // `Reflect::construct` is typed on a callable, so the constructor has to be
    // narrowed to a Function first -- and if `EventSource` is somehow not a
    // function on this host, that is a reason to take the JSON path, not a panic.
    let Ok(ctor_fn) = ctor.dyn_into::<js_sys::Function>() else {
        json_only(q.to_string(), id);
        return;
    };
    let Ok(source) = js_sys::Reflect::construct(&ctor_fn, &args) else {
        json_only(q.to_string(), id);
        return;
    };

    let on_step = Closure::wrap(Box::new(move |ev: JsValue| {
        if stale(id) {
            return;
        }
        let data = js::get_str(&ev, "data");
        let Ok(p) = js_sys::JSON::parse(&data) else { return };
        let s = mjs::store_ref("serp");
        js::call(&js::get(&s, "steps"), "push", &[p.clone()]);
        js::set_num(&s, "liveCalls", js::get_num(&p, "calls"));
        js::set_str(&s, "liveStage", &js::get_str(&p, "stage"));
        mjs::redraw();
    }) as Box<dyn FnMut(JsValue)>);

    let q_for_result = q.to_string();
    let on_result = Closure::wrap(Box::new(move |ev: JsValue| {
        let data = js::get_str(&ev, "data");
        let Ok(v) = js_sys::JSON::parse(&data) else {
            json_only(q_for_result.clone(), id);
            return;
        };
        apply_answer(id, v);
    }) as Box<dyn FnMut(JsValue)>);

    let q_for_error = q.to_string();
    let on_error = Closure::wrap(Box::new(move |_ev: JsValue| {
        // EventSource fires `error` for a dropped connection and then retries on
        // its own. We do not want that here: a retry re-runs a crawl, and a crawl
        // is billed to a GPU everyone shares. Close, and take the JSON answer.
        STREAM.with(|c| {
            if let Some(live) = c.borrow_mut().take() {
                js::call(&live.source, "close", &[]);
            }
        });
        if !stale(id) {
            json_only(q_for_error.clone(), id);
        }
    }) as Box<dyn FnMut(JsValue)>);

    js::call(&source, "addEventListener", &[JsValue::from_str("step"), on_step.as_ref().clone()]);
    js::call(&source, "addEventListener", &[JsValue::from_str("result"), on_result.as_ref().clone()]);
    js::call(&source, "addEventListener", &[JsValue::from_str("error"), on_error.as_ref().clone()]);

    STREAM.with(|c| {
        *c.borrow_mut() = Some(Live { source, _step: on_step, _result: on_result, _error: on_error })
    });
}

/// The elapsed clock. Mounted once per boot; it does nothing while a search is
/// not running.
///
/// Direct DOM, one text node, ~30 Hz -- deliberately NOT a store field. m-js's
/// redraw is the right tool for structure (a new step row appearing) and the
/// wrong one for a counter that changes every frame: at three decimal places the
/// store would be rewriting the subtree 30 times a second to move a few
/// characters. So m-js owns the node and this writes its textContent.
pub fn start_clock() {
    lifecycle::every(33.0, || {
        let s = mjs::store_ref("serp");
        if !js::get_bool(&s, "loading") {
            return;
        }
        let start = js::get_num(&s, "startedAt");
        if start == 0.0 {
            return;
        }
        let el = js::call(&js::get(&js::window(), "document"), "getElementById", &[JsValue::from_str("brain-elapsed")]);
        if el.is_falsy() {
            return;
        }
        let secs = (js_sys::Date::now() - start) / 1000.0;
        js::set_str(&el, "textContent", &format!("{secs:.3}s"));
    });
}

// ── small bridges ───────────────────────────────────────────────────────────

fn encode(s: &str) -> String {
    js::call_string(&js::global(), "encodeURIComponent", &[JsValue::from_str(s)])
}

fn document_title(t: &str) {
    let doc = js::get(&js::global(), "document");
    js::set_str(&doc, "title", t);
}
