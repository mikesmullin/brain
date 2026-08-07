// ui.js — the flat HUD layer, as an m.js component tree over a named store.
// The store survives HMR (window.__M_ALPINE_STORES__): query text, results,
// sidebar state, and chip settings all persist across hot edits of this file.
// scene.js installs its camera/highlight actions onto store.api after boot.
//
// m.js v3 rendering model (VDOM):
//   template → AST (once) → VNodes per redraw → keyed diff → DOM
// Any reactive store write schedules ONE coalesced rAF redraw. Unchanged
// state → zero DOM writes. Do not force extra ticks (streamTick etc.) for
// ordinary mutations — deep proxies already notify. Imperative DOM is only
// for contenteditable mention hosts (empty VDOM children) and x-html
// markdown (RawHTMLVNode; re-hydrate labels after string changes).
//
// Entity selection is a first-class SPA route: /e/slug1,slug2 (comma-separated
// multi-select). Every way of selecting an entity — clicking a node in the
// world, an entity link in the sidebar, pasting a permalink, browser
// back/forward — funnels through store.openEntities() → Router pushState →
// store.api.applyEntitySelection() (scene.js), which plays the zoom
// transition. To make something an entity link ANYWHERE in the HUD, render
// <a data-entity="slug[,slug]" href="…entityHref()…"> — one delegated
// document-level handler (below) adapts all of them onto the same path.
// Shift+click on entity links toggles multi-select (Ctrl/Cmd+click keeps the
// browser “open in new tab” behavior via the real /e/… permalink).
import M, { Router } from 'https://mikesmullin.github.io/m-js/dist/m.min.js'
import { marked } from 'marked'
import { registerChatStore } from '/chat/store.js'
import { hydrateEntityLinks, labelRows } from '/entity-labels.js'
import {
  initEntityModel,
  acquire,
  release,
  ensureMany,
  get as getEntity,
  peekLabel,
  refresh as refreshEntity,
  applyServerChange,
  subscribe as subscribeEntities,
} from '/entity-model.js'
import {
  attachMentionEditor,
  promoteEntityRefsInMarkdown,
  renderPlainWithMentions,
  setEditorText,
} from '/mentions.js'

const INSP_HOLDER = 'insp'

const PLACEHOLDER = {
  search: 'search your brain…',
  think: 'ask a question — search + LLM synthesis…',
  ontology: 'a multi-hop relational question…',
  graph: 'pattern, e.g. Team -->|SUPPORTS| Product  or  A *6> B --shortest',
  graphql: 'Class/id { field, REL { … } }',
}

// Class/id — id may include spaces (wiki links from LLM prose).
const SLUG_RE = /^[A-Za-z][\w]*\/[^\|\[\]]+$/

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
      return `<a class="entity-pill md-entity entity-link" data-entity="${safe}" href="${path}" title="${safe}"><i class="ph ph-cube entity-link-icon entity-link-icon-ready" aria-hidden="true"></i><span class="entity-pill-label">${text}</span></a>`
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

// Parse GFM → HTML. Entity refs become anchors via placeholders (so marked
// never escapes our <a class="md-entity"> chips into visible raw HTML).
function renderMarkdown(src) {
  if (!src) return ''
  const hrefFor = (slug) => Router.href('/e/' + encodeURIComponent(slug))
  return promoteEntityRefsInMarkdown(
    String(src),
    (md) => marked.parse(md, { async: false }),
    { hrefFor },
  )
}

/** User chat bubbles: escaped text + wiki-link pills (not full markdown). */
function renderUserMessage(src) {
  return renderPlainWithMentions(src, {
    hrefFor: (slug) => Router.href('/e/' + encodeURIComponent(slug)),
  })
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
    // [[Class/id|Label]] / [[Class/id]] → speak the label
    .replace(/\[\[\s*(?:[A-Z][A-Z0-9_]*:)?\s*([^\]|]+?)(?:\|([^\]]+))?\s*\]\]/g,
      (_, slug, label) => (label || slug || '').trim())
    .replace(/\u00a0/g, ' ')
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .replace(/[ \t]{2,}/g, ' ')
    .trim()
}

/**
 * Shared entity chip (SERPS citations, inspector header slugs, relation targets).
 * Same markup + classes everywhere so one document-level `a[data-entity]`
 * handler owns click / Shift multi-select / dblclick frame for all of them.
 *
 * @param {string} slugExpr m.js expression → slug string (e.g. `t.slug`, `s`, `r.slug`)
 * @param {{
 *   xFor?: string,
 *   keyExpr?: string,
 *   labelExpr?: string,
 *   xShow?: string,
 *   extraClass?: string,
 * }} [opts]
 * @returns {string}
 */
