//! bench.rs — the Panama Papers benchmark, and a project's own question suite.
//!
//! `docs/BENCHMARK.md` holds the protocol and the published results. The short
//! version, because it shapes this code:
//!
//! * **The prime metric is WARM query wall-time** for the query subcommands,
//!   against pgGraph's hot timings on the equivalent operations. Cold start and
//!   index-build time are footnotes, not pass/fail bars.
//! * **1 unrecorded warmup, then 10 measured iterations**, min/median/p95.
//! * **Correctness parity first, timing second.** A pair that disagrees on
//!   results gets no timing comparison until the disagreement is explained.
//! * `capped` must fire on BOTH sides of a pair or NEITHER, or the two sides
//!   are answering different questions.
//!
//! brain1 ran this on pglite — a single-threaded WASM Postgres inside one Bun
//! process — and still landed equal-or-better on most paired questions. Those
//! numbers are a floor, not a ceiling, and this is the version that removes the
//! handicap.

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::abox::Entity;
use crate::cli::{Args, Global};
use crate::commands::Ctx;
use crate::config;
use crate::query;
use crate::slug;
use crate::tbox::{Cardinality, ClassDef, ComponentDef, FieldDef, RelationDef, TBox};

pub async fn run(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["zip", "out", "iterations", "seed", "cases", "only"]);
    match a.positional.first().map(|s| s.as_str()).unwrap_or("") {
        "panama-etl" => panama_etl(&a),
        "panama" => panama_suite(&a, g).await,
        "suite" => project_suite(&a, g).await,
        other => bail!("unknown `brain bench` subcommand: '{other}' (see `brain help bench`)"),
    }
}

// ─── ETL: ICIJ CSVs -> a brain dataset ──────────────────────────────────────
//
// The dataset-mapping decisions, carried from brain1's run so the two are
// comparable:
//   A. one entity per category row; where a node_id appears in more than one
//      category file, link the siblings with SAME_NODE_ID (~1,150 pairs).
//   B. one relation per distinct `rel_type`, sanitized to ALL_CAPS, cardinality
//      mtm, and NO domain/range — ICIJ edges cross categories freely and a
//      single-class domain cannot express that. Omitted means wildcard, which
//      validates cleanly.
//   C. rich typed components: one component per class carrying that CSV's
//      actual columns.
//   D. hub nodes uncapped.

const CATEGORIES: &[(&str, &str)] = &[
    ("nodes-entities.csv", "Entity"),
    ("nodes-officers.csv", "Officer"),
    ("nodes-intermediaries.csv", "Intermediary"),
    ("nodes-addresses.csv", "Address"),
    ("nodes-others.csv", "Other"),
];

