//! corpus.rs — where the `.md` files come from, and how they are refreshed.
//!
//! `db/**.md` is the authoritative artifact and PostgreSQL is a derived index
//! over it (README: "the ONE .md -> index path" is `reindex`). That is what
//! makes a corpus *fetchable* rather than *state*: the container shallow-clones
//! it at boot, a webhook can ask for the latest commits without a restart, and
//! nothing needs preserving between the two because the index is disposable by
//! construction — drop it and reindex from disk.
//!
//! One file says where corpora come from (`corpora.yaml`), one command
//! materialises and refreshes them (`brain corpus pull`), and both servers reuse
//! that command for `POST /api/corpus/refresh` — so boot and the webhook cannot
//! drift into two different notions of "current".
//!
//! ## Why git rather than a baked image
//!
//! A baked corpus ties the served data to an image tag, which sounds tidy and
//! isn't: the image becomes a function of two repositories, so a tag naming one
//! of them is a provenance lie, and every corpus edit pays a cross-compile
//! release. A shallow clone costs one `fetch --depth 1` for the delta, and it
//! leaves provenance answerable *at runtime* (`git rev-parse`), which is what
//! `/api/brains` reports per database.
//!
//! ## What a refresh must not do
//!
//! It must not half-succeed silently. A *fresh clone* failure is fatal — a wiki
//! that answers 200 with no pages is worse than one that crash-loops and says
//! why — while a *refresh* failure falls back to the corpus already on disk and
//! says so, because stale service beats no service. That distinction is the
//! whole policy, and it lives here rather than in the entrypoint shell so the
//! webhook inherits it instead of re-deciding it.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::{Args, Global};
use crate::config::{self, Brain};
use crate::index;
use crate::llm::Embedder;
use crate::store::Store;
use crate::tbox::TBox;

/// How long one `git` invocation may take. A clone of a coordination repo is
/// seconds; a host that accepts the connection and then says nothing must not
/// pin the pod's init container (or a webhook request) forever.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

// ── configuration ───────────────────────────────────────────────────────────

/// One entry of `corpora.yaml`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Spec {
    pub repo: String,
    /// Branch or tag to track (`main` by default).
    #[serde(default = "default_ref", rename = "ref")]
    pub git_ref: String,
    /// Where the brain lives inside the repository: `.brain` for a coordination
    /// repo, unset for a repo that *is* a brain. This is the one fact the old
    /// single-repo clone could not express — it required `db/` at the repo root,
    /// so a coordination repo with its brain in a subdirectory never worked.
    #[serde(default)]
    pub subdir: Option<String>,
    /// Checkout directory; defaults to `<root>/<alias>`.
    #[serde(default)]
    pub dest: Option<String>,
}

fn default_ref() -> String {
    "main".to_string()
}

/// The whole file.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FileDoc {
    /// Which corpus a bare `/` lands on, and what a `--db`-less command reads.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub corpora: BTreeMap<String, Spec>,
}

/// One resolved corpus: what to clone, where it lands, where the brain is.
#[derive(Debug, Clone)]
pub struct Corpus {
    pub alias: String,
    pub repo: String,
    pub git_ref: String,
    /// Empty = the repository root is the brain.
    pub subdir: String,
    pub dest: PathBuf,
}

