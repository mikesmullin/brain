//! cli.rs — the command surface.
//!
//! brain1's `--help` shape is kept deliberately: the subcommand groups are how
//! the system explains itself, and they are grouped by *what you are doing*
//! (define the schema, fill the data, run it, verify it, query it) rather than
//! by implementation. Three changes from v1, all of them consequences of
//! decisions recorded in the docs:
//!
//! * **`crawl` is the sixth query mode, and the default.** The other five are
//!   unchanged.
//! * **`pg` replaces `db/pgdata/`.** The index lives in a shared PostgreSQL
//!   instance, one schema per brain, started on request and never implicitly.
//! * **`web` replaces `viz`.** The point cloud and the in-page agent are gone;
//!   what is left is a wiki with a search bar.

use anyhow::{bail, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const HELP: &str = r#"🧠 Brain — knowledge-graph
usage: brain [--use <alias>] [--db <path>] <subcommand> [args...]

select a brain:
  use         list brains, or select one persistently
  --use       per-command override (does not change the saved default)
  --db        per-command path override (wins over --use)

create your brain:
  init        scaffold new database

enumerate your schema:
  schema graph          yaml+mermaid graph view (with counts)
  schema uniq           unique component / class / relation names
  schema components     component(s): fields + methods
  schema classes        class(es): components / top / idField
  schema relations      relation(s): domain / range / cardinality / OWL axioms
  schema methods        component methods applicable to a class
  schema turtle         the T-box as Turtle (the W3C seam) or TSV triples
  schema orphans        entities with zero relations

define your schema: (T-BOX / STRUCTURE)
  def component         define a new component and its attributes
  def class             define a new class and its components
  def relation          define a new relationship (+ transitive/inverse/chain)

fill your data: (A-BOX / VALUES)
  new         instantiate an entity (class instance)
  set         update entity field value data
  ls          list class instances
  link        attach a relationship to an entity
  get         print an entity
  rm          remove an entity
  call        invoke a component method on an entity

work with external data:
  ingest      recursively read .md docs, auto-creating entities and schema (LLM)
  enrich      rewrite .md docs with [[wiki-links]]
  load        import an already-structured dataset (no LLM; benchmark/fixture)
  convert     translate an es6 schema.mjs + data.mjs pair into .md + schema.yaml

run your brain: (SERVER MODE — one process owns the connection pool)
  pg          install|start|stop|status|psql the shared PostgreSQL service
  server      start|stop|status|vacuum|reindex|export|components
  reindex     rebuild the index from entity yaml on disk (.md -> index)
  export      materialize the live index back to .md files (index -> .md)

verify brain integrity:
  validate    report linter findings for all entities
  refine      complete + de-dupe entities; canonicalize ids
  bench       run a dataset's benchmark suite (see docs/BENCHMARK.md)

query your brain: (all 6 run against the index — milliseconds)
  crawl       walk the graph, one typed decision per hop (DEFAULT)
  search      hybrid vector+BM25+RRF
  think       search + final LLM synthesis (answer/citations/gaps)
  ontology    LLM-driven graph traversal (natural language)
  graph       deterministic pattern-match (Mermaid-ish syntax; *N, --shortest)
  graphql     deterministic traversal (GraphQL-ish syntax)
  web         browse the brain as a wiki, with crawl behind the search bar

host your brain for external AI:
  mcp         launch MCP server (thin adapter over the same query surface)

for further help on any of these:
  help <subcommand>      list detailed args and examples
"#;

pub fn usage(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "init" => r#"Usage:
    brain init [--ontology <name>]

Description:
    Scaffold a new database in the current working directory:
      brain.yaml   per-db config (models, embeddings, postgres)
      db/          entity instances (<Class>/<id>.md) + schema.yaml

    Nothing rebuildable is written into db/: the index lives in the shared
    PostgreSQL instance, in a schema named after this brain.

Examples:
    brain init                        # scaffold db/ + brain.yaml here
    brain init --ontology dstwiki     # ...naming the ontology
"#,
        "schema" => r#"Usage:
    brain schema <graph|uniq|components|classes|relations|methods|turtle|tsv|orphans> [<name>]

Description:
    Inspect the T-box (schema). With no <name>, most subcommands print everything.

Subcommands:
    graph                yaml+mermaid graph view (with per-class counts)
    uniq                 unique component / class / relation names
    components [<Comp>]  component(s): their fields
    classes    [<Class>] class(es): their components / top / idField / parent
    relations  [<REL>]   relation(s): domain / range / cardinality / OWL axioms
    methods    [<Class>] component methods callable on a class
    turtle               the whole T-box as Turtle (owl:/rdfs: vocabulary)
    tsv                  the same axioms as subject<TAB>predicate<TAB>object
    orphans              entities with no relations

Examples:
    brain schema graph                   # whole schema as a mermaid graph
    brain schema classes Person          # just the Person class definition
    brain schema relations EATS          # one relation and its axioms
    brain schema turtle > tbox.ttl       # hand the T-box to a reasoner
"#,
        "def" => r#"Usage:
    brain def component <Name> --fields '<yamlflow>'
    brain def class <Name> [--components alias:Component ...] [--top] [--parent <Class>] [--id-field <path>]
    brain def relation <REL> <domain> <cardinality> <range> [--qualifiers <name> '<yamlflow>' ...]
                       [--transitive] [--symmetric] [--functional]
                       [--inverse-of <REL>] [--sub-property-of <REL>] [--chain <REL>,<REL>]

Description:
    Declare schema structure (the T-box): components, classes, and relations.
    Every axiom is checked against a NAMED invariant before it is written, so a
    chain whose legs do not compose, or an inverse whose ends do not swap,
    cannot be saved.

Options:
    --fields            inline YAML-flow field definitions (component)
    --components        alias:Component pairs the class composes (class)
    --top               mark the class a seed for `ontology` traversal
    --parent            rdfs:subClassOf
    --id-field          derive the entity id from this field (or RELATION)
    cardinality         one of: oto | otn | nto | mtm
    --transitive        owl:TransitiveProperty (requires domain == range)
    --symmetric         owl:SymmetricProperty  (requires domain == range)
    --functional        owl:FunctionalProperty (requires oto or nto)
    --inverse-of        owl:inverseOf          (ends must swap)
    --sub-property-of   rdfs:subPropertyOf     (parent must be mtm)
    --chain A,B         owl:propertyChainAxiom (range(A) must equal domain(B))

Examples:
    brain def component Naming --fields '{name: {type: string, required: true}}'
    brain def class Team --components naming:Naming --top
    brain def relation SUPPORTS Team mtm Product --qualifiers role '{type: string}'
    brain def relation PART_OF Region mtm Region --transitive
    brain def relation OWNED_BY Product mtm Franchise --inverse-of OWNS
    brain def relation GRANDPARENT_OF Person mtm Person --chain PARENT_OF,PARENT_OF
"#,
        "new" => r#"Usage:
    brain new <Class> <REL>=<slug> | <alias.field>=<value> ...

Description:
    Instantiate a new entity of an existing class. The id is derived from the
    class idField when it declares one, so you give only the class.

Examples:
    brain new Person identity.username=jdoe identity.name='Jane Doe'
    brain new EntityJournal BELONGS_TO=Person/jdoe   # id derived from the target
"#,
        "set" => r#"Usage:
    brain set <slug> <alias.field>=<value> | <REL>=<slug> ...
    brain set --file <path> [--class <Class>] [--partial]

Description:
    Write values onto an instance (creating it if needed). With --file, ingest a
    document: a .yaml/.md validates deterministically; any other text uses LLM
    extraction.

Options:
    --file      read instance(s) from a file instead of inline assignments
    --class     focus/hint the class when extracting from a doc
    --partial   allow partial (non-validating) writes on deterministic ingest

Examples:
    brain set Person/jdoe identity.name='Jane Doe'
    brain set Team/example SUPPORTS=Product/example
    brain set --file team.yaml --class Team
"#,
        "ls" => r#"Usage:
    brain ls [<Class>] [--long] [--limit N]

Description:
    Stream instance ids, grouped by class. Safe on multi-million-node graphs:
    entities are written as they arrive; column packing only buffers one
    terminal row. Ctrl-C aborts mid-stream.

Options:
    --long    print full <Class>/<id> slugs, one per line (no columns)
    --limit   stop after N (default: everything)

Examples:
    brain ls                # ids of every class, column-packed
    brain ls Person --long  # full Person/<id> slugs, one per line
"#,
        "link" => r#"Usage:
    brain link <slug> <REL> <slug> [qualifier=value ...]

Description:
    Add a typed relationship edge between two entities, with optional
    qualifiers. Slugs are case-insensitive.

Examples:
    brain link Team/example SUPPORTS Product/example role='primary'
"#,
        "get" => r#"Usage:
    brain get <slug> [--links] [--json]

Description:
    Print an entity's record. Slugs are case-insensitive.

Options:
    --links   also list incoming edges (which entities point at this one)
    --json    machine-readable

Examples:
    brain get Person/jdoe
    brain get Person/jdoe --links
"#,
        "call" => r#"Usage:
    brain call <slug> <method> [<params>]

Description:
    Invoke an ECS component method on an entity. <params> is YAML-flow (the
    outer {} is optional). Discover a class's methods with
    `brain schema methods <Class>`.

    A component method lives in the DATABASE, not in brain: a module at
    `components/<Comp>.{mjs,js,coffee}` beside db/, exporting `methods`. brain
    executes it with `bun`, else `node`, else $BRAIN_JS — run from the project
    root so the dataset's own dependencies resolve — hands it the entity as
    JSON, and writes back what the method changed. A brain with no
    `components/` never needs an interpreter.

    Only components and the body are adopted from the result. A method that
    rewired the graph would bypass schema validation on the relation, and
    `link` is the command for that.

Examples:
    brain schema methods EntityJournal
    brain call EntityJournal/jdoe Entry__list
    brain call EntityJournal/jdoe Entry__add 'msg: kicked off the migration'
"#,
        "refine" => r#"Usage:
    brain refine [--class <Class>] [--max-passes N] [--dry-run]

Description:
    Repair pass, in two halves.

    NATIVE, always: canonical ids. A class that declares an `idField` says what
    its entities are called, so a file whose name disagrees is misnamed. refine
    renames it, rewrites every edge that pointed at the old slug, and merges
    whatever now collides.

    SCRIPTED, when the dataset has one: `refiner/<Class>.{mjs,js,coffee}`
    exporting `refine(entity, ctx)`. Same execution model as `brain call` —
    see `brain help call`. Skipped, with a note, when no interpreter is found.

Options:
    --class        restrict to a single class
    --max-passes   bound the iterative relation-chain recursion (default: 4)
    --dry-run      print the renames and change nothing

Examples:
    brain refine
    brain refine --class Person --dry-run
"#,
        "rm" => r#"Usage:
    brain rm <slug> [<slug> ...] [--keep-file]

Description:
    Remove one or more entities by slug (case-insensitive), from the index and
    from disk.

Options:
    --keep-file   drop from the index only; leave the .md in place

Examples:
    brain rm Person/jdoe
"#,
        "ingest" => r#"Usage:
    brain ingest <dir> [--extract Class ...] [--exclude <path> ...] [--partial] [--suggest]

Description:
    Recursively LLM-extract entities from every .md under <dir>.

Options:
    --extract   limit extraction to specific classes
    --exclude   skip a path (repeatable)
    --partial   write partial entities even on deterministic failures
    --suggest   dry run: print recommended new `def` lines instead of writing

Examples:
    brain ingest ./docs
    brain ingest ./docs --extract Person Team
    brain ingest ./docs --suggest
"#,
        "enrich" => r#"Usage:
    brain enrich <path> [--ingest]

Description:
    Rewrite .md doc(s) in place, inserting [[wiki-links]] to entities. By
    default it only links entities that already exist.

Options:
    --ingest   also create missing entities (else link existing ones only)

Examples:
    brain enrich ./docs
    brain enrich notes.md --ingest
"#,
        "validate" => r#"Usage:
    brain validate [--quiet] [--errors-only]

Description:
    Validate every instance against the schema and lint the graph. Exits
    non-zero if any error is found.

    Errors are schema violations: an undeclared class, component, field or
    relation; a missing required field; a domain/range or cardinality breach;
    an edge pointing at nothing. Warnings and notices are shape advice and
    never change the exit code.

Options:
    --quiet        print only the summary counts
    --errors-only  suppress warnings and notices

Examples:
    brain validate
    brain validate --errors-only
"#,
        "reindex" => r#"Usage:
    brain reindex [--no-embed] [--check]

Description:
    Rebuild the index from the .md files on disk. This is the ONE place disk is
    read in bulk — nothing else triggers indexing implicitly.

    Rebuilding is OPT-IN: every other command rebuilds only when the index is
    empty, and the staleness check is skipped too. A stale index can serve
    slugs the files no longer have and nothing will warn you; this command is
    the whole remedy. The point of opt-in is that no time is spent detecting
    staleness unless you ask for it.

Options:
    --no-embed   skip embeddings entirely (keyword+graph only; no provider
                 calls — this is the mode the Panama Papers benchmark uses)
    --check      report whether the index looks stale, and change nothing

Examples:
    brain reindex
    brain reindex --no-embed
    brain reindex --check
"#,
        "export" => r#"Usage:
    brain export [--prune]

Description:
    Materialize the live index back to .md files (the mirror image of reindex,
    and the ONE place .md is written from the index).

Options:
    --prune   also delete .md files for entities no longer in the index

Examples:
    brain export
    brain export --prune
"#,
        "load" => r#"Usage:
    brain load <path> [--no-embed]

Description:
    Import an already-structured dataset (classes/components/relations already
    resolved — no LLM extraction). <path> must contain schema.yaml and
    <Class>/<id>.md dirs; files are copied into the active db/, then the index
    is rebuilt. For raw documents that need extraction, use `ingest`.

Examples:
    brain load ./panama-papers-db --no-embed
"#,
        "convert" => r#"Usage:
    brain convert <dir> [--out <dir>] [--ontology <name>]

Description:
    Translate an es6 brain database — a `schema.mjs` declaring the T-box and a
    `data.mjs` holding one object per entity — into brain2's format: one
    `<Class>/<id>.md` per entity with YAML frontmatter, plus `schema.yaml`.

    The convention in data.mjs is preserved exactly: lowercase keys are
    components, ALL-CAPS keys are relations, and a relation target is either a
    `Class/id` string or a map with `_to` plus qualifiers.

Options:
    --out        where to write (default: <active db/>)
    --ontology   name recorded in schema.yaml (default: the source dir's name)

Examples:
    brain convert /workspace/cli/dstwiki --out ./db
"#,
        "pg" => r#"Usage:
    brain pg <status|install|uninstall|start|stop|restart|enable|disable|logs|psql|url> [args]

Description:
    Manage the shared PostgreSQL instance — one rootless podman container,
    pgvector included, shared by every brain (and every other app) on this
    machine. It is deliberately NOT enabled by default: nothing starts a
    database behind your back.

    One server, many brains. Each brain owns a PostgreSQL *schema* named after
    its alias, so two brains can never collide in the namespace even if both
    want a table called `entities`.

Subcommands:
    status      container state, server version, and every brain schema on it
    install     write the systemd --user unit (does not start or enable it)
    uninstall   remove the unit
    start       start it (via the unit when installed, else podman directly)
    stop        stop it
    restart     stop then start
    enable      systemctl --user enable (add --now to start as well)
    disable     systemctl --user disable
    logs        tail the server log  [--lines N]
    psql        interactive shell on this brain's schema  [-c '<sql>']
    url         print the connection URL

Examples:
    brain pg install && brain pg start      # first run
    brain pg enable --now                   # ...and at login, from now on
    brain pg status
    brain pg psql -c 'select count(*) from entities'
"#,
        "server" => r#"Usage:
    brain server <start|stop|status|vacuum|reindex|export|components>

Description:
    Manage the long-running brain server: one process that owns the connection
    pool and the loaded T-box, and serves every other brain command (and MCP,
    and the web UI) over a unix socket. CLI invocations then start instantly
    and are consequence-free to abort. Exactly one server per brain.

    This is not the database — `brain pg` manages that. A brain server is
    optional; every command works without one by connecting directly.

Subcommands:
    start       run in the foreground (use &, tmux, or a service manager)
    stop        graceful shutdown
    status      pid, uptime, entity/link counts, index freshness
    reindex     rebuild the index in the running server
    export      write the live index back to .md
    vacuum      VACUUM ANALYZE and recount the cached counters
    components  label connected components (O(1) no-path answers + wiki
                neighbourhoods)

Examples:
    brain server start &
    brain server status
    brain server components
"#,
        "web" => r#"Usage:
    brain web [--port N] [--host H] [--open]

Description:
    Browse the brain as a wiki: one page per entity, its fields rendered as a
    reading page, its relations as links, and a search bar at the top that runs
    `crawl` by default.

    This replaces v1's `viz`. The WebGL point cloud and the in-page sidebar
    agent are both gone on purpose — two million dots on screen was a demo, not
    a tool, and an agent in a sidebar was worse at answering than a search box
    that crawls.

Options:
    --port   HTTP port (default: 4321)
    --host   bind address (default: 127.0.0.1)
    --open   also launch a browser

Examples:
    brain web
    brain web --port 8080
"#,
        "crawl" => r#"Usage:
    brain crawl <question> [--start <slug>] [--kind <Class>] [--steps N]
                [--no-finalizer] [--trace] [-v] [--json]
    brain "<question>"                      # crawl is the default mode

Description:
    Answer a natural-language question by walking the real graph, one typed
    decision per hop. Every turn is a reactive behaviour tree; every decision
    is a choice over options the engine already fetched, so a hallucinated path
    is not merely unlikely, it is unrepresentable.

    Two models are involved: a non-autoregressive decision slot answers typed
    questions (choice / noul) in tens of milliseconds, and a chat model phrases
    the final answer. By default the finalizer never sees the trace — a model
    handed an agent's work log will describe it, however firmly you ask it not
    to.

Options:
    --start           enter at a known slug instead of orienting
    --kind            force the answer's class (skips the kind Choice)
    --steps           step budget (default: 160)
    --no-finalizer    print the collected answer set, unphrased
    --trace           send the trace to the finalizer (debugging the crawl)
    -v                print the behaviour-tree walk and every decision
    --json            machine-readable

Examples:
    brain crawl 'which non-lead engineer supports the payments product?'
    brain 'what do beefalo eat?'
    brain crawl 'how do you craft an alchemy engine?' -v
"#,
        "search" => r#"Usage:
    brain search [--limit N] [--strategy hybrid|keyword|vector] [--no-expand]
                 [--explain] [--json] <query>

Description:
    Hybrid keyword + vector search fused with Reciprocal Rank Fusion, plus a
    1-hop relational expansion of the top seeds.

    The keyword half is BM25-shaped: an IDF per term so a rare word carries the
    row, and field weights over a banded tsvector (A = name/slug/class,
    B = roles, C = own fields, D = relation lines + body) so a name match
    outranks a mention in someone else's relation list.

Options:
    --limit      maximum results (default: 10)
    --strategy   hybrid (default) | keyword (FTS only) | vector (semantic only)
    --no-expand  skip the 1-hop relational expansion
    --explain    print per-stage rank attribution

Examples:
    brain search 'service ownership platform'
    brain search --limit 5 --explain 'payments platform'
    brain search --strategy keyword --no-expand Mossack
"#,
        "think" => r#"Usage:
    brain think [--limit N] [--json] <question>

Description:
    Retrieval-augmented synthesis: run search, then have an LLM compose a
    grounded answer with citations to sources and an explicit list of gaps.

Options:
    --limit   how many retrieved hits to feed the model (default: 8)

Examples:
    brain think 'what does the example team depend on and why?'
"#,
        "ontology" => r#"Usage:
    brain ontology [--max-turns N] [--json] <question>

Description:
    LLM-driven typed graph traversal. A tool-using agent walks typed edges over
    deterministic search/get/paths/list tools, so it can never name an entity
    that does not exist.

Options:
    --max-turns   tool-call budget (default: 20, or $BRAIN_MAX_TURNS)

Examples:
    brain ontology 'which non-lead engineer supports the payments product?'
"#,
        "graph" => r#"Usage:
    brain graph '<Subject> -->|REL| <Object>' [--json]

Description:
    Deterministic structural graph-match using Mermaid-ish edge syntax.
    Wildcards match N-degree connections: * / ** / *** are shorthand for
    1 / 2 / 3 hops; *N matches up to N hops for any N. `>` is shorthand for the
    unlabelled `-->`.

    Every result carries `capped`: true when a circuit breaker — not the
    graph's natural edge — stopped the search. A result that stopped because of
    the cap is a different fact from one that ran out of graph.

Options (inline in the pattern, or as flags):
    --shortest     return only the shortest path(s). Between two concrete
                   endpoints this is a bidirectional early-exit BFS, not an
                   enumerate-then-filter
    --undirected   walk edges in both directions
    --max-nodes    traversal circuit breaker (default: 100000)
    --limit        stop after N paths

Examples:
    brain graph 'Team -->|SUPPORTS| Product'
    brain graph '* > Person/jdoe > *'
    brain graph 'Person/jdoe *6> Person/asmith --shortest'
"#,
        "graphql" => r#"Usage:
    brain graphql '<slug> { field, REL { ... } }'

Description:
    Deterministic GraphQL-ish traversal from a known root. Lowercase names
    project a component; ALL-CAPS names traverse that relation and recurse.
    Slugs are case-insensitive; the query may be passed as an argument or piped
    via stdin / heredoc.

    Resolution is level-batched: one entity query and one links query per
    selection level, not two point queries per traversed node.

Examples:
    brain graphql 'person/jdoe { identity { name } }'
    brain graphql <<'EOF'
    Person/jdoe {
      identity { name },
      REPORTS_TO { identity { name } }
    }
    EOF
"#,
        "bench" => r#"Usage:
    brain bench <panama-etl|panama|suite> [options]

Description:
    Run a dataset's benchmark suite. See docs/BENCHMARK.md for the protocol and
    the published head-to-head results.

Subcommands:
    panama-etl   ICIJ Offshore Leaks CSVs -> a brain dataset
                 [--zip <path>] [--out <dir>] [--direct]
    panama       the paired question suite, warm: 1 warmup + 10 iterations
                 [--iterations N] [--seed <slug>] [--json]
    suite        a project's own test-cases.json against `crawl`
                 [--cases <path>] [--only <name>]

Examples:
    brain bench panama-etl --zip ~/full-oldb.LATEST.zip --out /data/panama
    brain bench panama --iterations 10
"#,
        "mcp" => r#"Usage:
    brain [--use <alias>] mcp

Description:
    Launch the MCP server over stdio. Exposes tools 1:1 with the CLI query and
    write surface: crawl, search, think, ontology, graph, graphql, get_entity,
    put_entity, delete_entity, schema, validate.

    `--use <alias>` pins this process to a brain without changing the saved
    default, so a long-lived agent stays on its database while other shells
    switch freely.

Examples:
    brain mcp
    brain --use ada mcp
"#,
        "use" => r#"Usage:
    brain use                         # list available brains
    brain use <alias>                 # select a memorized brain
    brain use .                       # register + select cwd
    brain use none                    # use the cwd-local db/
    brain use --rm <alias>            # forget a memorized alias

Description:
    List configured brains, or persistently select one. Aliases live in
    ~/.config/brain/brains.yaml as `alias: project-root` entries; the selected
    alias is stored as `current`. Each project root must contain a `db/`.

    Per-command override (does not persist):
      brain --use <alias> <command> ...
      brain --db <path> <command> ...
"#,
        _ => return None,
    })
}

