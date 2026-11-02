//! crawl.rs — answer a question by walking the real graph, one typed decision
//! per hop.
//!
//! Every turn is a **reactive behaviour tree**, re-evaluated from the root, in
//! the style of a game AI tree: conditions are cheap named predicates over
//! engine state, control flow is real `if`s with early returns, and one tick
//! resolves to exactly one action. Which stage is "active" is a *consequence*
//! of named conditions, never a stored mode. The branches, in priority order:
//!
//! ```text
//!   ? Fallback (root)
//!     -> COMMIT      standing on something that may answer the question
//!     -> RANK        the question asked for the BEST one, and the set is complete
//!     -> FINISH      the answer slot is fully enumerated
//!     -> ENUMERATE   judge the rest of the slot, in place
//!     -> SEEK        walk the chain toward a node of the answer kind
//!     -> ORIENT      find the object the question is about
//!     ? RECOVER      the chain ran out: go back, or widen the net
//! ```
//!
//! ## The seven rules that earned their keep
//!
//! 1. **Grounded options only.** Never offer an action that cannot change
//!    anything. Every menu entry is a real edge, a real node, or a search that
//!    actually surfaces something new.
//! 2. **Facts in the engine, judgements in the model.** Anything the schema or
//!    the graph already states is read, not asked.
//! 3. **The model prefers travelling to committing.** Only 3 of 20 contested
//!    turns disagree with the engine's own ordering, so the tree's priority is
//!    load-bearing far less often than it looks.
//! 4. **Never ask a question with no wrong answer.** A one-option menu is taken
//!    by the engine (p=1.000 every time it was measured); a node reached along
//!    the very relation the question names is not re-judged.
//! 5. **Scope the state payload.** A judgement is a function of (question, one
//!    node) and nothing else. `Product/f-command` scores 0.856 judged alone and
//!    0.566 with eighteen accumulated slugs in the payload — the same node, the
//!    same question, on opposite sides of the threshold.
//! 6. **Benchmark the whole distribution**, not the cases you are fixing.
//! 7. **A bound measured from the wrong thing is not a bound.**

use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::abox::pretty_slug;
use crate::llm::{Answer, Chat, ChatMessage, Question, Slot};
use crate::store::{EntityRow, Store};
use crate::tbox::TBox;

/// The commit gate. Left at 0.7 deliberately: the TRUE floor measured over the
/// benchmark is 0.708 (a 0.008 margin) while the FALSE side moved 0.216 under a
/// single prompt edit, so the slack belongs on the side that moves.
pub const GOAL_T: f64 = 0.7;
pub const KIND_NONE: &str = "*none*";
pub const FILTER_NONE: &str = "*none*";

const MAX_MOVES: usize = 24;
const MAX_SEARCH_FRESH: usize = 40;
const MAX_CLASS_ENUM: usize = 64;
const CLASS_EXAMPLES: usize = 5;
/// Per-node adjacency cap. brain1's in-memory world capped at 50; 256 here
/// because a relational slot is read off the same adjacency and 50 silently
/// truncated a slot bigger than itself.
const ADJ_CAP: i64 = 256;
/// A whole-class enumeration needs a turn per candidate, so the step budget is
/// sized for MAX_CLASS_ENUM candidates at two turns each, plus orientation and
/// the chain walk.
pub const MAX_STEPS: usize = 160;
const FINALIZER_CONTENT_BUDGET: usize = 6000;

// ─── words about English, not about any schema ──────────────────────────────

const IRREGULAR_PLURALS: &[(&str, &str)] = &[
    ("person", "people"), ("child", "children"), ("man", "men"), ("woman", "women"),
    ("datum", "data"), ("medium", "media"), ("criterion", "criteria"),
    ("phenomenon", "phenomena"), ("index", "indices"), ("matrix", "matrices"),
    ("vertex", "vertices"), ("appendix", "appendices"), ("analysis", "analyses"),
    ("thesis", "theses"), ("hypothesis", "hypotheses"), ("basis", "bases"),
    ("genus", "genera"), ("corpus", "corpora"), ("schema", "schemata"),
];

/// A fixed list, not a `-est` suffix rule: `-est` collides with forest, honest,
/// harvest, interest, request and quest, and a blacklist of those is a slide
/// back into reading the question with hand-written vocabulary.
const SUPERLATIVES: &[&str] = &[
    "best", "worst", "most", "least", "top", "greatest", "largest", "biggest",
    "smallest", "highest", "lowest", "strongest", "weakest", "fastest", "slowest",
    "cheapest", "nearest", "closest", "furthest", "farthest", "longest",
    "shortest", "heaviest", "lightest", "oldest", "newest", "safest",
];

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "of", "is",
    "are", "was", "were", "be", "by", "with", "for", "from", "that", "this",
    "which", "who", "what", "when", "where", "how", "has", "have", "had",
    "will", "would", "could", "should", "may", "might", "shall", "can",
    "its", "itself", "he", "she", "they", "them", "their", "his", "her",
    "you", "your", "we", "our", "not", "no", "do", "does", "did",
    "if", "then", "than", "about", "into", "after", "before", "above", "below",
];

const NAME_KEYS: &[&str] = &["name", "title", "displayName", "wikiPage"];

pub fn words_of(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() || ch == '\'' || ch == '\u{2019}' || ch == '-' {
            cur.push(ch.to_lowercase().next().unwrap_or(ch));
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn salient(words: &[String]) -> Vec<String> {
    words
        .iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()) && w.chars().count() >= 3)
        .cloned()
        .collect()
}

pub fn wants_single_best(prompt: &str) -> bool {
    words_of(prompt).iter().any(|t| SUPERLATIVES.contains(&t.as_str()))
}

fn surface_forms(cls: &str) -> HashSet<String> {
    let c = cls.to_lowercase();
    let mut f = HashSet::new();
    f.insert(c.clone());
    f.insert(format!("{c}s"));
    f.insert(format!("{c}es"));
    if let Some(stem) = c.strip_suffix('y') {
        f.insert(format!("{stem}ies"));
    }
    if let Some(stem) = c.strip_suffix("ies") {
        f.insert(format!("{stem}y"));
    }
    if let Some(stem) = c.strip_suffix("es") {
        f.insert(stem.to_string());
    }
    if let Some(stem) = c.strip_suffix('s') {
        f.insert(stem.to_string());
    }
    for (sg, pl) in IRREGULAR_PLURALS {
        if c == *sg {
            f.insert(pl.to_string());
        }
        if c == *pl {
            f.insert(sg.to_string());
        }
    }
    f
}

/// The schema fast path for the answer kind: the head noun of a `which`/`what`
/// question, and NOTHING ELSE.
///
/// A class name appearing anywhere else in the question is not evidence about
/// the answer's kind — it is usually what the question points *through* to
/// reach its answer, or part of an entity's name. Accepting those measured
/// worse than just asking the model: "who leads Team Cascade?" matched `Team`
/// from the entity name (the answer is a Person), and "what compound inhibits
/// this pathway?" matched `Pathway` from the object (the answer is a Drug) —
/// both confidently wrong, and both of which the Choice gets right.
pub fn answer_kind_fast(prompt: &str, classes: &[String]) -> Option<String> {
    if classes.is_empty() {
        return None;
    }
    let toks = words_of(prompt);
    if toks.is_empty() {
        return None;
    }
    let forms: Vec<(&String, HashSet<String>)> =
        classes.iter().map(|c| (c, surface_forms(c))).collect();
    let match_in = |window: &[String]| -> Option<String> {
        let mut hit: HashSet<&String> = HashSet::new();
        for t in window {
            for (c, f) in &forms {
                if f.contains(t) {
                    hit.insert(c);
                }
            }
        }
        if hit.len() == 1 {
            hit.into_iter().next().cloned()
        } else {
            None
        }
    };
    let q = toks.iter().position(|t| t == "which" || t == "what")?;
    let end = (q + 4).min(toks.len());
    if q + 1 >= end {
        return None;
    }
    match_in(&toks[q + 1..end])
}

/// `ANIMAL_DROPS` -> {animal drops, drops, drop}; `LEADER_OF` -> {leader}
fn rel_surface_forms(rel: &str) -> HashSet<String> {
    let mut f = HashSet::new();
    for w in rel.to_lowercase().split('_') {
        if w.is_empty() || w == "of" || w == "to" || w == "in" {
            continue;
        }
        f.insert(w.to_string());
        if let Some(s) = w.strip_suffix('s') {
            f.insert(s.to_string());
        }
        if let Some(s) = w.strip_suffix("es") {
            f.insert(s.to_string());
        }
    }
    f
}

pub fn class_of(slug: &str) -> &str {
    slug.split('/').next().unwrap_or("")
}

pub fn base_name(slug: &str) -> &str {
    slug.split('/').next_back().unwrap_or(slug)
}

// ─── the world: a lazily cached slice of the index ──────────────────────────

pub struct World<'a> {
    store: &'a Store,
    pub tbox: &'a TBox,
    pub classes: Vec<String>,
    /// Records, loaded in batches. A menu label needs the record (that is where
    /// the measured `employment.title` / `identity.givenName` bits come from),
    /// but not the record's edges.
    rows: HashMap<String, Option<EntityRow>>,
    /// (out, inn) per node, loaded only for nodes the crawl actually stands on
    /// or anchors to.
    edges: HashMap<String, (Vec<(String, String)>, Vec<(String, String)>)>,
    labels: HashMap<String, String>,
    class_counts: Vec<(String, i64)>,
}

