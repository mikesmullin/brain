# search.coffee — hybrid retrieval, mirroring gbrain's strategy:
#   keyword(FTS) + vector  ->  RRF(k=60) fusion  ->  relational expansion
#   ->  [cross-encoder rerank: PLACEHOLDER / config-gated no-op]
#
# Runs 100% against pglite (no disk reads, no world loads). The index must
# already exist — building it is `brain reindex`, an explicit user action, and
# is deliberately NOT triggered implicitly from here.
#
# NOTE: RRF is *fusion*, not a reranker. The cross-encoder rerank stage is a
# deliberate placeholder (config `search.reranker`, default off) to be wired later.
import { embedOne } from './embed.coffee'
import { NO_EMBED } from './index.coffee'

RRF_K = 60

# Reciprocal Rank Fusion over several named ranked lists of slugs.
rrf = (lists) ->
  scores = {}
  contrib = {}
  for own name, ranked of lists
    for slug, i in ranked
      s = 1 / (RRF_K + i + 1)
      scores[slug] = (scores[slug] or 0) + s
      (contrib[slug] ?= {})[name] = i + 1   # 1-based rank
  { scores, contrib }

export hybridSearch = (core, query, opts = {}) ->
  limit = opts.limit or 10
  strategy = opts.strategy or 'hybrid'   # hybrid | keyword | vector — opt out of fusion for full control
  expand = opts.expand isnt false        # 1-hop relational expansion (opt out with expand: false)
  throw new Error("unknown search strategy '#{strategy}' (hybrid|keyword|vector)") unless strategy in ['hybrid', 'keyword', 'vector']
  idx = core.idx
  unless await idx.isIndexed()
    throw new Error('no index found — run `brain reindex` first')

  spec = await idx.embedSpec()
  throw new Error('vector strategy unavailable: this index was built with --no-embed') if strategy is 'vector' and spec is NO_EMBED
  pool = Math.max(limit * 4, 20)

  vec = []
  if strategy in ['hybrid', 'vector'] and spec isnt NO_EMBED
    qEmb = await embedOne(spec, query)
    vec = await idx.vectorSearch(qEmb, pool)
  kw = if strategy in ['hybrid', 'keyword'] then await idx.keywordSearch(query, pool) else []

  # RRF over whichever ranked lists the strategy produced (a single list keeps
  # its native order — fusion only matters when both signals are present).
  lists = {}
  lists.vector = vec.map((r) -> r.slug) if vec.length
  lists.keyword = kw.map((r) -> r.slug) if kw.length
  { scores, contrib } = rrf(lists)

  # relational expansion: 1-hop neighbours of the top seeds carry relational_* meta.
  relational = {}
  if expand
    seeds = Object.entries(scores).sort((a, b) -> b[1] - a[1]).slice(0, 5).map (x) -> x[0]
    for seed in seeds
      for row in (await idx.outgoing(seed)).concat(await idx.incoming(seed))
        nbr = if row.from_slug is seed then row.to_slug else row.from_slug
        continue if scores[nbr]   # already a direct hit
        r = (relational[nbr] ?= { seed, hop: 1, path: [seed, nbr], via: [] })
        r.via.push(row.rel) unless row.rel in r.via
    # fold neighbours in with a small relational score
    for own nbr, meta of relational
      scores[nbr] = (scores[nbr] or 0) + 1 / (RRF_K + 20)
      contrib[nbr] ?= {}
      contrib[nbr].relational = meta.hop

  vecScore = {}; vecScore[r.slug] = r.score for r in vec
  ranked = Object.entries(scores).sort((a, b) -> b[1] - a[1]).slice(0, limit)

  # previews (live component values) for the final page, one indexed query
  slugs = (slug for [slug, _] in ranked)
  previews = {}
  if slugs.length
    pr = await idx.db.query 'SELECT slug, components FROM entities WHERE slug = ANY($1)', [slugs]
    for row in pr.rows
      previews[row.slug] = if typeof row.components is 'string' then JSON.parse(row.components) else row.components

  results = []
  for [slug, base] in ranked
    rel = relational[slug]
    res =
      slug: slug
      score: Number(base.toFixed(6))
      base_score: Number(base.toFixed(6))
      cosine: if vecScore[slug]? then Number(vecScore[slug].toFixed(4)) else null
      rerank: 'skipped(placeholder)'
    if rel
      res.relational_seed = rel.seed
      res.relational_hop = rel.hop
      res.relational_path = rel.path
      res.relational_via_link_types = rel.via
    if opts.explain
      res.explain = { ranks: contrib[slug], strategy, expand, reranker: core.cfg.search.reranker or 'off' }
    res.preview = previews[slug] if previews[slug] and Object.keys(previews[slug]).length
    results.push res
  results
