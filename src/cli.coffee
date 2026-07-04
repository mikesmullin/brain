# cli.coffee — command dispatcher for `brain`.
import { readFile } from 'fs/promises'
import { fileURLToPath } from 'url'
import { dirname, join } from 'path'
import { exists, brainRoot } from './config.coffee'

HELP = """
🧠 Brain — knowledge-graph
usage: brain <subcommand> [args...]

select a brain:
  use         list brains, or select one persistently

create your brain:
  init        scaffold new database

enumerate your schema:
  schema graph          yaml+mermaid graph view (with counts)
  schema uniq           unique component / class / relation names
  schema components     component(s): fields + methods
  schema classes        class(es): components / top / idField
  schema methods        component methods applicable to a class

define your schema: (T-BOX / STRUCTURE)
  def component         define a new component and its attributes
  def class             define a new class and its components
  def relation          define a new relationship

fill your data: (A-BOX / VALUES)
  new         instantiate an entity (class instance)
  set         update entity field value data
  ls          list class instances
  link        attach a relationship to an entity
  get         print an entity
  rm          remove an entity
  call        invoke given component method on an entity

work with external data:
  ingest      recursively read .md docs, auto-creating entities and schema
  enrich      rewrite .md docs with [[wiki-links]]

verify brain integrity:
  reindex     rebuild in-memory pglite index from entity yaml on disk
  validate    report linter findings for all entities
  refine      complete + de-dupe entities via per-class refiners

query your brain:
  search      hybrid vector+BM25+RRF (+rerank)
  think       search + final LLM synthesis (answer/citations/gaps)
  ontology    LLM-driven graph traversal (Natural language)
  graph       deterministic pattern-match (Mermaid-ish syntax)
  graphql     deterministic traversal (GraphQL-ish syntax)

host your brain for external AI:
  mcp         launch MCP server

for further help on any of these:
  help <subcommand>      list detailed args and examples
"""

