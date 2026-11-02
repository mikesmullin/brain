---
name: brain
description: ask a typed knowledge graph a question, in words or by pattern
---

# brain

A **slug** is always `Class/id` (class-prefixed). Lowercase frontmatter keys are
**components**; ALL-UPPERCASE keys are **relations**.

Schema is declared with **`def`** (T-box); instances are written with
**`set`** / **`new`** (A-box). Every write is validated against the schema.

Pick a brain with `--db <path>` or `--use <alias>`; both work on every command.

## Query — six modes, in the order to try them

```sh
# (#1, and the default) ask in words. Walks the real graph, one typed decision
# per hop over options the engine already fetched — it cannot name a node that
# does not exist. The subcommand is optional.
brain 'which non-lead engineer supports the payments product?'
brain crawl 'what do beefalo eat?' --json      # answer + the slugs behind it
brain crawl 'how do you craft an alchemy engine?' -v   # the behaviour-tree walk

# (#2) you know the structure you want. Deterministic, no model involved.
brain graphql 'Team/example { naming, USES_SYSTEM { info } }'
brain graph 'Team -->|SUPPORTS| Product'
brain graph 'Person/jdoe *6> Person/asmith --shortest'

# (#3) a ranked list from a fuzzy phrase. BM25-shaped keyword + pgvector, RRF.
brain search --limit 5 --explain 'service ownership platform'
brain search --strategy keyword --no-expand Mossack     # leanest path

# (#4) a written answer with citations and an explicit list of gaps
brain think 'what system does the example team depend on and why?'

# (#5) a multi-hop question whose shape you cannot predict; an agent walks
# typed edges through deterministic tools
brain ontology 'which non-lead engineer supports the payments product?'
```

`crawl` returns the phrased answer *and* `slugs`, so you can check it:
`brain crawl '…' --json | jq .collected` then `brain get <slug>`.

## Read

```sh
brain get Person/jdoe --links     # the entity's .md, plus who points at it
brain ls Person --long            # full slugs, one per line (streams; safe at 2M)
brain schema classes Person       # components / top / idField / parent
brain schema relations SUPPORTS   # ends, cardinality, OWL axioms, live edge count
brain schema graph                # the whole schema as a mermaid graph
brain schema turtle               # the T-box as owl:/rdfs: triples
```

## Write

```sh
brain def component Naming --fields '{name: {type: string, required: true}}'
brain def class Team --components naming:Naming --top
brain def relation SUPPORTS Team mtm Product --qualifiers role '{type: string}'
brain def relation PART_OF Region mtm Region --transitive
brain def relation OWNED_BY Product mtm Franchise --inverse-of OWNS

brain new Person identity.username=jdoe identity.name='Jane Doe'
brain set Person/jdoe employment.title='Staff Engineer'
brain link Team/example SUPPORTS Product/example role=primary
brain rm Person/jdoe
```

Field types: `string · int · float · bool · date · enum · ref · json`, with
`list: true` for arrays. `enum` carries `values:`; `ref` carries a `Class/id`
whose target must exist and may be narrowed with `allowedTypes:`.

Relation axioms, each checked against a named invariant at load time:
`transitive` and `symmetric` (need `domain == range`), `functional` (needs
`oto` or `nto`), `inverseOf` (the ends must swap), `subPropertyOf` (the parent
must be `mtm`), and a two-leg `chain` (the legs must compose).

## Code the dataset owns

```sh
brain schema methods EntityJournal        # what a class's components declare
brain call EntityJournal/jdoe Entry__add 'msg: kicked off the migration'
brain refine --dry-run                    # canonical ids: what would be renamed
brain refine                              # ...and do it, rewiring every edge
```

`components/<Comp>.{mjs,js,coffee}` exporting `methods`, and
`refiner/<Class>.{mjs,js,coffee}` exporting `refine(entity, ctx)`, both beside
`db/`. Run by `bun`, else `node`, else `$BRAIN_JS`, from the project root so the
dataset's own `node_modules` resolve. A brain with neither directory never needs
an interpreter.

## Run

```sh
brain pg install && brain pg start   # the shared PostgreSQL, once per machine
brain reindex                        # the ONE .md -> index path
brain validate                       # schema + lint findings; exits 1 on error
brain web                            # the wiki, with crawl behind the search bar
brain mcp                            # stdio MCP server
```

**Rebuilding is opt-in.** A command rebuilds only when the index is *empty*,
and the staleness check is skipped too. A stale index can serve slugs the files
no longer have and nothing will warn you — `brain reindex` is the remedy, and
`brain reindex --check` asks the question deliberately.

No brain server is required; every command works without one. `brain server
start` is worth it when you want one process to hold the pool and serialize
maintenance.

## Gotchas

- Slug lookups are case-insensitive, but the stored form is the schema's class
  casing with a lowercase id.
- `graph` always reports `capped`: a result that stopped at the circuit breaker
  is a different fact from one that ran out of graph.
- `graphql` may be passed as an argument or piped via stdin / heredoc.
- `crawl` needs two model endpoints (a decision slot and a chat model); the
  other five modes need none, and `search` needs none when the brain is
  configured `embed.model: none`.
