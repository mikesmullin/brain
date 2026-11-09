//! commands.rs — one function per subcommand.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use crate::abox::{self, Entity};
use crate::cli::{Args, Global};
use crate::config::{self, Brain};
use crate::index;
use crate::llm::{Chat, Embedder, Slot};
use crate::query;
use crate::store::Store;
use crate::tbox::{Cardinality, ClassDef, ComponentDef, FieldDef, RelationDef, TBox};

/// Everything a command needs, opened once.
pub struct Ctx {
    pub brain: Brain,
    pub tbox: TBox,
    pub store: Store,
    pub slot: Slot,
    pub chat: Chat,
    pub embedder: Option<Embedder>,
}

impl Ctx {
    /// Resolve the brain, load its T-box and connect to the index.
    ///
    /// Rebuilding is opt-in: the index is built here only when it is *empty*.
    /// No staleness check runs — the point of opt-in is that nothing spends
    /// time detecting staleness unless it was asked to.
    pub async fn open(g: &Global) -> Result<Ctx> {
        let brain = config::resolve(g.db.as_deref())?;
        let schema = brain.schema_path();
        let tbox = if schema.exists() {
            TBox::load(&schema)?
        } else {
            bail!(
                "{} has no schema.yaml — run `brain init` here, or point at a brain with --db",
                brain.db_root.display()
            )
        };
        let embedder = Embedder::new(&brain.config.embed);
        let dims = embedder.as_ref().map(|e| e.dims).unwrap_or(brain.config.embed.dims);
        let store = Store::connect(&brain.config.pg, &brain.pg_schema, dims).await?;
        store.ensure_schema().await?;
        let slot = Slot::new(&brain.config.llm)?;
        let chat = Chat::new(&brain.config.llm)?;
        let ctx = Ctx { brain, tbox, store, slot, chat, embedder };
        // Build only when the index is empty AND there is something to put in
        // it. A brain that has a schema and no entities yet — every brain, for
        // its first few commands — is not a brain with a stale index, and
        // announcing a rebuild of nothing on every `def` is just noise.
        if ctx.store.is_empty().await? {
            let files = index::enumerate(&ctx.brain, &ctx.tbox)?.len();
            if files > 0 {
                eprintln!("index is empty — building it once from {files} file(s) …");
                let r =
                    index::reindex(&ctx.brain, &ctx.tbox, &ctx.store, ctx.embedder.as_ref(), true).await?;
                eprintln!(
                    "indexed {} entities / {} edges in {:.1}s",
                    r.entities,
                    r.links,
                    r.total_ms as f64 / 1000.0
                );
                index::record_disk_fingerprint(&ctx.brain, &ctx.store).await?;
            }
        }
        Ok(ctx)
    }

    /// Resolve a user-typed slug the cheap way first: fold the class with the
    /// T-box's own casing and lowercase the id, which is how they are stored.
    pub async fn resolve_slug(&self, s: &str) -> Result<String> {
        let folded = match s.split_once('/') {
            Some((c, i)) => match self.tbox.resolve_class(c) {
                Some(cls) => format!("{cls}/{}", i.to_lowercase()),
                None => s.to_string(),
            },
            None => s.to_string(),
        };
        if let Some(found) = self.store.resolve(&folded).await? {
            return Ok(found);
        }
        if folded != s {
            if let Some(found) = self.store.resolve(s).await? {
                return Ok(found);
            }
        }
        bail!("no such entity: {s}")
    }
}

pub async fn dispatch(cmd: &str, argv: &[String], g: &Global) -> Result<i32> {
    match cmd {
        "init" => init(argv, g),
        "use" => use_cmd(argv),
        "pg" => pg_cmd(argv, g).await,
        "convert" => crate::convert::run(argv, g),
        "schema" => schema_cmd(argv, g).await,
        "def" => def_cmd(argv, g).await,
        "new" => new_cmd(argv, g).await,
        "set" => set_cmd(argv, g).await,
        "link" => link_cmd(argv, g).await,
        "get" => get_cmd(argv, g).await,
        "ls" => ls_cmd(argv, g).await,
        "rm" => rm_cmd(argv, g).await,
        "validate" => validate_cmd(argv, g).await,
        "call" => crate::ecs::call(&Ctx::open(g).await?, argv).await,
        "refine" => crate::ecs::refine(&Ctx::open(g).await?, argv).await,
        "reindex" => reindex_cmd(argv, g).await,
        "export" => export_cmd(argv, g).await,
        "load" => load_cmd(argv, g).await,
        "ingest" => ingest_cmd(argv, g).await,
        "enrich" => enrich_cmd(argv, g).await,
        // The production HTTP surface: same handlers as `web`, none of the dev
        // server's watchers, builds or self-restart. Runs inside the runtime
        // main() already built, unlike `web`.
        "serve" => crate::serve::run(argv, g).await,
        "server" => crate::server::run(argv, g).await,
        // `web` never reaches here: main() dispatches it before the runtime
        // starts, because the dev server owns the process and makes its own.
        "web" => unreachable!("dispatched in main()"),
        "mcp" => crate::mcp::run(g).await,
        "bench" => crate::bench::run(argv, g).await,
        "crawl" => crawl_cmd(argv, g).await,
        "search" => search_cmd(argv, g).await,
        "think" => think_cmd(argv, g).await,
        "ontology" => ontology_cmd(argv, g).await,
        "graph" => graph_cmd(argv, g).await,
        "graphql" => graphql_cmd(argv, g).await,
        other => bail!("unknown command: {other}"),
    }
}