function entityPillHtml(slugExpr, opts = {}) {
  const labelExpr = opts.labelExpr || `$store.viz.entityLabel(${slugExpr})`
  const extra = opts.extraClass ? ` ${opts.extraClass}` : ''
  const xFor = opts.xFor ? ` x-for="${opts.xFor}"` : ''
  const key = opts.keyExpr ? ` :key="${opts.keyExpr}"` : ''
  const xShow = opts.xShow ? ` x-show="${opts.xShow}"` : ''
  return `<a class="entity-pill entity-link${extra}"${xFor}${key}${xShow}
     :class="{
       'is-selected': $store.viz.isSlugSelected(${slugExpr}),
       'is-loading': $store.viz.entityLabelLoading(${slugExpr}),
     }"
     :data-entity="${slugExpr}" data-label-bound="1"
     :href="$store.viz.entityHref(${slugExpr})"
     :title="${slugExpr}">
    <span class="entity-link-icon entity-link-loading spin" aria-hidden="true"></span>
    <i class="ph ph-cube entity-link-icon entity-link-icon-ready" aria-hidden="true"></i>
    <span class="entity-pill-label" x-text="${labelExpr}"></span>
  </a>`
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
      <span class="chip" :class="{ on: $store.viz.strategy === 'hybrid' }" @click="$store.viz.setStrategy('hybrid')">hybrid</span>
      <span class="chip" :class="{ on: $store.viz.strategy === 'keyword' }" @click="$store.viz.setStrategy('keyword')">keyword</span>
      <span class="chip" :class="{ on: $store.viz.strategy === 'vector' }" @click="$store.viz.setStrategy('vector')">vector</span>
      <span class="chip" :class="{ on: $store.viz.expand }" @click="$store.viz.toggleExpand()" title="1-hop relational expansion">expand</span>
    </span>
    <span class="qwrap">
      <!-- Contenteditable: empty VDOM children so the mention editor owns the
           subtree; only attributes are patched across redraws. -->
      <div id="q" class="mention-editor mention-editor-single"
           contenteditable="true" role="textbox" spellcheck="false"
           data-empty="1"
           :data-placeholder="$store.viz.placeholder()"></div>
      <i class="ph ph-x qclear" x-show="$store.viz.q" title="clear" @click="$store.viz.clearQ()"></i>
    </span>
    <button id="go" x-show="!$store.viz.busy" title="run query" @click="$store.viz.run()">⏎</button>
    <button id="stop" x-show="$store.viz.busy" :class="{ stopping: $store.viz.stopping }"
            :title="$store.viz.stopping ? 'cancelling…' : 'cancel this query'"
            @click="$store.viz.stop()"><i class="ph-fill ph-stop"></i></button>
  </div>

  <div id="status" x-text="$store.viz.statusText"></div>
  <div id="home" class="edgebtn panel" :class="{ collapsed: $store.viz.collapsed }"
       title="back to universe (frame everything)" @click="$store.viz.api.frameUniverse()">⌂</div>
  <div id="collapse" class="edgebtn panel" :class="{ collapsed: $store.viz.collapsed }"
       @click="$store.viz.collapsed = !$store.viz.collapsed"
       x-text="$store.viz.collapsed ? '‹' : '›'"></div>

  <div id="sidebar"
       class="panel"
       :class="{
         collapsed: $store.viz.collapsed,
         resizing: $store.viz._resizing,
         'no-chat': !$store.chat.showChat,
         'chat-empty': !$store.chat.showChat && !$store.viz.showSerps(),
       }"
       :style="$store.viz.sidebarStyle()">
    <!-- left-edge width drag handle -->
    <div class="sidebar-resize-w" title="Drag to resize width"
         @pointerdown="$store.viz.startSidebarWidthResize($event)"></div>

    <!-- ── SERPS: non-LLM search / graph / graphql hits (classic results list) ─ -->
    <div id="results" x-show="$store.viz.showSerps()">
      <header class="serps-hdr">
        <span class="hdr-title"><i class="spin" x-show="$store.viz.busy"></i>
          <span x-text="$store.viz.title || 'results'"></span></span>
        <span class="hdr-right">
          <span id="qms" class="qms" x-show="$store.viz.busy || $store.viz.ms" x-text="$store.viz.ms"></span>
        </span>
      </header>
      <div class="serps-body">
        <div class="answer error" x-show="$store.viz.error" x-text="$store.viz.error"></div>
        <a x-for="r in $store.viz.rows" :key="r.slug || r.title"
           :class="'hit'
             + (r.kind === 'node' ? ' entity-link entity-pill' : '')
             + (r.kind === 'node' && $store.viz.isSlugSelected(r.slug) ? ' is-selected' : '')"
           :href="r.kind === 'node' ? $store.viz.entityHref(r.slug) : null"
           :data-entity="r.kind === 'node' ? r.slug : null"
           :data-label-bound="r.kind === 'node' ? '1' : null"
           :title="r.kind === 'node' ? r.slug : null"
           @click="r.kind !== 'node' && $store.viz.openRow(r)">
          <div class="hit-line">
            <i class="ph ph-cube entity-link-icon entity-link-icon-ready" x-show="r.kind === 'node'" aria-hidden="true"></i>
            <div class="slug entity-pill-label" x-text="r.title"></div>
          </div>
          <div class="meta" x-show="r.sub" x-text="r.sub"></div>
        </a>
        <pre class="json-hit" x-show="$store.viz.json" x-text="$store.viz.json"></pre>
      </div>
    </div>

    <!-- ── Chat (angela multi-session) — only when a session exists ─ -->
    <div class="chat-pane" x-show="$store.chat.showChat"
         :class="{ 'history-open': $store.chat.historyOpen }">
      <header class="chat-tabs-hdr">
        <button type="button" class="icon-btn history-toggle"
                :class="{ active: $store.chat.historyOpen }"
                title="Session history"
                @click="$store.chat.toggleHistory()">
          <i class="ph ph-clock-counter-clockwise" aria-hidden="true"></i>
        </button>
        <div class="chat-tabs">
          <div class="tab" x-for="tab in $store.chat.tabList" :key="tab.id"
               :class="{ active: tab.id === $store.chat.activeTabId }"
               @click="$store.chat.selectTab(tab.id)">
            <span x-text="tab.name"></span>
            <button type="button" class="x" @click.stop="$store.chat.closeTab(tab.id)" title="Close">
              <i class="ph ph-x" aria-hidden="true"></i>
            </button>
          </div>
          <button type="button" class="icon-btn" @click="$store.chat.newTab()" title="New chat">
            <i class="ph ph-plus" aria-hidden="true"></i>
          </button>
        </div>
        <span class="hdr-right">
          <span class="qms"
                x-show="$store.chat.activeWaiting || $store.chat.ms || $store.viz.busy || $store.viz.ms"
                x-text="$store.chat.stopwatchText()"></span>
          <button id="voice-toggle"
                  :class="{ on: $store.viz.speak }"
                  :title="$store.viz.speak ? 'Disable voice output' : 'Enable voice output (Ada speaks agent replies)'"
                  @click="$store.viz.toggleSpeak()"><i class="ph ph-speaker-high"></i></button>
        </span>
      </header>

      <div class="chat-body">
        <!-- Session history (narrow column inside chat pane) -->
        <aside class="chat-history" x-show="$store.chat.historyOpen">
          <div class="chat-history-hdr">
            <span>History</span>
            <button type="button" class="icon-btn trash-all"
                    title="Delete all sessions"
                    x-show="$store.chat.history.length"
                    @click="$store.chat.cleanAllSessions()">
              <i class="ph ph-trash" aria-hidden="true"></i>
            </button>
          </div>
          <div class="chat-history-list">
            <!-- history is a reactive array; assign/splice/replace schedules one redraw -->
            <div class="chat-history-empty"
                 x-show="!$store.chat.history.length">
              No sessions yet
            </div>
            <div class="chat-history-row"
                 x-for="item in $store.chat.history"
                 :key="item.id"
                 :class="{ active: item.id === $store.chat.activeSessionId }">
              <button type="button" class="chat-history-item"
                      :title="item.title || item.agent || item.id"
                      @click="$store.chat.openSession(item.id)">
                <div class="chat-history-title" x-text="item.title || item.agent || item.id"></div>
                <div class="chat-history-meta">
                  <span x-text="item.agent || ''"></span>
                  <span x-show="item.model" x-text="' · ' + $store.chat.shortModel(item.model)"></span>
                </div>
              </button>
              <button type="button" class="trash-one" title="Delete session"
                      @click.stop="$store.chat.deleteSession(item.id)">
                <i class="ph ph-trash" aria-hidden="true"></i>
              </button>
            </div>
          </div>
        </aside>

      <div class="messages" id="viz-chat-messages" @scroll.passive="$store.chat.onMessagesScroll()">
        <div class="welcome" x-show="$store.chat.showWelcome">
          <h1>Chat</h1>
          <p class="text-muted">Multi-session agent chat over the knowledge graph. Ask a follow-up or run think/ontology.</p>
        </div>

        <div class="answer warn" x-show="$store.viz.speakWarning">🔇 please install
          <a href="https://github.com/mikesmullin/ada" target="_blank" rel="noopener">ada</a>
          to hear results spoken aloud</div>
        <!-- Citation chips — same entity-pill as inspector / relation targets -->
        <div class="query-hits" x-show="$store.viz.isLlmMode() && $store.viz.rows.length">
          <div class="hits-label">citations</div>
          <div class="query-hits-list">
            ${entityPillHtml('r.slug', {
              xFor: 'r in $store.viz.rows',
              keyExpr: "'c-' + (r.slug || r.title)",
              xShow: "r.kind === 'node'",
              labelExpr: 'r.title || $store.viz.entityLabel(r.slug)',
              extraClass: 'citation-pill',
            })}
          </div>
        </div>

        <!-- Multi-turn chat transcript -->
        <div class="msg-block" x-for="msg in $store.chat.activeMessages" :key="msg.id"
             :class="{ 'msg-hidden': msg.visible === false, 'msg-user': msg.kind === 'user' }">
          <div class="bubble-user" x-show="msg.kind === 'user'"
               x-html="$store.chat.userMessageHtml(msg)"></div>
          <div class="bubble-assistant md-body is-streaming"
               x-show="msg.kind === 'assistant' && msg.streaming">
            <!-- msg.text is deep-reactive; each token write coalesces to ≤1 redraw/frame -->
            <span x-text="msg.text || ''"></span><span class="stream-caret" aria-hidden="true"></span>
          </div>
          <div class="bubble-assistant md-body"
               x-show="msg.kind === 'assistant' && !msg.streaming"
               x-html="$store.chat.messageHtml(msg)"></div>
          <div class="bubble-reasoning" x-show="msg.kind === 'reasoning'"
               :class="{ 'is-streaming': msg.streaming }">
            <div class="reasoning-header">
              <i class="ph ph-brain reasoning-icon" aria-hidden="true"></i>
              <span class="reasoning-label">Reasoning</span>
            </div>
            <div class="reasoning-body" x-text="msg.text || ''"></div>
          </div>
          <div class="tool-card tool-call-card" x-show="msg.kind === 'tool_call'">
            <div class="tool-call-header">
              <i class="ph ph-terminal-window tool-icon" aria-hidden="true"></i>
              <span class="tool-call-name" x-text="msg.name"></span>
            </div>
            <div class="tool-call-explanation" x-show="$store.chat.toolCallExplanation(msg)"
                 x-text="$store.chat.toolCallExplanation(msg)"></div>
            <pre class="tool-call-code" x-show="$store.chat.toolCallCode(msg)"
                 x-text="$store.chat.toolCallCode(msg)"></pre>
          </div>
          <div class="tool-card tool-card-row" x-show="msg.kind === 'tool_result'"
               :class="msg.ok === false ? 'err' : 'ok'">
            <i class="ph tool-icon" :class="msg.ok === false ? 'ph-x-circle' : 'ph-check-circle'"
               aria-hidden="true"></i>
            <span x-text="msg.name"></span>
            <pre class="tool-result-body" x-text="msg.text || ''"></pre>
          </div>
          <div class="approval-card" x-show="msg.kind === 'approval_needed'"
               :class="msg.status === 'pending' ? 'pending' : 'resolved'">
            <div class="approval-title">
              <i class="ph ph-warning tool-icon" aria-hidden="true"></i>
              Approval
            </div>
            <div class="approval-name" x-text="msg.name"></div>
            <div class="approval-actions" x-show="msg.status === 'pending'">
              <button type="button" class="btn-allow" @click="$store.chat.decideApproval(msg, 'allow')">Allow</button>
              <button type="button" class="btn-deny" @click="$store.chat.decideApproval(msg, 'deny')">Deny</button>
            </div>
          </div>
          <!-- Hover gutter: visibility · goto bookmark · gen metrics (cowork-style) -->
          <div class="msg-gutter"
               :class="{ 'has-goto': $store.chat.ensureGoto(msg).enabled }">
            <div class="msg-gutter-actions">
              <button type="button" class="gutter-btn"
                      :title="msg.visible === false ? 'Include in context window' : 'Exclude from context window'"
                      @click="$store.chat.toggleMsgVisible(msg)">
                <i class="ph"
                   :class="msg.visible === false ? 'ph-eye-slash' : 'ph-eye'"
                   aria-hidden="true"></i>
              </button>
              <button type="button" class="gutter-btn gutter-btn-goto"
                      :class="{ on: $store.chat.ensureGoto(msg).enabled }"
                      title="Goto bookmark — agent can jump here with goto(label)"
                      @click="$store.chat.toggleMsgGoto(msg)">
                <i class="ph ph-bookmark-simple" aria-hidden="true"></i>
              </button>
              <input type="text" class="goto-label-input"
                     :class="{ duplicate: $store.chat.isGotoLabelDuplicate(msg) }"
                     x-show="$store.chat.ensureGoto(msg).enabled"
                     :value="$store.chat.ensureGoto(msg).label"
                     @input="$store.chat.onGotoLabelInput(msg, $event)"
                     @click.stop
                     placeholder="label"
                     spellcheck="false"
                     autocomplete="off">
            </div>
            <div class="msg-gutter-metrics"
                 x-show="msg.telemetry || (msg.kind === 'tool_result' && msg.full_chars)"
                 x-html="$store.chat.telemetryHtml(msg)"></div>
          </div>
        </div>

        <div class="thinking" x-show="$store.chat.activeWaiting">
          Thinking <span>•</span><span>•</span><span>•</span>
        </div>
      </div>
      </div><!-- /.chat-body -->

      <footer class="composer" x-show="$store.chat.showChat">
        <div class="composer-inner">
          <div class="allowlist-panel" x-show="$store.chat.allowlistOpen">
            <label class="allowlist-label">
              allowed tools (one rule per line)
              <span class="allowlist-source"
                    x-text="$store.chat.allowlistOverridden ? '· ui override' : '· agent default'"></span>
            </label>
            <textarea class="allowlist-ta" x-model="$store.chat.allowlistText"
                      @input="$store.chat.onAllowlistInput()"
                      @blur="$store.chat.onAllowlistBlur()"
                      spellcheck="false"></textarea>
          </div>
          <div class="tools-panel" x-show="$store.chat.toolsPanelOpen">
            <div class="tools-panel-hdr">
              <label class="allowlist-label">
                tools for inference
                <span class="allowlist-source"
                      x-text="$store.chat.toolsEnabledOverridden ? '· ui override' : '· agent default'"></span>
              </label>
              <div class="tools-panel-actions">
                <button type="button" class="tools-mini-btn"
                        @click="$store.chat.enableAllTools()" title="Enable all">all</button>
                <button type="button" class="tools-mini-btn"
                        @click="$store.chat.disableAllTools()" title="Disable all">none</button>
                <button type="button" class="tools-mini-btn"
                        @click="$store.chat.resetToolsEnabled()" title="Reset to agent default">reset</button>
              </div>
            </div>
            <div class="tools-panel-hint">
              Unchecked tools are removed from the model (not registered). Distinct from allowlist (auto-approve).
            </div>
            <div class="tools-checklist" x-show="!$store.chat.toolsCatalogLoading">
              <label class="tools-check-row" x-for="t in $store.chat.toolsCatalog" :key="t.name">
                <input type="checkbox"
                       :checked="$store.chat.isToolEnabled(t.name)"
                       @change="$store.chat.setToolEnabled(t.name, $event.target.checked)" />
                <span class="tools-check-name" x-text="t.name" :title="t.description || t.name"></span>
              </label>
              <div class="tools-empty" x-show="!$store.chat.toolsCatalog.length && !$store.chat.toolsCatalogError">
                No tools registered for this agent.
              </div>
              <div class="tools-error" x-show="$store.chat.toolsCatalogError"
                   x-text="$store.chat.toolsCatalogError"></div>
            </div>
            <div class="tools-loading" x-show="$store.chat.toolsCatalogLoading">Loading tools…</div>
          </div>
          <!-- Contenteditable: empty VDOM children; mention editor owns subtree. -->
          <div class="draft-ta mention-editor mention-editor-multi" id="viz-chat-draft"
               contenteditable="true" role="textbox" spellcheck="true"
               data-empty="1"
               data-placeholder="Message… (Enter send, Shift+Enter newline, @mention)"></div>
          <div class="composer-row">
            <select class="dd agent-dd" id="viz-chat-agent"
                    :value="$store.chat.activeTab?.agent || ''"
                    @change="$store.chat.onAgentChange($event)"
                    :disabled="$store.chat.activeWaiting || !$store.chat.agents.length"
                    title="Agent">
              <option x-for="a in $store.chat.agents" :key="a.name"
                      :value="a.name" x-text="a.name"></option>
            </select>
            <select class="dd model-dd" id="viz-chat-model"
                    :value="$store.chat.activeTab?.model || ''"
                    @change="$store.chat.onModelChange($event)"
                    :disabled="$store.chat.activeWaiting || !$store.chat.modelOptions.length"
                    title="Model">
              <option x-for="m in $store.chat.modelOptions" :key="m"
                      :value="m" x-text="$store.chat.shortModel(m)"></option>
            </select>
            <select class="dd effort-dd" id="viz-chat-effort"
                    :value="$store.chat.reasoningEffort || 'medium'"
                    @change="$store.chat.onEffortChange($event)"
                    :disabled="$store.chat.activeWaiting"
                    title="Reasoning effort (provider effort; Gemma 4 also uses depth phrases)">
              <option value="low">low</option>
              <option value="medium">medium</option>
              <option value="high">high</option>
              <option value="xhigh">xhigh</option>
              <option value="max">max</option>
            </select>
            <button type="button" id="think-toggle" class="icon-btn"
                    x-show="$store.viz.canThink()"
                    :class="{ on: $store.viz.thinking }"
                    :title="$store.viz.thinking ? 'Disable Gemma 4 think token' : 'Enable Gemma 4 think token (<|think|>)'"
                    @click="$store.viz.toggleThinking()"><i class="ph-bold ph-brain"></i></button>
            <button type="button" id="sel-toggle" class="icon-btn"
                    :class="{ on: $store.viz.useSelection }"
                    :title="($store.viz.useSelection ? 'Exclude selection from chat context' : 'Include selection as chat context') + ' (' + $store.viz.selectedSlugs.length + ' selected)'"
                    @click="$store.viz.toggleUseSelection()"><i class="ph-bold ph-selection-plus"></i></button>
            <div class="ctx-pie" :title="$store.chat.contextTooltip" role="img"
                 :aria-label="$store.chat.contextTooltip">
              <svg viewBox="0 0 36 36" width="28" height="28" aria-hidden="true">
                <circle cx="18" cy="18" r="15.5" fill="none" stroke="rgba(255,255,255,.08)" stroke-width="4"/>
                <circle cx="18" cy="18" r="15.5" fill="none" stroke="#7aa2ff" stroke-width="4"
                        stroke-linecap="round" :stroke-dasharray="$store.chat.pieDash"
                        stroke-dashoffset="0" transform="rotate(-90 18 18)"/>
              </svg>
              <span class="ctx-pie-label" x-text="$store.chat.pieUsedK"></span>
            </div>
            <button type="button" class="btn-stop" x-show="$store.chat.activeWaiting"
                    @click="$store.chat.stop()" title="Stop agent">
              <i class="ph-fill ph-stop" aria-hidden="true"></i>
            </button>
            <button type="button" class="icon-btn tools-toggle"
                    :class="{ active: $store.chat.toolsPanelOpen }"
                    @click="$store.chat.toggleToolsPanel()"
                    :title="'Tools for inference' + ($store.chat.toolsEnabledOverridden ? ' (ui override)' : '')">
              <i class="ph ph-wrench" aria-hidden="true"></i>
            </button>
            <button type="button" class="icon-btn allowlist-toggle"
                    :class="{ active: $store.chat.allowlistOpen }"
                    @click="$store.chat.toggleAllowlist()" title="Allowlist">
              <i class="ph ph-shield-check" aria-hidden="true"></i>
            </button>
            <button type="button" class="btn-primary" x-show="!$store.chat.activeWaiting"
                    @click="$store.chat.send()"
                    :disabled="!($store.chat.draftInput || '').trim()">
              <i class="ph ph-paper-plane-right" aria-hidden="true"></i>
            </button>
          </div>
        </div>
      </footer>
    </div>

    <!-- vertical split between chat and inspector -->
    <div class="sidebar-resize-h" title="Drag to resize inspector"
         x-show="$store.viz.inspVisible()"
         @pointerdown="$store.viz.startInspectorHeightResize($event)"></div>

    <!-- ── Inspector (Unity-style multi-edit) ───────────────────── -->
    <div id="inspector" x-show="$store.viz.inspVisible()"
         :style="$store.viz.inspStyle()">
      <div class="insp-hdr">
        <span class="insp-title">Inspector</span>
        <span class="insp-count" x-text="$store.viz.inspCountLabel()"></span>
      </div>
      <div class="insp-body">
        <div class="insp-loading" x-show="$store.viz.insp.loading">Loading…</div>
        <div class="insp-error" x-show="$store.viz.insp.error" x-text="$store.viz.insp.error"></div>
        <div class="insp-slugs" x-show="$store.viz.insp.slugs.length">
          ${entityPillHtml('s', {
            xFor: 's in $store.viz.insp.slugs',
            keyExpr: 's',
            extraClass: 'insp-slug',
          })}
        </div>
        <div class="insp-row" x-for="f in $store.viz.insp.fields" :key="f.key">
          <label class="insp-key" :title="f.key" x-text="f.key"></label>
          <div class="insp-val">
            <!-- boolean -->
            <input type="checkbox" x-show="f.kind === 'bool' && !f.mixed"
                   :checked="f.value === true"
                   @change="$store.viz.inspCommit(f, $event.target.checked)">
            <!-- number -->
            <input type="number" class="insp-input" x-show="f.kind === 'number' && !f.mixed"
                   :value="f.value"
                   @change="$store.viz.inspCommit(f, $event.target.valueAsNumber)">
            <!-- string / mixed / complex display -->
            <input type="text" class="insp-input" x-show="f.kind === 'string' || f.mixed || f.kind === 'other'"
                   :class="{ mixed: f.mixed }"
                   :value="f.mixed ? '-' : f.display"
                   @focus="$store.viz.inspFocus(f, $event)"
                   @change="$store.viz.inspCommit(f, $event.target.value)"
                   :placeholder="f.mixed ? 'mixed' : ''">
          </div>
        </div>
        <!-- Relations: collapsed groups by type → expand for entity pills -->
        <div class="insp-section insp-rel-section" x-show="$store.viz.insp.relations.length">
          <div class="insp-section-title insp-rel-heading">Relations</div>
          <div class="insp-rel-tree">
            <div class="insp-rel-group" x-for="r in $store.viz.insp.relations" :key="r.key"
                 :class="{ open: r.open }">
              <button type="button" class="insp-rel-head"
                      @click="$store.viz.toggleInspRel(r)"
                      :title="r.key + ' (' + r.targets.length + ')'">
                <i class="ph insp-rel-toggle"
                   :class="r.open ? 'ph-minus' : 'ph-plus'"
                   aria-hidden="true"></i>
                <span class="insp-rel-count" x-text="r.targets.length"></span>
                <span class="insp-rel-key" x-text="r.key"></span>
              </button>
              <div class="insp-rel-targets" x-show="r.open">
                ${entityPillHtml('t.slug', {
                  xFor: 't in r.targets',
                  keyExpr: 't.slug',
                  extraClass: 'insp-rel-pill',
                })}
                <div class="insp-rel-empty" x-show="!r.targets.length">none</div>
              </div>
            </div>
          </div>
        </div>
        <div class="insp-status" x-show="$store.viz.insp.status" x-text="$store.viz.insp.status"></div>
      </div>
    </div>
  </div>

  <div id="mode3d" class="panel" @click="$store.viz.api.toggle3d()">
    <i class="ph" :class="{ 'ph-square': $store.viz.is3d, 'ph-cube': !$store.viz.is3d }"></i>
    <span x-text="$store.viz.is3d ? '2D' : '3D'"></span>
  </div>
  <div id="hint" class="panel">WASD fly · E/Q up/down · MMB orbit · Space pan · wheel dolly · click/Shift select · pill click/Shift · dblclick frame · Esc clear · F frame · Home out</div>