# Per-subcommand usage, shown for `brain <cmd> --help` (also `-h` / `help`) and
# when a command is invoked without its required arguments.
USAGE =
  init: """
    Usage:
        brain init

    Description:
        Scaffold a new database in the current working directory:
          brain.yaml   per-db config (models, reranker, storage aggregation)
          db/          entity instances (<Class>/<id>.md) + schema.yaml
          db/pgdata/   rebuildable pglite index (gitignored)

    Examples:
        brain init                                   # scaffold db/ + brain.yaml in the current directory
  """
  schema: """
    Usage:
        brain schema <graph|uniq|components|classes|methods> [<name>]

    Description:
        Inspect the T-box (schema). With no <name>, most subcommands print everything.

    Subcommands:
        graph                yaml+mermaid graph view (with per-class counts)
        uniq                 unique component / class / relation names
        components [<Comp>]  component(s): their fields + methods
        classes    [<Class>] class(es): their components / top / idField
        methods    [<Class>] component methods applicable to a class

    Examples:
        brain schema graph                   # whole schema as a mermaid graph (+ per-class counts)
        brain schema classes Person          # show just the Person class definition
        brain schema methods EntityJournal   # methods callable on EntityJournal entities
  """
  def: """
    Usage:
        brain def component <Name> --fields '<yamlflow>'
        brain def class <Name> [--components alias:Component ...] [--top]
        brain def relation <REL> <domain> <cardinality> <range> [--qualifiers <name> '<yamlflow>' ...]

    Description:
        Declare schema structure (the T-box): components, classes, and relations.

    Options:
        --fields       inline YAML-flow field definitions (component)
        --components   alias:Component pairs the class composes (class)
        --top          mark the class a seed for `ontology` traversal (class)
        --qualifiers   name + YAML-flow def for edge qualifiers (relation)
        cardinality    one of: oto | otn | nto | mtm (relation)

    Examples:
        brain def component Naming --fields '{name: {type: string, required: true}}'          # a reusable field-bag
        brain def class Team --components naming:Naming --top                                 # a class made of components
        brain def relation USES_SYSTEM Team mtm System --qualifiers reason '{type: string}'   # a many-to-many edge type
  """
  new: """
    Usage:
        brain new <Class> <REL>=<slug> | <alias.field>=<value> ...

    Description:
        Instantiate a new entity of an existing class. The id is derived from the
        class idField (e.g. a relation target's basename), so you give only the class.

    Examples:
        brain new EntityJournal BELONGS_TO=Person/jdoe   # id derived from the target -> EntityJournal/jdoe
  """
  set: """
    Usage:
        brain set <slug> <alias.field>=<value> | <REL>=<slug> ...
        brain set --file <path> [--class <Class>] [--partial]

    Description:
        Write values onto an instance (creating it if needed). With --file, ingest a
        document: a .yaml validates deterministically; any other text uses LLM extraction.

    Options:
        --file      read instance(s) from a file instead of inline assignments
        --class     focus/hint the class when extracting from a doc
        --partial   allow partial (non-validating) writes on deterministic ingest

    Examples:
        brain set Person/jdoe identity.name='Jane Doe'   # set a component field
        brain set Team/example USES_SYSTEM=System/example # add a relation edge inline
        brain set --file team.yaml --class Team          # ingest instance(s) from a file
  """
  ls: """
    Usage:
        brain ls [<Class>] [--long]

    Description:
        List instance ids, ls-style, grouped by class. With no class, list every class.

    Options:
        --long   print full <Class>/<id> slugs, one per line

    Examples:
        brain ls                # ids of every class, grouped
        brain ls Person         # ids of just the Person class
        brain ls Person --long  # full Person/<id> slugs, one per line
  """
  link: """
    Usage:
        brain link <slug> <REL> <slug> [qualifier=value ...]

    Description:
        Add a typed relationship edge between two entities, with optional qualifiers.
        Slugs are case-insensitive.

    Examples:
        brain link Team/example USES_SYSTEM System/example reason='core dependency'   # add a qualified edge
  """
  get: """
    Usage:
        brain get <slug> [--links]

    Description:
        Print an entity's source file verbatim. Slugs are case-insensitive.

    Options:
        --links   also list incoming edges (which entities point at this one)

    Examples:
        brain get Person/jdoe          # print the entity's .md source
        brain get Person/jdoe --links  # ...and list incoming edges (who points at it)
  """
  rm: """
    Usage:
        brain rm <slug> [<slug> ...]

    Description:
        Remove one or more entities by slug (case-insensitive).

    Examples:
        brain rm Person/jdoe                    # remove one entity
        brain rm EntityJournal/jdoe Team/example  # remove several at once
  """
  call: """
    Usage:
        brain call <slug> <method> [<params>]

    Description:
        Invoke an ECS component method on an entity. <params> is YAML-flow (outer {}
        optional). Discover a class's methods with `brain schema methods <Class>`.
        The result prints to stdout; a failure prints to stderr and exits 1.

    Examples:
        brain call EntityJournal/jdoe Entry__list                                  # invoke a no-arg method
        brain call EntityJournal/jdoe Entry__add 'msg: kicked off the migration'   # invoke with YAML-flow params
  """
  ingest: """
    Usage:
        brain ingest <dir> [--extract Class ...] [--exclude <path> ...] [--partial] [--suggest]

    Description:
        Recursively LLM-extract entities from every .md under <dir> (fast + lenient).
        Follow with `brain refine` to complete and de-dupe the results.

    Options:
        --extract   limit extraction to specific classes
        --exclude   skip a path (repeatable)
        --partial   write partial entities even on deterministic failures
        --suggest   dry run: print recommended new `def` lines instead of writing

    Examples:
        brain ingest ./docs                        # extract entities from every .md under ./docs
        brain ingest ./docs --extract Person Team  # ...limited to these classes
        brain ingest ./docs --suggest              # dry run: print suggested `def` lines, write nothing
  """
  enrich: """
    Usage:
        brain enrich <path> [--ingest]

    Description:
        Rewrite .md doc(s) in place, inserting [[wiki-links]] to entities. By default
        it only links entities that already exist.

    Options:
        --ingest   also create missing entities (else link existing ones only)

    Examples:
        brain enrich ./docs             # link mentions in every .md to existing entities
        brain enrich notes.md --ingest  # ...and create entities that don't exist yet
  """
  refine: """
    Usage:
        brain refine [--class <Class>] [--max-passes N]

    Description:
        Repair pass: resolve invalid / placeholder-id entities whose class has a refiner —
        filling fields, renaming to the canonical id, de-duping, and following relation chains.

    Options:
        --class        restrict to a single class
        --max-passes   bound the iterative relation-chain recursion (default: 4)

    Examples:
        brain refine                 # repair + de-dupe every entity that has a refiner
        brain refine --class Person  # ...only People
  """
  validate: """
    Usage:
        brain validate

    Description:
        Validate every instance against the schema and lint (orphan detection).
        Exits non-zero if any error is found.

    Examples:
        brain validate   # schema + lint check of every entity (exit 1 on error)
  """
  reindex: """
    Usage:
        brain reindex

    Description:
        Rebuild the in-memory pglite index (+ embeddings) from the .md files on disk.
      Search-dependent commands build the index automatically when it has never
      been created; run this explicitly after source data changes to refresh it.

    Examples:
        brain reindex   # rebuild the search index from the .md files on disk
  """
  search: """
    Usage:
        brain search [--limit N] [--explain] <query>

    Description:
        Hybrid keyword (FTS) + vector search fused with Reciprocal Rank Fusion, plus a
        1-hop relational expansion of the top seeds.

    Options:
        --limit     maximum results to return (default: 10)
        --explain   print per-stage rank attribution

    Examples:
        brain search "service ownership platform"              # ranked hybrid search
        brain search --limit 5 --explain "payments platform"  # top 5, with per-stage rank attribution
  """
  think: """
    Usage:
        brain think [--limit N] <question>

    Description:
        Retrieval-augmented synthesis: run search, then have an LLM compose a grounded
        answer with citations to sources and an explicit list of gaps.

    Options:
        --limit   how many retrieved hits to feed the model (default: 8)

    Examples:
        brain think "what does the example team depend on and why?" # grounded answer + citations + gaps
  """
  ontology: """
    Usage:
        brain ontology <question>

    Description:
        LLM-driven typed graph traversal. A tool-using agent walks typed edges over a
        deterministic BFS paths tool to answer multi-hop relational questions.

    Examples:
        brain ontology "which non-lead engineer supports the payments product?"   # multi-hop typed traversal
  """
  graph: """
    Usage:
        brain graph '<Subject> -->|REL| <Object>'

    Description:
        Deterministic structural graph-match using Mermaid-ish edge syntax. Wildcards
        * / ** / *** match 1 / 2 / 3-degree connections.

    Examples:
        brain graph 'Team -->|USES_SYSTEM| System'    # path-find Teams to Systems via USES_SYSTEM edges
        brain graph '* --> Person/jdoe --> *'         # nodes within 1 degree of the Person/jdoe entity
        brain graph '* > Person/jdoe > *'             # shorthand: '>' is the unlabeled '-->'
  """
  graphql: """
    Usage:
        brain graphql '<slug> { field, REL { ... } }'

    Description:
        Deterministic GraphQL-ish traversal from a known root. Select components,
        relations, and nested sub-fields. Slugs are case-insensitive; the query may be
        passed as an argument or piped via stdin / heredoc.

    Examples:
        brain graphql 'person/jdoe { identity { name } }'   # project fields + follow a relation
        brain graphql <<'EOF'                               # ...or read the query from stdin (heredoc)
        Person/jdoe {
          identity { name },
          REPORTS_TO { identity { name } }
        }
        EOF
  """
  mcp: """
    Usage:
        brain mcp

    Description:
        Launch the MCP server over stdio. Exposes tools: search, think, ontology, graph,
        graphql, get_entity, put_entity, schema_methods, method_invoke.

    Examples:
        brain mcp   # serve the brain to an MCP client over stdio
  """
  use: """
    Usage:
      brain use
      brain use <alias>
      brain use none

    Description:
        List configured brains, or persistently select one for subsequent
        `brain` commands. Aliases are read from ~/.config/brain/brains.yaml
        as alias: project-root entries; the selected alias is stored as
        `current`. Each project root must contain a `db/` directory.

    Examples:
        brain use                         # list available brains
        brain use mydb1                   # select the mydb1 brain
        brain use none                    # return to the cwd-local db
    """