impl<'a> World<'a> {
    pub async fn new(store: &'a Store, tbox: &'a TBox) -> Result<World<'a>> {
        let class_counts = store.class_counts().await?;
        let classes: Vec<String> = class_counts.iter().map(|(c, _)| c.clone()).collect();
        Ok(World {
            store,
            tbox,
            classes,
            rows: HashMap::new(),
            edges: HashMap::new(),
            labels: HashMap::new(),
            class_counts,
        })
    }

    /// Load records for a batch of nodes in one query. Edges are not fetched.
    pub async fn ensure_rows(&mut self, slugs: &[String]) -> Result<()> {
        let want: Vec<String> = slugs
            .iter()
            .filter(|s| !self.rows.contains_key(*s))
            .cloned()
            .collect();
        if want.is_empty() {
            return Ok(());
        }
        let got = self.store.get_many(&want).await?;
        let found: HashSet<String> = got.iter().map(|r| r.slug.clone()).collect();
        for r in got {
            self.labels.insert(r.slug.clone(), r.label.clone());
            self.rows.insert(r.slug.clone(), Some(r));
        }
        for w in want {
            if !found.contains(&w) {
                self.rows.insert(w, None);
            }
        }
        Ok(())
    }

    /// Load a node's record and both edge directions, once.
    pub async fn ensure(&mut self, slug: &str) -> Result<()> {
        self.ensure_rows(&[slug.to_string()]).await?;
        if self.edges.contains_key(slug) {
            return Ok(());
        }
        let canonical = match self.rows.get(slug).and_then(|r| r.as_ref()) {
            Some(r) => r.slug.clone(),
            None => {
                self.edges.insert(slug.to_string(), (vec![], vec![]));
                return Ok(());
            }
        };
        let o = self.store.expand(&[canonical.clone()], true, ADJ_CAP).await?;
        let i = self.store.expand(&[canonical.clone()], false, ADJ_CAP).await?;
        let out: Vec<(String, String)> = o.into_iter().map(|l| (l.rel, l.to)).collect();
        let inn: Vec<(String, String)> = i.into_iter().map(|l| (l.rel, l.from)).collect();
        let mut need: Vec<String> = out.iter().map(|(_, t)| t.clone()).collect();
        need.extend(inn.iter().map(|(_, f)| f.clone()));
        need.retain(|s| !self.labels.contains_key(s));
        if !need.is_empty() {
            for (k, v) in self.store.labels_of(&need).await? {
                self.labels.insert(k, v);
            }
        }
        self.edges.insert(slug.to_string(), (out, inn));
        Ok(())
    }

    pub fn out(&self, slug: &str) -> &[(String, String)] {
        self.edges.get(slug).map(|(o, _)| o.as_slice()).unwrap_or(&[])
    }

    pub fn inn(&self, slug: &str) -> &[(String, String)] {
        self.edges.get(slug).map(|(_, i)| i.as_slice()).unwrap_or(&[])
    }

    pub fn exists(&self, slug: &str) -> bool {
        self.rows.get(slug).map(|r| r.is_some()).unwrap_or(false)
    }

    pub fn label(&self, slug: &str) -> String {
        self.labels
            .get(slug)
            .cloned()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| pretty_slug(base_name(slug)))
    }

    pub fn row(&self, slug: &str) -> Option<&EntityRow> {
        self.rows.get(slug).and_then(|r| r.as_ref())
    }

    /// The node's full content as the decision model sees it: class, name, every
    /// own field, and one line per relation. Edge qualifiers are deliberately
    /// left out — they are metadata about the edge, not about either node.
    pub fn full(&self, slug: &str) -> String {
        let mut lines = vec![format!("{} - {}", class_of(slug), self.label(slug))];
        if let Some(r) = self.row(slug) {
            let e = r.to_entity();
            for (alias, fields) in &e.components {
                for (k, v) in fields {
                    if v.is_null() {
                        continue;
                    }
                    let txt = match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(a) => a
                            .iter()
                            .map(|x| x.as_str().map(|s| s.to_string()).unwrap_or_else(|| x.to_string()))
                            .collect::<Vec<_>>()
                            .join(", "),
                        other => other.to_string(),
                    };
                    if txt.trim().is_empty() {
                        continue;
                    }
                    lines.push(format!("  {alias}.{k}: {txt}"));
                }
            }
        }
        let mut by_rel: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (rel, to) in self.out(slug) {
            by_rel.entry(rel).or_default().push(to);
        }
        for (rel, tos) in by_rel {
            lines.push(format!("  REL {rel}: {}", tos.join(" | ")));
        }
        lines.join("\n")
    }

    /// The relation joining two nodes, in either direction.
    pub fn rel_label(&self, from: &str, to: &str) -> String {
        for (rel, t) in self.out(from) {
            if t == to {
                return rel.clone();
            }
        }
        for (rel, f) in self.inn(from) {
            if f == to {
                return rel.clone();
            }
        }
        for (rel, t) in self.out(to) {
            if t == from {
                return rel.clone();
            }
        }
        String::new()
    }

    /// A one-line description, used in every menu option, every class example
    /// and the goal noul's header.
    ///
    /// A choice criterion must carry enough to DECIDE on. The named fields are
    /// brain-db's and they are MEASURED: `employment.title` is what lets a nav
    /// menu satisfy "non-lead" (the prompt tells the model a Lead/Head/Manager
    /// is not non-lead, and that reads these strings), and `identity.givenName`
    /// is what satisfies "first name starting with C". They simply do not fire
    /// on a schema with no such fields, which is why the generic fallback
    /// exists underneath.
    pub fn brief(&self, slug: &str) -> String {
        let cls = class_of(slug);
        let name = self.label(slug);
        let mut bits: Vec<String> = Vec::new();
        if let Some(r) = self.row(slug) {
            let e = r.to_entity();
            let f = |p: &str| -> Option<String> {
                e.field(p).and_then(|v| v.as_str().map(|s| s.to_string())).filter(|s| !s.trim().is_empty())
            };
            if cls == "Person" {
                if let Some(g) = f("identity.givenName") {
                    bits.push(format!("first name {g}"));
                }
                if let Some(t) = f("employment.title") {
                    bits.push(t);
                }
            } else if let Some(serde_json::Value::Array(sh)) = e.field("naming.shorthand") {
                let j: Vec<String> = sh.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
                if !j.is_empty() {
                    bits.push(format!("aka {}", j.join("/")));
                }
            }
            if let Some(l) = f("info.lifecycle") {
                bits.push(format!("lifecycle {l}"));
            }
            if let Some(c) = f("info.category") {
                bits.push(c);
            }
            // Generic fallback: on an unknown schema, say SOMETHING about the
            // node rather than just repeating its name. Short scalars only — a
            // field is descriptive if it is brief; prose and urls are not menu
            // material — and never the name field itself.
            if bits.is_empty() {
                'outer: for fields in e.components.values() {
                    for (fk, fv) in fields {
                        if bits.len() >= 2 {
                            break 'outer;
                        }
                        if NAME_KEYS.contains(&fk.as_str()) || fk == "spawnCode" {
                            continue;
                        }
                        match fv {
                            serde_json::Value::Array(a)
                                if !a.is_empty() && a.iter().all(|x| x.is_string()) =>
                            {
                                let j = a
                                    .iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                if j.len() <= 48 {
                                    bits.push(format!("{fk} {j}"));
                                }
                            }
                            serde_json::Value::String(s) => {
                                let t = s.trim();
                                if t.is_empty() || t.len() > 40 || t.starts_with("http") {
                                    continue;
                                }
                                bits.push(format!("{fk} {t}"));
                            }
                            serde_json::Value::Number(n) => bits.push(format!("{fk} {n}")),
                            _ => {}
                        }
                    }
                }
            }
        }
        let head = if name.to_lowercase().starts_with(&cls.to_lowercase()) {
            name
        } else {
            format!("{cls} {name}")
        };
        if bits.is_empty() {
            head
        } else {
            format!("{head} ({})", bits.join("; "))
        }
    }

    /// Everything a ranking Choice needs: the node's own scalar fields, which
    /// is where the quantity being ranked lives.
    ///
    /// List-valued fields too — the discriminator is often in them. Mandrake
    /// Soup and Cooked Mandrake both heal 100, and what separates them is the
    /// soup's role "Sleep-Free Healing", invisible while this skipped arrays,
    /// which left the ranking a coin flip between equal numbers.
    pub fn rank_label(&self, slug: &str) -> String {
        // List fields FIRST, then scalars. The ordering is the whole point:
        // Mandrake Soup and Cooked Mandrake both heal 100 and both restore 150
        // hunger, so every scalar on them is a tie, and what separates them is
        // the soup's curated role "Sleep-Free Healing" against the other's
        // "Sleep Effects". With the fields emitted in map order and capped by
        // count, a dozen tied scalars crowded the roles out of the label
        // entirely and the ranking was a coin flip between equal numbers —
        // which is the same failure as skipping arrays, one layer along.
        // A curated category list is the most discriminating field a node has
        // by construction, so it goes first and the budget is on characters.
        let mut lists: Vec<String> = Vec::new();
        let mut scalars: Vec<String> = Vec::new();
        if let Some(r) = self.row(slug) {
            let e = r.to_entity();
            for fields in e.components.values() {
                for (fk, fv) in fields {
                    if NAME_KEYS.contains(&fk.as_str()) {
                        continue;
                    }
                    match fv {
                        serde_json::Value::Array(a) if !a.is_empty() && a.iter().all(|x| x.is_string()) => {
                            let j = a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ");
                            if j.len() <= 160 {
                                lists.push(format!("{fk}: {j}"));
                            }
                        }
                        serde_json::Value::String(s) => {
                            let t = s.trim();
                            if t.is_empty() || t.len() > 90 || t.starts_with("http") {
                                continue;
                            }
                            scalars.push(format!("{fk}: {t}"));
                        }
                        serde_json::Value::Number(n) => scalars.push(format!("{fk}: {n}")),
                        _ => {}
                    }
                }
            }
        }
        let mut bits: Vec<String> = Vec::new();
        let mut used = 0usize;
        for b in lists.into_iter().chain(scalars) {
            if used + b.len() > 700 && !bits.is_empty() {
                break;
            }
            used += b.len();
            bits.push(b);
        }
        let l = self.label(slug);
        if bits.is_empty() {
            l
        } else {
            format!("{l}  --  {}", bits.join("; "))
        }
    }

    /// Several examples per class, spread across it at an even stride.
    ///
    /// A class is identified by the range of things in it, and one instance
    /// does not show a range: "Item -- 190 node(s), e.g. Abigail's Flower"
    /// gives the model no reason to believe Item is where food lives, which is
    /// why "what do beefalo eat?" came back `*none*` at p=0.889.
    pub async fn class_examples(&mut self) -> Result<Vec<(String, i64, String, Vec<String>)>> {
        let mut out = Vec::new();
        for (cls, count) in self.class_counts.clone() {
            if count == 0 {
                continue;
            }
            let want = (CLASS_EXAMPLES as i64).min(count);
            let stride = (count / want.max(1)).max(1);
            let mut picked: Vec<String> = Vec::new();
            for i in 0..want {
                let off = i * stride;
                if off >= count {
                    break;
                }
                if let Some((slug, _, _)) = self.store.list_class(Some(&cls), 1, off).await?.into_iter().next() {
                    picked.push(slug);
                }
            }
            if picked.is_empty() {
                continue;
            }
            self.ensure(&picked[0]).await?;
            let labels = self.store.labels_of(&picked).await?;
            let names: Vec<String> = picked
                .iter()
                .map(|s| labels.get(s).cloned().unwrap_or_else(|| pretty_slug(base_name(s))))
                .collect();
            let example = self.brief(&picked[0]);
            out.push((cls, count, example, names));
        }
        Ok(out)
    }

    pub async fn count_of_class(&self, cls: &str) -> i64 {
        self.class_counts
            .iter()
            .find(|(c, _)| c == cls)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// Substring/keyword search over the index. brain1 matched a substring
    /// against a per-node "rich text" blob; postgres gives the ranked version
    /// of the same thing for free.
    pub async fn search(&mut self, term: &str) -> Result<Vec<String>> {
        let mut hits: Vec<String> = self
            .store
            .keyword(term, 64)
            .await?
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        if hits.is_empty() {
            hits = self.store.contains(term, 64).await?.into_iter().map(|(s, _)| s).collect();
        }
        let need: Vec<String> = hits.iter().filter(|s| !self.labels.contains_key(*s)).cloned().collect();
        if !need.is_empty() {
            for (k, v) in self.store.labels_of(&need).await? {
                self.labels.insert(k, v);
            }
        }
        Ok(hits)
    }

    pub async fn instances_of(&self, cls: &str, limit: i64) -> Result<Vec<String>> {
        Ok(self
            .store
            .list_class(Some(cls), limit, 0)
            .await?
            .into_iter()
            .map(|(s, _, _)| s)
            .collect())
    }

    /// The entity the question NAMES: earliest match wins, longest breaks a tie.
    ///
    /// It used to be longest-wins outright, and that picked the wrong noun
    /// whenever a question named two entities. "can wurt eat meat?" matches both
    /// `Character/Wurt` and `Item/Meat` at four characters each, Meat won on
    /// insertion order, and the T-box reading then took `EATS` (Animal -> Item)
    /// off the Item end and declared the answer kind to be Animal — so
    /// `Character/Wurt`, which holds the answer, could never be committed.
    /// English puts the subject first, and the subject is what a question is
    /// ABOUT.
    ///
    /// ALL the entities tied on the best match are returned, because a name is
    /// not unique: dstwiki has three nodes titled exactly "Mandrake" and which
    /// one a question means is a judgement, not something insertion order should
    /// decide.
    pub async fn question_objects(&mut self, prompt: &str) -> Result<Vec<String>> {
        let toks = words_of(prompt);
        if toks.is_empty() {
            return Ok(vec![]);
        }
        let joined = format!(" {} ", toks.join(" "));
        // Invert brain1's scan of every entity name: build the question's
        // n-grams and ask the index which of them name something. One query,
        // instead of one pass over two million labels.
        let mut grams: Vec<(usize, String)> = Vec::new();
        for i in 0..toks.len() {
            for j in i..toks.len().min(i + 6) {
                let g = toks[i..=j].join(" ");
                if g.chars().count() <= 3 {
                    continue;
                }
                let at = joined.find(&format!(" {g} ")).or_else(|| joined.find(&format!(" {g}s ")));
                if let Some(at) = at {
                    grams.push((at, g));
                }
            }
        }
        if grams.is_empty() {
            return Ok(vec![]);
        }
        let mut best: Vec<String> = Vec::new();
        let mut best_at = usize::MAX;
        let mut best_len = 0usize;
        for (at, g) in &grams {
            // also try the underscored slug-id spelling
            for cand in [g.clone(), g.replace(' ', "_")] {
                for row in self.store.by_label(&cand, 8).await? {
                    self.labels.insert(row.slug.clone(), row.label.clone());
                    let l = g.chars().count();
                    if *at < best_at || (*at == best_at && l > best_len) {
                        best = vec![row.slug.clone()];
                        best_at = *at;
                        best_len = l;
                    } else if *at == best_at && l == best_len && !best.contains(&row.slug) {
                        best.push(row.slug.clone());
                    }
                }
            }
        }
        for s in best.clone() {
            self.ensure(&s).await?;
        }
        Ok(best)
    }

    /// The answer's kind read off a DECLARED RELATION — a T-box fact, zero
    /// model calls. Consulted only when the class Choice has ABSTAINED, so it
    /// cannot touch a question the Choice already gets right.
    ///
    /// 1. find declared relations whose name the question uses (`EATS` <- "eat")
    /// 2. find the entity the question names ("beefalo")
    /// 3. the answer is at the end that entity is *not* at -> `Item`
    ///
    /// Step 3 is what keeps "which team supports <product>?" right: the object
    /// is the Product end, so the answer is the Team end.
    ///
    /// There is exactly ONE tier, and deliberately no fallback: the question
    /// names a relation AND the named node actually has such an edge. A
    /// relation the object's *class* could sit at an end of, but which no real
    /// edge at that object uses, is a guess — and a guess here overrides the
    /// class Choice's `*none*`, which is the signal for fact mode. "how to
    /// craft merm king" resolved its object to `Animal/Merm`, no craft relation
    /// touches that node, and the class-level guess said `Item`, which made
    /// every node holding the answer uncommittable while the crawl walked the
    /// whole correct path collecting nothing.
    pub async fn relation_kind(&mut self, prompt: &str) -> Result<Option<(String, Vec<String>, String)>> {
        let toks = words_of(prompt);
        if toks.is_empty() {
            return Ok(None);
        }
        let mut hit: Vec<(String, String, String)> = Vec::new(); // (rel, from, to)
        for (name, d) in &self.tbox.relations {
            let (Some(from), Some(to)) = (&d.domain, &d.range) else { continue };
            if !self.classes.contains(from) || !self.classes.contains(to) {
                continue;
            }
            for form in rel_surface_forms(name) {
                if form.chars().count() < 3 {
                    continue;
                }
                let matched = toks.iter().any(|t| {
                    *t == form
                        || (form.chars().count() >= 4 && t.starts_with(&form))
                        || (t.chars().count() >= 4 && form.starts_with(t.as_str()))
                });
                if matched {
                    hit.push((name.clone(), from.clone(), to.clone()));
                    break;
                }
            }
        }
        if hit.is_empty() {
            return Ok(None);
        }
        let objs = self.question_objects(prompt).await?;
        // No object found means no anchor for "the far end", so this declines
        // rather than guessing: without that guard it answered Product for
        // "which team supports <product>?" purely because the product's name
        // has a colon in it and did not match.
        let Some(obj) = objs.first().cloned() else { return Ok(None) };
        let obj_cls = class_of(&obj).to_string();
        self.ensure(&obj).await?;
        let mut at_obj: HashSet<String> = HashSet::new();
        for (rel, _) in self.out(&obj) {
            at_obj.insert(rel.clone());
        }
        for (rel, _) in self.inn(&obj) {
            at_obj.insert(rel.clone());
        }
        let use_: Vec<&(String, String, String)> = hit.iter().filter(|(n, _, _)| at_obj.contains(n)).collect();
        if use_.is_empty() {
            return Ok(None);
        }
        let mut kinds: HashSet<String> = HashSet::new();
        for (_, from, to) in &use_ {
            if obj_cls == *from && obj_cls != *to {
                kinds.insert(to.clone());
            } else if obj_cls == *to && obj_cls != *from {
                kinds.insert(from.clone());
            } else {
                kinds.insert(to.clone());
            }
        }
        if kinds.len() != 1 {
            return Ok(None);
        }
        let names: Vec<String> = use_.iter().map(|(n, _, _)| n.clone()).collect();
        Ok(Some((kinds.into_iter().next().unwrap(), names, obj)))
    }
}

