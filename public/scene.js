// scene.js — the three.js world: 2M-node point cloud, gl1-style camera rig,
// picking, overlays, FPS histogram. HMR-aware:
//   - node data (40MB binary) is fetched once and cached on window.__VIZ_DATA__
//   - the camera rig + view state persist on window.__VIZ_STATE__
//   - each boot disposes the previous scene (listeners, RAF, GL resources)
// so editing this file hot-swaps behavior without losing your place in space.
// three.js is the inner partner: it only paints #world. HUD state flows
// through M.store('viz') (m.js outer shell); scene installs store.api actions
// and reads selection/highlights — never mounts into #app.
import * as THREE from 'three'
import M from 'https://mikesmullin.github.io/m-js/dist/m.min.js'

export async function boot() {
  if (window.__VIZ_SCENE__) window.__VIZ_SCENE__.dispose()

  const store = M.store('viz')

  // ---------- data (fetched once, survives HMR) ----------
  if (!window.__VIZ_DATA__) {
    const meta = await (await fetch('/meta.json')).json()
    const buf = await (await fetch('/data.bin')).arrayBuffer()
    window.__VIZ_DATA__ = { meta, buf }
  }
  const { meta, buf } = window.__VIZ_DATA__
  const n = meta.nodes
  const pos = new Float32Array(buf, 0, n * 3)
  const comp = new Uint32Array(buf, n * 12, n)
  const psize = new Float32Array(buf, n * 16, n)
  store.statusText = n.toLocaleString() + ' nodes · ' + meta.components.toLocaleString() + ' components'

  // color per node: component id -> hue (giant component dimmed so islands pop)
  const col = new Float32Array(n * 3)
  for (let i = 0; i < n; i++) {
    const h = ((comp[i] * 2654435761) >>> 0) % 360
    const s = comp[i] === 0 ? 0.35 : 0.75, l = comp[i] === 0 ? 0.42 : 0.6
    const k = (m2) => (m2 + h / 30) % 12
    const a = s * Math.min(l, 1 - l)
    const f = (m2) => l - a * Math.max(-1, Math.min(k(m2) - 3, 9 - k(m2), 1))
    col[i * 3] = f(0); col[i * 3 + 1] = f(8); col[i * 3 + 2] = f(4)
  }

  // ---------- renderer / scene ----------
  const canvas = document.getElementById('world')
  const renderer = new THREE.WebGLRenderer({ canvas, antialias: false, powerPreference: 'high-performance' })
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2))
  const scene = new THREE.Scene()
  scene.background = new THREE.Color(0x050507)
  const camera = new THREE.PerspectiveCamera(55, 1, 0.1, meta.world_radius * 20)

  const geo = new THREE.BufferGeometry()
  geo.setAttribute('position', new THREE.BufferAttribute(pos, 3))
  geo.setAttribute('color', new THREE.BufferAttribute(col, 3))
  geo.setAttribute('psize', new THREE.BufferAttribute(psize, 1))
  geo.computeBoundingSphere()
  const uni = { uZMix: { value: 1 }, uScale: { value: 1 } }
  // Solid flat discs at 50% opacity — glow removed (normal alpha blending, no
  // additive halo). The transparency is deliberate: overlapping nodes show
  // through each other, so dense clusters read as depth/stacking instead of
  // one opaque blob. Edge is antialiased with a tight smoothstep; interior
  // alpha is constant.
  const mat = new THREE.ShaderMaterial({
    uniforms: uni,
    transparent: true, depthWrite: false,
    vertexShader: [
      'attribute float psize; attribute vec3 color; varying vec3 vColor;',
      'uniform float uZMix; uniform float uScale;',
      'void main() {',
      '  vColor = color;',
      '  vec3 p = vec3(position.xy, position.z * uZMix);',
      '  vec4 mv = modelViewMatrix * vec4(p, 1.0);',
      '  gl_PointSize = clamp((1.5 + psize * 1.2) * uScale * 220.0 / -mv.z, 1.0, 18.0);',
      '  gl_Position = projectionMatrix * mv;',
      '}',
    ].join('\n'),
    fragmentShader: [
      'varying vec3 vColor;',
      'void main() {',
      '  vec2 d = gl_PointCoord - 0.5;',
      '  float r2 = dot(d, d);',
      '  if (r2 > 0.25) discard;',
      '  float a = smoothstep(0.25, 0.21, r2) * 0.5;',
      '  gl_FragColor = vec4(vColor, a);',
      '}',
    ].join('\n'),
  })
  const points = new THREE.Points(geo, mat)
  scene.add(points)

  function overlayLayer(color, size) {
    const g = new THREE.BufferGeometry()
    g.setAttribute('position', new THREE.BufferAttribute(new Float32Array(0), 3))
    const m2 = new THREE.PointsMaterial({ color, size, sizeAttenuation: false, transparent: true, opacity: 0.95, depthTest: false })
    const p = new THREE.Points(g, m2); p.renderOrder = 2; scene.add(p); return p
  }
  const highlights = overlayLayer(0xffe17a, 7)
  const selection = overlayLayer(0xffffff, 10)
  const pathLine = new THREE.Line(
    new THREE.BufferGeometry(),
    new THREE.LineBasicMaterial({ color: 0x9ecbff, transparent: true, opacity: 0.85, depthTest: false }))
  pathLine.renderOrder = 1
  scene.add(pathLine)

  // ---------- persistent view state ----------
  const st = window.__VIZ_STATE__ ??= {
    rig: { target: new THREE.Vector3(0, 0, 0), yaw: 0, pitch: -1.2, dist: 0 },
    zMixTarget: 1, zMix: 1, selectedIdx: [], slugCache: {}, highlightIdx: [],
  }
  st.selectedIdx ??= []   // migrate pre-multi-select persisted state
  st.slugCache ??= {}
  const rig = st.rig
  uni.uZMix.value = st.zMix
  store.is3d = st.zMixTarget === 1
  const anim = { active: false, t: 0, from: null, to: null }

  const nodeXYZ = (i) => new THREE.Vector3(pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2] * uni.uZMix.value)
  function setOverlay(layer, indices) {
    if (layer === highlights) st.highlightIdx = indices.slice()
    const a = new Float32Array(indices.length * 3)
    indices.forEach((idx, j) => { const v = nodeXYZ(idx); a[j * 3] = v.x; a[j * 3 + 1] = v.y; a[j * 3 + 2] = v.z })
    layer.geometry.setAttribute('position', new THREE.BufferAttribute(a, 3))
    layer.geometry.computeBoundingSphere()
  }

  // ---------- camera rig: gl1 canvas-scene control scheme (Z is up) ----------
  function rigDir() {
    const cp = Math.cos(rig.pitch)
    return new THREE.Vector3(Math.sin(rig.yaw) * cp, Math.cos(rig.yaw) * cp, Math.sin(rig.pitch))
  }
  function applyRig() {
    camera.position.copy(rig.target).addScaledVector(rigDir(), -rig.dist)
    camera.up.set(0, 0, 1)
    camera.lookAt(rig.target)
  }
  function flyTo(v, dist) {
    anim.active = true; anim.t = 0
    anim.from = { target: rig.target.clone(), dist: rig.dist }
    anim.to = { target: v.clone(), dist: dist ?? Math.max(30, rig.dist * 0.15) }
  }
  function fitDist(radius) {
    const vf = THREE.MathUtils.degToRad(camera.fov / 2)
    const hf = Math.atan(Math.tan(vf) * camera.aspect)
    return radius / (0.8 * Math.tan(Math.min(vf, hf)))
  }
  function frameIdxs(idxs) {
    if (!idxs.length) return
    const c = new THREE.Vector3()
    for (const i of idxs) c.add(nodeXYZ(i))
    c.divideScalar(idxs.length)
    let r = 20
    for (const i of idxs) r = Math.max(r, nodeXYZ(i).distanceTo(c))
    flyTo(c, fitDist(r * 1.15))
  }
  // F key: frame the union of search highlights (yellow) and selection (white).
  // Never prefer one to the exclusion of the other when both are non-empty.
  function frameSelection() {
    const seen = new Set()
    const union = []
    for (const i of st.highlightIdx || []) {
      if (i >= 0 && !seen.has(i)) { seen.add(i); union.push(i) }
    }
    for (const i of st.selectedIdx || []) {
      if (i >= 0 && !seen.has(i)) { seen.add(i); union.push(i) }
    }
    frameIdxs(union)
  }
  function frameUniverse() { flyTo(new THREE.Vector3(0, 0, 0), fitDist(meta.world_radius)) }
  /** Frame only the given slugs (entity-pill double-click camera focus — selection unchanged). */
  async function frameSlugs(slugs) {
    const list = (Array.isArray(slugs) ? slugs : [slugs]).filter(Boolean)
    if (!list.length) return
    const idxs = (await Promise.all(list.map(resolveSlug))).filter((i) => i >= 0)
    if (idxs.length) frameIdxs(idxs)
  }

  // ---------- input (all listeners on an AbortController for clean HMR) ----------
  const ac = new AbortController()
  const sig = { signal: ac.signal }
  const keys = new Set()
  let mmb = false, moved = false

  // True when the user is typing in a form field or contenteditable (search /
  // chat mention editors). Camera WASD / Space / hotkeys must not steal those.
  function isTextEditable(el) {
    if (!el || el === document.body || el === document.documentElement) return false
    // Walk up: click may land on a child (pill icon, span) inside the editor
    const node = el.nodeType === Node.TEXT_NODE ? el.parentElement : el
    if (!node || !node.closest) return false
    const host = node.closest(
      'input, textarea, select, [contenteditable=""], [contenteditable="true"], [contenteditable="plaintext-only"]',
    )
    if (!host) return false
    if (host.isContentEditable) return true
    const tag = host.tagName
    if (tag === 'TEXTAREA' || tag === 'SELECT') return true
    if (tag === 'INPUT') {
      const type = String(host.type || 'text').toLowerCase()
      // Non-textual inputs shouldn't block camera
      if (
        type === 'button' || type === 'submit' || type === 'reset' ||
        type === 'checkbox' || type === 'radio' || type === 'file' ||
        type === 'image' || type === 'range' || type === 'color' ||
        type === 'hidden'
      ) return false
      return true
    }
    return false
  }
  function isTypingFocus() {
    return isTextEditable(document.activeElement)
  }
  /** Camera may consume keys only when focus is not in a text editor. */
  function cameraKeysLive() {
    return !isTypingFocus()
  }

  addEventListener('keydown', (e) => {
    // Typing in search / chat / inspector: never feed the camera key set
    if (isTypingFocus() || isTextEditable(e.target)) return
    if (e.code === 'Space') e.preventDefault()   // Space = pan modifier only
    keys.add(e.code)
    if (e.code === 'KeyF' || e.code === 'NumpadDecimal') frameSelection()
    // Home = same as the ⌂ "back to universe" button (zoom all the way out)
    if (e.code === 'Home') { e.preventDefault(); frameUniverse() }
    // Escape: clear selection (white) + search highlights (yellow) + path line
    if (e.code === 'Escape') {
      e.preventDefault()
      setOverlay(highlights, [])
      store.api.setPath?.([])
      store.openEntities([])
      store.clearResults?.()
      store.maybeCollapseSidebar?.()
    }
    if (e.code === 'Numpad7') { rig.pitch = -1.55 }
    if (e.code === 'Numpad1') { rig.pitch = 0; rig.yaw = 0 }
    if (e.code === 'Numpad3') { rig.pitch = 0; rig.yaw = Math.PI / 2 }
  }, sig)
  addEventListener('keyup', (e) => keys.delete(e.code), sig)
  // Focusing an editor clears sticky WASD/Space so the camera stops immediately
  addEventListener('focusin', (e) => {
    if (isTextEditable(e.target)) keys.clear()
  }, sig)

  canvas.addEventListener('contextmenu', (e) => e.preventDefault(), sig)
  canvas.addEventListener('mousedown', (e) => {
    moved = false
    // Clicking the world releases text focus so keys go back to the camera
    // and middle-click won't paste into a still-focused contenteditable.
    if (isTypingFocus()) {
      try { document.activeElement?.blur?.() } catch { /* ignore */ }
    }
    if (e.button === 1) {
      mmb = true
      e.preventDefault() // block Linux/X11 middle-click paste into focused fields
    }
  }, sig)

  // Global MMB: paste only when middle-clicking *inside* the focused editor;
  // otherwise prevent paste and (if on canvas) orbit was already started above.
  addEventListener('mousedown', (e) => {
    if (e.button !== 1) return
    const active = document.activeElement
    const onFocusedEditor =
      isTextEditable(active) &&
      (active === e.target || active.contains?.(e.target))
    if (onFocusedEditor) {
      // Let the browser paste into the focused field; no camera orbit
      mmb = false
      return
    }
    // Anywhere else: never paste (esp. into unfocused search/chat)
    e.preventDefault()
  }, { capture: true, signal: ac.signal })
  addEventListener('auxclick', (e) => {
    if (e.button !== 1) return
    const active = document.activeElement
    const onFocusedEditor =
      isTextEditable(active) &&
      (active === e.target || active.contains?.(e.target))
    if (!onFocusedEditor) e.preventDefault()
  }, { capture: true, signal: ac.signal })

  addEventListener('mouseup', (e) => {
    if (e.button === 1) mmb = false
    if (e.button === 0 && !moved && cameraKeysLive() && !keys.has('Space')) {
      pick(e, e.ctrlKey || e.metaKey || e.shiftKey)
    }
  }, sig)
  addEventListener('mousemove', (e) => {
    const dx = e.movementX, dy = e.movementY
    if (Math.abs(dx) + Math.abs(dy) > 2) moved = true
    // Space-pan only when not typing; MMB orbit only when not pasting in an editor
    const spacePan = cameraKeysLive() && keys.has('Space')
    if (spacePan || (mmb && e.shiftKey)) {
      // hold Space: pan follows the mouse delta directly (no button needed)
      const right = new THREE.Vector3().crossVectors(rigDir(), new THREE.Vector3(0, 0, 1)).normalize()
      const up = new THREE.Vector3().crossVectors(right, rigDir()).normalize()
      const s = rig.dist * 0.0016
      rig.target.addScaledVector(right, -dx * s).addScaledVector(up, dy * s)
    } else if (mmb) {
      // MMB turntable orbit — both axes inverted to match drag intuition
      rig.yaw += dx * 0.005
      rig.pitch = Math.max(-1.55, Math.min(1.55, rig.pitch - dy * 0.005))
    }
  }, sig)
  canvas.addEventListener('wheel', (e) => {
    rig.dist = Math.max(5, Math.min(meta.world_radius * 6, rig.dist * (e.deltaY > 0 ? 1.15 : 0.87)))
    e.preventDefault()
  }, { passive: false, signal: ac.signal })

  // ---------- picking (click = select; Ctrl/Cmd/Shift+click = multi-select toggle) ----------
  // Screen-space picking: project every node EXACTLY as the shader renders it
  // (including the 2D/3D zMix flatten) and select the node whose on-screen
  // center is nearest the cursor. Click-what-you-see by construction — no
  // raycast-vs-flattened-render mismatch, no invisible-depth mis-picks.
  // O(N) per click (~tens of ms at 2M nodes) — click-rate cheap.
  const PICK_RADIUS_PX = 14
  function pick(e, additive) {
    camera.updateMatrixWorld()
    const vp = new THREE.Matrix4().multiplyMatrices(camera.projectionMatrix, camera.matrixWorldInverse)
    const m = vp.elements
    const zmix = uni.uZMix.value
    const px = e.clientX, py = e.clientY
    const hw = innerWidth / 2, hh = innerHeight / 2
    let best = -1, bestD2 = PICK_RADIUS_PX * PICK_RADIUS_PX, bestW = Infinity
    for (let i = 0; i < n; i++) {
      const x = pos[i * 3], y = pos[i * 3 + 1], z = pos[i * 3 + 2] * zmix
      const w = m[3] * x + m[7] * y + m[11] * z + m[15]
      if (w <= 0) continue   // behind the camera
      const sx = ((m[0] * x + m[4] * y + m[8] * z + m[12]) / w + 1) * hw
      const sy = (1 - (m[1] * x + m[5] * y + m[9] * z + m[13]) / w) * hh
      const dx = sx - px, dy = sy - py
      const d2 = dx * dx + dy * dy
      // nearest on screen wins; near-ties go to the node closest to camera
      // (the one actually visible on top)
      if (d2 < bestD2 - 1 || (d2 < bestD2 + 1 && w < bestW)) {
        best = i; bestD2 = Math.min(d2, bestD2); bestW = w
      }
    }
    if (best >= 0) clickNode(best, additive)
    // empty-space click = select none (routes to / so back restores the
    // selection); Ctrl+click misses are forgiven mid-multi-select
    else if (!additive) store.openEntities([])
  }
  function syncSelection() {
    setOverlay(selection, st.selectedIdx)
    store.selectedSlugs = st.selectedIdx.map((i) => st.slugCache[i]).filter(Boolean)
    // Lime-highlight entity links that are in the selection set
    store.syncEntityLinkSelection?.()
  }
  // Selection is URL-first: a node click resolves its slug then routes through
  // store.openEntities (ui.js) → Router pushState → applyEntitySelection below.
  // Back/forward, permalinks, entity links, and direct clicks all converge on
  // the same apply path, so every route change gets the same zoom transition.
  async function clickNode(i, additive) {
    let slug = st.slugCache[i]
    if (!slug) {
      const d = await (await fetch('/node?i=' + i)).json()
      slug = st.slugCache[i] = d.slug
    }
    // noZoom: the node is already under the cursor — select in place
    store.openEntities(slug, additive, { noZoom: true })
  }
  async function resolveSlug(slug) {
    for (const [i, s] of Object.entries(st.slugCache)) if (s === slug) return +i
    const r = await (await fetch('/resolve?slug=' + encodeURIComponent(slug))).json()
    if (r.i >= 0) st.slugCache[r.i] = slug
    return r.i
  }
  async function applyEntitySelection(slugs) {
    const zoom = !store.skipZoomOnce   // one-shot flag from canvas clicks
    store.skipZoomOnce = false
    // Entity-link double-click: select without auto-zoom, then frame explicitly
    // once selection indices are resolved (works even if selection was unchanged).
    const frameAfter = !!store.frameAfterSelectOnce
    store.frameAfterSelectOnce = false
    const cur = st.selectedIdx.map((i) => st.slugCache[i])
    const same =
      slugs.length === cur.length && slugs.every((s, j) => s === cur[j])
    if (same) {
      if (frameAfter && st.selectedIdx.length) frameIdxs(st.selectedIdx)
      return
    }
    const idxs = (await Promise.all(slugs.map(resolveSlug))).filter((i) => i >= 0)
    st.selectedIdx = idxs
    syncSelection()
    // zero entities selected (empty-space click, ctrl-toggle-off, route to /):
    // clear the inspector (white selection rings already gone via syncSelection).
    // Yellow rings = search/think hit highlights — keep them while SERPS is open;
    // otherwise clear so they don't linger after a query is dismissed.
    if (!idxs.length) {
      store.detailSlug = ''
      store.loadInspector?.([])
      if (!store.showSerps?.()) {
        setOverlay(highlights, [])
        store.api.setPath?.([])
      }
      store.maybeCollapseSidebar?.()
      return
    }
    if (zoom || frameAfter) frameIdxs(idxs)
    // Multi-select inspector: load all selected entities (Unity mixed fields)
    const selSlugs = idxs.map((i) => st.slugCache[i]).filter(Boolean)
    if (selSlugs.length) {
      await store.loadInspector(selSlugs)
    } else {
      const d = await (await fetch('/node?i=' + idxs[idxs.length - 1])).json()
      store.showDetail(d)
    }
  }
  syncSelection()   // restore overlay + selectedSlugs after HMR

  // ---------- actions for the HUD (ui.js) ----------
  store.api = {
    flyToNode: (i) => flyTo(nodeXYZ(i)),
    applyEntitySelection,
    setHighlights: (idxs) => setOverlay(highlights, idxs),
    setPath: (idxs) => {
      const a = new Float32Array(idxs.length * 3)
      idxs.forEach((idx, j) => { const v = nodeXYZ(idx); a.set([v.x, v.y, v.z], j * 3) })
      pathLine.geometry.setAttribute('position', new THREE.BufferAttribute(a, 3))
    },
    frameUniverse,
    frameSelection,
    frameSlugs, // entity-link dblclick: frame only these slugs (not highlight union)
    toggle3d: () => {
      st.zMixTarget = st.zMixTarget ? 0 : 1
      store.is3d = st.zMixTarget === 1
    },
  }
  // Permalink deep link (/e/slug,…): the route was parsed by ui.js before this
  // scene existed — apply it now. No-op on HMR (selection already matches).
  if (store.routeSlugs?.length) applyEntitySelection(store.routeSlugs)

  // ---------- FPS + m.js flush histograms (gl1-style) ----------
  // #fps       = three.js frame time → fps
  // #mjs-draws = m.js rAF flushes per display frame (Mithril-style ≤1 redraw/frame)
  //              hover title also shows effects (binding re-runs) + full redraws
  const fpsCtx = document.getElementById('fps')?.getContext('2d')
  const mjsCtx = document.getElementById('mjs-draws')?.getContext('2d')
  const samples = new Float32Array(120); let sHead = 0
  // flushes-per-frame samples (expect 0 or 1 when healthy)
  const mjsSamples = new Float32Array(120); let mjsHead = 0
  let lastEffects = 0
  let lastRedraws = 0
  function coldHot(t) {
    t = Math.max(0, Math.min(1, t))
    let r, g, b
    if (t < 0.5) { const s = t * 2; r = 0.20 + 0.75 * s; g = 0.45 + 0.40 * s; b = 0.95 - 0.70 * s }
    else { const s = (t - 0.5) * 2; r = 0.95; g = 0.85 - 0.60 * s; b = 0.25 - 0.05 * s }
    return 'rgb(' + (r * 255 | 0) + ',' + (g * 255 | 0) + ',' + (b * 255 | 0) + ')'
  }
  /** Magenta ramp for flush intensity (full height ≈ 2 flushes/frame = bad). */
  function drawsHot(t) {
    t = Math.max(0, Math.min(1, t))
    const r = 0.45 + 0.55 * t
    const g = 0.20 + 0.15 * (1 - t)
    const b = 0.75 + 0.25 * t
    return 'rgb(' + (r * 255 | 0) + ',' + (g * 255 | 0) + ',' + (b * 255 | 0) + ')'
  }
  function drawFps(dtMs) {
    if (!fpsCtx) return
    samples[sHead] = dtMs; sHead = (sHead + 1) % samples.length
    fpsCtx.clearRect(0, 0, 120, 24)
    for (let i = 0; i < samples.length; i++) {
      const v = samples[(sHead + i) % samples.length]
      if (v <= 0.01) continue
      const t = Math.min(1, v / 33.3)
      const h = Math.max(1, 24 * t)
      fpsCtx.fillStyle = coldHot(t)
      fpsCtx.fillRect(i, 24 - h, 1, h)
    }
    let sum = 0, cnt = 0
    for (const v of samples) if (v > 0.01) { sum += v; cnt++ }
    if (cnt) {
      fpsCtx.fillStyle = '#fff'; fpsCtx.font = '11px ui-sans-serif'; fpsCtx.textAlign = 'center'; fpsCtx.textBaseline = 'middle'
      fpsCtx.shadowColor = 'rgba(0,0,0,.8)'; fpsCtx.shadowBlur = 3
      fpsCtx.fillText(Math.round(1000 / (sum / cnt)) + 'fps', 60, 12)
      fpsCtx.shadowBlur = 0
    }
  }
  /**
   * Sample m.js rAF flushes since last display frame (expect ≤1).
   * Y scale: full height = 2 flushes/frame (more than one pending slot = thrash).
   */
  function drawMjsDraws() {
    if (!mjsCtx) return
    let flushes = 0
    let effects = 0
    let redraws = 0
    try {
      const take =
        (typeof M?.takePerfStats === 'function' && M.takePerfStats.bind(M)) ||
        (typeof window !== 'undefined' &&
          typeof window.M?.takePerfStats === 'function' &&
          window.M.takePerfStats.bind(window.M)) ||
        null
      if (take) {
        const s = take()
        flushes = s.flushes || 0
        effects = s.effects || 0
        redraws = s.redraws || 0
      } else if (typeof M?.takeDrawCalls === 'function') {
        flushes = M.takeDrawCalls()
      } else if (typeof window !== 'undefined' && typeof window.M?.takeDrawCalls === 'function') {
        flushes = window.M.takeDrawCalls()
      }
    } catch {
      flushes = 0
    }
    lastEffects = effects
    lastRedraws = redraws
    mjsSamples[mjsHead] = flushes
    mjsHead = (mjsHead + 1) % mjsSamples.length
    mjsCtx.clearRect(0, 0, 120, 24)
    // Healthy: 0–1 flush/frame. Full bar height = 2 (double-schedule thrash).
    const SCALE = 2
    for (let i = 0; i < mjsSamples.length; i++) {
      const v = mjsSamples[(mjsHead + i) % mjsSamples.length]
      if (v <= 0) continue
      const t = Math.min(1, v / SCALE)
      const h = Math.max(1, 24 * t)
      mjsCtx.fillStyle = drawsHot(t)
      mjsCtx.fillRect(i, 24 - h, 1, h)
    }
    let sum = 0, cnt = 0, peak = 0
    for (const v of mjsSamples) {
      if (v > 0) { sum += v; cnt++ }
      if (v > peak) peak = v
    }
    const avg = cnt ? sum / cnt : 0
    mjsCtx.fillStyle = '#fff'
    mjsCtx.font = '11px ui-sans-serif'
    mjsCtx.textAlign = 'center'
    mjsCtx.textBaseline = 'middle'
    mjsCtx.shadowColor = 'rgba(0,0,0,.8)'
    mjsCtx.shadowBlur = 3
    // "1f" = rAF flushes this sample window average (whole number)
    const label = Math.round(avg) + 'f'
    mjsCtx.fillText(label, 60, 12)
    mjsCtx.shadowBlur = 0
    try {
      const el = document.getElementById('mjs-draws')
      if (el) {
        el.title =
          `m.js flushes/frame (last=${flushes}, avg=${avg.toFixed(2)}, peak=${peak}) · ` +
          `effects=${effects} redraws=${redraws} · ` +
          'flushes = rAF paint slots (Mithril-style ≤1/frame); effects = binding re-runs inside'
      }
    } catch { /* ignore */ }
  }

  // ---------- main loop ----------
  // Fly inertia (Quake1-style SV_Friction + SV_Accelerate on the orbit target).
  // Wishdir from WASD/QE, max speed scales with rig.dist so feel stays consistent
  // when zoomed; friction gives coast-out, accelerate gives spin-up from rest.
  const flyVel = new THREE.Vector3()
  const FLY_FRICTION = 6     // sv_friction-ish (Quake ground ~4–6)
  const FLY_ACCEL = 10       // sv_accelerate (Quake default 10)
  const FLY_STOPSPEED = 0.15 // fraction of maxSpeed used as Quake "stopspeed" floor
  const FLY_MAX = 0.9        // peak speed = FLY_MAX * rig.dist (matches old instant speed)
  const FLY_EPS = 1e-5

  function resize() {
    renderer.setSize(innerWidth, innerHeight, false)
    camera.aspect = innerWidth / innerHeight
    camera.updateProjectionMatrix()
  }
  addEventListener('resize', resize, sig)
  resize()
  if (!rig.dist) rig.dist = fitDist(meta.world_radius)   // first boot: frame the universe
  let raf = 0
  let last = performance.now()
  function frame(now) {
    const dt = Math.min(0.1, (now - last) / 1000); last = now
    // Scripted fly-to: kill residual fly velocity so inertia doesn't fight the tween
    if (anim.active) {
      flyVel.set(0, 0, 0)
    } else if (!keys.has('ControlLeft') && !keys.has('ControlRight')) {
      const fwd = rigDir()
      const upW = new THREE.Vector3(0, 0, 1)
      let right = new THREE.Vector3().crossVectors(fwd, upW)
      if (right.lengthSq() < 1e-8) right.set(1, 0, 0)
      else right.normalize()

      // wish direction from held keys (camera-relative, Quake "wishdir").
      // While a text field / contenteditable has focus, ignore movement keys
      // so typing WASD/Space never flies the camera.
      const wish = new THREE.Vector3()
      if (cameraKeysLive()) {
        if (keys.has('KeyW')) wish.add(fwd)
        if (keys.has('KeyS')) wish.sub(fwd)
        if (keys.has('KeyD')) wish.add(right)
        if (keys.has('KeyA')) wish.sub(right)
        if (keys.has('KeyE')) wish.add(upW)
        if (keys.has('KeyQ')) wish.sub(upW)
      }

      const maxSpeed = Math.max(1, rig.dist * FLY_MAX)

      // --- SV_Friction: always, so release keys → smooth coast to stop ---
      const speed = flyVel.length()
      if (speed > 0) {
        // Quake: control = max(speed, stopspeed); drop = control * friction * dt
        const stopspeed = maxSpeed * FLY_STOPSPEED
        const control = speed < stopspeed ? stopspeed : speed
        const drop = control * FLY_FRICTION * dt
        const newspeed = speed - drop
        if (newspeed <= 0) flyVel.set(0, 0, 0)
        else flyVel.multiplyScalar(newspeed / speed)
      }

      // --- SV_Accelerate: push velocity toward wishdir * maxSpeed ---
      if (wish.lengthSq() > 0) {
        wish.normalize()
        const currentspeed = flyVel.dot(wish)
        const addspeed = maxSpeed - currentspeed
        if (addspeed > 0) {
          // Quake: accelspeed = accel * frametime * wishspeed
          let accelspeed = FLY_ACCEL * dt * maxSpeed
          if (accelspeed > addspeed) accelspeed = addspeed
          flyVel.addScaledVector(wish, accelspeed)
        }
      }

      if (flyVel.lengthSq() > FLY_EPS) rig.target.addScaledVector(flyVel, dt)
      else flyVel.set(0, 0, 0)
    }
    if (anim.active) {
      anim.t = Math.min(1, anim.t + dt / 0.6)
      const e = anim.t < 0.5 ? 2 * anim.t * anim.t : 1 - Math.pow(-2 * anim.t + 2, 2) / 2
      rig.target.lerpVectors(anim.from.target, anim.to.target, e)
      rig.dist = anim.from.dist + (anim.to.dist - anim.from.dist) * e
      if (anim.t >= 1) anim.active = false
    }
    uni.uZMix.value += (st.zMixTarget - uni.uZMix.value) * Math.min(1, dt * 5)
    st.zMix = uni.uZMix.value
    applyRig()
    renderer.render(scene, camera)
    drawFps(dt * 1000)
    drawMjsDraws() // sample m.js refresh count for this display frame
    raf = requestAnimationFrame(frame)
  }
  raf = requestAnimationFrame(frame)

  window.__VIZ_SCENE__ = {
    dispose() {
      cancelAnimationFrame(raf)
      ac.abort()
      geo.dispose(); mat.dispose()
      highlights.geometry.dispose(); highlights.material.dispose()
      selection.geometry.dispose(); selection.material.dispose()
      pathLine.geometry.dispose(); pathLine.material.dispose()
      renderer.dispose()
    },
  }
}
