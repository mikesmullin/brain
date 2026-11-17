//! brains.rs — the set of brains one process serves.
//!
//! Every command but `web`/`serve` acts on exactly one database, chosen by
//! `--db` / `--use` / the working directory. The wiki is the exception: it can
//! switch between databases, so one process owns a *set* of them, each with its
//! own PostgreSQL schema, and the URL says which one a page is about
//! (`/alpha/e/Person/jdoe`, `/beta/`, …).
//!
//! Where the set comes from, in order:
//!
//! 1. `--corpora <file>` (or `$BRAIN_CORPORA`): a `corpora.yaml` naming the
//!    repositories to serve. This *replaces* the registry below, because a
//!    container has no `~/.config/brain/brains.yaml` and must not accidentally
//!    inherit the host's inventory — and because the same file is what the
//!    webhook pulls from, so the served set and the refreshable set are one fact;
//! 2. the brain `--db` / `--use` / `$BRAIN_ROOT` / the cwd resolves to — the
//!    default, and the one the URL falls back to;
//! 3. `--brain <alias>=<path>`, repeatable, for a run that names its brains
//!    explicitly (a container, a demo, a checkout that is not registered);
//! 4. every brain in `~/.config/brain/brains.yaml`, so the machine's own
//!    inventory shows up in the dropdown without a flag.
//!
//! One brain, many checkouts: all three coord repos keep their brain in
//! `.brain/`, so a directory-derived alias would name them all "brain" — and
//! therefore give them all the same schema. That is why a brain may name itself
//! (`brain.yaml: alias:`), and why the registry key wins over the directory.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::cli::Global;
use crate::config::{self, Brain};
use crate::corpus::Corpora;

pub struct Brains {
    /// The default brain first, then the rest by alias.
    pub list: Vec<Brain>,
    pub default: String,
}

/// `--brain alias=path`, repeatable, as either `--brain a=b` or `--brain=a=b`.
///
/// Both transports call this with their own argv before parsing their flags:
/// the transports accept some flags themselves (`--port`, `--web`), and the one
/// thing they must agree on is how a brain is named.
pub fn explicit(argv: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let spec = if arg == "--brain" {
            i += 1;
            argv.get(i).cloned()
        } else {
            arg.strip_prefix("--brain=").map(str::to_string)
        };
        if let Some(spec) = spec {
            match spec.split_once('=') {
                Some((alias, path)) if !alias.trim().is_empty() && !path.trim().is_empty() => {
                    out.push((alias.trim().to_string(), path.trim().to_string()));
                }
                _ => eprintln!("warning: ignoring --brain '{spec}' (want alias=path)"),
            }
        }
        i += 1;
    }
    out
}

/// Resolve every brain this process will serve.
///
/// A registry entry that does not resolve (a checkout that moved, a path with no
/// `db/`) is a warning rather than a failure: one stale line in `brains.yaml`
/// must not stop the dropdown from offering the brains that do work.
pub fn discover(
    global: &Global,
    explicit: &[(String, String)],
    corpora: Option<&Corpora>,
) -> Result<Brains> {
    let mut list: Vec<Brain> = Vec::new();
    let mut paths: BTreeMap<PathBuf, String> = BTreeMap::new();

    /// Add one brain, refusing only the case that cannot be resolved by
    /// precedence: two *different* databases claiming the same alias.
    fn push(list: &mut Vec<Brain>, paths: &mut BTreeMap<PathBuf, String>, brain: Brain) -> Result<()> {
        match paths.get(&brain.db_root) {
            Some(have) => {
                // The same database twice (registry + `--db`): keep the entry
                // already here, which is the higher-precedence one.
                if have != &brain.alias {
                    eprintln!(
                        "note: {} is both '{have}' and '{}' — keeping '{have}'",
                        brain.db_root.display(),
                        brain.alias
                    );
                }
                return Ok(());
            }
            None => {}
        }
        if let Some(other) = list.iter().find(|b| b.alias == brain.alias) {
            bail!(
                "two brains claim the alias '{}': {} and {} — name one in its brain.yaml (`alias:`)",
                brain.alias,
                other.db_root.display(),
                brain.db_root.display()
            );
        }
        paths.insert(brain.db_root.clone(), brain.alias.clone());
        list.push(brain);
        Ok(())
    }

    // 1 · a corpora file names the whole set, and its `default:` is the
    //     landing brain. Each entry resolves to a brain at `<dest>/<subdir>`.
    let mut default: Option<String> = None;
    if let Some(file) = corpora {
        for c in &file.entries {
            match config::from_root_named(&c.brain_dir(), Some(&c.alias)) {
                Ok(b) => push(&mut list, &mut paths, b)?,
                Err(e) => eprintln!("warning: corpus '{}' at {}: {e}", c.alias, c.brain_dir().display()),
            }
        }
        default = file.default.clone().or_else(|| list.first().map(|b| b.alias.clone()));
    }

    // 2 · the brain the command itself resolved to.
    let resolved = config::resolve(global.db.as_deref()).ok();
    if let Some(brain) = &resolved {
        default = default.or_else(|| Some(brain.alias.clone()));
        push(&mut list, &mut paths, brain.clone())?;
    }

    // 3 · explicitly named brains.
    for (alias, path) in explicit {
        match config::from_root_named(&config::expand(path), Some(alias)) {
            Ok(b) => push(&mut list, &mut paths, b)?,
            Err(e) => eprintln!("warning: --brain {alias}={path}: {e}"),
        }
    }

    // 4 · everything registered on this machine — unless a corpora file already
    //     said what the set is. A container that is handed a `corpora.yaml` must
    //     serve exactly those, not those plus whatever the image's home happens
    //     to hold.
    if corpora.is_none() {
        if let Ok(registry) = config::load_brains() {
            for (alias, path) in &registry.entries {
                let root = config::resolve_alias_path(path);
                match config::from_root_named(&root, Some(alias)) {
                    Ok(b) => push(&mut list, &mut paths, b)?,
                    Err(e) => eprintln!("warning: registered brain '{alias}' ({path}): {e}"),
                }
            }
        }
    }

    if list.is_empty() {
        bail!(
            "no brains to serve — pass --db <path>, --brain <alias>=<path>, or register one \
             with `brain use .` inside it"
        );
    }

    // Stable, predictable order for the dropdown: the default first, the rest by
    // alias. A selector whose options move between reloads is a selector nobody
    // can build muscle memory for.
    let default = default.unwrap_or_else(|| list[0].alias.clone());
    list.sort_by(|a, b| {
        let a_default = a.alias == default;
        let b_default = b.alias == default;
        b_default.cmp(&a_default).then_with(|| a.alias.cmp(&b.alias))
    });

    Ok(Brains { list, default })
}