// ─── the answer slot ────────────────────────────────────────────────────────

/// Set on the FIRST collect: the slot the whole answer set lives in. Which
/// shape it takes is decided by HOW the first answer was reached, which needs
/// no extra question — an answer reached by `move:` has a via-edge, one found
/// by search or jump does not.
#[derive(Debug, Clone, PartialEq)]
pub enum Anchor {
    /// The answer was reached by walking an edge from the object the question
    /// names, so the rest of the set is its siblings: same hub, same relation,
    /// same direction.
    Relational { hub: String, rel: String, dir: Dir, extend: bool, dist: Option<usize> },
    /// The question named no object to walk from ("which games do we have?"),
    /// so there is no hub and no relation: the candidate set is every instance
    /// of the kind.
    Class { cls: String },
    /// The answer is one node's own content. There is no set, so the frontier
    /// is empty and FINISH fires on the next tick.
    Fact,
    /// A superlative whose kind the class Choice abstained on: gather the
    /// neighbourhood of wherever the chain roots, then rank it relatively.
    RankHub { hub: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

impl Dir {
    fn word(&self) -> &'static str {
        match self {
            Dir::In => "into",
            Dir::Out => "out of",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Opt {
    pub id: String,
    pub label: String,
    pub group: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub items: Vec<Opt>,
    pub truncated: usize,
    /// Only true when an out-edge option is actually on THIS menu, so the seek
    /// prompt keeps its original wording byte-for-byte on every turn that has
    /// none — a prompt change that reaches turns it was not meant for is how a
    /// goal-noul rewrite destroyed the relational cases once already.
    pub has_out: bool,
}

impl Options {
    fn len(&self) -> usize {
        self.items.len()
    }
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Orient,
    Seek,
    Enumerate,
    Recover,
    Commit,
    Rank,
    Finish,
}

impl Stage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Orient => "orient",
            Stage::Seek => "seek",
            Stage::Enumerate => "enumerate",
            Stage::Recover => "recover",
            Stage::Commit => "commit",
            Stage::Rank => "rank",
            Stage::Finish => "finish",
        }
    }
}

// ─── the crawl engine ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Via {
    pub from: String,
    pub rel: String,
    pub dir: Dir,
}

/// Everything that makes the crawl replayable.
pub struct Crawl {
    pub prompt: String,
    pub cursor: Option<String>,
    pub root: Option<String>,
    pub collected: Vec<String>,
    pub done: bool,
    pub visited: Vec<String>,
    /// Persistent descriptions — the content pinned for the finalizer.
    pub read: Vec<String>,
    read_slugs: HashSet<String>,
    pub seen: Vec<String>,
    seen_set: HashSet<String>,
    pub search_terms: Vec<String>,
    search_terms_set: HashSet<String>,
    searched: HashSet<String>,
    /// Nodes reached from the root via REAL in-edges.
    on_chain: HashSet<String>,
    /// Nodes the goal noul already ruled out, with the score it gave. A node is
    /// judged ONCE: without this the tree would re-ask the same noul every turn
    /// the cursor sat on a rejected node, and could never move past it.
    pub rejected_at: HashMap<String, Option<f64>>,
    /// Answers that were collected, then lost the ranking. Kept apart from
    /// `rejected_at`, which means "the goal noul ruled this out": these DID
    /// qualify, they were simply not the best one.
    outranked: HashSet<String>,
    pub via: HashMap<String, Via>,
    pub anchor: Option<Anchor>,
    pub answer_kind: Option<String>,
    pub question_rels: Vec<String>,
    /// Whether the question restricts the answer by a property of the NODE, as
    /// opposed to only by where the node sits. `None` until asked.
    pub has_node_filter: Option<bool>,
    pub fact_mode: bool,
    pub rank_mode: bool,
    pub ranked: bool,
    pub question_objs: Vec<String>,
    pub shortlist: Vec<String>,
    /// (action @ node) pairs already chosen. Re-offering a choice the model
    /// already made at this node would re-play the same result — oscillation,
    /// not exploration.
    done_at: HashSet<String>,
    pub stall: Option<String>,
    no_progress: usize,
    pub bt_last_stage: Option<Stage>,
    pub bt_ticks: usize,
}

impl Crawl {
    pub fn new(prompt: &str, start: Option<String>, shortlist: Vec<String>, question_objs: Vec<String>) -> Crawl {
        let mut c = Crawl {
            prompt: prompt.to_string(),
            cursor: start.clone(),
            root: start.clone(),
            collected: vec![],
            done: false,
            visited: vec![],
            read: vec![],
            read_slugs: HashSet::new(),
            seen: vec![],
            seen_set: HashSet::new(),
            search_terms: vec![],
            search_terms_set: HashSet::new(),
            searched: HashSet::new(),
            on_chain: HashSet::new(),
            rejected_at: HashMap::new(),
            outranked: HashSet::new(),
            via: HashMap::new(),
            anchor: None,
            answer_kind: None,
            question_rels: vec![],
            has_node_filter: None,
            fact_mode: false,
            rank_mode: false,
            ranked: false,
            question_objs,
            shortlist: shortlist.into_iter().take(10).collect(),
            done_at: HashSet::new(),
            stall: None,
            no_progress: 0,
            bt_last_stage: None,
            bt_ticks: 0,
        };
        for t in salient(&words_of(prompt)) {
            c.add_term(t);
        }
        if let Some(s) = start {
            c.visited.push(s.clone());
            c.add_seen(&s);
            c.on_chain.insert(s);
        }
        c
    }

    fn add_term(&mut self, t: String) {
        if self.search_terms_set.insert(t.clone()) {
            self.search_terms.push(t);
        }
    }

    fn add_seen(&mut self, slug: &str) {
        if self.seen_set.insert(slug.to_string()) {
            self.seen.push(slug.to_string());
        }
        self.add_term(base_name(slug).to_lowercase());
    }

    pub fn is_question_object(&self, slug: Option<&str>) -> bool {
        match slug.or(self.cursor.as_deref()) {
            Some(s) => self.question_objs.iter().any(|q| q == s),
            None => false,
        }
    }

    pub fn wants_single_best(&self) -> bool {
        wants_single_best(&self.prompt)
    }

    pub fn via_rel(&self, slug: Option<&str>) -> Option<&str> {
        slug.or(self.cursor.as_deref())
            .and_then(|s| self.via.get(s))
            .map(|v| v.rel.as_str())
    }

    pub fn on_chain(&self, slug: Option<&str>) -> bool {
        match slug.or(self.cursor.as_deref()) {
            Some(s) => self.on_chain.contains(s),
            None => false,
        }
    }

    pub fn on_chain_any(&self) -> bool {
        !self.on_chain.is_empty()
    }

    pub fn rejected(&self, slug: Option<&str>) -> bool {
        match slug.or(self.cursor.as_deref()) {
            Some(s) => self.rejected_at.contains_key(s) || self.outranked.contains(s),
            None => false,
        }
    }

    pub fn reject(&mut self, slug: &str, noul: Option<f64>) {
        self.rejected_at.entry(slug.to_string()).or_insert(noul);
    }

    fn done_at(&self, id: &str, cur: Option<&str>) -> bool {
        self.done_at.contains(&format!("{id}@{}", cur.unwrap_or("?")))
    }

    fn set_rank_hub(&mut self) {
        if self.rank_mode && self.collected.is_empty() {
            if let Some(r) = &self.root {
                self.anchor = Some(Anchor::RankHub { hub: r.clone() });
            }
        }
    }

    fn mark_chain(&mut self, to: &str, from: Option<&str>) {
        if self.on_chain.is_empty() {
            self.on_chain.insert(to.to_string());
            if self.root.is_none() {
                self.root = Some(to.to_string());
            }
            self.set_rank_hub();
            return;
        }
        if let Some(f) = from {
            if self.on_chain.contains(f) {
                self.on_chain.insert(to.to_string());
            }
        }
    }

    /// Hops from the thing the question is ABOUT to `slug`, along the chain the
    /// crawl actually walked.
    ///
    /// The named entity is preferred, but only when the chain actually runs
    /// THROUGH it; otherwise the root. Both halves are needed. Measuring from
    /// the root alone over-counts when orientation lands *near* the object
    /// rather than on it — one question oriented onto a neighbour and measured
    /// 3 hops instead of 1, which switched extension on for an already-exact
    /// slot and ran to 160 steps collecting most of the graph. Measuring from
    /// the named entity alone fails when the chain bypasses it.
    fn chain_dist(&self, slug: &str) -> Option<usize> {
        let walk = |target: &str| -> Option<usize> {
            let mut d = 0usize;
            let mut cur = slug.to_string();
            let mut guard = 0;
            while cur != target && guard < 64 {
                guard += 1;
                let v = self.via.get(&cur)?;
                cur = v.from.clone();
                d += 1;
            }
            if cur == target {
                Some(d)
            } else {
                None
            }
        };
        if let Some(obj) = self.question_objs.first() {
            if let Some(d) = walk(obj) {
                return Some(d);
            }
        }
        self.root.as_deref().and_then(walk)
    }

    /// A candidate is live until it has been ruled on — collected or rejected.
    /// It used to be "not yet visited", which only worked while judging
    /// required stepping onto the node.
    fn live(&self, slug: &str) -> bool {
        !self.collected.iter().any(|c| c == slug)
            && !self.rejected_at.contains_key(slug)
            && !self.outranked.contains(slug)
    }

    pub fn bt_latch(&mut self, stage: Stage) -> bool {
        let changed = self.bt_last_stage != Some(stage);
        self.bt_last_stage = Some(stage);
        self.bt_ticks += 1;
        changed
    }
}

// ─── option sets, one grounded menu per stage ───────────────────────────────

/// The remaining answer candidates.
pub fn frontier(c: &Crawl, w: &World, class_members: &[String]) -> Vec<String> {
    let Some(anchor) = &c.anchor else { return vec![] };
    match anchor {
        Anchor::Fact => vec![],
        Anchor::RankHub { hub } => {
            let mut out: Vec<String> = Vec::new();
            for (_, from) in w.inn(hub) {
                if !out.contains(from) && c.live(from) {
                    out.push(from.clone());
                }
            }
            out.truncate(MAX_CLASS_ENUM);
            out
        }
        Anchor::Class { .. } => class_members
            .iter()
            .filter(|s| c.live(s))
            .take(MAX_CLASS_ENUM)
            .cloned()
            .collect(),
        Anchor::Relational { hub, rel, dir, extend, .. } => {
            let mut cands: Vec<String> = match dir {
                Dir::Out => w
                    .out(hub)
                    .iter()
                    .filter(|(r, _)| r == rel)
                    .map(|(_, t)| t.clone())
                    .collect(),
                Dir::In => w
                    .inn(hub)
                    .iter()
                    .filter(|(r, _)| r == rel)
                    .map(|(_, f)| f.clone())
                    .collect(),
            };
            // EXTENSION through a confirmed answer. The sibling slot assumes
            // the whole answer set sits in ONE relational position, and that
            // assumption breaks when it spans two — the lead of an org reached
            // by LEADER_OF and the four engineers under them by REPORTS_TO.
            //
            // Gated on `extend`, set when the first answer was NOT adjacent to
            // the question's object. That distinction is the whole safety
            // argument, and it is a fact, not a judgement:
            //   distance 1  — the answer sits on an edge OF the object, so that
            //     edge IS the relation being asked about and its slot is
            //     already the complete set. Extending would offer the SRE org
            //     behind the supporting team, which is not a team that supports
            //     the product; it scores 0.661 against a 0.7 gate, a 0.039
            //     margin, so NOT offering it is worth much more than trusting
            //     the noul to refuse it.
            //   distance >= 2 — no single edge expresses the question, so the
            //     answer set is the local same-kind cluster around where it
            //     landed.
            //
            // IN-edges only: out-edges onto the answer kind belong to SEEK,
            // which uses them to FIND the answer; growing the answer SET along
            // them walks the wrong way up a hierarchy. Measured — with
            // out-edges included, one question offered the supporting lead's
            // own Director of Engineering.
            //
            // Also gated on a resolved kind: extension's bound IS the answer
            // kind, and without one it grows through every in-neighbour of
            // every answer, which is a breadth-first walk of the whole graph
            // wearing a slot's clothes. And it reaches exactly ONE hop — only
            // answers that sit in the original sibling slot extend.
            if *extend && c.answer_kind.is_some() {
                let sib: HashSet<String> = cands.iter().cloned().collect();
                for got in &c.collected {
                    if !sib.contains(got) {
                        continue;
                    }
                    for (_, from) in w.inn(got) {
                        if !cands.contains(from) {
                            cands.push(from.clone());
                        }
                    }
                }
            }
            cands
                .into_iter()
                .filter(|s| c.answer_kind.as_deref().map_or(true, |k| class_of(s) == k))
                .filter(|s| c.live(s))
                .take(MAX_CLASS_ENUM)
                .collect()
        }
    }
}

/// A search term is only an option if it actually surfaces something new, and
/// is not so broad as to be useless.
pub fn fresh_searches(c: &Crawl, hits: &HashMap<String, Vec<String>>, suffix: &str) -> Vec<Opt> {
    let cur = c.cursor.as_deref();
    let mut out = Vec::new();
    let start = c.search_terms.len().saturating_sub(20);
    for t in &c.search_terms[start..] {
        if c.searched.contains(t) || c.done_at(&format!("search:{t}"), cur) {
            continue;
        }
        let Some(h) = hits.get(t) else { continue };
        let fresh = h.iter().filter(|s| !c.seen_set.contains(*s)).count();
        if fresh == 0 || fresh > MAX_SEARCH_FRESH {
            continue;
        }
        out.push(Opt {
            id: format!("search:{t}"),
            label: format!("search \"{t}\" -- surfaces {fresh}{suffix}"),
            group: "search",
        });
    }
    out
}

