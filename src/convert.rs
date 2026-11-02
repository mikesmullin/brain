//! convert.rs — translate an es6 brain into brain2's format, and the two
//! LLM-assisted write paths (`ingest`, `enrich`).
//!
//! The es6 databases (`schema.mjs` + `data.mjs`) came first and are where the
//! T-box vocabulary was worked out. brain2 keeps the *ideas* and changes the
//! *medium*: a folder of Markdown with YAML frontmatter, one file per entity,
//! which a human can browse as a wiki and git can diff line by line.
//!
//! The A-box convention survives unchanged, because it is what makes a file
//! self-describing: **lowercase keys are components, ALL-CAPS keys are
//! relations**, and a relation target is either a `Class/id` string or a map
//! with `_to` plus that edge's qualifiers.
//!
//! The parser here is a small JSON5-ish reader rather than a JS engine: it
//! handles comments, unquoted keys, trailing commas, single quotes and `...`
//! spreads of top-level `const` objects, which is the whole of the dialect
//! these two files actually use. Nothing is evaluated, so converting a database
//! never runs its code.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::abox::{Edge, Entity};
use crate::cli::{Args, Global};
use crate::commands::Ctx;
use crate::config;
use crate::llm::ChatMessage;
use crate::slug;
use crate::tbox::{Cardinality, ClassDef, ComponentDef, FieldDef, RelationDef, TBox};

// ─── a JSON5-ish reader ─────────────────────────────────────────────────────

struct P<'a> {
    s: &'a [u8],
    i: usize,
    consts: &'a BTreeMap<String, Value>,
}

impl<'a> P<'a> {
    fn new(s: &'a str, consts: &'a BTreeMap<String, Value>) -> P<'a> {
        P { s: s.as_bytes(), i: 0, consts }
    }

    fn ws(&mut self) {
        loop {
            while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
                self.i += 1;
            }
            if self.i + 1 < self.s.len() && self.s[self.i] == b'/' && self.s[self.i + 1] == b'/' {
                while self.i < self.s.len() && self.s[self.i] != b'\n' {
                    self.i += 1;
                }
                continue;
            }
            if self.i + 1 < self.s.len() && self.s[self.i] == b'/' && self.s[self.i + 1] == b'*' {
                self.i += 2;
                while self.i + 1 < self.s.len() && !(self.s[self.i] == b'*' && self.s[self.i + 1] == b'/') {
                    self.i += 1;
                }
                self.i = (self.i + 2).min(self.s.len());
                continue;
            }
            return;
        }
    }

    fn peek(&mut self) -> u8 {
        self.ws();
        if self.i < self.s.len() {
            self.s[self.i]
        } else {
            0
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == c {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if self.eat(c) {
            Ok(())
        } else {
            bail!(
                "expected '{}' at byte {} (near: {})",
                c as char,
                self.i,
                String::from_utf8_lossy(&self.s[self.i.saturating_sub(30)..(self.i + 30).min(self.s.len())])
            )
        }
    }

    fn string(&mut self) -> Result<String> {
        let q = self.peek();
        if q != b'"' && q != b'\'' && q != b'`' {
            bail!("expected a string at byte {}", self.i);
        }
        self.i += 1;
        let mut out = String::new();
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c == b'\\' {
                self.i += 1;
                let e = *self.s.get(self.i).unwrap_or(&b'\\');
                out.push(match e {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'0' => '\0',
                    b'u' => {
                        let hex = std::str::from_utf8(&self.s[self.i + 1..(self.i + 5).min(self.s.len())])
                            .unwrap_or("0000");
                        self.i += 4;
                        char::from_u32(u32::from_str_radix(hex, 16).unwrap_or(0xfffd)).unwrap_or('\u{fffd}')
                    }
                    other => other as char,
                });
                self.i += 1;
                continue;
            }
            if c == q {
                self.i += 1;
                // decode the bytes we accumulated as UTF-8 by rebuilding
                return Ok(out);
            }
            // copy one whole UTF-8 scalar
            let len = utf8_len(c);
            let chunk = &self.s[self.i..(self.i + len).min(self.s.len())];
            out.push_str(&String::from_utf8_lossy(chunk));
            self.i += len;
        }
        bail!("unterminated string")
    }

    fn ident(&mut self) -> String {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                self.i += 1;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(&self.s[start..self.i]).to_string()
    }

    fn value(&mut self) -> Result<Value> {
        match self.peek() {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' | b'\'' | b'`' => Ok(Value::String(self.string()?)),
            0 => bail!("unexpected end of input"),
            c if c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_digit()
                        || self.s[self.i] == b'.'
                        || self.s[self.i] == b'e'
                        || self.s[self.i] == b'E'
                        || self.s[self.i] == b'-'
                        || self.s[self.i] == b'+')
                {
                    self.i += 1;
                }
                let t = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
                Ok(serde_json::from_str(&t).unwrap_or(Value::String(t)))
            }
            _ => {
                let w = self.ident();
                match w.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" | "undefined" => Ok(Value::Null),
                    "" => bail!("unparseable value at byte {}", self.i),
                    // a bare identifier: a reference to a top-level const
                    other => Ok(self.consts.get(other).cloned().unwrap_or(Value::String(other.to_string()))),
                }
            }
        }
    }

    fn array(&mut self) -> Result<Value> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        loop {
            if self.eat(b']') {
                return Ok(Value::Array(out));
            }
            out.push(self.value()?);
            if !self.eat(b',') && self.peek() != b']' {
                bail!("expected ',' or ']' at byte {}", self.i);
            }
        }
    }

    fn object(&mut self) -> Result<Value> {
        self.expect(b'{')?;
        let mut out = Map::new();
        loop {
            if self.eat(b'}') {
                return Ok(Value::Object(out));
            }
            // `...CONST` spread
            if self.peek() == b'.' {
                self.i += 1;
                self.expect(b'.')?;
                self.expect(b'.')?;
                let v = self.value()?;
                if let Value::Object(m) = v {
                    for (k, vv) in m {
                        out.insert(k, vv);
                    }
                }
                self.eat(b',');
                continue;
            }
            let key = match self.peek() {
                b'"' | b'\'' | b'`' => self.string()?,
                _ => self.ident(),
            };
            if key.is_empty() {
                bail!("expected a key at byte {}", self.i);
            }
            self.expect(b':')?;
            let v = self.value()?;
            out.insert(key, v);
            if !self.eat(b',') && self.peek() != b'}' {
                bail!("expected ',' or '}}' at byte {}", self.i);
            }
        }
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Top-level `const NAME = { … }` / `= [ … ]` declarations, so a `...SPREAD`
/// inside a later literal resolves without evaluating anything.
fn collect_consts(src: &str) -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = src[i..].find("const ") {
        let at = i + rel;
        // must start a line (allowing indentation)
        let line_start = src[..at].rfind('\n').map(|x| x + 1).unwrap_or(0);
        if !src[line_start..at].trim().is_empty() {
            i = at + 6;
            continue;
        }
        let mut j = at + 6;
        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
            j += 1;
        }
        let ns = j;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
            j += 1;
        }
        let name = src[ns..j].to_string();
        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'=' {
            j += 1;
            let empty = BTreeMap::new();
            let mut p = P::new(src, &empty);
            p.i = j;
            if matches!(p.peek(), b'{' | b'[') {
                if let Ok(v) = p.value() {
                    out.insert(name, v);
                    i = p.i;
                    continue;
                }
            }
        }
        i = at + 6;
    }
    out
}

