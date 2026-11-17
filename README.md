<p align="center"><img src="docs/brain-logo.png" alt="brain" width="320"></p>

<h1 align="center">brain v2</h1>

<p align="center">A knowledge graph that lives in your repo — typed, searchable, and hard to corrupt.</p>

---

Pile enough notes, docs and facts into a folder and eventually you want to *ask
questions* of them — "which on-call engineer supports the payments service?" —
not just grep for keywords.

**brain** stores a typed knowledge graph as plain Markdown on disk, validates
every write against a schema, and answers questions over it from the CLI, from
a browser, or over MCP.

```sh
brain pg install && brain pg start          # the shared PostgreSQL, once per machine
brain init                                  # scaffold db/ + brain.yaml here
brain reindex                               # the ONE .md -> index path
brain 'which team supports the payments product?'
```

## What v2 is

Same idea as v1, same data on disk, almost nothing the same underneath.

| | v1 | v2 |
|---|---|---|
| Runtime | Bun + CoffeeScript | one Rust binary |
| Index | pglite (WASM Postgres, in-process) | PostgreSQL 18 + pgvector, one schema per brain |
| A server | required | optional — every command works without one |
| T-box | components / classes / relations | …plus six OWL axioms, each checked by a named invariant |
| Ways to ask | five | six — `crawl` is new, and the default |
| Browser | a WebGL point cloud + a sidebar agent | a wiki, written in Rust → wasm |

The `.md` format is unchanged and `schema.yaml` is read as-is, so migrating a v1
database is `brain reindex` and nothing else. Every v1 subcommand is present
except `viz`, which `web` replaces.

## `crawl` — ask in words

```
$ brain 'best use of mandrake for health'
Mandrake Soup is the best use of a Mandrake for health. It restores 150 Hunger,
100 Health, and 5 Sanity, and — unlike raw or cooked Mandrakes — it does not
cause sleep.
```

`crawl` walks the real graph to answer. Every turn is a reactive behaviour tree,
and every decision the model makes is a choice over a menu **the engine built
from real edges** — it never writes a slug, a relation name or a path, it picks.
A hallucinated path is not merely unlikely here; it is unrepresentable.

Two models: a non-autoregressive *decision slot* answers typed questions in tens
of milliseconds, and a chat model phrases the answer. By default the chat model
never sees the trace, because a model handed an agent's work log will describe
the log however firmly you ask it not to.

The other five modes are unchanged: `search` (BM25-shaped keyword + pgvector,
fused with RRF), `think` (retrieve, then synthesise with citations and gaps),
`ontology` (an agent walking typed edges through deterministic tools), `graph`
(Mermaid-ish pattern match), `graphql` (projection from a known root).

### Where a corpus comes from

A corpus is a **repository**, not image content. `corpora.yaml` names the
repositories to serve; the container shallow-clones them at boot, and
`POST /api/corpus/refresh` pulls the same file again on demand:

```yaml
default: alpha
corpora:
  alpha: { repo: git@git.example.com:alpha/coord.git, ref: main, subdir: .brain }
  beta:  { repo: git@git.example.com:beta/coord.git,  ref: main, subdir: .brain }
  gamma: { repo: git@git.example.com:gamma/coord.git, ref: main, subdir: .brain }
```

```sh
brain corpus list                       # what is configured, and which SHA each is at
brain corpus pull                       # clone/refresh, then reindex what moved
brain corpus pull --alias alpha --json  # one corpus, machine-readable
```

`subdir: .brain` is the fact the old single-repo clone could not express: a
coordination repo holds its brain in a subdirectory, while a repo that *is* a
brain has `db/` at its root. Both work; the file says which.

`pull` is shallow (`--depth 1`), so a refresh costs one fetch of the delta rather
than a re-clone, and it **skips the reindex when the SHA did not move**. A corpus
that cannot be cloned is an error (a wiki with no pages that answers 200 is worse
than one that says why); a corpus that cannot be refreshed keeps serving the
checkout on disk and reports `stale`, because stale service beats no service.

Refresh a running server without a restart:

```sh
curl -X POST -H "Authorization: Bearer $BRAIN_WEBHOOK_SECRET" \
     https://brain.example.com/api/corpus/refresh
```

It answers with the per-corpus report (old SHA → new SHA, whether it reindexed),
and `GET /api/brains` carries `repo`/`ref`/`sha`/`indexedAt` per database — so
"which corpus is serving me?" is a question with an answer. Unauthenticated
triggers are refused: without `BRAIN_WEBHOOK_SECRET` the route returns `403`
rather than exposing a `git fetch` + `TRUNCATE` to whoever can reach the port.
A second concurrent refresh gets `409`.

PostgreSQL is **derived and disposable** here: a refresh truncates and rebuilds
the index from the `.md` files, and nothing needs preserving between pulls.

## The T-box has axioms now

`schema.yaml` keeps v1's shape and adds the OWL vocabulary:

```yaml
relations:
  ITEM_HAS_VARIANT:
    domain: Item
    range: Item
    inverseOf: ITEM_VARIANT_OF      # owl:inverseOf — the ends must swap
  PART_OF:
    domain: Region
    range: Region
    transitive: true                # owl:TransitiveProperty — needs domain == range
  GRANDPARENT_OF:
    chain: [PARENT_OF, PARENT_OF]   # owl:propertyChainAxiom — range(A) must equal domain(B)
```

