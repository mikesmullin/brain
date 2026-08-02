# index.coffee — the pglite (embedded Postgres + pgvector) engine.
#
# While `brain server` runs, pglite is AUTHORITATIVE: queries and writes hit
# these tables only. Disk `.md` is read in exactly one place (`reindex`) and
# written in exactly one place (`export`). Traversal never touches the disk —
# it runs over the indexed `links` table (and its `adj` union view, the
# forward+reverse adjacency analog of a dual-CSR), so every hop is an index
# seek instead of a whole-world scan.
import { PGlite } from '@electric-sql/pglite'
import { vector } from '@electric-sql/pglite/vector'
import { paths } from './config.coffee'
import { detectDim, embedTexts, embedOne, providerModel } from './embed.coffee'

# Sentinel embed spec for graph-only datasets (e.g. benchmark loads): chunks
# get FTS only, vector search is skipped, and no embedding provider is called.
export NO_EMBED = 'none'

val2str = (v) ->
  if Array.isArray(v) then v.map(val2str).join(', ')
  else if v? and typeof v is 'object' then JSON.stringify(v)
  else String(v ? '')

# The searchable text projection of an entity (title + fields + relations + body).
export renderEntityText = (e) ->
  parts = [e.slug.replace('/', ' ')]
  for own alias, fields of (e.components or {})
    for own k, v of (fields or {})
      parts.push "#{alias}.#{k}: #{val2str(v)}"
  for own rel, targets of (e.relations or {})
    for t in (targets or [])
      quals = ("#{k}=#{val2str(v)}" for own k, v of t when k isnt '_to').join(' ')
      parts.push "#{rel} -> #{t._to}#{if quals then " (#{quals})" else ''}"
  parts.push(e.body) if e.body
  parts.join('\n')

toVec = (arr) -> '[' + arr.join(',') + ']'

parseJsonish = (v) -> if typeof v is 'string' then (try JSON.parse(v) catch then {}) else (v or {})

# derive the storage dir that owns an entity file (source = <dir>/<Class>/<id>.md)
storageDirOf = (e) ->
  suffix = "/#{e.cls}/#{e.id}.md"
  if e.source and e.source.endsWith(suffix) then e.source.slice(0, e.source.length - suffix.length) else null

export class Index
  constructor: (@cwd = process.cwd()) ->
    @dir = paths(@cwd).pgdata

  open: ->
    return @db if @db
    @db = await PGlite.create({ dataDir: @dir, extensions: { vector } })
    await @db.exec 'CREATE EXTENSION IF NOT EXISTS vector;'
    await @db.exec """
      CREATE TABLE IF NOT EXISTS meta (k text PRIMARY KEY, v text);
    """
    # Bring older pgdata dirs up to the current index set (no-op when present).
    # entities_cls_id_idx is required for streaming `brain ls` keyset scans on
    # multi-million-row graphs — without it each page is a full sort.
    if await @isIndexed()
      await @ensureIndexes()
    @db

  # Idempotent schema migrations for the live index (CREATE INDEX IF NOT EXISTS).
  # Called from open() for existing brains and mirrored in rebuild() for new ones.
  ensureIndexes: ->
    await @db.exec """
      CREATE INDEX IF NOT EXISTS entities_cls_id_idx ON entities (cls, id);
    """

  meta: (k) ->
    r = await @db.query 'SELECT v FROM meta WHERE k = $1', [k]
    r.rows[0]?.v

  setMeta: (k, v) ->
    await @db.query 'INSERT INTO meta (k,v) VALUES ($1,$2) ON CONFLICT (k) DO UPDATE SET v=$2', [k, String(v)]

  isIndexed: ->
    r = await @db.query "SELECT to_regclass('public.chunks') AS table_name"
    !!r.rows[0]?.table_name

  embedSpec: -> (await @meta('embed_spec')) or NO_EMBED

  # Multi-row batched INSERT (one statement per chunk instead of one per row —
  # matters at benchmark scale where per-statement WASM overhead dominates).
  # opts.into overrides the SQL column list and opts.tuple(placeholders) wraps
  # params in SQL expressions (e.g. to_tsvector) so derived columns are
  # computed during the insert instead of a full-table UPDATE afterwards.
  _insertMany: (table, cols, rows, chunkSize = 400, opts = {}) ->
    intoCols = opts.into or cols
    i = 0
    while i < rows.length
      slice = rows.slice(i, i + chunkSize)
      params = []
      tuples = slice.map (row) ->
        ph = cols.map (_, ci) ->
          params.push(row[ci])
          "$#{params.length}"
        if opts.tuple then opts.tuple(ph) else '(' + ph.join(',') + ')'
      await @db.query "INSERT INTO #{table} (#{intoCols.join(',')}) VALUES #{tuples.join(',')}", params
      i += chunkSize

  # Full rebuild from the in-memory world (the ONE sanctioned .md -> pglite
  # path). Search/GIN/HNSW indexes are created AFTER the bulk load, which is
  # substantially faster than maintaining them row-by-row during insert.
  reindex: (world, embedModel, opts = {}) ->
    await @open()
    noEmbed = opts.noEmbed or embedModel is NO_EMBED
    spec = if noEmbed then NO_EMBED else embedModel
    if noEmbed
      provider = 'none'; model = 'none'; dim = 0
    else
      { provider, model } = providerModel(embedModel)
      dim = await detectDim(embedModel)
    await @db.exec 'DROP VIEW IF EXISTS adj; DROP TABLE IF EXISTS chunks; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS entities;'
    embCol = if noEmbed then '' else ", embedding vector(#{dim})"
    await @db.exec """
      CREATE TABLE entities (
        slug text PRIMARY KEY, cls text, id text, source text, body text,
        components jsonb, storage_dir text
      );
      CREATE TABLE links (
        from_slug text, rel text, to_slug text, qualifiers jsonb
      );
      CREATE TABLE chunks (
        slug text PRIMARY KEY,
        text text,
        tsv tsvector#{embCol}
      );
      CREATE VIEW adj AS
        SELECT from_slug AS src, to_slug AS dst, rel, 'out' AS edge_dir FROM links
        UNION ALL
        SELECT to_slug AS src, from_slug AS dst, rel, 'in' AS edge_dir FROM links;
    """
    ents = world.entities
    texts = ents.map(renderEntityText)
    embeddings = []
    unless noEmbed
      BATCH = 64   # providers cap batch size / payload
      i = 0
      while i < texts.length
        slice = texts.slice(i, i + BATCH)
        embeddings = embeddings.concat(await embedTexts(embedModel, slice))
        i += BATCH
    await @_insertMany 'entities', ['slug', 'cls', 'id', 'source', 'body', 'components', 'storage_dir'],
      ([e.slug, e.cls, e.id, e.source or '', e.body or '', JSON.stringify(e.components or {}), storageDirOf(e) or ''] for e in ents)
    linkRows = []
    for e in ents
      for own rel, targets of (e.relations or {})
        for t in targets
          quals = {}
          quals[k] = v for own k, v of t when k isnt '_to'
          linkRows.push [e.slug, rel, t._to, JSON.stringify(quals)]
    await @_insertMany 'links', ['from_slug', 'rel', 'to_slug', 'qualifiers'], linkRows
    if noEmbed
      await @_insertMany 'chunks', ['slug', 'text'], ([e.slug, texts[i]] for e, i in ents), 200,
        { into: ['slug', 'text', 'tsv'], tuple: (ph) -> "(#{ph[0]},#{ph[1]},to_tsvector('english',#{ph[1]}))" }
    else
      await @_insertMany 'chunks', ['slug', 'text', 'embedding'],
        ([e.slug, texts[i], toVec(embeddings[i])] for e, i in ents), 100,
        { into: ['slug', 'text', 'tsv', 'embedding'], tuple: (ph) -> "(#{ph[0]},#{ph[1]},to_tsvector('english',#{ph[1]}),#{ph[2]})" }
    # indexes AFTER bulk load
    await @db.exec """
      CREATE INDEX entities_cls_idx ON entities (cls);
      CREATE INDEX entities_cls_id_idx ON entities (cls, id);
      CREATE INDEX entities_lower_idx ON entities (lower(slug));
      CREATE INDEX links_from_idx ON links (from_slug);
      CREATE INDEX links_to_idx ON links (to_slug);
      CREATE INDEX chunks_tsv_idx ON chunks USING GIN (tsv);
    """
    unless noEmbed
      # HNSW ANN index (pgvector 0.8.0): turns vector search from an O(N)
      # exact scan into an approximate index probe.
      await @db.exec 'CREATE INDEX chunks_embedding_idx ON chunks USING hnsw (embedding vector_cosine_ops);'
    await @setMeta 'embed_provider', provider
    await @setMeta 'embed_model', model
    await @setMeta 'embed_spec', spec
    await @setMeta 'embed_dim', String(dim)
    await @refreshStats({ entities: ents.length, links: linkRows.length })
    await @db.exec 'VACUUM ANALYZE;'
    { entities: ents.length, links: linkRows.length, dim, provider, model }

  # Incremental upsert for a single entity (the write path while the server
  # runs — pglite-first, no .md write; `export` materializes disk later).
  upsertEntity: (entity, embedModel = null) ->
    await @open()
    return { skipped: true, reason: 'not_indexed' } unless await @isIndexed()
    spec = await @embedSpec()
    text = renderEntityText(entity)
    await @db.query """
      INSERT INTO entities (slug, cls, id, source, body, components, storage_dir) VALUES ($1,$2,$3,$4,$5,$6,$7)
      ON CONFLICT (slug) DO UPDATE SET cls=$2, id=$3, source=$4, body=$5, components=$6, storage_dir=$7
    """, [entity.slug, entity.cls, entity.id, entity.source or '', entity.body or '',
          JSON.stringify(entity.components or {}), entity.storageDir or storageDirOf(entity) or '']
    if spec is NO_EMBED
      await @db.query """
        INSERT INTO chunks (slug, text, tsv) VALUES ($1,$2,to_tsvector('english',$2))
        ON CONFLICT (slug) DO UPDATE SET text=$2, tsv=to_tsvector('english',$2)
      """, [entity.slug, text]
    else
      emb = await embedOne(embedModel or spec, text)
      await @db.query """
        INSERT INTO chunks (slug, text, tsv, embedding) VALUES ($1,$2,to_tsvector('english',$2),$3)
        ON CONFLICT (slug) DO UPDATE SET text=$2, tsv=to_tsvector('english',$2), embedding=$3
      """, [entity.slug, text, toVec(emb)]
    await @db.query 'DELETE FROM links WHERE from_slug = $1', [entity.slug]
    for own rel, targets of (entity.relations or {})
      for t in (targets or [])
        quals = {}
        quals[k] = v for own k, v of t when k isnt '_to'
        await @db.query 'INSERT INTO links (from_slug, rel, to_slug, qualifiers) VALUES ($1,$2,$3,$4)',
          [entity.slug, rel, t._to, JSON.stringify(quals)]
    { slug: entity.slug }

  # Drop a slug from the index (after rm). No-op if not indexed.
  removeEntity: (slug) ->
    await @open()
    return { skipped: true, reason: 'not_indexed' } unless await @isIndexed()
    await @db.query 'DELETE FROM chunks WHERE slug = $1', [slug]
    await @db.query 'DELETE FROM links WHERE from_slug = $1 OR to_slug = $1', [slug]
    await @db.query 'DELETE FROM entities WHERE slug = $1', [slug]
    { slug }

  # Vector (semantic) search: [{ slug, score }] with score in [0,1] (1 = closest).
  # Served by the HNSW index; empty when the dataset was indexed with --no-embed.
  vectorSearch: (queryEmbedding, limit = 20) ->
    return [] if (await @embedSpec()) is NO_EMBED
    r = await @db.query """
      SELECT slug, 1 - (embedding <=> $1) AS score
      FROM chunks ORDER BY embedding <=> $1 ASC LIMIT $2
    """, [toVec(queryEmbedding), limit]
    r.rows

  # Keyword (lexical) search via Postgres FTS (ts_rank_cd as a BM25 stand-in).
  keywordSearch: (queryText, limit = 20) ->
    r = await @db.query """
      SELECT slug, ts_rank_cd(tsv, websearch_to_tsquery('english', $1)) AS score
      FROM chunks
      WHERE tsv @@ websearch_to_tsquery('english', $1)
      ORDER BY score DESC LIMIT $2
    """, [queryText, limit]
    r.rows

  # Relations (raw directed rows).
  outgoing: (slug) ->
    r = await @db.query 'SELECT from_slug, rel, to_slug, qualifiers FROM links WHERE from_slug = $1', [slug]
    r.rows
  incoming: (slug) ->
    r = await @db.query 'SELECT from_slug, rel, to_slug, qualifiers FROM links WHERE to_slug = $1', [slug]
    r.rows

  # Both directions in one indexed pass: [{ dir, rel, nbr }] (dir: out|in).
  neighbors: (slug, rel = null) ->
    sql = 'SELECT dst, rel, edge_dir FROM adj WHERE src = $1'
    args = [slug]
    if rel
      sql += ' AND rel = $2'
      args.push(rel)
    r = await @db.query sql, args
    ({ dir: row.edge_dir, rel: row.rel, nbr: row.dst } for row in r.rows)

  # One indexed query for a whole BFS frontier (directed, outgoing).
  frontierOut: (slugs, rel = null) ->
    return [] unless slugs.length
    sql = 'SELECT from_slug, rel, to_slug FROM links WHERE from_slug = ANY($1)'
    args = [slugs]
    if rel
      sql += ' AND rel = $2'
      args.push(rel)
    (await @db.query sql, args).rows

  # One indexed query for a whole reverse-BFS frontier (directed, incoming —
  # who points at these nodes). The backward half of bidirectional search.
  frontierIn: (slugs, rel = null) ->
    return [] unless slugs.length
    sql = 'SELECT from_slug, rel, to_slug FROM links WHERE to_slug = ANY($1)'
    args = [slugs]
    if rel
      sql += ' AND rel = $2'
      args.push(rel)
    (await @db.query sql, args).rows

  # One indexed query for a whole BFS frontier (undirected view, with direction tags).
  frontierAdj: (slugs) ->
    return [] unless slugs.length
    (await @db.query 'SELECT src, dst, rel, edge_dir FROM adj WHERE src = ANY($1)', [slugs]).rows

  entity: (slug) ->
    r = await @db.query 'SELECT * FROM entities WHERE slug = $1', [slug]
    r.rows[0]

  # Reconstruct the full entity object (components + relations + body) from
  # pglite — the query-time replacement for parsing the .md file.
  fullEntity: (slug) ->
    row = await @entity(slug)
    return null unless row
    rels = await @db.query 'SELECT rel, to_slug, qualifiers FROM links WHERE from_slug = $1 ORDER BY rel, to_slug', [slug]
    relations = {}
    for r in rels.rows
      target = Object.assign({ _to: r.to_slug }, parseJsonish(r.qualifiers))
      (relations[r.rel] ?= []).push(target)
    {
      slug: row.slug, cls: row.cls, id: row.id, source: row.source or ''
      body: row.body or '', components: parseJsonish(row.components), relations
      storageDir: row.storage_dir or ''
    }

  # Batch fullEntity: one entities query + one links query per chunk instead of
  # 2 point queries per slug — the difference between milliseconds and minutes
  # when hydrating a 36k-degree hub's neighbors.
  fullEntities: (slugs) ->
    out = {}
    return out unless slugs.length
    CHUNK = 5000
    i = 0
    while i < slugs.length
      batch = slugs.slice(i, i + CHUNK)
      ents = (await @db.query 'SELECT * FROM entities WHERE slug = ANY($1)', [batch]).rows
      rels = (await @db.query 'SELECT from_slug, rel, to_slug, qualifiers FROM links WHERE from_slug = ANY($1) ORDER BY from_slug, rel, to_slug', [batch]).rows
      for row in ents
        out[row.slug] = {
          slug: row.slug, cls: row.cls, id: row.id, source: row.source or ''
          body: row.body or '', components: parseJsonish(row.components), relations: {}
          storageDir: row.storage_dir or ''
        }
      for r in rels
        e = out[r.from_slug]
        continue unless e
        (e.relations[r.rel] ?= []).push Object.assign({ _to: r.to_slug }, parseJsonish(r.qualifiers))
      i += CHUNK
    out

  # Case-insensitive slug resolution (index-backed; replaces world.resolveSlug).
  resolveSlugDb: (input) ->
    raw = String(input ? '').trim()
    return null unless raw and raw.indexOf('/') > 0
    r = await @db.query 'SELECT slug FROM entities WHERE lower(slug) = lower($1) LIMIT 1', [raw]
    r.rows[0]?.slug or null

  classSlugs: (cls) ->
    r = await @db.query 'SELECT slug FROM entities WHERE lower(cls) = lower($1) ORDER BY slug', [cls]
    (row.slug for row in r.rows)

  classesOf: (slugs) ->
    return {} unless slugs.length
    r = await @db.query 'SELECT slug, cls FROM entities WHERE slug = ANY($1)', [slugs]
    out = {}
    out[row.slug] = row.cls for row in r.rows
    out

  listInstances: (cls = null) ->
    rows = []
    await @listInstancesEach cls, (row) -> rows.push(row)
    rows

  # Keyset-paginated walk over entities (cls, id). Invokes onRow once per
  # entity and never materializes the full result set — required for multi-
  # million-node graphs (brain ls streams these to the client).
  # Returns the number of rows visited.
  listInstancesEach: (cls = null, onRow, opts = {}) ->
    batchSize = opts.batchSize or 2000
    lastCls = null
    lastId = null
    total = 0
    loop
      if cls
        if lastId?
          r = await @db.query """
            SELECT cls, id FROM entities
            WHERE lower(cls) = lower($1) AND id > $2
            ORDER BY id
            LIMIT $3
          """, [cls, lastId, batchSize]
        else
          r = await @db.query """
            SELECT cls, id FROM entities
            WHERE lower(cls) = lower($1)
            ORDER BY id
            LIMIT $2
          """, [cls, batchSize]
      else
        if lastCls?
          r = await @db.query """
            SELECT cls, id FROM entities
            WHERE (cls, id) > ($1::text, $2::text)
            ORDER BY cls, id
            LIMIT $3
          """, [lastCls, lastId, batchSize]
        else
          r = await @db.query """
            SELECT cls, id FROM entities
            ORDER BY cls, id
            LIMIT $1
          """, [batchSize]
      rows = r.rows
      break if rows.length is 0
      for row in rows
        total++
        await onRow(row)
      last = rows[rows.length - 1]
      lastCls = last.cls
      lastId = last.id
      break if rows.length < batchSize
    total

  # Entities with zero relations, in or out (the `validate` orphan lint, on demand).
  orphans: ->
    rows = []
    await @orphansEach (row) -> rows.push({ slug: "#{row.cls}/#{row.id}", cls: row.cls })
    rows

  # Keyset-paginated orphan walk (cls, id) — stream-friendly for large graphs.
  # Same connectivity predicate as orphans(); ordered by cls, id like `brain ls`.
  orphansEach: (onRow, opts = {}) ->
    batchSize = opts.batchSize or 2000
    lastCls = null
    lastId = null
    total = 0
    orphanPred = """
      NOT EXISTS (SELECT 1 FROM links l WHERE l.from_slug = e.slug)
      AND NOT EXISTS (SELECT 1 FROM links l WHERE l.to_slug = e.slug)
    """
    loop
      if lastCls?
        r = await @db.query """
          SELECT e.cls, e.id FROM entities e
          WHERE #{orphanPred}
            AND (e.cls, e.id) > ($1::text, $2::text)
          ORDER BY e.cls, e.id
          LIMIT $3
        """, [lastCls, lastId, batchSize]
      else
        r = await @db.query """
          SELECT e.cls, e.id FROM entities e
          WHERE #{orphanPred}
          ORDER BY e.cls, e.id
          LIMIT $1
        """, [batchSize]
      rows = r.rows
      break if rows.length is 0
      for row in rows
        total++
        await onRow(row)
      last = rows[rows.length - 1]
      lastCls = last.cls
      lastId = last.id
      break if rows.length < batchSize
    total

  counts: ->
    e = await @db.query 'SELECT count(*)::int AS n FROM entities'
    l = await @db.query 'SELECT count(*)::int AS n FROM links'
    { entities: e.rows[0].n, links: l.rows[0].n }

  # Per-class instance counts (for `schema graph` labels) — pure SQL, no .md.
  classCounts: ->
    r = await @db.query 'SELECT cls, count(*)::int AS n FROM entities GROUP BY cls ORDER BY cls'
    out = {}
    out[row.cls] = row.n for row in r.rows
    out

  # Slug presence set for ref validation (values are `true`, not full entities).
  slugSet: ->
    r = await @db.query 'SELECT slug FROM entities'
    out = {}
    out[row.slug] = true for row in r.rows
    out

  # Cached size counters: live COUNT(*) over millions of rows costs hundreds of
  # ms, so `status` reads these instead. Refresh is deliberately USER-controlled
  # maintenance (reindex sets them; `vacuum` recounts) — never automatic.
  refreshStats: (known = null) ->
    c = known or await @counts()
    await @setMeta 'stat_entities', String(c.entities)
    await @setMeta 'stat_links', String(c.links)
    await @setMeta 'stats_refreshed_at', new Date().toISOString()
    c

  cachedCounts: ->
    e = await @meta('stat_entities')
    l = await @meta('stat_links')
    return null unless e? and l?
    { entities: parseInt(e, 10), links: parseInt(l, 10), stats_refreshed_at: await @meta('stats_refreshed_at') }

  allEntities: ->
    # For export: stream all rows + grouped relations. Rows come back in one
    # result set — acceptable for an explicit out-of-band operation.
    ents = (await @db.query 'SELECT * FROM entities ORDER BY slug').rows
    rels = (await @db.query 'SELECT from_slug, rel, to_slug, qualifiers FROM links ORDER BY from_slug, rel, to_slug').rows
    bySlug = {}
    for row in ents
      bySlug[row.slug] = {
        slug: row.slug, cls: row.cls, id: row.id, source: row.source or ''
        body: row.body or '', components: parseJsonish(row.components), relations: {}
        storageDir: row.storage_dir or ''
      }
    for r in rels
      e = bySlug[r.from_slug]
      continue unless e
      target = Object.assign({ _to: r.to_slug }, parseJsonish(r.qualifiers))
      (e.relations[r.rel] ?= []).push(target)
    (bySlug[row.slug] for row in ents)

  vacuum: ->
    await @db.exec 'VACUUM ANALYZE;'
    stats = await @refreshStats()   # maintenance-controlled counter refresh
    { ok: true, entities: stats.entities, links: stats.links }

  close: ->
    await @db.close() if @db
    @db = null
