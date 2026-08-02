/**
 * Lightweight HMR client for brain viz — connects to /__m_hmr.
 * Standalone (no m.js dependency). m.js itself is loaded from CDN.
 *
 * CSS: swap stylesheets in place.
 * JS: re-run window.__M_BOOT__(timestamp) so the app dynamically re-imports
 *     modules with cache bust. Named stores + Router URL are preserved.
 */

const HMR_PROTO = location.protocol === 'https:' ? 'wss' : 'ws'
const HMR_URL = `${HMR_PROTO}://${location.host}/__m_hmr`

/** @type {((path: string) => void|Promise<void>) | null} */
let customHandler = null

/**
 * @param {(path: string) => void|Promise<void>} fn
 */
export function onHotReload(fn) {
  customHandler = fn
}

function reloadCss(href) {
  const clean = href.split('?')[0]
  const links = document.querySelectorAll('link[rel="stylesheet"]')
  let found = false
  for (const link of links) {
    const url = new URL(/** @type {HTMLLinkElement} */ (link).href, location.href)
    if (
      url.pathname === clean ||
      url.pathname.endsWith(clean) ||
      clean.endsWith(url.pathname)
    ) {
      const next = /** @type {HTMLLinkElement} */ (link.cloneNode())
      next.href = url.pathname + '?t=' + Date.now()
      next.onload = () => link.remove()
      link.parentNode.insertBefore(next, link.nextSibling)
      found = true
    }
  }
  if (!found) {
    const link = document.createElement('link')
    link.rel = 'stylesheet'
    link.href = clean + '?t=' + Date.now()
    document.head.appendChild(link)
  }
  console.debug('[hmr] css', clean)
}

async function reloadJs(path) {
  console.debug('[hmr] js', path)
  if (customHandler) {
    await customHandler(path)
    return
  }
  const bust = Date.now()
  if (path && (path.startsWith('/') || path.startsWith('./'))) {
    const url = path.startsWith('/') ? path : '/' + path
    try {
      await import(url + '?t=' + bust)
    } catch (_) {
      /* boot will re-import */
    }
  }
  if (typeof window.__M_BOOT__ === 'function') {
    try {
      await window.__M_BOOT__(bust)
      console.debug('[hmr] boot ok', bust)
    } catch (err) {
      console.error('[hmr] boot failed, full reload', err)
      location.reload()
    }
  } else {
    location.reload()
  }
}

function connect() {
  let ws
  try {
    ws = new WebSocket(HMR_URL)
  } catch (err) {
    console.debug('[hmr] ws unavailable', err)
    return
  }
  ws.addEventListener('open', () => console.debug('[hmr] connected'))
  ws.addEventListener('close', () => {
    setTimeout(connect, 1500)
  })
  ws.addEventListener('message', async (ev) => {
    let msg
    try {
      msg = JSON.parse(String(ev.data))
    } catch {
      return
    }
    if (msg.type === 'connected') return
    const path = msg.path || msg.file || msg.url || ''
    if (!path) return
    if (/\.css($|\?)/i.test(path)) {
      reloadCss(path)
      return
    }
    if (/\.(js|mjs|coffee)($|\?)/i.test(path) || msg.type === 'update') {
      await reloadJs(path)
    }
  })
}

if (typeof window !== 'undefined' && !window.__BRAIN_HMR__) {
  window.__BRAIN_HMR__ = true
  connect()
}
