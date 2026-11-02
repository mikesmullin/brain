//! slug.rs — a slug is always `Class/id` (class-prefixed, no bare ids).
//!
//! Carried over verbatim from brain1: the V1 ambiguity where a bare `apollo`
//! could not be resolved to a class is what the class prefix exists to remove.
//! Classes are ProperCase; ids are lowercase-ish tokens. Comparison is
//! case-insensitive everywhere a user types a slug, but the stored form keeps
//! the schema's casing for the class and lowercase for the id.

use anyhow::{bail, Result};

/// `^[A-Z][A-Za-z0-9]*$`
pub fn is_class_name(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric())
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]*$`
pub fn is_id(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slug {
    pub cls: String,
    pub id: String,
}

impl Slug {
    pub fn new(cls: impl Into<String>, id: impl Into<String>) -> Self {
        Slug { cls: cls.into(), id: id.into() }
    }
    pub fn to_string(&self) -> String {
        format!("{}/{}", self.cls, self.id)
    }
}

impl std::fmt::Display for Slug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.cls, self.id)
    }
}

/// Parse `Class/id`. Errors name which half was rejected, so a typo in a long
/// slug does not read as a generic parse failure.
pub fn parse(s: &str) -> Result<Slug> {
    let s = s.trim();
    if s.is_empty() {
        bail!("slug is required");
    }
    let idx = match s.find('/') {
        Some(i) if i > 0 && i < s.len() - 1 => i,
        _ => bail!("invalid slug '{s}': expected 'Class/id' (class prefix required)"),
    };
    let cls = &s[..idx];
    let id = &s[idx + 1..];
    if !is_class_name(cls) {
        bail!("invalid class '{cls}' in slug '{s}': expected ProperCase");
    }
    if !is_id(id) {
        bail!("invalid id '{id}' in slug '{s}'");
    }
    Ok(Slug { cls: cls.to_string(), id: id.to_string() })
}

pub fn is_slug(s: &str) -> bool {
    parse(s).is_ok()
}

/// A relation name: ALL_CAPS with digits and underscores.
pub fn is_relation_name(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    !s.is_empty() && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Sanitize arbitrary text into an ALL_CAPS relation name
/// (`officer of` -> `OFFICER_OF`). Used by bulk importers.
pub fn sanitize_relation(s: &str) -> String {
    let mut out = String::new();
    let mut prev_us = true; // suppress a leading underscore
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Sanitize arbitrary text into a legal entity id (lowercase).
pub fn sanitize_id(s: &str) -> String {
    let mut out = String::new();
    let mut prev_us = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    while out.ends_with('_') || out.ends_with('.') || out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || !out.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
        out.insert(0, 'x');
    }
    out
}

/// Deterministic 8-hex placeholder id, as brain1's `canonical.placeholderId`.
/// Only idField classes use these, so they never collide with real ids.
pub fn placeholder_id(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let joined = parts.join("|").to_lowercase().trim().to_string();
    let mut h = Sha256::new();
    h.update(joined.as_bytes());
    hex::encode(h.finalize())[..8].to_string()
}
