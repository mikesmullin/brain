---
name: brain
description: search the knowledge graph
---

# brain

A **slug** is always `Class/id` (class-prefixed). Lowercase frontmatter keys are
**components**; ALL-UPPERCASE keys are **relations**.

Schema is declared with **`def`** (T-box); instances are written with **`set`** / **`new`** (A-box).

## Query

```sh
# (#1 query for best results) LLM-driven typed traversal (multi-hop). Uses a deterministic `paths` BFS tool internally.
brain ontology "which non-lead engineer supports the payments product?"
#   -> traverses Product -> Team(SUPPORTS) -> Team(SERVICE_PROVIDER_OF) -> Person(LEADER_OF/REPORTS_TO)

# (#2 query for okay results) search + LLM synthesis (grounded answer + citations + gaps)
brain think "what system does the example team depend on and why?"

# (#3 query for basic results) hybrid vector + keyword(FTS) + RRF fusion; --explain shows per-stage attribution
brain search --limit 5 --explain "service ownership platform"

# (#4 query if you know the structure you're looking for) deterministic GraphQL-ish traversal
brain graphql 'Team/example { naming, USES_SYSTEM { info } }'

# nested field selection (`identity { name }`) projects a single sub-field.
# slug LOOKUPS are case-insensitive; the query may be passed as an arg OR piped via stdin/heredoc.
# these two invocations are equivalent:
brain graphql 'person/jdoe { identity { name }, REPORTS_TO { identity { name }}}'
brain graphql <<'EOF'
Person/jdoe {
  identity { name },
  REPORTS_TO { identity { name } }
}
EOF
#   slug: Person/jdoe
#   identity:
#     name: Jane Doe
#   REPORTS_TO:
#     - slug: Person/asmith
#       identity:
#         name: Alan Smith

# (#5 query if you know the relationship path) deterministic structural graph-match (Mermaid). * / ** / *** = 1/2/3-degree wildcards
brain graph 'Team -->|USES_SYSTEM| System'
brain graph '* --> *'

```

## Changing Brains

You may safely assume that a brain has already been selected for you, until you learn otherwise.

```sh
brain use                 # list available brain aliases
brain use example         # select a database named "example" (selection is persisted)
brain use none            # will expect a database in the cwd
```

Selecting an alias makes it the active brain for subsequent `brain` commands.

## Define the schema (T-box) — verb `def`

```sh
# a component is a reusable, typed field-bag
brain def component Naming --fields '{name: {type: string, required: true}, shorthand: {type: string, list: true}}'
brain def component SystemInfo --fields '{name: {type: string, required: true}, category: {type: string}}'

# a class composes components; --top marks it a seed class for `ontology` traversal
brain def class Team   --components naming:Naming --top
brain def class System --components info:SystemInfo

# a relation: <REL> <domain> <cardinality> <range>   (cardinality: oto|otn|nto|mtm)
brain def relation USES_SYSTEM Team mtm System --qualifiers '{reason: {type: string}}'

brain schema graph       # yaml+mermaid view with per-class instance counts
brain schema classes
```

Field types: `string | bool | int | date | enum | ref | json`
(`ref` accepts `allowedTypes: [Class]`; `enum` accepts `values: [...]`; `list: true` = array).

## Write instances (A-box) — verbs `set` / `new` / `link`

```sh
# set component fields (values parse as YAML scalars)
brain set System/example info.name="Example System" info.category="application"
brain set Team/example  naming.name="Example Team" naming.shorthand='[Example]'

# add a relation edge (with optional qualifiers)
brain link Team/example USES_SYSTEM System/example reason="core dependency"

brain get Team/example --links     # read an entity + incoming links
brain rm  Team/example             # remove
```

### File ingest — `set --file` (mode inferred from extension)

```sh
# .yaml  => DETERMINISTIC: the file is instance(s); must validate before write
brain set --file team.yaml --class Team

# any other text (Markdown) => LLM EXTRACTION: the model fills schema-conformant
# entities and they must pass validation. --class is an optional focus hint.
brain set --file notes.md
brain set --file notes.md --class Person
```

Deterministic YAML doc shape (multi-doc with `---` allowed):

```yaml
_class: Team
_id: example
naming: { name: Example Team }
USES_SYSTEM: [ System/example ]
```

### Bulk directory ingest (Markdown only)

```sh
brain ingest ./docs                          # LLM-extract entities from every .md
brain ingest ./docs --extract Person Team    # focus on specific classes
brain ingest ./docs --exclude vectordb --exclude loganalyzer2
brain ingest ./docs --suggest                # DRY RUN: print recommended new `def` lines
```

Wiki-links in a markdown body (`[[Class/id]]` or `[[REL:Class/id]]`) are auto-reconciled into the
frontmatter relations on write.

### Per-class auto-refiners