// ─── create ─────────────────────────────────────────────────────────────────

fn init(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["ontology"]);
    let root = match &g.db {
        Some(d) => config::expand(d),
        None => std::env::current_dir()?,
    };
    let db = root.join("db");
    if db.join("schema.yaml").exists() {
        bail!("{} already holds a brain", db.display());
    }
    let name = a
        .get("ontology")
        .map(|s| s.to_string())
        .unwrap_or_else(|| config::alias_from_project(&root));
    std::fs::create_dir_all(&db)?;
    let tb = TBox::scaffold(&name);
    std::fs::write(db.join("schema.yaml"), tb.to_yaml())?;
    let cfg = root.join("brain.yaml");
    if !cfg.exists() {
        std::fs::write(
            &cfg,
            format!(
                "# {name} — per-database configuration.\n\
                 # The index lives in the shared PostgreSQL instance (`brain pg`), in a\n\
                 # schema named after this brain, so nothing rebuildable is kept in db/.\n\
                 embed:\n  model: none          # `none` skips the vector half entirely\n  \
                 url: http://127.0.0.1:1234/v1/embeddings\n  dims: 768\n\
                 llm:\n  chat_url: http://127.0.0.1:1234\n  chat_model: default\n  \
                 slot_url: http://127.0.0.1:1237\n\
                 pg:\n  host: 127.0.0.1\n  port: {}\n  dbname: {}\n  user: {}\n\
                 storage: []\n",
                config::DEFAULT_PG_PORT,
                config::DEFAULT_PG_DB,
                config::DEFAULT_PG_USER
            ),
        )?;
    }
    let alias = config::register_project(&root, false).unwrap_or_else(|_| name.clone());
    println!("scaffolded {}", db.display());
    println!("  schema.yaml   ontology: {name}  (1 class, 1 component, 1 relation)");
    println!("  brain.yaml    pg schema: {}", config::sanitize_ident(&alias));
    println!("\nnext:  brain pg install && brain pg start");
    println!("       brain def class …   # then `brain new` your first entity");
    Ok(0)
}

fn use_cmd(argv: &[String]) -> Result<i32> {
    let a = Args::parse(argv, &["rm"]);
    let mut brains = config::load_brains()?;
    if let Some(alias) = a.get("rm") {
        if brains.entries.remove(alias).is_none() {
            bail!("no such brain alias: {alias}");
        }
        if brains.current.as_deref() == Some(alias) {
            brains.current = None;
        }
        config::save_brains(&brains)?;
        println!("forgot {alias}");
        return Ok(0);
    }
    let Some(target) = a.positional.first() else {
        if brains.entries.is_empty() {
            println!("no brains registered yet — run `brain use .` inside one");
            return Ok(0);
        }
        let width = brains.entries.keys().map(|k| k.len()).max().unwrap_or(8);
        for (alias, path) in &brains.entries {
            let mark = if brains.current.as_deref() == Some(alias.as_str()) { "*" } else { " " };
            println!("{mark} {alias:width$}  {path}");
        }
        if brains.current.is_none() {
            println!("  (none selected — commands use the cwd-local db/)");
        }
        return Ok(0);
    };
    match target.as_str() {
        "." => {
            let cwd = std::env::current_dir()?;
            let alias = config::register_project(&cwd, true)?;
            let mut b = config::load_brains()?;
            b.current = Some(alias.clone());
            config::save_brains(&b)?;
            println!("using {alias}  ({})", cwd.display());
        }
        "none" => {
            brains.current = None;
            config::save_brains(&brains)?;
            println!("using the cwd-local db/");
        }
        alias => {
            if !brains.entries.contains_key(alias) {
                bail!("no such brain alias: {alias} (see `brain use`)");
            }
            brains.current = Some(alias.to_string());
            config::save_brains(&brains)?;
            println!("using {alias}  ({})", brains.entries[alias]);
        }
    }
    Ok(0)
}

// ─── the shared postgres service ────────────────────────────────────────────

async fn pg_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["lines", "c"]);
    let sub = a.positional.first().map(|s| s.as_str()).unwrap_or("status");
    // pg works without a resolvable brain; when there is one, its config wins.
    let (cfg, schema) = match config::resolve(g.db.as_deref()) {
        Ok(b) => (b.config.pg.clone(), b.pg_schema.clone()),
        Err(_) => (config::PgConfig::default(), "public".to_string()),
    };
    match sub {
        "status" => {
            let state = crate::pg::container_state();
            println!("container   {}  ({})", crate::pg::VOLUME, state);
            println!("unit        {}", crate::pg::unit_state());
            println!("url         {}", crate::pg::connection_url(&cfg));
            if state == "running" {
                match crate::pg::server_version(&cfg) {
                    Some(v) => println!("server      {v}"),
                    None => println!("server      (not answering yet)"),
                }
                match Store::connect(&cfg, &schema, 768).await {
                    Ok(s) => match s.list_schemas().await {
                        Ok(list) if !list.is_empty() => {
                            println!("\nbrains on this server:");
                            for (name, e, l) in list {
                                let mark = if name == schema { "*" } else { " " };
                                println!("{mark} {name:<24} {e:>10} entities  {l:>10} edges");
                            }
                        }
                        _ => println!("\nno brain schemas yet"),
                    },
                    Err(e) => println!("\ncannot query: {e}"),
                }
            } else {
                println!("\nstart it with `brain pg start` (first time: `brain pg install`)");
            }
        }
        "install" => {
            crate::pg::pull(false)?;
            let p = crate::pg::install_unit(&cfg)?;
            println!("wrote {}", p.display());
            println!("it is NOT enabled: `brain pg start` for now, `brain pg enable --now` for every login");
        }
        "uninstall" => {
            if crate::pg::uninstall_unit()? {
                println!("removed the systemd --user unit");
            } else {
                println!("no unit installed");
            }
        }
        "start" => {
            crate::pg::start(&cfg, true)?;
            println!("postgres is up on {}:{}", cfg.host, cfg.port);
        }
        "stop" => {
            crate::pg::stop()?;
            println!("stopped");
        }
        "restart" => {
            crate::pg::stop()?;
            crate::pg::start(&cfg, true)?;
            println!("restarted");
        }
        "enable" => {
            crate::pg::enable(a.has("now"))?;
            println!("enabled{}", if a.has("now") { " and started" } else { "" });
        }
        "disable" => {
            crate::pg::disable()?;
            println!("disabled");
        }
        "logs" => println!("{}", crate::pg::logs(a.num("lines").unwrap_or(60) as usize)?),
        "url" => println!("{}", crate::pg::connection_url(&cfg)),
        "psql" => return crate::pg::psql(&cfg, &schema, a.get("c")),
        other => bail!("unknown `brain pg` subcommand: {other} (see `brain help pg`)"),
    }
    Ok(0)
}