impl Corpus {
    /// The directory `brain --db` should be pointed at.
    pub fn brain_dir(&self) -> PathBuf {
        if self.subdir.is_empty() {
            self.dest.clone()
        } else {
            self.dest.join(&self.subdir)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Corpora {
    pub file: PathBuf,
    pub root: PathBuf,
    pub default: Option<String>,
    pub entries: Vec<Corpus>,
}

impl Corpora {
    /// Read and resolve a `corpora.yaml`.
    ///
    /// `root` is where aliases without an explicit `dest:` are checked out; it
    /// defaults to `corpora/` beside the file, which keeps a checkout
    /// self-contained and makes `--dest` the only thing a container has to pass.
    pub fn load(file: &Path, root: Option<&Path>) -> Result<Corpora> {
        let file = config::expand(&file.to_string_lossy());
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let doc: FileDoc = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing {}", file.display()))?;
        if doc.corpora.is_empty() {
            bail!("{} lists no corpora", file.display());
        }
        let root = match root {
            Some(r) => config::expand(&r.to_string_lossy()),
            None => file.parent().unwrap_or_else(|| Path::new(".")).join("corpora"),
        };

        let mut entries: Vec<Corpus> = Vec::new();
        for (alias, spec) in doc.corpora {
            let dest = match spec.dest.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                Some(d) => {
                    let p = config::expand(d);
                    if p.is_absolute() {
                        p
                    } else {
                        root.join(p)
                    }
                }
                None => root.join(&alias),
            };
            let subdir = spec
                .subdir
                .as_deref()
                .map(str::trim)
                .map(|s| s.trim_start_matches("./").trim_matches('/').to_string())
                // `None` and `.` both mean "the repo root is the brain".
                .map(|s| if s == "." { String::new() } else { s })
                .unwrap_or_default();
            entries.push(Corpus {
                alias,
                repo: spec.repo.trim().to_string(),
                git_ref: spec.git_ref.trim().to_string(),
                subdir,
                dest,
            });
        }
        if let Some(d) = &doc.default {
            if !entries.iter().any(|c| &c.alias == d) {
                bail!(
                    "{} names default: {d}, which is not one of its corpora ({})",
                    file.display(),
                    entries.iter().map(|c| c.alias.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }
        Ok(Corpora { file, root, default: doc.default, entries })
    }

    pub fn get(&self, alias: &str) -> Option<&Corpus> {
        self.entries.iter().find(|c| c.alias == alias)
    }

    /// The selected corpora: everything, or the named aliases.
    fn select(&self, only: &[String]) -> Result<Vec<&Corpus>> {
        if only.is_empty() {
            return Ok(self.entries.iter().collect());
        }
        let mut out = Vec::new();
        for alias in only {
            match self.get(alias) {
                Some(c) => out.push(c),
                None => bail!(
                    "no corpus named '{alias}' in {} (have: {})",
                    self.file.display(),
                    self.entries.iter().map(|c| c.alias.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
        Ok(out)
    }
}

/// Which corpora file this invocation should use: `--corpora`, then
/// `$BRAIN_CORPORA`, then nothing.
///
/// "Nothing" is a real answer, and the common one on a laptop: a developer
/// already has the checkout they are editing, and cloning it out from under
/// themselves would be a bug, not a convenience.
pub fn discover(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(config::expand(p));
    }
    std::env::var("BRAIN_CORPORA")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .map(|p| config::expand(&p))
}

/// `--corpora <path>` / `--corpora=path` from a command's own argv.
///
/// Read before each transport's flag parser, the same way `brains::explicit`
/// reads `--brain`: the transports accept their own flags, and the one thing
/// they must agree on is where the corpora come from.
pub fn flag(argv: &[String]) -> Option<String> {
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--corpora" {
            return argv.get(i + 1).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        }
        if let Some(v) = a.strip_prefix("--corpora=") {
            let v = v.trim().to_string();
            return (!v.is_empty()).then_some(v);
        }
        i += 1;
    }
    None
}

/// The corpora a server should serve: `--corpora`, else `$BRAIN_CORPORA`, else
/// `None` (serve whatever `--db`/registry resolves).
pub fn from_argv(argv: &[String]) -> Option<Result<Corpora>> {
    let path = discover(flag(argv).as_deref())?;
    let root = dest_root(None);
    Some(Corpora::load(&path, root.as_deref()))
}

/// `--dest`, then `$BRAIN_CORPORA_DIR`.
pub fn dest_root(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(config::expand(p));
    }
    std::env::var("BRAIN_CORPORA_DIR")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .map(|p| config::expand(&p))
}

// ── the pull ────────────────────────────────────────────────────────────────

/// What happened to one corpus. One struct for the log, the CLI's `--json` and
/// the webhook's response, so all three say the same thing.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub db: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Short SHA before this pull ("" when there was no checkout).
    pub from: String,
    /// Short SHA after this pull ("" when the pull did not happen).
    pub to: String,
    pub changed: bool,
    /// cloned | updated | unchanged | stale | failed
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reindexed: Option<Reindexed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reindexed {
    pub entities: i64,
    pub links: i64,
    pub files: usize,
    pub ms: u128,
}

impl Report {
    fn new(c: &Corpus) -> Report {
        Report {
            db: c.alias.clone(),
            repo: c.repo.clone(),
            git_ref: c.git_ref.clone(),
            from: String::new(),
            to: String::new(),
            changed: false,
            status: "unchanged",
            reindexed: None,
            warning: None,
            error: None,
        }
    }

    fn failed(c: &Corpus, e: impl std::fmt::Display) -> Report {
        Report { status: "failed", error: Some(e.to_string()), ..Report::new(c) }
    }
}

/// Materialise (or refresh) every selected corpus, and reindex the ones that
/// moved.
///
/// Per-corpus failures are reported, not propagated: one unreachable repository
/// must not stop the others from refreshing, and a webhook that dies on the
/// first error tells the caller nothing about the rest. The caller decides what
/// a failure means (the CLI exits 1; the webhook returns 207-ish JSON with
/// `ok: false`).
pub async fn pull(
    corpora: &Corpora,
    only: &[String],
    reindex: bool,
    quiet: bool,
) -> Result<Vec<Report>> {
    let selected = corpora.select(only)?;
    let mut out = Vec::with_capacity(selected.len());
    for c in selected {
        let mut report = Report::new(c);
        let brain_dir = c.brain_dir();

        // ── the checkout ────────────────────────────────────────────────────
        let existing = c.dest.join(".git").exists();
        if existing {
            report.from = head(&c.dest);
            match refresh_checkout(c).await {
                Ok(sha) => {
                    report.to = sha;
                    report.changed = report.from != report.to;
                    report.status = if report.changed { "updated" } else { "unchanged" };
                }
                Err(e) => {
                    // Stale beats absent: the corpus on disk still describes a
                    // real, known revision, and the pod keeps serving it.
                    report.status = "stale";
                    report.warning = Some(format!("refresh failed; serving the checkout on disk ({e})"));
                }
            }
        } else {
            match clone_checkout(c).await {
                Ok(sha) => {
                    report.to = sha;
                    report.changed = true;
                    report.status = "cloned";
                }
                Err(e) => {
                    out.push(Report::failed(c, e));
                    continue;
                }
            }
        }

        // ── is what landed actually a brain? ────────────────────────────────
        if !brain_dir.join("db").join("schema.yaml").is_file() {
            let mut failed = if report.status == "cloned" {
                Report::failed(c, format!(
                    "{} has no db/schema.yaml at {} — not a brain repository (check `subdir:`)",
                    c.repo,
                    if c.subdir.is_empty() { "its root".to_string() } else { c.subdir.clone() }
                ))
            } else {
                report.clone()
            };
            failed.status = "failed";
            failed.to.clear();
            out.push(failed);
            continue;
        }

        // ── is what landed *sane*? ──────────────────────────────────────────
        //
        // `reindex` will happily index a wrecked entity: a file written as one
        // physical line with literal backslash-n text has no closing `---`, so
        // the parser treats the whole file as body prose and the entity still
        // counts (that defect shipped once, in a pod that reported 86 entities
        // and looked healthy). The linter is the only thing that notices, and it
        // costs milliseconds — so a corpus that fails it does not get indexed,
        // and whatever index is already being served keeps serving.
        let brain = match config::from_root_named(&brain_dir, Some(&c.alias)) {
            Ok(b) => b,
            Err(e) => {
                report.status = "failed";
                report.error = Some(e.to_string());
                out.push(report);
                continue;
            }
        };
        let tbox = match TBox::load(&brain.schema_path()) {
            Ok(t) => t,
            Err(e) => {
                report.status = "failed";
                report.error = Some(format!("schema.yaml: {e}"));
                out.push(report);
                continue;
            }
        };
        match index::validate(&brain, &tbox) {
            Ok(findings) => {
                let errors: Vec<&index::Finding> = findings
                    .iter()
                    .filter(|f| f.severity == index::Severity::Error)
                    .collect();
                if let Some(first) = errors.first() {
                    report.status = "failed";
                    report.error = Some(format!(
                        "{} validation error(s), first: {first}",
                        errors.len()
                    ));
                    out.push(report);
                    continue;
                }
                let warnings = findings
                    .iter()
                    .filter(|f| f.severity == index::Severity::Warning)
                    .count();
                if warnings > 0 {
                    report.warning = Some(format!("{warnings} validation warning(s)"));
                }
            }
            Err(e) => {
                report.status = "failed";
                report.error = Some(format!("validation failed: {e}"));
                out.push(report);
                continue;
            }
        }

        // ── the index ───────────────────────────────────────────────────────
        if reindex && report.changed {
            match reindex_one(&brain, &tbox).await {
                Ok(r) => {
                    if !quiet {
                        eprintln!(
                            "  reindexed {} entities / {} edges in {}ms",
                            r.entities, r.links, r.ms
                        );
                    }
                    report.reindexed = Some(r);
                }
                Err(e) => {
                    // The corpus is on disk and correct; only its index is not.
                    // Say so and keep the previous index rather than failing the
                    // whole pull — the next boot retries from scratch.
                    report.status = "stale";
                    report.warning = Some(format!(
                        "reindex failed; the previous index is still being served ({e})"
                    ));
                }
            }
        }
        out.push(report);
    }
    Ok(out)
}

async fn clone_checkout(c: &Corpus) -> Result<String> {
    // Clone into a sibling temp dir and move it in, so an interrupted clone
    // never leaves a half-populated `db/` that the next boot mistakes for a
    // corpus (the same rule the entrypoint shell used, kept for the same reason).
    let parent = c.dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.part-{}",
        c.dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "corpus".into()),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let tmp_s = tmp.to_string_lossy().to_string();
    let result = git(
        None,
        &["clone", "--depth", "1", "--branch", &c.git_ref, &c.repo, &tmp_s],
    )
    .await;
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if c.dest.exists() {
        // A directory with no .git (an interrupted copy, a hand-placed corpus)
        // is not something to merge into: replace it, but only after the clone
        // that is going to replace it has succeeded.
        std::fs::remove_dir_all(&c.dest)
            .with_context(|| format!("replacing {}", c.dest.display()))?;
    }
    std::fs::rename(&tmp, &c.dest)
        .with_context(|| format!("moving clone into {}", c.dest.display()))?;
    Ok(head(&c.dest))
}

async fn refresh_checkout(c: &Corpus) -> Result<String> {
    let dir = c.dest.as_path();
    // `--depth 1` on the fetch keeps the delta small: only the new commits, not
    // the history, which is the entire reason a refresh can be a sub-second
    // operation on a repository that took minutes to clone.
    git(Some(dir), &["fetch", "--depth", "1", "origin", &c.git_ref]).await?;
    git(Some(dir), &["reset", "--hard", "FETCH_HEAD"]).await?;
    Ok(head(dir))
}

async fn reindex_one(brain: &Brain, tbox: &TBox) -> Result<Reindexed> {
    // `embed.model: none` (the production corpora) contacts no provider, so the
    // reindex is keyword + graph work only: no round-trips, nothing to wait on.
    let mut embedder = Embedder::new(&brain.config.embed);
    let mut dims = brain.config.embed.dims;
    if let Some(e) = &embedder {
        match e.probe_dims().await {
            Ok(d) => dims = d,
            Err(err) => {
                eprintln!("warn: embedding provider unreachable ({err}) — continuing without vectors");
                embedder = None;
            }
        }
    }
    let store = Store::connect(&brain.config.pg, &brain.pg_schema, dims).await?;
    store.ensure_schema().await?;
    let r = index::reindex(brain, tbox, &store, embedder.as_ref(), true).await?;
    index::record_disk_fingerprint(brain, &store).await?;
    Ok(Reindexed {
        entities: r.entities as i64,
        links: r.links as i64,
        files: r.files,
        ms: r.total_ms,
    })
}

// ── git ─────────────────────────────────────────────────────────────────────

async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    // Never stop on a prompt: a webhook has no terminal, and a `git` that waits
    // for credentials holds a request (or an init container) open until the
    // timeout. A missing credential should be an error with git's own words in it.
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_PAGER", "cat");
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {} timed out after {}s", args.join(" "), GIT_TIMEOUT.as_secs()))?
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {} failed: {}", args.join(" "), first_line(&err));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The checkout's short SHA, or `""` when there is no git checkout there.
///
/// Synchronous on purpose: the servers read it while assembling their brain set
/// (before the first request), where a subprocess per corpus is cheaper than the
/// machinery to make it async.
pub fn head(dir: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

// ── CLI ─────────────────────────────────────────────────────────────────────

pub async fn run(argv: &[String], _g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["file", "dest", "alias"]);
    let sub = a.positional.first().map(String::as_str).unwrap_or("pull");
    let file = match discover(a.get("file")) {
        Some(p) => p,
        None => bail!("no corpora file — pass `--file <path>` or set BRAIN_CORPORA"),
    };
    let corpora = Corpora::load(&file, dest_root(a.get("dest")).as_deref())?;

    match sub {
        "list" => {
            let only = a.get_all("alias");
            let selected = corpora.select(&only)?;
            if a.has("json") {
                let rows: Vec<Value> = selected
                    .iter()
                    .map(|c| {
                        json!({
                            "db": c.alias,
                            "repo": c.repo,
                            "ref": c.git_ref,
                            "subdir": c.subdir,
                            "dest": c.dest.display().to_string(),
                            "brain": c.brain_dir().display().to_string(),
                            "sha": head(&c.dest),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json!({
                    "file": corpora.file.display().to_string(),
                    "root": corpora.root.display().to_string(),
                    "default": corpora.default,
                    "corpora": rows,
                }))?);
                return Ok(0);
            }
            println!("{}  (root {})", corpora.file.display(), corpora.root.display());
            for c in selected {
                let sha = head(&c.dest);
                let mark = if corpora.default.as_deref() == Some(c.alias.as_str()) { "*" } else { " " };
                println!(
                    "{mark} {:<8} {:<6} {:<8} {}{}",
                    c.alias,
                    c.git_ref,
                    if sha.is_empty() { "—" } else { &sha },
                    c.dest.display(),
                    if c.subdir.is_empty() { String::new() } else { format!("  (brain: {})", c.subdir) },
                );
            }
            Ok(0)
        }
        "pull" => {
            let only = a.get_all("alias");
            let reindex = !a.has("no-reindex");
            let quiet = a.has("quiet") || a.has("json");
            if !quiet {
                eprintln!("pull: {} corpus/corpora from {}", corpora.entries.len(), corpora.file.display());
            }
            let reports = pull(&corpora, &only, reindex, quiet).await?;
            if a.has("json") {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                for r in &reports {
                    human_line(r);
                }
            }
            Ok(if reports.iter().any(|r| r.status == "failed") { 1 } else { 0 })
        }
        other => bail!("unknown `corpus` subcommand: {other} (want pull | list)"),
    }
}

fn human_line(r: &Report) {
    let movement = match (r.from.is_empty(), r.to.is_empty()) {
        (_, true) => r.from.clone(),
        (true, false) => format!("-> {}", r.to),
        (false, false) if r.changed => format!("{} -> {}", r.from, r.to),
        _ => r.from.clone(),
    };
    match &r.reindexed {
        Some(x) => println!(
            "{:<8} {:<6} {:<22} reindexed {} entities / {} edges in {}ms",
            r.db, r.git_ref, movement, x.entities, x.links, x.ms
        ),
        None => println!("{:<8} {:<6} {:<22} {}", r.db, r.git_ref, movement, r.status),
    }
    if let Some(w) = &r.warning {
        println!("{:<8} {:<6} {:<22} warning: {w}", "", "", "");
    }
    if let Some(e) = &r.error {
        println!("{:<8} {:<6} {:<22} error: {e}", "", "", "");
    }
}