/// `data.mjs` -> slug -> entity object.
pub fn parse_data_mjs(src: &str) -> Result<Map<String, Value>> {
    let consts = collect_consts(src);
    // `const entities = { … }` is the shape every es6 brain uses; fall back to
    // the first top-level object literal if it is spelled some other way.
    if let Some(Value::Object(m)) = consts.get("entities").cloned() {
        return Ok(m);
    }
    let at = src
        .find("entities")
        .and_then(|i| src[i..].find('{').map(|j| i + j))
        .context("data.mjs has no `entities` object")?;
    let empty = BTreeMap::new();
    let mut p = P::new(src, &empty);
    p.i = at;
    match p.value()? {
        Value::Object(m) => Ok(m),
        _ => bail!("data.mjs `entities` is not an object"),
    }
}

/// `schema.mjs` -> a T-box. Reads the fluent `tbox.component/class/rel` calls
/// and the chained axiom words; nothing is executed.
pub fn parse_schema_mjs(src: &str, ontology: &str) -> Result<TBox> {
    let consts = collect_consts(src);
    let mut tb = TBox { ontology: ontology.to_string(), ..Default::default() };
    let name = src
        .find("new Ontology(")
        .map(|i| i + "new Ontology(".len())
        .and_then(|i| {
            let empty = BTreeMap::new();
            let mut p = P::new(src, &empty);
            p.i = i;
            p.string().ok()
        });
    if let Some(n) = name {
        tb.ontology = n;
    }

    let mut i = 0usize;
    while i < src.len() {
        let Some(rel) = src[i..].find("tbox.") else { break };
        let at = i + rel + "tbox.".len();
        let empty = BTreeMap::new();
        let mut p = P::new(src, &empty);
        p.i = at;
        let kind = p.ident();
        if !p.eat(b'(') {
            i = at;
            continue;
        }
        match kind.as_str() {
            "component" => {
                let cname = p.string().unwrap_or_default();
                p.eat(b',');
                let mut fields: BTreeMap<String, FieldDef> = BTreeMap::new();
                if p.peek() == b'{' {
                    let pc = P { s: src.as_bytes(), i: p.i, consts: &consts };
                    let mut pc = pc;
                    if let Ok(Value::Object(m)) = pc.object() {
                        for (f, def) in m {
                            fields.insert(f, field_def(&def));
                        }
                    }
                    p.i = pc.i;
                }
                if !cname.is_empty() {
                    tb.components.insert(cname, ComponentDef { fields, methods: vec![] });
                }
                i = p.i;
            }
            "class" => {
                let cname = p.string().unwrap_or_default();
                p.eat(b',');
                let mut def = ClassDef::default();
                if p.peek() == b'{' {
                    let mut pc = P { s: src.as_bytes(), i: p.i, consts: &consts };
                    if let Ok(Value::Object(m)) = pc.object() {
                        def.top = m.get("top").and_then(|v| v.as_bool()).unwrap_or(false);
                        def.id_field = m
                            .get("idField")
                            .or_else(|| m.get("id_field"))
                            .and_then(|v| v.as_str().map(|s| s.to_string()));
                        def.parent = m.get("parent").and_then(|v| v.as_str().map(|s| s.to_string()));
                        if let Some(Value::Object(c)) = m.get("components") {
                            for (alias, comp) in c {
                                if let Some(s) = comp.as_str() {
                                    def.components.insert(alias.clone(), s.to_string());
                                }
                            }
                        }
                    }
                    p.i = pc.i;
                }
                if !cname.is_empty() {
                    tb.classes.insert(cname, def);
                }
                i = p.i;
            }
            "rel" | "relation" => {
                let rname = p.string().unwrap_or_default();
                p.eat(b',');
                let mut def = RelationDef::default();
                if p.peek() == b'{' {
                    let mut pc = P { s: src.as_bytes(), i: p.i, consts: &consts };
                    if let Ok(Value::Object(m)) = pc.object() {
                        def.domain = m
                            .get("from")
                            .or_else(|| m.get("domain"))
                            .and_then(|v| v.as_str().map(|s| s.to_string()));
                        def.range = m
                            .get("to")
                            .or_else(|| m.get("range"))
                            .and_then(|v| v.as_str().map(|s| s.to_string()));
                        def.cardinality = m
                            .get("card")
                            .or_else(|| m.get("cardinality"))
                            .and_then(|v| v.as_str())
                            .and_then(Cardinality::parse)
                            .unwrap_or(Cardinality::Mtm);
                        def.transitive = m.get("transitive").and_then(|v| v.as_bool()).unwrap_or(false);
                        def.symmetric = m.get("symmetric").and_then(|v| v.as_bool()).unwrap_or(false);
                        def.functional = m.get("functional").and_then(|v| v.as_bool()).unwrap_or(false);
                        def.inverse_of = m.get("inverseOf").and_then(|v| v.as_str().map(|s| s.to_string()));
                        def.sub_property_of =
                            m.get("subPropertyOf").and_then(|v| v.as_str().map(|s| s.to_string()));
                        if let Some(Value::Object(q)) = m.get("qualifiers") {
                            for (qn, qd) in q {
                                def.qualifiers.insert(qn.clone(), field_def(qd));
                            }
                        }
                        if let Some(Value::Array(ch)) = m.get("chain") {
                            def.chain = ch.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
                        }
                    }
                    p.i = pc.i;
                }
                // the chained axiom words: `.inverseOf("X").transitive()`
                p.eat(b')');
                loop {
                    p.ws();
                    if p.peek() != b'.' {
                        break;
                    }
                    p.i += 1;
                    let word = p.ident();
                    if !p.eat(b'(') {
                        break;
                    }
                    match word.as_str() {
                        "inverseOf" => def.inverse_of = p.string().ok(),
                        "subPropertyOf" => def.sub_property_of = p.string().ok(),
                        "transitive" => def.transitive = true,
                        "symmetric" => def.symmetric = true,
                        "functional" => def.functional = true,
                        "chain" => {
                            let mut legs = Vec::new();
                            while let Ok(s) = p.string() {
                                legs.push(s);
                                if !p.eat(b',') {
                                    break;
                                }
                            }
                            def.chain = legs;
                        }
                        _ => {}
                    }
                    p.eat(b')');
                }
                if !rname.is_empty() {
                    tb.relations.insert(rname, def);
                }
                i = p.i;
            }
            _ => i = at,
        }
    }
    Ok(tb)
}

