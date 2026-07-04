# search.coffee — hybrid retrieval, mirroring gbrain's strategy:
#   keyword(FTS) + vector  ->  RRF(k=60) fusion  ->  relational expansion
#   ->  [cross-encoder rerank: PLACEHOLDER / config-gated no-op]
#
# NOTE: RRF is *fusion*, not a reranker. The cross-encoder rerank stage is a
# deliberate placeholder (config `search.reranker`, default off) to be wired later.
import { Index } from './index.coffee'
import { loadConfig } from './config.coffee'
import { loadWorld } from './world.coffee'
import { embedOne } from './embed.coffee'

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

export hybridSearch = (cwd, query, opts = {}) ->
  limit = opts.limit or 10
  cfg = await loadConfig(cwd)
  idx = new Index(cwd)
  await idx.open()
  unless await idx.isIndexed()
    world = await loadWorld(cwd)
    console.error "search index missing; indexing #{world.entities.length} entities..."
    await idx.reindex(world, cfg.embed.model)

  model = (await idx.meta('embed_spec')) or cfg.embed.model
  qEmb = await embedOne(model, query)

  pool = Math.max(limit * 4, 20)
  vec = await idx.vectorSearch(qEmb, pool)
  kw = await idx.keywordSearch(query, pool)

  vecRanked = vec.map (r) -> r.slug
  kwRanked = kw.map (r) -> r.slug
  { scores, contrib } = rrf({ vector: vecRanked, keyword: kwRanked })

  # relational expansion: 1-hop neighbours of the top seeds carry relational_* meta.
  seeds = Object.entries(scores).sort((a, b) -> b[1] - a[1]).slice(0, 5).map (x) -> x[0]
  relational = {}
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

  results = for [slug, base], i in ranked
    e = await idx.entity(slug)
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
      res.explain = { ranks: contrib[slug], reranker: cfg.search.reranker or 'off' }
    res

  await idx.close()
  results
