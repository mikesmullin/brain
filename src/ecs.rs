//! ecs.rs — the two v1 commands that execute code the *dataset* owns:
//! `call` (component methods) and `refine` (per-class repair).
//!
//! ## Why these look different from everything else
//!
//! Every other command is pure Rust. These two are not, and cannot be: in v1 a
//! component method was `components/<Comp>.coffee` and a refiner was
//! `refiner/<Class>.coffee` — arbitrary code living in the database directory,
//! loaded at runtime, free to shell out to a company directory or call an
//! agent framework. That is the feature, not an implementation detail: the
//! schema says a class *has* methods, and the dataset says what they do.
//!
//! A compiled binary cannot evaluate CoffeeScript. So brain keeps the contract
//! and changes the mechanism: the module is executed by an **interpreter**
//! (`bun`, else `node`, else `$BRAIN_JS`), run with the brain's project root as
//! its working directory so the dataset's own `node_modules` resolve. Brain
//! passes the entity in as JSON and reads the result back as JSON.
//!
//! The dependency is real but *conditional*: a brain with no `components/` and
//! no `refiner/` never needs an interpreter, and the vast majority of brains
//! have neither.
//!
//! ## The deterministic half of refine is native
//!
//! Most of what v1's refiners actually did was `canonicalizeIds`: derive an
//! entity's id from its class's `idField`, rename the file when the two
//! disagree, rewrite every edge that pointed at the old slug, and merge what
//! collides. None of that needs a model or a dataset script, and it runs here
//! whether or not an interpreter exists.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::abox::{self, Entity};
use crate::cli::Args;
use crate::commands::Ctx;
use crate::index;
use crate::slug;

/// `bun`, else `node`, else whatever `$BRAIN_JS` names.
fn interpreter() -> Option<String> {
    if let Ok(v) = std::env::var("BRAIN_JS") {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    for c in ["bun", "node"] {
        if Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c.to_string());
        }
    }
    None
}

