//! config.rs — which brain am I talking to, and how is it configured.
//!
//! Layout (relative to a project root):
//! ```text
//!   brain.yaml             per-db config (optional), sibling of db/
//!   db/<Class>/<id>.md     authoritative entities (git-tracked)
//!   db/schema.yaml         T-box (git-tracked)
//! ```
//!
//! brain2 keeps no rebuildable state inside `db/` — the index lives in the
//! shared PostgreSQL instance, in a schema of its own named after the brain.
//! That is the one structural difference from brain1's `db/pgdata/`, and it is
//! what lets many brains (and many apps) share one server without any chance
//! of a namespace collision.
//!
//! Named brains live in `~/.config/brain/brains.yaml`:
//! ```yaml
//!   ada: /workspace/cli/ada
//!   current: ada
//! ```

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_PG_PORT: u16 = 5433;
pub const DEFAULT_PG_DB: &str = "brain";
pub const DEFAULT_PG_USER: &str = "brain";
pub const DEFAULT_CONTAINER: &str = "brain-postgres";
pub const DEFAULT_IMAGE: &str = "docker.io/pgvector/pgvector:pg18";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedConfig {
    /// `provider:model`, e.g. `local:nomic-embed-text`. `none` disables.
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_embed_url")]
    pub url: String,
    #[serde(default = "default_dims")]
    pub dims: usize,
    /// Bearer token for `url`. Empty/absent sends no `Authorization` header.
    ///
    /// Usually left out of here: `brain.yaml` is committed and a token is not,
    /// so prefer `BRAIN_EMBED_API_KEY` / `BRAIN_API_KEY` or
    /// `~/.config/brain/credentials.yaml`. See [`api_key`].
    #[serde(default, alias = "apiKey")]
    pub api_key: Option<String>,
}

fn default_embed_model() -> String {
    "none".into()
}
fn default_embed_url() -> String {
    "http://127.0.0.1:1234/v1/embeddings".into()
}
fn default_dims() -> usize {
    768
}

impl Default for EmbedConfig {
    fn default() -> Self {
        EmbedConfig {
            model: default_embed_model(),
            url: default_embed_url(),
            dims: default_dims(),
            api_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Chat/finalizer endpoint (OpenAI-compatible `/v1/chat/completions`).
    #[serde(default = "default_chat_url")]
    pub chat_url: String,
    #[serde(default = "default_model")]
    pub chat_model: String,
    /// The non-autoregressive decision slot used by `crawl` for typed questions.
    #[serde(default = "default_slot_url")]
    pub slot_url: String,
    /// Bearer token for both `chat_url` and `slot_url` — one provider usually
    /// serves both. See [`api_key`] for where else a token can come from.
    #[serde(default, alias = "apiKey")]
    pub api_key: Option<String>,
}

fn default_chat_url() -> String {
    std::env::var("BRAIN_CHAT_URL").unwrap_or_else(|_| "http://127.0.0.1:1234".into())
}
fn default_model() -> String {
    std::env::var("BRAIN_CHAT_MODEL").unwrap_or_else(|_| "default".into())
}
fn default_slot_url() -> String {
    std::env::var("SYSTEMONE_URL").unwrap_or_else(|_| "http://127.0.0.1:1237".into())
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            chat_url: default_chat_url(),
            chat_model: default_model(),
            slot_url: default_slot_url(),
            api_key: None,
        }
    }
}

/// The bearer token one of the model endpoints wants, or `None` to send no
/// `Authorization` header at all (every local server: llama.cpp, ollama, the
/// systemone box).
///
/// `service` is `"chat"`, `"slot"` or `"embed"`. Resolved in this order:
///
/// 1. the `api_key` in this brain's `brain.yaml`;
/// 2. `BRAIN_<SERVICE>_API_KEY`, e.g. `BRAIN_SLOT_API_KEY`;
/// 3. `BRAIN_API_KEY`, for a provider serving all three;
/// 4. `~/.config/brain/credentials.yaml`, by service, then its `default`.
///
/// The last one is the usual answer: `brain.yaml` is committed with the
/// knowledge base, a token is not.
///
/// ```yaml
/// # ~/.config/brain/credentials.yaml
/// default: mc-…            # used by any service without its own entry
/// embed:   mc-…
/// ```
pub fn api_key(service: &str, configured: Option<&str>) -> Option<String> {
    let usable = |v: &str| (!v.trim().is_empty()).then(|| v.to_string());
    if let Some(k) = configured.as_deref().and_then(usable) {
        return Some(k);
    }
    let per_service = format!("BRAIN_{}_API_KEY", service.to_uppercase());
    for var in [per_service.as_str(), "BRAIN_API_KEY"] {
        if let Some(k) = std::env::var(var).ok().as_deref().and_then(usable) {
            return Some(k);
        }
    }
    let creds = credentials();
    creds
        .get(service)
        .or_else(|| creds.get("default"))
        .and_then(|v| usable(v))
}

