//! brain's wiki frontend — Rust, compiled to wasm, rendering through m-js.
//!
//! This is the whole client. There is no JavaScript in this repository except
//! the two bridge files the rustex1 kit provides (`app.mjs` boots the wasm,
//! `hot.mjs` speaks m-js's hot-reload protocol). Components, templates, state,
//! routes and networking are all Rust, so editing a page is `cargo`'s problem
//! and not a second toolchain's.
//!
//! What the UI is, and is not: v1 put two million entities on screen as a WebGL
//! point cloud and an agent in a sidebar. Both are gone. This is a wiki — one
//! page per entity, set for reading a paragraph — with a search bar that runs
//! `crawl` by default, because `crawl` is the only mode that takes a sentence
//! and still cannot name a node that does not exist.

use rustex1_kit::{self as kit, App, Runtime};
use wasm_bindgen::prelude::*;

mod pages;
mod store;

struct Brain;

impl App for Brain {
    fn register_stores(&self, _rt: &mut Runtime) {
        store::register();
    }

    fn register_routes(&self, rt: &mut Runtime) {
        pages::register(rt);
    }

    /// No boot toast. The page already says the wasm came back by rendering at
    /// all, and "brain wasm is up", every boot, forever, is not information.
    fn boot_toast(&self) -> Option<(String, String)> {
        None
    }

    fn started(&self, _rt: &mut Runtime) {
        // The schema and the counts are fetched once and kept: they change only
        // when someone reindexes, and every page's sidebar reads them.
        store::load_meta();
        // One clock loop for the life of the instance. It is inert whenever no
        // search is in flight, so paying for it on the home page costs nothing.
        store::start_clock();
    }
}

/// Called by the kit's `app.mjs` — on first load and after every wasm rebuild.
#[wasm_bindgen]
pub fn boot(m: JsValue, router: JsValue, hooks: JsValue, opts: JsValue) -> Result<(), JsValue> {
    kit::boot(m, router, hooks, opts, &Brain)
}

// ── console surface ─────────────────────────────────────────────────────────

#[wasm_bindgen(js_name = buildInfo)]
pub fn build_info() -> String {
    kit::build_info()
}

#[wasm_bindgen(js_name = debugState)]
pub fn debug_state() -> String {
    kit::debug_state()
}

#[wasm_bindgen(js_name = dumpTemplate)]
pub fn dump_template(name: &str) -> String {
    kit::template_of(name)
}

#[wasm_bindgen(js_name = lintReport)]
pub fn lint_report() -> String {
    kit::lint_report()
}
