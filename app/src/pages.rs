//! The three pages: home, one entity (or one class), and the SERP.
//!
//! A page is an `m()` tree handed to `Def::tree`. The tree compiles to an m-js
//! template string, so the directives (`x-for`, `x-text`, `:href`, `@click`)
//! are the same ones a hand-written template would use — the gain is that the
//! structure is a Rust expression the compiler checks, and the kit's linter
//! checks the directives before anything paints.

use rustex1_kit::h::{lit, m, Node};
use rustex1_kit::mjs::Def;
use rustex1_kit::{js, mjs, Runtime};
use wasm_bindgen::prelude::*;

use crate::store;

pub fn register(_rt: &mut Runtime) {
    mjs::router_set_mode("path");
    mjs::router_register("/", "brain", |_| home());
    // The search state is in the path, not a query string: m-js's Router
    // matches on the pathname and drops everything after `?`.
    mjs::router_register("/search/:q", "brain · search", |p| search(p));
    mjs::router_register("/search/:q/:mode", "brain · search", |p| search(p));
    // m-js's `:param` matches one path segment, so a slug's two halves are two
    // params — which is also the distinction the page wants: one segment is a
    // class listing, two is an entity.
    mjs::router_register("/e/:cls", "brain · class", |p| class_page(p));
    mjs::router_register("/e/:cls/:id", "brain · entity", |p| entity_page(p));
}

// ── chrome ──────────────────────────────────────────────────────────────────

/// The top bar: the brand, and the search bar that is the point of the whole
/// page. `@submit.prevent` is deliberately not used — the handler calls
/// `preventDefault` itself, so the same method works from a button, from Enter,
/// and from a mode switch that re-runs the query.
fn bar() -> Node {
    let modes: Vec<Node> = store::MODES
        .iter()
        .map(|md| {
            m("option")
                .attr("value", md)
                .bind("selected", &format!("$store.serp.mode === '{md}'"))
                .child(lit(md))
        })
        .collect();
    m("header").class("bar").kids(vec![
        m("a")
            .class("brand")
            .attr("href", "/")
            .on("click", "$store.nav.home($event)")
            .kids(vec![
                m("span").class("glyph").child(lit("🧠")),
                m("span").child(lit("brain")),
                m("span").class("onto").text("$store.meta.ontology"),
            ]),
        m("form")
            .class("search")
            .on("submit", "$store.serp.submit($event)")
            .kids(vec![
                m("input")
                    .attr("name", "q")
                    .attr("autocomplete", "off")
                    .attr("spellcheck", "false")
                    .attr("placeholder", "Ask a question, or search…")
                    .bind("value", "$store.serp.q")
                    .on("input", "$store.serp.setQ($event)"),
                m("select").on("change", "$store.serp.setMode($event)").kids(modes),
                m("button").attr("type", "submit").child(lit("Ask")),
            ]),
    ])
}

/// Classes with their counts, and the graph's size. Every page carries it, so
/// the schema is always one click away from whatever you are reading.
fn sidebar() -> Node {
    m("aside").kids(vec![
        m("div").class("box").kids(vec![
            m("h3").child(lit("Classes")),
            m("ul").on("click", "$store.nav.link($event)").child(
                m("li")
                    .for_each("c in $store.meta.classes.filter(x => x.count > 0)")
                    .key("c.name")
                    .kids(vec![
                        m("a").bind("href", "'/e/' + c.name").text("c.name"),
                        m("span").class("n").text("c.count.toLocaleString()"),
                    ]),
            ),
        ]),
        m("div").class("box").kids(vec![
            m("h3").child(lit("Graph")),
            m("ul").kids(vec![
                stat_row("entities", "$store.meta.entities.toLocaleString()"),
                stat_row("edges", "$store.meta.links.toLocaleString()"),
                stat_row("relations", "$store.meta.relations.length"),
            ]),
        ]),
    ])
}

fn stat_row(label: &str, expr: &str) -> Node {
    m("li").kids(vec![
        m("span").child(lit(label)),
        m("span").class("n").text(expr),
    ])
}

/// Bar + two columns. Every page has the same shape, so the search bar never
/// moves between pages.
fn shell(body: Vec<Node>) -> Node {
    m("div").kids(vec![
        bar(),
        m("main").child(
            m("div")
                .class("cols")
                .kids(vec![m("div").on("click", "$store.nav.link($event)").kids(body), sidebar()]),
        ),
    ])
}

fn spinner(expr: &str) -> Node {
    m("div").class("spinner").iff("$store.page.loading").text(expr)
}

// ── home ────────────────────────────────────────────────────────────────────