fn field_def(v: &Value) -> FieldDef {
    match v {
        Value::Object(m) => FieldDef {
            r#type: m.get("type").and_then(|t| t.as_str()).unwrap_or("string").to_string(),
            required: m.get("required").and_then(|t| t.as_bool()).unwrap_or(false),
            list: m.get("list").and_then(|t| t.as_bool()).unwrap_or(false),
            description: m.get("description").and_then(|t| t.as_str().map(|s| s.to_string())),
            values: m
                .get("values")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
            allowed_types: m
                .get("allowedTypes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
        },
        Value::String(t) => FieldDef { r#type: t.clone(), ..Default::default() },
        _ => FieldDef::default(),
    }
}

/// One es6 entity object -> an [`Entity`].
pub fn entity_from_object(slug_str: &str, obj: &Map<String, Value>) -> Result<Entity> {
    let s = slug::parse(slug_str)?;
    let mut e = Entity::new(s.cls, s.id);
    for (k, v) in obj {
        if k.starts_with('_') {
            continue;
        }
        if slug::is_relation_name(k) {
            let items: Vec<&Value> = match v {
                Value::Array(a) => a.iter().collect(),
                Value::Null => vec![],
                one => vec![one],
            };
            let mut edges = Vec::new();
            for it in items {
                match it {
                    Value::String(t) => edges.push(Edge::plain(t.trim())),
                    Value::Object(m) => {
                        let Some(to) = m.get("_to").and_then(|x| x.as_str()) else { continue };
                        let qualifiers: BTreeMap<String, Value> = m
                            .iter()
                            .filter(|(k, _)| *k != "_to")
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        edges.push(Edge { to: to.trim().to_string(), qualifiers });
                    }
                    _ => {}
                }
            }
            if !edges.is_empty() {
                e.relations.insert(k.clone(), edges);
            }
            continue;
        }
        match v {
            Value::Object(m) => {
                e.components.insert(k.clone(), m.clone());
            }
            Value::Null => {}
            other => {
                let mut m = Map::new();
                m.insert("value".into(), other.clone());
                e.components.insert(k.clone(), m);
            }
        }
    }
    Ok(e)
}

// ─── the command ────────────────────────────────────────────────────────────

pub fn run(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["out", "ontology"]);
    let Some(src_dir) = a.positional.first() else { bail!("usage: convert") };
    let src = config::expand(src_dir);
    let schema_mjs = src.join("schema.mjs");
    let data_mjs = src.join("data.mjs");
    if !schema_mjs.exists() || !data_mjs.exists() {
        bail!(
            "{} needs both schema.mjs and data.mjs (that pair is what makes it an es6 brain)",
            src.display()
        );
    }
    let out: PathBuf = match a.get("out") {
        Some(o) => config::expand(o),
        None => config::resolve(g.db.as_deref())
            .map(|b| b.db_root)
            .unwrap_or_else(|_| src.join("db")),
    };
    let ontology = a
        .get("ontology")
        .map(|s| s.to_string())
        .unwrap_or_else(|| config::alias_from_project(&src));

    let schema_src = std::fs::read_to_string(&schema_mjs)?;
    let mut tb = parse_schema_mjs(&schema_src, &ontology)?;

    let data_src = std::fs::read_to_string(&data_mjs)?;
    let entities = parse_data_mjs(&data_src)?;

    // An es6 slug is a JS object key and can hold anything; a brain2 slug is a
    // FILENAME, and `id` is deliberately narrow (`[A-Za-z0-9][A-Za-z0-9._-]*`).
    // dstwiki has 23 entities whose ids carry an apostrophe — `Abigail's_Flower`
    // — so they are renamed, and every edge that points at one is renamed with
    // them. Dropping them instead loses real data; renaming only the key leaves
    // dangling edges. The display name is unaffected: it comes from the entity's
    // own title field, which is where an apostrophe belongs.
    let mut rename: BTreeMap<String, String> = BTreeMap::new();
    for slug_str in entities.keys() {
        if slug::is_slug(slug_str) {
            continue;
        }
        let Some((cls, id)) = slug_str.split_once('/') else { continue };
        if !slug::is_class_name(cls) {
            continue;
        }
        let fixed: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
            .collect();
        let fixed = fixed.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_string();
        if fixed.is_empty() {
            continue;
        }
        rename.insert(slug_str.clone(), format!("{cls}/{fixed}"));
    }
    let remap = |s: &str| -> String { rename.get(s).cloned().unwrap_or_else(|| s.to_string()) };

    // Widen the T-box to admit what the A-box actually holds. An es6 database
    // grows by crawling, so data.mjs routinely runs ahead of schema.mjs; losing
    // those fields in translation would be the opposite of a migration. Each
    // addition is reported so it can be reviewed rather than discovered later.
    let mut added_fields: Vec<String> = Vec::new();
    let mut added_rels: Vec<String> = Vec::new();
    let mut added_classes: Vec<String> = Vec::new();
    let mut widened_card: Vec<String> = Vec::new();
    for (slug_str, obj) in &entities {
        let Ok(s) = slug::parse(&remap(slug_str)) else { continue };
        let Value::Object(obj) = obj else { continue };
        if !tb.classes.contains_key(&s.cls) {
            tb.classes.insert(s.cls.clone(), ClassDef { top: true, ..Default::default() });
            added_classes.push(s.cls.clone());
        }
        for (k, v) in obj {
            if slug::is_relation_name(k) {
                // A cardinality the data contradicts is not a constraint, it is
                // a stale declaration. dstwiki declares YIELDS and SPAWNS `nto`
                // and then gives 27 structures several targets each — a Grave
                // really does yield more than one thing. Widening to `mtm` here
                // is the same rule as widening a missing field: the A-box is
                // what exists, and a schema that contradicts it fails validation
                // on arrival rather than telling anyone anything.
                let n = match v {
                    Value::Array(a) => a.len(),
                    Value::Null => 0,
                    _ => 1,
                };
                if n > 1 {
                    if let Some(rd) = tb.relations.get_mut(k) {
                        if rd.cardinality.is_single_valued() {
                            rd.cardinality = Cardinality::Mtm;
                            rd.functional = false;
                            widened_card.push(k.clone());
                        }
                    }
                }
                if !tb.relations.contains_key(k) {
                    let to = match v {
                        Value::Array(a) => a
                            .iter()
                            .filter_map(|x| x.as_str().or_else(|| x.get("_to").and_then(|y| y.as_str())))
                            .next()
                            .and_then(|t| slug::parse(t).ok())
                            .map(|t| t.cls),
                        _ => None,
                    };
                    tb.relations.insert(
                        k.clone(),
                        RelationDef {
                            domain: Some(s.cls.clone()),
                            range: to,
                            cardinality: Cardinality::Mtm,
                            ..Default::default()
                        },
                    );
                    added_rels.push(k.clone());
                }
                continue;
            }
            let Value::Object(fields) = v else { continue };
            // the component this alias maps to, inventing one if the class does
            // not compose anything under that name
            let comp = match tb.classes.get(&s.cls).and_then(|c| c.components.get(k)) {
                Some(c) => c.clone(),
                None => {
                    let cname = format!("{}{}", k[..1].to_uppercase(), &k[1..]);
                    tb.components.entry(cname.clone()).or_default();
                    tb.classes.entry(s.cls.clone()).or_default().components.insert(k.clone(), cname.clone());
                    cname
                }
            };
            let cd = tb.components.entry(comp.clone()).or_default();
            for (fk, fv) in fields {
                if cd.fields.contains_key(fk) {
                    continue;
                }
                let fd = match fv {
                    Value::Array(items) => FieldDef {
                        r#type: if items.iter().all(|x| x.is_string() || x.is_null()) { "string".into() } else { "json".into() },
                        list: true,
                        ..Default::default()
                    },
                    Value::Bool(_) => FieldDef { r#type: "bool".into(), ..Default::default() },
                    Value::Number(n) if n.is_i64() => FieldDef { r#type: "int".into(), ..Default::default() },
                    Value::Number(_) => FieldDef { r#type: "float".into(), ..Default::default() },
                    Value::Object(_) => FieldDef { r#type: "json".into(), ..Default::default() },
                    _ => FieldDef::default(),
                };
                cd.fields.insert(fk.clone(), fd);
                added_fields.push(format!("{comp}.{fk}"));
            }
        }
    }

    // Relations whose declared ends do not exist as classes become wildcards
    // rather than blocking the load; the same rule the ICIJ import needs.
    let known: Vec<String> = tb.classes.keys().cloned().collect();
    for r in tb.relations.values_mut() {
        if let Some(d) = &r.domain {
            if !known.contains(d) {
                r.domain = None;
            }
        }
        if let Some(d) = &r.range {
            if !known.contains(d) {
                r.range = None;
            }
        }
        // an inverse whose ends no longer swap is not an inverse
        r.transitive = r.transitive && r.domain == r.range;
        r.symmetric = r.symmetric && r.domain == r.range;
    }
    let pairs: Vec<(String, Option<String>)> = tb
        .relations
        .iter()
        .map(|(k, v)| (k.clone(), v.inverse_of.clone()))
        .collect();
    for (name, inv) in pairs {
        let Some(inv) = inv else { continue };
        let ok = match (tb.relations.get(&name), tb.relations.get(&inv)) {
            (Some(a), Some(b)) => a.domain == b.range && a.range == b.domain,
            _ => false,
        };
        if !ok {
            if let Some(r) = tb.relations.get_mut(&name) {
                r.inverse_of = None;
            }
        }
    }
    tb.validate().context("the converted schema does not satisfy its own invariants")?;

    std::fs::create_dir_all(&out)?;
    std::fs::write(out.join("schema.yaml"), tb.to_yaml())?;

    let mut n = 0usize;
    let mut skipped = 0usize;
    for (slug_str, obj) in &entities {
        let Value::Object(obj) = obj else { continue };
        match entity_from_object(&remap(slug_str), obj) {
            Ok(mut e) => {
                for edges in e.relations.values_mut() {
                    for edge in edges.iter_mut() {
                        edge.to = remap(&edge.to);
                    }
                }
                e.write(&out)?;
                n += 1;
            }
            Err(err) => {
                eprintln!("warn: {slug_str}: {err}");
                skipped += 1;
            }
        }
    }

    println!("{} -> {}", src.display(), out.display());
    println!(
        "  schema.yaml  {} class(es), {} component(s), {} relation(s)",
        tb.classes.len(),
        tb.components.len(),
        tb.relations.len()
    );
    println!("  entities     {n} file(s){}", if skipped > 0 { format!(", {skipped} skipped") } else { String::new() });
    if !rename.is_empty() {
        println!(
            "  renamed      {} id(s) that held a character a filename may not: {}{}",
            rename.len(),
            rename.keys().take(3).cloned().collect::<Vec<_>>().join(", "),
            if rename.len() > 3 { " …" } else { "" }
        );
    }
    widened_card.sort();
    widened_card.dedup();
    if !widened_card.is_empty() {
        println!(
            "  widened      {} relation(s) from a single-valued cardinality the data contradicts: {}",
            widened_card.len(),
            widened_card.join(", ")
        );
    }
    for (what, list) in [("class", added_classes), ("relation", added_rels), ("field", added_fields)] {
        if list.is_empty() {
            continue;
        }
        println!(
            "  widened the T-box with {} undeclared {what}(s) found in the data: {}{}",
            list.len(),
            list.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
            if list.len() > 8 { " …" } else { "" }
        );
    }
    println!("\nnext:  brain reindex   # then `brain crawl '<question>'`");
    Ok(0)
}

// ─── the LLM-assisted write paths ───────────────────────────────────────────

const EXTRACT_SYSTEM: &str = "\
You turn a document into entities for a typed knowledge graph. You are given the schema and one \
document. Reply with JSON only: {\"entities\": [{\"slug\": \"Class/id\", \"components\": {\"alias\": \
{\"field\": value}}, \"relations\": {\"REL\": [\"Class/id\"]}}]}.

Rules: use ONLY the classes, component aliases, fields and relations the schema lists; never invent \
a name. Omit anything the document does not state — a missing field is correct, a guessed one is \
not. An id is lowercase, with words joined by hyphens. If the document describes nothing that fits \
the schema, reply {\"entities\": []}.";

fn schema_digest(tb: &TBox) -> String {
    let mut s = format!("ontology: {}\n", tb.ontology);
    for (cname, c) in &tb.classes {
        let fields: Vec<String> = c
            .components
            .iter()
            .map(|(alias, comp)| {
                let f = tb
                    .components
                    .get(comp)
                    .map(|cd| cd.fields.keys().cloned().collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                format!("{alias}({f})")
            })
            .collect();
        s.push_str(&format!("class {cname}: {}\n", fields.join(" ")));
    }
    for (rname, r) in &tb.relations {
        s.push_str(&format!(
            "rel {rname}: {} -> {}\n",
            r.domain.clone().unwrap_or_else(|| "*".into()),
            r.range.clone().unwrap_or_else(|| "*".into())
        ));
    }
    s
}

/// Write the entities a JSON payload describes. Shared by `ingest`,
/// `set --file` and the MCP `put_entity` tool.
pub async fn write_entities_json(ctx: &Ctx, v: &Value) -> Result<usize> {
    write_extracted(ctx, v, false).await
}

async fn write_extracted(ctx: &Ctx, v: &Value, partial: bool) -> Result<usize> {
    let list = v["entities"].as_array().cloned().unwrap_or_default();
    let mut n = 0usize;
    for item in list {
        let Some(slug_str) = item["slug"].as_str() else { continue };
        let Ok(s) = slug::parse(slug_str) else {
            eprintln!("warn: skipping malformed slug '{slug_str}'");
            continue;
        };
        let Some(cls) = ctx.tbox.resolve_class(&s.cls) else {
            eprintln!("warn: skipping {slug_str}: '{}' is not a declared class", s.cls);
            continue;
        };
        let mut e = Entity::new(cls, s.id.to_lowercase());
        if let Some(Value::Object(comps)) = item.get("components") {
            for (alias, fields) in comps {
                if let Value::Object(f) = fields {
                    e.components.insert(alias.clone(), f.clone());
                }
            }
        }
        if let Some(Value::Object(rels)) = item.get("relations") {
            for (rel, targets) in rels {
                let Some(r) = ctx.tbox.resolve_relation(rel) else { continue };
                if let Value::Array(ts) = targets {
                    for t in ts {
                        if let Some(t) = t.as_str() {
                            if slug::is_slug(t) {
                                e.add_edge(r, t);
                            }
                        }
                    }
                }
            }
        }
        // merge rather than clobber, so a second document about the same entity
        // adds to it
        let path = e.path_in(&ctx.brain.db_root);
        if path.exists() {
            if let Ok(existing) = crate::abox::read_entity(&e.cls, &e.id, &path) {
                let mut merged = existing;
                merged.merge(e);
                merged.write(&ctx.brain.db_root)?;
                ctx.store.upsert(&merged).await?;
                n += 1;
                continue;
            }
        }
        let findings = {
            let mut probe = ctx.brain.clone();
            probe.config.storage.clear();
            e.write(&ctx.brain.db_root)?;
            let f = crate::index::validate(&probe, &ctx.tbox)?;
            f.into_iter()
                .filter(|x| x.subject == e.slug() && x.severity == crate::index::Severity::Error)
                .collect::<Vec<_>>()
        };
        if !findings.is_empty() && !partial {
            for f in &findings {
                eprintln!("warn: {f}");
            }
            std::fs::remove_file(e.path_in(&ctx.brain.db_root)).ok();
            eprintln!("  (re-run with --partial to keep it anyway)");
            continue;
        }
        ctx.store.upsert(&e).await?;
        n += 1;
    }
    ctx.store.refresh_stats().await?;
    Ok(n)
}

/// `set --file`: a `.yaml`/`.md` validates deterministically, anything else
/// goes through extraction.
pub async fn ingest_structured_file(
    ctx: &Ctx,
    path: PathBuf,
    class_hint: Option<&str>,
    partial: bool,
) -> Result<usize> {
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
    if ext == "yaml" || ext == "yml" || ext == "json" || ext == "md" {
        let (front, _) = crate::abox::split_frontmatter(&text);
        let body = if front.is_empty() { text.as_str() } else { front };
        let doc: serde_yaml::Value = serde_yaml::from_str(body).context("parsing the file")?;
        let json = crate::abox::yaml_to_json(&doc);
        // either the extraction shape, or a bare slug -> object map
        if json.get("entities").is_some() {
            return write_extracted(ctx, &json, partial).await;
        }
        if let Value::Object(m) = &json {
            let mut n = 0;
            for (k, v) in m {
                if let (true, Value::Object(obj)) = (slug::is_slug(k), v) {
                    let e = entity_from_object(k, obj)?;
                    e.write(&ctx.brain.db_root)?;
                    ctx.store.upsert(&e).await?;
                    n += 1;
                }
            }
            if n > 0 {
                ctx.store.refresh_stats().await?;
                return Ok(n);
            }
        }
    }
    let hint = match class_hint {
        Some(c) => format!("\nFocus on the class {c}.\n"),
        None => String::new(),
    };
    let v = ctx
        .chat
        .complete_json(&[
            ChatMessage::system(EXTRACT_SYSTEM),
            ChatMessage::user(format!(
                "SCHEMA:\n{}{hint}\nDOCUMENT ({}):\n{}",
                schema_digest(&ctx.tbox),
                path.display(),
                text.chars().take(24000).collect::<String>()
            )),
        ])
        .await?;
    write_extracted(ctx, &v, partial).await
}

pub async fn ingest_documents(
    ctx: &Ctx,
    dir: &Path,
    extract: &[String],
    exclude: &[String],
    partial: bool,
    suggest: bool,
) -> Result<i32> {
    let mut files: Vec<PathBuf> = Vec::new();
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let s = p.to_string_lossy().to_string();
        if exclude.iter().any(|x| s.contains(x.as_str())) {
            continue;
        }
        // never ingest the brain's own entity files
        if p.starts_with(&ctx.brain.db_root) {
            continue;
        }
        files.push(p.to_path_buf());
    }
    if files.is_empty() {
        println!("no .md files under {}", dir.display());
        return Ok(0);
    }
    println!("{} document(s) under {}", files.len(), dir.display());
    let hint = if extract.is_empty() {
        String::new()
    } else {
        format!("\nExtract ONLY these classes: {}.\n", extract.join(", "))
    };
    let mut total = 0usize;
    for f in &files {
        let text = std::fs::read_to_string(f)?;
        if text.trim().is_empty() {
            continue;
        }
        let user = format!(
            "SCHEMA:\n{}{hint}\nDOCUMENT ({}):\n{}",
            schema_digest(&ctx.tbox),
            f.display(),
            text.chars().take(24000).collect::<String>()
        );
        let sys = if suggest {
            "You read a document and report what a typed knowledge graph would NEED in order to \
hold it. Reply with JSON only: {\"def\": [\"brain def class … \", …]} listing the `brain def` \
commands that would add the missing classes, components, fields and relations. Omit anything the \
schema already has. Never invent content that the document does not state."
        } else {
            EXTRACT_SYSTEM
        };
        let v = match ctx.chat.complete_json(&[ChatMessage::system(sys), ChatMessage::user(user)]).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warn: {}: {e}", f.display());
                continue;
            }
        };
        if suggest {
            for line in v["def"].as_array().cloned().unwrap_or_default() {
                if let Some(s) = line.as_str() {
                    println!("{s}");
                }
            }
            continue;
        }
        let n = write_extracted(ctx, &v, partial).await?;
        total += n;
        println!("  {}  -> {n} entity(ies)", f.display());
    }
    if !suggest {
        println!("\n{total} entity(ies) written — run `brain refine`-style checks with `brain validate`");
    }
    Ok(0)
}

/// Insert `[[wiki-links]]` for entities a document mentions.
pub async fn enrich(ctx: &Ctx, path: &Path, create_missing: bool) -> Result<i32> {
    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        for e in walkdir::WalkDir::new(path).into_iter().flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("md")
                && !e.path().starts_with(&ctx.brain.db_root)
            {
                files.push(e.path().to_path_buf());
            }
        }
    } else {
        files.push(path.to_path_buf());
    }
    // Every label in the brain, longest first, so "Super Mario Galaxy" wins
    // over the "Mario" inside it.
    let mut labels: Vec<(String, String)> = Vec::new();
    let mut offset = 0i64;
    loop {
        let page = ctx.store.list_class(None, 4096, offset).await?;
        if page.is_empty() {
            break;
        }
        for (slug, _, label) in &page {
            if label.chars().count() >= 4 {
                labels.push((label.clone(), slug.clone()));
            }
        }
        offset += page.len() as i64;
    }
    labels.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let mut touched = 0usize;
    for f in &files {
        let text = std::fs::read_to_string(f)?;
        let mut out = text.clone();
        let mut hits = 0usize;
        for (label, _slug) in &labels {
            if out.contains(&format!("[[{label}]]")) {
                continue;
            }
            // one link per label per document: the first mention is the one a
            // reader follows
            if let Some(at) = find_word(&out, label) {
                out.replace_range(at..at + label.len(), &format!("[[{label}]]"));
                hits += 1;
            }
        }
        if create_missing {
            eprintln!("note: --ingest creates nothing yet for {}; run `brain ingest` on it instead", f.display());
        }
        if hits > 0 {
            std::fs::write(f, &out)?;
            println!("  {}  -> {hits} link(s)", f.display());
            touched += 1;
        }
    }
    println!("\nlinked {touched} document(s)");
    Ok(0)
}

/// A whole-word, case-sensitive match outside an existing `[[…]]`.
fn find_word(hay: &str, needle: &str) -> Option<usize> {
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(needle) {
        let at = from + rel;
        let before_ok = at == 0
            || !hay[..at]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '[')
                .unwrap_or(false);
        let after = at + needle.len();
        let after_ok = after >= hay.len()
            || !hay[after..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric() || c == ']')
                .unwrap_or(false);
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}