fn panama_etl(a: &Args) -> Result<i32> {
    let zip = a
        .get("zip")
        .map(config::expand)
        .context("--zip <full-oldb.LATEST.zip> is required")?;
    let out = a.get("out").map(config::expand).unwrap_or_else(|| PathBuf::from("./panama-brain/db"));
    let t0 = Instant::now();
    std::fs::create_dir_all(&out)?;

    // Extract into a scratch dir beside the output; the CSVs are read twice
    // (once for nodes, once for edges) and streaming a zip twice is slower than
    // paying for the bytes once.
    let raw = out.join(".icij-csv");
    std::fs::create_dir_all(&raw)?;
    let present = CATEGORIES.iter().all(|(f, _)| raw.join(f).exists()) && raw.join("relationships.csv").exists();
    if !present {
        eprintln!("extracting {} …", zip.display());
        let file = std::fs::File::open(&zip).with_context(|| format!("opening {}", zip.display()))?;
        let mut ar = zip::ZipArchive::new(file).context("reading the archive")?;
        for i in 0..ar.len() {
            let mut f = ar.by_index(i)?;
            let name = f.name().rsplit('/').next().unwrap_or("").to_string();
            if !name.ends_with(".csv") {
                continue;
            }
            let dst = raw.join(&name);
            let mut bytes = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut bytes)?;
            std::fs::write(&dst, bytes)?;
            eprintln!("  {name}");
        }
    }

    // ── pass 1: nodes ──
    let mut tb = TBox { ontology: "panama".into(), ..Default::default() };
    // node_id -> every class that holds it, for decision A
    let mut owners: HashMap<String, Vec<&'static str>> = HashMap::new();
    let written = AtomicU64::new(0);

    for (file, class) in CATEGORIES {
        let path = raw.join(file);
        if !path.exists() {
            eprintln!("warn: {file} not in the archive — skipping {class}");
            continue;
        }
        eprintln!("nodes: {class} <- {file}");
        let mut rdr = csv::ReaderBuilder::new().flexible(true).from_path(&path)?;
        let headers: Vec<String> = rdr.headers()?.iter().map(|h| h.trim().to_string()).collect();
        let id_col = headers
            .iter()
            .position(|h| h == "node_id")
            .with_context(|| format!("{file} has no node_id column"))?;

        // decision C: one component per class, carrying the real columns.
        // Everything is typed `string`: ICIJ dates are `18-NOV-1999`, which no
        // date parser accepts, and bulk indexing does not validate anyway — the
        // types matter for later interactive use, not for load speed.
        let comp = format!("{class}Row");
        let mut fields = std::collections::BTreeMap::new();
        for h in &headers {
            if h == "node_id" || h.is_empty() {
                continue;
            }
            fields.insert(sanitize_field(h), FieldDef::default());
        }
        tb.components.insert(comp.clone(), ComponentDef { fields, methods: vec![] });
        let mut components = std::collections::BTreeMap::new();
        components.insert("row".to_string(), comp);
        tb.classes.insert(
            class.to_string(),
            ClassDef { components, top: true, id_field: None, parent: None },
        );

        let dir = out.join(class);
        std::fs::create_dir_all(&dir)?;

        let mut batch: Vec<csv::StringRecord> = Vec::with_capacity(65536);
        for rec in rdr.records() {
            let Ok(rec) = rec else { continue };
            let Some(id) = rec.get(id_col) else { continue };
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            owners.entry(id.to_string()).or_default().push(class);
            batch.push(rec);
            if batch.len() == 65536 {
                flush_nodes(&dir, class, &headers, id_col, &batch, &written)?;
                batch.clear();
            }
        }
        flush_nodes(&dir, class, &headers, id_col, &batch, &written)?;
        eprintln!("  {} entities so far", written.load(Ordering::Relaxed));
    }

    // ── pass 2: edges ──
    // Appended to each start node's file, which means re-reading the file we
    // just wrote. That is still much cheaper than holding 2M entity objects in
    // memory, which is what cost brain1 a 19.4 GB peak.
    let rel_path = raw.join("relationships.csv");
    if !rel_path.exists() {
        bail!("relationships.csv is not in the archive");
    }
    eprintln!("edges: relationships.csv");
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_path(&rel_path)?;
    let h: Vec<String> = rdr.headers()?.iter().map(|x| x.trim().to_string()).collect();
    let col = |name: &str| h.iter().position(|x| x == name);
    let (Some(ci), Some(co), Some(ct)) = (
        col("node_id_start").or_else(|| col("START_ID")),
        col("node_id_end").or_else(|| col("END_ID")),
        col("rel_type").or_else(|| col("TYPE")).or_else(|| col("link")),
    ) else {
        bail!("relationships.csv has no recognisable start/end/type columns: {h:?}");
    };

    // slug -> "REL\ttarget" lines, accumulated then appended per class dir
    let mut pending: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut rels: std::collections::BTreeSet<String> = Default::default();
    let mut edges = 0u64;
    let mut dropped = 0u64;
    for rec in rdr.records() {
        let Ok(rec) = rec else { continue };
        let (Some(s), Some(e), Some(t)) = (rec.get(ci), rec.get(co), rec.get(ct)) else { continue };
        let (s, e) = (s.trim(), e.trim());
        let rel = slug::sanitize_relation(t);
        if rel.is_empty() {
            continue;
        }
        // skip rows whose endpoints are in no node CSV — pgGraph's loader does
        // the same, so the two sides walk the same edge set
        let (Some(so), Some(eo)) = (owners.get(s), owners.get(e)) else {
            dropped += 1;
            continue;
        };
        let from = format!("{}/{s}", so[0]);
        let to = format!("{}/{e}", eo[0]);
        rels.insert(rel.clone());
        pending.entry(from).or_default().push((rel, to));
        edges += 1;
    }
    // decision A: SAME_NODE_ID between siblings
    let mut same = 0u64;
    for (id, cls) in &owners {
        if cls.len() < 2 {
            continue;
        }
        for w in cls.windows(2) {
            pending
                .entry(format!("{}/{id}", w[0]))
                .or_default()
                .push(("SAME_NODE_ID".to_string(), format!("{}/{id}", w[1])));
            same += 1;
        }
    }
    if same > 0 {
        rels.insert("SAME_NODE_ID".into());
    }
    // decision B: declare every relation, mtm, no domain/range
    for r in &rels {
        tb.relations.insert(
            r.clone(),
            RelationDef { cardinality: Cardinality::Mtm, ..Default::default() },
        );
    }
    tb.validate()?;
    std::fs::write(out.join("schema.yaml"), tb.to_yaml())?;

    eprintln!("appending {edges} edge(s) to {} file(s) …", pending.len());
    let items: Vec<(String, Vec<(String, String)>)> = pending.into_iter().collect();
    items.par_iter().for_each(|(from, list)| {
        let Ok(s) = slug::parse(from) else { return };
        let p = out.join(&s.cls).join(format!("{}.md", s.id));
        let Ok(text) = std::fs::read_to_string(&p) else { return };
        // insert the relation lines just before the closing `---`
        let mut by_rel: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (r, t) in list {
            by_rel.entry(r.as_str()).or_default().push(t.as_str());
        }
        let mut block = String::new();
        for (r, ts) in by_rel {
            block.push_str(r);
            block.push_str(":\n");
            for t in ts {
                block.push_str("- ");
                block.push_str(t);
                block.push('\n');
            }
        }
        // the frontmatter's closing delimiter is the second `---` line
        let mut parts = text.splitn(3, "---\n");
        let (_, front, body) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let _ = std::fs::write(&p, format!("---\n{front}{block}---\n{body}"));
    });

    let n = written.load(Ordering::Relaxed);
    println!("\n{} -> {}", zip.display(), out.display());
    println!("  entities      {n}");
    println!("  edges         {edges}  (+{same} SAME_NODE_ID)");
    println!("  relations     {}", rels.len());
    println!("  dropped       {dropped} edge row(s) with an endpoint in no node CSV");
    println!("  wall          {:.1}s", t0.elapsed().as_secs_f64());
    println!("\nnext:  BRAIN_ROOT={} brain reindex --no-embed", out.display());
    println!("       brain bench panama");
    Ok(0)
}

