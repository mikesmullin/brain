# server.coffee — SERVER MODE: the one long-running process that owns pglite.
#
# `brain server start` holds the fully-indexed graph resident for its whole
# lifetime and serves every other `brain` invocation (and MCP) over a unix
# socket carrying NDJSON JSON-RPC ({id, method, params} -> {id, result|error}).
#
# Guarantees:
#   - Exactly one server (and one pglite) per db/: guarded by db/.lock
#     (PID + start timestamp; stale locks from dead PIDs are cleaned up).
#   - Thin clients can be killed at will — they never touch pgdata/, so
#     Ctrl-C on a CLI command can no longer corrupt the on-disk database.
#   - Correctness over uptime: while maintenance (reindex/export) runs,
#     queries WAIT for it to finish rather than reading a partial index —
#     a stall beats a wrong answer.
import net from 'net'
import { existsSync, unlinkSync, readFileSync, writeFileSync } from 'fs'
import { paths } from './config.coffee'
import { Core } from './core.coffee'

# Read db/.lock -> { pid, started } | null.
export readLock = (cwd) ->
  p = paths(cwd).lock
  return null unless existsSync(p)
  try
    data = JSON.parse(readFileSync(p, 'utf-8'))
    return null unless data.pid
    data
  catch
    null

pidAlive = (pid) ->
  try
    process.kill(pid, 0)
    true
  catch
    false

# Is a brain server currently running for this db/?  -> { pid, started } | null
export serverRunning = (cwd) ->
  lock = readLock(cwd)
  return null unless lock
  return lock if pidAlive(lock.pid)
  null   # stale lock (dead PID) — caller may clean it up

fmtUptime = (ms) ->
  s = Math.floor(ms / 1000)
  d = Math.floor(s / 86400); s -= d * 86400
  h = Math.floor(s / 3600); s -= h * 3600
  m = Math.floor(s / 60); s -= m * 60
  parts = []
  parts.push("#{d}d") if d
  parts.push("#{h}h") if h or d
  parts.push("#{m}m") if m or h or d
  parts.push("#{s}s")
  parts.join(' ')

# Methods that mutate/replace the whole index. They run exclusively: queued
# behind in-flight work, and all other requests queue behind them.
MAINTENANCE = new Set(['reindex', 'export', 'vacuum', 'components', 'viz_layout'])