readVersion = ->
  try
    here = dirname(fileURLToPath(import.meta.url))
    pkg = JSON.parse(await readFile(join(here, '..', 'package.json'), 'utf-8'))
    pkg.version
  catch then '0.0.0'

COMMANDS =
  init: './commands/init.coffee'
  def: './commands/def.coffee'
  schema: './commands/schema.coffee'
  new: './commands/new.coffee'
  set: './commands/set.coffee'
  link: './commands/link.coffee'
  get: './commands/get.coffee'
  ls: './commands/ls.coffee'
  call: './commands/call.coffee'
  rm: './commands/rm.coffee'
  validate: './commands/validate.coffee'
  reindex: './commands/reindex.coffee'
  refine: './commands/refine.coffee'
  search: './commands/search.coffee'
  ingest: './commands/ingest.coffee'
  enrich: './commands/enrich.coffee'
  graph: './commands/graph.coffee'
  think: './commands/think.coffee'
  ontology: './commands/ontology.coffee'
  graphql: './commands/graphql.coffee'
  mcp: './commands/mcp.coffee'
  use: './commands/use.coffee'

# Help/usage output gets an extra trailing blank line so it's spaced from the next shell prompt.
showHelp = (s) -> console.log(s + '\n')
errHelp = (s) -> console.error(s + '\n')