pub fn orient_options(c: &Crawl, w: &World, searches: &HashMap<String, Vec<String>>) -> Options {
    let cur = c.cursor.as_deref();
    let mut cands: Vec<String> = Vec::new();
    // The entity the question NAMES goes first, always. ORIENT is asked to pick
    // "the node the question is ABOUT", so a node whose own name is in the
    // question is the strongest candidate there can be — and it was being
    // thrown away by a 40-candidate cap that sorted by insertion order.
    // Withholding the only option that mattered is the exact opposite of what
    // grounded options are for. This ADDS an option; it does not take the
    // decision.
    for qo in &c.question_objs {
        if w.exists(qo) && !cands.contains(qo) {
            cands.push(qo.clone());
        }
    }
    for h in &c.shortlist {
        if w.exists(h) && !cands.contains(h) {
            cands.push(h.clone());
        }
    }
    for s in &c.seen {
        if !cands.contains(s) {
            cands.push(s.clone());
        }
    }
    let mut items = Vec::new();
    for sl in cands.iter().take(40) {
        if c.done_at(&format!("jump:{sl}"), cur) {
            continue;
        }
        items.push(Opt { id: format!("jump:{sl}"), label: w.brief(sl), group: "orient" });
    }
    items.extend(fresh_searches(c, searches, " more node(s) to choose from"));
    Options { items, truncated: 0, has_out: false }
}

/// The real edges of the cursor that can advance the crawl: every IN-edge (the
/// chain direction — the walk runs backwards along the relation, THROUGH
/// intermediate nodes), plus any OUT-edge that lands directly ON a node of the
/// answer kind.
///
/// Out-edges were excluded outright, and that was the design's deepest limit:
/// "what mario games do we have?" orients onto `Franchise/mario` — correctly,
/// since Mario really is the object the question names — and the five games
/// hang off it by `OWNS`, an OUT-edge. The only in-edge was a team, so the menu
/// held exactly one option at p=1.000 pointing the wrong way, and the crawl
/// walked the org chart down to an SRE and gave up.
///
/// They are admitted ONLY when the target IS the answer kind, and that
/// restriction is what keeps rule 1 intact. Admitting every out-edge would put
/// a product's 21 HAS_SERVICE neighbours on the menu — precisely the dilution
/// that got out-edges excluded in the first place.
pub fn forward_moves(c: &Crawl, w: &World) -> Options {
    let Some(cur) = c.cursor.as_deref() else { return Options::default() };
    let mut cand: Vec<(String, String, Dir)> = w
        .inn(cur)
        .iter()
        .map(|(rel, from)| (from.clone(), rel.clone(), Dir::In))
        .collect();
    for (rel, to) in w.out(cur) {
        if c.answer_kind.as_deref().map_or(true, |k| class_of(to) != k) {
            continue;
        }
        if cand.iter().any(|(t, _, _)| t == to) {
            continue;
        }
        cand.push((to.clone(), rel.clone(), Dir::Out));
    }
    // When the question names a declared relation and this node has edges on
    // it, those are the only moves offered. "what do beefalo eat?" is about
    // EATS, and Beefalo has four Item out-edges: Twigs and Cut_Grass by EATS,
    // Manure and Beefalo_Wool by ANIMAL_DROPS. All four are the answer kind, so
    // the kind filter cannot separate them — but the question can, and the
    // relation it names is an engine fact. Narrowing only ever happens when the
    // T-box supplied those names, i.e. when the class Choice abstained.
    if !c.question_rels.is_empty() {
        let on_q: Vec<_> = cand
            .iter()
            .filter(|(_, r, _)| c.question_rels.contains(r))
            .cloned()
            .collect();
        if !on_q.is_empty() {
            cand = on_q;
        }
    }
    // Fair cap: round-robin across relation types so a high-degree node cannot
    // starve a rare relation type. Keyed by direction too, so a 21-edge
    // out-relation cannot starve a rare in-relation either.
    let mut by_rel: Vec<(String, Vec<(String, String, Dir)>)> = Vec::new();
    for e in cand.iter() {
        let k = format!("{} {}", if e.2 == Dir::In { "in" } else { "out" }, e.1);
        match by_rel.iter_mut().find(|(kk, _)| *kk == k) {
            Some((_, v)) => v.push(e.clone()),
            None => by_rel.push((k, vec![e.clone()])),
        }
    }
    let mut picked: Vec<(String, String, Dir)> = Vec::new();
    let mut more = true;
    while more && picked.len() < MAX_MOVES {
        more = false;
        for (_, list) in by_rel.iter_mut() {
            if list.is_empty() {
                continue;
            }
            more = true;
            picked.push(list.remove(0));
            if picked.len() >= MAX_MOVES {
                break;
            }
        }
    }
    let mut items = Vec::new();
    let mut has_out = false;
    for (to, rel, dir) in &picked {
        if c.done_at(&format!("move:{to}"), Some(cur)) || c.collected.iter().any(|x| x == to) {
            continue;
        }
        if *dir == Dir::Out {
            has_out = true;
        }
        let how = match dir {
            Dir::In => format!("its {rel} edge INTO {}", w.label(cur)),
            Dir::Out => format!("the {rel} edge FROM {} INTO it", w.label(cur)),
        };
        items.push(Opt {
            id: format!("move:{to}"),
            label: format!("step onto {}  --  reached by {how}", w.brief(to)),
            group: "move",
        });
    }
    Options { items, truncated: cand.len().saturating_sub(picked.len()), has_out }
}

pub fn slot_candidates(c: &Crawl, w: &World, front: &[String], total: usize) -> Options {
    let cur = c.cursor.as_deref();
    let Some(anchor) = &c.anchor else { return Options::default() };
    let mut items = Vec::new();
    for cd in front {
        if c.done_at(&format!("move:{cd}"), cur) {
            continue;
        }
        let why = match anchor {
            Anchor::RankHub { hub } => format!(
                "hangs off {}, one of the candidates to be ranked",
                w.label(hub)
            ),
            Anchor::Class { cls } => format!("another {cls} still to be judged"),
            Anchor::Fact => "the answer is this node's own content".to_string(),
            Anchor::Relational { hub, rel, dir, .. } => {
                let in_slot = match dir {
                    Dir::Out => w.out(hub).iter().any(|(r, t)| t == cd && r == rel),
                    Dir::In => w.inn(hub).iter().any(|(r, f)| f == cd && r == rel),
                };
                if in_slot {
                    format!(
                        "another {rel} {} {}, same slot as the answer(s) already committed",
                        dir.word(),
                        w.label(hub)
                    )
                } else {
                    "hangs off an answer already committed, still to be judged".to_string()
                }
            }
        };
        items.push(Opt {
            id: format!("move:{cd}"),
            label: format!("step onto {}  --  {why}", w.brief(cd)),
            group: "move",
        });
    }
    let truncated = total.saturating_sub(items.len());
    Options { items, truncated, has_out: false }
}

/// Only reachable when the chain has run out: going backwards, or widening the
/// net. Never offered while a forward in-edge exists — on a chain that runs
/// outward, backwards is not progress.
pub fn recovery_options(c: &Crawl, w: &World, searches: &HashMap<String, Vec<String>>) -> Options {
    let cur = c.cursor.as_deref();
    let mut items = Vec::new();
    let start = c.seen.len().saturating_sub(8);
    for sl in &c.seen[start..] {
        if Some(sl.as_str()) == cur {
            continue;
        }
        if c.done_at(&format!("jump:{sl}"), cur) {
            continue;
        }
        let unvisited = w
            .inn(sl)
            .iter()
            .filter(|(_, f)| !c.visited.iter().any(|v| v == f))
            .count();
        if unvisited == 0 {
            continue;
        }
        let live = w
            .inn(sl)
            .iter()
            .map(|(_, f)| f)
            .filter(|f| {
                !c.collected.iter().any(|x| x == *f)
                    && c.answer_kind.as_deref().map_or(true, |k| class_of(f) == k)
                    && !c.done_at(&format!("move:{f}"), Some(sl))
            })
            .count();
        let tail = if live > 0 {
            format!(
                "[{live} un-visited {} node(s) hang off this one]",
                c.answer_kind.clone().unwrap_or_else(|| "candidate".into())
            )
        } else {
            format!("[{unvisited} un-visited node(s) hang off this one]")
        };
        items.push(Opt {
            id: format!("jump:{sl}"),
            label: format!("go back to {}  {tail}", w.brief(sl)),
            group: "jump",
        });
    }
    items.extend(fresh_searches(c, searches, " node(s) you have not seen yet"));
    Options { items, truncated: 0, has_out: false }
}

// ─── apply: execute exactly one action ──────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
pub struct Applied {
    pub action: String,
    pub arg: Option<String>,
    pub note: String,
    pub refused: Option<String>,
}

pub fn apply(c: &mut Crawl, w: &World, id: &str, front: &[String]) -> Applied {
    let cur = c.cursor.clone();
    // Record EVERY chosen action at this node (accepted OR refused) so the
    // engine never offers the same (action @ node) again: repeating it would
    // just re-play the same result. Per-node history, not per-visit.
    if !id.is_empty() {
        c.done_at.insert(format!("{id}@{}", cur.as_deref().unwrap_or("?")));
    }
    let refused = |m: String| Applied { refused: Some(m), ..Default::default() };

    if let Some(to) = id.strip_prefix("move:") {
        let Some(cur) = cur.as_deref() else { return refused("no cursor".into()) };
        let is_out = w.out(cur).iter().any(|(_, t)| t == to);
        let is_in = w.inn(cur).iter().any(|(_, f)| f == to);
        // A sibling in the answer slot is reachable by construction: it is an
        // in-neighbour of a hub already on the chain, so stepping onto it from
        // a just-judged sibling needs no detour back through the hub.
        let is_sibling = front.iter().any(|f| f == to);
        if !is_out && !is_in && !is_sibling {
            return refused(format!("not a neighbour of {cur}"));
        }
        if is_sibling && !is_in && !is_out {
            match c.anchor.clone() {
                // A class slot has no hub: its members are candidates by TYPE,
                // not by position, so they are on-chain by construction.
                Some(Anchor::Class { .. }) => {
                    c.on_chain.insert(to.to_string());
                }
                Some(Anchor::Relational { hub, rel, dir, .. }) => {
                    c.mark_chain(to, Some(&hub));
                    c.via
                        .entry(to.to_string())
                        .or_insert(Via { from: hub, rel, dir });
                }
                Some(Anchor::RankHub { hub }) => {
                    c.mark_chain(to, Some(&hub));
                    c.via.entry(to.to_string()).or_insert(Via {
                        from: hub,
                        rel: String::new(),
                        dir: Dir::In,
                    });
                }
                _ => {}
            }
            c.cursor = Some(to.to_string());
            c.visited.push(to.to_string());
            c.add_seen(to);
            return Applied { action: "move".into(), arg: Some(to.into()), note: "in".into(), refused: None };
        }
        let rel = w.rel_label(cur, to);
        c.mark_chain(to, Some(cur));
        c.via.entry(to.to_string()).or_insert(Via {
            from: cur.to_string(),
            rel: rel.clone(),
            dir: if is_out { Dir::Out } else { Dir::In },
        });
        c.cursor = Some(to.to_string());
        c.visited.push(to.to_string());
        c.add_seen(to);
        return Applied {
            action: "move".into(),
            arg: Some(to.into()),
            note: if is_out { "out".into() } else { "in".into() },
            refused: None,
        };
    }

    // A jump deliberately does NOT extend the chain: a search hit that merely
    // looks like the answer must not become collectable. That rule was measured
    // — without it a real non-lead SRE named C-something in an unrelated org
    // got committed — and it stays.
    //
    // But a jump taken while NOTHING has been collected is not an extension, it
    // is a RE-ORIENTATION: the crawl is admitting it entered in the wrong
    // place. Then the node jumped to becomes the new root and the chain starts
    // again from it. Without this the crawl could reach the right neighbourhood
    // and be unable to use it. The guard is `collected.is_empty()`: once
    // anything is committed the slot and the chain are load-bearing and nothing
    // re-roots.
    if let Some(to) = id.strip_prefix("jump:") {
        let in_shortlist = c.shortlist.iter().any(|h| h == to);
        // The question's named entity is a legitimate entry point, exactly like
        // a shortlist hit: it is known from the question itself, not invented.
        // Without this the engine OFFERED the named entity (orient puts it
        // first) and then refused it — offering an action that cannot be taken
        // is the rule-1 failure in its purest form.
        if !c.seen_set.contains(to) && !in_shortlist && !c.question_objs.iter().any(|q| q == to) {
            return refused(format!(
                "never seen {to} (you can only jump to nodes you have already visited or seen)"
            ));
        }
        let mut note = String::new();
        if c.collected.is_empty() && !c.on_chain.is_empty() && !c.on_chain.contains(to) {
            c.on_chain.clear();
            c.on_chain.insert(to.to_string());
            c.root = Some(to.to_string());
            c.set_rank_hub();
            note = "re-oriented: new root".into();
        }
        c.mark_chain(to, None);
        c.cursor = Some(to.to_string());
        c.visited.push(to.to_string());
        c.add_seen(to);
        return Applied { action: "jump".into(), arg: Some(to.into()), note, refused: None };
    }

    if let Some(term) = id.strip_prefix("search:") {
        c.searched.insert(term.to_string());
        return Applied {
            action: "search".into(),
            arg: Some(term.into()),
            note: format!("searched {term}"),
            refused: None,
        };
    }

    // `skip:<slug>` — judged in place and ruled out. The rejection itself is
    // recorded by the tree; this makes it a first-class step so the trace and
    // the viable path show the judgement.
    if let Some(tgt) = id.strip_prefix("skip:") {
        return Applied {
            action: "skip".into(),
            arg: Some(tgt.into()),
            note: format!("ruled out {tgt}"),
            refused: None,
        };
    }

    if id == "collect" || id.starts_with("collect:") {
        let tgt = match id.strip_prefix("collect:") {
            Some(t) => t.to_string(),
            None => match &cur {
                Some(x) => x.clone(),
                None => return refused("nothing to collect".into()),
            },
        };
        if c.collected.contains(&tgt) {
            return refused(format!("already collected {tgt}"));
        }
        c.collected.push(tgt.clone());
        // Pin the node's whole content, not just its slug. The decision model
        // always sees it; the FINALIZER only ever got the slug list and a
        // count, so for a question whose answer is a PROPERTY rather than a
        // node it had nothing to answer from — one crawl collected the node
        // whose field IS the recipe and the finalizer replied "nodes read: 0".
        if c.read_slugs.insert(tgt.clone()) {
            c.read.push(w.full(&tgt));
        }
        let mut note = format!("collected {tgt}");
        // The first answer fixes the slot the whole answer set lives in.
        if c.anchor.is_none() {
            match c.via.get(&tgt).cloned() {
                Some(v) => {
                    let dist = c.chain_dist(&tgt);
                    let extend = dist.map(|d| d >= 2).unwrap_or(false);
                    note += &format!(
                        " -- answer slot = {} {} {}  [{}{}]",
                        v.rel,
                        v.dir.word(),
                        v.from,
                        match dist {
                            None => "dist ?".to_string(),
                            Some(d) => format!("{d} hop(s) from the object"),
                        },
                        if extend { ", extends through answers" } else { ", exact" }
                    );
                    c.anchor = Some(Anchor::Relational {
                        hub: v.from,
                        rel: v.rel,
                        dir: v.dir,
                        extend,
                        dist,
                    });
                }
                None if c.fact_mode => {
                    c.anchor = Some(Anchor::Fact);
                    note += " -- the answer is this node's own content; nothing to enumerate";
                }
                None => {
                    // Reached without walking a chain => the question named no
                    // object, so the answer set is the class.
                    let cls = class_of(&tgt).to_string();
                    note += &format!(" -- answer slot = every {cls}");
                    c.anchor = Some(Anchor::Class { cls });
                }
            }
        }
        return Applied { action: "collect".into(), arg: Some(tgt), note, refused: None };
    }

    if let Some(tgt) = id.strip_prefix("rank:") {
        if !c.collected.iter().any(|x| x == tgt) {
            return refused(format!("not in the answer set: {tgt}"));
        }
        let dropped: Vec<String> = c.collected.iter().filter(|x| *x != tgt).cloned().collect();
        for d in &dropped {
            c.outranked.insert(d.clone());
        }
        c.collected = vec![tgt.to_string()];
        c.ranked = true;
        return Applied {
            action: "rank".into(),
            arg: Some(tgt.into()),
            note: format!("kept {tgt} as the single best of {}", dropped.len() + 1),
            refused: None,
        };
    }

    match id {
        "give_up" => {
            c.done = true;
            Applied {
                action: "give_up".into(),
                arg: None,
                note: "gave up - explored everything reachable, none answers".into(),
                refused: None,
            }
        }
        "stop" => {
            // Fold the cursor in ONLY as a last-resort fallback (nothing
            // collected at all). With a non-empty answer set the cursor is
            // wherever navigation left us — often a hub we backtracked to — and
            // is not an answer.
            if c.collected.is_empty() {
                if let Some(x) = &cur {
                    c.collected.push(x.clone());
                    if c.read_slugs.insert(x.clone()) {
                        c.read.push(w.full(x));
                    }
                }
            }
            c.done = true;
            Applied { action: "stop".into(), arg: None, note: String::new(), refused: None }
        }
        other => refused(format!("unknown action {other}")),
    }
}

