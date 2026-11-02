//! tbox.rs — the T-box (structure): components, classes, relations.
//!
//! `db/schema.yaml` keeps brain1's shape — `components:` / `classes:` /
//! `relations:` with `domain` / `range` / `cardinality` / `qualifiers` — and
//! extends relations with the OWL vocabulary brain2 needs to export a
//! certificate a proof assistant can check:
//!
//! ```yaml
//! ontology: dstwiki
//! relations:
//!   ITEM_HAS_VARIANT:
//!     domain: Item
//!     range: Item
//!     cardinality: mtm
//!     inverseOf: ITEM_VARIANT_OF      # owl:inverseOf
//!   PART_OF:
//!     domain: Region
//!     range: Region
//!     transitive: true                # owl:TransitiveProperty
//!   SIBLING_OF:
//!     symmetric: true                 # owl:SymmetricProperty
//!   REPORTS_TO:
//!     cardinality: oto
//!     functional: true                # owl:FunctionalProperty
//!   GRANDPARENT_OF:
//!     chain: [PARENT_OF, PARENT_OF]   # owl:propertyChainAxiom
//!   ANCESTOR_OF:
//!     subPropertyOf: RELATED_TO       # rdfs:subPropertyOf
//! ```
//!
//! Every axiom is checked at load time against a NAMED invariant, so a schema
//! that would let the reasoner walk out of its own range cannot be loaded —
//! the error says which invariant and on what. That is the whole point of
//! putting the words in the schema rather than in prose: a chain whose legs do
//! not compose does not exist after `load()`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const BASE_O: &str = "https://bz.io/o#";
pub const BASE_D: &str = "https://bz.io/d#";
pub const OWL: &str = "http://www.w3.org/2002/07/owl#";
pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";

/// A named invariant violation. The name is part of the contract: tooling can
/// match on it, and the message always says which subject failed.
#[derive(Debug)]
pub struct TBoxError {
    pub invariant: &'static str,
    pub subject: String,
    pub detail: String,
}

impl std::fmt::Display for TBoxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] on {}: {}", self.invariant, self.subject, self.detail)
    }
}
impl std::error::Error for TBoxError {}

fn err(invariant: &'static str, subject: impl Into<String>, detail: impl Into<String>) -> anyhow::Error {
    anyhow!(TBoxError { invariant, subject: subject.into(), detail: detail.into() })
}

// ── field / component ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldDef {
    #[serde(default = "default_type")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub list: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `enum` only: the permitted values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// `ref` only: the classes a reference may point at. Empty is a wildcard.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "allowedTypes")]
    pub allowed_types: Vec<String>,
}

fn default_type() -> String {
    "string".into()
}

/// Hand-written, not derived. `#[derive(Default)]` would give `type` the empty
/// string, which is not a declarable type — and the failure surfaces a long way
/// from the cause: an importer that builds a field with `..Default::default()`
/// writes a schema that then refuses to load. The serde default and the Rust
/// default have to agree, so this states it once.
impl Default for FieldDef {
    fn default() -> Self {
        FieldDef {
            r#type: default_type(),
            required: false,
            list: false,
            description: None,
            values: Vec::new(),
            allowed_types: Vec::new(),
        }
    }
}
fn is_false(b: &bool) -> bool {
    !*b
}

/// brain1's set, plus `float`. `enum` carries a `values` list and `ref` carries
/// a `Class/id` whose target must exist — a `ref` is a *scalar* that happens to
/// name an entity, which is a different thing from a relation: it is a property
/// of this node, not an edge in the graph, and nothing traverses it.
pub const FIELD_TYPES: &[&str] =
    &["string", "int", "float", "bool", "date", "enum", "ref", "json"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComponentDef {
    #[serde(default)]
    pub fields: BTreeMap<String, FieldDef>,
    /// Method names this component contributes (ECS-style behaviour). The
    /// bodies live outside the schema; the schema only declares the surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClassDef {
    /// alias -> Component name. The alias is the lowercase A-box key.
    #[serde(default)]
    pub components: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub top: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_field: Option<String>,
    /// rdfs:subClassOf
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    Oto,
    Otn,
    Nto,
    Mtm,
}

impl Cardinality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Cardinality::Oto => "oto",
            Cardinality::Otn => "otn",
            Cardinality::Nto => "nto",
            Cardinality::Mtm => "mtm",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "oto" => Some(Cardinality::Oto),
            "otn" => Some(Cardinality::Otn),
            "nto" => Some(Cardinality::Nto),
            "mtm" => Some(Cardinality::Mtm),
            _ => None,
        }
    }
    /// At most one target per source — the two shapes `functional` permits.
    pub fn is_single_valued(&self) -> bool {
        matches!(self, Cardinality::Oto | Cardinality::Nto)
    }
}

