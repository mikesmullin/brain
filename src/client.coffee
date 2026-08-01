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

nextId = 0

# One request over the unix socket; resolves with `result` or rejects with the
# server's error. No client-side timeout by default: think/ontology legitimately
# run LLM loops for a while, and maintenance gates can hold queries briefly.
export request = (cwd, method, params = {}) ->
  p = paths(cwd)
  throw noServerError(cwd) unless serverRunning(cwd) and existsSync(p.sock)
  id = ++nextId
  new Promise (resolve, reject) ->
    conn = net.connect p.sock
    buffer = ''
    settled = false
    fail = (err) ->
      return if settled
      settled = true
      conn.destroy()
      # translate socket-level failures into the friendly no-server error
      if err?.code in ['ENOENT', 'ECONNREFUSED', 'ECONNRESET']
        reject(noServerError(cwd))
      else
        reject(err)
    conn.on 'error', fail
    conn.on 'connect', ->
      conn.write JSON.stringify({ id, method, params }) + '\n'
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

export { serverRunning }
