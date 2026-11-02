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
The frontend is Rust compiled to wasm, rendering through [m-js] on the
[`rustex1`](vendor/rustex1) kit — there is no JavaScript here except the kit's
two bridge files. Saving `app/**.rs` hot-swaps the wasm in under a second with
no page reload; saving `web/styles.css` costs zero redraws.

## Benchmark

2,017,662 nodes / 3.3M edges of ICIJ Offshore Leaks data, against the same
questions pgGraph's harness uses. Protocol, parity caveats and results:
[docs/BENCHMARK.md](docs/BENCHMARK.md).

```sh
brain bench panama-etl --zip full-oldb.LATEST.zip --out /data/panama/db
brain --db /data/panama reindex --no-embed
brain --db /data/panama bench panama
```

## Layout

```
brain.yaml               per-db config, sibling of db/
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