/// Flags that apply to every command, peeled before dispatch.
#[derive(Debug, Default, Clone)]
pub struct Global {
    pub use_alias: Option<String>,
    pub db: Option<String>,
}

pub fn peel_global(argv: Vec<String>) -> Result<(Global, Vec<String>)> {
    let mut g = Global::default();
    let mut rest: Vec<String> = Vec::new();
    let mut it = argv.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--use" => match it.next() {
                Some(v) => g.use_alias = Some(v),
                None => bail!("--use needs an alias (see `brain use`)"),
            },
            "--db" => match it.next() {
                Some(v) => g.db = Some(v),
                None => bail!("--db needs a path"),
            },
            s if s.starts_with("--use=") => g.use_alias = Some(s[6..].to_string()),
            s if s.starts_with("--db=") => g.db = Some(s[5..].to_string()),
            _ => rest.push(a),
        }
    }
    Ok((g, rest))
}

pub const COMMANDS: &[&str] = &[
    "init", "def", "schema", "new", "set", "link", "get", "ls", "rm", "validate", "reindex",
    "export", "load", "convert", "pg", "server", "search", "ingest", "enrich", "graph", "think",
    "ontology", "graphql", "crawl", "web", "mcp", "use", "bench", "call", "refine",
];

/// Small helper for the hand-rolled flag parsing each command does.
pub struct Args {
    pub positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    pub fn parse(argv: &[String], valued: &[&str]) -> Args {
        let mut positional = Vec::new();
        let mut flags: Vec<(String, Option<String>)> = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if let Some(body) = a.strip_prefix("--") {
                if let Some((k, v)) = body.split_once('=') {
                    flags.push((k.to_string(), Some(v.to_string())));
                } else if valued.contains(&body) {
                    flags.push((body.to_string(), argv.get(i + 1).cloned()));
                    i += 1;
                } else {
                    flags.push((body.to_string(), None));
                }
            } else if a.len() == 2 && a.starts_with('-') && a != "-" {
                let k = a[1..].to_string();
                if valued.contains(&k.as_str()) {
                    flags.push((k, argv.get(i + 1).cloned()));
                    i += 1;
                } else {
                    flags.push((k, None));
                }
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Args { positional, flags }
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_deref())
    }
    pub fn get_all(&self, name: &str) -> Vec<String> {
        self.flags
            .iter()
            .filter(|(k, _)| k == name)
            .filter_map(|(_, v)| v.clone())
            .collect()
    }
    pub fn num(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(|v| v.parse().ok())
    }
    pub fn rest(&self) -> String {
        self.positional.join(" ")
    }
}
