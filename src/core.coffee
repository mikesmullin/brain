# core.coffee — the ONE implementation of every query + write operation.
#
# CLI commands and MCP tools are thin adapters over this class (adapter
# pattern): they parse their own input shape, call the same core method, and
# format the same result. There is deliberately no second implementation
# anywhere — editing core.coffee immediately serves both surfaces, so the
# CLI/MCP staleness-drift bug class is impossible by construction.
#
# Query time is 100% pglite: no .md reads, no world loads. Disk is read only
# by `reindex` and written only by `exportMd`.
import yaml from 'js-yaml'
import { loadConfig, storageDirs, paths } from './config.coffee'
import { loadSchema } from './schema.coffee'
import { Index, renderEntityText, NO_EMBED } from './index.coffee'
import { parseSlug, formatSlug } from './slug.coffee'
import { isRelationKey, reconcileBodyLinks, serializeEntity, writeEntityFile, removeEntityFile, discoverEntityFiles } from './storage.coffee'
import { validateData } from './validate.coffee'
import { canonicalizeIds, idFieldOf } from './canonical.coffee'
import { applicableMethods, normalizeResult, signatureOf } from './components.coffee'
import { hybridSearch } from './search.coffee'
import { think } from './think.coffee'
import { ontologyQuery } from './ontology.coffee'
import { runQuery } from './graphmatch.coffee'
import { runGraphql } from './graphqlish.coffee'
import { basename, join } from 'path'
import { relative, sep } from 'path'

# Flattened-YAML `content` (lowercase keys = components, UPPERCASE = relations) -> entity.
export contentToEntity = (slug, content) ->
  { cls, id } = parseSlug(slug)
  data = yaml.load(content) or {}
  components = {}; relations = {}
  for own k, v of data when k not in ['_class', '_id']
    if isRelationKey(k)
      relations[k] = (if Array.isArray(v) then v else [v]).map (t) -> if typeof t is 'string' then { _to: t } else t
    else components[k] = v
  { slug: formatSlug(cls, id), cls, id, components, relations, body: '' }

