# server.coffee — SERVER MODE: the one long-running process that owns pglite.
#
# `brain server start` holds the fully-indexed graph resident for its whole
# lifetime and serves every other `brain` invocation (and MCP) over a unix
# socket carrying NDJSON JSON-RPC ({id, method, params} -> {id, result|error}).
#
# Guarantees:
#   - Exactly one server (and one pglite) per db/: guarded by db/.lock
#     (PID + start timestamp). The lock is claimed *before* the slow pglite
#     init, with exclusive create (`wx`), so two concurrent starts cannot both
#     pass the guard. If a live owner already holds the lock, the *incoming*
#     process self-aborts — we never kill the incumbent.
#   - Lock release is ownership-checked: a dying process only unlinks .lock if
#     it still contains its own PID (won't clobber a successor's lock).
#   - Thin clients can be killed at will — they never touch pgdata/, so
#     Ctrl-C on a CLI command can no longer corrupt the on-disk database.
#   - Correctness over uptime: while maintenance (reindex/export) runs,
#     queries WAIT for it to finish rather than reading a partial index —
#     a stall beats a wrong answer.
import net from 'net'
import { existsSync, unlinkSync, readFileSync, openSync, writeSync, closeSync } from 'fs'
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

# Claim db/.lock exclusively *before* slow init.
#   { ok: true }                         — we own the lock (our PID written)
#   { ok: false, conflict: {pid,started} } — live owner; caller must self-abort
#
# Uses open('wx') so two concurrent starters cannot both create the file.
# Stale locks (dead PID) are removed and we retry once.
claimLock = (cwd, started) ->
  p = paths(cwd).lock
  payload = JSON.stringify({ pid: process.pid, started })

  tryCreate = ->
    try
      fd = openSync(p, 'wx')   # O_CREAT|O_EXCL — fail if exists
      try
        writeSync(fd, payload)
      finally
        closeSync(fd)
      true
    catch err
      throw err unless err.code is 'EEXIST'
      false

  return { ok: true } if tryCreate()

  existing = readLock(cwd)
  if existing and pidAlive(existing.pid)
    return { ok: false, conflict: existing }

  # Stale (dead PID, corrupt, or empty) — remove and retry once.
  try unlinkSync(p) if existsSync(p)
  catch then undefined

  return { ok: true } if tryCreate()

  # Lost the race to another starter that claimed between our unlink and create.
  existing = readLock(cwd)
  if existing and pidAlive(existing.pid)
    return { ok: false, conflict: existing }
  # Pathological: file exists but unreadable / pid dead again.
  { ok: false, conflict: existing or { pid: 'unknown', started: 0 } }

# Unlink .lock only if it still names this process — never erase a successor's claim.
releaseLockIfOwned = (cwd) ->
  p = paths(cwd).lock
  existing = readLock(cwd)
  return unless existing?.pid is process.pid
  try unlinkSync(p) if existsSync(p)
  catch then undefined

# Methods that mutate/replace the whole index. They run exclusively: queued
# behind in-flight work, and all other requests queue behind them.
MAINTENANCE = new Set(['reindex', 'export', 'vacuum', 'components', 'viz_layout'])