// ─── the typed questions ────────────────────────────────────────────────────

fn criteria_of(opts: &Options) -> BTreeMap<String, String> {
    opts.items.iter().map(|o| (o.id.clone(), o.label.clone())).collect()
}

pub fn nav_choice(c: &Crawl, w: &World, opts: &Options, stage: Stage) -> Question {
    let kind = c.answer_kind.clone();
    let criteria = criteria_of(opts);

    if stage == Stage::Orient {
        let mut s = String::from(
            "You are about to walk a knowledge graph to answer a question, but first you must find your STARTING POINT.\n",
        );
        s += &format!("\nQUESTION: \"{}\"\n", c.prompt);
        if let Some(k) = &kind {
            s += &format!("\nThe question's ANSWER will be a {k} node. That is NOT what you are picking now.\n");
        }
        s += "\nPick the node that is the OBJECT the question is ABOUT -- the thing named in the question. \
For \"which team supports <product>?\" the object is the PRODUCT, not a team. \
For \"who works on <product>?\" the object is the PRODUCT, not a person.\n\
Do NOT pick a node just because it is the answer's kind, and do NOT pick something that merely \
resembles the answer -- the answer is reached by walking FROM the object, and picking the wrong \
starting point sends the whole crawl into an unrelated part of the graph.\n\
If the question names no object at all (it asks for a list of everything of some kind), pick any \
node of that kind: it is the first one to judge, not a route.\n\
If none of the listed nodes fits, pick a search that would surface it.\n\
\nPick exactly one:";
        return Question::choice(stage.as_str(), criteria, s);
    }

    let where_ = match c.cursor.as_deref() {
        Some(cur) => format!(
            "\nWHERE I AM (full content + relations, already read for you):\n{}\n",
            w.full(cur)
        ),
        None => String::new(),
    };
    let chain = if c.visited.is_empty() {
        String::new()
    } else {
        format!(
            "\nCHAIN I HAVE WALKED (most recent last; I am AT the last one):\n{}\n",
            c.visited.join(" -> ")
        )
    };
    let done = if c.collected.is_empty() {
        "\nANSWERS ALREADY COMMITTED: (none yet)\n".to_string()
    } else {
        format!(
            "\nANSWERS ALREADY COMMITTED ({}): {}\n",
            c.collected.len(),
            c.collected.join(", ")
        )
    };
    let kind_line = match &kind {
        Some(k) => format!("\nTHE QUESTION ASKS FOR NODES OF KIND: {k}\n"),
        None => String::new(),
    };
    let trunc = if opts.truncated > 0 {
        format!("\n(note: {} further candidate(s) are not shown this turn)\n", opts.truncated)
    } else {
        String::new()
    };

    let head = format!(
        "You are walking a knowledge graph to answer a question. This turn you choose only WHERE TO GO NEXT.\n\
You are NOT deciding whether the current node is the answer, and NOT deciding whether to finish -- \
those are asked separately. Choose the move that best advances toward the answer.\n\
\nQUESTION: \"{}\"\n{kind_line}{where_}{chain}{done}{trunc}",
        c.prompt
    );

    let tail = match stage {
        Stage::Enumerate => {
            let lead = match &c.anchor {
                Some(Anchor::Class { cls }) => format!(
                    "\nThis question asks for a LIST. Every {cls} has to be judged one at a time, and the \
options below are the ones not yet judged. They are interchangeable -- pick any of them and the \
rest will come round in later turns. Do not try to pick the 'best' one.\n"
                ),
                Some(Anchor::Relational { rel: _, dir, .. }) => format!(
                    "\nYou have already committed at least one answer. The options below are its siblings in the same \
slot (same relation {} the same node), which is \
where the rest of the answer set lives. They \
are interchangeable -- pick any one; the rest will come round in later turns.\n",
                    match dir { Dir::Out => "out of", Dir::In => "into" }
                ),
                _ => "\nYou have already committed at least one answer. The options below are its siblings in the same \
slot, which is where the rest of the answer set lives. They are interchangeable -- pick any one; \
the rest will come round in later turns.\n"
                    .to_string(),
            };
            format!("{lead}\nPick exactly one:")
        }
        Stage::Recover => "\nThe chain has run out at this node: there is no forward in-edge left to step onto. Your options are \
to go back to a node that still has something un-visited behind it, or to widen the net with a search. \
Each option says what is actually left there.\n\
\nPick exactly one:"
            .to_string(),
        _ => {
            let out_note = if opts.has_out {
                format!(
                    "SOME OPTIONS RUN THE OTHER WAY: an option that says \"reached by the <REL> edge FROM here INTO \
it\" is a node this one points AT, and it is on the menu because it is already a {} \
-- the kind the question asks for. Those are the shortest route there is: one step and you are \
standing on a candidate. The options that run INTO this node are routes THROUGH, not arrivals.\n",
                    kind.clone().unwrap_or_else(|| "node".into())
                )
            } else {
                String::new()
            };
            format!(
                "\nHOW THE GRAPH RUNS: the answer sits several hops from the object the question names, and the chain \
runs along IN-edges. Every option below is a REAL in-edge of the node I am on; its text names the \
relation and describes the node you would land on. Read the relation and the description against the \
question and pick the one that moves toward the answer.\n{out_note}\
IF THE QUESTION HAS A FILTER (non-lead, a first-name letter, a lifecycle, ...) use the descriptions to \
pick a candidate that SATISFIES it -- a Lead/Head/Manager/Director is not 'non-lead'.\n\
\nPick exactly one:"
            )
        }
    };

    Question::choice(stage.as_str(), criteria, head + &tail)
}

pub fn kind_question(prompt: &str, examples: &[(String, i64, String, Vec<String>)]) -> Question {
    let mut criteria: BTreeMap<String, String> = BTreeMap::new();
    for (cls, count, example, names) in examples {
        // The first example keeps its descriptive bits (a job title, a
        // lifecycle) -- that is what tells "the class that holds people" from
        // the rest; the others are bare names, which is enough to show the
        // class's range for a few tokens each.
        let rest: Vec<String> = names.iter().skip(1).cloned().collect();
        criteria.insert(
            cls.clone(),
            format!(
                "{cls} -- {count} node(s), e.g. {}{}",
                if example.is_empty() { cls.clone() } else { example.clone() },
                if rest.is_empty() { String::new() } else { format!(", {}", rest.join(", ")) }
            ),
        );
    }
    // "it asks for a fact" was far too wide a door. EVERY answer is a fact, and
    // a question that does not spell out its kind reads as a fact-question: all
    // three dstwiki questions came back *none* at p=0.66-0.93 even though their
    // answers were Items sitting in the graph. So *none* is described by what it
    // really is: an answer that is not a node of any class.
    criteria.insert(
        KIND_NONE.to_string(),
        "none of these -- the answer is not a node in this graph at all: a NUMBER (how many ...), \
a DATE, a yes/no, or a measurement read off one node"
            .to_string(),
    );
    Question::choice(
        "kind",
        criteria,
        format!(
            "QUESTION: \"{prompt}\"\n\n\
What KIND of node would ANSWER this question? Pick the class the answer is an instance of.\n\
Each option below is a class in this graph's schema, with how many nodes it holds and a few real\n\
examples of what is in it. Read the examples: the class NAME alone is often too general to tell\n\
which class a given answer would live in.\n\n\
Judge by what the answer would BE, not by what the question mentions. The question names other\n\
things on the way to its answer -- those are not the answer's kind. In \"which team supports\n\
<product>?\" the answer is a team and the product is just how the question points at it; in\n\
\"which product does <team> support?\" it is the other way round. If the question asks for a\n\
PERSON by their role (an engineer, a lead, a producer, a maintainer), the answer's kind is\n\
whichever class holds people -- read the examples to tell which one that is.\n\
Ignore any adjective or filter in the question (non-lead, senior, first name starting with C):\n\
those narrow WHICH node answers, they do not change its KIND.\n\
A question that does not SAY its kind still asks for nodes. \"what do beefalo eat?\" is answered\n\
by the things a beefalo eats, and those things are nodes of whichever class holds them -- so the\n\
kind is that class, not \"none\". Pick the last option ONLY when nothing in the graph could be the\n\
answer: a count, a date, a yes/no, or a number read off a single node.\n\n\
Pick exactly one:"
        ),
    )
}

/// The commit judgement.
///
/// DO NOT reintroduce a "the engine already settled the position, judge only
/// the node's own restrictions" clause. It was written, measured and reverted:
/// scoped to COMMIT it cost one node its separation and produced a false
/// positive; scoped to ENUMERATE it barely moved the nodes it was written for
/// and WRECKED the case that was working — "what mario games do we have?" fell
/// from 0.845/0.721 to 0.384/0.434, because telling the noul that position is
/// settled stopped it reading the node's name, which is the only place "mario"
/// lives. The real lesson is upstream: a question whose entire content is
/// positional gives this noul nothing to judge, and the fix is not to re-word
/// it but to not ask it. See [`filter_question`].
pub fn goal_question(c: &Crawl, w: &World, node: &str) -> Question {
    let cls = class_of(node);
    // FACT MODE asks a different question, because "is this node one of the
    // answers?" is not the question. When the answer is a recipe, a yes/no or a
    // property, the node does not BECOME the answer -- it HOLDS it. Asked the
    // set-shaped question, the noul refused every node that held an answer
    // (0.138, 0.239, 0.337; only "how to craft X" scraped by at 0.795, and only
    // because it reads close enough to "this node is about crafting X").
    if c.fact_mode {
        return Question::noul(format!(
            "QUESTION: \"{}\"\n\
CURRENT NODE TYPE: {cls}.\n\
CURRENT NODE:\n{}\n\
\nThis question asks for a FACT, not for a list of nodes: a recipe, a yes/no, a property,\n\
a number. So you are NOT deciding whether this node is itself the answer -- it would not\n\
be. You are deciding whether THIS NODE'S OWN RECORD, printed above, CONTAINS the answer.\n\
\nAnswer TRUE when the record holds what was asked: the field is there and its value\n\
settles the question. It still counts when the answer turns out to be \"no\", when it is\n\
written as prose rather than a tidy value, and when it sits in a description rather than\n\
a field named after the question.\n\
Answer FALSE when this record is about something else entirely, or when it is about the\n\
right thing but says nothing on the point actually asked -- being related to the subject\n\
is not the same as answering the question about it.",
            c.prompt,
            w.full(node)
        ));
    }
    let class_slot = matches!(c.anchor, Some(Anchor::Class { .. }));
    let kind_line = match &c.answer_kind {
        Some(k) => format!("WANTED KIND: {k}.   CURRENT NODE TYPE: {cls}.\n"),
        None => format!("CURRENT NODE TYPE: {cls}.\n"),
    };
    let position = match &c.anchor {
        Some(Anchor::Class { cls: k }) => {
            format!("This question asks for a list, so every {k} is being checked one at a time.\n")
        }
        _ => format!("PATH SO FAR: {}\n", c.visited.join(" -> ")),
    };
    let kind_clause = match &c.answer_kind {
        Some(k) => format!("  - its type is exactly {k}\n"),
        None => String::new(),
    };
    Question::noul(format!(
        "QUESTION: \"{}\"\n{kind_line}CURRENT NODE:\n{}\n{position}\
\nIs the CURRENT NODE itself one of the answers the question asks for?\n\
Answer TRUE only when every one of these is satisfied:\n{kind_clause}\
  - it is the sort of thing the question is about. The class may be BROADER than\n\
    the question: a class of products can hold entries that are not what the\n\
    question asks for, and such a node is FALSE however well it fits otherwise.\n\
  - every restriction in the question holds for THIS node. For example: \"non-lead\"\n\
    means the node's own job title must NOT contain Lead / Head / Manager /\n\
    Director / Chief; \"first name starting with X\" means its given name starts\n\
    with X. Read the node's own fields above to check each one.\n\
Answer FALSE when the node is the wrong type, when it is not the sort of thing the\n\
question is about, or when it breaks a restriction{}",
        c.prompt,
        w.full(node),
        if class_slot { ".\n" } else { ", or when it is merely a hub/waypoint whose neighbours contain the answers.\n" }
    ))
}

