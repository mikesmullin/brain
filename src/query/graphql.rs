//! graphql.rs — deterministic projection from a known root.
//!
//! ```text
//!   Person/jdoe { identity { name }, REPORTS_TO { identity { name } } }
//! ```
//!
//! Lowercase names project a component (and may be narrowed by a nested
//! selection); ALL-CAPS names traverse that relation and recurse.
//!
//! Resolution is **level-batched**: one entity query and one links query per
//! selection level, not two point queries per traversed node. That was found
//! live during brain1's first benchmark run — projecting through a hub of
//! 36,000 edges took minutes as point queries and 891 ms batched.

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

use crate::store::Store;

#[derive(Debug, Clone)]
pub struct Sel {
    pub name: String,
    pub children: Option<Vec<Sel>>,
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if "{}(),".contains(c) {
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
            out.push(c.to_string());
        } else if c.is_whitespace() {
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn parse_set(toks: &[String], mut pos: usize) -> (Vec<Sel>, usize) {
    let mut items = Vec::new();
    pos += 1; // consume '{'
    while pos < toks.len() && toks[pos] != "}" {
        if toks[pos] == "," {
            pos += 1;
            continue;
        }
        let name = toks[pos].clone();
        pos += 1;
        let mut node = Sel { name, children: None };
        if pos < toks.len() && toks[pos] == "{" {
            let (c, p) = parse_set(toks, pos);
            node.children = Some(c);
            pos = p;
        }
        items.push(node);
    }
    if pos < toks.len() && toks[pos] == "}" {
        pos += 1;
    }
    (items, pos)
}

pub fn parse(query: &str) -> Result<(String, Option<Vec<Sel>>)> {
    let toks = tokenize(query);
    if toks.is_empty() {
        bail!("empty query");
    }
    let slug = toks[0].clone();
    if toks.len() > 1 && toks[1] == "{" {
        let (sel, _) = parse_set(&toks, 1);
        return Ok((slug, Some(sel)));
    }
    Ok((slug, None))
}

fn project_value(val: Option<&Value>, children: &Option<Vec<Sel>>) -> Value {
    let Some(kids) = children else {
        return val.cloned().unwrap_or(Value::Null);
    };
    let Some(Value::Object(m)) = val else {
        return val.cloned().unwrap_or(Value::Null);
    };
    let mut out = Map::new();
    for k in kids {
        out.insert(k.name.clone(), project_value(m.get(&k.name), &k.children));
    }
    Value::Object(out)
}

fn resolve_many<'a>(
    store: &'a Store,
    slugs: Vec<String>,
    sel: &'a [Sel],
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HashMap<String, Value>>> + Send + 'a>> {
    Box::pin(async move {
        if depth > 32 {
            bail!("graphql traversal exceeded max nesting depth (32)");
        }
        let mut results: HashMap<String, Value> = HashMap::new();
        if slugs.is_empty() {
            return Ok(results);
        }
        // one entities query for the whole level
        let rows = store.get_many(&slugs).await?;
        let by_slug: HashMap<String, &crate::store::EntityRow> =
            rows.iter().map(|r| (r.slug.to_lowercase(), r)).collect();

        // one links query for the whole level, narrowed to the relations this
        // selection actually names. Without the narrowing, projecting one
        // relation off a 36,284-edge hub fetched all 36,284 to find 30.
        let canonical: Vec<String> = rows.iter().map(|r| r.slug.clone()).collect();
        let wanted: Vec<String> = sel
            .iter()
            .filter(|i| crate::slug::is_relation_name(&i.name))
            .map(|i| i.name.clone())
            .collect();
        let edges = if wanted.is_empty() {
            Vec::new()
        } else {
            store.expand_rel(&canonical, true, 1_000_000, Some(&wanted)).await?
        };
        let mut outs: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for e in edges {
            outs.entry(e.from.to_lowercase())
                .or_default()
                .entry(e.rel)
                .or_default()
                .push(e.to);
        }

        // recurse once per relation-with-children, over the union of targets
        let mut child_maps: HashMap<String, HashMap<String, Value>> = HashMap::new();
        for item in sel {
            if !crate::slug::is_relation_name(&item.name) {
                continue;
            }
            let Some(kids) = &item.children else { continue };
            let mut targets: Vec<String> = Vec::new();
            let mut seen = HashSet::new();
            for s in &canonical {
                if let Some(rels) = outs.get(&s.to_lowercase()) {
                    if let Some(list) = rels.get(&item.name) {
                        for t in list {
                            if seen.insert(t.to_lowercase()) {
                                targets.push(t.clone());
                            }
                        }
                    }
                }
            }
            let m = resolve_many(store, targets, kids, depth + 1).await?;
            child_maps.insert(item.name.clone(), m);
        }

        for want in &slugs {
            let key = want.to_lowercase();
            let Some(row) = by_slug.get(&key) else {
                results.insert(want.clone(), json!({ "slug": want, "error": "not found" }));
                continue;
            };
            let mut obj = Map::new();
            obj.insert("slug".into(), Value::String(row.slug.clone()));
            for item in sel {
                if crate::slug::is_relation_name(&item.name) {
                    let list = outs
                        .get(&row.slug.to_lowercase())
                        .and_then(|m| m.get(&item.name))
                        .cloned()
                        .unwrap_or_default();
                    let vals: Vec<Value> = list
                        .into_iter()
                        .map(|t| match (&item.children, child_maps.get(&item.name)) {
                            (Some(_), Some(cm)) => cm
                                .get(&t)
                                .cloned()
                                .unwrap_or_else(|| json!({ "slug": t, "error": "not found" })),
                            _ => Value::String(t),
                        })
                        .collect();
                    obj.insert(item.name.clone(), Value::Array(vals));
                } else {
                    let comp = row.components.get(&item.name);
                    obj.insert(item.name.clone(), project_value(comp, &item.children));
                }
            }
            results.insert(row.slug.clone(), Value::Object(obj));
        }
        Ok(results)
    })
}

pub async fn run(store: &Store, query: &str) -> Result<Value> {
    let (slug, sel) = parse(query)?;
    let Some(canonical) = store.resolve(&slug).await? else {
        return Ok(json!({ "slug": slug, "error": "not found" }));
    };
    let Some(sel) = sel else {
        // no selection set: the whole entity, both halves
        let Some((e, _)) = crate::store::hydrate(store, &canonical).await? else {
            return Ok(json!({ "slug": canonical, "error": "not found" }));
        };
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
                    Value::Array(v.iter().map(|x| Value::String(x.to.clone())).collect()),
                )
            })
            .collect();
        return Ok(json!({ "slug": e.slug(), "components": comps, "relations": rels }));
    };
    let m = resolve_many(store, vec![canonical.clone()], &sel, 0).await?;
    Ok(m.get(&canonical).cloned().unwrap_or(Value::Null))
}
