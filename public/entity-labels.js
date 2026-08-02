/**
 * Entity display-name cache + DOM hydration for a[data-entity] links.
 *
 * Labels live in entity-model.js. Network/WS lookups are deferred until an
 * anchor is actually visible (IntersectionObserver) — below-the-fold chat
 * history and off-screen pills do not fetch until first seen.
 */
import {
  ensureLabels,
  getLabel,
  reconcileHolders,
} from '/entity-model.js'

const LINK_HOLDER = 'dom-links'
const HYDRATED_ATTR = 'data-entity-hydrated'
const PENDING_ATTR = 'data-entity-pending'

/** @type {IntersectionObserver|null} */
let visibilityObserver = null
/** Anchors currently scheduled on the IntersectionObserver */
const observed = new WeakSet()

/**
 * @param {string} slug
 * @returns {Promise<string>}
 */
export async function resolveLabel(slug) {
  if (!slug) return ''
  const map = await ensureLabels([slug])
  return map.get(slug) || getLabel(slug) || slug
}

/**
 * @param {string[]} slugs
 * @returns {Promise<Map<string, string>>}
 */
export async function fetchLabels(slugs) {
  return ensureLabels(slugs || [])
}

/**
 * Should we replace the link's visible text with the resolved name?
 * Keep custom LLM prose, wiki `|display text` (data-fixed-label), and
 * anchors whose label is bound by m.js x-text (data-label-bound) — those
 * must not be mutated by DOM hydrate (would double-paint slug + name).
 * Replace bare slugs / empty / code-wrapped slugs only.
 * @param {HTMLElement} a
 * @param {string} slug
 */
function shouldReplaceText(a, slug) {
  if (a.dataset.fixedLabel === '1' || a.getAttribute('data-fixed-label') === '1') {
    return false
  }
  // Reactive label from entityLabel() / r.title — store is source of truth
  if (a.dataset.labelBound === '1' || a.getAttribute('data-label-bound') === '1') {
    return false
  }
  const clone = a.cloneNode(true)
  clone.querySelectorAll('.entity-link-icon').forEach((n) => n.remove())
  const text = (clone.textContent || '').trim()
  if (!text) return true
  if (text === slug) return true
  const code = a.querySelector('code')
  if (code && (code.textContent || '').trim() === slug) return true
  return false
}

/** Reactive m.js pills own their icon; DOM hydrate must not inject another cube. */
function isLabelBound(a) {
  return (
    a?.dataset?.labelBound === '1' ||
    a?.getAttribute?.('data-label-bound') === '1'
  )
}

/**
 * Ensure the anchor has exactly one cube icon (ready state).
 * Replaces a loading spinner if present. No-op for data-label-bound anchors
 * (template owns spinner/cube via is-loading class).
 * @param {HTMLAnchorElement} a
 */
export function ensureEntityLinkIcon(a) {
  if (!a || isLabelBound(a)) return
  a.classList.add('entity-link')
  a.classList.remove('is-loading')
  const icons = [...a.querySelectorAll(':scope > .entity-link-icon')]
  const cube = icons.find(
    (el) => el.classList.contains('ph-cube') && !el.classList.contains('entity-link-loading'),
  )
  // Drop extras (spinner + duplicate cubes from older hydrates)
  for (const el of icons) {
    if (el !== cube) el.remove()
  }
  if (cube) return
  const icon = document.createElement('i')
  icon.className = 'ph ph-cube entity-link-icon'
  icon.setAttribute('aria-hidden', 'true')
  a.insertBefore(icon, a.firstChild)
}

/**
 * Show animated spinner in place of the cube while label data is queued/loading.
 * No-op for data-label-bound anchors (UI uses :class is-loading + CSS).
 * @param {HTMLAnchorElement} a
 * @param {boolean} loading
 */
export function setEntityLinkLoading(a, loading) {
  if (!a || isLabelBound(a)) return
  a.classList.add('entity-link')
  if (!loading) {
    ensureEntityLinkIcon(a)
    a.removeAttribute(PENDING_ATTR)
    return
  }
  a.classList.add('is-loading')
  a.setAttribute(PENDING_ATTR, '1')
  // Single spinner icon — remove any cubes/spinners first
  for (const el of [...a.querySelectorAll(':scope > .entity-link-icon')]) {
    el.remove()
  }
  const spin = document.createElement('span')
  spin.className = 'entity-link-icon entity-link-loading spin'
  spin.setAttribute('aria-hidden', 'true')
  spin.setAttribute('title', 'Loading…')
  a.insertBefore(spin, a.firstChild)
}