export class BrainServer
  constructor: (@cwd = process.cwd()) ->
    @p = paths(@cwd)
    @started = Date.now()
    @maintenance = null   # in-flight maintenance promise (queries await it)
    @requests = 0

  log: (msg) -> console.log "[brain server] #{msg}"

  start: ->
    # singleton guard
    existing = readLock(@cwd)
    if existing
      if pidAlive(existing.pid)
        since = new Date(existing.started).toISOString()
        up = fmtUptime(Date.now() - existing.started)
        console.error "brain server already running for #{@p.root} (PID #{existing.pid}, started #{since}, up #{up})."
        console.error "Stop it first (`brain server stop`), or use a different db/."
        return 1
      @log "removing stale lock (PID #{existing.pid} is not alive)"
      unlinkSync(@p.lock)

    @core = await new Core(@cwd).init()
    unless await @core.isIndexed()
      await @core.close()
      console.error "No index found at #{@p.pgdata}."
      console.error "Run `brain reindex` first, then start the server."
      return 1

    writeFileSync(@p.lock, JSON.stringify({ pid: process.pid, started: @started }))
    unlinkSync(@p.sock) if existsSync(@p.sock)

    @srv = net.createServer (conn) => @handleConnection(conn)
    await new Promise (resolve, reject) =>
      @srv.once 'error', reject
      @srv.listen @p.sock, resolve

    stop = => @shutdown()
    process.on 'SIGINT', stop
    process.on 'SIGTERM', stop

    st = await @core.status()
    @log "ready on #{@p.sock} (PID #{process.pid})"
    @log "db: #{@p.root} · #{st.entities} entities · #{st.links} links · embed: #{st.embed}"
    # keep the process alive until shutdown
    await new Promise (resolve) => @resolveShutdown = resolve
    0

  shutdown: ->
    return if @stopping
    @stopping = true
    @log 'shutting down...'
    @srv?.close()
    # let any in-flight maintenance finish so pgdata closes consistent
    try await @maintenance if @maintenance
    await @core.close()   # clean pglite close — no more corrupted pgdata on exit
    unlinkSync(@p.sock) if existsSync(@p.sock)
    unlinkSync(@p.lock) if existsSync(@p.lock)
    @log 'bye'
    @resolveShutdown?()

  handleConnection: (conn) ->
    buffer = ''
    conn.on 'data', (chunk) =>
      buffer += chunk.toString('utf8')
      while (nl = buffer.indexOf('\n')) >= 0
        line = buffer.slice(0, nl)
        buffer = buffer.slice(nl + 1)
        continue unless line.trim()
        @handleRequest(conn, line)
    conn.on 'error', ->   # client went away mid-request; nothing to do

  handleRequest: (conn, line) ->
    req = null
    try
      req = JSON.parse(line)
    catch
      conn.write JSON.stringify({ id: null, error: 'malformed request (expected one JSON object per line)' }) + '\n'
      return
    { id, method, params } = req
    @requests++
    respond = (payload) ->
      try conn.write JSON.stringify(Object.assign({ id }, payload)) + '\n'
    try
      result = await @dispatch(method, params or {})
      respond { result }
      if method is 'stop'
        setTimeout (=> @shutdown()), 10
    catch err
      respond { error: err.message or String(err) }

  # Correctness over uptime: queries wait out maintenance; maintenance ops
  # chain behind each other.
  dispatch: (method, params) ->
    if MAINTENANCE.has(method)
      prev = @maintenance
      task = do =>
        await prev if prev
        await @execute(method, params)
      gate = task.catch(->)   # gate stays up until this op settles (even on failure)
      @maintenance = gate
      try
        await task
      finally
        @maintenance = null if @maintenance is gate
    else
      await @maintenance if @maintenance   # stall, don't lie
      await @execute(method, params)

  execute: (method, params) ->
    core = @core
    switch method
      when 'ping' then { pong: true }
      when 'status'
        st = await core.status()
        Object.assign st,
          pid: process.pid
          started: new Date(@started).toISOString()
          uptime: fmtUptime(Date.now() - @started)
          requests: @requests
          memory_mb: Math.round(process.memoryUsage().rss / 1048576)
      when 'stop' then { stopping: true }
      # queries — the 5 search surfaces + reads
      when 'search' then await core.search(params.query, { limit: params.limit, explain: params.explain, strategy: params.strategy, expand: params.expand })
      when 'think' then await core.think(params.question, { limit: params.limit, model: params.model, thinking: params.thinking, selection: params.selection })
      when 'ontology' then await core.ontology(params.question, { maxCalls: params.max_calls, model: params.model, thinking: params.thinking, selection: params.selection })
      when 'graph' then await core.graph(params.pattern, { shortest: params.shortest, maxNodes: params.max_nodes })
      when 'graphql' then await core.graphql(params.query)
      when 'get_entity' then await core.getEntity(params.slug, !!params.include_links)
      when 'render_entity' then { slug: params.slug, text: await core.renderEntity(params.slug) }
      when 'ls' then await core.ls(params.class or null)
      when 'schema_methods' then await core.schemaMethods(params.class)
      when 'schema_orphans' then await core.schemaOrphans()
      # writes — pglite-first; .md is materialized by `export`
      when 'put_entity' then await core.putEntity(params.slug, params.content, !!params.overwrite)
      when 'delete_entity' then await core.deleteEntity(params.slug)
      when 'set_instance' then await core.setInstance(params.slug, params.assignments or [])
      when 'link' then await core.link(params.from, params.rel, params.to, params.qualifiers or {})
      when 'method_invoke' then await core.methodInvoke(params.slug, params.method, params.params or {})
      when 'components_stats' then await core.componentsStats()
      # maintenance — exclusive; queries queue behind these
      when 'reindex' then await core.reindex({ noEmbed: !!params.no_embed })
      when 'export' then await core.exportMd({ prune: !!params.prune })
      when 'vacuum' then await core.vacuum()
      when 'components' then await core.components((msg) => @log "components: #{msg}")
      when 'viz_layout' then await core.vizLayout({ force: !!params.force }, (msg) => @log "viz_layout: #{msg}")
      else
        throw new Error("unknown method: #{method}")

export startServer = (cwd = process.cwd()) ->
  await new BrainServer(cwd).start()
