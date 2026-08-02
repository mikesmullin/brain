// ui.js — the flat HUD layer, as an m.js component tree over a named store.
// The store survives HMR (window.__M_STORES__): query text, results, sidebar
// state, and chip settings all persist across hot edits of this file.
// scene.js installs its camera/highlight actions onto store.api after boot.
//
// Entity selection is a first-class SPA route: /e/slug1,slug2 (comma-separated
// multi-select). Every way of selecting an entity — clicking a node in the
// world, an entity link in the sidebar, pasting a permalink, browser
// back/forward — funnels through store.openEntities() → Router pushState →
// store.api.applyEntitySelection() (scene.js), which plays the zoom
// transition. To make something an entity link ANYWHERE in the HUD, render
// <a data-entity="slug[,slug]" href="…entityHref()…"> — one delegated
// document-level handler (below) adapts all of them onto the same path.
import M, { Router } from '/vendor/m-js/src/index.js'
import { marked } from 'marked'

const PLACEHOLDER = {
  search: 'search your brain…',
  think: 'ask a question — search + LLM synthesis…',
  ontology: 'a multi-hop relational question…',
  graph: 'pattern, e.g. Team -->|SUPPORTS| Product  or  A *6> B --shortest',
  graphql: 'Class/id { field, REL { … } }',
}

// Class/id slug shape used in brain (ProperCase class + id).
const SLUG_RE = /^[A-Za-z][\w]*\/[\w.-]+$/

