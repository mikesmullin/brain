/**
 * entity-ws.js — persistent WebSocket for SPA entity lookups.
 *
 * Avoids flooding the HTTP server with GET /nodes and /labels. Protocol:
 *   → { id, type: 'nodes', slugs: string[] }
 *   ← { id, type: 'nodes_result', entities: [...] }
 *   → { id, type: 'labels', slugs: string[] }
 *   ← { id, type: 'labels_result', labels: { [slug]: name } }
 *   ← { id, type: 'error', error: string }
 */
const WS_PATH = '/__entity_ws'
const RECONNECT_MS = 800
const REQUEST_TIMEOUT_MS = 60_000

/** @type {WebSocket|null} */
let socket = null
/** @type {Promise<WebSocket>|null} */
let connecting = null
let nextId = 1
/** @type {Map<string, { resolve: (v: any) => void, reject: (e: any) => void, timer: any }>} */
const pending = new Map()

function wsUrl() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${proto}://${location.host}${WS_PATH}`
}

function settle(id, fn) {
  const p = pending.get(id)
  if (!p) return
  pending.delete(id)
  try {
    clearTimeout(p.timer)
  } catch {
    /* ignore */
  }
  fn(p)
}

function rejectAll(err) {
  for (const [id] of [...pending.entries()]) {
    settle(id, (p) => p.reject(err))
  }
}

function onMessage(ev) {
  let msg
  try {
    msg = JSON.parse(typeof ev.data === 'string' ? ev.data : String(ev.data))
  } catch {
    return
  }
  if (!msg || msg.id == null) return
  const id = String(msg.id)
  if (msg.type === 'error') {
    settle(id, (p) => p.reject(new Error(msg.error || 'entity ws error')))
    return
  }
  if (msg.type === 'nodes_result' || msg.type === 'labels_result') {
    settle(id, (p) => p.resolve(msg))
    return
  }
}

/**
 * Ensure a live WebSocket (reconnects after close).
 * @returns {Promise<WebSocket>}
 */
export function ensureEntityWs() {
  if (socket && socket.readyState === WebSocket.OPEN) {
    return Promise.resolve(socket)
  }
  if (connecting) return connecting

  connecting = new Promise((resolve, reject) => {
    let settled = false
    const ws = new WebSocket(wsUrl())
    const fail = (err) => {
      if (settled) return
      settled = true
      connecting = null
      reject(err instanceof Error ? err : new Error(String(err)))
    }
    ws.addEventListener('open', () => {
      if (settled) return
      settled = true
      socket = ws
      connecting = null
      resolve(ws)
    })
    ws.addEventListener('message', onMessage)
    ws.addEventListener('error', () => {
      fail(new Error('entity websocket error'))
    })
    ws.addEventListener('close', () => {
      if (socket === ws) socket = null
      rejectAll(new Error('entity websocket closed'))
      connecting = null
      // Soft reconnect so the next request re-opens
      setTimeout(() => {
        if (!socket) {
          try {
            void ensureEntityWs()
          } catch {
            /* ignore */
          }
        }
      }, RECONNECT_MS)
    })
  })
  return connecting
}

/**
 * JSON-RPC-ish request over the entity WebSocket.
 * @param {'nodes'|'labels'} type
 * @param {object} body
 * @returns {Promise<any>}
 */
export async function entityWsRequest(type, body = {}) {
  const ws = await ensureEntityWs()
  const id = String(nextId++)
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      settle(id, (p) => p.reject(new Error(`entity ws timeout (${type})`)))
    }, REQUEST_TIMEOUT_MS)
    pending.set(id, { resolve, reject, timer })
    try {
      ws.send(JSON.stringify({ id, type, ...body }))
    } catch (err) {
      settle(id, (p) =>
        p.reject(err instanceof Error ? err : new Error(String(err))),
      )
    }
  })
}

/**
 * @param {string[]} slugs
 * @returns {Promise<{ entities: any[] }>}
 */
export async function wsFetchNodes(slugs) {
  const msg = await entityWsRequest('nodes', { slugs })
  return { entities: msg.entities || [] }
}

/**
 * @param {string[]} slugs
 * @returns {Promise<{ labels: Record<string, string> }>}
 */
export async function wsFetchLabels(slugs) {
  const msg = await entityWsRequest('labels', { slugs })
  return { labels: msg.labels || {} }
}

export default {
  ensureEntityWs,
  entityWsRequest,
  wsFetchNodes,
  wsFetchLabels,
}