Bulk LLM ingest is **lenient by default** — it writes partial entities fast (missing values become
warnings) and, for classes with an `idField`, assigns a deterministic **placeholder id** when the
canonical value is unknown (e.g. `Person/a9089c66` for a person seen by name before their username is
resolved). You then run a separate repair pass:

```sh
brain refine [--class Person] [--max-passes N]
```

`refine` finds invalid / placeholder-id entities whose class has a **refiner** and resolves them
(idempotent; also a general repair tool). A refiner is a `.coffee` module exporting
`refine(cwd, entity, ctx)`; lookup order: `<cwd>/refiner/<Class>.coffee` (per-dataset) then built-in
`src/refiners/<Class>.coffee`. Refiners are dataset-specific (they call your own tools/directories),
so they live with your data, not in this repo.

Example — a `Person` refiner can resolve people against a company directory (e.g. a directory/`ldap`
CLI, with a chat-directory fallback for former employees), filling `identity`/`contact`/`employment`
and adding a `REPORTS_TO` edge from the directory's manager. `refine` then:
- renames the placeholder file to the canonical id (`Person/a9089c66` → `Person/jdoe`), merging
  into any existing record (this is the dedup win), and
- follows the manager chain recursively (batch-level, bounded by `--max-passes`) — auto-creating and
  resolving each manager (`jdoe → asmith → bmanager → …`).

**`idField`** — a class can declare `idField: "alias.field"` (e.g. `Person → identity.username`) so its
slug/filename is derived from a canonical field. This prevents the name-vs-username duplication problem:
the LLM's arbitrary id is discarded; the id is the username (or a placeholder until resolved).

## Component methods, relation-derived ids, listing (advanced but powerful)

**Component methods (ECS-style).** A component can ship reusable methods in
`<cwd>/components/<Component>.coffee` (or built-in `src/components/`), callable on *any* class that
uses that component. Each method is `fn(entity, alias, args)` and returns `{ success, error?, content }`.
For example a built-in **`Journal`** component (`entries: [{id, msg, created}]`) exposes
`Entry__list` / `Entry__add(msg)` / `Entry__remove(id)`:

```sh
brain schema methods EntityJournal        # list the methods a class exposes (via its components)
#   classes:
#   - EntityJournal:
#     components:
#     - Journal: # alias: journal
#       methods: |-
#         - Entry__list()  # List all journal entries (newest last).
#         - Entry__add(msg: string)  # Append a journal entry with the given message.
#         - Entry__remove(id: string)  # Remove a journal entry by its id.

brain call EntityJournal/jdoe Entry__add 'msg: kicked off the migration'
#   added entry a1b2c3                     # content -> stdout; a failure -> stderr, exit 1
```

**Relation-derived `idField`.** `idField` may also name a **relation** (ALL_UPPERCASE), so an entity's
id is the basename of that relation's single target. E.g. a per-person journal whose slug *is* its owner:

```yaml
# schema.yaml
classes:
  EntityJournal:
    idField: BELONGS_TO            # id = target id of BELONGS_TO (must be single-valued)
    components: { journal: Journal }
relations:
  BELONGS_TO: { domain: EntityJournal, range: Person, cardinality: nto }
```

**Create it from the CLI (`new`).** Instantiate a new instance of the class with just the relation;
the id is derived, and the new file is written to the storage dir that *defines* the class (so a private
class stays in its private dir):

```sh
brain new EntityJournal BELONGS_TO=Person/jdoe
#   new EntityJournal/jdoe -> .../EntityJournal/jdoe.md
```

**List instances** — `ls`-style, ids only, grouped by class:

```sh
brain ls EntityJournal
#   EntityJournal/
#     jdoe        asmith
brain ls                 # all classes    ·    brain ls Person --long   # full slugs, one per line
```

**Find an entity relative to another** — incoming edges are a deterministic lookup:

```sh
brain get Person/jdoe --links
#   incoming:
#   - from: EntityJournal/jdoe
#     rel: BELONGS_TO
```

Slug **lookups** are case-insensitive (classes are ProperCase, ids lowercase), so
`brain get person/JDOE` == `brain get Person/jdoe`. This applies to reads only — when you **write** a
slug (e.g. a `[[Class/id]]` wiki-link, or a `<REL>=<slug>` assignment) use the canonical casing.

## Validate + index

```sh
brain validate      # schema validation + lint (orphan detection); exit 1 on error
brain reindex       # rebuild the pglite index + embeddings from .md
```

## MCP server

```sh
brain mcp           # stdio MCP server
```

Exposes tools: `search`, `think`, `ontology`, `graph`, `graphql`, `get_entity`, `put_entity`,
`schema_methods`, `method_invoke`.
`put_entity` validates before writing and returns validation/lint failures as MCP tool errors — the
sanctioned write path for agents (no direct file edits).

## Notes

- Debug LLM steps: `DEBUG=1 brain ontology "..."` prints the full agl-ai agent/provider trace.
- Always `brain validate` after edits; run `brain reindex` after ingest/import to refresh search.