export main = (argv) ->
  cmd = argv[0]
  rest = argv.slice(1)

  # top-level help: `brain`, `brain -h`, `brain --help`
  if not cmd or cmd in ['--help', '-h']
    showHelp HELP
    return 0
  # `brain help` prints the same top-level help; `brain help <subcommand>` prints detailed usage.
  if cmd is 'help'
    sub = rest[0]
    unless sub
      showHelp HELP
      return 0
    if USAGE[sub]
      showHelp USAGE[sub]
      return 0
    console.error "unknown command: #{sub}\n"
    showHelp HELP
    return 1
  if cmd in ['--version', '-V', 'version']
    console.log await readVersion()
    return 0

  modPath = COMMANDS[cmd]
  unless modPath
    console.error "unknown command: #{cmd}\n"
    showHelp HELP
    return 1

  # Per-command help: `brain <cmd> -h` / `brain <cmd> --help`.
  if '-h' in rest or '--help' in rest
    showHelp (USAGE[cmd] or "no detailed help for '#{cmd}'")
    return 0

  if cmd not in ['init', 'use'] and not exists()
    console.error 'fatal: brain database not found.'
    console.error 'select a database with `brain use`, or invoke this command from a valid brain database directory.'
    return 1

  try
    mod = await import(modPath)
  catch err
    console.error "command '#{cmd}' is not available yet: #{err.message}"
    return 1

  try
    code = await mod.run(rest)
    process.exitCode = code ? 0
    code ? 0
  catch err
    # A missing-required-arg error (message starts with "usage:") shows the full help.
    if /^usage:/i.test(err.message or '') and USAGE[cmd]
      errHelp USAGE[cmd]
    else
      console.error "error: #{err.message}"
    process.exitCode = 1
    1
