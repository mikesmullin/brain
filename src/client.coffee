# client.coffee — CLI MODE: thin RPC client for the running brain server.
#
# Clients never open pglite and never manage its lifecycle — startup is
# instant, invocations parallelize freely, and aborting one (Ctrl-C) is
# consequence-free for the on-disk database. If no server is running, query
# commands fail fast with a pointer to `brain server start`.
import net from 'net'
import { existsSync } from 'fs'
import { paths } from './config.coffee'
import { serverRunning } from './server.coffee'

export noServerError = (cwd) ->
  p = paths(cwd)
  new Error """
    no brain server running for #{p.root}
    Start one first:  brain server start
    (If a server was running, it may have crashed — check for a stale #{p.lock})
  """

# Unix-socket / connection failures Bun and Node both surface as "server gone"
isConnectGone = (err) ->
  return false unless err?
  code = err.code or err.errno
  return true if code in ['ENOENT', 'ECONNREFUSED', 'ECONNRESET', 'EPIPE', 'ENOTCONN']
  msg = String(err.message or err)
  /ENOENT|ECONNREFUSED|ECONNRESET|EPIPE|not known|connect/i.test(msg)

nextId = 0

# One request over the unix socket; resolves with `result` or rejects with the
# server's error. No client-side timeout by default: think/ontology legitimately
# run LLM loops for a while, and maintenance gates can hold queries briefly.
connectRpc = (cwd) ->
  p = paths(cwd)
  # Live PID alone is not enough — sock may be gone after a crash (stale .lock).
  throw noServerError(cwd) unless serverRunning(cwd) and existsSync(p.sock)
  id = ++nextId
  # Bun may throw ENOENT synchronously when the pipe path is missing
  try
    conn = net.connect p.sock
  catch err
    if isConnectGone(err) then throw noServerError(cwd) else throw err
  { p, id, conn }

export request = (cwd, method, params = {}) ->
  { id, conn } = connectRpc(cwd)
  new Promise (resolve, reject) ->
    buffer = ''
    settled = false
    fail = (err) ->
      return if settled
      settled = true
      try conn?.destroy() catch then undefined
      # translate socket-level failures into the friendly no-server error
      if isConnectGone(err)
        reject(noServerError(cwd))
      else
        reject(err)
    conn.on 'error', fail
    conn.on 'connect', ->
      try
        conn.write JSON.stringify({ id, method, params }) + '\n'
      catch err
        fail(err)
    conn.on 'data', (chunk) ->
      buffer += chunk.toString('utf8')
      nl = buffer.indexOf('\n')
      return if nl < 0
      settled = true
      conn.end()
      try
        msg = JSON.parse(buffer.slice(0, nl))
        if msg.error then reject(new Error(msg.error)) else resolve(msg.result)
      catch err
        reject(err)
    conn.on 'close', -> fail(new Error('connection closed before response')) unless settled

# Streaming RPC: server emits many NDJSON frames
#   {id, item: ...} × N
#   {id, done: true, result?}
# onItem is awaited for each item as it arrives (no full-result buffer), so
# callers can apply stdout backpressure. Resolves with the final `result`
# (or undefined). Rejects on error / hangup. Destroying work mid-stream
# (throw from onItem, Ctrl-C) tears down the socket so the server stops too.
export requestStream = (cwd, method, params = {}, onItem) ->
  { id, conn } = connectRpc(cwd)
  new Promise (resolve, reject) ->
    buffer = ''
    settled = false
    # Serialize async onItem handlers — data events can deliver many frames
    # in one tick, and we must not interleave writes / skip backpressure.
    pending = Promise.resolve()
    fail = (err) ->
      return if settled
      settled = true
      try conn?.destroy() catch then undefined
      if isConnectGone(err)
        reject(noServerError(cwd))
      else
        reject(err)
    settle = (value) ->
      return if settled
      settled = true
      try conn.end() catch then undefined
      resolve(value)
    enqueue = (fn) ->
      pending = pending.then(-> fn()).catch (err) ->
        fail(err)
        undefined
    conn.on 'error', fail
    conn.on 'connect', ->
      try
        conn.write JSON.stringify({ id, method, params }) + '\n'
      catch err
        fail(err)
    conn.on 'data', (chunk) ->
      buffer += chunk.toString('utf8')
      while (nl = buffer.indexOf('\n')) >= 0
        line = buffer.slice(0, nl)
        buffer = buffer.slice(nl + 1)
        continue unless line.trim()
        try
          msg = JSON.parse(line)
        catch err
          fail(err)
          return
        if msg.error?
          fail(new Error(msg.error))
          return
        if msg.item?
          do (item = msg.item) ->
            enqueue ->
              return if settled
              await onItem(item)
        if msg.done
          do (result = msg.result) ->
            enqueue ->
              return if settled
              settle(result)
          return
    conn.on 'close', -> fail(new Error('connection closed before stream completed')) unless settled

export { serverRunning }