// ─── schema ─────────────────────────────────────────────────────────────────

async fn schema_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["limit"]);
    let sub = a.positional.first().map(|s| s.as_str()).unwrap_or("graph");
    let name = a.positional.get(1).cloned();
    let ctx = Ctx::open(g).await?;
    let tb = &ctx.tbox;
    match sub {
        "turtle" => print!("{}", tb.to_turtle()),
        "tsv" => print!("{}", tb.to_tsv()),
        "uniq" => {
            println!("components ({})", tb.components.len());
            for k in tb.components.keys() {
                println!("  {k}");
            }
            println!("classes ({})", tb.classes.len());
            for k in tb.classes.keys() {
                println!("  {k}");
            }
            println!("relations ({})", tb.relations.len());
            for k in tb.relations.keys() {
                println!("  {k}");
            }
        }
        "components" => {
            for (cname, c) in &tb.components {
                if let Some(n) = &name {
                    if !cname.eq_ignore_ascii_case(n) {
                        continue;
                    }
                }
                println!("{cname}");
                for (f, d) in &c.fields {
                    println!(
                        "  {f}: {}{}{}",
                        d.r#type,
                        if d.required { " (required)" } else { "" },
                        if d.list { " []" } else { "" }
                    );
                }
            }
        }
        "classes" => {
            let counts: BTreeMap<String, i64> = ctx.store.class_counts().await?.into_iter().collect();
            for (cname, c) in &tb.classes {
                if let Some(n) = &name {
                    if !cname.eq_ignore_ascii_case(n) {
                        continue;
                    }
                }
                println!(
                    "{cname}{}{}  — {} node(s)",
                    if c.top { " *top" } else { "" },
                    match &c.parent {
                        Some(p) => format!(" : {p}"),
                        None => String::new(),
                    },
                    counts.get(cname).copied().unwrap_or(0)
                );
                for (alias, comp) in &c.components {
                    println!("  {alias}: {comp}");
                }
                if let Some(f) = &c.id_field {
                    println!("  idField: {f}");
                }
            }
        }
        "relations" => {
            let counts: BTreeMap<String, i64> = ctx.store.relation_counts().await?.into_iter().collect();
            for (rname, r) in &tb.relations {
                if let Some(n) = &name {
                    if !rname.eq_ignore_ascii_case(n) {
                        continue;
                    }
                }
                let mut axioms: Vec<String> = Vec::new();
                if r.transitive {
                    axioms.push("transitive".into());
                }
                if r.symmetric {
                    axioms.push("symmetric".into());
                }
                if r.functional || r.cardinality.is_single_valued() {
                    axioms.push("functional".into());
                }
                if let Some(i) = &r.inverse_of {
                    axioms.push(format!("inverseOf {i}"));
                }
                if let Some(i) = &r.sub_property_of {
                    axioms.push(format!("subPropertyOf {i}"));
                }
                if r.chain.len() == 2 {
                    axioms.push(format!("chain {} ∘ {}", r.chain[0], r.chain[1]));
                }
                println!(
                    "{rname}  {} --{}--> {}   [{}]{}  — {} edge(s)",
                    r.domain.clone().unwrap_or_else(|| "*".into()),
                    r.cardinality.as_str(),
                    r.range.clone().unwrap_or_else(|| "*".into()),
                    r.cardinality.as_str(),
                    if axioms.is_empty() { String::new() } else { format!("  {}", axioms.join(", ")) },
                    counts.get(rname).copied().unwrap_or(0)
                );
                for (q, d) in &r.qualifiers {
                    println!("    qualifier {q}: {}", d.r#type);
                }
            }
        }
        "methods" => return crate::ecs::print_methods(&ctx, name.as_deref()),
        "orphans" => {
            let limit = a.num("limit").unwrap_or(500);
            let rows = ctx.store.orphans(limit).await?;
            if rows.is_empty() {
                println!("no orphans — every entity has at least one relation");
            }
            for (slug, _) in rows {
                println!("{slug}");
            }
        }
        "graph" => {
            let counts: BTreeMap<String, i64> = ctx.store.class_counts().await?.into_iter().collect();
            let rcounts: BTreeMap<String, i64> = ctx.store.relation_counts().await?.into_iter().collect();
            println!("ontology: {}", tb.ontology);
            println!("```mermaid");
            println!("graph LR");
            for (cname, c) in &tb.classes {
                let n = counts.get(cname).copied().unwrap_or(0);
                println!("  {cname}[\"{cname}<br/>{n}\"]");
                if let Some(p) = &c.parent {
                    println!("  {cname} -.->|is a| {p}");
                }
            }
            for (rname, r) in &tb.relations {
                let (Some(d), Some(rg)) = (&r.domain, &r.range) else { continue };
                let n = rcounts.get(rname).copied().unwrap_or(0);
                println!("  {d} -->|\"{rname} ({n})\"| {rg}");
            }
            println!("```");
        }
        other => bail!("unknown `brain schema` subcommand: {other} (see `brain help schema`)"),
    }
    Ok(0)
}