export class BrainServer
  constructor: (@cwd = process.cwd()) ->
    @p = paths(@cwd)
    @started = Date.now()
    @maintenance = null   # in-flight maintenance promise (queries await it)
    @inflight = new Map()   # qid -> { agents, cancelled }: cancellable LLM runs
    @requests = 0
    @ownsLock = false

  log: (msg) -> console.log "[brain server] #{msg}"

  # Drop lock (if ours) + close core if partially opened. Used on failed start.
  abortStart: (core = null) ->
    try await core.close() if core
    catch then undefined
    releaseLockIfOwned(@cwd) if @ownsLock
    @ownsLock = false

  start: ->
    # 1) Claim the lock BEFORE slow pglite init. Incoming process self-aborts
    #    if a live owner already holds it — we never kill the incumbent.
    claimed = claimLock(@cwd, @started)
    unless claimed.ok
      c = claimed.conflict
      since = if c.started then new Date(c.started).toISOString() else '?'
      up = if c.started then fmtUptime(Date.now() - c.started) else '?'
      console.error "brain server already running for #{@p.root} (PID #{c.pid}, started #{since}, up #{up})."
      console.error "Stop it first (`brain server stop`), or use a different db/."
      return 1
    @ownsLock = true

    core = null
    try
      # 2) Slow path — lock already held, so a concurrent start will self-abort.
      core = await new Core(@cwd).init()
      @core = core
      unless await @core.isIndexed()
        console.error "No index found at #{@p.pgdata}."
        console.error "Run `brain reindex` first, then start the server."
        await @abortStart(core)
        @core = null
        return 1

      # 3) Bind the socket. Stale sock file from a dead process is safe to replace
      #    because we hold the lock. If bind fails (path busy), self-abort.
      try unlinkSync(@p.sock) if existsSync(@p.sock)
      catch then undefined

      @srv = net.createServer (conn) => @handleConnection(conn)
      try
        await new Promise (resolve, reject) =>
          @srv.once 'error', reject
          @srv.listen @p.sock, resolve
      catch err
        console.error "brain server failed to bind #{@p.sock}: #{err.message}"
        console.error "Another process may still be using this db/. Stop it first."
        try @srv.close()
        catch then undefined
        @srv = null
        await @abortStart(core)
        @core = null
        return 1

      stop = => @shutdown()
      process.on 'SIGINT', stop
      process.on 'SIGTERM', stop

      st = await @core.status()
      @log "ready on #{@p.sock} (PID #{process.pid})"
      @log "db: #{@p.root} · #{st.entities} entities · #{st.links} links · embed: #{st.embed}"
      # keep the process alive until shutdown
      await new Promise (resolve) => @resolveShutdown = resolve
      0
    catch err
      # Unexpected failure during init — release the lock so a retry can start.
      await @abortStart(core)
      @core = null
      throw err

  # Abort every registered LLM agent (think/ontology). Used by cancel RPC and
  # by shutdown (Ctrl+C / SIGTERM) so LM Studio slots are freed immediately
  # instead of decoding until the 5s force-exit watchdog.
  cancelAllInflight: (reason = 'server shutting down') ->
    n = 0
    for [id, entry] from @inflight
      entry.cancelled = true
      for a in entry.agents
        try
          a.abort(reason)
          n++
        catch then continue
      @log "cancel qid=#{id} agents=#{entry.agents.length} (#{reason})"
    n

  shutdown: ->
    return if @stopping
    @stopping = true
    # Watchdog: if graceful close stalls (in-flight provider calls, hung
    # sockets), force-exit — a zombie server looping inference retries against
    # the GPU is far worse than a skipped cleanup step. Lock/sock removal is
    # ownership-checked so we never clobber a successor's lock.
    watchdog = setTimeout (=>
      @log 'graceful shutdown stalled — forcing exit'
      try unlinkSync(@p.sock) if existsSync(@p.sock)
      catch then undefined
      releaseLockIfOwned(@cwd)
      process.exit(0)
    ), 5000
    @log 'shutting down...'
    # First: kill any live inference so Ctrl+C doesn't leave LM Studio pegged
    # while we wait on pglite close / maintenance.
    aborted = @cancelAllInflight('server shutting down')
    @log "aborted #{aborted} in-flight agent(s)" if aborted
    @srv?.close()
    # let any in-flight maintenance finish so pgdata closes consistent
    try await @maintenance if @maintenance
    try await @core.close()   # clean pglite close — no more corrupted pgdata on exit
    catch then undefined
    try unlinkSync(@p.sock) if existsSync(@p.sock)
    catch then undefined
    releaseLockIfOwned(@cwd)
    @ownsLock = false
    clearTimeout(watchdog)
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

  # Cancellable LLM runs: think/ontology register their agl agent here (keyed
  # by the client-supplied qid) so a concurrent `cancel` request can abort the
  # in-flight inference (agl's Agent#abort recalls the provider stream). The
  # cancelled flag also covers the race where cancel lands while the agent is
  # still being constructed. No qid -> not cancellable (CLI callers).
  #
  # Single-flight: starting a new cancellable query auto-cancels any other
  # inflight ones (viz UI only allows one; this enforces it server-side too so
  # a refresh + re-submit can't stack LM Studio slots).
  withCancel: (qid, method, meta, fn) ->
    return await fn((->), (-> false)) unless qid
    qid = String(qid)
    # Supersede anything else still running
    for [otherId, other] from @inflight
      continue if otherId is qid
      other.cancelled = true
      try a.abort('superseded by new query') for a in other.agents
      @log "inflight #{otherId} superseded by #{qid}"
    entry =
      agents: []
      cancelled: false
      method: method
      question: meta?.question or ''
      model: meta?.model or ''
      started: Date.now()
    @inflight.set(qid, entry)
    onAgent = (a) ->
      # cancel landed during retrieval/agent construction: abort + throw so we
      # never start inference (agent.abort is also called for belt-and-braces)
      if entry.cancelled
        try a.abort('cancelled by user')
        throw new Error('cancelled by user')
      entry.agents.push(a)
      # if cancel races the push, abort immediately
      if entry.cancelled
        try a.abort('cancelled by user')
        throw new Error('cancelled by user')
    isCancelled = -> entry.cancelled
    try
      result = await fn(onAgent, isCancelled)
      throw new Error('cancelled by user') if entry.cancelled
      result
    finally
      @inflight.delete(qid)

  # Snapshot of in-flight cancellable queries (for viz refresh recovery).
  inflightStatus: ->
    now = Date.now()
    items = []
    for [qid, e] from @inflight
      items.push
        qid: qid
        method: e.method
        question: e.question
        model: e.model
        started: new Date(e.started).toISOString()
        elapsed_ms: now - e.started
        cancelled: e.cancelled
        agents: e.agents.length
    { inflight: items }

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
          inflight: @inflightStatus().inflight
      when 'stop' then { stopping: true }
      when 'inflight' then @inflightStatus()
      # queries — the 5 search surfaces + reads
      when 'search' then await core.search(params.query, { limit: params.limit, explain: params.explain, strategy: params.strategy, expand: params.expand })
      when 'think' then await @withCancel params.qid, 'think', { question: params.question, model: params.model }, (onAgent, isCancelled) =>
        core.think(params.question, { limit: params.limit, model: params.model, thinking: params.thinking, selection: params.selection, onAgent, isCancelled })
      when 'ontology' then await @withCancel params.qid, 'ontology', { question: params.question, model: params.model }, (onAgent, isCancelled) =>
        core.ontology(params.question, { maxCalls: params.max_calls, model: params.model, thinking: params.thinking, selection: params.selection, onAgent, isCancelled })
      # cancel an in-flight think/ontology by qid — idempotent (repeat calls
      # and unknown/already-finished qids are safe no-ops). Empty qid cancels ALL.
      when 'cancel'
        qid = String(params.qid or '')
        unless qid
          n = @cancelAllInflight('cancelled by user')
          return { cancelled: n > 0, found: n > 0, count: n }
        entry = @inflight.get(qid)
        unless entry
          return { cancelled: false, found: false, count: 0 }
        entry.cancelled = true
        n = 0
        for a in entry.agents
          try
            a.abort('cancelled by user')
            n++
          catch then continue
        @log "cancel qid=#{qid} agents=#{n}/#{entry.agents.length}"
        { cancelled: true, found: true, count: 1 }
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