/// Does the question restrict the ANSWER ITSELF?
///
/// Asked once, and consulted only for a RELATIONAL answer slot. This exists
/// because of rule 4, which the goal noul was walking straight into: "which
/// people support <product>?" has no node-local content whatsoever — the people
/// who support a product are the staff of the org that supports it, and not one
/// of their own records mentions the product. Asked anyway, the goal noul
/// hedged at 0.43–0.65 on all four correct answers and the crawl returned one
/// of five. Both cheaper fixes were measured and failed: lowering the gate
/// cannot work (a genuine wrong-position node sits at 0.661 against 0.7), and
/// re-wording the goal noul made it worse.
///
/// **The instrument matters more than the wording.** Three were measured over
/// the same 13 graded questions: a Noul ("does this question restrict the
/// answer itself?") got 4 wrong with scores that were mush (TRUE 0.276–0.802,
/// FALSE 0.099–0.168, so any threshold is fitted to the sample); a two-way
/// Choice got 5 wrong and *confidently* wrong (p=0.88 on one); naming the SORTS
/// of restriction with an explicit `*none*` got 1 wrong. The winner has the
/// shape the `kind` Choice already uses, and for the same reason — the model is
/// comparing concrete recognisable alternatives rather than calibrating an
/// absolute probability.
pub fn filter_question(c: &Crawl) -> Question {
    let kind = c.answer_kind.clone().unwrap_or_else(|| "node".into());
    let mut criteria: BTreeMap<String, String> = BTreeMap::new();
    criteria.insert(
        FILTER_NONE.to_string(),
        format!("nothing about the answer itself -- the question only says it is a {kind} and what it relates to"),
    );
    criteria.insert(
        "title".into(),
        "a job title or role the answer must have, or must not have (non-lead, senior, director)".into(),
    );
    criteria.insert(
        "name".into(),
        "something about the answer's own name (starts with a letter, contains a word)".into(),
    );
    criteria.insert(
        "status".into(),
        "a status, lifecycle or flag the answer must have (active, sunset, incubation)".into(),
    );
    criteria.insert(
        "sort".into(),
        format!("what SORT of {kind} it must be, where that class also holds other sorts"),
    );
    Question::choice(
        "filter",
        criteria,
        format!(
            "QUESTION: \"{}\"\n\
The answer is a {kind} node.\n\
\nThe question says what the answer relates to. Does it ALSO say something about the answer\n\
ITSELF -- something you would check by reading that node's own record? Pick which sort of\n\
thing it says, or {FILTER_NONE} if it says nothing about the answer itself at all.\n\
\nPick exactly one:",
            c.prompt
        ),
    )
}

pub fn rank_question(c: &Crawl, w: &World) -> Question {
    let criteria: BTreeMap<String, String> =
        c.collected.iter().map(|s| (s.clone(), w.rank_label(s))).collect();
    Question::choice(
        "rank",
        criteria,
        format!(
            "QUESTION: \"{}\"\n\n\
The question asks for a SINGLE best one, and these {} are the complete set it\n\
has to be the best of -- every one of them already qualifies, so you are not deciding\n\
whether they belong. Read each node's fields and pick the one the question would single\n\
out. Where the question names a quantity (health, damage, speed, cost), compare that\n\
quantity: the fields are quoted verbatim from the source, so a value may be prose rather\n\
than a bare number -- read the whole of it and compare the totals it describes.\n\n\
Pick exactly one:",
            c.prompt,
            c.collected.len()
        ),
    )
}

/// The state payload the slot sees alongside the questions.
///
/// NAVIGATION wants the crawl's context. A GOAL judgement does not — it is a
/// self-contained question about one node, whose content is already spelled out
/// in its own instructions. Feeding it the accumulating crawl state makes an
/// answer that should be reproducible drift with how far the crawl has got:
/// `Product/f-command` scores 0.856 judged on its own and 0.566 judged on turn
/// 36 of an enumeration with eighteen slugs in the payload — the same node, the
/// same question, on opposite sides of the threshold.
pub fn state_view(c: &Crawl, names: &[String]) -> String {
    let judgement_only = !names.is_empty() && names.iter().all(|n| n == "goal" || n == "filter");
    if judgement_only {
        return serde_json::json!({ "question": c.prompt }).to_string();
    }
    let tail = c.seen.len().saturating_sub(8);
    serde_json::json!({
        "question": c.prompt,
        "cursor": c.cursor,
        "collected": c.collected,
        "seen": &c.seen[tail..],
        "searchTerms": c.search_terms,
    })
    .to_string()
}

// ─── the behaviour tree ─────────────────────────────────────────────────────