fn home() -> JsValue {
    let mut d = Def::new().named("route /");
    d.tree(shell(vec![
        m("div").class("kicker").child(lit("knowledge graph")),
        m("h1").text("$store.meta.ontology"),
        m("p").class("lede").child(lit(
            "Every entity is one Markdown file with typed fields and typed edges. \
Ask a question in the bar above and the crawler walks the real graph to answer it — \
one decision per hop, over options the engine already fetched, so a path it reports \
is a path that exists.",
        )),
        m("div").class("stats").kids(vec![
            big("$store.meta.entities.toLocaleString()", "entities"),
            big("$store.meta.links.toLocaleString()", "edges"),
            big("$store.meta.classes.length", "classes"),
            big("$store.meta.relations.length", "relations"),
        ]),
        m("h2").child(lit("Browse by class")),
        m("div").class("grid").child(
            m("a")
                .class("card")
                .for_each("c in $store.meta.classes.filter(x => x.count > 0)")
                .key("c.name")
                .bind("href", "'/e/' + c.name")
                .kids(vec![
                    m("div").class("t").text("c.name"),
                    m("div").class("n").text("c.count.toLocaleString() + ' entities'"),
                ]),
        ),
        m("h2").child(lit("Relations")),
        m("div").class("rel").child(
            m("div").for_each("r in $store.meta.relations").key("r.name").kids(vec![
                m("span").class("name").text("r.name"),
                m("span")
                    .class("qual")
                    .text("' ' + (r.domain || '*') + ' → ' + (r.range || '*') + '  [' + r.cardinality + ']' + (r.transitive ? ' transitive' : '') + (r.symmetric ? ' symmetric' : '') + (r.inverseOf ? ' inverseOf ' + r.inverseOf : '')"),
            ]),
        ),
    ]));
    d.finish()
}

fn big(value: &str, label: &str) -> Node {
    m("div").kids(vec![m("b").text(value), lit(label)])
}

// ── one class ───────────────────────────────────────────────────────────────

fn class_page(params: JsValue) -> JsValue {
    let cls = js::get_str(&params, "cls");
    store::load_class(&cls);
    let mut d = Def::new().named("route /e/:cls");
    d.tree(shell(vec![
        spinner("'reading…'"),
        m("div").class("err").iff("$store.page.error").text("$store.page.error"),
        m("div").iff("$store.page.listing").kids(vec![
            m("div").class("kicker").child(lit("class")),
            m("h1").text("$store.page.listing.class"),
            m("div").class("grid").child(
                m("a")
                    .class("card")
                    .for_each("x in $store.page.listing.entities")
                    .key("x.slug")
                    .bind("href", "'/e/' + x.slug")
                    .kids(vec![
                        m("div").class("t").text("x.label"),
                        m("div").class("n").text("x.slug"),
                    ]),
            ),
            m("div")
                .class("empty")
                .iff("$store.page.listing && !$store.page.listing.entities.length")
                .child(lit("Nothing in this class yet.")),
        ]),
    ]));
    d.finish()
}

// ── one entity ──────────────────────────────────────────────────────────────

fn entity_page(params: JsValue) -> JsValue {
    let cls = js::get_str(&params, "cls");
    let id = js::get_str(&params, "id");
    store::load_entity(&cls, &id);

    let mut d = Def::new().named("route /e/:cls/:id");
    // `fmt` and `quals` are page methods rather than template expressions: a
    // field's value can be a list, a number, a block of prose or a nested
    // object, and deciding that in a template is how a template becomes a
    // program.
    d.method("fmt", |_this, args| {
        let v = js::arg(&args, 0);
        JsValue::from_str(&format_value(&v))
    })
    .method("quals", |_this, args| {
        let list = js::arg(&args, 0);
        let n = js::get_num(&list, "length") as u32;
        let mut parts: Vec<String> = Vec::new();
        for i in 0..n {
            let q = js::get(&list, &i.to_string());
            parts.push(format!("{}={}", js::get_str(&q, "name"), format_value(&js::get(&q, "value"))));
        }
        JsValue::from_str(&parts.join(" "))
    })
    // [[wiki-links]] become links and paragraphs become paragraphs. There is no
    // Markdown engine here on purpose: an entity's body is prose, not a
    // document, and the fields above it carry the structure.
    .method("prose", |_this, args| {
        let text = js::arg_str(&args, 0);
        JsValue::from_str(&render_prose(&text))
    })
    .tree(shell(vec![
        spinner("'reading…'"),
        m("div").class("err").iff("$store.page.error").text("$store.page.error"),
        m("article").iff("$store.page.entity").kids(vec![
            m("header").kids(vec![
                m("div").class("kicker").child(
                    m("a")
                        .bind("href", "'/e/' + $store.page.entity.class")
                        .text("$store.page.entity.class"),
                ),
                m("h1").text("$store.page.entity.label"),
                m("div").class("slug").text("$store.page.entity.slug"),
            ]),
            m("div")
                .class("prose")
                .iff("$store.page.entity.body && $store.page.entity.body.trim()")
                .inner_html("prose($store.page.entity.body)"),
            // components
            m("div").for_each("c in $store.page.entity.components").key("c.alias").kids(vec![
                m("h2").text("c.alias"),
                m("dl").class("fields").child(
                    m("template").for_each("f in c.fields").key("f.name").kids(vec![
                        m("dt").text("f.name"),
                        m("dd")
                            .iff("Array.isArray(f.value)")
                            .child(m("div").class("chips").child(
                                m("span").class("chip").for_each("v in f.value").key("v").text("v"),
                            )),
                        m("dd")
                            .iff("!Array.isArray(f.value)")
                            .bind("class", "String(f.value).length > 120 ? 'long' : ''")
                            .text("fmt(f.value)"),
                    ]),
                ),
            ]),
            // out-edges
            m("div").iff("$store.page.entity.out.length").kids(vec![
                m("h2").child(lit("Relations")),
                edge_block("r in $store.page.entity.out", false),
            ]),
            // in-edges
            m("div").iff("$store.page.entity.in.length").kids(vec![
                m("h2").child(lit("Referenced by")),
                edge_block("r in $store.page.entity.in", true),
            ]),
            m("h2").child(lit("Source")),
            m("pre").text("$store.page.entity.markdown"),
        ]),
    ]));
    d.finish()
}

