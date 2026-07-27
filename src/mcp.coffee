# mcp.coffee — Model Context Protocol server over stdio (default) or http.
# Exposes brain's read/query surfaces + a write path. This is the sanctioned way
# for an external agent to use the brain (never direct file edits). put_entity
# always persists; schema validation is reported as soft issues to fix up when
# possible, not as a hard tool error that blocks storage.
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js'
import yaml from 'js-yaml'
import { loadWorld, resolveSlug } from './world.coffee'
import { hybridSearch } from './search.coffee'
import { think } from './think.coffee'
import { ontologyQuery } from './ontology.coffee'
import { runQuery } from './graphmatch.coffee'
import { runGraphql } from './graphqlish.coffee'
import { upsertEntity } from './upsert.coffee'
import { parseSlug, formatSlug } from './slug.coffee'
import { isRelationKey, removeEntityFile } from './storage.coffee'
import { dirname } from 'path'
import { applicableMethods, invokeMethod, signatureOf } from './components.coffee'
import { Index } from './index.coffee'
import { loadConfig } from './config.coffee'

TOOLS = [
  {
    name: 'search'
    description: 'Hybrid (vector + keyword + RRF) search over the knowledge graph. Returns YAML results.'
    inputSchema:
      type: 'object'
      properties:
        query: { type: 'string' }
        limit: { type: 'number' }
        explain: { type: 'boolean' }
      required: ['query']
  }
  {
    name: 'think'
    description: 'Search + LLM synthesis: returns a grounded answer with citations and gaps.'
    inputSchema:
      type: 'object'
      properties: { question: { type: 'string' }, limit: { type: 'number' } }
      required: ['question']
  }
  {
    name: 'ontology'
    description: 'LLM-driven typed relationship traversal (multi-hop graph questions).'
    inputSchema:
      type: 'object'
      properties: { question: { type: 'string' } }
      required: ['question']
  }
  {
    name: 'graph'
    description: 'Deterministic structural graph-match (Mermaid syntax), e.g. "Team -->|SUPPORTS| Product".'
    inputSchema:
      type: 'object'
      properties: { pattern: { type: 'string' } }
      required: ['pattern']
  }
  {
    name: 'graphql'
    description: 'Deterministic GraphQL-ish traversal, e.g. "Team/cloud { naming, USES_SYSTEM { info } }".'
    inputSchema:
      type: 'object'
      properties: { query: { type: 'string' } }
      required: ['query']
  }
  {
    name: 'get_entity'
    description: 'Read one entity (components + relations, optionally incoming links) by slug.'
    inputSchema:
      type: 'object'
      properties: { slug: { type: 'string' }, include_links: { type: 'boolean' } }
      required: ['slug']
  }
  {
    name: 'put_entity'
    description: 'Create/update an entity. `content` is flattened YAML frontmatter (lowercase keys = components, UPPERCASE keys = relations). Always writes the record even if schema validation fails; invalid writes succeed with a notice listing issues to fix when possible (overwrite=true).'
    inputSchema:
      type: 'object'
      properties:
        slug: { type: 'string' }
        content: { type: 'string' }
        overwrite: { type: 'boolean' }
      required: ['slug', 'content']
  }
  {
    name: 'delete_entity'
    description: 'Permanently remove an entity by slug (Class/id), e.g. Note/family-kids or Person/lsmullin. Deletes the .md file and drops it from the search index. Same as CLI `brain rm`. Does not cascade-delete other entities that only link to it.'
    inputSchema:
      type: 'object'
      properties:
        slug: { type: 'string', description: 'Entity slug Class/id to remove' }
      required: ['slug']
  }
  {
    name: 'schema_methods'
    description: 'List the ECS component methods applicable to a CLASS, with signatures.'
    inputSchema:
      type: 'object'
      properties: { class: { type: 'string' } }
      required: ['class']
  }
  {
    name: 'method_invoke'
    description: 'Invoke an ECS component method on an entity (by slug). Returns the method\'s content string (or an error).'
    inputSchema:
      type: 'object'
      properties:
        slug: { type: 'string' }
        method: { type: 'string' }
        params: { type: 'object' }
      required: ['slug', 'method']
  }
]

textResult = (obj) -> { content: [{ type: 'text', text: (if typeof obj is 'string' then obj else yaml.dump(obj, { lineWidth: 120, sortKeys: false, noRefs: true })) }] }
errorResult = (msg) -> { content: [{ type: 'text', text: msg }], isError: true }

# Agent-facing put_entity outcome: always "saved" when write succeeded; invalid
# schema is a soft notice, not isError.
formatPutEntityResult = (entity, r) ->
  base =
    slug: entity.slug
    path: r.path
    valid: r.valid isnt false
    warnings: r.warnings or []
  if r.valid isnt false
    return base
  reasons = r.validationErrors or []
  notice = [
    "Record was created and persisted at #{r.path}, but it is considered INVALID for the following reasons:"
    (reasons.map (m) -> "  - #{m}").join('\n') or '  - (unspecified validation errors)'
    ''
    'Fixing these is not mandatory for the data to be stored and persisted.'
    'If you can, try an update (overwrite=true) with corrected content so the record becomes valid.'
  ].join('\n')
  Object.assign base,
    valid: false
    validation_errors: reasons
    notice: notice