// marked v15 renderer: Class/id and /e/… links become in-app entity links
// (data-entity); everything else opens in a new tab.
const mdRenderer = {
  link({ href, title, tokens }) {
    const text = this.parser.parseInline(tokens)
    const h = href || ''
    let slug = null
    if (SLUG_RE.test(h)) slug = h
    else {
      const m = h.match(/^\/e\/(.+)$/)
      if (m) slug = decodeURIComponent(m[1].split(',')[0])
    }
    if (slug) {
      const safe = slug.replace(/"/g, '&quot;')
      const path = Router.href('/e/' + encodeURIComponent(slug))
      return `<a class="md-entity entity-link" data-entity="${safe}" href="${path}">${text}</a>`
    }
    const t = title ? ` title="${String(title).replace(/"/g, '&quot;')}"` : ''
    return `<a href="${h.replace(/"/g, '&quot;')}"${t} target="_blank" rel="noopener noreferrer">${text}</a>`
  },
}

marked.use({
  gfm: true,
  breaks: true,   // single newlines → <br> (chat-friendly)
  renderer: mdRenderer,
})

// Parse GFM → HTML. Also promote bare `Class/id` code spans to entity links.
function renderMarkdown(src) {
  if (!src) return ''
  let html = marked.parse(String(src), { async: false })
  // `Officer/123` → clickable entity chip
  html = html.replace(/<code>([A-Za-z][\w]*\/[\w.-]+)<\/code>/g, (_, slug) => {
    const path = Router.href('/e/' + encodeURIComponent(slug))
    return `<a class="md-entity entity-link" data-entity="${slug}" href="${path}"><code>${slug}</code></a>`
  })
  return html
}

// Plain speech text for Ada / TTS: strip markdown (and any residual HTML) so
// the voice server never reads `**bold**`, list markers, link URLs, etc.
function stripMarkdownForSpeech(src) {
  if (!src) return ''
  let html
  try {
    html = marked.parse(String(src), { async: false })
  } catch {
    html = String(src)
  }
  // Prefer DOM textContent when available (browser); fall back to tag strip.
  let text
  if (typeof document !== 'undefined') {
    const d = document.createElement('div')
    d.innerHTML = html
    text = d.textContent || d.innerText || ''
  } else {
    text = String(html).replace(/<[^>]+>/g, ' ')
  }
  return text
    .replace(/\u00a0/g, ' ')
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .replace(/[ \t]{2,}/g, ' ')
    .trim()
}

const TEMPLATE = `
<div>
  <div id="searchbar" class="panel">
    <select :value="$store.viz.mode" @change="$store.viz.setMode($event.target.value)">
      <option value="search">search</option>
      <option value="think">think</option>
      <option value="ontology">ontology</option>
      <option value="graph">graph</option>
      <option value="graphql">graphql</option>
    </select>
    <span id="searchopts" x-show="$store.viz.mode === 'search'">
      <span :class="'chip ' + ($store.viz.strategy === 'hybrid' ? 'on' : '')" @click="$store.viz.setStrategy('hybrid')">hybrid</span>
      <span :class="'chip ' + ($store.viz.strategy === 'keyword' ? 'on' : '')" @click="$store.viz.setStrategy('keyword')">keyword</span>
      <span :class="'chip ' + ($store.viz.strategy === 'vector' ? 'on' : '')" @click="$store.viz.setStrategy('vector')">vector</span>
      <span :class="'chip ' + ($store.viz.expand ? 'on' : '')" @click="$store.viz.toggleExpand()" title="1-hop relational expansion">expand</span>
    </span>
    <span id="llmopts" x-show="$store.viz.mode === 'think' || $store.viz.mode === 'ontology'">
      <select :value="$store.viz.model" @change="$store.viz.setModel($event.target.value)">
        <template x-for="mm in $store.viz.models">
          <option :value="mm" x-text="mm || (($store.viz.aglDefault || 'agl') + ' (default)')"></option>
        </template>
      </select>
      <button id="think-toggle" x-show="$store.viz.canThink()" :class="{ on: $store.viz.thinking }"
              :title="$store.viz.thinking ? 'Disable thinking' : 'Enable thinking'"
              @click="$store.viz.toggleThinking()"><i class="ph-bold ph-brain"></i></button>
      <button id="sel-toggle" :class="{ on: $store.viz.useSelection }"
              :title="($store.viz.useSelection ? 'Exclude selection from context' : 'Include selection as context') + ' (' + $store.viz.selectedSlugs.length + ' selected)'"
              @click="$store.viz.toggleUseSelection()"><i class="ph-bold ph-selection-plus"></i></button>
    </span>
    <span class="qwrap">
      <input id="q" spellcheck="false" autocomplete="off" x-model="$store.viz.q"
             :placeholder="$store.viz.placeholder()"
             @keydown="if ($event.key === 'Enter' && !$event.repeat) { $event.preventDefault(); $store.viz.run() }">
      <i class="ph ph-x qclear" x-show="$store.viz.q" title="clear" @click="$store.viz.clearQ()"></i>
    </span>
    <button id="go" x-show="!$store.viz.busy" title="run query" @click="$store.viz.run()">⏎</button>
    <button id="stop" x-show="$store.viz.busy" :class="$store.viz.stopping ? 'stopping' : ''"
            :title="$store.viz.stopping ? 'cancelling…' : 'cancel this query'"
            @click="$store.viz.stop()"><i class="ph-fill ph-stop"></i></button>
  </div>

  <div id="status" x-text="$store.viz.statusText"></div>
  <div id="home" :class="'edgebtn panel ' + ($store.viz.collapsed ? 'collapsed' : '')"
       title="back to universe (frame everything)" @click="$store.viz.api.frameUniverse()">⌂</div>
  <div id="collapse" :class="'edgebtn panel ' + ($store.viz.collapsed ? 'collapsed' : '')"
       @click="$store.viz.collapsed = !$store.viz.collapsed"
       x-text="$store.viz.collapsed ? '‹' : '›'"></div>

  <div id="sidebar" :class="'panel ' + ($store.viz.collapsed ? 'collapsed' : '')">
    <header>
      <span class="hdr-title"><i class="spin" x-show="$store.viz.busy"></i><span x-text="$store.viz.title"></span></span>
      <span class="hdr-right">
        <span id="qms" x-text="$store.viz.ms"></span>
        <button id="voice-toggle" x-show="$store.viz.mode === 'think' || $store.viz.mode === 'ontology'"
                :class="{ on: $store.viz.speak }"
                :title="$store.viz.speak ? 'Disable voice output' : 'Enable voice output (Ada speaks the answer)'"
                @click="$store.viz.toggleSpeak()"><i class="ph ph-speaker-high"></i></button>
      </span>
    </header>
    <div id="results">
      <div class="answer warn" x-show="$store.viz.speakWarning">🔇 please install
        <a href="https://github.com/mikesmullin/ada" target="_blank" rel="noopener">ada</a>
        to hear these results spoken aloud</div>
      <div class="answer error" x-show="$store.viz.error" x-text="$store.viz.error"></div>
      <div class="answer md" x-show="$store.viz.answer" x-html="$store.viz.answerHtml()"></div>
      <pre x-show="$store.viz.json" x-text="$store.viz.json"></pre>
      <template x-for="r in $store.viz.rows">
        <a :class="'hit' + (r.kind === 'node' ? ' entity-link' : '')"
           :href="r.kind === 'node' ? $store.viz.entityHref(r.slug) : null"
           :data-entity="r.kind === 'node' ? r.slug : null"
           @click="r.kind !== 'node' && $store.viz.openRow(r)">
          <div class="slug" x-text="r.title"></div>
          <div class="meta" x-show="r.sub" x-text="r.sub"></div>
        </a>
      </template>
    </div>
    <div id="detail" x-show="$store.viz.detailSlug">
      <b x-text="$store.viz.detailSlug"></b>
      <pre x-text="$store.viz.detailJson"></pre>
    </div>
  </div>

  <div id="mode3d" class="panel" @click="$store.viz.api.toggle3d()">
    <i :class="'ph ' + ($store.viz.is3d ? 'ph-square' : 'ph-cube')"></i>
    <span x-text="$store.viz.is3d ? '2D' : '3D'"></span>
  </div>
  <div id="hint" class="panel">WASD fly · E/Q up/down · MMB orbit · hold Space: pan · wheel dolly · click select · Ctrl+click multi-select · F frame · Home zoom out</div>
</div>
`

// Explicit UI persistence across page loads (the browser's own form-value
// restoration is disabled via :value bindings + autocomplete=off — it used to
// repopulate the DOM without updating the store, desyncing the mode chips).
const STORAGE_KEY = 'brain-viz-ui'
function loadPersisted() {
  try { return JSON.parse(localStorage.getItem(STORAGE_KEY)) || {} } catch { return {} }
}

// In-flight query handle (window-stashed so an HMR reboot mid-query can still
// cancel it). Kept OUT of the reactive store on purpose — AbortController must
// not be proxied.
//   ac      — aborts the browser→viz fetch
//   qid     — server-side cancel key (LLM modes)
//   timer   — live elapsed-ms interval
//   done    — promise settled when the current run()/adopt finishes
//   gen     — monotonic id so a superseded run never clobbers the next one
const RUN = (window.__VIZ_RUN__ ??= { ac: null, qid: null, timer: null, done: null, settle: null, gen: 0 })

function fmtElapsed(t0) {
  return ((performance.now() - t0) / 1000).toFixed(3) + 's'
}

function startTimer(store, t0) {
  if (RUN.timer != null) { clearInterval(RUN.timer); RUN.timer = null }
  const tick = () => { store.ms = fmtElapsed(t0) }
  tick()
  RUN.timer = setInterval(tick, 32)
  return () => {
    if (RUN.timer != null) { clearInterval(RUN.timer); RUN.timer = null }
    store.ms = fmtElapsed(t0)
  }
}

export async function boot() {
  const cfg = await (await fetch('/config.json')).json()
  const saved = loadPersisted()
  const qByMode = Object.assign({ search: '', think: '', ontology: '', graph: '', graphql: '' }, saved.qByMode || {})
  // first visit (nothing persisted): default to ontology — the most capable
  // query mode, so a newcomer's first question gets the best possible answer
  const mode = PLACEHOLDER[saved.mode] ? saved.mode : 'ontology'

  const store = M.store('viz', {
    // state (survives HMR; mode/strategy/model/thinking/queries survive page loads)
    mode, q: qByMode[mode] || '', qByMode,
    strategy: saved.strategy || 'hybrid', expand: saved.expand !== false,
    models: cfg.models,
    model: cfg.models.includes(saved.model) ? saved.model : cfg.default,
    aglDefault: cfg.agl_default || '',   // what the '' model spec resolves to
    thinking: !!saved.thinking,   // clamped below if the model can't think
    useSelection: !!saved.useSelection, selectedSlugs: [],
    speak: !!saved.speak,
    collapsed: true, title: 'results', ms: '', error: '', answer: '', json: '', speakWarning: false,
    rows: [], detailSlug: '', detailJson: '', statusText: '', is3d: true, busy: false, stopping: false,
    routeSlugs: [],   // entity slugs parsed from the current /e/… route
    skipZoomOnce: false,   // one-shot: next route apply selects without flying
    api: {},   // installed by scene.js: flyToNode, applyEntitySelection, setHighlights, setPath, frameUniverse, frameSelection, toggle3d

    placeholder() { return PLACEHOLDER[this.mode] },

    // Thinking is only offered for models matching *:gemma* — the only ones
    // that understand the <|think|> prefix token (see thinkPrefix, think.coffee).
    // The '' spec is judged by what it resolves to (aglDefault).
    canThink() { return /:.*gemma/i.test(this.model || this.aglDefault) },

    // GFM markdown → HTML for the sidebar answer pane (think / ontology).
    answerHtml() { return renderMarkdown(this.answer) },

    persist() {
      try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify({
          mode: this.mode, strategy: this.strategy, expand: this.expand,
          model: this.model, thinking: this.thinking, useSelection: this.useSelection,
          speak: this.speak, qByMode: this.qByMode,
        }))
      } catch { /* storage unavailable — persistence is best-effort */ }
    },

    // per-mode query memory: stash the current text, restore the new mode's
    setMode(m2) {
      this.qByMode[this.mode] = this.q
      this.mode = m2
      this.q = this.qByMode[m2] || ''
      this.persist()
    },
    setStrategy(s) { this.strategy = s; this.persist() },
    toggleExpand() { this.expand = !this.expand; this.persist() },
    setModel(m2) {
      this.model = m2
      if (!this.canThink()) this.thinking = false   // toggle is hidden now — don't leave it latched on
      this.persist()
    },
    toggleThinking() { this.thinking = !this.thinking; this.persist() },
    toggleUseSelection() { this.useSelection = !this.useSelection; this.persist() },
    toggleSpeak() { this.speak = !this.speak; this.persist() },
    // Note: toggles persist to localStorage — a refresh restores last state.
    // Object :class="{ on: … }" (not ternary strings) so m.js classList.toggle
    // actually removes `on` when false (string binder left it stuck).
    clearQ() { this.q = ''; this.qByMode[this.mode] = ''; this.persist() },

    // ---- entity links (the one place selection-by-reference is implemented) ----
    // Permalink for one or more slugs, e.g. entityHref(['a','b']) → /e/a,b
    entityHref(slugs) {
      const list = Array.isArray(slugs) ? slugs : [slugs]
      return Router.href('/e/' + list.map(encodeURIComponent).join(','))
    },
    // Navigate to an entity selection. additive=true toggles the slugs in/out
    // of the current selection (Ctrl+click semantics). Pushes a history entry;
    // the route-change handler applies it to the scene (with zoom transition —
    // suppressed for one navigation by noZoom, used by direct canvas clicks:
    // the node is already under your cursor, yanking the camera is jarring.
    // Entity links / permalinks / back-forward still fly to the selection).
    openEntities(slugs, additive = false, { noZoom = false } = {}) {
      let list = Array.isArray(slugs) ? slugs.slice() : [slugs]
      if (additive) {
        const cur = this.selectedSlugs.slice()
        for (const s of list) {
          const at = cur.indexOf(s)
          if (at >= 0) cur.splice(at, 1); else cur.push(s)
        }
        list = cur
      }
      this.skipZoomOnce = noZoom
      Router.set(list.length ? '/e/' + list.map(encodeURIComponent).join(',') : '/')
    },

    openRow(r) {   // path rows only — node rows render as entity links
      const idxs = r.pathIdx.filter((i) => i >= 0)
      this.api.setHighlights(idxs)
      this.api.setPath(idxs)
      if (idxs.length) this.api.frameSelection()
    },

    showDetail(d) {
      this.detailSlug = d.slug
      this.detailJson = JSON.stringify(
        { components: d.components, relations: d.relations, incoming: (d.incoming || []).slice(0, 30) }, null, 1)
      this.collapsed = false
    },

    // Cancel the in-flight query and wait until it has fully settled.
    // Always fires BOTH: server-side cancel (frees LM Studio) AND local fetch
    // abort (unblocks the UI). Re-entrant: while stopping, re-sends cancel.
    // Resolves when busy flips false (or immediately if nothing is running).
    async stop() {
      if (!this.busy) return
      this.stopping = true
      const qid = RUN.qid
      // Always hit cancel for LLM modes; empty qid cancels ALL server inflight
      // (covers refresh recovery when we adopted a query but lost the original fetch).
      if (qid || this._adopted) {
        try {
          await fetch('/cancel?qid=' + encodeURIComponent(qid || ''))
        } catch { /* network blip — still abort local */ }
      }
      try { RUN.ac?.abort() } catch { /* ignore */ }
      if (RUN.done) {
        try { await RUN.done } catch { /* ignore */ }
      }
    },

    // One query at a time. Submitting while busy = stop the old one first
    // (Enter has the same effect as the stop button, then starts the new query).
    // `_launching` collapses key-repeat / double-Enter storms into a single launch.
    async run() {
      const q = this.q.trim()
      if (!q) return
      if (this._launching) return
      this._launching = true
      let gen = 0
      let settle = null
      let freezeTimer = () => {}
      let mode = this.mode
      let signal = null
      let qid = null
      try {
        if (this.busy) await this.stop()
        if (this.busy) return   // stop failed / still draining — don't stack

        mode = this.mode
        this.qByMode[mode] = this.q
        this.persist()
        gen = ++RUN.gen
        this.busy = true
        this.stopping = false
        this._adopted = false
        RUN.ac = new AbortController()
        // Capture qid in a local — never re-read RUN.qid for the request URL
        // (concurrent stop/adopt must not change the id mid-flight).
        qid = mode === 'think' || mode === 'ontology'
          ? Date.now().toString(36) + Math.random().toString(36).slice(2) : null
        RUN.qid = qid
        RUN.done = new Promise((r) => { settle = r })
        RUN.settle = settle
        signal = RUN.ac.signal
        this.title = mode === 'think' || mode === 'ontology' ? 'thinking…' : 'searching…'
        this.error = ''; this.answer = ''; this.json = ''; this.rows = []; this.speakWarning = false
        this.collapsed = false
        const t0 = performance.now()
        freezeTimer = startTimer(this, t0)
      } finally {
        this._launching = false
      }

      const finish = (label) => { freezeTimer(); this.title = label }
      try {
        if (mode === 'search') {
          const hits = await (await fetch('/search?q=' + encodeURIComponent(q) + '&strategy=' + this.strategy + '&expand=' + this.expand, { signal })).json()
          if (gen !== RUN.gen) return
          if (hits.error) throw new Error(hits.error)
          finish('results (' + hits.length + ')')
          this.api.setHighlights(hits.filter((h) => h.i >= 0).map((h) => h.i))
          this.rows = hits.map((h) => ({
            kind: 'node', i: h.i, slug: h.slug, title: h.slug,
            sub: 'score ' + h.score + (h.preview ? ' · ' + JSON.stringify(h.preview).slice(0, 60) : ''),
          }))
        } else if (mode === 'think' || mode === 'ontology') {
          // POST JSON (not GET): avoids browser auto-retry of "idempotent" GETs
          // after a dropped connection, which re-fired the same qid.
          // sel is sent whenever the toggle is on — even empty.
          const selSlugs = this.routeSlugs.length ? this.routeSlugs : this.selectedSlugs
          const body = {
            q,
            qid,
            model: this.model,
            think: this.thinking,
          }
          if (this.useSelection) body.sel = selSlugs.join(',')
          const res = await (await fetch('/' + mode, {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(body),
            signal,
          })).json()
          if (gen !== RUN.gen) return
          if (res.error) throw new Error(res.error)
          finish(mode)
          this.answer = res.answer || '(no answer)'
          if (this.speak && res.answer) {
            // Speak plain text only — scrub markdown so TTS doesn't read ** # []()
            const spoken = stripMarkdownForSpeech(res.answer)
            if (spoken) {
              fetch('/speak?text=' + encodeURIComponent(spoken))
                .then((r) => r.json())
                .then((s) => { if (s.error) this.speakWarning = true })
                .catch(() => { this.speakWarning = true })
            }
          }
          const nodes = (res.citation_nodes || res.entity_nodes || []).filter((x) => x.slug)
          this.api.setHighlights(nodes.filter((x) => x.i >= 0).map((x) => x.i))
          this.rows = nodes.map((x) => ({ kind: 'node', i: x.i, slug: x.slug, title: x.slug, sub: '' }))
        } else if (mode === 'graph') {
          const res = await (await fetch('/graph?pattern=' + encodeURIComponent(q), { signal })).json()
          if (gen !== RUN.gen) return
          if (res.error) throw new Error(res.error)
          finish('matches (' + res.count + (res.capped ? ', capped' : '') + ')')
          this.rows = res.matches.slice(0, 200).map((m2) => ({
            kind: 'path', pathIdx: m2.pathIdx, title: m2.path.join(' → '), sub: m2.via.join(' · '),
          }))
        } else {   // graphql
          const res = await (await fetch('/graphql?q=' + encodeURIComponent(q), { signal })).json()
          if (gen !== RUN.gen) return
          if (res.error) throw new Error(res.error)
          finish('graphql')
          this.json = JSON.stringify(res, null, 1)
          const r = await (await fetch('/resolve?slug=' + encodeURIComponent(res.slug || ''))).json()
          if (r.i >= 0) { this.api.setHighlights([r.i]); this.api.flyToNode(r.i) }
        }
      } catch (err) {
        if (gen !== RUN.gen) return
        if (err.name === 'AbortError' || /cancel/i.test(err.message || '')) {
          finish('cancelled')
        } else {
          finish('error')
          this.error = err.message
        }
      } finally {
        if (gen === RUN.gen) {
          if (RUN.timer != null) { clearInterval(RUN.timer); RUN.timer = null }
          this.busy = false
          this.stopping = false
          this._adopted = false
          RUN.ac = null
          RUN.qid = null
        }
        settle?.()
        if (RUN.settle === settle) { RUN.done = null; RUN.settle = null }
      }
    },

    // After a browser refresh: if the brain server still has an LLM query
    // running, re-adopt it — show the pulsing stop button, restore qid for
    // cancel, and poll until it finishes. New submits are blocked until then
    // (single-flight).
    async adoptInflight() {
      let data
      try { data = await (await fetch('/inflight')).json() } catch { return }
      const items = (data && data.inflight) || []
      if (!items.length || this.busy) return
      const cur = items[0]
      const gen = ++RUN.gen
      this.busy = true
      this.stopping = !!cur.cancelled
      this._adopted = true
      RUN.qid = cur.qid
      RUN.ac = null
      let settle
      RUN.done = new Promise((r) => { settle = r })
      RUN.settle = settle
      if (cur.method === 'think' || cur.method === 'ontology') {
        this.mode = cur.method
        if (cur.question) { this.q = cur.question; this.qByMode[cur.method] = cur.question }
        if (cur.model && this.models.includes(cur.model)) this.model = cur.model
      }
      this.title = cur.cancelled ? 'cancelling…' : (cur.method === 'ontology' || cur.method === 'think' ? 'thinking…' : 'searching…')
      this.error = ''; this.answer = ''; this.json = ''; this.rows = []
      this.collapsed = false
      const t0 = performance.now() - (Number(cur.elapsed_ms) || 0)
      const freezeTimer = startTimer(this, t0)
      // Poll until the server drops this qid (finished or cancelled).
      try {
        while (gen === RUN.gen) {
          await new Promise((r) => setTimeout(r, 400))
          let snap
          try { snap = await (await fetch('/inflight')).json() } catch { break }
          const still = ((snap && snap.inflight) || []).some((x) => x.qid === cur.qid)
          if (!still) break
          if (((snap.inflight || []).find((x) => x.qid === cur.qid) || {}).cancelled) {
            this.stopping = true
            this.title = 'cancelling…'
          }
        }
        if (gen !== RUN.gen) return
        freezeTimer()
        this.title = this.stopping ? 'cancelled' : 'done'
      } finally {
        if (gen === RUN.gen) {
          if (RUN.timer != null) { clearInterval(RUN.timer); RUN.timer = null }
          this.busy = false
          this.stopping = false
          this._adopted = false
          RUN.qid = null
          RUN.ac = null
        }
        settle?.()
        if (RUN.settle === settle) { RUN.done = null; RUN.settle = null }
      }
    },
  })
  // persisted thinking=true is only honored if the restored model still supports it
  if (!store.canThink()) store.thinking = false

  // ---------- SPA entity routes ----------
  // Registered BEFORE mount so Router.start() (called inside M.mount) matches
  // a deep-linked /e/… URL and fills Router.params.
  Router.register('/', 'brain viz', () => ({ template: '' }))
  Router.register('/e/:slugs', 'brain viz', () => ({ template: '' }))

  // Delegated entity-link adapter: any <a data-entity="slug[,slug]"> in the
  // document routes through openEntities. Modified clicks (Ctrl/Cmd/Shift/
  // middle) fall through to the browser — the href is a real permalink.
  const onEntityLink = (e) => {
    if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey) return
    const a = e.target.closest?.('a[data-entity]')
    if (!a) return
    e.preventDefault()
    M.store('viz').openEntities(a.dataset.entity.split(',').filter(Boolean))
  }
  if (window.__ENTITY_LINK__) document.removeEventListener('click', window.__ENTITY_LINK__)
  window.__ENTITY_LINK__ = onEntityLink
  document.addEventListener('click', onEntityLink)

  M.mount('#app', () => ({ template: TEMPLATE }))

  // Route → scene is one-directional: Router.set / popstate / initial load all
  // land in syncRoute, which hands the slug list to the scene's apply function
  // (idempotent — re-applying the current selection is a no-op, so HMR reboots
  // don't re-trigger the zoom). Registered AFTER M.mount on purpose: mount
  // installs its own single-slot Router.onChange (clearInstances + full
  // redraw, for apps whose root is Router.render()). Ours is a static template
  // over a reactive store, so we replace that handler — store bindings already
  // keep the DOM current, and a full re-render per navigation would only
  // destroy input focus.
  const syncRoute = () => {
    const store = M.store('viz')
    store.routeSlugs = (Router.params.slugs || '').split(',').filter(Boolean).map(decodeURIComponent)
    store.api.applyEntitySelection?.(store.routeSlugs)
  }
  Router.onChange(syncRoute)
  syncRoute()      // deep link: scene not booted yet → scene.js applies routeSlugs after boot

  // Refresh recovery: if the backend still has a query running, show stop UI.
  store.adoptInflight().catch(() => {})
}

