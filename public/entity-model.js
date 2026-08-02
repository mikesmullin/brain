/**
 * entity-model.js — SPA model for entity cache / CRUD (MVC "Model").
 *
 * Single place the viz UI uses to:
 *   - fetch entities over WebSocket (/__entity_ws: nodes + labels)
 *   - cache short-lived records in M.store('entities')
 *   - refcount holders (inspector, on-screen entity links)
 *   - evict when no UI component still needs the entity
 *   - apply server/agent mutation snapshots (entity_changed)
 *
 * Views (inspector, entity-link labels) read from the store and re-render when
 * `rev` bumps. They should acquire(slug, holder) while visible and
 * release(slug, holder) when gone.
 */
import M from '/vendor/m-js/src/index.js'
import { wsFetchNodes, wsFetchLabels, ensureEntityWs } from '/entity-ws.js'

/** @typedef {{
 *   slug: string,
 *   components?: object,
 *   relations?: object,
 *   incoming?: any[],
 *   label?: string,
 *   i?: number,
 *   error?: string,
 *   deleted?: boolean,
 *   loading?: boolean,
 *   fetchedAt?: number,
 *   includeLinks?: boolean,
 * }} EntityRecord */

const STORE = 'entities'
/** Grace period at refcount 0 before unload (rapid UI click-around). */
const EVICT_GRACE_MS = 60_000
/** Min gap between /nodes network requests (SPA-side throttle). */
const FETCH_THROTTLE_MS = 250
const holders = new Map() // slug -> Set<holderId>
const inflight = new Map() // slug -> Promise<EntityRecord|null>
/** @type {Map<string, ReturnType<typeof setTimeout>>} pending eviction timers */
const evictTimers = new Map()
/** @type {Set<(slug: string, rec: EntityRecord|null, reason: string) => void>} */
const listeners = new Set()

// ── /nodes fetch throttle (LIFO queue, ≤1 request per FETCH_THROTTLE_MS) ──
let lastNodesFetchAt = 0
/** @type {ReturnType<typeof setTimeout>|null} */
let nodesThrottleTimer = null
/**
 * LIFO stack of pending /nodes jobs.
 * @type {Array<{ slugs: string[], resolve: (data: any) => void, reject: (err: any) => void }>}
 */
const nodesFetchQueue = []

/**
 * Enqueue a nodes lookup over the entity WebSocket (not HTTP).
 * At most one WS round-trip every FETCH_THROTTLE_MS; when several pile up,
 * newest jobs drain first (LIFO) and unique slugs are coalesced into one batch.
 * @param {string[]} slugs
 * @returns {Promise<{ entities?: any[] }>}
 */
function enqueueNodesFetch(slugs) {
  const list = [...new Set((slugs || []).filter(Boolean))]
  if (!list.length) return Promise.resolve({ entities: [] })
  return new Promise((resolve, reject) => {
    nodesFetchQueue.push({ slugs: list, resolve, reject })
    pumpNodesFetchQueue()
  })
}

function pumpNodesFetchQueue() {
  if (nodesThrottleTimer != null) return
  if (!nodesFetchQueue.length) return

  const now = Date.now()
  const wait = Math.max(0, lastNodesFetchAt + FETCH_THROTTLE_MS - now)

  nodesThrottleTimer = setTimeout(() => {
    nodesThrottleTimer = null
    void drainNodesFetchQueue()
  }, wait)
}

async function drainNodesFetchQueue() {
  if (!nodesFetchQueue.length) return

  // LIFO: pop from end. Drain everything currently queued into one batch so
  // rapid click-around coalesces; newest jobs were pushed last.
  /** @type {typeof nodesFetchQueue} */
  const jobs = []
  while (nodesFetchQueue.length) jobs.push(nodesFetchQueue.pop())

  const slugSet = new Set()
  for (const j of jobs) {
    for (const s of j.slugs) slugSet.add(s)
  }
  const batch = [...slugSet]
  lastNodesFetchAt = Date.now()

  try {
    // Persistent WS — avoids HTTP GET /nodes storms under multi-select / chat
    const data = await wsFetchNodes(batch)
    for (const j of jobs) j.resolve(data)
  } catch (err) {
    for (const j of jobs) j.reject(err)
  }

  // More jobs may have arrived during the request
  pumpNodesFetchQueue()
}