impl Default for Cardinality {
    fn default() -> Self {
        Cardinality::Mtm
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelationDef {
    /// rdfs:domain — `None` is a wildcard (ICIJ-style edges cross classes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// rdfs:range
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    #[serde(default)]
    pub cardinality: Cardinality,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub qualifiers: BTreeMap<String, FieldDef>,
    // ── the OWL words ──
    #[serde(default, skip_serializing_if = "is_false")]
    pub transitive: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub symmetric: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub functional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_property_of: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,
    /// Derived by `load()` from the chain legs; never written by hand.
    #[serde(skip)]
    pub chain_from: Option<String>,
    #[serde(skip)]
    pub chain_to: Option<String>,
}

// ── the on-disk document ─────────────────────────────────────────────────────
//
// Serde aliases accept both `idField` (brain1's spelling, and what a human
// types) and `id_field`; we always write `idField` back.

#[derive(Debug, Clone, Default, Deserialize)]
struct RawClass {
    #[serde(default)]
    components: BTreeMap<String, String>,
    #[serde(default)]
    top: bool,
    #[serde(default, alias = "id_field", alias = "idField")]
    id_field: Option<String>,
    #[serde(default)]
    parent: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawRelation {
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    range: Option<String>,
    #[serde(default, alias = "card")]
    cardinality: Option<String>,
    #[serde(default)]
    qualifiers: BTreeMap<String, FieldDef>,
    #[serde(default)]
    transitive: bool,
    #[serde(default)]
    symmetric: bool,
    #[serde(default)]
    functional: bool,
    #[serde(default, alias = "inverse_of", alias = "inverseOf")]
    inverse_of: Option<String>,
    #[serde(default, alias = "sub_property_of", alias = "subPropertyOf")]
    sub_property_of: Option<String>,
    #[serde(default)]
    chain: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawSchema {
    #[serde(default)]
    ontology: Option<String>,
    #[serde(default)]
    components: BTreeMap<String, ComponentDef>,
    #[serde(default)]
    classes: BTreeMap<String, RawClass>,
    #[serde(default)]
    relations: BTreeMap<String, RawRelation>,
}

// ── the loaded T-box ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TBox {
    pub ontology: String,
    pub components: BTreeMap<String, ComponentDef>,
    pub classes: BTreeMap<String, ClassDef>,
    pub relations: BTreeMap<String, RelationDef>,
}

impl TBox {
    pub fn from_yaml(text: &str) -> Result<TBox> {
        let raw: RawSchema = serde_yaml::from_str(text).context("parsing schema.yaml")?;
        let mut tb = TBox {
            ontology: raw.ontology.unwrap_or_else(|| "brain".into()),
            components: raw.components,
            classes: BTreeMap::new(),
            relations: BTreeMap::new(),
        };
        for (name, c) in raw.classes {
            tb.classes.insert(
                name,
                ClassDef {
                    components: c.components,
                    top: c.top,
                    id_field: c.id_field,
                    parent: c.parent,
                },
            );
        }
        for (name, r) in raw.relations {
            let card = match r.cardinality.as_deref() {
                None => Cardinality::Mtm,
                Some(s) => Cardinality::parse(s).ok_or_else(|| {
                    err("invalid_cardinality", &name, format!("cardinality '{s}' — expected oto/otn/nto/mtm"))
                })?,
            };
            tb.relations.insert(
                name,
                RelationDef {
                    domain: r.domain,
                    range: r.range,
                    cardinality: card,
                    qualifiers: r.qualifiers,
                    transitive: r.transitive,
                    symmetric: r.symmetric,
                    functional: r.functional,
                    inverse_of: r.inverse_of,
                    sub_property_of: r.sub_property_of,
                    chain: r.chain,
                    chain_from: None,
                    chain_to: None,
                },
            );
        }
        tb.validate()?;
        Ok(tb)
    }

    pub fn load(path: &Path) -> Result<TBox> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        TBox::from_yaml(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Every named invariant, in dependency order. A T-box that loads is a
    /// T-box whose cross-references all resolve.
    pub fn validate(&mut self) -> Result<()> {
        // class component references
        for (cname, c) in &self.classes {
            for (alias, comp) in &c.components {
                if !self.components.contains_key(comp) {
                    return Err(err("unknown_component", format!("{cname}.{alias}"), format!("'{comp}' not declared")));
                }
            }
            if let Some(f) = &c.id_field {
                // either `alias.field` on a declared component, or a relation name
                if crate::slug::is_relation_name(f) {
                    // resolved after relations load; checked below
                } else {
                    let (a, fld) = f
                        .split_once('.')
                        .ok_or_else(|| err("idfield_shape", cname, format!("idField '{f}' must be 'alias.field' or a RELATION name")))?;
                    let comp = c.components.get(a).ok_or_else(|| {
                        err("idfield_unknown_alias", cname, format!("idField '{f}' names alias '{a}', which the class does not compose"))
                    })?;
                    let cd = &self.components[comp];
                    if !cd.fields.contains_key(fld) {
                        return Err(err("idfield_unknown_field", cname, format!("idField '{f}': component '{comp}' has no field '{fld}'")));
                    }
                }
            }
            // field types
        }
        for (comp, cd) in &self.components {
            for (fname, fd) in &cd.fields {
                if !FIELD_TYPES.contains(&fd.r#type.as_str()) {
                    return Err(err(
                        "unknown_field_type",
                        format!("{comp}.{fname}"),
                        format!("type '{}' — expected one of {}", fd.r#type, FIELD_TYPES.join("/")),
                    ));
                }
                if fd.r#type == "enum" && fd.values.is_empty() {
                    return Err(err(
                        "enum_without_values",
                        format!("{comp}.{fname}"),
                        "type 'enum' needs a `values` list — an enum with no values admits nothing",
                    ));
                }
                for t in &fd.allowed_types {
                    if !self.classes.contains_key(t) {
                        return Err(err(
                            "ref_unknown_class",
                            format!("{comp}.{fname}"),
                            format!("allowedTypes names '{t}', which is not a declared class"),
                        ));
                    }
                }
            }
        }

        // subclass graph
        for (n, c) in &self.classes {
            if let Some(p) = &c.parent {
                if p != n && !self.classes.contains_key(p) {
                    return Err(err("unknown_class", n, format!("parent '{p}' not declared")));
                }
            }
        }
        for n in self.classes.keys() {
            let mut cur = self.classes[n].parent.clone();
            let mut seen: HashSet<String> = HashSet::new();
            while let Some(c) = cur {
                if &c == n {
                    return Err(err("subclass_cycle", n, "n → … → n"));
                }
                if !seen.insert(c.clone()) {
                    break;
                }
                cur = self.classes.get(&c).and_then(|k| k.parent.clone());
            }
        }

        // relation domain/range resolve
        let rel_names: HashSet<String> = self.relations.keys().cloned().collect();
        for (name, r) in &self.relations {
            for (field, v) in [("domain", &r.domain), ("range", &r.range)] {
                if let Some(cls) = v {
                    if !self.classes.contains_key(cls) {
                        return Err(err("unknown_class", format!("{name}.{field}"), format!("'{cls}' not declared")));
                    }
                }
            }
            for (field, v) in [("inverseOf", &r.inverse_of), ("subPropertyOf", &r.sub_property_of)] {
                if let Some(o) = v {
                    if !rel_names.contains(o) {
                        return Err(err("unknown_ref", format!("{name}.{field}"), format!("'{o}' not declared")));
                    }
                }
            }
            if !r.chain.is_empty() {
                for leg in &r.chain {
                    if !rel_names.contains(leg) {
                        return Err(err("chain_undefined_leg", name, format!("leg '{leg}' not declared")));
                    }
                }
                if r.chain.len() != 2 {
                    return Err(err("chain_arity", name, format!("chain has {} legs — the language defines two-leg chains", r.chain.len())));
                }
            }
            if r.transitive && r.domain != r.range {
                return Err(err(
                    "transitive_range_mismatch",
                    name,
                    format!(
                        "transitive with domain '{}' != range '{}' — the closure would walk out of range",
                        r.domain.clone().unwrap_or_else(|| "*".into()),
                        r.range.clone().unwrap_or_else(|| "*".into())
                    ),
                ));
            }
            if r.symmetric && r.domain != r.range {
                return Err(err(
                    "symmetric_range_mismatch",
                    name,
                    "symmetric needs domain == range — the reverse edge must stay typed",
                ));
            }
            if r.functional && !r.cardinality.is_single_valued() {
                return Err(err(
                    "functional_with_mtm",
                    name,
                    format!("functional with cardinality '{}' — functional needs oto or nto", r.cardinality.as_str()),
                ));
            }
            if let Some(inv) = &r.inverse_of {
                let i = &self.relations[inv];
                if i.domain != r.range || i.range != r.domain {
                    return Err(err(
                        "inverse_pair_mismatch",
                        name,
                        format!(
                            "'{inv}' is {}→{}; an inverse of {}→{} must be {}→{}",
                            i.domain.clone().unwrap_or_else(|| "*".into()),
                            i.range.clone().unwrap_or_else(|| "*".into()),
                            r.domain.clone().unwrap_or_else(|| "*".into()),
                            r.range.clone().unwrap_or_else(|| "*".into()),
                            r.range.clone().unwrap_or_else(|| "*".into()),
                            r.domain.clone().unwrap_or_else(|| "*".into())
                        ),
                    ));
                }
            }
            if let Some(sp) = &r.sub_property_of {
                let s = &self.relations[sp];
                if s.cardinality != Cardinality::Mtm {
                    return Err(err(
                        "subproperty_cardinality",
                        name,
                        format!("subPropertyOf '{sp}' (card '{}') — a kind-of relation must be mtm", s.cardinality.as_str()),
                    ));
                }
            }
        }

        // chain composition, resolved over the finished registry
        let snapshot: HashMap<String, (Option<String>, Option<String>)> = self
            .relations
            .iter()
            .map(|(k, v)| (k.clone(), (v.domain.clone(), v.range.clone())))
            .collect();
        let mut chains: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        for (name, r) in &self.relations {
            if r.chain.len() != 2 {
                continue;
            }
            let (a, b) = (&r.chain[0], &r.chain[1]);
            let (a_from, a_to) = &snapshot[a];
            let (b_from, b_to) = &snapshot[b];
            if a_from.is_none() || a_to.is_none() {
                return Err(err("chain_untyped_leg", name, format!("leg '{a}' has no declared domain/range")));
            }
            if b_from != a_to {
                return Err(err(
                    "chain_leg_mismatch",
                    name,
                    format!(
                        "range({a}) = {} but domain({b}) = {} — the legs do not compose",
                        a_to.clone().unwrap_or_else(|| "*".into()),
                        b_from.clone().unwrap_or_else(|| "*".into())
                    ),
                ));
            }
            chains.push((name.clone(), a_from.clone(), b_to.clone()));
        }
        for (name, from, to) in chains {
            let r = self.relations.get_mut(&name).unwrap();
            r.chain_from = from;
            r.chain_to = to;
        }
        // idField relation references
        let bad: Vec<(String, String)> = self
            .classes
            .iter()
            .filter_map(|(n, c)| {
                let f = c.id_field.as_ref()?;
                if crate::slug::is_relation_name(f) && !self.relations.contains_key(f) {
                    Some((n.clone(), f.clone()))
                } else {
                    None
                }
            })
            .collect();
        if let Some((n, f)) = bad.into_iter().next() {
            return Err(err("idfield_unknown_relation", n, format!("idField '{f}' is not a declared relation")));
        }
        Ok(())
    }

    // ── lookups ──────────────────────────────────────────────────────────────

    /// Case-insensitive class resolution (`person` -> `Person`).
    pub fn resolve_class(&self, name: &str) -> Option<&str> {
        if self.classes.contains_key(name) {
            return self.classes.get_key_value(name).map(|(k, _)| k.as_str());
        }
        self.classes
            .keys()
            .find(|k| k.eq_ignore_ascii_case(name))
            .map(|k| k.as_str())
    }

    pub fn resolve_relation(&self, name: &str) -> Option<&str> {
        if self.relations.contains_key(name) {
            return self.relations.get_key_value(name).map(|(k, _)| k.as_str());
        }
        let up = name.to_ascii_uppercase();
        self.relations.keys().find(|k| **k == up).map(|k| k.as_str())
    }

    /// The class plus every ancestor, nearest first.
    pub fn ancestry(&self, cls: &str) -> Vec<String> {
        let mut out = vec![cls.to_string()];
        let mut cur = self.classes.get(cls).and_then(|c| c.parent.clone());
        let mut guard = 0;
        while let Some(p) = cur {
            if guard > 64 {
                break;
            }
            guard += 1;
            cur = self.classes.get(&p).and_then(|c| c.parent.clone());
            out.push(p);
        }
        out
    }

    /// alias -> fields, for the component bag of one class (including inherited).
    pub fn class_fields(&self, cls: &str) -> BTreeMap<String, (String, BTreeMap<String, FieldDef>)> {
        let mut out = BTreeMap::new();
        for anc in self.ancestry(cls).into_iter().rev() {
            if let Some(c) = self.classes.get(&anc) {
                for (alias, comp) in &c.components {
                    if let Some(cd) = self.components.get(comp) {
                        out.insert(alias.clone(), (comp.clone(), cd.fields.clone()));
                    }
                }
            }
        }
        out
    }

    pub fn id_field_of(&self, cls: &str) -> Option<&str> {
        for anc in self.ancestry(cls) {
            if let Some(c) = self.classes.get(&anc) {
                if let Some(f) = &c.id_field {
                    return Some(f.as_str());
                }
            }
        }
        None
    }




    // ── serialization ────────────────────────────────────────────────────────

    /// Write schema.yaml in brain1's key order, with the OWL words appended
    /// per relation only where they are set.
    pub fn to_yaml(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("ontology: {}\n", self.ontology));
        s.push_str("components:\n");
        for (name, c) in &self.components {
            s.push_str(&format!("  {name}:\n    fields:\n"));
            for (f, d) in &c.fields {
                s.push_str(&format!("      {f}:\n        type: {}\n", d.r#type));
                if d.required {
                    s.push_str("        required: true\n");
                }
                if d.list {
                    s.push_str("        list: true\n");
                }
                if let Some(desc) = &d.description {
                    s.push_str(&format!("        description: {}\n", yaml_scalar(desc)));
                }
                if !d.values.is_empty() {
                    s.push_str(&format!(
                        "        values: [{}]\n",
                        d.values.iter().map(|v| yaml_scalar(v)).collect::<Vec<_>>().join(", ")
                    ));
                }
                if !d.allowed_types.is_empty() {
                    s.push_str(&format!("        allowedTypes: [{}]\n", d.allowed_types.join(", ")));
                }
            }
            if !c.methods.is_empty() {
                s.push_str("    methods:\n");
                for m in &c.methods {
                    s.push_str(&format!("      - {m}\n"));
                }
            }
        }
        s.push_str("classes:\n");
        for (name, c) in &self.classes {
            s.push_str(&format!("  {name}:\n"));
            if !c.components.is_empty() {
                s.push_str("    components:\n");
                for (alias, comp) in &c.components {
                    s.push_str(&format!("      {alias}: {comp}\n"));
                }
            }
            if let Some(p) = &c.parent {
                s.push_str(&format!("    parent: {p}\n"));
            }
            if let Some(f) = &c.id_field {
                s.push_str(&format!("    idField: {f}\n"));
            }
            if c.top {
                s.push_str("    top: true\n");
            }
        }
        s.push_str("relations:\n");
        for (name, r) in &self.relations {
            s.push_str(&format!("  {name}:\n"));
            if let Some(d) = &r.domain {
                s.push_str(&format!("    domain: {d}\n"));
            }
            if let Some(d) = &r.range {
                s.push_str(&format!("    range: {d}\n"));
            }
            s.push_str(&format!("    cardinality: {}\n", r.cardinality.as_str()));
            if r.transitive {
                s.push_str("    transitive: true\n");
            }
            if r.symmetric {
                s.push_str("    symmetric: true\n");
            }
            if r.functional {
                s.push_str("    functional: true\n");
            }
            if let Some(i) = &r.inverse_of {
                s.push_str(&format!("    inverseOf: {i}\n"));
            }
            if let Some(i) = &r.sub_property_of {
                s.push_str(&format!("    subPropertyOf: {i}\n"));
            }
            if !r.chain.is_empty() {
                s.push_str(&format!("    chain: [{}]\n", r.chain.join(", ")));
            }
            if !r.qualifiers.is_empty() {
                s.push_str("    qualifiers:\n");
                for (q, d) in &r.qualifiers {
                    s.push_str(&format!("      {q}:\n        type: {}\n", d.r#type));
                    if d.required {
                        s.push_str("        required: true\n");
                    }
                    if d.list {
                        s.push_str("        list: true\n");
                    }
                }
            }
        }
        s
    }

    // ── the W3C seam: asserted triples, TSV, Turtle ──────────────────────────

    pub fn triples(&self) -> Vec<(String, String, String)> {
        let o = |n: &str| format!("{BASE_O}{n}");
        let mut out = Vec::new();
        for (name, c) in &self.classes {
            out.push((o(name), format!("{RDF}type"), format!("{OWL}Class")));
            if let Some(p) = &c.parent {
                out.push((o(name), format!("{RDFS}subClassOf"), o(p)));
            }
        }
        for (n, r) in &self.relations {
            let rr = o(n);
            out.push((rr.clone(), format!("{RDF}type"), format!("{OWL}ObjectProperty")));
            if let Some(d) = &r.domain {
                out.push((rr.clone(), format!("{RDFS}domain"), o(d)));
            }
            if let Some(d) = &r.range {
                out.push((rr.clone(), format!("{RDFS}range"), o(d)));
            }
            if r.cardinality.is_single_valued() || r.functional {
                out.push((rr.clone(), format!("{RDF}type"), format!("{OWL}FunctionalProperty")));
            }
            if r.transitive {
                out.push((rr.clone(), format!("{RDF}type"), format!("{OWL}TransitiveProperty")));
            }
            if r.symmetric {
                out.push((rr.clone(), format!("{RDF}type"), format!("{OWL}SymmetricProperty")));
            }
            if let Some(i) = &r.inverse_of {
                out.push((rr.clone(), format!("{OWL}inverseOf"), o(i)));
            }
            if let Some(i) = &r.sub_property_of {
                out.push((rr.clone(), format!("{RDFS}subPropertyOf"), o(i)));
            }
            if r.chain.len() == 2 {
                out.push((rr.clone(), format!("{OWL}propertyChainAxiom"), format!("{} {}", o(&r.chain[0]), o(&r.chain[1]))));
            }
        }
        out
    }

    pub fn to_tsv(&self) -> String {
        let mut s: String = self
            .triples()
            .into_iter()
            .map(|(a, b, c)| format!("{a}\t{b}\t{c}\n"))
            .collect();
        if s.is_empty() {
            s.push('\n');
        }
        s
    }

    pub fn to_turtle(&self) -> String {
        let mut l: Vec<String> = Vec::new();
        l.push(format!("@prefix o:    <{BASE_O}> ."));
        l.push(format!("@prefix d:    <{BASE_D}> ."));
        l.push(format!("@prefix owl:  <{OWL}> ."));
        l.push(format!("@prefix rdf:  <{RDF}> ."));
        l.push(format!("@prefix rdfs: <{RDFS}> ."));
        l.push(String::new());
        l.push(format!(
            "# {} — {} classes, {} relations, {} components",
            self.ontology,
            self.classes.len(),
            self.relations.len(),
            self.components.len()
        ));
        l.push(String::new());
        for (name, c) in &self.classes {
            match &c.parent {
                Some(p) => l.push(format!("o:{name} a owl:Class ; rdfs:subClassOf o:{p} .")),
                None => l.push(format!("o:{name} a owl:Class .")),
            }
        }
        l.push(String::new());
        for (n, r) in &self.relations {
            let mut types = vec!["owl:ObjectProperty".to_string()];
            if r.cardinality.is_single_valued() || r.functional {
                types.push("owl:FunctionalProperty".into());
            }
            if r.transitive {
                types.push("owl:TransitiveProperty".into());
            }
            if r.symmetric {
                types.push("owl:SymmetricProperty".into());
            }
            let mut preds: Vec<String> = vec![format!("a {}", types.join(", "))];
            if let Some(d) = &r.domain {
                preds.push(format!("rdfs:domain o:{d}"));
            }
            if let Some(d) = &r.range {
                preds.push(format!("rdfs:range o:{d}"));
            }
            if let Some(i) = &r.sub_property_of {
                preds.push(format!("rdfs:subPropertyOf o:{i}"));
            }
            if let Some(i) = &r.inverse_of {
                preds.push(format!("owl:inverseOf o:{i}"));
            }
            if r.chain.len() == 2 {
                preds.push(format!("owl:propertyChainAxiom ( o:{} o:{} )", r.chain[0], r.chain[1]));
            }
            l.push(format!("o:{n}\n    {} .", preds.join(" ;\n    ")));
        }
        l.join("\n") + "\n"
    }

    /// A content hash over the T-box, used to detect that the index was built
    /// against a different schema.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.to_yaml().as_bytes());
        hex::encode(h.finalize())[..16].to_string()
    }

    pub fn scaffold(ontology: &str) -> TBox {
        let mut tb = TBox { ontology: ontology.to_string(), ..Default::default() };
        let mut fields = BTreeMap::new();
        fields.insert(
            "name".to_string(),
            FieldDef { r#type: "string".into(), required: true, ..Default::default() },
        );
        fields.insert(
            "description".to_string(),
            FieldDef { r#type: "string".into(), ..Default::default() },
        );
        tb.components.insert("Naming".into(), ComponentDef { fields, methods: vec![] });
        let mut comps = BTreeMap::new();
        comps.insert("naming".to_string(), "Naming".to_string());
        tb.classes.insert(
            "Thing".into(),
            ClassDef { components: comps, top: true, id_field: None, parent: None },
        );
        tb.relations.insert(
            "RELATED_TO".into(),
            RelationDef {
                domain: Some("Thing".into()),
                range: Some("Thing".into()),
                cardinality: Cardinality::Mtm,
                symmetric: true,
                ..Default::default()
            },
        );
        tb
    }
}

/// Quote a YAML scalar only when it needs it.
///
/// The leading-indicator case is not decoration. YAML gives a dozen characters
/// a special meaning *only in first position*, and the ICIJ dataset contains a
/// name that begins `- ` — which YAML reads as a sequence item, which makes the
/// whole mapping invalid, which silently lost 25 entities out of two million
/// at index time. Anything that starts with an indicator gets quoted, whether
/// or not that particular one would have been ambiguous here.
pub fn yaml_scalar(s: &str) -> String {
    const INDICATOR: &[char] = &[
        '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
        '@', '`',
    ];
    let needs = s.is_empty()
        || s.contains(['\n', ':', '#', '"', '\'', '{', '}', '[', ']', ',', '&', '*', '!', '|', '>', '%', '@', '`'])
        || s.starts_with(INDICATOR)
        || s.trim() != s
        || s.parse::<f64>().is_ok()
        || matches!(s, "true" | "false" | "null" | "yes" | "no" | "~" | "on" | "off");
    if !needs {
        return s.to_string();
    }
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