/// `<project>/components/<Comp>.{mjs,js,coffee}`
fn component_module(root: &Path, comp: &str) -> Option<PathBuf> {
    for ext in ["mjs", "js", "coffee"] {
        let p = root.join("components").join(format!("{comp}.{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn refiner_module(root: &Path, cls: &str) -> Option<PathBuf> {
    for ext in ["mjs", "js", "coffee"] {
        let p = root.join("refiner").join(format!("{cls}.{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// The shim, evaluated by the interpreter with the project root as cwd so the
/// dataset's own dependencies resolve. It is passed one argument: a JSON file
/// holding the request. It prints one JSON object.
const SHIM: &str = r#"
const fs = await import('node:fs');
const req = JSON.parse(fs.readFileSync(process.argv[process.argv.length - 1], 'utf8'));
const out = (o) => { process.stdout.write(JSON.stringify(o)); process.exit(0); };
try {
  if (String(req.module).endsWith('.coffee')) {
    try { await import('bun-coffeescript/register'); }
    catch (e) { out({ ok: false, error: 'a .coffee module needs bun with bun-coffeescript installed in the project: ' + e.message }); }
  }
  const mod = await import('file://' + req.module);
  if (req.mode === 'list') {
    const m = mod.methods || {};
    out({ ok: true, methods: Object.entries(m).map(([name, d]) => ({
      name, description: d.description || '', parameters: d.parameters || {}, required: d.required || [] })) });
  }
  if (req.mode === 'call') {
    const d = (mod.methods || {})[req.method];
    if (!d || typeof d.fn !== 'function') out({ ok: false, error: `no method '${req.method}'` });
    const entity = req.entity;
    const r = await d.fn(entity, req.alias, req.params || {});
    out({ ok: r ? r.success !== false : true, error: r && r.error, content: r && r.content, entity });
  }
  if (req.mode === 'calcId') {
    if (typeof mod.calcId !== 'function') out({ ok: true, id: null });
    out({ ok: true, id: await mod.calcId(req.entity) });
  }
  if (req.mode === 'refine') {
    const fn = mod.refine || mod.default;
    if (typeof fn !== 'function') out({ ok: true, entity: req.entity, changed: false });
    const entity = req.entity;
    const r = await fn(entity, req.ctx || {});
    out({ ok: true, entity: (r && r.entity) || entity, changed: !!(r && r.changed), content: r && r.content });
  }
  out({ ok: false, error: 'unknown mode ' + req.mode });
} catch (e) {
  out({ ok: false, error: String((e && e.stack) || e) });
}
"#;

fn run_shim(root: &Path, req: &Value) -> Result<Value> {
    let Some(js) = interpreter() else {
        bail!(
            "this database carries its own JavaScript (components/ or refiner/), and no \
interpreter was found. Install `bun` (needed for .coffee) or `node`, or set $BRAIN_JS."
        );
    };
    let dir = std::env::temp_dir().join(format!("brain-shim-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let reqfile = dir.join("req.json");
    std::fs::write(&reqfile, serde_json::to_vec(req)?)?;
    let out = Command::new(&js)
        .current_dir(root)
        .arg("-e")
        .arg(SHIM)
        .arg(&reqfile)
        .output()
        .with_context(|| format!("running {js}"))?;
    let _ = std::fs::remove_file(&reqfile);
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() {
        bail!(
            "{js} produced no output: {}",
            String::from_utf8_lossy(&out.stderr).trim().chars().take(400).collect::<String>()
        );
    }
    let v: Value = serde_json::from_str(&stdout)
        .with_context(|| format!("{js} returned non-JSON: {}", stdout.chars().take(300).collect::<String>()))?;
    Ok(v)
}

fn entity_to_json(e: &Entity) -> Value {
    let comps: Map<String, Value> = e
        .components
        .iter()
        .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
        .collect();
    let rels: Map<String, Value> = e
        .relations
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                Value::Array(v.iter().map(|x| json!({ "_to": x.to })).collect()),
            )
        })
        .collect();
    json!({ "slug": e.slug(), "cls": e.cls, "id": e.id, "components": comps, "relations": rels, "body": e.body })
}

/// Fold a module's edits back onto the entity. Only components and the body
/// are adopted: a method that rewired the graph would bypass schema validation
/// on the relation, and `link` is the command for that.
fn apply_entity_json(e: &mut Entity, v: &Value) {
    if let Some(Value::Object(comps)) = v.get("components") {
        e.components.clear();
        for (alias, fields) in comps {
            if let Value::Object(f) = fields {
                e.components.insert(alias.clone(), f.clone());
            }
        }
    }
    if let Some(Value::String(b)) = v.get("body") {
        e.body = b.clone();
    }
}

// ── schema methods ──────────────────────────────────────────────────────────

/// The methods callable on a class: every component it composes, in alias
/// order, that has a module beside the database.
pub fn methods_of(ctx: &Ctx, cls: &str) -> Result<Vec<(String, String, Value)>> {
    let mut out = Vec::new();
    for (alias, (comp, _)) in ctx.tbox.class_fields(cls) {
        let Some(module) = component_module(&ctx.brain.project_root, &comp) else { continue };
        let req = json!({ "mode": "list", "module": module.display().to_string() });
        let r = run_shim(&ctx.brain.project_root, &req)?;
        if r["ok"].as_bool() != Some(true) {
            eprintln!("warn: {comp}: {}", r["error"].as_str().unwrap_or("failed"));
            continue;
        }
        for m in r["methods"].as_array().cloned().unwrap_or_default() {
            out.push((alias.clone(), comp.clone(), m));
        }
    }
    Ok(out)
}

pub fn print_methods(ctx: &Ctx, which: Option<&str>) -> Result<i32> {
    let classes: Vec<String> = match which {
        Some(c) => vec![ctx
            .tbox
            .resolve_class(c)
            .with_context(|| format!("'{c}' is not a declared class"))?
            .to_string()],
        None => ctx.tbox.classes.keys().cloned().collect(),
    };
    let mut any = false;
    for cls in classes {
        let ms = methods_of(ctx, &cls)?;
        if ms.is_empty() {
            continue;
        }
        any = true;
        println!("{cls}");
        for (alias, comp, m) in ms {
            let name = m["name"].as_str().unwrap_or("?");
            println!(
                "  {name}  ({alias}: {comp})  {}",
                m["description"].as_str().unwrap_or("")
            );
            if let Some(Value::Object(params)) = m.get("parameters") {
                let req: Vec<&str> =
                    m["required"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
                for (p, d) in params {
                    println!(
                        "      {p}: {}{}  {}",
                        d["type"].as_str().unwrap_or("string"),
                        if req.contains(&p.as_str()) { " (required)" } else { "" },
                        d["description"].as_str().unwrap_or("")
                    );
                }
            }
        }
    }
    if !any {
        println!("no component methods — this database has no components/ modules");
        if interpreter().is_none() {
            println!("(and no JavaScript interpreter was found, which is only needed if it had)");
        }
    }
    Ok(0)
}

// ── call ────────────────────────────────────────────────────────────────────

pub async fn call(ctx: &Ctx, argv: &[String]) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    if a.positional.len() < 2 {
        bail!("usage: call");
    }
    let canonical = ctx.resolve_slug(&a.positional[0]).await?;
    let method = a.positional[1].clone();
    // params are YAML flow, outer braces optional, exactly as v1
    let params: Value = match a.positional.get(2) {
        None => Value::Object(Map::new()),
        Some(p) => {
            let text = p.trim();
            let wrapped = if text.starts_with('{') { text.to_string() } else { format!("{{{text}}}") };
            let y: serde_yaml::Value = serde_yaml::from_str(&wrapped).context("parsing params (YAML flow)")?;
            abox::yaml_to_json(&y)
        }
    };

    let s = slug::parse(&canonical)?;
    let path = ctx.brain.db_root.join(&s.cls).join(format!("{}.md", s.id));
    let mut e = abox::read_entity(&s.cls, &s.id, &path)?;

    // which component declares this method?
    let mut found: Option<(String, PathBuf)> = None;
    for (alias, (comp, _)) in ctx.tbox.class_fields(&s.cls) {
        let Some(module) = component_module(&ctx.brain.project_root, &comp) else { continue };
        let req = json!({ "mode": "list", "module": module.display().to_string() });
        let r = run_shim(&ctx.brain.project_root, &req)?;
        let has = r["methods"]
            .as_array()
            .map(|a| a.iter().any(|m| m["name"].as_str() == Some(method.as_str())))
            .unwrap_or(false);
        if has {
            found = Some((alias.clone(), module));
            break;
        }
    }
    let Some((alias, module)) = found else {
        bail!("no method '{method}' on {canonical} (see `brain schema methods {}`)", s.cls);
    };

    let req = json!({
        "mode": "call", "module": module.display().to_string(),
        "method": method, "alias": alias, "params": params,
        "entity": entity_to_json(&e),
    });
    let r = run_shim(&ctx.brain.project_root, &req)?;
    if r["ok"].as_bool() != Some(true) {
        eprintln!("error: {}", r["error"].as_str().unwrap_or("the method failed"));
        return Ok(1);
    }
    if let Some(ent) = r.get("entity") {
        apply_entity_json(&mut e, ent);
        e.write(&ctx.brain.db_root)?;
        ctx.store.upsert(&e).await?;
    }
    match r.get("content") {
        Some(Value::String(t)) => println!("{t}"),
        Some(Value::Null) | None => {}
        Some(other) => println!("{}", serde_json::to_string_pretty(other)?),
    }
    Ok(0)
}

// ── refine ──────────────────────────────────────────────────────────────────

/// Derive an entity's id from its class's `idField`.
fn canonical_id(ctx: &Ctx, e: &Entity) -> Option<String> {
    let idf = ctx.tbox.id_field_of(&e.cls)?;
    if slug::is_relation_name(idf) {
        let first = e.relations.get(idf)?.first()?;
        return slug::parse(&first.to).ok().map(|t| t.id);
    }
    match e.field(idf)? {
        Value::String(v) if !v.trim().is_empty() => Some(slug::sanitize_id(v)),
        _ => None,
    }
}

pub async fn refine(ctx: &Ctx, argv: &[String]) -> Result<i32> {
    let a = Args::parse(argv, &["class", "max-passes"]);
    let only = match a.get("class") {
        Some(c) => Some(
            ctx.tbox
                .resolve_class(c)
                .with_context(|| format!("'{c}' is not a declared class"))?
                .to_string(),
        ),
        None => None,
    };
    let max_passes = a.num("max-passes").unwrap_or(ctx.brain.config.refine_max_passes as i64).max(1);
    let dry = a.has("dry-run");

    let mut renamed = 0usize;
    let mut merged = 0usize;
    let mut scripted = 0usize;

    for pass in 1..=max_passes {
        let files = index::enumerate(&ctx.brain, &ctx.tbox)?;
        let mut loaded: Vec<Entity> = Vec::new();
        for (cls, id, path) in &files {
            if let Some(o) = &only {
                if cls != o {
                    continue;
                }
            }
            match abox::read_entity(cls, id, path) {
                Ok(e) => loaded.push(e),
                Err(err) => eprintln!("warn: {err}"),
            }
        }

        // 1 · the dataset's own refiner, when it has one and an interpreter exists
        if interpreter().is_some() {
            let mut by_class: BTreeMap<String, PathBuf> = BTreeMap::new();
            for e in &loaded {
                if let Some(m) = refiner_module(&ctx.brain.project_root, &e.cls) {
                    by_class.insert(e.cls.clone(), m);
                }
            }
            for e in loaded.iter_mut() {
                let Some(module) = by_class.get(&e.cls) else { continue };
                let req = json!({
                    "mode": "refine", "module": module.display().to_string(),
                    "entity": entity_to_json(e),
                    "ctx": { "db": ctx.brain.db_root.display().to_string(), "pass": pass },
                });
                match run_shim(&ctx.brain.project_root, &req) {
                    Ok(r) if r["ok"].as_bool() == Some(true) => {
                        if r["changed"].as_bool() == Some(true) {
                            if let Some(ent) = r.get("entity") {
                                apply_entity_json(e, ent);
                                scripted += 1;
                            }
                        }
                    }
                    Ok(r) => eprintln!("warn: refiner {}: {}", e.cls, r["error"].as_str().unwrap_or("failed")),
                    Err(err) => eprintln!("warn: refiner {}: {err}", e.cls),
                }
            }
        }

        // 2 · canonical ids, natively. The id a class's `idField` dictates is a
        //     fact about the entity, so a file whose name disagrees with it is
        //     simply misnamed.
        let mut rename: HashMap<String, String> = HashMap::new();
        for e in loaded.iter_mut() {
            let Some(want) = canonical_id(ctx, e) else { continue };
            if want == e.id || want.is_empty() {
                continue;
            }
            rename.insert(e.slug(), format!("{}/{want}", e.cls));
            e.id = want;
        }
        if rename.is_empty() && scripted == 0 {
            break;
        }

        // 3 · rewrite every edge that pointed at a renamed entity, and merge
        //     what now collides.
        for e in loaded.iter_mut() {
            for edges in e.relations.values_mut() {
                for edge in edges.iter_mut() {
                    if let Some(to) = rename.get(&edge.to) {
                        edge.to = to.clone();
                    }
                }
            }
        }
        let mut by_slug: BTreeMap<String, Entity> = BTreeMap::new();
        for e in loaded {
            match by_slug.get_mut(&e.slug()) {
                Some(existing) => {
                    existing.merge(e);
                    merged += 1;
                }
                None => {
                    by_slug.insert(e.slug(), e);
                }
            }
        }

        if dry {
            for (from, to) in &rename {
                println!("would rename {from} -> {to}");
            }
            return Ok(0);
        }
        for (from, _) in &rename {
            let s = slug::parse(from)?;
            std::fs::remove_file(ctx.brain.db_root.join(&s.cls).join(format!("{}.md", s.id))).ok();
            ctx.store.delete(from).await.ok();
            renamed += 1;
        }
        for e in by_slug.values() {
            e.write(&ctx.brain.db_root)?;
            ctx.store.upsert(e).await?;
        }
        // Edges held by entities outside the pass's scope still point at the
        // old slug, so a `--class` run has to fix the rest of the graph too.
        if only.is_some() && !rename.is_empty() {
            for (cls, id, path) in index::enumerate(&ctx.brain, &ctx.tbox)? {
                let Ok(mut e) = abox::read_entity(&cls, &id, &path) else { continue };
                let mut touched = false;
                for edges in e.relations.values_mut() {
                    for edge in edges.iter_mut() {
                        if let Some(to) = rename.get(&edge.to) {
                            edge.to = to.clone();
                            touched = true;
                        }
                    }
                }
                if touched {
                    e.write(&ctx.brain.db_root)?;
                    ctx.store.upsert(&e).await?;
                }
            }
        }
        if rename.is_empty() {
            break;
        }
    }

    ctx.store.refresh_stats().await?;
    println!("refine: {renamed} renamed, {merged} merged, {scripted} changed by a dataset refiner");
    if interpreter().is_none() {
        println!("(no JavaScript interpreter found — per-class refiner/ modules were skipped)");
    }
    Ok(0)
}