function emptyState() {
  return {
    /** @type {Record<string, EntityRecord>} */
    bySlug: {},
    /** Bumped on any cache change so m.js bindings re-run. */
    rev: 0,
  }
}

/**
 * Reactive entity store (survives HMR via M.store bucket).
 */
export function entityStore() {
  try {
    const s = M.store(STORE)
    if (s && typeof s === 'object' && s.bySlug != null) return s
  } catch {
    /* not registered yet */
  }
  return M.store(STORE, emptyState())
}

/** Ensure store exists (call from ui boot). Warm the entity WebSocket. */
export function initEntityModel() {
  const s = entityStore()
  // Open WS early so first inspector open isn't blocked on handshake
  try {
    void ensureEntityWs()
  } catch {
    /* ignore */
  }
  return s
}

function bump(reason = '') {
  const s = entityStore()
  s.rev = (s.rev || 0) + 1
  return s
}

function notify(slug, rec, reason) {
  for (const fn of listeners) {
    try {
      fn(slug, rec, reason)
    } catch (err) {
      console.warn('[entity-model] listener error', err)
    }
  }
}

/**
 * Subscribe to cache mutations (upsert / remove / evict).
 * @param {(slug: string, rec: EntityRecord|null, reason: string) => void} fn
 * @returns {() => void} unsubscribe
 */
