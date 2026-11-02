//! abox.rs — the A-box (values): one `.md` file per entity, YAML frontmatter
//! plus a free-text body.
//!
//! ```markdown
//! ---
//! identity:
//!   name: Avery Adams
//!   username: aadams
//! REPORTS_TO:
//! - Person/knia2
//! SUPPORTS:
//! - _to: Product/m-sm64ds
//!   role: primary
//! ---
//!
//! # Person/aadams
//!
//! Free prose. Indexed, never parsed.
//! ```
//!
//! The convention that makes the file self-describing, carried from brain1:
//! **lowercase keys are components** (attribute bags), **ALL-CAPS keys are
//! relations** (typed edges). A relation target is either a bare `Class/id`
//! or a map with `_to` plus that edge's qualifiers — so a qualified edge costs
//! nothing to read and nothing to write when there are no qualifiers.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::slug::{self, Slug};
use crate::tbox::{yaml_scalar, TBox};

/// One edge: a target slug plus that edge's qualifier values.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub to: String,
    pub qualifiers: BTreeMap<String, Value>,
}

impl Edge {
    pub fn plain(to: impl Into<String>) -> Edge {
        Edge { to: to.into(), qualifiers: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Entity {
    pub cls: String,
    pub id: String,
    /// alias -> { field -> value }
    pub components: BTreeMap<String, Map<String, Value>>,
    /// REL -> edges
    pub relations: BTreeMap<String, Vec<Edge>>,
    pub body: String,
}

impl Entity {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.cls, self.id)
    }

    pub fn new(cls: impl Into<String>, id: impl Into<String>) -> Entity {
        Entity { cls: cls.into(), id: id.into(), ..Default::default() }
    }

    pub fn field(&self, path: &str) -> Option<&Value> {
        let (a, f) = path.split_once('.')?;
        self.components.get(a)?.get(f)
    }

    pub fn set_field(&mut self, path: &str, v: Value) -> Result<()> {
        let (a, f) = path
            .split_once('.')
            .with_context(|| format!("field path '{path}' must be 'alias.field'"))?;
        self.components
            .entry(a.to_string())
            .or_default()
            .insert(f.to_string(), v);
        Ok(())
    }

    /// First non-empty of the conventional label fields, else the prettified id.
    /// dstwiki uses `identity.title`; brain-db uses `identity.name` / `naming.name`.
    pub fn label(&self) -> String {
        const PATHS: &[&str] = &[
            "naming.name",
            "identity.name",
            "identity.title",
            "info.name",
            "identity.displayName",
            "identity.wikiPage",
        ];
        for p in PATHS {
            if let Some(Value::String(s)) = self.field(p) {
                if !s.trim().is_empty() {
                    return s.clone();
                }
            }
        }
        // any component field literally called name/title
        for fields in self.components.values() {
            for k in ["name", "title"] {
                if let Some(Value::String(s)) = fields.get(k) {
                    if !s.trim().is_empty() {
                        return s.clone();
                    }
                }
            }
        }
        pretty_slug(&self.id)
    }

    pub fn add_edge(&mut self, rel: &str, to: &str) {
        let v = self.relations.entry(rel.to_string()).or_default();
        if !v.iter().any(|e| e.to.eq_ignore_ascii_case(to)) {
            v.push(Edge::plain(to));
        }
    }

    /// Union `other` into `self`: incoming scalars win, edges union by target.
    pub fn merge(&mut self, other: Entity) {
        for (alias, fields) in other.components {
            let dst = self.components.entry(alias).or_default();
            for (k, v) in fields {
                if !v.is_null() {
                    dst.insert(k, v);
                }
            }
        }
        for (rel, edges) in other.relations {
            let dst = self.relations.entry(rel).or_default();
            for e in edges {
                if let Some(x) = dst.iter_mut().find(|x| x.to.eq_ignore_ascii_case(&e.to)) {
                    for (k, v) in e.qualifiers {
                        x.qualifiers.insert(k, v);
                    }
                } else {
                    dst.push(e);
                }
            }
        }
        if self.body.trim().is_empty() {
            self.body = other.body;
        }
    }

    // ── on-disk form ─────────────────────────────────────────────────────────

    pub fn to_markdown(&self) -> String {
        let mut s = String::from("---\n");
        for (alias, fields) in &self.components {
            if fields.is_empty() {
                s.push_str(&format!("{alias}: {{}}\n"));
                continue;
            }
            s.push_str(&format!("{alias}:\n"));
            for (k, v) in fields {
                s.push_str(&emit_field(k, v, 2));
            }
        }
        for (rel, edges) in &self.relations {
            if edges.is_empty() {
                continue;
            }
            s.push_str(&format!("{rel}:\n"));
            for e in edges {
                if e.qualifiers.is_empty() {
                    s.push_str(&format!("- {}\n", yaml_scalar(&e.to)));
                } else {
                    s.push_str(&format!("- _to: {}\n", yaml_scalar(&e.to)));
                    for (k, v) in &e.qualifiers {
                        s.push_str(&emit_field(k, v, 2));
                    }
                }
            }
        }
        s.push_str("---\n\n");
        let body = self.body.trim();
        if body.is_empty() {
            s.push_str(&format!("# {}\n", self.slug()));
        } else {
            s.push_str(body);
            s.push('\n');
        }
        s
    }

    /// Parse one entity file. `cls`/`id` come from the path, not the content —
    /// the filesystem is the index of record for identity.
    pub fn from_markdown(cls: &str, id: &str, text: &str) -> Result<Entity> {
        let (front, body) = split_frontmatter(text);
        let mut e = Entity::new(cls, id);
        // `to_markdown` writes `# Class/id` as the body when there is no prose,
        // so reading it back has to recognise its own placeholder. Otherwise a
        // round trip turns "this entity has no prose" into "this entity's prose
        // is its own slug", which then renders as a paragraph in the wiki and
        // gets indexed as content.
        let trimmed = body.trim();
        e.body = if trimmed == format!("# {cls}/{id}") || trimmed.is_empty() {
            String::new()
        } else {
            body.to_string()
        };
        if front.trim().is_empty() {
            return Ok(e);
        }
        let doc: serde_yaml::Value =
            serde_yaml::from_str(front).context("parsing entity frontmatter")?;
        let map = match doc {
            serde_yaml::Value::Mapping(m) => m,
            serde_yaml::Value::Null => return Ok(e),
            _ => bail!("entity frontmatter must be a YAML mapping"),
        };
        for (k, v) in map {
            let key = match k {
                serde_yaml::Value::String(s) => s,
                other => format!("{other:?}"),
            };
            if key.starts_with('_') {
                continue; // reserved
            }
            if slug::is_relation_name(&key) {
                e.relations.insert(key, parse_edges(&v)?);
            } else {
                let json = yaml_to_json(&v);
                match json {
                    Value::Object(m) => {
                        e.components.insert(key, m);
                    }
                    Value::Null => {
                        e.components.insert(key, Map::new());
                    }
                    other => {
                        // a bare scalar at component level: keep it addressable
                        let mut m = Map::new();
                        m.insert("value".into(), other);
                        e.components.insert(key, m);
                    }
                }
            }
        }
        Ok(e)
    }

    pub fn path_in(&self, db_root: &Path) -> PathBuf {
        db_root.join(&self.cls).join(format!("{}.md", self.id))
    }

    pub fn write(&self, db_root: &Path) -> Result<PathBuf> {
        let p = self.path_in(db_root);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, self.to_markdown())
            .with_context(|| format!("writing {}", p.display()))?;
        Ok(p)
    }
}

