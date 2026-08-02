/**
 * Entity display-name cache + DOM hydration for a[data-entity] links.
 *
 * - Fetches /labels?slugs=… in batches (deduped, in-flight coalesced)
 * - Sets anchor text to the human name when the current text is empty / the slug
 * - Always sets native title= tooltip to the slug
 */
const cache = (window.__ENTITY_LABEL_CACHE__ ??= new Map())
/** @type {Map<string, Promise<string>>} */
const inflight = (window.__ENTITY_LABEL_INFLIGHT__ ??= new Map())

/**
 * @param {string} slug
 * @returns {Promise<string>}
 */
export async function resolveLabel(slug) {
  if (!slug) return ''
  if (cache.has(slug)) return cache.get(slug)
  if (inflight.has(slug)) return inflight.get(slug)
  const p = fetchLabels([slug]).then((m) => m.get(slug) || slug)
  inflight.set(slug, p)
  try {
    return await p
  } finally {
    inflight.delete(slug)
  }
}

/**
 * @param {string[]} slugs
 * @returns {Promise<Map<string, string>>}
 */
export async function fetchLabels(slugs) {
  const out = new Map()
  const need = []
  for (const s of slugs) {
    if (!s) continue
    if (cache.has(s)) out.set(s, cache.get(s))
    else need.push(s)
  }
  if (!need.length) return out

  // Chunk to keep URLs reasonable
  const CHUNK = 40
  for (let i = 0; i < need.length; i += CHUNK) {
    const chunk = need.slice(i, i + CHUNK)
    try {
      const res = await fetch(
        '/labels?slugs=' + chunk.map(encodeURIComponent).join(','),
      )
      const data = await res.json()
      const labels = data.labels || {}
      for (const s of chunk) {
        const name = (labels[s] != null && String(labels[s]).trim()) || s
        cache.set(s, name)
        out.set(s, name)
      }
    } catch {
      for (const s of chunk) {
        cache.set(s, s)
        out.set(s, s)
      }
    }
  }
  return out
}

/**
 * Should we replace the link's visible text with the resolved name?
 * Keep custom LLM prose and wiki `|display text` (data-fixed-label);
 * replace bare slugs / empty / code-wrapped slugs only.
 * @param {HTMLElement} a
 * @param {string} slug
 */
function shouldReplaceText(a, slug) {
  // Wiki `[[Class/id|display text]]` / mention chips with an author-chosen label
  if (a.dataset.fixedLabel === '1' || a.getAttribute('data-fixed-label') === '1') {
    return false
  }
  // Ignore icon glyph text when comparing
  const clone = a.cloneNode(true)
  clone.querySelectorAll('.entity-link-icon').forEach((n) => n.remove())
  const text = (clone.textContent || '').trim()
  if (!text) return true
  if (text === slug) return true
  // <a><code>Class/id</code></a>
  const code = a.querySelector('code')
  if (code && (code.textContent || '').trim() === slug) return true
  return false
}

/**
 * Ensure a leading in-app citation icon (Phosphor) left of the label.
 * @param {HTMLAnchorElement} a
 */
export function ensureEntityLinkIcon(a) {
  // Any existing cube (including nested in .hit-line) counts — avoid doubles
  if (!a || a.querySelector('.entity-link-icon')) return
  a.classList.add('entity-link')
  const icon = document.createElement('i')
  icon.className = 'ph ph-cube entity-link-icon'
  icon.setAttribute('aria-hidden', 'true')
  a.insertBefore(icon, a.firstChild)
}

/**
 * Hydrate all (or under root) entity links: name as text, slug as title, icon.
 * @param {ParentNode} [root]
 * @returns {Promise<void>}
 */
export async function hydrateEntityLinks(root = document) {
  const nodes = root.querySelectorAll
    ? root.querySelectorAll('a[data-entity]')
    : []
  /** @type {HTMLAnchorElement[]} */
  const anchors = [...nodes]
  if (!anchors.length) return

  const slugSet = new Set()
  for (const a of anchors) {
    const slug = String(a.dataset.entity || '')
      .split(',')
      .filter(Boolean)[0]
    if (slug) slugSet.add(slug)
  }
  if (!slugSet.size) return

  const labels = await fetchLabels([...slugSet])

  for (const a of anchors) {
    const slugs = String(a.dataset.entity || '')
      .split(',')
      .filter(Boolean)
    if (!slugs.length) continue
    const slug = slugs[0]
    const name = labels.get(slug) || slug

    // Native tooltip always shows the slug (and multi-select list if any)
    a.setAttribute('title', slugs.join(', '))
    a.dataset.label = name
    ensureEntityLinkIcon(a)

    if (!shouldReplaceText(a, slug)) continue

    const code = a.querySelector('code')
    if (code && (code.textContent || '').trim() === slug) {
      code.textContent = name
    } else {
      // Prefer updating a .slug child (SERPS / citation hit rows)
      const slugEl = a.querySelector('.slug')
      if (slugEl) {
        slugEl.textContent = name
      } else if (
        a.childElementCount === 1 &&
        a.querySelector(':scope > .entity-link-icon')
      ) {
        // Icon + bare text node(s)
        for (const node of [...a.childNodes]) {
          if (node.nodeType === Node.TEXT_NODE) node.remove()
        }
        a.appendChild(document.createTextNode(name))
      } else if (a.childElementCount === 0) {
        a.textContent = name
        ensureEntityLinkIcon(a)
      } else {
        // Keep structure; only rewrite pure text nodes after the icon
        let replaced = false
        for (const node of [...a.childNodes]) {
          if (node.nodeType === Node.TEXT_NODE && node.textContent.trim()) {
            node.textContent = name
            replaced = true
            break
          }
        }
        if (!replaced) {
          a.appendChild(document.createTextNode(' ' + name))
        }
      }
    }
  }
}

/**
 * Resolve labels for a list of row objects with .slug; mutates .title / .label.
 * @param {Array<{ slug?: string, title?: string, label?: string }>} rows
 */
export async function labelRows(rows) {
  if (!rows?.length) return rows
  const slugs = rows.map((r) => r.slug).filter(Boolean)
  const labels = await fetchLabels(slugs)
  for (const r of rows) {
    if (!r.slug) continue
    const name = labels.get(r.slug) || r.slug
    r.label = name
    // Keep title as display string for existing x-text bindings
    if (!r.title || r.title === r.slug) r.title = name
  }
  return rows
}
