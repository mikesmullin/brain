// ui.js — the flat HUD layer, as an m.js component tree over a named store.
// The store survives HMR (window.__M_STORES__): query text, results, sidebar
// state, and chip settings all persist across hot edits of this file.
// scene.js installs its camera/highlight actions onto store.api after boot.
import M from '/vendor/m-js/src/index.js'

const PLACEHOLDER = {
  search: 'search your brain…',
  think: 'ask a question — search + LLM synthesis…',
  ontology: 'a multi-hop relational question…',
  graph: 'pattern, e.g. Team -->|SUPPORTS| Product  or  A *6> B --shortest',
  graphql: 'Class/id { field, REL { … } }',
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
          <option :value="mm" x-text="mm || 'default (agl)'"></option>
        </template>
      </select>
      <button id="think-toggle" :class="$store.viz.thinking ? 'on' : ''"
              :title="$store.viz.thinking ? 'Disable thinking' : 'Enable thinking'"
              @click="$store.viz.toggleThinking()"><i class="ph ph-brain"></i></button>
      <button id="sel-toggle" :class="$store.viz.useSelection ? 'on' : ''"
              :title="($store.viz.useSelection ? 'Exclude selection from context' : 'Include selection as context') + ' (' + $store.viz.selectedSlugs.length + ' selected)'"
              @click="$store.viz.toggleUseSelection()"><i class="ph ph-selection-plus"></i></button>
    </span>
    <span class="qwrap">
      <input id="q" spellcheck="false" autocomplete="off" x-model="$store.viz.q"
             :placeholder="$store.viz.placeholder()"
             @keydown="if ($event.key === 'Enter') $store.viz.run()">
      <i class="ph ph-x qclear" x-show="$store.viz.q" title="clear" @click="$store.viz.clearQ()"></i>
    </span>
    <button @click="$store.viz.run()">⏎</button>
    <button id="voice-toggle" x-show="$store.viz.mode === 'think' || $store.viz.mode === 'ontology'"
            :class="$store.viz.speak ? 'on' : ''"
            :title="$store.viz.speak ? 'Disable voice output' : 'Enable voice output (Ada speaks the answer)'"
            @click="$store.viz.toggleSpeak()"><i class="ph ph-speaker-high"></i></button>
  </div>

  <div id="status" x-text="$store.viz.statusText"></div>
  <div id="home" :class="'edgebtn panel ' + ($store.viz.collapsed ? 'collapsed' : '')"
       title="back to universe (frame everything)" @click="$store.viz.api.frameUniverse()">⌂</div>
  <div id="collapse" :class="'edgebtn panel ' + ($store.viz.collapsed ? 'collapsed' : '')"
       @click="$store.viz.collapsed = !$store.viz.collapsed"
       x-text="$store.viz.collapsed ? '‹' : '›'"></div>

  <div id="sidebar" :class="'panel ' + ($store.viz.collapsed ? 'collapsed' : '')">
    <header><span x-text="$store.viz.title"></span><span id="qms" x-text="$store.viz.ms"></span></header>
    <div id="results">
      <div class="answer error" x-show="$store.viz.error" x-text="$store.viz.error"></div>
      <div class="answer" x-show="$store.viz.answer" x-text="$store.viz.answer"></div>
      <pre x-show="$store.viz.json" x-text="$store.viz.json"></pre>
      <template x-for="r in $store.viz.rows">
        <div class="hit" @click="$store.viz.openRow(r)">
          <div class="slug" x-text="r.title"></div>
          <div class="meta" x-show="r.sub" x-text="r.sub"></div>
        </div>
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
  <div id="hint" class="panel">WASD fly · E/Q up/down · MMB orbit · hold Space: pan · wheel dolly · click select · Ctrl+click multi-select · F frame</div>
</div>
`

// Explicit UI persistence across page loads (the browser's own form-value
// restoration is disabled via :value bindings + autocomplete=off — it used to
// repopulate the DOM without updating the store, desyncing the mode chips).
const STORAGE_KEY = 'brain-viz-ui'
function loadPersisted() {
  try { return JSON.parse(localStorage.getItem(STORAGE_KEY)) || {} } catch { return {} }
}

export async function boot() {
  const cfg = await (await fetch('/config.json')).json()
  const saved = loadPersisted()
  const qByMode = Object.assign({ search: '', think: '', ontology: '', graph: '', graphql: '' }, saved.qByMode || {})
  const mode = PLACEHOLDER[saved.mode] ? saved.mode : 'search'

  M.store('viz', {
    // state (survives HMR; mode/strategy/model/thinking/queries survive page loads)
    mode, q: qByMode[mode] || '', qByMode,
    strategy: saved.strategy || 'hybrid', expand: saved.expand !== false,
    models: cfg.models,
    model: cfg.models.includes(saved.model) ? saved.model : cfg.default,
    thinking: !!saved.thinking,
    useSelection: !!saved.useSelection, selectedSlugs: [],
    speak: !!saved.speak,
    collapsed: true, title: 'results', ms: '', error: '', answer: '', json: '',
    rows: [], detailSlug: '', detailJson: '', statusText: '', is3d: true,
    api: {},   // installed by scene.js: flyToNode, selectNode, setHighlights, setPath, frameUniverse, frameSelection, toggle3d

    placeholder() { return PLACEHOLDER[this.mode] },

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
    setModel(m2) { this.model = m2; this.persist() },
    toggleThinking() { this.thinking = !this.thinking; this.persist() },
    toggleUseSelection() { this.useSelection = !this.useSelection; this.persist() },
    toggleSpeak() { this.speak = !this.speak; this.persist() },
    clearQ() { this.q = ''; this.qByMode[this.mode] = ''; this.persist() },

    openRow(r) {
      if (r.kind === 'node' && r.i >= 0) { this.api.flyToNode(r.i); this.api.selectNode(r.i) }
      else if (r.kind === 'path') {
        const idxs = r.pathIdx.filter((i) => i >= 0)
        this.api.setHighlights(idxs)
        this.api.setPath(idxs)
        if (idxs.length) this.api.frameSelection()
      }
    },

    showDetail(d) {
      this.detailSlug = d.slug
      this.detailJson = JSON.stringify(
        { components: d.components, relations: d.relations, incoming: (d.incoming || []).slice(0, 30) }, null, 1)
      this.collapsed = false
    },

    async run() {
      const q = this.q.trim()
      if (!q) return
      const mode = this.mode
      this.qByMode[mode] = this.q
      this.persist()
      this.title = mode === 'think' || mode === 'ontology' ? 'thinking…' : 'searching…'
      this.ms = ''; this.error = ''; this.answer = ''; this.json = ''; this.rows = []
      this.collapsed = false
      const t0 = performance.now()
      const done = (label) => { this.title = label; this.ms = Math.round(performance.now() - t0) + ' ms' }
      try {
        if (mode === 'search') {
          const hits = await (await fetch('/search?q=' + encodeURIComponent(q) + '&strategy=' + this.strategy + '&expand=' + this.expand)).json()
          if (hits.error) throw new Error(hits.error)
          done('results (' + hits.length + ')')
          this.api.setHighlights(hits.filter((h) => h.i >= 0).map((h) => h.i))
          this.rows = hits.map((h) => ({
            kind: 'node', i: h.i, title: h.slug,
            sub: 'score ' + h.score + (h.preview ? ' · ' + JSON.stringify(h.preview).slice(0, 60) : ''),
          }))
        } else if (mode === 'think' || mode === 'ontology') {
          const sel = this.useSelection && this.selectedSlugs.length ? '&sel=' + encodeURIComponent(this.selectedSlugs.join(',')) : ''
          const res = await (await fetch('/' + mode + '?q=' + encodeURIComponent(q) + '&model=' + encodeURIComponent(this.model) + '&think=' + this.thinking + sel)).json()
          if (res.error) throw new Error(res.error)
          done(mode)
          this.answer = res.answer || '(no answer)'
          if (this.speak && res.answer) fetch('/speak?text=' + encodeURIComponent(res.answer)).catch(() => {})
          const nodes = (res.citation_nodes || res.entity_nodes || []).filter((x) => x.slug)
          this.api.setHighlights(nodes.filter((x) => x.i >= 0).map((x) => x.i))
          this.rows = nodes.map((x) => ({ kind: 'node', i: x.i, title: x.slug, sub: '' }))
        } else if (mode === 'graph') {
          const res = await (await fetch('/graph?pattern=' + encodeURIComponent(q))).json()
          if (res.error) throw new Error(res.error)
          done('matches (' + res.count + (res.capped ? ', capped' : '') + ')')
          this.rows = res.matches.slice(0, 200).map((m2) => ({
            kind: 'path', pathIdx: m2.pathIdx, title: m2.path.join(' → '), sub: m2.via.join(' · '),
          }))
        } else {   // graphql
          const res = await (await fetch('/graphql?q=' + encodeURIComponent(q))).json()
          if (res.error) throw new Error(res.error)
          done('graphql')
          this.json = JSON.stringify(res, null, 1)
          const r = await (await fetch('/resolve?slug=' + encodeURIComponent(res.slug || ''))).json()
          if (r.i >= 0) { this.api.setHighlights([r.i]); this.api.flyToNode(r.i) }
        }
      } catch (err) {
        done('error')
        this.error = err.message
      }
    },
  })

  M.mount('#app', () => ({ template: TEMPLATE }))
}