/// BT status codes, matching the author's C implementation:
/// `F` failure (this branch declines, fall through), `R` running (needs a typed
/// answer before it can resolve), `T` success (this branch produced the action).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Bt {
    F,
    R,
    T,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchTrace {
    pub branch: &'static str,
    pub conds: Vec<(&'static str, bool)>,
    pub status: Option<Bt>,
}

/// Per-turn scratch, keyed by question name. A condition that needs a model
/// answer asks for it here: on the first pass the answer is absent, so the need
/// is recorded and the tree yields `Pending`. That is what makes the question
/// set a CONSEQUENCE of the active branch, instead of a second cascade of ifs
/// that has to be kept in agreement with the first.
#[derive(Default)]
pub struct Blackboard {
    pub answers: BTreeMap<String, Option<Answer>>,
    pub needs: BTreeMap<String, Question>,
    wanted: Vec<String>,
    pub asked: Vec<String>,
    pub free_nav: usize,
    pub walk: Vec<BranchTrace>,
}

impl Blackboard {
    fn ask(&mut self, name: &str, build: impl FnOnce() -> Question) -> Option<Option<Answer>> {
        if let Some(a) = self.answers.get(name) {
            return Some(a.clone());
        }
        if !self.needs.contains_key(name) {
            self.needs.insert(name.to_string(), build());
            self.asked.push(name.to_string());
        }
        None
    }
    fn take_needs(&mut self) -> BTreeMap<String, Question> {
        let n = std::mem::take(&mut self.needs);
        self.wanted = n.keys().cloned().collect();
        n
    }
    /// Bind every question we asked, even if the slot answered none of them, so
    /// a silent slot cannot make the tree ask the same thing forever.
    fn bind(&mut self, got: &BTreeMap<String, Answer>) {
        for k in std::mem::take(&mut self.wanted) {
            let v = got.get(&k).cloned();
            self.answers.insert(k, v);
        }
    }
}

/// Everything the tree reads that needs a round trip to compute, gathered once
/// per turn so the tree itself stays pure and synchronous.
pub struct Turn {
    pub front: Vec<String>,
    pub orient: Options,
    pub seek: Options,
    pub enumerate: Options,
    pub recover: Options,
}

impl Turn {
    fn options(&self, stage: Stage) -> &Options {
        match stage {
            Stage::Orient => &self.orient,
            Stage::Enumerate => &self.enumerate,
            Stage::Recover => &self.recover,
            _ => &self.seek,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Tick {
    Pending,
    Act { action: String, stage: Stage, why: String },
}

fn act(stage: Stage, action: impl Into<String>, why: impl Into<String>) -> Tick {
    Tick::Act { action: action.into(), stage, why: why.into() }
}

// ── the named conditions ──
fn right_kind(c: &Crawl) -> bool {
    match (&c.answer_kind, c.cursor.as_deref()) {
        (None, _) => true,
        (Some(k), Some(cur)) => class_of(cur) == k,
        (Some(_), None) => false,
    }
}
fn uncollected(c: &Crawl) -> bool {
    match c.cursor.as_deref() {
        Some(cur) => !c.collected.iter().any(|x| x == cur),
        None => false,
    }
}
fn have_answers(c: &Crawl) -> bool {
    !c.collected.is_empty()
}
fn slot_open(c: &Crawl, t: &Turn) -> bool {
    c.anchor.is_some() && !t.front.is_empty()
}
fn slot_exhausted(c: &Crawl, t: &Turn) -> bool {
    c.anchor.is_some() && t.front.is_empty()
}
fn has_forward_edge(t: &Turn) -> bool {
    !t.seek.is_empty()
}
/// The chain has genuinely run out here: nothing forward to step onto and no
/// slot left to judge. Without this, RECOVER's precondition was merely "some
/// seen node has an unvisited edge", which is true almost always — so RECOVER
/// was actionable on 100% of turns and was held back only by sitting last in
/// the fallback order. That made the hand-written priority load-bearing for the
/// wrong reason, and hid how rarely the branches are genuinely contested.
fn chain_exhausted(c: &Crawl, t: &Turn) -> bool {
    !has_forward_edge(t) && !slot_open(c, t)
}


/// One tick. Returns exactly one action, or `Pending` when it needs a typed
/// answer first.
pub fn bt_tick(c: &mut Crawl, w: &World, t: &Turn, bb: &mut Blackboard) -> Tick {
    bb.walk.clear();

    macro_rules! seq {
        ($branch:expr, [ $( ($name:expr, $val:expr) ),* $(,)? ]) => {{
            let mut vals: Vec<(&'static str, bool)> = Vec::new();
            let mut ok = true;
            $(
                if ok {
                    let v: bool = $val;
                    vals.push(($name, v));
                    if !v { ok = false; }
                }
            )*
            if ok {
                bb.walk.push(BranchTrace { branch: $branch, conds: vals, status: None });
                true
            } else {
                bb.walk.push(BranchTrace { branch: $branch, conds: vals, status: Some(Bt::F) });
                false
            }
        }};
    }
    macro_rules! resolve {
        ($st:expr) => {{
            if let Some(last) = bb.walk.last_mut() {
                last.status = Some($st);
            }
        }};
    }

    // A menu with ONE option is a question with no wrong answer — rule 4 in its
    // plainest form. The model cannot pick anything else, so asking only buys
    // an acknowledgement: measured at p=1.000 on all three such turns of a
    // chain with no branching in it at all. Which option wins is identical
    // either way, so this changes no decision; it just stops paying ~630 tokens
    // and a round trip to be told the only thing that could be said.
    enum Nav {
        Pending,
        Pick(String),
        None,
    }
    fn nav_for(c: &Crawl, w: &World, t: &Turn, bb: &mut Blackboard, stage: Stage) -> Nav {
        let opts = t.options(stage);
        if opts.is_empty() {
            return Nav::None;
        }
        if opts.len() == 1 {
            bb.free_nav += 1;
            return Nav::Pick(opts.items[0].id.clone());
        }
        match bb.ask("action", || nav_choice(c, w, opts, stage)) {
            None => Nav::Pending,
            Some(ans) => match ans.and_then(|a| a.choice) {
                Some(ch) if opts.items.iter().any(|o| o.id == ch) => Nav::Pick(ch),
                _ => Nav::None,
            },
        }
    }

    // ? Fallback (root)

    // -> Sequence: COMMIT — I am standing on something that may answer the question
    if seq!(
        "COMMIT",
        [
            ("cursor", c.cursor.is_some()),
            ("onChain", c.on_chain(None)),
            ("rightKind", right_kind(c)),
            ("uncollected", uncollected(c)),
            ("unjudged", !c.rejected(None)),
        ]
    ) {
        let cur = c.cursor.clone().unwrap();
        // FACT MODE, standing on the very thing the question names: its record
        // IS the answer, and asking is rule 4 again. Measured on "whats
        // mandrake soup", where the crawl jumps straight to the soup and the
        // fact noul then refuses it at 0.556 — and worse, ranks the plain
        // Mandrake ABOVE it at 0.651, so no threshold separates them.
        //
        // NOT for a superlative. There the named thing is the SUBJECT and the
        // answer is something else — "best use of mandrake for health" names
        // the mandrake and is answered by what is made from it — so collecting
        // the named node would confidently answer with the wrong thing.
        if c.fact_mode && c.is_question_object(None) && !c.wants_single_best() {
            resolve!(Bt::T);
            return act(
                Stage::Commit,
                "collect",
                "this is the thing the question names; its own record is the answer",
            );
        }
        // The cursor got here along the very relation the question asked about,
        // and the question says nothing about the node itself: then standing
        // here IS the answer and there is no question left to ask. Rule 4
        // again, and the measurement that forced it is stark — asked "is this
        // one of the things beefalo eat?", the goal noul scores Cut_Grass 0.884
        // but Twigs 0.137, because it is judging whether a twig looks edible
        // rather than whether the EATS edge exists. The edge exists; that is
        // not a judgement.
        let on_question_rel = c.answer_kind.is_some()
            && !c.question_rels.is_empty()
            && c.via_rel(None).map(|r| c.question_rels.iter().any(|q| q == r)).unwrap_or(false);
        if on_question_rel {
            if c.has_node_filter.is_none() {
                match bb.ask("filter", || filter_question(c)) {
                    None => {
                        resolve!(Bt::R);
                        return Tick::Pending;
                    }
                    Some(ans) => {
                        let ch = ans.and_then(|a| a.choice);
                        c.has_node_filter = Some(ch.as_deref().map(|x| x != FILTER_NONE).unwrap_or(true));
                    }
                }
            }
            if c.has_node_filter == Some(false) {
                resolve!(Bt::T);
                let rel = c.via_rel(None).unwrap_or("").to_string();
                return act(
                    Stage::Commit,
                    "collect",
                    format!(
                        "reached by {rel}, the relation the question asks about; \
the question restricts nothing about the node"
                    ),
                );
            }
        }
        match bb.ask("goal", || goal_question(c, w, &cur)) {
            None => {
                resolve!(Bt::R);
                return Tick::Pending;
            }
            Some(ans) => {
                let noul = ans.and_then(|a| a.noul);
                if let Some(n) = noul {
                    if n >= GOAL_T {
                        resolve!(Bt::T);
                        return act(Stage::Commit, "collect", format!("goal noul {n:.3} >= {GOAL_T}"));
                    }
                }
                // Judged and ruled out: record it so the same node is never
                // re-judged, which is also what stops the tree re-asking this
                // noul every turn.
                resolve!(Bt::F);
                c.reject(&cur, noul);
            }
        }
    }

    // -> Sequence: RANK — the question asked for the BEST one, and the set it
    //    has to be the best OF is now complete. Enumerate first, then narrow: a
    //    superlative over a set the crawl has not finished collecting would be
    //    picking the best of whatever it happened to have. Sits ahead of FINISH
    //    so it runs while the slot is exhausted, and fires at most once.
    if seq!(
        "RANK",
        [
            ("haveAnswers", have_answers(c)),
            ("slotExhausted", slot_exhausted(c, t)),
            ("superlative", c.wants_single_best()),
            ("notRanked", !c.ranked),
            ("severalAnswers", c.collected.len() > 1),
        ]
    ) {
        match bb.ask("rank", || rank_question(c, w)) {
            None => {
                resolve!(Bt::R);
                return Tick::Pending;
            }
            Some(ans) => {
                let n = c.collected.len();
                match ans.and_then(|a| a.choice) {
                    Some(ch) if c.collected.iter().any(|x| *x == ch) => {
                        resolve!(Bt::T);
                        return act(
                            Stage::Rank,
                            format!("rank:{ch}"),
                            format!("the question asks for one best of {n}"),
                        );
                    }
                    // No usable pick: fall through and return the whole set
                    // rather than nothing.
                    _ => {
                        resolve!(Bt::F);
                        c.ranked = true;
                    }
                }
            }
        }
    }

    // -> Sequence: FINISH — the answer slot is fully enumerated. Not a
    //    judgement: every candidate in the slot has been visited and ruled on.
    if seq!(
        "FINISH",
        [("haveAnswers", have_answers(c)), ("slotExhausted", slot_exhausted(c, t))]
    ) {
        resolve!(Bt::T);
        return act(Stage::Finish, "stop", "answer slot fully enumerated");
    }

    // -> Sequence: ENUMERATE — judge the rest of the slot, IN PLACE.
    //    The candidates are interchangeable and every one of them has to be
    //    ruled on, so "which next" is not a decision — the engine iterates and
    //    only the judgement is asked. Measured before this: action@enumerate
    //    was 30 of 80 slot calls and 55% of all input tokens, spent choosing
    //    between options with no wrong answer.
    if seq!("ENUMERATE", [("slotOpen", slot_open(c, t))]) {
        let next = t.front[0].clone();
        // RANK MODE gathers; it does not judge. Each member is a candidate for
        // one relative decision later, so thresholding them here is the very
        // thing that cannot work. Bounded by one hop from the hub.
        if matches!(c.anchor, Some(Anchor::RankHub { .. })) {
            resolve!(Bt::T);
            return act(
                Stage::Enumerate,
                format!("collect:{next}"),
                "gathered as a candidate; a superlative is decided by ranking, not by a gate",
            );
        }
        let class_slot = matches!(c.anchor, Some(Anchor::Class { .. }));
        // A RELATIONAL slot, and a question that restricts nothing about the
        // node itself: membership of the slot IS the answer. A CLASS slot
        // always asks — every instance of the kind is a candidate there, so
        // position carries no information at all and the goal noul is the only
        // filter in existence.
        //
        // SAFETY: collecting a slot without judging it is only sound while the
        // slot is structurally bounded, and it is bounded by TWO things — the
        // hub + relation + direction, and the answer kind. With the kind
        // unresolved the second bound is gone: the frontier stops filtering by
        // class, extension keeps widening through each new answer, and nothing
        // asks a question. Measured the hard way: 160 steps and 150-odd nodes
        // collected, most of the graph, instead of three.
        if !class_slot && c.answer_kind.is_some() && c.has_node_filter.is_none() {
            match bb.ask("filter", || filter_question(c)) {
                None => {
                    resolve!(Bt::R);
                    return Tick::Pending;
                }
                Some(ans) => {
                    let ch = ans.and_then(|a| a.choice);
                    c.has_node_filter = Some(ch.as_deref().map(|x| x != FILTER_NONE).unwrap_or(true));
                }
            }
        }
        if !class_slot && c.answer_kind.is_some() && c.has_node_filter == Some(false) {
            resolve!(Bt::T);
            return act(
                Stage::Enumerate,
                format!("collect:{next}"),
                "in the answer slot; the question restricts nothing about the node itself",
            );
        }
        match bb.ask("goal", || goal_question(c, w, &next)) {
            None => {
                resolve!(Bt::R);
                return Tick::Pending;
            }
            Some(ans) => {
                let noul = ans.and_then(|a| a.noul);
                resolve!(Bt::T);
                if let Some(n) = noul {
                    if n >= GOAL_T {
                        return act(
                            Stage::Enumerate,
                            format!("collect:{next}"),
                            format!("slot candidate, goal noul {n:.3} >= {GOAL_T}"),
                        );
                    }
                }
                c.reject(&next, noul);
                return act(
                    Stage::Enumerate,
                    format!("skip:{next}"),
                    format!(
                        "slot candidate ruled out, goal noul {} < {GOAL_T}",
                        match noul {
                            None => "n/a".to_string(),
                            Some(n) => format!("{n:.3}"),
                        }
                    ),
                );
            }
        }
    }

    // -> Sequence: SEEK — walk the chain toward a node of the answer kind
    if seq!(
        "SEEK",
        [("onChain", c.on_chain(None)), ("hasForwardEdge", has_forward_edge(t))]
    ) {
        match nav_for(c, w, t, bb, Stage::Seek) {
            Nav::Pending => {
                resolve!(Bt::R);
                return Tick::Pending;
            }
            Nav::Pick(p) => {
                resolve!(Bt::T);
                let why = if t.seek.len() == 1 {
                    "forward along the chain -- the only edge out, no call"
                } else {
                    "forward along the chain"
                };
                return act(Stage::Seek, p, why);
            }
            Nav::None => resolve!(Bt::F),
        }
    }

    // -> Sequence: ORIENT — find the object the question is about
    if seq!("ORIENT", [("noChain", !c.on_chain_any())]) {
        match nav_for(c, w, t, bb, Stage::Orient) {
            Nav::Pending => {
                resolve!(Bt::R);
                return Tick::Pending;
            }
            Nav::Pick(p) => {
                resolve!(Bt::T);
                return act(Stage::Orient, p, "entering at the question's object");
            }
            Nav::None => resolve!(Bt::F),
        }
    }

    // ? Fallback: RECOVER — the chain ran out; go back or widen the net
    if seq!(
        "RECOVER",
        [
            ("chainExhausted", chain_exhausted(c, t)),
            ("hasRecovery", !t.recover.is_empty())
        ]
    ) {
        match nav_for(c, w, t, bb, Stage::Recover) {
            Nav::Pending => {
                resolve!(Bt::R);
                return Tick::Pending;
            }
            Nav::Pick(p) => {
                resolve!(Bt::T);
                return act(Stage::Recover, p, "chain exhausted here");
            }
            Nav::None => resolve!(Bt::F),
        }
    }

    // right-most leaf, no recovery
    let got = have_answers(c);
    bb.walk.push(BranchTrace {
        branch: if got { "FINISH*" } else { "GIVE_UP" },
        conds: vec![("haveAnswers", got)],
        status: Some(Bt::T),
    });
    if got {
        act(Stage::Finish, "stop", "nothing left to try")
    } else {
        act(Stage::Finish, "give_up", "nothing reachable answers the question")
    }
}

// ─── the runner ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub step: usize,
    pub cursor: Option<String>,
    pub stage: &'static str,
    pub action: String,
    pub arg: Option<String>,
    pub why: String,
    pub note: String,
    pub refused: Option<String>,
    pub slot_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrawlResult {
    pub question: String,
    pub answer: Option<String>,
    pub collected: Vec<String>,
    pub labels: Vec<String>,
    pub kind: Option<String>,
    pub mode: &'static str,
    pub steps: Vec<Step>,
    pub path: Vec<String>,
    pub ended: &'static str,
    pub stall: Option<String>,
    pub calls: u64,
    pub tokens: u64,
    pub ms: u128,
}

pub struct CrawlOpts {
    pub start: Option<String>,
    pub kind: Option<String>,
    pub finalize: bool,
    pub trace: bool,
    pub verbose: bool,
    pub max_steps: usize,
}

impl Default for CrawlOpts {
    fn default() -> Self {
        CrawlOpts {
            start: None,
            kind: None,
            finalize: true,
            trace: false,
            verbose: false,
            max_steps: MAX_STEPS,
        }
    }
}

/// Resolve the answer's kind ONCE, before the first turn.
///
/// 1. schema fast path — 0 ms, no call, no domain vocabulary
/// 2. the kind Choice — one small call, closed to the real classes
/// 3. the T-box — a relation the question names declares what is at each end
/// 4. unresolved / `*none*` — fact mode, or rank mode for a superlative
///
/// Resolved up front rather than multiplexed into turn 0 because the slot's
/// latency grows with total prompt tokens, so folding it into the navigation
/// prompt costs more than asking it on its own (216 ms vs 115+41).
async fn resolve_kind(
    c: &mut Crawl,
    w: &mut World<'_>,
    slot: &Slot,
    log: &mut (dyn FnMut(&str) + Send),
) -> Result<()> {
    if c.answer_kind.is_some() {
        return Ok(()); // caller override / replay
    }
    if let Some(fast) = answer_kind_fast(&c.prompt, &w.classes) {
        log(&format!("  kind: {fast}  (schema fast path, 0 calls)"));
        c.answer_kind = Some(fast);
        return Ok(());
    }
    let examples = w.class_examples().await?;
    let nclasses = examples.len();
    let mut qs = BTreeMap::new();
    qs.insert("kind".to_string(), kind_question(&c.prompt, &examples));
    let state = serde_json::json!({ "question": c.prompt }).to_string();
    let got = slot.ask(&state, &qs).await.unwrap_or_default();
    let ans = got.get("kind").cloned().unwrap_or_default();
    let choice = ans.choice.clone();
    let p = ans.p;
    if let Some(k) = choice.clone() {
        if k != KIND_NONE && w.classes.contains(&k) {
            log(&format!(
                "  kind: {k}  (model Choice over {nclasses} classes{})",
                match p {
                    Some(p) => format!(", p={p:.2}"),
                    None => String::new(),
                }
            ));
            c.answer_kind = Some(k);
            return Ok(());
        }
    }
    // The Choice abstained. Before giving up, read the kind off the T-box.
    // An unresolved kind is not a soft failure — out-edges are only offered
    // when the target IS the answer kind, so without one the crawl cannot
    // traverse the relation at all.
    if let Some((kind, rels, _obj)) = w.relation_kind(&c.prompt).await? {
        log(&format!(
            "  kind: {kind}  (Choice abstained; derived from {}, declared in the schema, 0 calls)",
            rels.join("/")
        ));
        c.question_rels = rels;
        c.answer_kind = Some(kind);
        return Ok(());
    }
    let pstr = match p {
        Some(p) => format!("{p:.2}"),
        None => "?".into(),
    };
    if choice.as_deref() == Some(KIND_NONE) && c.wants_single_best() {
        c.rank_mode = true;
        log(&format!(
            "  kind: *none* (p={pstr}) but the question asks for the BEST one, so there IS a set \
-- gathering the candidates around wherever the crawl settles and ranking them relatively"
        ));
        return Ok(());
    }
    if choice.as_deref() == Some(KIND_NONE) {
        c.fact_mode = true;
        log(&format!(
            "  kind: *none* (p={pstr}) -- the answer is a FACT read off a node, not a node set; \
the crawl will walk until it finds the node whose own record holds it, collect that, and stop"
        ));
        return Ok(());
    }
    log("  kind: unresolved (no choice returned) -- the kind filter will not apply");
    Ok(())
}

/// Gather everything the tree reads, once per turn.
async fn build_turn(c: &mut Crawl, w: &mut World<'_>) -> Result<Turn> {
    if let Some(cur) = c.cursor.clone() {
        w.ensure(&cur).await?;
    }
    // the anchor's hub, and the class members when the slot is a class
    let mut class_members: Vec<String> = Vec::new();
    match c.anchor.clone() {
        Some(Anchor::Relational { ref hub, .. }) | Some(Anchor::RankHub { ref hub }) => {
            w.ensure(hub).await?;
        }
        Some(Anchor::Class { ref cls }) => {
            class_members = w.instances_of(cls, (MAX_CLASS_ENUM * 4) as i64).await?;
        }
        _ => {}
    }
    // answers already collected need their edges for slot extension
    for got in c.collected.clone() {
        w.ensure(&got).await?;
    }
    let front = frontier(c, w, &class_members);
    let front_total = match &c.anchor {
        Some(Anchor::Class { cls }) => {
            let n = w.count_of_class(cls).await as usize;
            n.saturating_sub(c.collected.len() + c.rejected_at.len())
        }
        _ => front.len(),
    };

    // every node that might appear on a menu needs its record, for the
    // descriptive bits a choice criterion is判断 on
    let mut need: Vec<String> = front.clone();
    if let Some(cur) = c.cursor.as_deref() {
        need.extend(w.out(cur).iter().map(|(_, t)| t.clone()));
        need.extend(w.inn(cur).iter().map(|(_, f)| f.clone()));
    }
    for q in &c.question_objs {
        need.push(q.clone());
    }
    for s in &c.shortlist {
        need.push(s.clone());
    }
    let tail = c.seen.len().saturating_sub(40);
    need.extend(c.seen[tail..].iter().cloned());
    need.sort();
    need.dedup();
    need.truncate(400);
    w.ensure_rows(&need).await?;

    // search options: a term is only offered when it surfaces something new
    let mut searches: HashMap<String, Vec<String>> = HashMap::new();
    let start = c.search_terms.len().saturating_sub(20);
    for term in c.search_terms[start..].to_vec() {
        if c.searched.contains(&term) {
            continue;
        }
        searches.insert(term.clone(), w.search(&term).await?);
    }

    let orient = orient_options(c, w, &searches);
    let seek = forward_moves(c, w);
    let enumerate = slot_candidates(c, w, &front, front_total);
    let recover = recovery_options(c, w, &searches);
    Ok(Turn { front, orient, seek, enumerate, recover })
}

/// Tick the tree; whenever it needs a typed answer, fetch exactly that and tick
/// again. Each need is bound once per turn, so this terminates.
async fn decide(
    c: &mut Crawl,
    w: &World<'_>,
    t: &Turn,
    slot: &Slot,
    verbose: bool,
) -> Result<(String, Stage, String, u128, Vec<BranchTrace>)> {
    let mut bb = Blackboard::default();
    let mut slot_ms = 0u128;
    for _ in 0..6 {
        match bt_tick(c, w, t, &mut bb) {
            Tick::Act { action, stage, why } => {
                c.bt_latch(stage);
                return Ok((action, stage, why, slot_ms, std::mem::take(&mut bb.walk)));
            }
            Tick::Pending => {}
        }
        let needs = bb.take_needs();
        if needs.is_empty() {
            break;
        }
        let names: Vec<String> = needs.keys().cloned().collect();
        let state = state_view(c, &names);
        let t0 = std::time::Instant::now();
        let got = slot.ask(&state, &needs).await.unwrap_or_default();
        slot_ms += t0.elapsed().as_millis();
        if verbose {
            for (k, a) in &got {
                eprintln!("  [{k}] {}", a.fmt());
            }
        }
        bb.bind(&got);
    }
    c.bt_latch(Stage::Finish);
    Ok((
        "stop".into(),
        Stage::Finish,
        "decision did not resolve".into(),
        slot_ms,
        std::mem::take(&mut bb.walk),
    ))
}

// ─── the finalizer ──────────────────────────────────────────────────────────
//
// The crawler navigates and stops; a chat model phrases the answer. Two things
// make the difference between an answer and a report:
//
//  * **By default the trace is not sent at all.** A model handed a step-by-step
//    log of an agent's work will describe it, however firmly the system prompt
//    asks it not to. Without the log there is nothing to narrate, and the
//    answer is all that is left to write.
//  * **The headers are neutral.** An earlier draft said "CONTENT OF THE
//    COLLECTED NODE(S) -- verbatim from the graph", and the answers came back
//    saying "the record notes that …". Naming the containers in the prompt is
//    what teaches the model to name them back.
//
// There is no length rule. A first draft capped it at "usually one sentence"
// and that cap immediately cost completeness — "what do beefalo eat?" came back
// "Beefalo can eat Cut Grass", dropping Twigs, which the crawl had collected.

const SYSTEM_ANSWER: &str = "\
You know a set of facts about a subject, and you are answering a question about it. The facts \
you are given are true and complete for this purpose: they were looked up for you, and what the \
lookup returned IS the answer. You are not re-deciding it. Describe and quote freely, but never \
argue that something returned does not really qualify -- a detail written for one purpose often \
reads oddly out of context (\"non-survivor food\" marks what a player cannot eat, not what an \
animal cannot). \
Rules: \
1) ANSWER THE QUESTION, as someone who simply knows the answer. Do not analyse, summarise or \
describe what you were given, how it is organised, or how the answer was arrived at. Never refer \
to a lookup, a search, a crawl, a trace, steps, data, a record, an entry, a node, a slug, a \
class, a field, a property or an \"answer set\". The reader is asking about the subject itself \
and does not know any of that exists. Say \"Wurt cannot eat meat\", never \"the ability field \
says Wurt cannot eat meat\"; say \"Cut Grass is harvested from Grass Tufts\", never \"the entry \
notes that Cut Grass is harvested from Grass Tufts\". \
2) NAME EVERY ITEM YOU WERE GIVEN AS AN ANSWER. If there are twenty, all twenty appear. A partial \
list is a wrong answer. \
3) A yes/no question gets yes or no first, then the reason. \
4) Quote wording directly where that wording IS the answer, attributing it to the thing it is \
about. \
5) If what you know does not answer the question, say so plainly and say what you do know about \
the subject. Never invent a fact. \
Say as much as the question needs -- length is not the point; not talking about the material or \
the process is the point.";

const SYSTEM_TRACE: &str = "\
You answer questions about a knowledge graph (entities and the relations between them). A crawler \
agent explored the graph for you and produced a trace. Read the trace and answer the question, \
grounded in it. Rules: 1) If the trace supports a specific node or edge, lead with it and cite the \
evidence briefly (name the node and the relation). 2) If the trace shows no answer (give_up / no \
connecting edge) or the evidence is insufficient, say so plainly and describe what you DID find \
(nearest relevant node, what is missing). Do NOT invent a node or edge that is not in the trace. \
3) Concise: 2-5 sentences. No preamble.";

/// The content of the nodes the crawl committed, on a character budget: a
/// one-node answer gets its whole record, a twenty-node enumeration gets as
/// many as fit and a count of the rest (there the slug list IS the answer, so
/// the content is context rather than the point).
fn collected_content(c: &Crawl, header: &str) -> String {
    if c.read.is_empty() {
        return String::new();
    }
    let mut out: Vec<&String> = Vec::new();
    let mut used = 0usize;
    let mut omitted = 0usize;
    for b in &c.read {
        if used + b.len() > FINALIZER_CONTENT_BUDGET && !out.is_empty() {
            omitted += 1;
            continue;
        }
        used += b.len();
        out.push(b);
    }
    format!(
        "\n\n{header}\n{}{}",
        out.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n"),
        if omitted > 0 {
            format!("\n\n({omitted} more, already named above)")
        } else {
            String::new()
        }
    )
}

fn render_steps(steps: &[Step]) -> String {
    let all: Vec<String> = steps
        .iter()
        .map(|s| {
            format!(
                "[step {}]  cursor={}  stage={}  action={}{}{}",
                s.step,
                s.cursor.clone().unwrap_or_else(|| "—".into()),
                s.stage,
                s.action,
                if s.note.is_empty() { String::new() } else { format!("\n  {}", s.note) },
                match &s.refused {
                    Some(r) => format!("\n  refused: {r}"),
                    None => String::new(),
                }
            )
        })
        .collect();
    let full = all.join("\n");
    if full.len() <= 4200 {
        return full;
    }
    let head = &all[..3.min(all.len())];
    let tail = &all[all.len().saturating_sub(15)..];
    let mid = all.len().saturating_sub(head.len() + tail.len());
    let mut v: Vec<String> = head.to_vec();
    v.push(format!("… (middle {mid} steps omitted) …"));
    v.extend(tail.iter().cloned());
    v.join("\n")
}

async fn finalize(
    c: &Crawl,
    w: &World<'_>,
    chat: &Chat,
    steps: &[Step],
    trace: bool,
) -> Result<String> {
    let ended = if c.done { "stopped" } else { "no answer" };
    let user = if trace {
        format!(
            "QUESTION: \"{}\"\n\n\
CRAWL TRACE (one line per action, decision model)\n{}\
\n\nOUTCOME: {ended}  path: {}  collected answer set: {}  final cursor: {}{}\
\n\nNow answer the QUESTION. Ground every claim in the trace and in the node content above -- \
when the answer is a field of a node, quote that field. If the material does not support a \
definite answer, say so plainly.\n",
            c.prompt,
            render_steps(steps),
            if c.visited.is_empty() { "none".to_string() } else { c.visited.join(" -> ") },
            if c.collected.is_empty() { "(none)".to_string() } else { c.collected.join(" ; ") },
            c.cursor.clone().unwrap_or_else(|| "none".into()),
            collected_content(c, "CONTENT OF THE COLLECTED NODE(S):")
        )
    } else {
        format!(
            "QUESTION: \"{}\"\n\n{}{}\n\nAnswer the question.\n",
            c.prompt,
            if c.collected.is_empty() {
                "NOTHING FOUND answers this question.\n".to_string()
            } else {
                format!(
                    "THE ANSWER IS: {}\n",
                    c.collected.iter().map(|s| w.label(s)).collect::<Vec<_>>().join(", ")
                )
            },
            collected_content(c, "WHAT IS KNOWN ABOUT EACH:")
        )
    };
    let sys = if trace { SYSTEM_TRACE } else { SYSTEM_ANSWER };
    chat.complete(&[ChatMessage::system(sys), ChatMessage::user(user)], 0.2).await
}

/// Answer `question` by walking the graph.
pub async fn run(
    store: &Store,
    tbox: &TBox,
    slot: &Slot,
    chat: Option<&Chat>,
    embedder: Option<&crate::llm::Embedder>,
    question: &str,
    opts: &CrawlOpts,
) -> Result<CrawlResult> {
    let t0 = std::time::Instant::now();
    let mut log = |m: &str| {
        if opts.verbose {
            eprintln!("{m}");
        }
    };
    let mut w = World::new(store, tbox).await?;

    // A hybrid-search shortlist, shown as context to orient the model to the
    // target. Deliberately NOT seeded into `seen` — wrong-kind candidates add
    // doubt, and a seeded node becomes a tempting "jump home" escape.
    let shortlist: Vec<String> = super::search::run(
        store,
        embedder,
        question,
        &super::search::SearchOpts { limit: 10, expand: false, ..Default::default() },
    )
    .await
    .map(|r| r.hits.into_iter().map(|h| h.slug).collect())
    .unwrap_or_default();

    let question_objs = w.question_objects(question).await?;
    let start = match &opts.start {
        Some(s) => store.resolve(s).await?,
        None => None,
    };
    if let Some(s) = &start {
        w.ensure(s).await?;
    }
    let mut c = Crawl::new(question, start, shortlist, question_objs);
    if let Some(k) = &opts.kind {
        if w.classes.contains(k) {
            c.answer_kind = Some(k.clone());
        }
    }
    resolve_kind(&mut c, &mut w, slot, &mut log).await?;

    let mut steps: Vec<Step> = Vec::new();
    for i in 0..opts.max_steps {
        if c.done {
            break;
        }
        let turn = build_turn(&mut c, &mut w).await?;
        let (action, stage, why, slot_ms, walk) = decide(&mut c, &w, &turn, slot, opts.verbose).await?;
        if opts.verbose {
            let st = walk
                .iter()
                .map(|b| {
                    format!(
                        "{}{}",
                        b.branch,
                        match b.status {
                            Some(Bt::T) => "✓",
                            Some(Bt::F) => "✗",
                            Some(Bt::R) => "…",
                            None => "?",
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("step {:>3}  {stage:?}  {action}   [{st}]", i + 1);
        }
        let prev = c.cursor.clone();
        let res = apply(&mut c, &w, &action, &turn.front);
        if res.refused.is_some() {
            c.no_progress += 1;
            c.stall = Some(format!("refused action: {action}"));
        } else {
            c.no_progress = 0;
        }
        if c.no_progress >= 3 && !c.done {
            c.done = true;
            c.stall = Some(format!(
                "gave up: repeated non-progress action '{action}' without moving (cursor {})",
                c.cursor.clone().unwrap_or_else(|| "none".into())
            ));
        }
        steps.push(Step {
            step: i + 1,
            cursor: c.cursor.clone().or(prev),
            stage: stage.as_str(),
            action: action.clone(),
            arg: res.arg.clone(),
            why,
            note: res.note.clone(),
            refused: res.refused.clone(),
            slot_ms,
        });
        // A `search:` action only widens what the next turn may offer, so make
        // its hits concrete the way the engine's own `seen` set expects.
        if let Some(term) = action.strip_prefix("search:") {
            let hits = w.search(term).await?;
            for h in hits.into_iter().take(25) {
                c.add_seen(&h);
            }
        }
        if res.refused.is_some() {
            log("  >> model picked an invalid action; re-presenting valid ones");
            continue;
        }
        if c.done {
            break;
        }
    }

    // ensure every answer's record is loaded for the labels and the finalizer
    let got = c.collected.clone();
    w.ensure_rows(&got).await?;
    let labels: Vec<String> = got.iter().map(|s| w.label(s)).collect();
    let (calls, tokens) = slot.counters();
    let mut answer = None;
    if opts.finalize {
        if let Some(chat) = chat {
            match finalize(&c, &w, chat, &steps, opts.trace).await {
                Ok(t) if !t.trim().is_empty() => answer = Some(t),
                Ok(_) => {}
                Err(e) => log(&format!("  finalizer failed: {e}")),
            }
        }
    }
    Ok(CrawlResult {
        question: question.to_string(),
        answer,
        collected: got,
        labels,
        kind: c.answer_kind.clone(),
        mode: if c.fact_mode {
            "fact"
        } else if c.rank_mode {
            "rank"
        } else {
            "set"
        },
        path: c.visited.clone(),
        ended: if c.done { "stopped" } else { "no_answer" },
        stall: c.stall.clone(),
        steps,
        calls,
        tokens,
        ms: t0.elapsed().as_millis(),
    })
}