fn flush_nodes(
    dir: &Path,
    class: &str,
    headers: &[String],
    id_col: usize,
    batch: &[csv::StringRecord],
    written: &AtomicU64,
) -> Result<()> {
    batch.par_iter().for_each(|rec| {
        let Some(id) = rec.get(id_col) else { return };
        let id = id.trim();
        let mut e = Entity::new(class, id);
        let mut fields = serde_json::Map::new();
        for (i, h) in headers.iter().enumerate() {
            if i == id_col || h.is_empty() {
                continue;
            }
            let Some(v) = rec.get(i) else { continue };
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            fields.insert(sanitize_field(h), serde_json::Value::String(v.to_string()));
        }
        e.components.insert("row".to_string(), fields);
        let p = dir.join(format!("{id}.md"));
        let _ = std::fs::write(p, e.to_markdown());
        written.fetch_add(1, Ordering::Relaxed);
    });
    Ok(())
}

fn sanitize_field(h: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in h.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(if upper { c.to_ascii_uppercase() } else { c });
            upper = false;
        } else {
            upper = !out.is_empty();
        }
    }
    if out.is_empty() {
        "value".into()
    } else {
        out
    }
}

// ─── the paired question suite ──────────────────────────────────────────────

struct Timing {
    min: f64,
    median: f64,
    p95: f64,
}

fn summarize(mut ms: Vec<f64>) -> Timing {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = ms.len();
    Timing {
        min: ms.first().copied().unwrap_or(0.0),
        median: if n == 0 { 0.0 } else { ms[n / 2] },
        p95: if n == 0 { 0.0 } else { ms[((n as f64 * 0.95) as usize).min(n - 1)] },
    }
}

