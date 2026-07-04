<!-- LOGO -->
<h1>
<p align="center">
  <img src="docs/brain-logo.png" width="450" alt="Brain logo" />
</h1>
  <p align="center">
    <a href="#install">Install</a>
    ·
    <a href="#intialize-db">Initiailze DB</a>
    ·
    <a href="#daily-usage">Daily Usage</a>
    ·
    <a href="#query">Query</a>
    ·
    📖 <a href="https://mikesmullin.github.io/brain/">HTML Documentation</a> 👈
    ·
    <a href="#contributing">Contributing</a>
  </p>
</p>

# 🧠 Brain

**A knowledge graph that lives in your repo — typed, searchable, and resistant to corruption.** Pile enough notes, docs, and facts into a folder and sooner or later you want to
*ask questions* of them — "which on-call engineer supports the payments service?" — not just grep for
keywords. The usual options all disappoint: a vector store hands back fuzzy text with no structure, a
raw LLM agent let loose on your files will blow its context wnidow, fail to discover answers, invent duplicates and other schema-invalid junk, and submit your proprietary data to someone else's cloud. You end up with either no structure or no
trust.

`brain` gives you what you wanted. It stores a **typed knowledge graph** as plain
[Markdown](https://commonmark.org) files on disk — one entity per file, flattened
[YAML](https://yaml.org) frontmatter — so the data stays git-friendly, human-editable, and yours.
Structure comes from an [entity-component-system](https://en.wikipedia.org/wiki/Entity_component_system)
ontology (the same pattern game engines use to keep thousands of objects organized), and **every write
— from the CLI *or* over [MCP](https://modelcontextprotocol.io) — is validated against that schema**, so
the graph can't drift into inconsistency. Queries run against an embedded
([pglite](https://pglite.dev) w/ pgvector) index rebuilt from those `.md` files, giving you
hybrid vector + keyword search, deterministic graph traversal, and grounded LLM synthesis via
[agl-ai](https://github.com/mikesmullin/agl) microagents.

## Select a brain

Brain aliases can be stored in `~/.config/brain/brains.yaml`:

```yaml
mydb1: ~/path/to/brain-db
```

List available aliases with `brain use`, then persistently select one with:

```sh
brain use mydb1
brain ls
brain use none              # return to the database in the current directory
```

The selected alias is stored in the `current` key in `brains.yaml` and its path
is used instead of the current working directory's `db/`.

**Why use it**

- **A typed graph, not a bag of vectors.** `brain` validates every instance against a  schema and answers *relational* questions by traversing typed edges — not just nearest-neighbor text.
- **The model can't go rogue.** Both CLI *and* MCP are fully validated so the graph stays internally consistent, and verifiable.
- **Cwd-local and offline-friendly.** Nothing leaves your machine. Keep multiple independent brains (ie. one brain per app),
  commit them to different repos (some shared, some private), and share `.md` files with peers who reindex with their own embedder.
- **The index is a cache, never the source of truth.** Mutations write `.md` first, then reindex
  (`.md → pgsql`), so you can always rebuild — and diff — the whole graph from disk. Every git commit is a fullly revisioned snapshot backup.

## Install

Built for [Bun](https://bun.sh) w/ [CoffeeScript](https://coffeescript.org/).

```sh
bun install
bun link              # register the global `brain` command
```

**NOTE:** npm dependency `agl-ai` supplies embeddings + chat via GitHub Copilot (default), LM-Studio, or Ollama.

## Initialize DB

Run `brain init` in any directory to prepare a new database.

```
brain.yaml               # per-db configuration
db/
  <Class>/<id>.md        # entity instances (A-box; git-tracked)
  schema.yaml            # classes, attributes, relationships (T-box; git-tracked)
  pgdata/                # pglite data dir (rebuildable → gitignored)
```

**HOW TO SHARE:** the `db/` files are portable; a peer may clone them and run `brain reindex` to rebuild their
in-memory RDBMS index.

## Daily Usage

```sh
brain # will print help
```

See [SKILL.md](SKILL.md) for further CLI usage examples.

## Query

`brain` exposes **five** query subcommands. They trade off along two axes: *deterministic vs.
LLM-assisted*, and *ranked retrieval vs. exact graph traversal*. Pick by what you know going in.

| Command | Kind | Algorithm | Use it when… |
| ------- | ---- | --------- | ------------ |
| `search` | deterministic retrieval | hybrid FTS + vector → RRF fusion → relational expansion | you want a ranked list of relevant entities from a fuzzy phrase |
| `think` | LLM synthesis (RAG) | `search` → LLM answer with citations + gaps | you want a written answer, not a list |
| `ontology` | LLM graph traversal | tool-using agent over a deterministic BFS `paths` tool | multi-hop relational questions where you don't know the slugs |
| `graph` | deterministic match | structural pattern-match (Mermaid syntax) | "find every pair matching this edge shape" |
| `graphql` | deterministic traversal | GraphQL-ish field projection from a known root | you know the entity and want specific fields + neighbors |

### `search` — hybrid retrieval

Runs a keyword ([Postgres FTS](https://www.postgresql.org/docs/current/textsearch.html)) query **and**
a vector query ([pgvector](https://github.com/pgvector/pgvector) cosine similarity over the entity
embeddings) in parallel, then fuses the two ranked lists with
[Reciprocal Rank Fusion](https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf) (RRF, `k=60`) — so a
result that ranks well in *either* signal floats up without hand-tuned weights. It then does a **1-hop
relational expansion**: the top seeds pull in their typed neighbors (tagged with the relation they came
through) so structurally-adjacent entities surface even if their text didn't match. `--explain` prints
the per-stage rank attribution. (A cross-encoder rerank stage is wired as a config-gated placeholder,
`search.reranker: off`.)

> **Ideal for:** exploratory lookup when you want a ranked, explainable list of entities and will read
> the results yourself.

```sh
brain search "show me things about the payments platform"
```

### `think` — grounded synthesis (RAG)

Retrieval-augmented generation: it calls `search` under the hood, feeds the top-N hits to an
[agl-ai](https://github.com/mikesmullin/agl) chat model, and returns a natural-language **answer with
citations** to the source slugs and an explicit list of **gaps** (what the graph couldn't support). The
LLM only ever sees retrieved context, so answers stay grounded in your data.

> **Ideal for:** natural-language Q&A where you want a synthesized paragraph and sourcing, rather than a
> list to sift through.

```sh
brain think "what does the example team depend on and why?"
```

### `ontology` — typed multi-hop traversal

A tool-using agent (not a one-shot prompt): given a question, it may `search` and walk typed edges over
several turns. The heavy lifting is deterministic — a **bidirectional [BFS](https://en.wikipedia.org/wiki/Breadth-first_search)
`paths` tool** finds every path from a start entity to entities of a target class within `maxHops`, and
returns compact paths so the model can answer multi-hop questions in a single call. The model only
decides *where to look*; it never fabricates edges.

> **Ideal for:** relational questions that span several hops and where you don't know the exact
> entities — and the *path* is the answer.

```sh
brain ontology "which non-lead engineer supports the payments product?"
```

### `graph` — structural pattern-match

A fully deterministic, no-LLM structural query using a [Mermaid](https://mermaid.js.org/)-like edge
syntax. `*` / `**` / `***` are 1/2/3-degree wildcards, so you can match by shape rather than identity:

> **Ideal for:** enumerating all subgraphs that match a *pattern* — audits, orphan-hunting, "who uses
> what" — where you want exact, fast, reproducible results.

```sh
brain graph 'Team -->|USES_SYSTEM| System'   # every Team→System usage edge
brain graph '* --> *'                         # every edge in the graph
```

### `graphql` — projected traversal from a known root

A deterministic [GraphQL](https://graphql.org/)-ish DSL: start from a known slug and select exactly the
components, relations, and nested sub-fields you want (relations recurse into the target entity). Slugs
are matched case-insensitively, and the query can be an arg or piped via stdin/heredoc.

> **Ideal for:** you already know the entity and want a precise, shaped projection of specific fields
> plus its neighbors — an API-style read, not a search.

```sh
brain graphql 'Person/jdoe { identity { name }, REPORTS_TO { identity { name } } }'
```

## Concepts

Three mechanisms let a `brain` dataset carry its own behavior alongside its data. All three are
**dataset-local** — they live next to your `db/` (so they can call your own tools and directories) and
take precedence over any built-ins.

### Refiners

**The problem:** When you bulk-import a pile of notes and let the LLM pull out entities, it usually only
catches *part* of the story: it sees "Jane from the payments team" but not her username, email, or who
she reports to. You end up with a half-filled record under a made-up id — and if Jane is mentioned in
three different docs, you get three different half-filled Janes. Cleaning that up by hand across hundreds
of entities is exactly the tedium you were trying to avoid.

**Our solution:** A **refiner** is a small script you write *once per class* that knows how to finish the
job. It looks each unfinished entity up in a source you already trust — a company directory, an internal
API, a spreadsheet — fills in the blanks, gives the record its real id, **merges the duplicates into
one**, and can even pull in related records (like that person's manager) automatically. Import stays fast
and forgiving (unknowns are parked under a temporary id like `Person/a9089c66` instead of failing the
import); and the `refiner` subcommand is the cleanup pass you run afterward, as often as you like — it only touches records
that don't pass schema validation.

For example, a `Person` refiner that queries your corporate directory fills in a person's
identity/contact/employment details, renames `Person/a9089c66` → `Person/jdoe` (folding in any duplicate
that already existed — that's a dedup win), and follows the management chain so `jdoe → asmith → …` all
get created and resolved in a single pass.

```sh
brain ingest ./docs             # bulk-extract entities from every .md in a folder (fast + lenient)
brain refine                    # then finish + de-dupe every entity that still needs it
brain refine --class Person     # ...or just clean up the people
```

Because a refiner calls *your* tools, it lives within your brain database directory (`./refiner/{{Class}}.coffee`), not
inside the `brain` codebase.

### Component methods

**The problem:** Some entities aren't just facts you read — they're things you *act on*. A running
journal you append notes to, a ticket you move between states, a checklist you tick off. You could open
the file and hand-edit the YAML every time, but that's fiddly and easy to botch (a malformed date, a
broken list), and letting an AI agent freely edit files is exactly how a knowledge base fills up with
corrupt junk.

**Our solution:** Attach small, safe operations directly to your data. Because every entity is assembled
from reusable **components** (the "C" in the ECS ontology), any behavior you attach to a component is
instantly available on *every* class that uses it — write "add a journal entry" once and everything with
a journal gets it. Crucially, these operations run through the same validation as every other write, so
they (and the agents that call them) can't scribble malformed data into your files.

For example, a custom `Journal` component could give any class an append-only log. Say you keep a running
journal per teammate:

```sh
brain schema methods EntityJournal
# classes:
# - EntityJournal:
#   components:
#   - Journal: # alias: journal
#     methods: |-
#       - Entry__list()  # List all journal entries (newest last).
#       - Entry__add(msg: string)  # Append a journal entry with the given message.
#       - Entry__remove(id: string)  # Remove a journal entry by its id.
brain call EntityJournal/jdoe Entry__add 'msg: kicked off the migration'
```

**NOTE:** _The parameter syntax is [YAML Flow](https://www.yaml.info/learn/flowstyle.html) syntax (with optional outer `{}` curly-braces)._

The same operations are reachable over [MCP](https://modelcontextprotocol.io) (`schema_methods` +
`method_invoke`), so an AI assistant can safely append to that journal on your behalf — through a
validated, typed API, never by editing files directly.


### Wiki-links

Inside an entity's Markdown **body**, wiki-links become typed edges in the frontmatter automatically on
the next write / reindex:

| Syntax | Becomes |
| ------ | ------- |
| `[[Class/id]]` | a `LINKS_TO` edge to that entity |
| `[[Class/id\|Anchor text]]` | same edge; the anchor is display-only (keeps your original wording) |
| `[[REL:Class/id]]` | a typed `REL` edge (any relation in your schema) |
| `[[REL:Class/id\|Anchor text]]` | typed `REL` edge with a display anchor |

Plain, unqualified paths like `[[meetings/2026-01-01]]` are treated as ordinary page links and ignored.
Wiki-links are handy when you want the *prose* to stay readable while the *graph* stays in sync:

- **Mention without breaking the sentence** — "shipped by [[Person/jdoe|Jane]]" reads naturally and links.
- **Assert a typed fact inline** — "owned by [[OWNED_BY:Team/example]]" adds the edge without hand-editing YAML.
- **Auto-enrichment** — `brain enrich <path>` rewrites existing docs, inserting wiki-links to entities it finds.
- **Hand-authoring** — cross-reference as you write; relations reconcile deterministically on save.

### Roadmap

- **Cross-encoder reranker** — the search pipeline (`keyword + vector → RRF → boosts → [rerank]`) has
  the rerank stage stubbed as a config-gated no-op (`search.reranker: off`). Wire a real cross-encoder
  later (e.g. a local `llama-server --reranking`, or a hosted reranker). RRF fusion runs today; only
  the final rerank step is deferred.
- **pdf/docx (and other) ingest formats** — a stub reader interface; v1 ingests Markdown only.

## Inspiration

- [msmullin/memo](https://github.com/mikesmullin/memo)
- [msmullin/ontology](https://github.com/mikesmullin/ontology)
- [garrytan/gbrain](https://github.com/garrytan/gbrain)