Those comments are the invariants, not documentation. A schema that violates one
does not load, and the error names it (`chain_leg_mismatch`,
`inverse_pair_mismatch`, `transitive_range_mismatch`, …). The language is the
type: a chain whose legs do not compose does not exist after `load()`.

`brain schema turtle` emits the whole T-box as `owl:`/`rdfs:` triples.

## The database

One rootless podman container, shared by every brain on the machine, and
**deliberately not enabled by default** — nothing starts a database behind your
back.

```sh
brain pg install        # write the systemd --user unit; do not start it
brain pg start          # start it now
brain pg enable --now   # ...and at every login, from now on
brain pg status         # container state, server version, every brain schema on it
```

Each brain owns a PostgreSQL *schema* named after its alias, so two brains — or
brain and any other app sharing the instance — cannot collide even if both want
a table called `entities`.

## The wiki

```sh
./dev.sh                # http://127.0.0.1:4321
```

One page per entity, its relations as links, and a search bar that runs `crawl`.

### One server, many brains

The wiki is not limited to one database. A process serves every brain it can
find, the selector beside the wordmark switches between them, and **the URL says
which one a page is about**:

```
/alpha/e/Person/jdoe            # one brain's entity
/beta/                          # another brain's home page
/beta/search/service%20ownership/search
```

`/` redirects to the default brain, so a pasted host still lands somewhere real,
and a link to an entity carries its database with it — a shared URL opens the
graph it names rather than whatever the reader last selected.

The set served is: a `--corpora <file>` if one is named (which replaces the
registry below, because the file is also what a refresh pulls), then the brain
`--db`/`--use` resolves to as the default, then every
`--brain <alias>=<path>`, then everything in `~/.config/brain/brains.yaml`. A
container with no registry and one corpus serves exactly one brain, which is what
production wants.

```sh
brain --db ~/work/alpha/coord/.brain web \
      --brain beta=~/work/beta/coord/.brain \
      --brain gamma=~/work/gamma/coord/.brain
```

Every brain indexes into **its own PostgreSQL schema**, so they cannot collide
even when all of them have a table called `entities`:

```sh
brain pg status     # container, server version, size per schema
```

Two things make sibling brains work in practice, both of them in `brain.yaml`:

```yaml
alias: alpha          # what the selector shows and the URL segment says
pg:
  schema: brain_alpha # this brain's index; defaults to the alias, sanitized
```

They are needed because of the one layout that keeps coming up: a coordination
repo keeps its brain in `.brain/`, so *every* coord repo — alpha, beta, gamma —
would otherwise be called "brain" and share one schema. A brain that names itself
can sit beside its siblings.

The frontend is Rust compiled to wasm, rendering through [m-js] on the
[`rustex1`](vendor/rustex1) kit — there is no JavaScript here except the kit's
two bridge files. Saving `app/**.rs` hot-swaps the wasm in 398 ms with no page reload; saving
`web/styles.css` costs zero redraws. The third tier — a backend `src/**.rs`
change — rebuilds but does not reliably re-exec, because the dev server
restarts via `current_exe()` and the build has already replaced that file;
restart `./dev.sh` after a backend change.

## Benchmark

2,017,662 entities / 2.90M edges of ICIJ Offshore Leaks data, against the same
questions pgGraph's harness uses. Warm medians, same machine, same seeds:

| Question | v1 (pglite) | v2 | pgGraph |
|---|---|---|---|
| status / graph size | 3.9 ms | **0.2 ms** | 47 ms |
| two-hop neighbourhood, capped 500 | 58 ms | 62 ms | 231 ms |
| two-hop neighbourhood, uncapped | **334 ms** | 534 ms | 4,510 ms |
| one-hop projection (the query DSL) | 6 ms | **1.8 ms** | 1,458 ms |
| shortest path, 1 hop | 64 ms | 51 ms | **4.1 ms** |
| text search, ranked top-20 | 601 ms | 430 ms | **193 ms** |
| index build | 447 s, 19.4 GB peak | **105 s** | 52–90 s |

One question got slower than v1 and the cause is measured, not guessed: the
uncapped walk sends a 36,256-string parameter back to the server on its second
hop, and pglite — running inside the process — never paid for that. Protocol,
every attempted fix and its measurement:
[docs/BENCHMARK.md](docs/BENCHMARK.md).

```sh
brain bench panama-etl --zip full-oldb.LATEST.zip --out /data/panama/db
brain --db /data/panama reindex --no-embed
brain --db /data/panama bench panama
```

## Layout

```
brain.yaml               per-db config, sibling of db/ (alias, endpoints, pg schema)
db/
  <Class>/<id>.md        entities — the A-box (git-tracked)
  schema.yaml            the T-box (git-tracked)
```

That is the whole of it. Nothing rebuildable is written into `db/`.

## Build

```sh
cargo build --release
blaze build --release    # the same, ~0.1s incremental (https://ix.dev/blaze)
```

`./dev.sh` additionally fetches the pinned `wasm-bindgen` and the
`wasm32-unknown-unknown` target if they are missing.

## Documentation

[`docs/index.html`](docs/index.html) — the full guide, versioned with this
release rather than published from a floating branch.

## License

MIT — see [LICENSE](LICENSE).

[m-js]: https://github.com/mikesmullin/m-js