/// Warm protocol: one unrecorded warmup, then `iterations` measured runs.
async fn measure<F, Fut, T>(iterations: usize, mut f: F) -> Result<(Timing, T)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let last = f().await?; // warmup, not recorded
    let mut ms = Vec::with_capacity(iterations);
    let mut out = last;
    for _ in 0..iterations {
        let t = Instant::now();
        out = f().await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    Ok((summarize(ms), out))
}

async fn panama_suite(a: &Args, g: &Global) -> Result<i32> {
    let ctx = Ctx::open(g).await?;
    let iterations = a.num("iterations").unwrap_or(10) as usize;
    let st = ctx.store.stats().await?;
    println!("brain bench panama — {} entities / {} edges", st.entities, st.links);
    println!("protocol: 1 warmup + {iterations} measured iterations, warm server, host-side wall clock\n");

    // Seed A: the highest out-degree node. Derived once and reused, so both
    // systems answer the same question.
    let seed = match a.get("seed") {
        Some(s) => ctx.resolve_slug(s).await?,
        None => {
            let top = ctx.store.top_degree(1).await?;
            let Some((slug, deg)) = top.into_iter().next() else { bail!("the index has no edges") };
            println!("seed A (highest out-degree): {slug}  ({deg} edges)");
            slug
        }
    };
    // Target B: any neighbour. Target C: a node 3–4 hops out.
    let out = ctx.store.expand(&[seed.clone()], true, 1).await?;
    let target_b = out.first().map(|l| l.to.clone());
    let probe = query::graph::run(&ctx.store, &ctx.tbox, &format!("{seed} *4> * --limit 2000")).await?;
    let target_c = probe.paths.iter().find(|p| p.hops >= 3).map(|p| p.nodes.last().cloned().unwrap_or_default());
    if let Some(b) = &target_b {
        println!("target B (1 hop):  {b}");
    }
    if let Some(c) = &target_c {
        println!("target C (3 hops): {c}");
    }
    println!();

    let mut rows: Vec<(String, Timing, String)> = Vec::new();

    // Q1 — graph size
    let (t, _) = measure(iterations, || async { ctx.store.stats().await.map(|s| s.entities) }).await?;
    rows.push(("Q1 status / graph size".into(), t, format!("{} entities", st.entities)));

    // Q2 — search "Mossack"
    let (t, r) = measure(iterations, || async {
        query::search::run(
            &ctx.store,
            ctx.embedder.as_ref(),
            "Mossack",
            &query::search::SearchOpts { limit: 20, ..Default::default() },
        )
        .await
    })
    .await?;
    rows.push(("Q2 search \"Mossack\" (top-20)".into(), t, format!("{} hit(s)", r.hits.len())));

    // Q3 — two-hop neighbourhood, capped and uncapped
    let (t, r) = measure(iterations, || async {
        query::graph::run(&ctx.store, &ctx.tbox, &format!("{seed} *2> * --limit 500")).await
    })
    .await?;
    rows.push((
        "Q3a two-hop neighbourhood, capped 500".into(),
        t,
        format!("{} path(s), capped={}", r.paths.len(), r.capped),
    ));
    let (t, r) = measure(iterations.min(3), || async {
        query::graph::run(&ctx.store, &ctx.tbox, &format!("{seed} *2> *")).await
    })
    .await?;
    rows.push((
        "Q3b two-hop neighbourhood, uncapped".into(),
        t,
        format!("{} node(s) visited, capped={}", r.visited, r.capped),
    ));

    // Q4 — one-hop projection through the query DSL
    let comp = ctx
        .tbox
        .classes
        .get(crate::query::crawl::class_of(&seed))
        .and_then(|c| c.components.keys().next().cloned())
        .unwrap_or_else(|| "row".into());
    let rel = ctx.store.out_edges(&seed).await?.first().map(|l| l.rel.clone());
    if let Some(rel) = rel {
        let q = format!("{seed} {{ {comp} {{ name }}, {rel} {{ {comp} {{ name }} }} }}");
        let (t, _) = measure(iterations, || async { query::graphql::run(&ctx.store, &q).await }).await?;
        rows.push(("Q4 one-hop projection (graphql)".into(), t, q));
    }

    // Q5 — shortest path
    for (label, target) in [("1-hop", target_b.clone()), ("3-hop", target_c.clone())] {
        let Some(tg) = target else { continue };
        let pattern = format!("{seed} *6> {tg} --shortest");
        let (t, r) = measure(iterations, || async {
            query::graph::run(&ctx.store, &ctx.tbox, &pattern).await
        })
        .await?;
        rows.push((
            format!("Q5 shortest path, {label} target"),
            t,
            format!(
                "{} path(s), {} hop(s)",
                r.paths.len(),
                r.paths.first().map(|p| p.hops).unwrap_or(0)
            ),
        ));
    }

    let w = rows.iter().map(|r| r.0.len()).max().unwrap_or(30);
    println!("{:<w$}  {:>9}  {:>9}  {:>9}   result", "question", "min ms", "median", "p95");
    for (name, t, note) in &rows {
        println!("{name:<w$}  {:>9.1}  {:>9.1}  {:>9.1}   {note}", t.min, t.median, t.p95);
    }

    if a.has("json") {
        let payload = json!({
            "entities": st.entities, "links": st.links, "iterations": iterations, "seed": seed,
            "questions": rows.iter().map(|(n,t,note)| json!({
                "question": n, "min_ms": t.min, "median_ms": t.median, "p95_ms": t.p95, "result": note
            })).collect::<Vec<_>>(),
        });
        let dir = ctx.brain.project_root.join("bench").join("results");
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(format!("{}.json", chrono::Local::now().format("%Y-%m-%dT%H-%M-%S")));
        std::fs::write(&p, serde_json::to_string_pretty(&payload)?)?;
        println!("\nwrote {}", p.display());
    }
    Ok(0)
}