fn parse_yaml_flow(s: &str) -> Result<Value> {
    let text = s.trim();
    let wrapped = if text.starts_with('{') || text.starts_with('[') {
        text.to_string()
    } else {
        format!("{{{text}}}")
    };
    let v: serde_yaml::Value = serde_yaml::from_str(&wrapped).context("parsing YAML flow")?;
    Ok(abox::yaml_to_json(&v))
}

fn fields_from_flow(s: &str) -> Result<BTreeMap<String, FieldDef>> {
    let v = parse_yaml_flow(s)?;
    let Value::Object(m) = v else { bail!("--fields must be a YAML mapping") };
    let mut out = BTreeMap::new();
    for (k, def) in m {
        let fd: FieldDef = match def {
            Value::Object(_) => serde_json::from_value(def).context("field definition")?,
            Value::String(t) => FieldDef { r#type: t, ..Default::default() },
            _ => FieldDef::default(),
        };
        out.insert(k, fd);
    }
    Ok(out)
}

async fn def_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(
        argv,
        &["fields", "components", "qualifiers", "parent", "id-field", "inverse-of", "sub-property-of", "chain"],
    );
    let kind = a.positional.first().map(|s| s.as_str()).unwrap_or("");
    let name = a.positional.get(1).cloned();
    let ctx = Ctx::open(g).await?;
    let mut tb = ctx.tbox.clone();
    match (kind, name) {
        ("component", Some(name)) => {
            let fields = fields_from_flow(a.get("fields").unwrap_or("{}"))?;
            tb.components.insert(name.clone(), ComponentDef { fields, methods: vec![] });
            tb.validate()?;
            write_schema(&ctx.brain, &tb)?;
            println!("component {name} ({} field(s))", tb.components[&name].fields.len());
        }
        ("class", Some(name)) => {
            let mut components = BTreeMap::new();
            for pair in a.get_all("components") {
                for one in pair.split_whitespace() {
                    let (alias, comp) = one
                        .split_once(':')
                        .with_context(|| format!("--components takes alias:Component, got '{one}'"))?;
                    components.insert(alias.to_string(), comp.to_string());
                }
            }
            // also accept bare trailing `alias:Component` positionals
            for p in a.positional.iter().skip(2) {
                if let Some((alias, comp)) = p.split_once(':') {
                    components.insert(alias.to_string(), comp.to_string());
                }
            }
            tb.classes.insert(
                name.clone(),
                ClassDef {
                    components,
                    top: a.has("top"),
                    id_field: a.get("id-field").map(|s| s.to_string()),
                    parent: a.get("parent").map(|s| s.to_string()),
                },
            );
            tb.validate()?;
            write_schema(&ctx.brain, &tb)?;
            println!("class {name}");
        }
        ("relation", Some(name)) => {
            let domain = a.positional.get(2).cloned();
            let card = a.positional.get(3).cloned();
            let range = a.positional.get(4).cloned();
            let (domain, card, range) = match (domain, card, range) {
                (Some(d), Some(c), Some(r)) => (Some(d), c, Some(r)),
                // `def relation REL --inverse-of X` is legal: the ends come
                // from the relation it inverts.
                _ if a.has("inverse-of") => {
                    let inv = a.get("inverse-of").unwrap();
                    let r = tb
                        .relations
                        .get(inv)
                        .with_context(|| format!("--inverse-of names an undeclared relation: {inv}"))?;
                    (r.range.clone(), "mtm".to_string(), r.domain.clone())
                }
                _ => bail!("usage: relation"),
            };
            let cardinality = Cardinality::parse(&card)
                .with_context(|| format!("cardinality '{card}' — expected oto/otn/nto/mtm"))?;
            let mut qualifiers = BTreeMap::new();
            let qs = a.get_all("qualifiers");
            let mut i = 0;
            while i + 1 < qs.len() {
                qualifiers.extend(fields_from_flow(&format!("{{{}: {}}}", qs[i], qs[i + 1]))?);
                i += 2;
            }
            // `--qualifiers name '<flow>'` also arrives as two positionals
            if qs.len() == 1 {
                if let Some(flow) = a.positional.get(5) {
                    qualifiers.extend(fields_from_flow(&format!("{{{}: {}}}", qs[0], flow))?);
                }
            }
            // `--chain A,B` or `--chain A --chain B`. Deliberately not
            // `--chain A B`: a bare second word is indistinguishable from a
            // positional, so which one it was would depend on where the flag
            // appeared in the line.
            let chain: Vec<String> = a
                .get_all("chain")
                .iter()
                .flat_map(|v| v.split(',').map(|x| x.trim().to_string()))
                .filter(|x| !x.is_empty())
                .collect();
            tb.relations.insert(
                name.clone(),
                RelationDef {
                    domain,
                    range,
                    cardinality,
                    qualifiers,
                    transitive: a.has("transitive"),
                    symmetric: a.has("symmetric"),
                    functional: a.has("functional"),
                    inverse_of: a.get("inverse-of").map(|s| s.to_string()),
                    sub_property_of: a.get("sub-property-of").map(|s| s.to_string()),
                    chain,
                    chain_from: None,
                    chain_to: None,
                },
            );
            tb.validate()?;
            write_schema(&ctx.brain, &tb)?;
            println!("relation {name}");
        }
        _ => bail!("usage: def"),
    }
    Ok(0)
}