fn parse_edges(v: &serde_yaml::Value) -> Result<Vec<Edge>> {
    let items: Vec<&serde_yaml::Value> = match v {
        serde_yaml::Value::Sequence(s) => s.iter().collect(),
        serde_yaml::Value::Null => return Ok(vec![]),
        one => vec![one],
    };
    // Dedupe by target within one relation. A repeated edge is not
    // information, and it is also what makes `(from_slug, rel, to_slug)` a key:
    // the index carries a unique constraint so an interactive write can use
    // ON CONFLICT, and a duplicate can only ever arrive from one entity's own
    // file. The ICIJ relationship table has 435,267 of them.
    let mut out: Vec<Edge> = Vec::with_capacity(items.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in items {
        match it {
            serde_yaml::Value::String(s) => {
                let t = s.trim();
                if seen.insert(t.to_lowercase()) {
                    out.push(Edge::plain(t));
                }
            }
            serde_yaml::Value::Mapping(_) => {
                let json = yaml_to_json(it);
                let Value::Object(m) = json else { continue };
                let to = m
                    .get("_to")
                    .and_then(|x| x.as_str())
                    .map(|s| s.trim().to_string());
                let Some(to) = to else {
                    bail!("relation target map is missing `_to`");
                };
                let qualifiers = m
                    .into_iter()
                    .filter(|(k, _)| k != "_to")
                    .collect::<BTreeMap<String, Value>>();
                if seen.insert(to.to_lowercase()) {
                    out.push(Edge { to, qualifiers });
                }
            }
            other => {
                let json = yaml_to_json(other);
                if let Some(s) = json.as_str() {
                    if seen.insert(s.to_lowercase()) {
                        out.push(Edge::plain(s));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `---\n…\n---\n body` -> (frontmatter, body). A file with no frontmatter is
/// all body, which is how `ingest` sees a plain document.
pub fn split_frontmatter(text: &str) -> (&str, &str) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let t = t.trim_start_matches(['\r', '\n']);
    if !t.starts_with("---") {
        return ("", text);
    }
    let after = &t[3..];
    let after = after.strip_prefix('\r').unwrap_or(after);
    let after = match after.strip_prefix('\n') {
        Some(x) => x,
        None => return ("", text),
    };
    // find a line that is exactly `---`
    let mut idx = 0usize;
    for line in after.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            let front = &after[..idx];
            let body = &after[idx + line.len()..];
            return (front, body.trim_start_matches(['\r', '\n']));
        }
        idx += line.len();
    }
    ("", text)
}

fn emit_field(k: &str, v: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match v {
        Value::Array(items) => {
            if items.is_empty() {
                return format!("{pad}{k}: []\n");
            }
            let mut s = format!("{pad}{k}:\n");
            for it in items {
                match it {
                    Value::Object(_) | Value::Array(_) => {
                        s.push_str(&format!("{pad}- {}\n", serde_json::to_string(it).unwrap_or_default()));
                    }
                    _ => s.push_str(&format!("{pad}- {}\n", scalar_yaml(it))),
                }
            }
            s
        }
        Value::Object(m) => {
            if m.is_empty() {
                return format!("{pad}{k}: {{}}\n");
            }
            let mut s = format!("{pad}{k}:\n");
            for (kk, vv) in m {
                s.push_str(&emit_field(kk, vv, indent + 2));
            }
            s
        }
        other => format!("{pad}{k}: {}\n", scalar_yaml(other)),
    }
}

fn scalar_yaml(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if s.contains('\n') {
                // block scalar keeps prose readable in the wiki view
                let body: String = s
                    .trim_end()
                    .lines()
                    .map(|l| format!("\n    {l}"))
                    .collect();
                format!("|-{body}")
            } else {
                yaml_scalar(s)
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

pub fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        serde_yaml::Value::String(s) => Value::String(s.clone()),
        serde_yaml::Value::Sequence(s) => Value::Array(s.iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(m) => {
            let mut o = Map::new();
            for (k, v) in m {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    other => format!("{other:?}"),
                };
                o.insert(key, yaml_to_json(v));
            }
            Value::Object(o)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

/// `Cut_Grass` -> `Cut Grass`; `m-sm64ds` is left alone (hyphens are real).
pub fn pretty_slug(id: &str) -> String {
    id.replace('_', " ")
}

/// A flat human-readable rendering of one entity's own fields, used by the
/// crawl's state payload and by the wiki page renderer.
pub fn render_fields(e: &Entity, limit_chars: usize) -> String {
    let mut out = String::new();
    for (alias, fields) in &e.components {
        for (k, v) in fields {
            let txt = match v {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .map(|i| i.as_str().map(|s| s.to_string()).unwrap_or_else(|| i.to_string()))
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::Null => continue,
                other => other.to_string(),
            };
            if txt.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("{alias}.{k}: {txt}\n"));
            if out.len() > limit_chars {
                out.push_str("…\n");
                return out;
            }
        }
    }
    out
}

/// Scan a db root for `<Class>/<id>.md` files. The class comes from the
/// directory name, which must be a declared class when a T-box is supplied.
pub fn scan(db_root: &Path, tbox: Option<&TBox>) -> Result<Vec<(Slug, PathBuf)>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(db_root) {
        Ok(r) => r,
        Err(_) => return Ok(out),
    };
    for dir in rd.flatten() {
        if !dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let cls = dir.file_name().to_string_lossy().to_string();
        if !slug::is_class_name(&cls) {
            continue;
        }
        if let Some(tb) = tbox {
            if !tb.classes.contains_key(&cls) {
                continue;
            }
        }
        for f in walkdir::WalkDir::new(dir.path())
            .max_depth(1)
            .into_iter()
            .flatten()
        {
            let p = f.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let id = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
            if !slug::is_id(&id) {
                continue;
            }
            out.push((Slug::new(cls.clone(), id), p.to_path_buf()));
        }
    }
    out.sort_by(|a, b| a.0.to_string().cmp(&b.0.to_string()));
    Ok(out)
}

pub fn read_entity(cls: &str, id: &str, path: &Path) -> Result<Entity> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Entity::from_markdown(cls, id, &text).with_context(|| format!("in {}", path.display()))
}
