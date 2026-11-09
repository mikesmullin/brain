/**
 * brain — the PRODUCTION hot client, served by `brain serve` at `/hot.mjs`.
 *
 * This file replaces the rustex1 kit's `kit/web/hot.mjs` in the deployed image.
 * It is embedded into the binary with `include_str!` and routed explicitly in
 * `src/serve.rs`, which is deliberate: a filesystem shadow (`web/hot.mjs`) would
 * disable hot reload for `brain web` too, and that is the one command where the
 * real client is the whole point. Here, development keeps the kit's client and
 * production cannot reach it.
 *
 * ## Why the kit's client does not belong in a deployment
 *
 * None of what it does is harmful — it is that it never stops, and lies:
 *
 * 1. **The socket reconnects forever.** `hot.mjs` ends its `close` handler with
 *    `setTimeout(connect, backoff)` and caps the backoff at 4 s, so a client
 *    with no server behind `/__m_hmr` opens a fresh TCP connection plus TLS
 *    handshake plus upgrade request every four seconds, per browser tab, for as
 *    long as the tab is open. `brain serve` answers that upgrade honestly (426)
 *    and the loop continues anyway.
 * 2. **The panel reads "offline" on a healthy server.** `kit/src/live.rs` runs
 *    at 5 Hz and derives the status chip entirely from `window.__HMR__.connected`
 *    — `false` renders `state: "offline"` in tone `err`. With no socket there is
 *    no `connected`, so every deployed brain parks a red OFFLINE badge on a pod
 *    whose `/health` says `ready: true`.
 * 3. **Telemetry polls two routes that do not exist.** `kit/src/api.rs`'s
 *    `spawn_all()` fetches `/api/status` every 2 s and `/api/builds` every 3 s,
 *    and a non-JSON body funnels into `fail()`, which writes `session.error` and
 *    `session.online = false`.
 *
 * The kit ships no opt-out — `api::spawn_all()` is called unconditionally in the
 * boot path at `kit/src/lib.rs:204`, and `vendor/rustex1` is a submodule, so
 * patching it upstream is neither available here nor durable across
 * `git submodule update`. So the app supplies its own client instead, which is
 * the extension mechanism the kit documents for `app.mjs` and which works the
 * same way for this file.
 *
 * ## What is kept, and why
 *
 * `config()` is kept verbatim in behaviour, because `app.mjs` boots through it:
 * it fetches `/api/config` once, memoizes it, and hands `{pkgJs, pkgWasm}` to the
 * wasm loader. Break that and the page sits on its splash screen forever.
 *
 * `window.__HMR__` is still created, with the same field names `live.rs` reads,
 * so nothing downstream has to null-check. `connected` is `true` because it is
 * the least-wrong value of the two available: the field drives a *liveness*
 * chip, and this app genuinely is live — `brain serve` answers `/api/status`
 * with real uptime and request counts. `false` would render a server that is
 * demonstrably healthy as offline. `phase: 'live'` and `building: false` say
 * what is actually true: there is no build pipeline behind this process, and no
 * source tree for one to watch.
 *
 * `overlay()`, `flash()` and `connect()` become no-ops, keeping the exports the
 * kit's `app.mjs` imports (`{ config, overlay }`) and the ones `kit/src/hooks.rs`
 * reaches for. A boot failure is still reported — `app.mjs` renders the
 * `.app-error` box itself — and `overlay()` mirrors the text to `console.error`
 * so nothing is lost by dropping the floating panel.
 */

/** Mirrors the kit's defaults; `config()` overwrites these from `/api/config`. */
const cfg = {
  app: 'brain',
  pkgJs: '/pkg/brain_app.js',
  pkgWasm: '/pkg/brain_app_bg.wasm',
  // Present so a reader that expects the dev contract finds a string, not
  // undefined. Nothing connects to it; `connect()` below is a no-op.
  hmrPath: '/__m_hmr',
  dev: false,
}

let configPromise = null

/** `GET /api/config`, once. Same contract the kit's client implements. */
export function config() {
  if (!configPromise) {
    configPromise = fetch('/api/config', { cache: 'no-store' })
      .then((r) => (r.ok ? r.json() : {}))
      .then((json) => {
        Object.assign(cfg, json || {}, { dev: false })
        // The other half of turning production on. This file *replaces* the
        // kit's hot.mjs, so the kit's own publication of this flag never runs
        // here — without this line, a patched kit would still poll
        // /api/status and /api/builds against `brain serve`, because
        // `api::dev_mode()` reads exactly this global and treats "absent" as
        // dev:true. With it, the kit's poll loops never start at all, and the
        // honest /api/status and /api/builds handlers in serve.rs remain as
        // defence for a kit that predates the gate.
        window.__RUSTEX1_DEV__ = false
        document.documentElement.dataset.hmr = 'production'
        return cfg
      })
      .catch(() => cfg)
  }
  return configPromise
}

/**
 * The state `kit/src/live.rs` mirrors into the `session` store at 5 Hz. Same
 * keys as the dev client's, so the kit's widgets bind without special cases.
 */
const stats = (window.__HMR__ ||= {
  connected: true, // drives the liveness chip — see the note above
  building: false,
  phase: 'live',
  file: '',
  swaps: 0,
  pulse: 0,
  lastBuildMs: 0,
  error: '',
  lastMessage: '',
  bootId: '',
  events: [],
  production: true,
})

/** No floating panel in production; keep the text where a person can find it. */
export function overlay(message, target) {
  if (message) {
    console.error(`[brain] ${target || 'diagnostics'}: ${message}`)
  }
}

/** No 1 s strip: there are no rebuilds to make visible. */
export function flash() {}

/** No socket. Nothing would answer it, and a retry loop is not a feature. */
export function connect() {}

export default { connect, overlay, stats, flash, config }