fn write_schema(brain: &Brain, tb: &TBox) -> Result<()> {
    std::fs::write(brain.schema_path(), tb.to_yaml())
        .with_context(|| format!("writing {}", brain.schema_path().display()))
}

// ─── A-box writes ───────────────────────────────────────────────────────────

/// `alias.field=value` / `REL=Class/id`, applied to one entity.
fn apply_assignments(e: &mut Entity, tb: &TBox, pairs: &[String]) -> Result<()> {
    for p in pairs {
        let (k, v) = p.split_once('=').with_context(|| format!("expected key=value, got '{p}'"))?;
        if crate::slug::is_relation_name(k) {
            let rel = tb
                .resolve_relation(k)
                .with_context(|| format!("'{k}' is not a declared relation"))?
                .to_string();
            crate::slug::parse(v)?;
            e.add_edge(&rel, v);
            continue;
        }
        let (alias, field) = k
            .split_once('.')
            .with_context(|| format!("'{k}' must be alias.field (or an ALL_CAPS relation)"))?;
        let declared = tb.class_fields(&e.cls);
        let (_, fields) = declared
            .get(alias)
            .with_context(|| format!("'{alias}' is not a component of {}", e.cls))?;
        let fd = fields
            .get(field)
            .with_context(|| format!("'{field}' is not a field of {alias}"))?;
        let parsed = coerce(fd, v)?;
        if fd.list {
            let cur = e.components.entry(alias.to_string()).or_default();
            match cur.get_mut(field) {
                Some(Value::Array(arr)) => arr.push(parsed),
                _ => {
                    cur.insert(field.to_string(), Value::Array(vec![parsed]));
                }
            }
        } else {
            e.set_field(k, parsed)?;
        }
    }
    Ok(())
}

fn coerce(fd: &FieldDef, raw: &str) -> Result<Value> {
    Ok(match fd.r#type.as_str() {
        "int" => Value::from(raw.parse::<i64>().with_context(|| format!("'{raw}' is not an int"))?),
        "float" => Value::from(raw.parse::<f64>().with_context(|| format!("'{raw}' is not a float"))?),
        "bool" => Value::Bool(matches!(raw.to_lowercase().as_str(), "true" | "yes" | "1" | "on")),
        "json" => parse_yaml_flow(raw).unwrap_or(Value::String(raw.to_string())),
        _ => Value::String(raw.to_string()),
    })
}

/// The id a class's `idField` dictates, or a deterministic placeholder.
fn derive_id(tb: &TBox, e: &Entity) -> String {
    if let Some(idf) = tb.id_field_of(&e.cls) {
        if crate::slug::is_relation_name(idf) {
            if let Some(edges) = e.relations.get(idf) {
                if let Some(first) = edges.first() {
                    if let Ok(t) = crate::slug::parse(&first.to) {
                        return t.id;
                    }
                }
            }
        } else if let Some(Value::String(v)) = e.field(idf) {
            if !v.trim().is_empty() {
                return crate::slug::sanitize_id(v);
            }
        }
    }
    let label = e.label();
    if !label.trim().is_empty() && label != abox::pretty_slug(&e.id) {
        return crate::slug::sanitize_id(&label);
    }
    crate::slug::placeholder_id(&[&e.cls, &label])
}

async fn new_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let Some(cls) = a.positional.first() else { bail!("usage: new") };
    let ctx = Ctx::open(g).await?;
    let cls = ctx
        .tbox
        .resolve_class(cls)
        .with_context(|| format!("'{cls}' is not a declared class (see `brain schema classes`)"))?
        .to_string();
    let mut e = Entity::new(cls, "x");
    apply_assignments(&mut e, &ctx.tbox, &a.positional[1..])?;
    e.id = derive_id(&ctx.tbox, &e);
    let path = e.write(&ctx.brain.db_root)?;
    ctx.store.upsert(&e).await?;
    ctx.store.refresh_stats().await?;
    println!("{}  ({})", e.slug(), path.display());
    Ok(0)
}

async fn set_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["file", "class"]);
    let ctx = Ctx::open(g).await?;
    if let Some(file) = a.get("file") {
        let n = crate::convert::ingest_structured_file(&ctx, PathBuf::from(file), a.get("class"), a.has("partial")).await?;
        println!("wrote {n} entity(ies)");
        return Ok(0);
    }
    let Some(slug) = a.positional.first() else { bail!("usage: set") };
    let s = crate::slug::parse(slug)?;
    let cls = ctx
        .tbox
        .resolve_class(&s.cls)
        .with_context(|| format!("'{}' is not a declared class", s.cls))?
        .to_string();
    let id = s.id.to_lowercase();
    let path = ctx.brain.db_root.join(&cls).join(format!("{id}.md"));
    let mut e = if path.exists() {
        abox::read_entity(&cls, &id, &path)?
    } else {
        Entity::new(cls.clone(), id.clone())
    };
    apply_assignments(&mut e, &ctx.tbox, &a.positional[1..])?;
    e.write(&ctx.brain.db_root)?;
    ctx.store.upsert(&e).await?;
    ctx.store.refresh_stats().await?;
    println!("{}", e.slug());
    Ok(0)
}

