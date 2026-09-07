# mcp.coffee — Model Context Protocol server over stdio: a THIN ADAPTER over
# the brain server's RPC surface. Every tool maps 1:1 onto the same core
# method the CLI uses (adapter pattern — one implementation, two surfaces), so
# a user in the terminal and an agent over MCP are equally productive and can
# never drift apart. If the pglite owner is down, tool calls (and adapter
# startup) auto-start it — agents should not have to run `brain server start`.
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js'
import yaml from 'js-yaml'
import { request, requestStream, serverRunning, noServerError } from './client.coffee'
import { ensureServerRunning } from './server.coffee'
import { attachQueryAdvice } from './mcp-advice.mjs'
import { loadSchema, formatClassDetail } from './schema.coffee'
import { storageDirs } from './config.coffee'

TOOLS = [
  {
    name: 'search'
    description: 'Hybrid (vector + keyword + RRF) search over the knowledge graph. Returns YAML `{ results, advice }`. `strategy` opts out of fusion (keyword-only or vector-only); `expand: false` skips the 1-hop relational expansion. `advice` says what to try next if hits are empty or only lexically related (prefer ontology > think > search).'
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
    description: 'Search + LLM synthesis: returns a grounded answer with citations, gaps, and `advice` on whether to try ontology next (do not fall back to search — think already searched).'
    inputSchema:
      type: 'object'
      properties: { question: { type: 'string' }, limit: { type: 'number' } }
      required: ['question']
  }
  {
    name: 'ontology'
    description: 'LLM-driven typed relationship traversal (multi-hop graph questions). Returns the walk plus `advice` if it did not land (try think/search with different wording, or get_entity).'
    inputSchema:
      type: 'object'
      properties: { question: { type: 'string' } }
      required: ['question']
  }
  {
    name: 'graph'
    description: 'Deterministic structural graph-match (Mermaid syntax), e.g. "Team -->|SUPPORTS| Product". Wildcards *N match up to N hops; add "--shortest" to a pattern to return only minimum-hop paths. Results include capped: true when a traversal limit (not the graph) ended the search. Empty results include `advice`: this is not English — use ontology/think/search for NL questions.'
    inputSchema:
      type: 'object'
      properties: { pattern: { type: 'string' } }
      required: ['pattern']
  }
  {
    name: 'graphql'
    description: 'Deterministic GraphQL-ish traversal from a known Class/id, e.g. "Team/cloud { naming, USES_SYSTEM { info } }". Empty/not-found results include `advice` pointing at ontology/think/search for natural-language questions.'
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
    description: 'Create/update an entity. `content` is flattened YAML frontmatter (lowercase keys = components, UPPERCASE keys = relations). Writes land in the live index immediately (searchable at once); schema validation is reported as soft issues, not a hard error (overwrite=true to replace an existing record). When invalid, the result includes the valid shape for the class — fix and retry. The .md file materializes on `brain export`.'
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
# schema is a soft notice, not isError. On invalid, the notice encourages a
# retry AND includes the valid shape for the entity's class (from schema.yaml)
# so the next attempt can be right first time.
formatPutEntityResult = (r, ctx = {}) ->
  base =
    slug: r.slug
    valid: r.valid isnt false
    warnings: r.warnings or []
  if r.valid isnt false
    return base
  reasons = r.validationErrors or []
  lines = [
    "Record was stored in the live index, but it is considered INVALID for the following reasons:"
    (reasons.map (m) -> "  - #{m}").join('\n') or '  - (unspecified validation errors)'
    ''
    'Please try again with corrected content (overwrite=true). Valid shape for this class:'
  ]
  cls = String(ctx.slug or r.slug or '').split('/')[0] or 'unknown'
  try
    dirs = await storageDirs ctx.cwd or process.cwd()
    schema = await loadSchema dirs
    lines.push '', formatClassDetail(schema, cls)
  catch e
    lines.push '', "(could not load schema detail here: #{e.message} — try `schema classes #{cls}`)"
  notice = lines.join('\n')
  Object.assign base,
    valid: false
    validation_errors: reasons
    notice: notice

# tool name -> RPC method + params mapping (1:1 with the CLI's calls)
handleCall = (cwd, name, args) ->
  switch name
    when 'search'
      textResult attachQueryAdvice 'search', await request(cwd, 'search', { query: args.query, limit: args.limit or 10, explain: !!args.explain, strategy: args.strategy or 'hybrid', expand: args.expand isnt false })
    when 'think'
      textResult attachQueryAdvice 'think', await request(cwd, 'think', { question: args.question, limit: args.limit or 8 })
    when 'ontology'
      textResult attachQueryAdvice 'ontology', await request(cwd, 'ontology', { question: args.question })
    when 'graph'
      textResult attachQueryAdvice 'graph', await request(cwd, 'graph', { pattern: args.pattern })
    when 'graphql'
      textResult attachQueryAdvice 'graphql', await request(cwd, 'graphql', { query: args.query })
    when 'get_entity'
      textResult(await request(cwd, 'get_entity', { slug: args.slug, include_links: !!args.include_links }))
    when 'put_entity'
      textResult(await formatPutEntityResult(await request(cwd, 'put_entity', { slug: args.slug, content: args.content, overwrite: !!args.overwrite }), { cwd, slug: args.slug }))
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

isNoServer = (err) ->
  msg = String(err?.message or err or '')
  /no brain server running/i.test(msg) or
    (err?.code in ['ENOENT', 'ECONNREFUSED', 'ECONNRESET', 'EPIPE', 'ENOTCONN'])

runTool = (cwd, name, args) ->
  try
    await handleCall cwd, name, args
  catch err
    unless isNoServer(err)
      return errorResult("error: #{err.message}")
    process.stderr.write "brain mcp: server down on #{name}, auto-starting...\n"
    unless await ensureServerRunning(cwd)
      return errorResult(noServerError(cwd).message)
    try
      await handleCall cwd, name, args
    catch err2
      errorResult("error: #{err2.message}")

export startStdio = (cwd = process.cwd()) ->
  unless serverRunning(cwd)
    process.stderr.write "brain mcp: no server for #{cwd}, auto-starting...\n"
    unless await ensureServerRunning(cwd)
      process.stderr.write(noServerError(cwd).message + '\n')
      return 1
  server = new Server({ name: 'brain', version: '0.1.0' }, { capabilities: { tools: {} } })
  server.setRequestHandler ListToolsRequestSchema, -> { tools: TOOLS }
  server.setRequestHandler CallToolRequestSchema, (req) ->
    await runTool cwd, req.params.name, req.params.arguments or {}
  await server.connect(new StdioServerTransport())
  process.stderr.write("brain MCP server ready (stdio) — proxying to brain server\n")
  0
