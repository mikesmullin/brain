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

use rustex1_kit::{api, js, mjs, mjs::Def};
use wasm_bindgen::prelude::*;

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
    js::set_str(&s, "q", q);
    js::set_str(&s, "mode", mode);
    js::set_bool(&s, "loading", true);
    js::set_str(&s, "error", "");
    for k in ["crawl", "think", "ontology", "graph", "graphql"] {
        js::set(&s, k, &JsValue::NULL);
    }
    js::set(&s, "hits", &js::new_array().into());
    document_title(&format!("{q} — brain"));
    let path = format!("/api/query?q={}&mode={}", encode(q), encode(mode));
    api::get(&path, None, move |v| {
        let s = mjs::store_ref("serp");
        js::set_bool(&s, "loading", false);
        let err = js::get_str(&v, "error");
        if !err.is_empty() {
            js::set_str(&s, "error", &err);
            mjs::redraw();
            return;
        }
        for k in ["crawl", "think", "ontology", "graph", "graphql"] {
            let got = js::get(&v, k);
            if !js::is_missing(&got) {
                js::set(&s, k, &got);
            }
        }
        let search = js::get(&v, "search");
        if !js::is_missing(&search) {
            js::set(&s, "hits", &js::get(&search, "hits"));
        }
        mjs::redraw();
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