contentToEntity = (slug, content) ->
  { cls, id } = parseSlug(slug)
  data = yaml.load(content) or {}
  components = {}; relations = {}
  for own k, v of data when k not in ['_class', '_id']
    if isRelationKey(k)
      relations[k] = (if Array.isArray(v) then v else [v]).map (t) -> if typeof t is 'string' then { _to: t } else t
    else components[k] = v
  { slug: formatSlug(cls, id), cls, id, components, relations, body: '' }

handleCall = (cwd, name, args) ->
  switch name
    when 'search'
      textResult(await hybridSearch(cwd, args.query, { limit: args.limit or 10, explain: !!args.explain }))
    when 'think'
      textResult(await think(cwd, args.question, { limit: args.limit or 8 }))
    when 'ontology'
      textResult(await ontologyQuery(cwd, args.question))
    when 'graph'
      textResult(await runQuery(cwd, args.pattern))
    when 'graphql'
      textResult(await runGraphql(cwd, args.query))
    when 'get_entity'
      world = await loadWorld(cwd)
      e = resolveSlug(world, args.slug)
      return errorResult("not found: #{args.slug}") unless e
      slug = e.slug
      out = { slug: e.slug, components: e.components, relations: e.relations }
      if args.include_links
        out.incoming = ({ from: o.slug, rel } for o in world.entities for own rel, ts of (o.relations or {}) when ts.some((t) -> t._to is slug))
      textResult(out)
    when 'put_entity'
      world = await loadWorld(cwd)
      slug = parseSlug(args.slug).slug
      return errorResult("#{slug} already exists (set overwrite=true to replace)") if world.bySlug[slug] and not args.overwrite
      try
        entity = contentToEntity(slug, args.content)
        # Soft validation: always persist; report schema issues for optional fix-up.
        r = await upsertEntity(world, entity, { strict: false })
        # Keep pglite search in sync — .md is SoT but search previously lagged
        # until a manual `brain reindex`, so Ada could "save" then fail recall.
        try
          cfg = await loadConfig(cwd)
          idx = new Index(cwd)
          await idx.open()
          unless await idx.isIndexed()
            # First write: build full index from disk (includes this entity).
            world2 = await loadWorld(cwd)
            await idx.reindex(world2, cfg.embed.model)
          else
            # Reload entity from disk so source path is set for the index row.
            world2 = await loadWorld(cwd)
            e2 = world2.bySlug[entity.slug] or entity
            await idx.upsertEntity(e2, cfg.embed.model)
          await idx.close()
        catch idxErr
          # Non-fatal: get_entity still works; search may lag until reindex.
          process.stderr.write("brain mcp: index update failed after put_entity: #{idxErr.message}\n")
        textResult formatPutEntityResult(entity, r)
      catch err
        errorResult("put_entity failed: #{err.message}")
    when 'delete_entity'
      world = await loadWorld(cwd)
      e = resolveSlug(world, args.slug)
      return errorResult("not found: #{args.slug}") unless e
      try
        # Prefer source path dir (multi-storage); fall back to class dir / primary.
        storageDir = null
        if e.source
          suffix = "/#{e.cls}/#{e.id}.md"
          storageDir = if e.source.endsWith(suffix) then e.source.slice(0, e.source.length - suffix.length) else dirname(dirname(e.source))
        storageDir or= world.schema.classDirs?[e.cls]
        storageDir or= world.primaryStorageDir
        await removeEntityFile(storageDir, e.cls, e.id)
        try
          cfg = await loadConfig(cwd)
          idx = new Index(cwd)
          await idx.open()
          if await idx.isIndexed()
            await idx.removeEntity(e.slug)
          await idx.close()
        catch idxErr
          process.stderr.write("brain mcp: index update failed after delete_entity: #{idxErr.message}\n")
        textResult({ slug: e.slug, removed: true, path: e.source or "#{storageDir}/#{e.cls}/#{e.id}.md" })
      catch err
        errorResult("delete_entity failed: #{err.message}")
    when 'schema_methods'
      world = await loadWorld(cwd)
      cls = args.class
      return errorResult("unknown class: #{cls}") unless world.schema.classes?[cls]
      methods = await applicableMethods(cwd, world.schema, cls)
      textResult("#{signatureOf(m.method, m.def)}#{if m.def.description then '  # ' + m.def.description else ''}" for m in methods)
    when 'method_invoke'
      world = await loadWorld(cwd)
      e = resolveSlug(world, args.slug)
      return errorResult("not found: #{args.slug}") unless e
      r = await invokeMethod(cwd, world, e.slug, args.method, args.params or {})
      text = (if not r.success and r.error then "#{r.error}\n" else '') + (r.content or '')
      if r.success then textResult(text) else errorResult(text)
    else
      errorResult("unknown tool: #{name}")

export startStdio = (cwd = process.cwd()) ->
  server = new Server({ name: 'brain', version: '0.1.0' }, { capabilities: { tools: {} } })
  server.setRequestHandler ListToolsRequestSchema, -> { tools: TOOLS }
  server.setRequestHandler CallToolRequestSchema, (req) ->
    try
      await handleCall(cwd, req.params.name, req.params.arguments or {})
    catch err
      errorResult("error: #{err.message}")
  await server.connect(new StdioServerTransport())
  process.stderr.write("brain MCP server ready (stdio)\n")
