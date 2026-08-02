# mcp.coffee — Model Context Protocol server over stdio: a THIN ADAPTER over
# the brain server's RPC surface. Every tool maps 1:1 onto the same core
# method the CLI uses (adapter pattern — one implementation, two surfaces), so
# a user in the terminal and an agent over MCP are equally productive and can
# never drift apart. Requires a running `brain server` (it owns pglite).
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js'
import yaml from 'js-yaml'
import { request, requestStream, serverRunning, noServerError } from './client.coffee'

TOOLS = [
  {
    name: 'search'
    description: 'Hybrid (vector + keyword + RRF) search over the knowledge graph. Returns YAML results. `strategy` opts out of fusion (keyword-only or vector-only); `expand: false` skips the 1-hop relational expansion.'
    inputSchema:
      type: 'object'
      properties:
        query: { type: 'string' }
        limit: { type: 'number' }
        explain: { type: 'boolean' }
        strategy: { type: 'string', enum: ['hybrid', 'keyword', 'vector'] }
        expand: { type: 'boolean' }
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
    description: 'Deterministic structural graph-match (Mermaid syntax), e.g. "Team -->|SUPPORTS| Product". Wildcards *N match up to N hops; add "--shortest" to a pattern to return only minimum-hop paths. Results include capped: true when a traversal limit (not the graph) ended the search.'
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
    description: 'Create/update an entity. `content` is flattened YAML frontmatter (lowercase keys = components, UPPERCASE keys = relations). Writes land in the live index immediately (searchable at once); schema validation is reported as soft issues, not a hard error (overwrite=true to replace an existing record). The .md file materializes on `brain export`.'
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
    description: 'Remove an entity by slug (Class/id) from the live index, e.g. Note/family-kids or Person/lsmullin. Same as CLI `brain rm`. The .md file is removed on `brain export --prune`. Does not cascade-delete other entities that only link to it.'
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
  {
    name: 'schema_orphans'
    description: 'List entities with zero relations (in or out) — the connectivity check `validate` runs as a lint, available on demand.'
    inputSchema: { type: 'object', properties: {}, required: [] }
  }
]

textResult = (obj) -> { content: [{ type: 'text', text: (if typeof obj is 'string' then obj else yaml.dump(obj, { lineWidth: 120, sortKeys: false, noRefs: true })) }] }
errorResult = (msg) -> { content: [{ type: 'text', text: msg }], isError: true }

# Agent-facing put_entity outcome: always "saved" when write succeeded; invalid
# schema is a soft notice, not isError.
formatPutEntityResult = (r) ->
  base =
    slug: r.slug
    valid: r.valid isnt false
    warnings: r.warnings or []
  if r.valid isnt false
    return base
  reasons = r.validationErrors or []
  notice = [
    "Record was stored in the live index, but it is considered INVALID for the following reasons:"
    (reasons.map (m) -> "  - #{m}").join('\n') or '  - (unspecified validation errors)'
    ''
    'Fixing these is not mandatory for the data to be stored and persisted.'
    'If you can, try an update (overwrite=true) with corrected content so the record becomes valid.'
  ].join('\n')
  Object.assign base,
    valid: false
    validation_errors: reasons
    notice: notice

# tool name -> RPC method + params mapping (1:1 with the CLI's calls)
handleCall = (cwd, name, args) ->
  switch name
    when 'search'
      textResult(await request(cwd, 'search', { query: args.query, limit: args.limit or 10, explain: !!args.explain, strategy: args.strategy or 'hybrid', expand: args.expand isnt false }))
    when 'think'
      textResult(await request(cwd, 'think', { question: args.question, limit: args.limit or 8 }))
    when 'ontology'
      textResult(await request(cwd, 'ontology', { question: args.question }))
    when 'graph'
      textResult(await request(cwd, 'graph', { pattern: args.pattern }))
    when 'graphql'
      textResult(await request(cwd, 'graphql', { query: args.query }))
    when 'get_entity'
      textResult(await request(cwd, 'get_entity', { slug: args.slug, include_links: !!args.include_links }))
    when 'put_entity'
      textResult(formatPutEntityResult(await request(cwd, 'put_entity', { slug: args.slug, content: args.content, overwrite: !!args.overwrite })))
    when 'delete_entity'
      textResult(await request(cwd, 'delete_entity', { slug: args.slug }))
    when 'schema_methods'
      res = await request(cwd, 'schema_methods', { class: args.class })
      textResult("#{m.signature}#{if m.description then '  # ' + m.description else ''}" for m in res)
    when 'method_invoke'
      r = await request(cwd, 'method_invoke', { slug: args.slug, method: args.method, params: args.params or {} })
      text = (if not r.success and r.error then "#{r.error}\n" else '') + (r.content or '')
      if r.success then textResult(text) else errorResult(text)
    when 'schema_orphans'
      # Stream accumulates into a bulk list for the MCP tool result.
      rows = []
      await requestStream cwd, 'schema_orphans', {}, (item) ->
        rows.push({ slug: "#{item.cls}/#{item.id}", cls: item.cls })
      textResult({ count: rows.length, orphans: (r.slug for r in rows) })
    else
      errorResult("unknown tool: #{name}")

export startStdio = (cwd = process.cwd()) ->
  unless serverRunning(cwd)
    process.stderr.write(noServerError(cwd).message + '\n')
    return 1
  server = new Server({ name: 'brain', version: '0.1.0' }, { capabilities: { tools: {} } })
  server.setRequestHandler ListToolsRequestSchema, -> { tools: TOOLS }
  server.setRequestHandler CallToolRequestSchema, (req) ->
    try
      await handleCall(cwd, req.params.name, req.params.arguments or {})
    catch err
      errorResult("error: #{err.message}")
  await server.connect(new StdioServerTransport())
  process.stderr.write("brain MCP server ready (stdio) — proxying to brain server\n")
  0