async fn link_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    if a.positional.len() < 3 {
        bail!("usage: link");
    }
    let ctx = Ctx::open(g).await?;
    let from = ctx.resolve_slug(&a.positional[0]).await?;
    let rel = ctx
        .tbox
        .resolve_relation(&a.positional[1])
        .with_context(|| format!("'{}' is not a declared relation", a.positional[1]))?
        .to_string();
    let to = ctx.resolve_slug(&a.positional[2]).await?;
    let fs = crate::slug::parse(&from)?;
    let path = ctx.brain.db_root.join(&fs.cls).join(format!("{}.md", fs.id));
    let mut e = abox::read_entity(&fs.cls, &fs.id, &path)?;
    let quals: BTreeMap<String, Value> = a.positional[3..]
        .iter()
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
    let edges = e.relations.entry(rel.clone()).or_default();
    match edges.iter_mut().find(|x| x.to.eq_ignore_ascii_case(&to)) {
        Some(x) => x.qualifiers.extend(quals),
        None => edges.push(abox::Edge { to: to.clone(), qualifiers: quals }),
    }
    e.write(&ctx.brain.db_root)?;
    ctx.store.upsert(&e).await?;
    ctx.store.refresh_stats().await?;
    println!("{from} --{rel}--> {to}");
    Ok(0)
}

async fn get_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let Some(want) = a.positional.first() else { bail!("usage: get") };
    let ctx = Ctx::open(g).await?;
    let slug = ctx.resolve_slug(want).await?;
    let Some((e, incoming)) = crate::store::hydrate(&ctx.store, &slug).await? else {
        bail!("no such entity: {want}")
    };
    if a.has("json") {
        let comps: serde_json::Map<String, Value> = e
            .components
            .iter()
            .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "slug": e.slug(), "class": e.cls, "label": e.label(),
                "components": comps,
                "relations": e.relations.iter().map(|(r, es)| (r.clone(),
                    es.iter().map(|x| x.to.clone()).collect::<Vec<_>>())).collect::<BTreeMap<_,_>>(),
                "incoming": incoming.iter().map(|l| json!({"rel": l.rel, "from": l.from})).collect::<Vec<_>>(),
            }))?
        );
        return Ok(0);
    }
    print!("{}", e.to_markdown());
    if a.has("links") {
        println!("\n<!-- incoming ({}) -->", incoming.len());
        for l in incoming {
            println!("{} --{}--> {}", l.from, l.rel, slug);
        }
    }
    Ok(0)
}

async fn ls_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["limit"]);
    let ctx = Ctx::open(g).await?;
    let want = match a.positional.first() {
        Some(c) => Some(
            ctx.tbox
                .resolve_class(c)
                .with_context(|| format!("'{c}' is not a declared class"))?
                .to_string(),
        ),
        None => None,
    };
    let limit = a.num("limit").unwrap_or(i64::MAX);
    let long = a.has("long");
    let width: usize = std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let mut offset = 0i64;
    let mut printed = 0i64;
    let mut cur_class = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut rowlen = 0usize;
    loop {
        let page = ctx.store.list_class(want.as_deref(), 4096, offset).await?;
        if page.is_empty() {
            break;
        }
        for (slug, cls, _) in &page {
            if printed >= limit {
                break;
            }
            if long {
                writeln!(out, "{slug}")?;
            } else {
                if *cls != cur_class {
                    if !row.is_empty() {
                        writeln!(out, "  {}", row.join("  "))?;
                        row.clear();
                        rowlen = 0;
                    }
                    writeln!(out, "{cls}")?;
                    cur_class = cls.clone();
                }
                let id = slug.split('/').next_back().unwrap_or(slug).to_string();
                if rowlen + id.len() + 2 > width {
                    writeln!(out, "  {}", row.join("  "))?;
                    row.clear();
                    rowlen = 0;
                }
                rowlen += id.len() + 2;
                row.push(id);
            }
            printed += 1;
        }
        if printed >= limit {
            break;
        }
        offset += page.len() as i64;
    }
    if !row.is_empty() {
        writeln!(out, "  {}", row.join("  "))?;
    }
    Ok(0)
}

async fn rm_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    if a.positional.is_empty() {
        bail!("usage: rm");
    }
    let ctx = Ctx::open(g).await?;
    let mut n = 0;
    for want in &a.positional {
        let slug = match ctx.resolve_slug(want).await {
            Ok(s) => s,
            Err(_) => {
                eprintln!("warn: no such entity: {want}");
                continue;
            }
        };
        ctx.store.delete(&slug).await?;
        if !a.has("keep-file") {
            let s = crate::slug::parse(&slug)?;
            let p = ctx.brain.db_root.join(&s.cls).join(format!("{}.md", s.id));
            std::fs::remove_file(&p).ok();
        }
        println!("removed {slug}");
        n += 1;
    }
    ctx.store.refresh_stats().await?;
    Ok(if n == 0 { 1 } else { 0 })
}

