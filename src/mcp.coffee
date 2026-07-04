# mcp.coffee — Model Context Protocol server over stdio (default) or http.
# Exposes brain's read/query surfaces + a validated write path. This is the
# sanctioned way for an external agent to use the brain (never direct file edits):
# every write goes through validation, and validation/lint errors are returned as
# MCP tool errors.
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
import { isRelationKey } from './storage.coffee'
import { applicableMethods, invokeMethod, signatureOf } from './components.coffee'

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
    description: 'Create/update an entity. `content` is flattened YAML frontmatter (lowercase keys = components, UPPERCASE keys = relations). Validates before writing; returns a tool error on validation failure.'
    inputSchema:
      type: 'object'
      properties:
        slug: { type: 'string' }
        content: { type: 'string' }
        overwrite: { type: 'boolean' }
      required: ['slug', 'content']
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
        r = await upsertEntity(world, entity)
        textResult({ slug: entity.slug, path: r.path, warnings: r.warnings })
      catch err
        errorResult("validation failed: #{err.message}")
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