export class Core
  constructor: (@cwd = process.cwd()) ->

  init: ->
    @cfg = await loadConfig(@cwd)
    @dirs = await storageDirs(@cwd, @cfg)
    @schema = await loadSchema(@dirs)
    @idx = new Index(@cwd)
    await @idx.open()
    @

  close: -> await @idx?.close()

  isIndexed: -> await @idx.isIndexed()

  # ---- resolution ------------------------------------------------------------

  resolveSlug: (input) -> await @idx.resolveSlugDb(input)

  requireSlug: (input) ->
    slug = await @resolveSlug(input)
    throw new Error("not found: #{input}") unless slug
    slug

  # ---- shallow (per-entity) validation --------------------------------------
  # Reuses validateData with a bySlug map populated from SQL for just the refs
  # this entity mentions — O(refs) instead of O(all entities). Whole-graph
  # lints (orphans) are excluded here; `schema orphans` covers those on demand.
  validateEntity: (entity) ->
    targets = []
    for own rel, ts of (entity.relations or {})
      targets.push(t._to) for t in (ts or []) when t._to
    for own alias, fields of (entity.components or {})
      for own k, v of (fields or {})
        targets.push(v) if typeof v is 'string' and v.indexOf('/') > 0
    targets = [...new Set(targets)]
    bySlug = {}
    bySlug[entity.slug] = entity
    if targets.length
      r = await @idx.db.query 'SELECT slug FROM entities WHERE slug = ANY($1)', [targets]
      bySlug[row.slug] = true for row in r.rows
    res = validateData({ schema: @schema, entities: [entity], bySlug, duplicates: [] })
    warnings = res.warnings.filter (m) -> not m.startsWith('orphan:')
    { valid: res.errors.length is 0, errors: res.errors, warnings }

  # ---- queries (the 5 search surfaces + reads) -------------------------------

  search: (query, opts = {}) -> await hybridSearch(@, query, opts)
  think: (question, opts = {}) -> await think(@, question, opts)
  ontology: (question, opts = {}) -> await ontologyQuery(@, question, opts)
  graph: (pattern, opts = {}) -> await runQuery(@, pattern, opts)
  graphql: (query) -> await runGraphql(@, query)

  getEntity: (slugRaw, includeLinks = false) ->
    slug = await @requireSlug(slugRaw)
    e = await @idx.fullEntity(slug)
    out = { slug: e.slug, components: e.components, relations: e.relations }
    out.body = e.body if e.body
    if includeLinks
      out.incoming = ({ from: row.from_slug, rel: row.rel } for row in await @idx.incoming(slug))
    out

  # Canonical .md rendering of the live (pglite) entity — what `get` prints.
  renderEntity: (slugRaw) ->
    slug = await @requireSlug(slugRaw)
    e = await @idx.fullEntity(slug)
    serializeEntity(e)

  # Case-insensitive schema class presence (CLI class args are often lowercase).
  _schemaHasClass: (name) ->
    return false unless name? and @schema?.classes?
    return true if @schema.classes[name]?
    l = String(name).toLowerCase()
    for k of @schema.classes
      return true if k.toLowerCase() is l
    false

  # Stream every instance as {cls, id} via onItem — does not build a byClass map.
  # Used by the server's NDJSON stream for `brain ls` (2M+ entity graphs).
  lsEach: (cls, onItem) ->
    count = await @idx.listInstancesEach cls, (row) -> await onItem(row)
    if cls and count is 0 and not @_schemaHasClass(cls)
      throw new Error("unknown class '#{cls}'")
    count

  # Bulk collect (tests / small graphs). Prefer lsEach for large result sets.
  ls: (cls = null) ->
    byClass = {}
    await @lsEach cls, (r) -> (byClass[r.cls] ?= []).push(r.id)
    byClass

  schemaMethods: (cls) ->
    throw new Error("unknown class: #{cls}") unless @schema.classes?[cls]
    methods = await applicableMethods(@cwd, @schema, cls)
    ({ signature: signatureOf(m.method, m.def), description: m.def.description or '' } for m in methods)

  schemaOrphans: -> await @idx.orphans()

  # Stream orphans as {cls, id} (for CLI listing — same shape as lsEach).
  schemaOrphansEach: (onItem) ->
    await @idx.orphansEach (row) -> await onItem(row)

  # T-box only (from server-resident schema — no entity .md load).
  schemaInfo: ->
    {
      components: @schema.components or {}
      classes: @schema.classes or {}
      relations: @schema.relations or {}
    }

  # schema graph labels with live per-class counts from pglite.
  schemaGraphView: ->
    { schemaGraph } = await import('./schema.coffee')
    counts = await @idx.classCounts()
    g = schemaGraph(@schema, counts)
    # Graph-wide totals: instance nodes, link edges, T-box component types.
    stats = (await @idx.cachedCounts()) or await @idx.counts()
    g.totals =
      nodes: stats.entities
      relationships: stats.links
      components: Object.keys(@schema.components or {}).length
    g

  # Full-graph validate from pglite (batched). Never reads entity .md files.
  validateAll: (opts = {}) ->
    bySlug = await @idx.slugSet()
    errors = []
    warnings = []
    n = 0
    batch = []
    BATCH = opts.batchSize or 500
    flush = =>
      return unless batch.length
      map = await @idx.fullEntities(batch)
      ents = (map[s] for s in batch when map[s])
      res = validateData({ schema: @schema, entities: ents, bySlug, duplicates: [] }, { skipOrphans: true, lenient: !!opts.lenient })
      errors.push(e) for e in res.errors
      warnings.push(w) for w in res.warnings
      n += ents.length
      batch = []
    await @idx.listInstancesEach null, (row) =>
      batch.push(formatSlug(row.cls, row.id))
      await flush() if batch.length >= BATCH
    await flush()
    # Orphan lint via SQL (same definition as validateData's degree check).
    for row in await @idx.orphans()
      warnings.push("orphan: #{row.slug} has no relations (incoming or outgoing)")
    {
      valid: errors.length is 0
      errors
      warnings
      counts:
        entities: n
        classes: Object.keys(@schema.classes or {}).length
        relations: Object.keys(@schema.relations or {}).length
    }

  # Deterministic bidirectional BFS (global visited set, batched one SQL query
  # per hop) from `from` to every entity of class `toClass` within `maxHops`.
  # Edge notation in `via`: `REL>` outgoing, `<REL` incoming.
  findPaths: (from, toClass, maxHops = 4, opts = {}) ->
    maxResults = opts.maxResults or 60
    maxNodes = opts.maxNodes or 100000
    results = []
    seen = {}
    seen[from] = true
    visited = 1
    capped = false
    frontier = [{ slug: from, path: [from], via: [] }]
    hops = 0
    while frontier.length and results.length < maxResults and hops < maxHops
      rows = await @idx.frontierAdj(frontier.map (f) -> f.slug)
      bySrc = {}
      (bySrc[r.src] ?= []).push(r) for r in rows
      next = []
      for f in frontier
        for row in (bySrc[f.slug] or [])
          continue if seen[row.dst]
          seen[row.dst] = true
          visited++
          edge = if row.edge_dir is 'out' then "#{row.rel}>" else "<#{row.rel}"
          next.push({ slug: row.dst, path: f.path.concat(row.dst), via: f.via.concat(edge) })
          if visited >= maxNodes
            capped = true
            break
        break if capped
      hops++
      # class check for the whole new frontier in one query
      if next.length
        classes = await @idx.classesOf(next.map (n) -> n.slug)
        for n in next
          if classes[n.slug] is toClass and results.length < maxResults
            results.push({ end: n.slug, hops, path: n.path, via: n.via })
      capped = true if results.length >= maxResults
      break if capped
      frontier = next
    { results, capped }

  # ---- writes (pglite-first; .md is materialized later by exportMd) ---------

  putEntity: (slugRaw, content, overwrite = false) ->
    slug = parseSlug(slugRaw).slug
    existing = await @idx.entity(slug)
    throw new Error("#{slug} already exists (set overwrite=true to replace)") if existing and not overwrite
    entity = contentToEntity(slug, content)
    entity.body = existing.body if existing?.body and not entity.body
    entity.storageDir = existing?.storage_dir or @schema.classDirs?[entity.cls] or paths(@cwd).storage
    reconcileBodyLinks(entity)
    res = await @validateEntity(entity)
    await @idx.upsertEntity(entity, @cfg.embed.model)
    { slug: entity.slug, valid: res.valid, warnings: res.warnings, validationErrors: res.errors }

  deleteEntity: (slugRaw) ->
    slug = await @requireSlug(slugRaw)
    await @idx.removeEntity(slug)
    { slug, removed: true, note: 'removed from the live index; run `brain export --prune` to also remove the .md file' }

  # `set <Class/id> k=v ...` / `new <Class> k=v ...` (id derived from idField).
  setInstance: (slugRaw, assignments = []) ->
    if slugRaw.indexOf('/') > 0
      { cls, id } = parseSlug(slugRaw)
      existingSlug = await @resolveSlug(slugRaw)
      entity = if existingSlug then await @idx.fullEntity(existingSlug) else { slug: formatSlug(cls, id), cls, id, components: {}, relations: {}, body: '' }
    else
      cls = slugRaw
      throw new Error("unknown class '#{cls}'") unless @schema.classes?[cls]
      throw new Error("class '#{cls}' has no idField; give an explicit id (#{cls}/<id>)") unless idFieldOf(@schema, cls)
      entity = { slug: null, cls, id: null, components: {}, relations: {}, body: '' }
    await @applyAssignments(entity, assignments)
    unless entity.id
      { calcResolver } = await import('./refine.coffee')
      await canonicalizeIds(@schema, [entity], { calc: calcResolver(@cwd) })
      throw new Error("could not derive an id for #{cls} (idField unresolved — set the id-source field/relation, or give an explicit id)") unless entity.id
    entity.storageDir or= @schema.classDirs?[entity.cls] or paths(@cwd).storage
    reconcileBodyLinks(entity)
    res = await @validateEntity(entity)
    await @idx.upsertEntity(entity, @cfg.embed.model)
    { slug: entity.slug, valid: res.valid, warnings: res.warnings, validationErrors: res.errors }

  # Apply `alias.field=value` (component) and `REL=Class/id` (relation) assignments.
  applyAssignments: (entity, assignments) ->
    for a in assignments
      eq = a.indexOf('=')
      throw new Error("assignment must be key=value, got '#{a}'") unless eq > 0
      key = a.slice(0, eq)
      rawVal = a.slice(eq + 1)
      if isRelationKey(key)
        target = (await @resolveSlug(rawVal)) or parseSlug(rawVal).slug
        entity.relations[key] ?= []
        entity.relations[key] = entity.relations[key].filter (t) -> t._to isnt target
        entity.relations[key].push({ _to: target })
      else
        [alias, field] = key.split('.')
        throw new Error("component assignment key must be alias.field, got '#{key}'") unless alias and field
        entity.components[alias] ?= {}
        entity.components[alias][field] = yaml.load(rawVal)
    entity

  link: (fromRaw, rel, toRaw, qualifiers = {}) ->
    from = await @requireSlug(fromRaw)
    to = (await @resolveSlug(toRaw)) or parseSlug(toRaw).slug
    entity = await @idx.fullEntity(from)
    target = Object.assign({ _to: to }, qualifiers)
    entity.relations[rel] ?= []
    entity.relations[rel] = entity.relations[rel].filter (t) -> t._to isnt to
    entity.relations[rel].push(target)
    res = await @validateEntity(entity)
    await @idx.upsertEntity(entity, @cfg.embed.model)
    { from, rel, to, valid: res.valid, warnings: res.warnings, validationErrors: res.errors }

  methodInvoke: (slugRaw, methodName, args = {}) ->
    slug = await @requireSlug(slugRaw)
    e = await @idx.fullEntity(slug)
    applicable = await applicableMethods(@cwd, @schema, e.cls)
    hit = applicable.find (m) -> m.method is methodName
    return { success: false, error: "#{e.cls} has no component method '#{methodName}'", content: '' } unless hit
    before = serializeEntity(e)
    result = normalizeResult(await hit.def.fn(e, hit.alias, args))
    if result.success and serializeEntity(e) isnt before
      await @idx.upsertEntity(e, @cfg.embed.model)
    { success: result.success, error: result.error, content: result.content }

  # ---- maintenance (out-of-band; the server gates queries while these run) --

  # The ONE .md -> pglite path. Explicit, user-initiated, rare.
  reindex: (opts = {}) ->
    { loadWorld } = await import('./world.coffee')
    world = await loadWorld(@cwd)
    model = if opts.noEmbed then NO_EMBED else @cfg.embed.model
    res = await @idx.reindex(world, model, { noEmbed: !!opts.noEmbed })
    @schema = world.schema   # pick up schema.yaml edits made since startup
    res

  # The ONE pglite -> .md path. Explicit, user-initiated. Overwrites .md from
  # the live index; with prune=true also deletes .md files for entities that
  # no longer exist in the index.
  exportMd: (opts = {}) ->
    ents = await @idx.allEntities()
    primary = paths(@cwd).storage
    written = 0
    for e in ents
      dir = e.storageDir or @schema.classDirs?[e.cls] or primary
      await writeEntityFile(dir, e)
      written++
    pruned = []
    if opts.prune
      live = new Set(ents.map (e) -> e.slug)
      for dir in @dirs
        for fp in await discoverEntityFiles(dir)
          rel = relative(dir, fp).split(sep)
          continue unless rel.length is 2
          slug = "#{rel[0]}/#{basename(rel[1], '.md')}"
          unless live.has(slug)
            await removeEntityFile(dir, rel[0], basename(rel[1], '.md'))
            pruned.push(slug)
    { written, pruned: pruned.length, prunedSlugs: pruned }

  vacuum: -> await @idx.vacuum()

  # Connected components (maintenance: union-find + component_id writeback).
  components: (log = null) ->
    { computeComponents } = await import('./analysis.coffee')
    await computeComponents(@idx, log or (->))

  componentsStats: ->
    { componentsStats } = await import('./analysis.coffee')
    (await componentsStats(@idx)) or { error: 'not computed — run `brain server components` first' }

  # Viz layout (maintenance: positions + colors cached to db/viz/*).
  vizLayout: (opts = {}, log = null) ->
    fs = await import('fs')
    fsp = await import('fs/promises')
    metaPath = join(paths(@cwd).root, 'viz', 'meta.json')
    if fs.existsSync(metaPath) and not opts.force
      cached = JSON.parse(await fsp.readFile(metaPath, 'utf-8'))
      return Object.assign(cached, { cached: true })
    analysis = await import('./analysis.coffee')
    await analysis.vizLayout(@idx, @cwd, log or (->))

  status: ->
    indexed = await @isIndexed()
    out = { indexed, root: paths(@cwd).root }
    if indexed
      # cached counters (refreshed by reindex/vacuum — user-controlled); live
      # COUNT(*) only as a fallback for indexes built before the cache existed
      Object.assign(out, (await @idx.cachedCounts()) or await @idx.counts())
      out.embed = await @idx.embedSpec()
    out.classes = Object.keys(@schema.classes or {}).length
    out