/**
 * Primary slug on an entity anchor.
 * @param {HTMLElement} a
 * @returns {string}
 */
function primarySlug(a) {
  return (
    String(a?.dataset?.entity || '')
      .split(',')
      .filter(Boolean)[0] || ''
  )
}

/**
 * Collect slugs currently referenced by a[data-entity] under root.
 * @param {ParentNode} [root]
 * @returns {string[]}
 */
export function collectEntityLinkSlugs(root = document) {
  const nodes = root.querySelectorAll
    ? root.querySelectorAll('a[data-entity]')
    : []
  const slugSet = new Set()
  for (const a of nodes) {
    for (const s of String(a.dataset.entity || '')
      .split(',')
      .filter(Boolean)) {
      slugSet.add(s)
    }
  }
  return [...slugSet]
}

/**
 * Slugs for anchors already hydrated (visible at least once).
 * Used for entity-model refcount — don't hold below-the-fold unfetched links.
 * @param {ParentNode} [root]
 */
export function collectHydratedEntityLinkSlugs(root = document) {
  const nodes = root.querySelectorAll
    ? root.querySelectorAll(`a[data-entity][${HYDRATED_ATTR}="1"]`)
    : []
  const slugSet = new Set()
  for (const a of nodes) {
    const s = primarySlug(a)
    if (s) slugSet.add(s)
  }
  return [...slugSet]
}

/**
 * Apply a resolved display name onto an anchor (DOM only).
 * @param {HTMLAnchorElement} a
 * @param {string} slug
 * @param {string} name
 */
function applyLabelToAnchor(a, slug, name) {
  const slugs = String(a.dataset.entity || '')
    .split(',')
    .filter(Boolean)
  a.setAttribute('title', slugs.join(', ') || slug)
  a.dataset.label = name
  ensureEntityLinkIcon(a) // cube (replaces spinner if any)
  a.setAttribute(HYDRATED_ATTR, '1')
  a.removeAttribute(PENDING_ATTR)
  a.classList.remove('is-loading')

  if (!shouldReplaceText(a, slug)) return

  const code = a.querySelector('code')
  if (code && (code.textContent || '').trim() === slug) {
    code.textContent = name
    return
  }
  // Prefer dedicated label slots (.slug, .*-label) over appending siblings
  const labelEl =
    a.querySelector(':scope > .slug') ||
    a.querySelector(':scope > .insp-rel-pill-label') ||
    a.querySelector(':scope > .citation-pill-label') ||
    a.querySelector(':scope > .entity-link-label') ||
    a.querySelector(':scope > span:not(.entity-link-icon)')
  if (labelEl) {
    labelEl.textContent = name
    // Drop any stray text nodes left from older buggy hydrates (slug + name)
    for (const node of [...a.childNodes]) {
      if (node.nodeType === Node.TEXT_NODE && node.textContent.trim()) {
        node.remove()
      }
    }
    return
  }
  if (
    a.childElementCount === 1 &&
    a.querySelector(':scope > .entity-link-icon')
  ) {
    for (const node of [...a.childNodes]) {
      if (node.nodeType === Node.TEXT_NODE) node.remove()
    }
    a.appendChild(document.createTextNode(name))
    return
  }
  if (a.childElementCount === 0) {
    a.textContent = name
    ensureEntityLinkIcon(a)
    return
  }
  let replaced = false
  for (const node of [...a.childNodes]) {
    if (node.nodeType === Node.TEXT_NODE && node.textContent.trim()) {
      node.textContent = name
      replaced = true
      break
    }
  }
  // Never append a second label next to existing element children — that
  // produces "Class/id Display Name". Prefer replacing first text-bearing child.
  if (!replaced) {
    const anyTextEl = a.querySelector('span, code, .slug')
    if (anyTextEl) anyTextEl.textContent = name
  }
}

/**
 * Fetch + paint one anchor (first-visible path).
 * @param {HTMLAnchorElement} a
 */
