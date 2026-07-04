# index.coffee — the pglite (embedded Postgres + pgvector) engine.
#
# One-directional flow: authoritative .md  ->  pglite. `reindex` rebuilds the
# whole index from the loaded world. All search/query runs against pglite.
import { PGlite } from '@electric-sql/pglite'
import { vector } from '@electric-sql/pglite/vector'
import { paths } from './config.coffee'
import { detectDim, embedTexts, providerModel } from './embed.coffee'

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
    @db

  meta: (k) ->
    r = await @db.query 'SELECT v FROM meta WHERE k = $1', [k]
    r.rows[0]?.v

  isIndexed: ->
    r = await @db.query "SELECT to_regclass('public.chunks') AS table_name"
    !!r.rows[0]?.table_name

  # Full rebuild from the in-memory world. Detects embedding dim from the model.
  reindex: (world, embedModel) ->
    await @open()
    { provider, model } = providerModel(embedModel)
    dim = await detectDim(embedModel)
    await @db.exec 'DROP TABLE IF EXISTS chunks; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS entities;'
    await @db.exec """
      CREATE TABLE entities (
        slug text PRIMARY KEY, cls text, id text, source text, body text
      );
      CREATE TABLE links (
        from_slug text, rel text, to_slug text, qualifiers jsonb
      );
      CREATE TABLE chunks (
        slug text PRIMARY KEY,
        text text,
        tsv tsvector,
        embedding vector(#{dim})
      );
      CREATE INDEX chunks_tsv_idx ON chunks USING GIN (tsv);
    """
    ents = world.entities
    texts = ents.map(renderEntityText)
    # embed in batches (providers cap batch size / payload)
    embeddings = []
    BATCH = 64
    i = 0
    while i < texts.length
      slice = texts.slice(i, i + BATCH)
      embeddings = embeddings.concat(await embedTexts(embedModel, slice))
      i += BATCH
    for e, i in ents
      await @db.query 'INSERT INTO entities (slug, cls, id, source, body) VALUES ($1,$2,$3,$4,$5)',
        [e.slug, e.cls, e.id, e.source, e.body or '']
      await @db.query 'INSERT INTO chunks (slug, text, tsv, embedding) VALUES ($1,$2,to_tsvector(\'english\',$2),$3)',
        [e.slug, texts[i], toVec(embeddings[i])]
      for own rel, targets of (e.relations or {})
        for t in targets
          quals = {}
          quals[k] = v for own k, v of t when k isnt '_to'
          await @db.query 'INSERT INTO links (from_slug, rel, to_slug, qualifiers) VALUES ($1,$2,$3,$4)',
            [e.slug, rel, t._to, JSON.stringify(quals)]
    await @db.query 'INSERT INTO meta (k,v) VALUES ($1,$2) ON CONFLICT (k) DO UPDATE SET v=$2', ['embed_provider', provider]
    await @db.query 'INSERT INTO meta (k,v) VALUES ($1,$2) ON CONFLICT (k) DO UPDATE SET v=$2', ['embed_model', model]
    await @db.query 'INSERT INTO meta (k,v) VALUES ($1,$2) ON CONFLICT (k) DO UPDATE SET v=$2', ['embed_spec', embedModel]
    await @db.query 'INSERT INTO meta (k,v) VALUES ($1,$2) ON CONFLICT (k) DO UPDATE SET v=$2', ['embed_dim', String(dim)]
    { entities: ents.length, dim, provider, model }

  # Vector (semantic) search: returns [{ slug, score }] with score in [0,1] (1 = closest).
  vectorSearch: (queryEmbedding, limit = 20) ->
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

  # Relations for graph traversal.
  outgoing: (slug) ->
    r = await @db.query 'SELECT from_slug, rel, to_slug, qualifiers FROM links WHERE from_slug = $1', [slug]
    r.rows
  incoming: (slug) ->
    r = await @db.query 'SELECT from_slug, rel, to_slug, qualifiers FROM links WHERE to_slug = $1', [slug]
    r.rows

  entity: (slug) ->
    r = await @db.query 'SELECT * FROM entities WHERE slug = $1', [slug]
    r.rows[0]

  close: ->
    await @db.close() if @db
    @db = null