export function subscribe(fn) {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

/** @param {string} slug */
export function get(slug) {
  if (!slug) return null
  return entityStore().bySlug[slug] || null
}

/**
 * Non-reactive label lookup (store + window cache). Prefer this from
 * entityLabel() pills so hundreds of bindings don't subscribe to every rev.
 * @param {string} slug
 * @returns {string}
 */
export function peekLabel(slug) {
  if (!slug) return ''
  const rec = entityStore().bySlug[slug]
  if (rec?.label && rec.label !== slug) return rec.label
  if (rec?.deleted) return slug
  const fromBody = labelFromComponents(rec)
  if (fromBody && fromBody !== slug) return fromBody
  try {
    const c = window.__ENTITY_LABEL_CACHE__
    if (c?.has(slug)) {
      const n = c.get(slug)
      if (n && n !== slug) return n
    }
  } catch {
    /* ignore */
  }
  return rec?.label || fromBody || slug
}

/** Reactive read helper — touch rev so x-text re-runs when desired. */
export function getLabel(slug) {
  void entityStore().rev
  return peekLabel(slug)
}

/**
 * @param {EntityRecord|null|undefined} rec
 * @param {string} [fallback]
 */
/**
 * Best-effort label from an entity body when no server `label` is present.
 * Prefers common name fields, then Address-style `info.address`.
 * Authoritative path is server-side schema `displayField` (attached as `label`
 * on /nodes and /labels responses).
 */
export function labelFromComponents(rec, fallback = '') {
  if (!rec) return fallback
  const comps = rec.components || {}
  const pick = (v) => {
    if (v == null || typeof v === 'object') return ''
    const t = String(v).trim()
    return t
  }
  for (const key of ['info', 'meta', 'profile', 'identity', 'naming']) {
    const bag = comps[key]
    if (!bag || typeof bag !== 'object') continue
    for (const fname of [
      'name',
      'title',
      'label',
      'display_name',
      'full_name',
      'address',
    ]) {
      const t = pick(bag[fname])
      if (t) return t
    }
  }
  if (rec.label && String(rec.label).trim()) return String(rec.label).trim()
  return fallback || rec.slug || ''
}

/**
 * True when the cache entry has a real entity body (not a label-only stub).
 * Label stubs use components: {} which is truthy — must not skip /nodes fetch.
 * @param {EntityRecord|null|undefined} rec
 */
export function hasEntityBody(rec) {
  if (!rec || rec.deleted || rec.error || rec.labelOnly) return false
  const c = rec.components
  if (c == null || typeof c !== 'object') return false
  return Object.keys(c).length > 0
}

/** @param {EntityRecord|null|undefined} rec */
function needsFetch(rec) {
  if (!rec) return true
  if (rec.loading) return true
  if (rec.error || rec.deleted) return true
  if (!hasEntityBody(rec)) return true
  return false
}

/**
 * Cancel a pending grace-period eviction (refcount went back above zero).
 * @param {string} slug
 */
function cancelScheduledEvict(slug) {
  const t = evictTimers.get(slug)
  if (t == null) return
  clearTimeout(t)
  evictTimers.delete(slug)
}

/**
 * After refcount hits 0, wait EVICT_GRACE_MS of continuous zero-refs before unload.
 * Re-acquire / any acquire cancels the timer so rapid UI churn keeps the cache.
 * @param {string} slug
 */
function scheduleEvict(slug) {
  if (!slug) return
  cancelScheduledEvict(slug)
  if (refCount(slug) > 0) return
  const t = setTimeout(() => {
    evictTimers.delete(slug)
    // Still unheld for the full grace window?
    if (refCount(slug) === 0) evict(slug)
  }, EVICT_GRACE_MS)
  evictTimers.set(slug, t)
}

/**
 * Register a UI holder for this slug. While refcount > 0 the record stays cached.
 * @param {string} slug
 * @param {string} holderId e.g. 'insp', 'dom-links', 'chat-card:…'
 */
export function acquire(slug, holderId) {
  if (!slug || !holderId) return
  let set = holders.get(slug)
  if (!set) {
    set = new Set()
    holders.set(slug, set)
  }
  set.add(holderId)
  // Any live holder cancels a pending unload
  cancelScheduledEvict(slug)
}

/**
 * Drop a holder; when no holders remain, schedule eviction after grace period.
 * @param {string} slug
 * @param {string} holderId
 */
export function release(slug, holderId) {
  if (!slug || !holderId) return
  const set = holders.get(slug)
  if (!set) return
  set.delete(holderId)
  if (set.size === 0) {
    holders.delete(slug)
    scheduleEvict(slug)
  }
}

/**
 * Replace the full holder set for a namespace (e.g. all on-screen entity links).
 * holders not in `slugs` are released; new ones are acquired.
 * @param {string} holderId
 * @param {string[]} slugs
 */
export function reconcileHolders(holderId, slugs) {
  if (!holderId) return
  const want = new Set((slugs || []).filter(Boolean))
  // Release slugs that had this holder but are no longer wanted
  for (const [slug, set] of [...holders.entries()]) {
    if (set.has(holderId) && !want.has(slug)) {
      release(slug, holderId)
    }
  }
  for (const slug of want) acquire(slug, holderId)
}

function refCount(slug) {
  return holders.get(slug)?.size || 0
}

/**
 * Unload from cache now (if still unheld). Prefer scheduleEvict for normal paths.
 * @param {string} slug
 */
export function evict(slug) {
  if (!slug) return
  cancelScheduledEvict(slug)
  if (refCount(slug) > 0) return
  const s = entityStore()
  if (!s.bySlug[slug]) return
  const next = { ...s.bySlug }
  delete next[slug]
  s.bySlug = next
  bump('evict')
  notify(slug, null, 'evict')
}

/**
 * Insert or replace a full entity snapshot (from /nodes or entity_changed).
 * @param {object} raw
 * @param {{ reason?: string, silent?: boolean }} [opts]
 *   silent: write cache without bump/notify (bulk ensureMany); call
 *   notifyBatch() once afterward.
 * @returns {EntityRecord|null}
 */
export function upsert(raw, opts = {}) {
  if (!raw?.slug) return null
  const slug = String(raw.slug)
  const prev = get(slug) || { slug }
  const comps =
    raw.components !== undefined ? raw.components : prev.components
  const bodyOk =
    comps != null &&
    typeof comps === 'object' &&
    !Array.isArray(comps) &&
    Object.keys(comps).length > 0
  /** @type {EntityRecord} */
  const rec = {
    ...prev,
    slug,
    components: comps,
    relations: raw.relations !== undefined ? raw.relations : prev.relations,
    incoming: raw.incoming !== undefined ? raw.incoming : prev.incoming,
    i: raw.i != null ? raw.i : prev.i,
    error: raw.error,
    deleted: !!raw.deleted,
    loading: false,
    // Real body from /nodes / entity_changed clears label-only stub flag
    labelOnly: raw.labelOnly === true ? true : bodyOk ? false : !!prev.labelOnly,
    fetchedAt: Date.now(),
    includeLinks:
      raw.includeLinks != null
        ? !!raw.includeLinks
        : raw.incoming != null
          ? true
          : prev.includeLinks,
    label:
      (raw.label && String(raw.label).trim()) ||
      labelFromComponents(raw) ||
      labelFromComponents(prev) ||
      prev.label ||
      slug,
  }
  // Skip reactive notify when nothing material changed (stops label/hydrate loops)
  const reason = opts.reason || 'upsert'
  if (
    prev &&
    prev.slug === rec.slug &&
    prev.label === rec.label &&
    prev.loading === rec.loading &&
    prev.error === rec.error &&
    prev.deleted === rec.deleted &&
    prev.labelOnly === rec.labelOnly &&
    prev.components === rec.components &&
    prev.relations === rec.relations &&
    prev.incoming === rec.incoming
  ) {
    try {
      if (rec.label) {
        ;(window.__ENTITY_LABEL_CACHE__ ??= new Map()).set(slug, rec.label)
      }
    } catch {
      /* ignore */
    }
    return prev
  }

  try {
    const c = (window.__ENTITY_LABEL_CACHE__ ??= new Map())
    if (rec.label) c.set(slug, rec.label)
  } catch {
    /* ignore */
  }
  const s = entityStore()
  // Mutate bag in place when possible — replacing whole bySlug re-triggers
  // every subscriber of bySlug on each single-entity write.
  if (!s.bySlug[slug]) {
    s.bySlug = { ...s.bySlug, [slug]: rec }
  } else {
    s.bySlug[slug] = rec
  }

  if (!opts.silent) {
    bump(reason)
    notify(slug, rec, reason)
  }
  if (refCount(slug) === 0) scheduleEvict(slug)
  else cancelScheduledEvict(slug)
  return rec
}

/**
 * One rev bump + notify after a silent bulk upsert pass.
 * @param {string} reason
 * @param {string[]} [slugs]
 */
export function notifyBatch(reason = 'fetchMany', slugs = []) {
  bump(reason)
  if (slugs.length) {
    for (const slug of slugs) notify(slug, get(slug), reason)
  } else {
    notify('*', null, reason)
  }
}

/**
 * Mark deleted (or remove from cache).
 * @param {string} slug
 */
export function markDeleted(slug) {
  if (!slug) return
  const rec = {
    slug,
    deleted: true,
    components: {},
    relations: {},
    incoming: [],
    label: slug,
    fetchedAt: Date.now(),
    loading: false,
  }
  const s = entityStore()
  s.bySlug = { ...s.bySlug, [slug]: rec }
  bump('delete')
  notify(slug, rec, 'delete')
  try {
    window.__ENTITY_LABEL_CACHE__?.delete?.(slug)
  } catch {
    /* ignore */
  }
  if (refCount(slug) === 0) scheduleEvict(slug)
  else cancelScheduledEvict(slug)
}

/**
 * Apply a chat/NDJSON entity_changed event from the server.
 * @param {{ slug?: string, entity?: object, deleted?: boolean, stale?: boolean }} msg
 */
export async function applyServerChange(msg) {
  if (!msg?.slug) return
  const slug = String(msg.slug)
  if (msg.deleted) {
    markDeleted(slug)
    return
  }
  if (msg.entity && typeof msg.entity === 'object') {
    upsert({ ...msg.entity, slug }, { reason: 'server' })
    return
  }
  // stale / no body — refetch if held
  if (refCount(slug) > 0 || get(slug)) {
    await ensure(slug, { force: true, includeLinks: true })
  }
}

/**
 * Set display label without a full entity body (from /labels).
 * No-ops when the label is already current — critical to avoid
 * notify → hydrate → setLabel → notify hot loops.
 * @param {string} slug
 * @param {string} label
 */
export function setLabel(slug, label) {
  if (!slug) return
  const name = (label != null && String(label).trim()) || slug
  // Always keep legacy cache in sync (no reactive notify)
  try {
    ;(window.__ENTITY_LABEL_CACHE__ ??= new Map()).set(slug, name)
  } catch {
    /* ignore */
  }
  const prev = get(slug)
  if (prev) {
    if (prev.label === name) return // already current — do not upsert/notify
    if (hasEntityBody(prev)) {
      upsert({ ...prev, label: name, labelOnly: false }, { reason: 'label' })
    } else {
      upsert(
        { ...prev, label: name, labelOnly: true, components: prev.components },
        { reason: 'label' },
      )
    }
    return
  }
  // Always write label-only stubs into the store — relation pills / chat links
  // often have no holder yet. Without this, ensureLabels only filled the
  // window cache and entityLabel() stayed stuck on the slug forever.
  // Unheld stubs still age out via the normal 60s grace eviction.
  upsert(
    { slug, label: name, labelOnly: true, components: undefined },
    { reason: 'label' },
  )
}

/**
 * Fetch one entity into the cache (coalesced).
 * @param {string} slug
 * @param {{ force?: boolean, includeLinks?: boolean }} [opts]
 * @returns {Promise<EntityRecord|null>}
 */
export async function ensure(slug, opts = {}) {
  if (!slug) return null
  const includeLinks = opts.includeLinks !== false
  const prev = get(slug)
  if (
    !opts.force &&
    prev &&
    !prev.loading &&
    hasEntityBody(prev) &&
    !prev.error &&
    !prev.deleted &&
    (!includeLinks || prev.includeLinks || prev.incoming)
  ) {
    return prev
  }
  if (inflight.has(slug)) return inflight.get(slug)

  const p = (async () => {
    const s = entityStore()
    const placeholder = {
      ...(get(slug) || { slug }),
      slug,
      loading: true,
    }
    // In-place write, no rev bump for loading (UI uses insp.loading)
    s.bySlug[slug] = placeholder
    try {
      const data = await enqueueNodesFetch([slug])
      const raw = (data.entities || []).find((e) => e?.slug === slug) ||
        (data.entities || [])[0]
      if (!raw || raw.error) {
        const errRec = {
          slug,
          error: raw?.error || data.error || 'not found',
          loading: false,
          fetchedAt: Date.now(),
        }
        s.bySlug[slug] = errRec
        notifyBatch('error', [slug])
        return errRec
      }
      const touched = [slug]
      for (const ent of data.entities || []) {
        if (!ent?.slug || ent.error) continue
        if (ent.slug === slug) continue
        upsert({ ...ent, includeLinks: true }, { reason: 'fetch', silent: true })
        touched.push(ent.slug)
      }
      const out = upsert(
        { ...raw, slug, includeLinks: true },
        { reason: 'fetch', silent: true },
      )
      notifyBatch('fetch', touched)
      return out
    } catch (err) {
      const errRec = {
        slug,
        error: err?.message || String(err),
        loading: false,
        fetchedAt: Date.now(),
      }
      s.bySlug[slug] = errRec
      notifyBatch('error', [slug])
      return errRec
    } finally {
      inflight.delete(slug)
    }
  })()
  inflight.set(slug, p)
  return p
}

/**
 * Fetch many entities (one /nodes round-trip). Acquires nothing — caller holds refs.
 * @param {string[]} slugs
 * @param {{ force?: boolean }} [opts]
 * @returns {Promise<EntityRecord[]>}
 */
export async function ensureMany(slugs, opts = {}) {
  const list = [...new Set((slugs || []).filter(Boolean))]
  if (!list.length) return []

  const need = opts.force
    ? list
    : list.filter((s) => needsFetch(get(s)))

  if (need.length) {
    // Mark loading in place — no rev bump (avoids thrashing every entityLabel)
    const s0 = entityStore()
    for (const slug of need) {
      s0.bySlug[slug] = {
        ...(s0.bySlug[slug] || { slug }),
        slug,
        loading: true,
      }
    }
    try {
      const data = await enqueueNodesFetch(need)
      const touched = []
      for (const raw of data.entities || []) {
        if (!raw?.slug) continue
        if (raw.error) {
          s0.bySlug[raw.slug] = {
            slug: raw.slug,
            error: raw.error,
            loading: false,
            fetchedAt: Date.now(),
          }
          touched.push(raw.slug)
        } else {
          upsert(
            { ...raw, includeLinks: true },
            { reason: 'fetch', silent: true },
          )
          touched.push(raw.slug)
        }
      }
      for (const slug of need) {
        const r = get(slug)
        if (r?.loading) {
          s0.bySlug[slug] = {
            ...r,
            loading: false,
            error: r.error || 'not found',
            fetchedAt: Date.now(),
          }
          if (!touched.includes(slug)) touched.push(slug)
        }
      }
      // Single reactive wave for the whole batch (avoids effect-cap storms)
      notifyBatch('fetchMany', touched)
    } catch (err) {
      console.warn('[entity-model] ensureMany failed', err)
      const touched = []
      for (const slug of need) {
        s0.bySlug[slug] = {
          slug,
          error: err?.message || String(err),
          loading: false,
          fetchedAt: Date.now(),
        }
        touched.push(slug)
      }
      notifyBatch('error', touched)
    }
  }

  return list.map((s) => get(s)).filter(Boolean)
}

/**
 * Batch labels via /labels for slugs missing a name (link hydration).
 * @param {string[]} slugs
 * @returns {Promise<Map<string, string>>}
 */
export async function ensureLabels(slugs) {
  const out = new Map()
  const need = []
  for (const s of slugs || []) {
    if (!s) continue
    const rec = get(s)
    const lab = rec?.label || labelFromComponents(rec)
    if (lab && lab !== s) {
      out.set(s, lab)
      continue
    }
    try {
      const c = window.__ENTITY_LABEL_CACHE__
      if (c?.has(s) && c.get(s) !== s) {
        out.set(s, c.get(s))
        setLabel(s, c.get(s))
        continue
      }
    } catch {
      /* ignore */
    }
    need.push(s)
  }
  if (!need.length) return out

  const CHUNK = 40
  for (let i = 0; i < need.length; i += CHUNK) {
    const chunk = need.slice(i, i + CHUNK)
    try {
      const data = await wsFetchLabels(chunk)
      const labels = data.labels || {}
      for (const s of chunk) {
        const name = (labels[s] != null && String(labels[s]).trim()) || s
        setLabel(s, name)
        out.set(s, name)
      }
    } catch {
      for (const s of chunk) {
        out.set(s, s)
        setLabel(s, s)
      }
    }
  }
  return out
}

/**
 * Force refetch if the entity is held (or always if force).
 * @param {string} slug
 */
export async function refresh(slug, opts = {}) {
  if (!slug) return null
  if (!opts.force && refCount(slug) === 0 && !get(slug)) return null
  return ensure(slug, { force: true, includeLinks: true })
}

export { EVICT_GRACE_MS, FETCH_THROTTLE_MS }

export default {
  initEntityModel,
  entityStore,
  subscribe,
  get,
  getLabel,
  peekLabel,
  labelFromComponents,
  hasEntityBody,
  acquire,
  release,
  reconcileHolders,
  evict,
  upsert,
  markDeleted,
  applyServerChange,
  setLabel,
  ensure,
  ensureMany,
  ensureLabels,
  refresh,
  notifyBatch,
  EVICT_GRACE_MS,
  FETCH_THROTTLE_MS,
}