</div>
`

// Explicit UI persistence across page loads (the browser's own form-value
// restoration is disabled via :value bindings + autocomplete=off — it used to
// repopulate the DOM without updating the store, desyncing the mode chips).
const STORAGE_KEY = 'brain-viz-ui'
const SIDEBAR_W_DEFAULT = 380
const SIDEBAR_W_MIN = 280
const SIDEBAR_W_MAX = 720
const INSP_H_DEFAULT = 260
const INSP_H_MIN = 100
const CHAT_MIN_H = 160
const SPLIT_H = 6
function loadPersisted() {
  try { return JSON.parse(localStorage.getItem(STORAGE_KEY)) || {} } catch { return {} }
}
function clampSidebarW(n) {
  const v = Number(n)
  if (!Number.isFinite(v)) return SIDEBAR_W_DEFAULT
  return Math.round(Math.min(SIDEBAR_W_MAX, Math.max(SIDEBAR_W_MIN, v)))
}
function clampInspH(n, maxH = 800) {
  const v = Number(n)
  if (!Number.isFinite(v)) return INSP_H_DEFAULT
  return Math.round(Math.min(maxH, Math.max(INSP_H_MIN, v)))
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

/** Unique Class/id refs from [[wiki-links]] in assistant text (for SERP highlights). */
function wikiSlugsFromText(text) {
  const out = []
  const seen = new Set()
  const re = /\[\[(?:[A-Z][A-Z0-9_]*:)?([A-Za-z][\w]*\/[^\]|#]+)(?:[|#][^\]]*)?\]\]/g
  let m
  while ((m = re.exec(String(text || '')))) {
    const slug = m[1].trim()
    if (!slug || seen.has(slug) || !SLUG_RE.test(slug)) continue
    seen.add(slug)
    out.push(slug)
  }
  return out
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

// ── Inspector helpers (Unity multi-edit aggregation) ─────────────────────

function classifyValue(v) {
  if (typeof v === 'boolean') return 'bool'
  if (typeof v === 'number' && Number.isFinite(v)) return 'number'
  if (typeof v === 'string') return 'string'
  return 'other'
}

function formatFieldDisplay(v) {
  if (v == null) return ''
  if (typeof v === 'object') {
    try { return JSON.stringify(v) } catch { return String(v) }
  }
  return String(v)
}

function valuesEqual(a, b) {
  if (a === b) return true
  if (a == null || b == null) return a == b
  if (typeof a === 'object' || typeof b === 'object') {
    try { return JSON.stringify(a) === JSON.stringify(b) } catch { return false }
  }
  return false
}

/** Flatten entity.components → Map of alias.field → value */
function flattenComponents(entity) {
  /** @type {Map<string, any>} */
  const out = new Map()
  const comps = entity?.components || {}
  for (const [alias, fields] of Object.entries(comps)) {
    if (!fields || typeof fields !== 'object' || Array.isArray(fields)) {
      out.set(alias, fields)
      continue
    }
    for (const [k, v] of Object.entries(fields)) {
      out.set(`${alias}.${k}`, v)
    }
  }
  return out
}

function aggregateFields(entities) {
  if (!entities.length) return []
  /** @type {Map<string, { value: any, mixed: boolean }>} */
  const acc = new Map()
  for (const e of entities) {
    const flat = flattenComponents(e)
    for (const [key, val] of flat) {
      if (!acc.has(key)) {
        acc.set(key, { value: val, mixed: false })
      } else {
        const cur = acc.get(key)
        if (!cur.mixed && !valuesEqual(cur.value, val)) cur.mixed = true
      }
    }
    // Keys present in prior entities but missing here → mixed empty
    for (const [key, cur] of acc) {
      if (!flat.has(key)) cur.mixed = true
    }
  }
  // Also mark keys only on some entities
  const keysPerEntity = entities.map((e) => new Set(flattenComponents(e).keys()))
  for (const [key, cur] of acc) {
    if (!keysPerEntity.every((s) => s.has(key))) cur.mixed = true
  }

  return [...acc.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([key, { value, mixed }]) => ({
      key,
      kind: mixed ? 'string' : classifyValue(value),
      value,
      display: mixed ? '-' : formatFieldDisplay(value),
      mixed,
    }))
}

/**
 * Aggregate relations across a multi-select set.
 * Groups by relationship type (outgoing + incoming), union of related slugs.
 * Each group starts collapsed (`open: false`); UI expands to entity pills.
 * @param {Array<{ relations?: object, incoming?: Array<{ from?: string, rel?: string }> }>} entities
 * @returns {Array<{ key: string, open: boolean, targets: Array<{ slug: string }> }>}
 */
function aggregateRelations(entities) {
  if (!entities.length) return []
  /** @type {Map<string, Set<string>>} */
  const byRel = new Map()
  const add = (rel, slug) => {
    if (!rel || !slug) return
    let set = byRel.get(rel)
    if (!set) {
      set = new Set()
      byRel.set(rel, set)
    }
    set.add(slug)
  }
  for (const e of entities) {
    const rels = e.relations || {}
    for (const [rel, targets] of Object.entries(rels)) {
      for (const t of targets || []) {
        const slug = typeof t === 'string' ? t : t?._to
        if (slug) add(rel, String(slug))
      }
    }
    // Reverse edges from the index (include_links on /nodes)
    for (const link of e.incoming || []) {
      if (link?.from && link?.rel) add(String(link.rel), String(link.from))
    }
  }
  return [...byRel.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([key, set]) => ({
      key,
      open: false,
      targets: [...set]
        .sort((a, b) => a.localeCompare(b))
        .map((slug) => ({ slug })),
    }))
}

export async function boot() {
  const cfg = await (await fetch('/config.json')).json()
  const saved = loadPersisted()
  const qByMode = Object.assign({ search: '', think: '', ontology: '', graph: '', graphql: '' }, saved.qByMode || {})
  // first visit (nothing persisted): default to ontology — the most capable
  // query mode, so a newcomer's first question gets the best possible answer
  const mode = PLACEHOLDER[saved.mode] ? saved.mode : 'ontology'

  // SPA entity model (cache + refcount) — before chat/viz so both can read it
  initEntityModel()

  // Chat store (angela multi-session) — must exist before mount so $store.chat binds
  const chatStore = registerChatStore()
  chatStore.init?.()

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
    _labelTick: 0,   // bumped when entity name cache fills → re-paint x-text labels
    routeSlugs: [],   // entity slugs parsed from the current /e/… route
    skipZoomOnce: false,   // one-shot: next route apply selects without flying
    frameAfterSelectOnce: false, // one-shot: frame survivors after single-click select
    // Sidebar layout (px) — persisted; CSS vars keep edge-buttons aligned
    sidebarWidth: clampSidebarW(saved.sidebarWidth),
    inspHeight: clampInspH(saved.inspHeight),
    _resizing: false,
    // Inspector (Unity-style multi-edit): flattened component fields
    insp: {
      loading: false, error: '', status: '',
      slugs: [], entities: [],
      fields: [],   // { key, kind, value, display, mixed }
      relations: [], // read-only aggregate of relations
    },
    api: {},   // installed by scene.js: flyToNode, applyEntitySelection, setHighlights, setPath, frameUniverse, frameSelection, toggle3d

    placeholder() { return PLACEHOLDER[this.mode] },

    // Thinking toggle (Gemma 4 only): injects <|think|> + effort phrase via Angela.
    // Pattern *:gemma4 / gemma-4 — see angela/think isGemma4Model.
    canThink() {
      let m = this.model || this.aglDefault
      try {
        const cm = M.store('chat')?.activeTab?.model
        if (cm) m = cm
      } catch { /* chat not ready */ }
      return /gemma[-_]?4/i.test(m || '')
    },

    /** Active chat model, falling back to persisted viz.model / agl default. */
    effectiveModel() {
      try {
        const cm = M.store('chat')?.activeTab?.model
        if (cm) return cm
      } catch { /* ignore */ }
      return this.model || this.aglDefault || ''
    },

    /**
     * Speak plain text via Ada when the speaker toggle is on.
     * Shared by one-shot think/ontology and multi-turn chat replies.
     */
    speakText(src) {
      if (!this.speak || !src) return
      const spoken = stripMarkdownForSpeech(src)
      if (!spoken) return
      fetch('/speak?text=' + encodeURIComponent(spoken))
        .then((r) => r.json())
        .then((s) => { if (s.error) this.speakWarning = true })
        .catch(() => { this.speakWarning = true })
    },

    // GFM markdown → HTML for the sidebar answer pane (think / ontology).
    answerHtml() { return renderMarkdown(this.answer) },

    /** think / ontology — multi-turn chat; everything else is classic SERPS. */
    isLlmMode() {
      return this.mode === 'think' || this.mode === 'ontology'
    },

    /**
     * Non-LLM query results pane (search / graph / graphql).
     * Separate from the chat UI so SERPS stay a plain hit list.
     */
    showSerps() {
      if (this.isLlmMode()) return false
      return !!(
        this.busy ||
        this.error ||
        this.json ||
        (this.rows && this.rows.length)
      )
    },

    /** Chat tabs that still have transcript content (not just an empty shell). */
    hasActiveChat() {
      try {
        const c = M.store('chat')
        return !!(c && c.tabs?.some((t) => t.messages?.length))
      } catch {
        return false
      }
    },

    /**
     * Anything that warrants keeping the right sidebar open.
     * Used after deselect / clear-search so we only auto-collapse when empty.
     */
    sidebarHasContent() {
      if (this.showSerps()) return true
      if (this.hasActiveChat()) return true
      if (this.busy) return true
      if (this.insp?.slugs?.length || this.detailSlug) return true
      // LLM one-shot still mid-flight or citation list before seed settles
      if (this.isLlmMode() && (this.answer || this.error || this.rows?.length)) return true
      return false
    },

    /** Collapse only when SERPS, chat, and selection are all gone. */
    maybeCollapseSidebar() {
      if (!this.sidebarHasContent()) this.collapsed = true
    },

    /** Whether a slug is in the current multi-select set (route or scene). */
    isSlugSelected(slug) {
      if (!slug) return false
      if ((this.selectedSlugs || []).includes(slug)) return true
      if ((this.routeSlugs || []).includes(slug)) return true
      return false
    },

    /**
     * Resolved display name (or slug if unknown). Kicks off one-shot prefetch.
     *
     * Under m.js v3 VDOM there is no per-binding effect subscription: any
     * store write schedules one full-tree redraw, so we do NOT read
     * entityStore().rev here (that would be pointless noise). Labels land via
     * entities-store upserts (or _labelTick when only Set membership changes
     * for the spinner). peekLabel is a plain cache read.
     */
    entityLabel(slug) {
      void this._labelTick
      if (!slug) return ''
      const name = peekLabel(slug)
      if (name && name !== slug) return name
      // Unknown / still the slug — one-shot resolve
      void this.prefetchEntityLabel(slug)
      return name || slug
    },

    /**
     * True while a label fetch is in flight (or not yet attempted) for a slug
     * that still displays as the raw Class/id. Used for pill spinners.
     * _labelTick exists because loading state lives in non-reactive Sets;
     * mutating a Set does not notify the store proxy.
     */
    entityLabelLoading(slug) {
      void this._labelTick
      if (!slug) return false
      const name = peekLabel(slug)
      if (name && name !== slug) return false
      this._labelPrefetch = this._labelPrefetch || new Set()
      this._labelTried = this._labelTried || new Set()
      // In-flight always shows spinner
      if (this._labelPrefetch.has(slug)) return true
      // Not tried yet — will be kicked by entityLabel; treat as loading
      if (!this._labelTried.has(slug)) return true
      return false
    },

    async prefetchEntityLabel(slug) {
      if (!slug) return
      this._labelPrefetch = this._labelPrefetch || new Set()
      this._labelTried = this._labelTried || new Set()
      if (this._labelPrefetch.has(slug) || this._labelTried.has(slug)) return
      // Already have a human name in store/cache
      const existing = peekLabel(slug)
      if (existing && existing !== slug) {
        this._labelTried.add(slug)
        return
      }
      this._labelPrefetch.add(slug)
      this._labelTick = (this._labelTick || 0) + 1 // paint spinner
      try {
        const { resolveLabel } = await import('/entity-labels.js')
        await resolveLabel(slug)
        this._labelTried.add(slug)
      } catch {
        this._labelTried.add(slug)
      } finally {
        this._labelPrefetch?.delete?.(slug)
        // Always tick so pills drop spinner / pick up name (or stay on slug)
        this._labelTick = (this._labelTick || 0) + 1
      }
    },

    /**
     * Batch-resolve labels for many slugs (relation expand). One WS round-trip
     * instead of N× one-slug.
     * @param {string[]} slugs
     */
    async prefetchEntityLabels(slugs) {
      const list = [...new Set((slugs || []).filter(Boolean))]
      if (!list.length) return
      this._labelPrefetch = this._labelPrefetch || new Set()
      this._labelTried = this._labelTried || new Set()
      const need = list.filter((s) => {
        const n = peekLabel(s)
        if (n && n !== s) {
          this._labelTried.add(s)
          return false
        }
        if (this._labelTried.has(s) || this._labelPrefetch.has(s)) return false
        return true
      })
      if (!need.length) {
        this._labelTick = (this._labelTick || 0) + 1
        return
      }
      for (const s of need) this._labelPrefetch.add(s)
      this._labelTick = (this._labelTick || 0) + 1
      try {
        const { ensureLabels } = await import('/entity-model.js')
        await ensureLabels(need)
      } catch {
        /* ignore */
      } finally {
        for (const s of need) {
          this._labelPrefetch.delete(s)
          this._labelTried.add(s)
        }
        this._labelTick = (this._labelTick || 0) + 1
      }
    },

    /**
     * Rebuild inspector fields/relations from entity-model cache (no network).
     * Called when the model upserts a slug currently shown in the inspector.
     * Does NOT hydrate DOM links or setLabel — that path loops back into the model.
     */
    rebuildInspectorFromModel() {
      const list = (this.insp?.slugs || []).filter(Boolean)
      if (!list.length) return
      // Do not read entityStore().rev — would subscribe this path to every upsert
      const entities = list
        .map((s) => getEntity(s))
        .filter((e) => e && !e.error && !e.deleted && !e.loading)
      if (!entities.length) return
      const fields = aggregateFields(entities)
      const prevOpen = new Set(
        (this.insp.relations || []).filter((g) => g.open).map((g) => g.key),
      )
      // Cap relation targets so a hub node doesn't spawn thousands of x-for effects
      const REL_TARGET_CAP = 48
      const relations = aggregateRelations(entities).map((g) => ({
        ...g,
        targets: g.targets.slice(0, REL_TARGET_CAP),
        open: prevOpen.has(g.key),
      }))
      const sig = entities
        .map((e) => `${e.slug}:${e.fetchedAt || 0}:${e.label || ''}`)
        .join('|')
      if (sig === this._inspModelSig) return
      this._inspModelSig = sig
      // Single bag replace = fewer intermediate reactive triggers than 4 sets
      this.detailSlug = list.join(', ')
      this.insp = {
        ...this.insp,
        entities,
        fields,
        relations,
        loading: false,
        error: this.insp.error || '',
        slugs: list.slice(),
      }
    },

    /**
     * Tag every a[data-entity] in the DOM with .is-selected when its slug is
     * in the selection set. Needed for markdown-rendered links (static HTML)
     * that can't use reactive :class.
     * @param {{ hydrate?: boolean }} [opts] hydrate=true also resolves link labels
     *   (default false — hydration is owned by refreshEntityLabels to avoid loops)
     */
    syncEntityLinkSelection(opts = {}) {
      const set = new Set([
        ...(this.selectedSlugs || []),
        ...(this.routeSlugs || []),
      ])
      const apply = () => {
        document.querySelectorAll('a[data-entity]').forEach((a) => {
          const slugs = String(a.dataset.entity || '')
            .split(',')
            .filter(Boolean)
          a.classList.toggle(
            'is-selected',
            slugs.length > 0 && slugs.every((s) => set.has(s)),
          )
        })
        if (opts.hydrate) void hydrateEntityLinks(document)
      }
      // Next frame so m.js x-for / x-html paint first
      if (typeof requestAnimationFrame === 'function') {
        requestAnimationFrame(apply)
      } else {
        apply()
      }
    },

    /**
     * Hydrate entity-link labels after content paint (SERPS / chat).
     * Selection classes only — does not re-enter sync→hydrate→sync.
     */
    async refreshEntityLabels() {
      if (this._refreshLabelsBusy) return
      this._refreshLabelsBusy = true
      try {
        await new Promise((r) => requestAnimationFrame(() => r()))
        await hydrateEntityLinks(document)
        // Class-only pass (no nested hydrate)
        this.syncEntityLinkSelection?.({ hydrate: false })
      } finally {
        this._refreshLabelsBusy = false
      }
    },

    /** Wipe non-LLM SERPS (and shared one-shot result fields). */
    clearResults() {
      this.error = ''
      this.answer = ''
      this.json = ''
      this.rows = []
      this.ms = ''
      this.title = 'results'
      this.speakWarning = false
      try {
        this.api.setHighlights?.([])
        this.api.setPath?.([])
      } catch { /* scene not ready */ }
    },

    sidebarStyle() {
      const w = this.sidebarWidth || SIDEBAR_W_DEFAULT
      // Custom props work via m-js setProperty (Alpine-style style bind).
      return { width: w + 'px', '--sidebar-w': w + 'px' }
    },
    inspStyle() {
      const h = this.inspHeight || INSP_H_DEFAULT
      // max-height + overflow hidden keep the panel from growing with content
      // so .insp-body's overflow-y:auto actually scrolls.
      return {
        height: h + 'px',
        maxHeight: h + 'px',
        flex: '0 0 auto',
        overflow: 'hidden',
      }
    },
    applyLayoutVars() {
      try {
        const root = document.documentElement
        root.style.setProperty('--sidebar-w', (this.sidebarWidth || SIDEBAR_W_DEFAULT) + 'px')
      } catch { /* ignore */ }
    },

    // ── Sidebar / inspector drag-resize ────────────────────────────
    startSidebarWidthResize(e) {
      if (e.button != null && e.button !== 0) return
      e.preventDefault()
      e.stopPropagation()
      const startX = e.clientX
      const startW = this.sidebarWidth || SIDEBAR_W_DEFAULT
      this._resizing = true
      const prevCursor = document.body.style.cursor
      const prevUserSelect = document.body.style.userSelect
      document.body.style.cursor = 'ew-resize'
      document.body.style.userSelect = 'none'
      const onMove = (ev) => {
        // Dragging the left edge: move left → wider, right → narrower
        const next = clampSidebarW(startW + (startX - ev.clientX))
        this.sidebarWidth = next
        this.applyLayoutVars()
      }
      const onUp = () => {
        this._resizing = false
        document.body.style.cursor = prevCursor
        document.body.style.userSelect = prevUserSelect
        window.removeEventListener('pointermove', onMove)
        window.removeEventListener('pointerup', onUp)
        window.removeEventListener('pointercancel', onUp)
        this.persist()
      }
      window.addEventListener('pointermove', onMove)
      window.addEventListener('pointerup', onUp)
      window.addEventListener('pointercancel', onUp)
    },

    startInspectorHeightResize(e) {
      if (e.button != null && e.button !== 0) return
      e.preventDefault()
      e.stopPropagation()
      const startY = e.clientY
      const startH = this.inspHeight || INSP_H_DEFAULT
      const sidebarEl = document.getElementById('sidebar')
      const maxH = sidebarEl
        ? Math.max(INSP_H_MIN, sidebarEl.clientHeight - CHAT_MIN_H - SPLIT_H)
        : 600
      this._resizing = true
      const prevCursor = document.body.style.cursor
      const prevUserSelect = document.body.style.userSelect
      document.body.style.cursor = 'ns-resize'
      document.body.style.userSelect = 'none'
      const onMove = (ev) => {
        // Dragging the split up → taller inspector
        const next = clampInspH(startH + (startY - ev.clientY), maxH)
        this.inspHeight = next
      }
      const onUp = () => {
        this._resizing = false
        document.body.style.cursor = prevCursor
        document.body.style.userSelect = prevUserSelect
        window.removeEventListener('pointermove', onMove)
        window.removeEventListener('pointerup', onUp)
        window.removeEventListener('pointercancel', onUp)
        this.persist()
      }
      window.addEventListener('pointermove', onMove)
      window.addEventListener('pointerup', onUp)
      window.addEventListener('pointercancel', onUp)
    },

    persist() {
      try {
        // Preserve chatModel (and other keys) written by the chat store
        let prev = {}
        try { prev = JSON.parse(localStorage.getItem(STORAGE_KEY) || '{}') || {} } catch { /* ignore */ }
        let chatModel = prev.chatModel || ''
        try {
          const cm = M.store('chat')?.preferredModel
          if (cm) chatModel = cm
        } catch { /* chat not ready */ }
        localStorage.setItem(STORAGE_KEY, JSON.stringify({
          ...prev,
          mode: this.mode, strategy: this.strategy, expand: this.expand,
          model: this.model, thinking: this.thinking, useSelection: this.useSelection,
          reasoningEffort: (() => {
            try { return M.store('chat')?.reasoningEffort || 'medium' } catch { return 'medium' }
          })(),
          speak: this.speak, qByMode: this.qByMode,
          sidebarWidth: this.sidebarWidth,
          inspHeight: this.inspHeight,
          // Last chat-composer model — applied to new tabs from search / + tab
          chatModel: chatModel || this.model || prev.chatModel || '',
        }))
      } catch { /* storage unavailable — persistence is best-effort */ }
    },

    // per-mode query memory: stash the current text, restore the new mode's
    setMode(m2) {
      // Capture live contenteditable before switching
      try {
        const t = window.__SEARCH_MENTION__?.getText?.()
        if (typeof t === 'string') this.q = t
      } catch { /* ignore */ }
      this.qByMode[this.mode] = this.q
      this.mode = m2
      this.q = this.qByMode[m2] || ''
      this.persist()
      queueMicrotask(() => {
        try {
          window.__SEARCH_MENTION__?.setText?.(this.q || '')
        } catch { /* ignore */ }
      })
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
    clearQ() {
      this.q = ''
      this.qByMode[this.mode] = ''
      this.persist()
      // Keep the contenteditable search composer in sync
      try {
        window.__SEARCH_MENTION__?.clear?.()
      } catch { /* ignore */ }
      // X in the search field also dismisses SERPS in the sidebar
      this.clearResults()
      this.maybeCollapseSidebar()
    },

    // ---- entity links (the one place selection-by-reference is implemented) ----
    // Permalink for one or more slugs, e.g. entityHref(['a','b']) → /e/a,b
    entityHref(slugs) {
      const list = Array.isArray(slugs) ? slugs : [slugs]
      return Router.href('/e/' + list.map(encodeURIComponent).join(','))
    },
    /** Current selection as slugs (scene + route — either may lag briefly). */
    currentSelectionSlugs() {
      const a = this.selectedSlugs || []
      const b = this.routeSlugs || []
      if (!a.length) return b.slice()
      if (!b.length) return a.slice()
      // Prefer selectedSlugs when both set; union if they diverge
      const set = new Set([...a, ...b])
      return [...set]
    },

    // Navigate to an entity selection. additive=true toggles the slugs in/out
    // of the current selection (Shift+click on links / canvas). Pushes a history
    // entry; the route-change handler applies it to the scene.
    // Camera fly-to is controlled by noZoom:
    //   - canvas click + entity-pill single-click → noZoom (select in place)
    //   - entity-pill double-click → frame only (frameEntityOnly; selection unchanged)
    //   - deep links / back-forward → zoom (skipZoomOnce left false)
    openEntities(slugs, additive = false, { noZoom = false } = {}) {
      let list = Array.isArray(slugs) ? slugs.slice() : [slugs]
      if (additive) {
        const cur = this.currentSelectionSlugs()
        for (const s of list) {
          const at = cur.indexOf(s)
          if (at >= 0) cur.splice(at, 1); else cur.push(s)
        }
        list = cur
      }
      this.skipZoomOnce = noZoom
      Router.set(list.length ? '/e/' + list.map(encodeURIComponent).join(',') : '/')
    },

    /**
     * Entity-link single-click selection, then frame-zoom the surviving set:
     *   - Shift: toggle membership of the link's slugs (multi-select)
     *   - Plain click when all link slugs are already selected: toggle them OFF
     *     (sole selection → clear; multi → remove just those)
     *   - Plain click otherwise: replace selection with the link's slugs
     * After any change, camera frames the remaining selection (if non-empty).
     */
    selectEntityLink(slugs, { shiftKey = false, noZoom = true } = {}) {
      const list = (Array.isArray(slugs) ? slugs : [slugs]).filter(Boolean)
      const cur = this.currentSelectionSlugs()
      let next
      if (shiftKey) {
        next = cur.slice()
        for (const s of list) {
          const at = next.indexOf(s)
          if (at >= 0) next.splice(at, 1)
          else next.push(s)
        }
      } else {
        const allSelected =
          list.length > 0 && list.every((s) => cur.includes(s))
        if (allSelected) {
          // Toggle off these slugs
          next = cur.filter((s) => !list.includes(s))
        } else {
          next = list.slice()
        }
      }
      // Apply exact survivor set (no second toggle pass)
      this.frameAfterSelectOnce = next.length > 0
      this.openEntities(next, false, { noZoom })
      // Frame survivors only — not F-style yellow-highlight union
      if (next.length) {
        requestAnimationFrame(() => {
          if (!this.frameAfterSelectOnce) return // apply already framed
          this.frameAfterSelectOnce = false
          try {
            void this.api.frameSlugs?.(next)
          } catch { /* scene not ready */ }
        })
      }
    },

    /**
     * Frame-zoom onto these slugs WITHOUT changing the selection set.
     * Used by entity-pill double-click (markdown, mentions, relations, citations).
     * @param {string|string[]} slugs
     */
    frameEntityOnly(slugs) {
      const list = (Array.isArray(slugs) ? slugs : [slugs]).filter(Boolean)
      if (!list.length) return
      try {
        void this.api.frameSlugs?.(list)
      } catch { /* scene not ready */ }
    },

    /**
     * @deprecated use frameEntityOnly — double-click must not replace selection
     */
    openEntitiesAndFrame(slugs) {
      this.frameEntityOnly(slugs)
    },

    openRow(r) {   // path rows only — node rows render as entity links
      const idxs = r.pathIdx.filter((i) => i >= 0)
      this.api.setHighlights(idxs)
      this.api.setPath(idxs)
      if (idxs.length) this.api.frameSelection()
    },

    showDetail(d) {
      // Legacy single-entity path — prefer loadInspector for multi-select
      this.detailSlug = d.slug
      this.detailJson = JSON.stringify(
        { components: d.components, relations: d.relations, incoming: (d.incoming || []).slice(0, 30) }, null, 1)
      this.collapsed = false
      void this.loadInspector(d.slug ? [d.slug] : [])
    },

    inspCountLabel() {
      const n = this.insp.slugs.length
      if (!n) return ''
      return n === 1 ? '1 selected' : n + ' selected'
    },

    /** Inspector panel visible when loading or it has fields/relations/slugs. */
    inspVisible() {
      const i = this.insp
      if (!i) return false
      if (i.loading) return true
      if (i.fields?.length || i.relations?.length || i.slugs?.length) return true
      return false
    },

    /**
     * Expand/collapse a relation group; prefetch labels for target pills.
     * @param {{ open?: boolean, targets?: Array<{ slug: string }> }} r
     */
    toggleInspRel(r) {
      if (!r) return
      r.open = !r.open
      if (r.open && r.targets?.length) {
        // One batched /labels WS call for the whole group (not N× one-slug)
        void this.prefetchEntityLabels(r.targets.map((t) => t?.slug).filter(Boolean))
        // Lime selection classes on newly painted pills
        this.syncEntityLinkSelection?.()
      }
    },

    /**
     * Open inspector for slugs: acquire entity-model refs, fetch via model,
     * render fields from the store (Unity-style multi-edit aggregates).
     * @param {string[]} slugs
     */
    async loadInspector(slugs) {
      const list = (slugs || []).filter(Boolean)
      // Release previous inspector holds
      for (const s of this.insp?.slugs || []) {
        release(s, INSP_HOLDER)
      }
      if (!list.length) {
        this.insp = { loading: false, error: '', status: '', slugs: [], entities: [], fields: [], relations: [] }
        this.detailSlug = ''
        return
      }
      for (const s of list) acquire(s, INSP_HOLDER)
      this.insp.loading = true
      this.insp.error = ''
      this.insp.status = ''
      this.insp.slugs = list.slice()
      this.collapsed = false
      try {
        await ensureMany(list, { force: false })
        const entities = list
          .map((s) => getEntity(s))
          .filter((e) => e && !e.error && !e.deleted)
        if (!entities.length) {
          const firstErr = list.map((s) => getEntity(s)).find((e) => e?.error)
          throw new Error(firstErr?.error || 'no entities')
        }
        // Single rebuild path (sets _inspModelSig so subscriber no-ops)
        this._inspModelSig = ''
        this.rebuildInspectorFromModel()
        this.insp.loading = false
        // Light label tick for slug pills — not full DOM hydrate
        this._labelTick = (this._labelTick || 0) + 1
      } catch (err) {
        this.insp.error = err.message || String(err)
        this.insp.fields = []
        this.insp.relations = []
        this.insp.loading = false
      } finally {
        this.insp.loading = false
      }
    },

    inspFocus(f, ev) {
      // When focusing a mixed field showing "-", clear so user can type a new value
      if (f.mixed && ev?.target && ev.target.value === '-') {
        ev.target.value = ''
      }
    },

    /**
     * Commit an inspector field edit to all selected entities via set_instance.
     */
    async inspCommit(f, raw) {
      if (!f || !this.insp.slugs.length) return
      // Ignore blank submit on mixed (user didn't change)
      if (f.mixed && (raw === '' || raw === '-' || raw == null)) return

      let val = raw
      if (f.kind === 'bool') val = !!raw
      else if (f.kind === 'number') {
        val = typeof raw === 'number' ? raw : Number(raw)
        if (!Number.isFinite(val)) {
          this.insp.status = 'invalid number'
          return
        }
      } else if (f.kind === 'other') {
        // Try JSON parse for arrays/objects
        try { val = JSON.parse(String(raw)) } catch { val = String(raw) }
      } else {
        val = String(raw ?? '')
      }

      // YAML-ish assignment value for set_instance
      let yamlVal
      if (typeof val === 'boolean' || typeof val === 'number') yamlVal = String(val)
      else if (val == null) yamlVal = 'null'
      else if (typeof val === 'object') yamlVal = JSON.stringify(val)
      else {
        // quote strings that need it
        const s = String(val)
        yamlVal = /[:#\n]|^\s|\s$/.test(s) ? JSON.stringify(s) : s
      }

      const assignment = f.key + '=' + yamlVal
      this.insp.status = 'saving…'
      try {
        const res = await (await fetch('/entity/set', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            slugs: this.insp.slugs.slice(),
            assignments: [assignment],
          }),
        })).json()
        if (!res.ok) {
          const err = (res.results || []).find((r) => !r.ok)
          throw new Error(err?.error || res.error || 'save failed')
        }
        // Refresh entity-model for edited slugs (single source of truth)
        await Promise.all(
          (this.insp.slugs || []).map((s) => refreshEntity(s, { force: true })),
        )
        this.rebuildInspectorFromModel()
        this.insp.status = 'saved'
        setTimeout(() => { if (this.insp.status === 'saved') this.insp.status = '' }, 1200)
      } catch (err) {
        this.insp.status = err.message || String(err)
      }
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
      // think/ontology from the search bar run through chat NDJSON — abort that too
      try { await M.store('chat')?.stop?.() } catch { /* ignore */ }
      try { RUN.ac?.abort() } catch { /* ignore */ }
      if (RUN.done) {
        try { await RUN.done } catch { /* ignore */ }
      }
    },

    // One query at a time. Submitting while busy = stop the old one first
    // (Enter has the same effect as the stop button, then starts the new query).
    // `_launching` collapses key-repeat / double-Enter storms into a single launch.
    async run() {
      // Prefer live search contenteditable (wiki-serialized mentions)
      try {
        const t = window.__SEARCH_MENTION__?.getText?.()
        if (typeof t === 'string') this.q = t
      } catch { /* ignore */ }
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
        // One-shot /think|/ontology RPC still uses qid; chat-stream path does not.
        qid = null
        RUN.qid = qid
        RUN.done = new Promise((r) => { settle = r })
        RUN.settle = settle
        signal = RUN.ac.signal
        this.title = mode === 'think' || mode === 'ontology' ? 'thinking…' : 'searching…'
        this.error = ''; this.answer = ''; this.json = ''; this.rows = []; this.speakWarning = false
        this.collapsed = false
        // Hybrid search: open a chat tab with the query (no LLM stream).
        // think/ontology use chat.send() below — live NDJSON (tools + tokens).
        if (mode === 'search') {
          try {
            const chat = M.store('chat')
            if (chat) {
              chat.newTab()
              const t = chat.activeTab
              if (t) {
                t.name = q.slice(0, 24) || 'Search'
                t.isNew = false
                t.waiting = true
                t.messages.push({
                  id: crypto.randomUUID(),
                  kind: 'user',
                  text: q,
                  visible: true,
                  telemetry: null,
                  eventId: null,
                  goto: { enabled: false, label: '' },
                })
                chat.startStopwatch()
              }
            }
          } catch { /* chat store not ready */ }
        }
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
          void labelRows(this.rows).then(() => this.refreshEntityLabels?.())
        } else if (mode === 'think' || mode === 'ontology') {
          // Live multi-turn chat stream (same path as the composer): tools,
          // reasoning, and token deltas via /api/chat NDJSON — not the one-shot
          // /think|/ontology RPC that only dumped a final answer into the pane.
          const chat = M.store('chat')
          if (!chat) throw new Error('chat store not ready')
          chat.newTab()
          const t = chat.activeTab
          if (t) {
            t.model = this.effectiveModel() || this.model || t.model
            t.name = q.slice(0, 24) || mode
          }
          // chat.send owns waiting/stopwatch; abort via viz.stop() → chat.stop()
          await chat.send(q)
          if (gen !== RUN.gen) return
          // Final assistant text for answer panel + SERP highlights from wiki-links
          const asst = [...(chat.activeTab?.messages || [])]
            .reverse()
            .find((m) => m.kind === 'assistant' && m.text && !m.streaming)
          this.answer = (asst?.text || '').trim() || '(no answer)'
          finish(mode)
          const slugs = wikiSlugsFromText(this.answer)
          if (slugs.length) {
            const resolved = await Promise.all(
              slugs.slice(0, 40).map(async (slug) => {
                try {
                  const r = await (await fetch('/resolve?slug=' + encodeURIComponent(slug))).json()
                  return { slug, i: r.i ?? -1 }
                } catch {
                  return { slug, i: -1 }
                }
              }),
            )
            if (gen !== RUN.gen) return
            this.api.setHighlights(resolved.filter((x) => x.i >= 0).map((x) => x.i))
            this.rows = resolved.map((x) => ({
              kind: 'node', i: x.i, slug: x.slug, title: x.slug, sub: '',
            }))
            void labelRows(this.rows).then(() => this.refreshEntityLabels?.())
          }
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
          // Surface the failure in the chat tab we already opened
          try {
            M.store('chat')?.seedFromQuery?.({
              question: q,
              answer: 'Error: ' + (err.message || String(err)),
              mode,
            })
          } catch { /* ignore */ }
        }
      } finally {
        if (gen === RUN.gen) {
          if (RUN.timer != null) { clearInterval(RUN.timer); RUN.timer = null }
          this.busy = false
          this.stopping = false
          this._adopted = false
          RUN.ac = null
          RUN.qid = null
          try { M.store('chat')?.clearWaiting?.() } catch { /* ignore */ }
          // SERPS / citation links just painted — names + selection styling
          void this.refreshEntityLabels?.()
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
  store.applyLayoutVars()

  // ---------- SPA entity routes ----------
  // Registered BEFORE mount so Router.start() (called inside M.mount) matches
  // a deep-linked /e/… URL and fills Router.params.
  Router.register('/', 'brain viz', () => ({ template: '' }))
  Router.register('/e/:slugs', 'brain viz', () => ({ template: '' }))

  // Delegated entity-pill adapter: ONE path for every a[data-entity] / .entity-pill
  // (markdown anchors, mention composer chips, relation pills, citations, SERPS).
  //   single click     → after LINK_CLICK_MS: select / toggle (selection changes)
  //   double-click     → within LINK_CLICK_MS: cancel pending select, frame-zoom
  //                      that entity ONLY — selection set is NOT modified
  //   Shift+click      → multi-select toggle (add/remove)
  //   Ctrl/Cmd+click   → browser new-tab via permalink
  //
  // Capture phase so inspector / contenteditable cannot swallow the gesture.
  const LINK_CLICK_MS = 300
  /** @type {{ timer: ReturnType<typeof setTimeout>, slugs: string[], shiftKey: boolean } | null} */
  let pendingLinkClick = null
  const clearPendingLinkClick = () => {
    if (pendingLinkClick?.timer != null) clearTimeout(pendingLinkClick.timer)
    pendingLinkClick = null
  }
  // HMR: drop any timer from a previous boot
  if (window.__ENTITY_LINK_PENDING__) {
    try { clearTimeout(window.__ENTITY_LINK_PENDING__.timer) } catch { /* ignore */ }
    window.__ENTITY_LINK_PENDING__ = null
  }

  /** @param {Element|null|undefined} el */
  const entityLinkFromEvent = (el) => {
    if (!el?.closest) return null
    return (
      el.closest('a[data-entity]') ||
      el.closest('a.entity-pill') ||
      el.closest('a.entity-link[href*="/e/"]') ||
      el.closest('a.md-entity[href*="/e/"]')
    )
  }

  /** @param {HTMLAnchorElement} a */
  const slugsFromEntityLink = (a) => {
    const raw =
      a.getAttribute('data-entity') ||
      a.dataset?.entity ||
      ''
    if (raw) return String(raw).split(',').map((s) => s.trim()).filter(Boolean)
    // Fallback: parse /e/slug1,slug2 from href
    try {
      const href = a.getAttribute('href') || ''
      const m = href.match(/\/e\/([^?#]+)/)
      if (m) {
        return decodeURIComponent(m[1])
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean)
      }
    } catch { /* ignore */ }
    return []
  }

  /**
   * Double-click / second click within LINK_CLICK_MS:
   * camera frames the pill's entity — does NOT change selection.
   * @param {string[]} slugs
   */
  const frameEntityLink = (slugs) => {
    clearPendingLinkClick()
    window.__ENTITY_LINK_PENDING__ = null
    M.store('viz').frameEntityOnly(slugs)
  }

  const onEntityLink = (e) => {
    if (e.button !== 0) return
    // Ctrl/Cmd → open permalink in new tab (don't steal the browser gesture)
    if (e.ctrlKey || e.metaKey) return
    const a = /** @type {HTMLAnchorElement|null} */ (entityLinkFromEvent(/** @type {Element} */ (e.target)))
    if (!a) return
    const slugs = slugsFromEntityLink(a)
    if (!slugs.length) return
    e.preventDefault()
    e.stopPropagation()
    const shiftKey = !!e.shiftKey

    // Second click within the debounce window → double-click: frame only
    // (cancel pending single-click select so selection stays put)
    if (pendingLinkClick) {
      frameEntityLink(slugs)
      return
    }

    pendingLinkClick = {
      slugs,
      shiftKey,
      timer: setTimeout(() => {
        const job = pendingLinkClick
        pendingLinkClick = null
        window.__ENTITY_LINK_PENDING__ = null
        if (!job) return
        // Single-click: may change selection (plain / shift toggle)
        M.store('viz').selectEntityLink(job.slugs, {
          shiftKey: job.shiftKey,
          noZoom: true,
        })
      }, LINK_CLICK_MS),
    }
    window.__ENTITY_LINK_PENDING__ = pendingLinkClick
  }
  const onEntityLinkDbl = (e) => {
    if (e.button !== 0) return
    if (e.ctrlKey || e.metaKey) return
    const a = /** @type {HTMLAnchorElement|null} */ (entityLinkFromEvent(/** @type {Element} */ (e.target)))
    if (!a) return
    const slugs = slugsFromEntityLink(a)
    if (!slugs.length) return
    e.preventDefault()
    e.stopPropagation()
    // Browser dblclick event: same as second-click path (frame, no select)
    frameEntityLink(slugs)
  }
  // Remove prior listeners (HMR) — both bubble and capture registrations
  if (window.__ENTITY_LINK__) {
    document.removeEventListener('click', window.__ENTITY_LINK__, true)
    document.removeEventListener('click', window.__ENTITY_LINK__, false)
  }
  if (window.__ENTITY_LINK_DBL__) {
    document.removeEventListener('dblclick', window.__ENTITY_LINK_DBL__, true)
    document.removeEventListener('dblclick', window.__ENTITY_LINK_DBL__, false)
  }
  window.__ENTITY_LINK__ = onEntityLink
  window.__ENTITY_LINK_DBL__ = onEntityLinkDbl
  // Capture: runs before sidebar/inspector handlers; shift-click multi-select
  // works on relation pills, citations, markdown chips, SERPS, composer chips.
  document.addEventListener('click', onEntityLink, true)
  document.addEventListener('dblclick', onEntityLinkDbl, true)

  // HMR remount: drop the previous VDOM tree so the new TEMPLATE AST is used.
  // Stores (viz/chat/entities) live in window.__M_ALPINE_STORES__ and survive.
  // Without unmount, rootInstance keeps the old template string forever.
  try {
    if (M.root || document.getElementById('app')?.childNodes?.length) {
      M.unmount()
    }
  } catch {
    /* first boot — nothing to unmount */
  }
  // Clear any leftover DOM if unmount raced (defensive)
  const appEl = document.getElementById('app')
  if (appEl && appEl.childNodes.length) appEl.replaceChildren()

  M.mount('#app', () => ({ template: TEMPLATE }))

  // Entity-model → views: when a cached entity changes, rebuild inspector if
  // that slug is open, and refresh link labels.
  if (window.__ENTITY_MODEL_UNSUB__) {
    try {
      window.__ENTITY_MODEL_UNSUB__()
    } catch { /* ignore */ }
  }
  // Debounced view sync — NEVER call refreshEntityLabels on 'label' (that
  // re-enters hydrate→setLabel→notify→hydrate and melts the tab).
  let _entityViewSyncTimer = null
  /** @type {Set<string>} */
  const _entityViewPending = new Set()
  /** @type {Set<string>} */
  const _entityViewReasons = new Set()
  window.__ENTITY_MODEL_UNSUB__ = subscribeEntities((slug, _rec, reason) => {
    try {
      const r = reason || 'upsert'
      // Ignore pure label writes from hydrate — rev bump already refreshes x-text
      if (r === 'label' || r === 'evict' || r === 'loading') {
        if (r === 'label') {
          const viz = M.store('viz')
          if (viz) viz._labelTick = (viz._labelTick || 0) + 1
        }
        return
      }
      if (slug) _entityViewPending.add(slug)
      else _entityViewPending.add('*')
      _entityViewReasons.add(r)
      if (_entityViewSyncTimer != null) return
      _entityViewSyncTimer = setTimeout(() => {
        _entityViewSyncTimer = null
        const pending = [..._entityViewPending]
        const reasons = [..._entityViewReasons]
        _entityViewPending.clear()
        _entityViewReasons.clear()
        try {
          const viz = M.store('viz')
          if (!viz) return
          const inspSlugs = viz.insp?.slugs || []
          const hitInsp =
            pending.includes('*') ||
            pending.some((s) => inspSlugs.includes(s))
          if (hitInsp) viz.rebuildInspectorFromModel?.()
          // DOM hydrate only for body/server changes — not every upsert
          const needHydrate = reasons.some((x) =>
            x === 'server' || x === 'delete' || x === 'fetch' || x === 'fetchMany',
          )
          if (needHydrate) void viz.refreshEntityLabels?.()
          else viz._labelTick = (viz._labelTick || 0) + 1
        } catch (err) {
          console.warn('[entity-model] view sync failed', err)
        }
      }, 32) // ~2 frames — coalesce storms
    } catch (err) {
      console.warn('[entity-model] view sync failed', err)
    }
  })

  // ---- @-mention composers (search + chat) ----
  const hrefFor = (slug) => Router.href('/e/' + encodeURIComponent(slug))

  const bindSearchMention = () => {
    const el = document.getElementById('q')
    if (!el) return
    window.__SEARCH_MENTION__?.destroy?.()
    // Seed pills before attach so we never emit an empty onChange wipe
    setEditorText(el, store.q || '', { hrefFor })
    window.__SEARCH_MENTION__ = attachMentionEditor(el, {
      multiline: false,
      hrefFor,
      onChange: (text) => {
        store.q = text
        store.qByMode[store.mode] = text
      },
      onSubmit: () => {
        if (!store.busy) void store.run()
      },
      syncSelectionClass: () => store.syncEntityLinkSelection?.(),
    })
  }

  const bindChatMention = () => {
    const el = document.getElementById('viz-chat-draft')
    if (!el) return
    window.__CHAT_MENTION__?.destroy?.()
    let seed = ''
    try {
      seed = M.store('chat')?.activeTab?.input || ''
    } catch { /* ignore */ }
    setEditorText(el, seed, { hrefFor })
    window.__CHAT_MENTION__ = attachMentionEditor(el, {
      multiline: true,
      hrefFor,
      onChange: (text) => {
        try {
          const chat = M.store('chat')
          if (chat?.activeTab) chat.activeTab.input = text
        } catch { /* ignore */ }
      },
      onSubmit: () => {
        try {
          void M.store('chat')?.send?.()
        } catch { /* ignore */ }
      },
      syncSelectionClass: () => store.syncEntityLinkSelection?.(),
    })
  }

  // After mount + next frame so contenteditable nodes exist
  requestAnimationFrame(() => {
    bindSearchMention()
    bindChatMention()
  })

  // Expose for chat store (tab switch / clear after send)
  store._bindChatMention = bindChatMention
  store._chatMention = () => window.__CHAT_MENTION__
  store._searchMention = () => window.__SEARCH_MENTION__

  // Route → scene is one-directional: Router.set / popstate / initial load all
  // land in syncRoute, which hands the slug list to the scene's apply function
  // (idempotent — re-applying the current selection is a no-op, so HMR reboots
  // don't re-trigger the zoom).
  //
  // Registered AFTER M.mount on purpose: mount installs a single-slot
  // Router.onChange that clearInstances() + nulls rootInstance (correct for
  // apps whose factory is Router.render()). Ours is ONE static template over
  // reactive stores — route changes only write store.routeSlugs, which already
  // schedules a coalesced VDOM redraw. Replacing that handler avoids tearing
  // down the whole tree (and focus) on every /e/… navigation.
  const syncRoute = () => {
    const store = M.store('viz')
    store.routeSlugs = (Router.params.slugs || '').split(',').filter(Boolean).map(decodeURIComponent)
    store.api.applyEntitySelection?.(store.routeSlugs)
    // applyEntitySelection is a no-op when selection is unchanged — still refresh
    // lime link styling against the latest routeSlugs (markdown x-html pills).
    store.syncEntityLinkSelection?.()
  }
  Router.onChange(syncRoute)
  syncRoute()      // deep link: scene not booted yet → scene.js applies routeSlugs after boot

  // Refresh recovery: if the backend still has a query running, show stop UI.
  store.adoptInflight().catch(() => {})
}

