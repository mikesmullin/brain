//! index.rs — the two places disk and index meet, and nothing in between.
//!
//! * [`reindex`]  `.md` -> index. The ONE place `.md` is read in bulk.
//! * [`export`]   index -> `.md`. The ONE place `.md` is written from the index.
//!
//! Rebuilding is **opt-in**, exactly as brain1 ended up: a command rebuilds
//! only when the index is empty or `--reindex` was passed, and the staleness
//! *check* is skipped too. The point of opt-in is that no time is spent
//! detecting staleness or reindexing unless it was explicitly asked for — a
//! stale index can serve slugs the files no longer have, and nothing will warn
//! you. `brain reindex` is the whole remedy.

use anyhow::{bail, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::abox::{self, Entity};
use crate::config::Brain;
use crate::llm::Embedder;
use crate::store::{Stats, Store};
use crate::tbox::TBox;

const COPY_BATCH: usize = 4096;
const EMBED_BATCH: usize = 64;

pub struct ReindexReport {
    pub entities: u64,
    pub links: u64,
    pub embedded: u64,
    pub files: usize,
    pub parse_ms: u128,
    pub copy_ms: u128,
    pub index_ms: u128,
    pub embed_ms: u128,
    pub total_ms: u128,
}

/// Enumerate every `<Class>/<id>.md` across the brain's storage roots.
pub fn enumerate(brain: &Brain, tbox: &TBox) -> Result<Vec<(String, String, PathBuf)>> {
    let mut out = Vec::new();
    for root in brain.storage_roots() {
        for (slug, path) in abox::scan(&root, Some(tbox))? {
            out.push((slug.cls, slug.id, path));
        }
    }
    out.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    Ok(out)
}

/// Parse one batch of files in parallel. A file that fails to parse is reported
/// and skipped: one broken entity must not cost you the other two million.
fn parse_batch(files: &[(String, String, PathBuf)], bad: &Arc<AtomicU64>) -> Vec<Entity> {
    files
        .par_iter()
        .filter_map(|(cls, id, path)| match abox::read_entity(cls, id, path) {
            Ok(e) => Some(e),
            Err(err) => {
                bad.fetch_add(1, Ordering::Relaxed);
                if bad.load(Ordering::Relaxed) <= 20 {
                    eprintln!("warn: {err}");
                }
                None
            }
        })
        .collect()
}

/// Full rebuild: truncate, parallel-parse, concurrent `COPY`, then build the
/// indexes. Indexes last is deliberate — maintaining a GIN index during a
/// 2M-row load costs several times what building it once afterwards does.
pub async fn reindex(
    brain: &Brain,
    tbox: &TBox,
    store: &Store,
    embedder: Option<&Embedder>,
    quiet: bool,
) -> Result<ReindexReport> {
    let t0 = Instant::now();
    let files = enumerate(brain, tbox)?;
    if !quiet {
        eprintln!("reindex: {} entity files across {} storage root(s)", files.len(), brain.storage_roots().len());
    }
    store.ensure_schema().await?;
    store.begin_rebuild().await?;

    let bad = Arc::new(AtomicU64::new(0));
    let ents = Arc::new(AtomicU64::new(0));
    let lnks = Arc::new(AtomicU64::new(0));
    let parse_ns = Arc::new(AtomicU64::new(0));
    let copy_ns = Arc::new(AtomicU64::new(0));

    let batches: Vec<Vec<(String, String, PathBuf)>> =
        files.chunks(COPY_BATCH).map(|c| c.to_vec()).collect();
    let nbatches = batches.len();
    let lanes = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(16).max(2);
    let queue = Arc::new(tokio::sync::Mutex::new(batches.into_iter()));
    let done = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..lanes {
        let queue = queue.clone();
        let store = store.clone();
        let (bad, ents, lnks, parse_ns, copy_ns, done) =
            (bad.clone(), ents.clone(), lnks.clone(), parse_ns.clone(), copy_ns.clone(), done.clone());
        handles.push(tokio::spawn(async move {
            loop {
                let batch = {
                    let mut g = queue.lock().await;
                    g.next()
                };
                let Some(batch) = batch else { break };
                let bad2 = bad.clone();
                let tp = Instant::now();
                let parsed = tokio::task::spawn_blocking(move || parse_batch(&batch, &bad2))
                    .await
                    .map_err(|e| anyhow::anyhow!("parse task: {e}"))?;
                parse_ns.fetch_add(tp.elapsed().as_nanos() as u64, Ordering::Relaxed);
                let tc = Instant::now();
                let (e, l) = store.copy_batch(&parsed).await?;
                copy_ns.fetch_add(tc.elapsed().as_nanos() as u64, Ordering::Relaxed);
                ents.fetch_add(e, Ordering::Relaxed);
                lnks.fetch_add(l, Ordering::Relaxed);
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                if !quiet && nbatches > 20 && d % (nbatches as u64 / 20).max(1) == 0 {
                    eprintln!(
                        "  {:>3}%  {} entities, {} edges",
                        d * 100 / nbatches as u64,
                        ents.load(Ordering::Relaxed),
                        lnks.load(Ordering::Relaxed)
                    );
                }
            }
            Ok::<(), anyhow::Error>(())
        }));
    }
    for h in handles {
        h.await.map_err(|e| anyhow::anyhow!("copy lane: {e}"))??;
    }

    let ti = Instant::now();
    if !quiet {
        eprintln!("reindex: building indexes …");
    }
    store.finish_rebuild().await?;
    let index_ms = ti.elapsed().as_millis();

    // embeddings (skipped entirely when the brain is configured `none`)
    let te = Instant::now();
    let mut embedded = 0u64;
    if let Some(emb) = embedder {
        embedded = embed_all(store, emb, quiet).await?;
        store.ensure_vector_index().await?;
    }
    let embed_ms = te.elapsed().as_millis();

    let nbad = bad.load(Ordering::Relaxed);
    if nbad > 0 {
        eprintln!("warn: {nbad} file(s) could not be parsed and were skipped");
    }

    let mut st = store.refresh_stats().await?;
    st.fingerprint = tbox.fingerprint();
    st.indexed_at = chrono::Local::now().to_rfc3339();
    st.embed_model = brain.config.embed.model.clone();
    store.set_stats(&st).await?;

    Ok(ReindexReport {
        entities: ents.load(Ordering::Relaxed),
        links: lnks.load(Ordering::Relaxed),
        embedded,
        files: files.len(),
        parse_ms: (parse_ns.load(Ordering::Relaxed) / 1_000_000) as u128,
        copy_ms: (copy_ns.load(Ordering::Relaxed) / 1_000_000) as u128,
        index_ms,
        embed_ms,
        total_ms: t0.elapsed().as_millis(),
    })
}

/// The text that gets embedded: label, class, every own field, then the body.
/// Relation lines are left out on purpose — they are already the strongest
/// band in the keyword half, and putting them in the vector too made the two
/// halves of the hybrid agree with each other instead of covering for each
/// other.
pub fn embed_text(row: &crate::store::EntityRow) -> String {
    let e = row.to_entity();
    let mut s = format!("{} ({})\n", row.label, row.cls);
    s.push_str(&abox::render_fields(&e, 4000));
    if !row.body.trim().is_empty() {
        s.push('\n');
        s.push_str(row.body.trim());
    }
    s.chars().take(6000).collect()
}

async fn embed_all(store: &Store, emb: &Embedder, quiet: bool) -> Result<u64> {
    let total = store.stats().await?.entities;
    let mut offset = 0i64;
    let mut n = 0u64;
    loop {
        let page = store.list_class(None, 2048, offset).await?;
        if page.is_empty() {
            break;
        }
        let slugs: Vec<String> = page.iter().map(|(s, _, _)| s.clone()).collect();
        let rows = store.get_many(&slugs).await?;
        for chunk in rows.chunks(EMBED_BATCH) {
            let texts: Vec<String> = chunk.iter().map(embed_text).collect();
            let vecs = emb.embed(&texts).await?;
            let pairs: Vec<(String, Vec<f32>)> = chunk
                .iter()
                .map(|r| r.slug.clone())
                .zip(vecs.into_iter())
                .collect();
            n += pairs.len() as u64;
            store.put_embeddings(&pairs).await?;
        }
        offset += page.len() as i64;
        if !quiet && total > 0 {
            eprintln!("  embed {}/{}", n, total);
        }
    }
    Ok(n)
}

/// Index -> `.md`. While the index is authoritative (a server is running and
/// taking writes), `.md` is a human-readable, git-friendly snapshot you take
/// when you want one.
pub async fn export(brain: &Brain, store: &Store, prune: bool) -> Result<(usize, usize)> {
    let mut written = 0usize;
    let mut offset = 0i64;
    let mut live: std::collections::HashSet<String> = Default::default();
    loop {
        let page = store.list_class(None, 2048, offset).await?;
        if page.is_empty() {
            break;
        }
        let slugs: Vec<String> = page.iter().map(|(s, _, _)| s.clone()).collect();
        for slug in &slugs {
            if let Some((e, _)) = crate::store::hydrate(store, slug).await? {
                e.write(&brain.db_root)?;
                live.insert(e.slug().to_lowercase());
                written += 1;
            }
        }
        offset += page.len() as i64;
    }
    let mut pruned = 0usize;
    if prune {
        for (slug, path) in abox::scan(&brain.db_root, None)? {
            if !live.contains(&slug.to_string().to_lowercase()) {
                std::fs::remove_file(&path).ok();
                pruned += 1;
            }
        }
    }
    Ok((written, pruned))
}

/// Copy an already-structured dataset (`schema.yaml` + `<Class>/<id>.md`) into
/// the active `db/`. No LLM, no extraction — this is the benchmark / fixture /
/// onboarding path.
pub fn load_dataset(brain: &Brain, src: &Path) -> Result<(usize, bool)> {
    let schema = src.join("schema.yaml");
    if !schema.exists() {
        bail!("{} has no schema.yaml — that is what distinguishes a dataset from a document tree (see `brain ingest`)", src.display());
    }
    std::fs::create_dir_all(&brain.db_root)?;
    std::fs::copy(&schema, brain.db_root.join("schema.yaml"))?;
    let mut n = 0usize;
    for entry in walkdir::WalkDir::new(src).into_iter().flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let rel = p.strip_prefix(src).unwrap_or(p);
        let dst = brain.db_root.join(rel);
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::copy(p, &dst)?;
        n += 1;
    }
    Ok((n, true))
}