// ─── a project's own question suite ─────────────────────────────────────────

async fn project_suite(a: &Args, g: &Global) -> Result<i32> {
    let ctx = Ctx::open(g).await?;
    let path = a
        .get("cases")
        .map(config::expand)
        .unwrap_or_else(|| ctx.brain.project_root.join("tests").join("test-cases.json"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} (pass --cases <path>)", path.display()))?;
    let cases: Vec<serde_json::Value> = serde_json::from_str(&text)?;
    let only = a.get("only");
    let mut pass = 0usize;
    let mut fail = 0usize;
    for case in &cases {
        // Accept the es6 fixture shape (`id` / `expectedAnswer` / `answerKind`)
        // as well as brain2's own, so a project's existing cases run unmodified.
        let name = case["name"].as_str().or_else(|| case["id"].as_str()).unwrap_or("?");
        if let Some(o) = only {
            if name != o {
                continue;
            }
        }
        let q = case["question"].as_str().unwrap_or("");
        let expect: Vec<String> = case["expect"]
            .as_array()
            .or_else(|| case["expectedAnswer"].as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let t0 = Instant::now();
        let r = query::crawl::run(
            &ctx.store,
            &ctx.tbox,
            &ctx.slot,
            Some(&ctx.chat),
            ctx.embedder.as_ref(),
            q,
            &query::crawl::CrawlOpts {
                start: case["start"].as_str().map(|s| s.to_string()),
                kind: case["kind"].as_str().or_else(|| case["answerKind"].as_str()).map(|s| s.to_string()),
                finalize: false,
                ..Default::default()
            },
        )
        .await?;
        let got: Vec<String> = r.collected.clone();
        let ok = !expect.is_empty()
            && expect.len() == got.len()
            && expect.iter().all(|e| got.iter().any(|x| x.eq_ignore_ascii_case(e)));
        if ok {
            pass += 1;
        } else {
            fail += 1;
        }
        println!(
            "{} {name:<28} {:>5}ms {:>3} step(s)  {}",
            if ok { "PASS" } else { "FAIL" },
            t0.elapsed().as_millis(),
            r.steps.len(),
            got.join(", ")
        );
        if !ok && !expect.is_empty() {
            println!("     expected: {}", expect.join(", "));
        }
    }
    println!("\n{pass}/{} passing", pass + fail);
    Ok(if fail > 0 { 1 } else { 0 })
}