// ─── verify / maintain ──────────────────────────────────────────────────────

async fn validate_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let ctx = Ctx::open(g).await?;
    let findings = index::validate(&ctx.brain, &ctx.tbox)?;
    let errors = findings.iter().filter(|f| f.severity == index::Severity::Error).count();
    let warnings = findings.iter().filter(|f| f.severity == index::Severity::Warning).count();
    let notices = findings.len() - errors - warnings;
    if !a.has("quiet") {
        for f in &findings {
            if a.has("errors-only") && f.severity != index::Severity::Error {
                continue;
            }
            println!("{f}");
        }
    }
    println!("\n{errors} error(s), {warnings} warning(s), {notices} notice(s)");
    Ok(if errors > 0 { 1 } else { 0 })
}

async fn reindex_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let brain = config::resolve(g.db.as_deref())?;
    let tbox = TBox::load(&brain.schema_path())?;
    let no_embed = a.has("no-embed");
    let mut embedder = if no_embed { None } else { Embedder::new(&brain.config.embed) };
    // Trust the provider over brain.yaml about its own vector width.
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
    if a.has("check") {
        let (stale, st) = index::staleness(&brain, &tbox, &store).await?;
        println!("entities    {}", st.entities);
        println!("edges       {}", st.links);
        println!("indexed at  {}", if st.indexed_at.is_empty() { "never" } else { &st.indexed_at });
        println!("schema fp   {} (now {})", st.fingerprint, tbox.fingerprint());
        println!("stale       {stale}");
        return Ok(if stale { 1 } else { 0 });
    }
    let r = index::reindex(&brain, &tbox, &store, embedder.as_ref(), false).await?;
    index::record_disk_fingerprint(&brain, &store).await?;
    println!(
        "\nindexed {} entities / {} edges from {} file(s){}",
        r.entities,
        r.links,
        r.files,
        if r.embedded > 0 { format!(" / {} vectors", r.embedded) } else { String::new() }
    );
    println!(
        "  parse {:.1}s (summed across lanes)  copy {:.1}s  indexes {:.1}s{}  total {:.1}s",
        r.parse_ms as f64 / 1000.0,
        r.copy_ms as f64 / 1000.0,
        r.index_ms as f64 / 1000.0,
        if r.embed_ms > 50 { format!("  embed {:.1}s", r.embed_ms as f64 / 1000.0) } else { String::new() },
        r.total_ms as f64 / 1000.0
    );
    Ok(0)
}

async fn export_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let ctx = Ctx::open(g).await?;
    let (written, pruned) = index::export(&ctx.brain, &ctx.store, a.has("prune")).await?;
    println!("wrote {written} .md file(s){}", if pruned > 0 { format!(", pruned {pruned}") } else { String::new() });
    Ok(0)
}

async fn load_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let Some(src) = a.positional.first() else { bail!("usage: load") };
    let brain = config::resolve(g.db.as_deref())?;
    let (n, _) = index::load_dataset(&brain, &config::expand(src))?;
    println!("copied {n} entity file(s) + schema.yaml into {}", brain.db_root.display());
    let tbox = TBox::load(&brain.schema_path())?;
    let embedder = if a.has("no-embed") { None } else { Embedder::new(&brain.config.embed) };
    let dims = embedder.as_ref().map(|e| e.dims).unwrap_or(brain.config.embed.dims);
    let store = Store::connect(&brain.config.pg, &brain.pg_schema, dims).await?;
    let r = index::reindex(&brain, &tbox, &store, embedder.as_ref(), false).await?;
    index::record_disk_fingerprint(&brain, &store).await?;
    println!("indexed {} entities / {} edges in {:.1}s", r.entities, r.links, r.total_ms as f64 / 1000.0);
    Ok(0)
}

// ─── the six query modes ────────────────────────────────────────────────────

async fn crawl_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["start", "kind", "steps"]);
    let question = a.rest();
    if question.trim().is_empty() {
        bail!("usage: crawl");
    }
    let ctx = Ctx::open(g).await?;
    let opts = query::crawl::CrawlOpts {
        start: a.get("start").map(|s| s.to_string()),
        kind: a.get("kind").map(|s| s.to_string()),
        finalize: !a.has("no-finalizer"),
        trace: a.has("trace"),
        verbose: a.has("v") || a.has("verbose"),
        max_steps: a.num("steps").unwrap_or(query::crawl::MAX_STEPS as i64) as usize,
    };
    let r = query::crawl::run(
        &ctx.store,
        &ctx.tbox,
        &ctx.slot,
        Some(&ctx.chat),
        ctx.embedder.as_ref(),
        &question,
        &opts,
    )
    .await?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(0);
    }
    match &r.answer {
        Some(t) => println!("{t}"),
        None if r.collected.is_empty() => println!("No answer found."),
        None => {
            for (slug, label) in r.collected.iter().zip(r.labels.iter()) {
                println!("{label}  [{slug}]");
            }
        }
    }
    if opts.verbose || a.has("stats") {
        eprintln!(
            "\n{} step(s), {} slot call(s), ~{} tokens, {}ms{}",
            r.steps.len(),
            r.calls,
            r.tokens,
            r.ms,
            match &r.kind {
                Some(k) => format!(", kind={k}, mode={}", r.mode),
                None => format!(", mode={}", r.mode),
            }
        );
        eprintln!("path: {}", r.path.join(" -> "));
    }
    Ok(if r.collected.is_empty() { 1 } else { 0 })
}