/// `~/.config/brain/credentials.yaml`, read once per process. Absent or
/// unparseable is simply empty — a brain with no provider credentials still
/// indexes, searches by keyword and crawls the graph; only the calls that ask
/// a model something fail, and they say so.
fn credentials() -> &'static BTreeMap<String, String> {
    static CREDS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    CREDS.get_or_init(|| {
        std::fs::read_to_string(config_dir().join("credentials.yaml"))
            .ok()
            .and_then(|s| serde_yaml::from_str(&s).ok())
            .unwrap_or_default()
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_db")]
    pub dbname: String,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    /// PostgreSQL schema that holds this brain's index. Defaults to the brain
    /// alias, sanitized. One brain, one schema, no collisions — ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

fn default_host() -> String {
    std::env::var("BRAIN_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}
fn default_port() -> u16 {
    std::env::var("BRAIN_PG_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PG_PORT)
}
fn default_db() -> String {
    std::env::var("BRAIN_PG_DB").unwrap_or_else(|_| DEFAULT_PG_DB.into())
}
fn default_user() -> String {
    std::env::var("BRAIN_PG_USER").unwrap_or_else(|_| DEFAULT_PG_USER.into())
}

impl Default for PgConfig {
    fn default() -> Self {
        PgConfig {
            host: default_host(),
            port: default_port(),
            dbname: default_db(),
            user: default_user(),
            password: std::env::var("BRAIN_PG_PASSWORD").ok(),
            schema: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    #[serde(default)]
    pub embed: EmbedConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub pg: PgConfig,
    /// Extra storage dirs unioned into the runtime graph (per-repo knowledge
    /// bases, ACL splits). `./db` is always first.
    #[serde(default)]
    pub storage: Vec<String>,
    #[serde(default = "default_refine_passes")]
    pub refine_max_passes: usize,
}

fn default_refine_passes() -> usize {
    4
}

/// A fully resolved brain: where its files are, what it is called, how to
/// reach its index.
#[derive(Debug, Clone)]
pub struct Brain {
    pub alias: String,
    pub project_root: PathBuf,
    pub db_root: PathBuf,
    pub config: BrainConfig,
    pub pg_schema: String,
}

impl Brain {
    pub fn schema_path(&self) -> PathBuf {
        self.db_root.join("schema.yaml")
    }
    /// Every storage root, `db/` first.
    pub fn storage_roots(&self) -> Vec<PathBuf> {
        let mut out = vec![self.db_root.clone()];
        for s in &self.config.storage {
            let p = expand(s);
            let p = if p.is_absolute() { p } else { self.project_root.join(p) };
            if p != self.db_root && !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }
    pub fn socket_path(&self) -> PathBuf {
        runtime_dir().join(format!("{}.sock", sanitize_ident(&self.alias)))
    }
    pub fn pid_path(&self) -> PathBuf {
        runtime_dir().join(format!("{}.pid", sanitize_ident(&self.alias)))
    }
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let d = base.join("brain");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("brain")
}

pub fn brains_config_path() -> PathBuf {
    config_dir().join("brains.yaml")
}

pub fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn expand(s: &str) -> PathBuf {
    let s = s.trim();
    if s == "~" {
        return home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(s)
}

/// A PostgreSQL identifier that cannot need quoting: lowercase, alnum + `_`.
pub fn sanitize_ident(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        return "brain".into();
    }
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        format!("b_{out}")
    } else {
        out
    }
}

// ── brains.yaml ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct BrainsFile {
    pub entries: BTreeMap<String, String>,
    pub current: Option<String>,
}

pub fn load_brains() -> Result<BrainsFile> {
    let p = brains_config_path();
    let mut out = BrainsFile::default();
    if !p.exists() {
        return Ok(out);
    }
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap_or(serde_yaml::Value::Null);
    let serde_yaml::Value::Mapping(m) = doc else {
        if text.trim().is_empty() {
            return Ok(out);
        }
        bail!("brain aliases file must contain a YAML mapping: {}", p.display());
    };
    for (k, v) in m {
        let (Some(k), Some(v)) = (k.as_str(), v.as_str()) else { continue };
        if k == "current" {
            out.current = if v == "none" { None } else { Some(v.to_string()) };
        } else {
            out.entries.insert(k.to_string(), v.to_string());
        }
    }
    Ok(out)
}

pub fn save_brains(b: &BrainsFile) -> Result<()> {
    let p = brains_config_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut s = String::new();
    for (k, v) in &b.entries {
        s.push_str(&format!("{k}: {v}\n"));
    }
    s.push_str(&format!("current: {}\n", b.current.clone().unwrap_or_else(|| "none".into())));
    std::fs::write(&p, s).with_context(|| format!("writing {}", p.display()))?;
    Ok(())
}

pub fn resolve_alias_path(value: &str) -> PathBuf {
    let p = expand(value);
    if p.is_absolute() {
        p
    } else {
        brains_config_path().parent().unwrap_or(Path::new("/")).join(p)
    }
}

pub fn alias_from_project(root: &Path) -> String {
    let base = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "brain".into());
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() || cleaned == "current" {
        "brain".into()
    } else {
        cleaned
    }
}

/// Register a project root (a directory that contains `db/`) under an alias.
/// Never touches `current`.
pub fn register_project(root: &Path, overwrite: bool) -> Result<String> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if !root.join("db").exists() {
        bail!("not a brain project (no db/): {}", root.display());
    }
    let mut brains = load_brains()?;
    let base = alias_from_project(&root);
    if !overwrite {
        for (name, path) in &brains.entries {
            if resolve_alias_path(path).canonicalize().unwrap_or_else(|_| resolve_alias_path(path)) == root {
                return Ok(name.clone());
            }
        }
        let mut alias = base.clone();
        let mut n = 2;
        while brains.entries.contains_key(&alias) {
            alias = format!("{base}-{n}");
            n += 1;
        }
        brains.entries.insert(alias.clone(), root.display().to_string());
        save_brains(&brains)?;
        return Ok(alias);
    }
    brains.entries.insert(base.clone(), root.display().to_string());
    save_brains(&brains)?;
    Ok(base)
}

// ── resolution ───────────────────────────────────────────────────────────────

static USE_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();

pub fn set_use_override(v: Option<String>) {
    let _ = USE_OVERRIDE.set(v);
}

fn use_override() -> Option<String> {
    USE_OVERRIDE.get().cloned().flatten()
}

fn load_brain_config(project_root: &Path) -> BrainConfig {
    let p = project_root.join("brain.yaml");
    if !p.exists() {
        return BrainConfig::default();
    }
    let Ok(text) = std::fs::read_to_string(&p) else { return BrainConfig::default() };
    // Tolerate brain1's shape: `refine: {maxPasses: N}` and `storage: [..]`.
    let doc: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(d) => d,
        Err(_) => return BrainConfig::default(),
    };
    let mut cfg: BrainConfig = serde_yaml::from_value(doc.clone()).unwrap_or_default();
    if let Some(m) = doc.as_mapping() {
        if let Some(r) = m.get(serde_yaml::Value::from("refine")).and_then(|v| v.as_mapping()) {
            for key in ["maxPasses", "max_passes"] {
                if let Some(n) = r.get(serde_yaml::Value::from(key)).and_then(|v| v.as_u64()) {
                    cfg.refine_max_passes = n as usize;
                }
            }
        }
    }
    cfg = with_env_overrides(cfg);
    cfg
}

/// Deployment-time overrides: the environment wins over `brain.yaml`, which wins
/// over the built-in defaults.
///
/// Most of these were already half-implemented. `BRAIN_CHAT_URL`,
/// `BRAIN_CHAT_MODEL`, `SYSTEMONE_URL` and every `BRAIN_PG_*` were reachable only
/// as `#[serde(default = ...)]` field defaults, which fire when `brain.yaml`
/// *omits* the key and are silently shadowed by one that sets it. dstwiki's
/// committed file sets all of them, so a container that configured its endpoints
/// through the platform's env block kept dialing a laptop: `127.0.0.1:5433` for
/// the database, and `127.0.0.1:1234` / `127.0.0.1:1237` for chat and the
/// decision slot. Measured in a local cluster 2026-11-08: a crawl reported
/// `calls: 4, ms: 120, path: []` because all four model calls were instant
/// `ECONNREFUSED`s against the pod's own loopback, while the same query on the
/// laptop took 2.2 s and answered.
///
/// An override that a committed file can defeat is not an override.
///
/// # The exception, and why it stays
///
/// `api_key` keeps its own documented order — `brain.yaml` first, then
/// `BRAIN_<SERVICE>_API_KEY`, then `BRAIN_API_KEY`, then `credentials.yaml` (see
/// [`api_key`]). A URL is infrastructure: which server answers is a fact about
/// the deployment, and the same corpus has to be servable from a laptop, a pod,
/// and a managed instance without anyone editing a file that belongs to the data.
/// A token the corpus author committed is an assertion about the corpus.
/// Silently preferring whatever the pod was handed would turn a misconfigured
/// secret into an authentication failure against a provider nobody chose, which
/// is harder to diagnose than the loud, local decision it overrode.
///
/// # Every applied override is announced
///
/// Pointing a brain's reasoning at a different model is significant enough that
/// it belongs in the log rather than in a diff nobody reads. Values are printed
/// for URLs, models and ports; credential-bearing variables are reported as set
/// and their contents never reach stderr.
fn with_env_overrides(mut cfg: BrainConfig) -> BrainConfig {
    /// Announce an override once per process. `load_brain_config` runs more than
    /// once for a single command (the `exists()` gate, then `Ctx::open`), and a
    /// line that repeats on every invocation is a line that gets skimmed out of
    /// existence. Parse warnings are deliberately NOT deduplicated: those are
    /// errors about the caller's own input, and they should be impossible to miss.
    fn announce(what: String) {
        use std::collections::HashSet;
        use std::sync::Mutex;
        use std::sync::OnceLock;
        static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
        if let Ok(mut g) = seen.lock() {
            if g.insert(what.clone()) {
                eprintln!("config: {what}");
            }
        }
    }

    /// Apply one string override, announcing it only when it actually changes
    /// something — an env var that agrees with the file is not news.
    fn over(slot: &mut String, name: &str, value: &str) {
        if value != slot.as_str() {
            announce(format!("{name} overrides brain.yaml: {slot} -> {value}"));
            *slot = value.to_string();
        }
    }
    fn first_of<'a>(names: &[&'a str]) -> Option<(&'a str, String)> {
        names.iter().filter_map(|n| env_nonempty(n).map(|v| (*n, v))).next()
    }

    // ---- database: where the index lives --------------------------------
    if let Some((n, v)) = first_of(&["BRAIN_PG_HOST"]) {
        over(&mut cfg.pg.host, n, &v);
    }
    if let Some((n, v)) = first_of(&["BRAIN_PG_DB"]) {
        over(&mut cfg.pg.dbname, n, &v);
    }
    if let Some((n, v)) = first_of(&["BRAIN_PG_USER"]) {
        over(&mut cfg.pg.user, n, &v);
    }
    if let Some(v) = env_nonempty("BRAIN_PG_PORT") {
        match v.parse::<u16>() {
            Ok(n) if n != cfg.pg.port => {
                announce(format!(
                    "BRAIN_PG_PORT overrides brain.yaml: {} -> {}",
                    cfg.pg.port, n
                ));
                cfg.pg.port = n;
            }
            Ok(_) => {}
            // Loud beats quiet: a typo'd port that fell back to the file's value
            // would reconnect to the wrong server and read as a mystery.
            Err(e) => eprintln!("warning: ignoring BRAIN_PG_PORT='{v}' ({e}); keeping {}", cfg.pg.port),
        }
    }
    if env_nonempty("BRAIN_PG_PASSWORD").is_some() {
        // Never printed: a password in a container log is a credential leak with
        // a retention policy attached.
        cfg.pg.password = env_nonempty("BRAIN_PG_PASSWORD");
        announce("BRAIN_PG_PASSWORD set from environment".to_string());
    }

    // ---- model endpoints: which server answers --------------------------
    if let Some((n, v)) = first_of(&["BRAIN_CHAT_URL"]) {
        over(&mut cfg.llm.chat_url, n, &v);
    }
    if let Some((n, v)) = first_of(&["BRAIN_CHAT_MODEL"]) {
        over(&mut cfg.llm.chat_model, n, &v);
    }
    // BRAIN_SLOT_URL is the honest name for the slot endpoint; SYSTEMONE_URL is
    // what the code has always honoured, so it stays as a legacy alias rather
    // than becoming a breaking rename nobody asked for.
    if let Some((n, v)) = first_of(&["BRAIN_SLOT_URL", "SYSTEMONE_URL"]) {
        over(&mut cfg.llm.slot_url, n, &v);
    }
    if let Some((n, v)) = first_of(&["BRAIN_EMBED_URL"]) {
        over(&mut cfg.embed.url, n, &v);
    }
    if let Some((n, v)) = first_of(&["BRAIN_EMBED_MODEL"]) {
        over(&mut cfg.embed.model, n, &v);
    }
    if let Some(v) = env_nonempty("BRAIN_EMBED_DIMS") {
        match v.parse::<usize>() {
            // dims travels with the embedder: overriding the model without the
            // dimension would silently mismatch the pgvector column, so it gets
            // the same treatment rather than being left to the file.
            Ok(n) if n != cfg.embed.dims => {
                announce(format!(
                    "BRAIN_EMBED_DIMS overrides brain.yaml: {} -> {}",
                    cfg.embed.dims, n
                ));
                cfg.embed.dims = n;
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: ignoring BRAIN_EMBED_DIMS='{v}' ({e}); keeping {}", cfg.embed.dims),
        }
    }

    cfg
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Walk up from `start` looking for a directory with a `db/` child.
fn find_project_upward(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start.to_path_buf());
    while let Some(d) = cur {
        if d.join("db").join("schema.yaml").exists() || d.join("db").is_dir() {
            return Some(d);
        }
        cur = d.parent().map(|p| p.to_path_buf());
    }
    None
}

/// Resolve the active brain: `--db` wins, then `--use`, then `$BRAIN_ROOT`,
/// then the saved `current`, then the cwd (walking up).
pub fn resolve(db_override: Option<&str>) -> Result<Brain> {
    if let Some(d) = db_override {
        let p = expand(d);
        let p = p.canonicalize().unwrap_or(p);
        // accept either the project root or the db/ dir itself
        let (project_root, db_root) = if p.join("db").is_dir() {
            (p.clone(), p.join("db"))
        } else {
            (p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| p.clone()), p.clone())
        };
        return Ok(build(alias_from_project(&project_root), project_root, db_root));
    }
    let brains = load_brains()?;
    let want = use_override().or_else(|| brains.current.clone());
    if let Some(alias) = want {
        if alias == "none" {
            // explicit cwd-local
        } else if let Some(path) = brains.entries.get(&alias) {
            let root = resolve_alias_path(path);
            let db = root.join("db");
            if !db.is_dir() {
                bail!("brain '{alias}' points at {} which has no db/", root.display());
            }
            return Ok(build(alias, root, db));
        } else {
            bail!("unknown brain alias '{alias}' (see `brain use`)");
        }
    }
    if let Ok(r) = std::env::var("BRAIN_ROOT") {
        let db = expand(&r);
        let root = db.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| db.clone());
        return Ok(build(alias_from_project(&root), root, db));
    }
    let cwd = std::env::current_dir()?;
    match find_project_upward(&cwd) {
        Some(root) => {
            let db = root.join("db");
            Ok(build(alias_from_project(&root), root, db))
        }
        None => bail!("no brain database found"),
    }
}

fn build(alias: String, project_root: PathBuf, db_root: PathBuf) -> Brain {
    let config = load_brain_config(&project_root);
    let pg_schema = config
        .pg
        .schema
        .clone()
        .map(|s| sanitize_ident(&s))
        .unwrap_or_else(|| sanitize_ident(&alias));
    Brain { alias, project_root, db_root, config, pg_schema }
}

pub fn exists(db_override: Option<&str>) -> bool {
    resolve(db_override).map(|b| b.db_root.is_dir()).unwrap_or(false)
}
