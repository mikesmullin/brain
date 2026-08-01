// scene.js — the three.js world: 2M-node point cloud, gl1-style camera rig,
// picking, overlays, FPS histogram. HMR-aware:
//   - node data (40MB binary) is fetched once and cached on window.__VIZ_DATA__
//   - the camera rig + view state persist on window.__VIZ_STATE__
//   - each boot disposes the previous scene (listeners, RAF, GL resources)
// so editing this file hot-swaps behavior without losing your place in space.
import * as THREE from 'three'
import M from '/vendor/m-js/src/index.js'

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
  // Additive glow restored (screen-space picking below guarantees that
  // whatever you SEE under the cursor is what a click selects — clickability
  // no longer depends on depth occlusion). Falloff kept tight so points read
  // as discs with a soft rim, not wide halo rings.
  const mat = new THREE.ShaderMaterial({
    uniforms: uni,
    transparent: true, depthWrite: false, blending: THREE.AdditiveBlending,
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
      '  float a = smoothstep(0.25, 0.16, r2) * 0.9;',
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
  function frameSelection() {
    const idxs = st.highlightIdx.length ? st.highlightIdx : st.selectedIdx
    if (!idxs.length) return
    const c = new THREE.Vector3()
    for (const i of idxs) c.add(nodeXYZ(i))
    c.divideScalar(idxs.length)
    let r = 20
    for (const i of idxs) r = Math.max(r, nodeXYZ(i).distanceTo(c))
    flyTo(c, fitDist(r * 1.15))
  }
  function frameUniverse() { flyTo(new THREE.Vector3(0, 0, 0), fitDist(meta.world_radius)) }

  // ---------- input (all listeners on an AbortController for clean HMR) ----------
  const ac = new AbortController()
  const sig = { signal: ac.signal }
  const keys = new Set()
  let mmb = false, moved = false
  addEventListener('keydown', (e) => {
    if (e.target.tagName === 'INPUT' || e.target.tagName === 'SELECT') return
    if (e.code === 'Space') e.preventDefault()   // Space = pan modifier only
    keys.add(e.code)
    if (e.code === 'KeyF' || e.code === 'NumpadDecimal') frameSelection()
    if (e.code === 'Numpad7') { rig.pitch = -1.55 }
    if (e.code === 'Numpad1') { rig.pitch = 0; rig.yaw = 0 }
    if (e.code === 'Numpad3') { rig.pitch = 0; rig.yaw = Math.PI / 2 }
  }, sig)
  addEventListener('keyup', (e) => keys.delete(e.code), sig)
  canvas.addEventListener('contextmenu', (e) => e.preventDefault(), sig)
  canvas.addEventListener('mousedown', (e) => {
    moved = false
    if (e.button === 1) { mmb = true; e.preventDefault() }
  }, sig)
  addEventListener('mouseup', (e) => {
    if (e.button === 1) mmb = false
    if (e.button === 0 && !moved && !keys.has('Space')) pick(e, e.ctrlKey)
  }, sig)
  addEventListener('mousemove', (e) => {
    const dx = e.movementX, dy = e.movementY
    if (Math.abs(dx) + Math.abs(dy) > 2) moved = true
    if (keys.has('Space') || (mmb && e.shiftKey)) {
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

  // ---------- picking (click = select; Ctrl+click = multi-select toggle) ----------
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
    if (best >= 0) selectNode(best, additive)
  }
  function syncSelection() {
    setOverlay(selection, st.selectedIdx)
    store.selectedSlugs = st.selectedIdx.map((i) => st.slugCache[i]).filter(Boolean)
  }
  async function selectNode(i, additive = false) {
    if (additive) {
      const at = st.selectedIdx.indexOf(i)
      if (at >= 0) st.selectedIdx.splice(at, 1)
      else st.selectedIdx.push(i)
    } else {
      st.selectedIdx = [i]
    }
    const d = await (await fetch('/node?i=' + i)).json()
    st.slugCache[i] = d.slug
    syncSelection()
    if (st.selectedIdx.includes(i)) store.showDetail(d)
  }
  syncSelection()   // restore overlay + selectedSlugs after HMR

  // ---------- actions for the HUD (ui.js) ----------
  store.api = {
    flyToNode: (i) => flyTo(nodeXYZ(i)),
    selectNode: (i) => selectNode(i, false),
    setHighlights: (idxs) => setOverlay(highlights, idxs),
    setPath: (idxs) => {
      const a = new Float32Array(idxs.length * 3)
      idxs.forEach((idx, j) => { const v = nodeXYZ(idx); a.set([v.x, v.y, v.z], j * 3) })
      pathLine.geometry.setAttribute('position', new THREE.BufferAttribute(a, 3))
    },
    frameUniverse,
    frameSelection,
    toggle3d: () => {
      st.zMixTarget = st.zMixTarget ? 0 : 1
      store.is3d = st.zMixTarget === 1
    },
  }

  // ---------- FPS histogram (gl1-style) ----------
  const fpsCtx = document.getElementById('fps').getContext('2d')
  const samples = new Float32Array(120); let sHead = 0
  function coldHot(t) {
    t = Math.max(0, Math.min(1, t))
    let r, g, b
    if (t < 0.5) { const s = t * 2; r = 0.20 + 0.75 * s; g = 0.45 + 0.40 * s; b = 0.95 - 0.70 * s }
    else { const s = (t - 0.5) * 2; r = 0.95; g = 0.85 - 0.60 * s; b = 0.25 - 0.05 * s }
    return 'rgb(' + (r * 255 | 0) + ',' + (g * 255 | 0) + ',' + (b * 255 | 0) + ')'
  }
  function drawFps(dtMs) {
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

  // ---------- main loop ----------
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
    if (!keys.has('ControlLeft') && !keys.has('ControlRight')) {
      const fwd = rigDir(), upW = new THREE.Vector3(0, 0, 1)
      const right = new THREE.Vector3().crossVectors(fwd, upW).normalize()
      const v = new THREE.Vector3()
      if (keys.has('KeyW')) v.add(fwd)
      if (keys.has('KeyS')) v.sub(fwd)
      if (keys.has('KeyD')) v.add(right)
      if (keys.has('KeyA')) v.sub(right)
      if (keys.has('KeyE')) v.add(upW)
      if (keys.has('KeyQ')) v.sub(upW)
      if (v.lengthSq()) rig.target.addScaledVector(v.normalize(), dt * rig.dist * 0.9)
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