// ── validation ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Notice,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub subject: String,
    pub rule: &'static str,
    pub detail: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warn",
            Severity::Notice => "notice",
        };
        write!(f, "{s:7} {:<40} [{}] {}", self.subject, self.rule, self.detail)
    }
}

/// Validate every entity against the T-box and lint the graph.
///
/// Errors are schema violations (undeclared class/component/field/relation,
/// a missing required field, a domain/range breach, an edge to nothing).
/// Warnings and notices are shape advice that never blocks.
pub fn validate(brain: &Brain, tbox: &TBox) -> Result<Vec<Finding>> {
    let files = enumerate(brain, tbox)?;
    let known: std::collections::HashSet<String> =
        files.iter().map(|(c, i, _)| format!("{c}/{i}").to_lowercase()).collect();

    // Anything in a directory the T-box does not declare is invisible to
    // `enumerate`, so look for those separately — a silently ignored class is
    // worse than a loud one.
    let mut findings: Vec<Finding> = Vec::new();
    for root in brain.storage_roots() {
        if let Ok(rd) = std::fs::read_dir(&root) {
            for d in rd.flatten() {
                if !d.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let name = d.file_name().to_string_lossy().to_string();
                if crate::slug::is_class_name(&name) && !tbox.classes.contains_key(&name) {
                    findings.push(Finding {
                        severity: Severity::Error,
                        subject: format!("{name}/"),
                        rule: "undeclared_class",
                        detail: format!("directory {} holds entities of an undeclared class", d.path().display()),
                    });
                }
            }
        }
    }

    let chunks: Vec<Vec<Finding>> = files
        .par_iter()
        .map(|(cls, id, path)| {
            let mut out = Vec::new();
            let e = match abox::read_entity(cls, id, path) {
                Ok(e) => e,
                Err(err) => {
                    out.push(Finding {
                        severity: Severity::Error,
                        subject: format!("{cls}/{id}"),
                        rule: "unparseable",
                        detail: format!("{err}"),
                    });
                    return out;
                }
            };
            let slug = e.slug();
            let declared = tbox.class_fields(cls);
            for (alias, fields) in &e.components {
                let Some((comp, defs)) = declared.get(alias) else {
                    out.push(Finding {
                        severity: Severity::Error,
                        subject: slug.clone(),
                        rule: "undeclared_component",
                        detail: format!("'{alias}' is not a component of {cls} (valid: {})", declared.keys().cloned().collect::<Vec<_>>().join(", ")),
                    });
                    continue;
                };
                for k in fields.keys() {
                    if !defs.contains_key(k) {
                        out.push(Finding {
                            severity: Severity::Error,
                            subject: slug.clone(),
                            rule: "undeclared_field",
                            detail: format!("{alias}.{k} is not a field of component {comp}"),
                        });
                    }
                }
                for (fname, fd) in defs {
                    let present = fields.get(fname).map(|v| !v.is_null()).unwrap_or(false);
                    if fd.required && !present {
                        out.push(Finding {
                            severity: Severity::Error,
                            subject: slug.clone(),
                            rule: "missing_required",
                            detail: format!("{alias}.{fname} is required by {comp}"),
                        });
                    }
                    if let Some(v) = fields.get(fname) {
                        if let Some(bad) = type_mismatch(fd, v) {
                            out.push(Finding {
                                severity: Severity::Warning,
                                subject: slug.clone(),
                                rule: "type_mismatch",
                                detail: format!("{alias}.{fname}: {bad}"),
                            });
                        }
                        // A `ref` names an entity, so it can dangle exactly as
                        // an edge can — and a reference to nothing is an error
                        // for the same reason.
                        if fd.r#type == "ref" {
                            for one in scalars_of(v) {
                                match crate::slug::parse(one) {
                                    Err(e) => out.push(Finding {
                                        severity: Severity::Error,
                                        subject: slug.clone(),
                                        rule: "bad_ref",
                                        detail: format!("{alias}.{fname}: {e}"),
                                    }),
                                    Ok(t) => {
                                        if !fd.allowed_types.is_empty()
                                            && !fd.allowed_types.contains(&t.cls)
                                        {
                                            out.push(Finding {
                                                severity: Severity::Error,
                                                subject: slug.clone(),
                                                rule: "ref_class_not_allowed",
                                                detail: format!(
                                                    "{alias}.{fname} -> {t}: class '{}' is not in allowedTypes [{}]",
                                                    t.cls,
                                                    fd.allowed_types.join(", ")
                                                ),
                                            });
                                        }
                                        if !known.contains(&t.to_string().to_lowercase()) {
                                            out.push(Finding {
                                                severity: Severity::Error,
                                                subject: slug.clone(),
                                                rule: "dangling_ref",
                                                detail: format!("{alias}.{fname} -> {t} does not exist"),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for (rel, edges) in &e.relations {
                let Some(rd) = tbox.relations.get(rel) else {
                    out.push(Finding {
                        severity: Severity::Error,
                        subject: slug.clone(),
                        rule: "undeclared_relation",
                        detail: format!("'{rel}' is not a declared relation"),
                    });
                    continue;
                };
                if let Some(dom) = &rd.domain {
                    if !tbox.ancestry(cls).iter().any(|a| a == dom) {
                        out.push(Finding {
                            severity: Severity::Error,
                            subject: slug.clone(),
                            rule: "domain_violation",
                            detail: format!("{rel} has domain {dom}, but this entity is a {cls}"),
                        });
                    }
                }
                if rd.cardinality.is_single_valued() && edges.len() > 1 {
                    out.push(Finding {
                        severity: Severity::Error,
                        subject: slug.clone(),
                        rule: "cardinality_violation",
                        detail: format!("{rel} is '{}' but carries {} targets", rd.cardinality.as_str(), edges.len()),
                    });
                }
                for ed in edges {
                    match crate::slug::parse(&ed.to) {
                        Err(err) => out.push(Finding {
                            severity: Severity::Error,
                            subject: slug.clone(),
                            rule: "bad_target",
                            detail: format!("{rel} -> {err}"),
                        }),
                        Ok(t) => {
                            if !known.contains(&t.to_string().to_lowercase()) {
                                out.push(Finding {
                                    severity: Severity::Error,
                                    subject: slug.clone(),
                                    rule: "dangling_edge",
                                    detail: format!("{rel} -> {} does not exist", t),
                                });
                            } else if let Some(rng) = &rd.range {
                                if !tbox.ancestry(&t.cls).iter().any(|a| a == rng) {
                                    out.push(Finding {
                                        severity: Severity::Error,
                                        subject: slug.clone(),
                                        rule: "range_violation",
                                        detail: format!("{rel} has range {rng}, but {} is a {}", t, t.cls),
                                    });
                                }
                            }
                        }
                    }
                    for q in ed.qualifiers.keys() {
                        if !rd.qualifiers.contains_key(q) {
                            out.push(Finding {
                                severity: Severity::Notice,
                                subject: slug.clone(),
                                rule: "undeclared_qualifier",
                                detail: format!("{rel} carries qualifier '{q}', which the relation does not declare"),
                            });
                        }
                    }
                }
            }
            if let Some(idf) = tbox.id_field_of(cls) {
                if !crate::slug::is_relation_name(idf) {
                    if let Some(serde_json::Value::String(v)) = e.field(idf) {
                        if v.to_lowercase() != *id && !v.trim().is_empty() {
                            out.push(Finding {
                                severity: Severity::Warning,
                                subject: slug.clone(),
                                rule: "id_not_canonical",
                                detail: format!("idField {idf} = '{v}' but the file is '{id}' (run `brain refine`)"),
                            });
                        }
                    }
                }
            }
            if e.relations.is_empty() {
                out.push(Finding {
                    severity: Severity::Notice,
                    subject: slug.clone(),
                    rule: "orphan",
                    detail: "no relations".into(),
                });
            }
            out
        })
        .collect();
    for c in chunks {
        findings.extend(c);
    }
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(a.subject.cmp(&b.subject))
            .then(a.rule.cmp(b.rule))
    });
    Ok(findings)
}

/// Every string inside a value, so a `list: true` field of refs is checked
/// element by element.
fn scalars_of(v: &serde_json::Value) -> Vec<&str> {
    match v {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
        _ => vec![],
    }
}

fn type_mismatch(fd: &crate::tbox::FieldDef, v: &serde_json::Value) -> Option<String> {
    use serde_json::Value as V;
    if fd.list {
        return match v {
            V::Array(_) => None,
            _ => Some("declared `list: true` but the value is not a sequence".into()),
        };
    }
    match (fd.r#type.as_str(), v) {
        (_, V::Null) => None,
        ("string", V::String(_)) => None,
        ("int", V::Number(n)) if n.is_i64() || n.is_u64() => None,
        ("float", V::Number(_)) => None,
        ("bool", V::Bool(_)) => None,
        ("date", V::String(_)) => None,
        ("ref", V::String(_)) => None,
        ("enum", V::String(s)) => {
            if fd.values.iter().any(|x| x == s) {
                None
            } else {
                Some(format!("'{s}' is not one of [{}]", fd.values.join(", ")))
            }
        }
        ("json", _) => None,
        (t, got) => Some(format!("declared {t}, got {}", kind_of(got))),
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    use serde_json::Value as V;
    match v {
        V::Null => "null",
        V::Bool(_) => "bool",
        V::Number(_) => "number",
        V::String(_) => "string",
        V::Array(_) => "list",
        V::Object(_) => "map",
    }
}

/// Did the index get built from these files? Only consulted when asked.
pub async fn staleness(brain: &Brain, tbox: &TBox, store: &Store) -> Result<(bool, Stats)> {
    let st = store.stats().await?;
    let schema_changed = !st.fingerprint.is_empty() && st.fingerprint != tbox.fingerprint();
    let disk = crate::store::disk_fingerprint(&brain.storage_roots());
    let recorded = store
        .get_meta("disk")
        .await?
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    Ok((schema_changed || (!recorded.is_empty() && recorded != disk), st))
}

pub async fn record_disk_fingerprint(brain: &Brain, store: &Store) -> Result<()> {
    let fp = crate::store::disk_fingerprint(&brain.storage_roots());
    store.put_meta("disk", &serde_json::Value::String(fp)).await
}