fn edge_block(for_expr: &str, incoming: bool) -> Node {
    m("div").class("rel").for_each(for_expr).key("r.rel").kids(vec![
        m("span")
            .class(if incoming { "name in" } else { "name" })
            .text("r.rel"),
        m("ul").child(
            m("li").for_each("t in r.targets").key("t.slug").kids(vec![
                m("a").bind("href", "'/e/' + t.slug").text("t.label"),
                m("span")
                    .class("qual")
                    .iff("t.qualifiers && t.qualifiers.length")
                    .text("quals(t.qualifiers)"),
            ]),
        ),
    ])
}

// ── the SERP ────────────────────────────────────────────────────────────────

fn search(params: JsValue) -> JsValue {
    // The URL is the state. Reading it from the route params rather than from
    // the store is what makes a pasted link, an edited URL and a Back press all
    // produce the same page.
    let q = decode(&js::get_str(&params, "q"));
    let mode = match js::get_str(&params, "mode") {
        m if m.is_empty() => "crawl".to_string(),
        m => m,
    };
    store::run_query(&q, &mode);

    let mut d = Def::new().named("route /search");
    d.method("crawlMeta", |_this, args| {
        let c = js::arg(&args, 0);
        let kind = js::get_str(&c, "kind");
        let mut parts: Vec<String> = Vec::new();
        if !kind.is_empty() {
            parts.push(format!("kind {kind}"));
        }
        parts.push(format!("mode {}", js::get_str(&c, "crawl_mode")));
        parts.push(format!("{} step(s)", js::get_num(&c, "steps") as u64));
        parts.push(format!("{} decision(s)", js::get_num(&c, "calls") as u64));
        parts.push(format!("{}ms", js::get_num(&c, "ms") as u64));
        JsValue::from_str(&parts.join(" · "))
    })
    .method("pathHtml", |_this, args| {
        let p = js::arg(&args, 0);
        let nodes = js::get(&p, "nodes");
        let via = js::get(&p, "via");
        let n = js::get_num(&nodes, "length") as u32;
        let mut out = String::new();
        for i in 0..n {
            let node = js::get_str(&nodes, &i.to_string());
            if i > 0 {
                let rel = js::get_str(&via, &(i - 1).to_string());
                out.push_str(&format!(r#" <span class="via">--{}--&gt;</span> "#, escape(&rel)));
            }
            out.push_str(&format!(
                r#"<a href="/e/{}">{}</a>"#,
                escape(&node),
                escape(&node)
            ));
        }
        JsValue::from_str(&out)
    })
    .tree(shell(vec![
        m("div").class("kicker").text("$store.serp.mode + ' · ' + $store.serp.q"),
        m("div")
            .class("spinner")
            .iff("$store.serp.loading")
            .text("$store.serp.mode === 'crawl' ? 'walking the graph…' : 'searching…'"),
        m("div").class("err").iff("$store.serp.error").text("$store.serp.error"),
        // crawl
        m("div").class("answer").iff("$store.serp.crawl").kids(vec![
            m("div").class("label").child(lit("Answer")),
            m("div").class("text").iff("$store.serp.crawl.answer").text("$store.serp.crawl.answer"),
            m("div")
                .class("text")
                .iff("$store.serp.crawl && !$store.serp.crawl.answer && $store.serp.crawl.labels.length")
                .text("$store.serp.crawl.labels.join(', ')"),
            m("div")
                .class("text")
                .iff("$store.serp.crawl && !$store.serp.crawl.answer && !$store.serp.crawl.labels.length")
                .child(lit("Nothing in this graph answers that.")),
            m("div")
                .class("cites")
                .iff("$store.serp.crawl.slugs.length")
                .on("click", "$store.nav.link($event)")
                .child(
                    m("a")
                        .class("chip")
                        .for_each("s in $store.serp.crawl.slugs")
                        .key("s")
                        .bind("href", "'/e/' + s")
                        .text("s"),
                ),
            m("div").class("meta").text("crawlMeta($store.serp.crawl)"),
        ]),
        // think / ontology — same shape, different engine
        answer_card("think", "Think", "$store.serp.think.answer", "$store.serp.think.sources"),
        answer_card(
            "ontology",
            "Ontology traversal",
            "$store.serp.ontology.answer",
            "$store.serp.ontology.slugs",
        ),
        // graph
        m("div").iff("$store.serp.graph").kids(vec![
            m("h2").text("$store.serp.graph.paths.length + ' path(s)' + ($store.serp.graph.capped ? ' (capped)' : '')"),
            m("div")
                .class("hit")
                .for_each("p in $store.serp.graph.paths")
                .key("p.nodes.join('>')")
                .child(m("div").class("title").inner_html("pathHtml(p)")),
            m("div")
                .class("empty")
                .iff("$store.serp.graph && !$store.serp.graph.paths.length")
                .child(lit("No path matches that pattern.")),
        ]),
        // graphql
        m("div").iff("$store.serp.graphql").kids(vec![
            m("h2").child(lit("Projection")),
            m("pre").text("JSON.stringify($store.serp.graphql, null, 2)"),
        ]),
        // the ranked rows, under whatever answered above. An answer you cannot
        // check is worse than one you can.
        m("div").iff("$store.serp.hits.length").kids(vec![
            m("h2").text("($store.serp.crawl ? 'Evidence' : 'Results') + ' · ' + $store.serp.hits.length"),
            m("div").class("hit").for_each("h in $store.serp.hits").key("h.slug").kids(vec![
                m("div").class("path").text("h.slug"),
                m("div")
                    .class("title")
                    .child(m("a").bind("href", "'/e/' + h.slug").text("h.label")),
                m("div").class("snippet").iff("h.snippet").text("h.snippet"),
                m("div").class("via").text("h.via"),
            ]),
        ]),
        m("div")
            .class("empty")
            .iff("!$store.serp.loading && !$store.serp.hits.length && !$store.serp.crawl && !$store.serp.graph && !$store.serp.graphql && !$store.serp.think && !$store.serp.ontology && !$store.serp.error")
            .child(lit("No matches.")),
    ]));
    d.finish()
}

fn answer_card(key: &str, label: &str, text_expr: &str, cites_expr: &str) -> Node {
    m("div")
        .class("answer")
        .iff(&format!("$store.serp.{key}"))
        .kids(vec![
            m("div").class("label").child(lit(label)),
            m("div").class("text").text(text_expr),
            m("div")
                .class("cites")
                .on("click", "$store.nav.link($event)")
                .child(
                    m("a")
                        .class("chip")
                        .for_each(&format!("s in {cites_expr}"))
                        .key("s")
                        .bind("href", "'/e/' + s")
                        .text("s"),
                ),
        ])
}

// ── value rendering ─────────────────────────────────────────────────────────

fn format_value(v: &JsValue) -> String {
    if v.is_null() || v.is_undefined() {
        return String::new();
    }
    if let Some(s) = v.as_string() {
        return s;
    }
    if let Some(n) = v.as_f64() {
        return if n.fract() == 0.0 {
            format!("{}", n as i64)
        } else {
            format!("{n}")
        };
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    // an object or a nested list: show it, rather than "[object Object]"
    let json = js::get(&js::global(), "JSON");
    js::call_string(&json, "stringify", &[v.clone(), JsValue::NULL, JsValue::from_f64(1.0)])
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn render_prose(text: &str) -> String {
    text.split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("<p>{}</p>", link_wikis(&escape(p))))
        .collect::<Vec<_>>()
        .join("")
}

/// `[[Some Entity]]` -> a link. The target is a label, so the server resolves
/// it the same way a typed slug is resolved.
fn link_wikis(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("[[") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        match after.find("]]") {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&format!(r#"<a href="/e/{name}">{name}</a>"#));
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[at..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode(s: &str) -> String {
    js::call_string(&js::global(), "decodeURIComponent", &[JsValue::from_str(s)])
}