async function hydrateOneAnchor(a) {
  if (!a?.isConnected) return
  if (a.getAttribute(HYDRATED_ATTR) === '1') return
  if (a.getAttribute(PENDING_ATTR) === '1') return

  const slug = primarySlug(a)
  if (!slug) return

  // m.js-bound labels/icons — never mutate DOM (would double the cube)
  if (isLabelBound(a)) {
    a.classList.add('entity-link')
    a.setAttribute(HYDRATED_ATTR, '1')
    return
  }

  // Fixed display text: icon + title only, no network
  if (a.dataset.fixedLabel === '1' || a.getAttribute('data-fixed-label') === '1') {
    const slugs = String(a.dataset.entity || '')
      .split(',')
      .filter(Boolean)
    a.setAttribute('title', slugs.join(', ') || slug)
    ensureEntityLinkIcon(a)
    a.setAttribute(HYDRATED_ATTR, '1')
    return
  }

  // Cache hit: paint sync, no WS
  const cached = getLabel(slug)
  if (cached && cached !== slug) {
    applyLabelToAnchor(a, slug, cached)
    return
  }

  // Queued / in-flight: spinner instead of cube until label resolves
  setEntityLinkLoading(a, true)
  try {
    const labels = await ensureLabels([slug])
    if (!a.isConnected) return
    const name = labels.get(slug) || getLabel(slug) || slug
    applyLabelToAnchor(a, slug, name)
  } catch {
    if (a.isConnected) {
      applyLabelToAnchor(a, slug, slug)
    }
  }
}

function ensureVisibilityObserver() {
  if (visibilityObserver) return visibilityObserver
  if (typeof IntersectionObserver === 'undefined') {
    // No IO (tests / ancient browsers): hydrate eagerly on schedule
    visibilityObserver = {
      observe(el) {
        void hydrateOneAnchor(/** @type {HTMLAnchorElement} */ (el))
      },
      unobserve() {},
      disconnect() {},
    }
    return visibilityObserver
  }
  visibilityObserver = new IntersectionObserver(
    (entries) => {
      const work = []
      for (const entry of entries) {
        if (!entry.isIntersecting) continue
        const a = /** @type {HTMLAnchorElement} */ (entry.target)
        visibilityObserver?.unobserve(a)
        work.push(hydrateOneAnchor(a))
      }
      if (!work.length) return
      // After first-sees settle, hold only hydrated anchors in the model
      void Promise.all(work).then(() => {
        reconcileHolders(LINK_HOLDER, collectHydratedEntityLinkSlugs(document))
      })
    },
    {
      // root null = viewport; small margin preloads just before scroll-in
      root: null,
      rootMargin: '80px 0px',
      threshold: 0,
    },
  )
  return visibilityObserver
}

/**
 * Schedule entity anchors for visibility-based label lookup.
 * Already-hydrated anchors are left alone; cached labels paint immediately;
 * others wait until first intersection.
 *
 * @param {ParentNode} [root]
 * @returns {Promise<void>}
 */
export async function hydrateEntityLinks(root = document) {
  const nodes = root.querySelectorAll
    ? root.querySelectorAll('a[data-entity]')
    : []
  /** @type {HTMLAnchorElement[]} */
  const anchors = [...nodes]
  if (!anchors.length) {
    reconcileHolders(LINK_HOLDER, collectHydratedEntityLinkSlugs(document))
    return
  }

  const io = ensureVisibilityObserver()

  for (const a of anchors) {
    const slug = primarySlug(a)
    if (!slug) continue

    // Reactive pills (inspector / citations / SERPS): m.js owns icon + label
    if (isLabelBound(a)) {
      a.classList.add('entity-link')
      a.setAttribute(HYDRATED_ATTR, '1')
      continue
    }

    ensureEntityLinkIcon(a)

    // Already done
    if (a.getAttribute(HYDRATED_ATTR) === '1') continue

    // Sync paint from cache / fixed label without waiting for IO
    if (a.dataset.fixedLabel === '1' || a.getAttribute('data-fixed-label') === '1') {
      void hydrateOneAnchor(a)
      continue
    }
    const cached = getLabel(slug)
    if (cached && cached !== slug) {
      void hydrateOneAnchor(a)
      continue
    }

    // Defer network until visible (IO fires immediately if already on-screen)
    if (!observed.has(a)) {
      observed.add(a)
      io.observe(a)
    }
  }

  // Holders only for anchors that have actually been hydrated
  reconcileHolders(LINK_HOLDER, collectHydratedEntityLinkSlugs(document))
}

/**
 * Resolve labels for a list of row objects with .slug; mutates .title / .label.
 * Used for SERPS rows that are already on-screen when the query returns.
 * @param {Array<{ slug?: string, title?: string, label?: string }>} rows
 */
export async function labelRows(rows) {
  if (!rows?.length) return rows
  const slugs = rows.map((r) => r.slug).filter(Boolean)
  const labels = await ensureLabels(slugs)
  for (const r of rows) {
    if (!r.slug) continue
    const name = labels.get(r.slug) || getLabel(r.slug) || r.slug
    r.label = name
    if (!r.title || r.title === r.slug) r.title = name
  }
  return rows
}