async fn search_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["limit", "strategy"]);
    let q = a.rest();
    if q.trim().is_empty() {
        bail!("usage: search");
    }
    let ctx = Ctx::open(g).await?;
    let strategy = match a.get("strategy") {
        None => query::search::Strategy::Hybrid,
        Some(s) => query::search::Strategy::parse(s)
            .with_context(|| format!("--strategy '{s}' — expected hybrid/keyword/vector"))?,
    };
    let opts = query::search::SearchOpts {
        limit: a.num("limit").unwrap_or(10),
        strategy,
        expand: !a.has("no-expand"),
        explain: a.has("explain"),
        snippets: a.has("snippets"),
    };
    let r = query::search::run(&ctx.store, ctx.embedder.as_ref(), &q, &opts).await?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(0);
    }
    if r.hits.is_empty() {
        println!("no matches for \"{q}\"");
        return Ok(1);
    }
    let w = r.hits.iter().map(|h| h.slug.len()).max().unwrap_or(20);
    for (i, h) in r.hits.iter().enumerate() {
        print!("{:>3}. {:<w$}  {}", i + 1, h.slug, h.label);
        if opts.explain {
            print!(
                "   [{} score={:.4}{}{}]",
                h.via,
                h.score,
                h.keyword_rank.map(|x| format!(" kw#{x}")).unwrap_or_default(),
                h.vector_rank.map(|x| format!(" vec#{x}")).unwrap_or_default()
            );
        }
        println!();
        if let Some(s) = &h.snippet {
            println!("     {s}");
        }
    }
    eprintln!("\n{} hit(s) via {} in {}ms", r.hits.len(), r.strategy, r.ms);
    Ok(0)
}

async fn think_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["limit"]);
    let q = a.rest();
    if q.trim().is_empty() {
        bail!("usage: think");
    }
    let ctx = Ctx::open(g).await?;
    let r = query::think::run(
        &ctx.store,
        ctx.embedder.as_ref(),
        &ctx.chat,
        &q,
        a.num("limit").unwrap_or(8),
    )
    .await?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("{}", r.answer);
        if !r.sources.is_empty() {
            eprintln!("\nsources: {}", r.sources.join(", "));
        }
    }
    Ok(0)
}

async fn ontology_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["max-turns"]);
    let q = a.rest();
    if q.trim().is_empty() {
        bail!("usage: ontology");
    }
    let ctx = Ctx::open(g).await?;
    let r = query::ontology::run(
        &ctx.store,
        &ctx.tbox,
        &ctx.chat,
        &q,
        a.num("max-turns").map(|n| n as usize),
    )
    .await?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("{}", r.answer);
        if !r.slugs.is_empty() {
            eprintln!("\nevidence: {}", r.slugs.join(", "));
        }
        eprintln!("{} turn(s), {}ms", r.turns, r.ms);
    }
    Ok(0)
}

async fn graph_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["max-nodes", "limit"]);
    let mut pattern = a.rest();
    if pattern.trim().is_empty() {
        bail!("usage: graph");
    }
    // flags are legal inline in the pattern as well, so put the parsed ones back
    if a.has("shortest") {
        pattern.push_str(" --shortest");
    }
    if a.has("undirected") {
        pattern.push_str(" --undirected");
    }
    if let Some(n) = a.num("max-nodes") {
        pattern.push_str(&format!(" --max-nodes {n}"));
    }
    if let Some(n) = a.num("limit") {
        pattern.push_str(&format!(" --limit {n}"));
    }
    let ctx = Ctx::open(g).await?;
    let r = query::graph::run(&ctx.store, &ctx.tbox, &pattern).await?;
    if a.has("json") {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(0);
    }
    for p in &r.paths {
        let mut s = String::new();
        for (i, n) in p.nodes.iter().enumerate() {
            if i > 0 {
                s.push_str(&format!(" --{}--> ", p.via.get(i - 1).map(|x| x.as_str()).unwrap_or("")));
            }
            s.push_str(n);
        }
        println!("{s}");
    }
    eprintln!(
        "\n{} path(s), {} node(s) visited, capped={}, {}ms",
        r.paths.len(),
        r.visited,
        r.capped,
        r.ms
    );
    Ok(if r.paths.is_empty() { 1 } else { 0 })
}

async fn graphql_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let mut q = a.rest();
    if q.trim().is_empty() {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        q = buf;
    }
    if q.trim().is_empty() {
        bail!("usage: graphql");
    }
    let ctx = Ctx::open(g).await?;
    let r = query::graphql::run(&ctx.store, &q).await?;
    println!("{}", serde_json::to_string_pretty(&r)?);
    Ok(0)
}

// ─── the LLM-assisted write paths ───────────────────────────────────────────

async fn ingest_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &["extract", "exclude"]);
    let Some(dir) = a.positional.first() else { bail!("usage: ingest") };
    let ctx = Ctx::open(g).await?;
    crate::convert::ingest_documents(
        &ctx,
        &config::expand(dir),
        &a.get_all("extract"),
        &a.get_all("exclude"),
        a.has("partial"),
        a.has("suggest"),
    )
    .await
}

async fn enrich_cmd(argv: &[String], g: &Global) -> Result<i32> {
    let a = Args::parse(argv, &[]);
    let Some(path) = a.positional.first() else { bail!("usage: enrich") };
    let ctx = Ctx::open(g).await?;
    crate::convert::enrich(&ctx, &config::expand(path), a.has("ingest")).await
}
