/**
 * Chat store for brain viz — multi-tab angela sessions (cowork-compatible API).
 * Survives HMR via M.store('chat').
 */
import M from '/vendor/m-js/src/m.js';
import { marked } from 'marked';
import {
  extractWikiSlugs,
  promoteEntityRefsInMarkdown,
  renderPlainWithMentions,
  serializeEditor,
} from '/mentions.js';

// Class/id — id may include spaces (wiki links from LLM prose).
const SLUG_RE = /^[A-Za-z][\w]*\/[^\|\[\]]+$/;

const mdRenderer = {
  link({ href, title, tokens }) {
    const text = this.parser.parseInline(tokens);
    const h = href || '';
    let slug = null;
    if (SLUG_RE.test(h)) slug = h;
    else {
      const m = h.match(/^\/e\/(.+)$/);
      if (m) slug = decodeURIComponent(m[1].split(',')[0]);
    }
    if (slug) {
      const safe = slug.replace(/"/g, '&quot;');
      return `<a class="entity-pill md-entity entity-link" data-entity="${safe}" href="/e/${encodeURIComponent(slug)}" title="${safe}"><i class="ph ph-cube entity-link-icon entity-link-icon-ready" aria-hidden="true"></i><span class="entity-pill-label">${text}</span></a>`;
    }
    const t = title ? ` title="${String(title).replace(/"/g, '&quot;')}"` : '';
    return `<a href="${h.replace(/"/g, '&quot;')}"${t} target="_blank" rel="noopener noreferrer">${text}</a>`;
  },
};

marked.use({ gfm: true, breaks: true, renderer: mdRenderer });

export function renderMarkdown(src) {
  if (!src) return '';
  const hrefFor = (slug) => '/e/' + encodeURIComponent(slug);
  // Placeholders around entity chips so marked won't escape the HTML
  return promoteEntityRefsInMarkdown(
    String(src),
    (md) => marked.parse(md, { async: false }),
    { hrefFor },
  );
}

/** User bubbles: plain text + wiki-link entity pills. */
export function renderUserMessage(src) {
  return renderPlainWithMentions(src);
}

/**
 * Session transcript cache (client). Avoids re-fetching / re-parsing large
 * jsonl sessions when flipping history items. Invalidated on send/delete.
 * @type {Map<string, { session: object, messages: object[] }>}
 */
const sessionCache = (window.__CHAT_SESSION_CACHE__ ??= new Map());

/** Precompute bubble HTML so x-for doesn't re-run marked per paint. */
function decorateMessages(messages) {
  if (!Array.isArray(messages)) return [];
  return messages.map((m) => {
    const row = { ...m };
    if (!row.goto || typeof row.goto !== 'object') {
      row.goto = { enabled: false, label: '' };
    }
    if (row.kind === 'assistant' && row.text && !row.streaming) {
      row.html = row.html || renderMarkdown(row.text);
    }
    if (row.kind === 'user' && row.text) {
      row.userHtml = row.userHtml || renderUserMessage(row.text);
    }
    if (row.telemetry && !row.telemetryHtml) {
      // filled lazily via store method if needed
    }
    return row;
  });
}

function cacheSession(sessionId, session, messages) {
  if (!sessionId) return;
  try {
    sessionCache.set(String(sessionId), {
      session: session || {},
      messages: decorateMessages(messages || []),
    });
  } catch {
    /* ignore quota / clone issues */
  }
}

function invalidateSessionCache(sessionId) {
  if (sessionId) sessionCache.delete(String(sessionId));
}

/** Sync contenteditable draft from active tab.input (wiki text). */
function syncDraftEditor(chatStore) {
  queueMicrotask(() => {
    const api = window.__CHAT_MENTION__;
    if (!api) return;
    const text = chatStore?.activeTab?.input || '';
    // Avoid clobbering if the editor already matches (e.g. mid-type)
    const cur = api.getText?.() || '';
    if (cur === text) return;
    api.setText?.(text);
  });
}

/**
 * @returns {object} chat store
 */
/** Shared with viz UI prefs (brain-viz-ui) — chatModel is not gated by config.yaml models. */
const UI_STORAGE_KEY = 'brain-viz-ui';

function readUiPrefs() {
  try {
    return JSON.parse(localStorage.getItem(UI_STORAGE_KEY) || '{}') || {};
  } catch {
    return {};
  }
}

function writeUiPrefs(patch) {
  try {
    const cur = readUiPrefs();
    localStorage.setItem(UI_STORAGE_KEY, JSON.stringify({ ...cur, ...patch }));
  } catch {
    /* storage unavailable */
  }
}

export function registerChatStore() {
  return M.store('chat', {
    tabs: [],
    activeTabId: null,
    nextTabId: 1,
    agents: [],
    history: [],
    historyOpen: false,
    activeSessionId: null,
    projectRoot: '',
    /** Last model chosen in the composer dropdown (localStorage-backed). */
    preferredModel: '',
    stickToBottom: true,
    stickThresholdPx: 40,
    /**
     * Live / frozen elapsed for the current (or last) chat turn.
     * Reset on every send(); freezes when the stream finishes or is stopped.
     */
    ms: '',
    _elapsedTimer: null,
    _elapsedT0: 0,
    /** Bumped on stream tokens so fine-grained UI stays live (m.js). */
    streamTick: 0,
    _scrollProgrammatic: false,
    _bootstrapped: false,
    _msgObserver: null,
    _streamRaf: 0,

    init() {
      // No default empty "Chat 1" — the chat pane stays hidden until a session
      // is created (seedFromQuery / newTab / send). Drop a leftover empty default
      // from older builds that always seeded one tab.
      if (
        this.tabs?.length === 1 &&
        this.tabs[0].isNew &&
        !(this.tabs[0].messages && this.tabs[0].messages.length) &&
        !this.tabs[0].sessionId
      ) {
        this.tabs = [];
        this.activeTabId = null;
      } else if (this.tabs?.length) {
        if (!this.tabs.some((t) => t.id === this.activeTabId)) {
          this.activeTabId = this.tabs[0].id;
        }
      } else {
        this.activeTabId = null;
      }
      // Restore last model pick before any newTab / search seed
      this.loadPreferredModel();
      if (!this._bootstrapped) {
        this._bootstrapped = true;
        if (typeof fetch === 'function') void this.bootstrap();
      }
      queueMicrotask(() => {
        this.syncSelects();
        this.ensureMessagesPinned();
      });
    },

    loadPreferredModel() {
      const raw = readUiPrefs();
      // Prefer dedicated chatModel; fall back to legacy viz.model
      this.preferredModel = String(raw.chatModel || raw.model || '').trim();
    },

    /**
     * Remember model for future tabs + page reloads.
     * @param {string} model
     */
    savePreferredModel(model) {
      const m = String(model || '').trim();
      if (!m) return;
      this.preferredModel = m;
      writeUiPrefs({ chatModel: m, model: m });
      try {
        const viz = M.store('viz');
        if (viz) {
          viz.model = m;
          if (!viz.canThink?.()) viz.thinking = false;
          // Merge through viz.persist so other fields stay in sync
          viz.persist?.();
        }
      } catch {
        /* ignore */
      }
    },

    /**
     * Models offered for a tab's agent (or the active tab).
     * @param {object} [tab]
     * @returns {string[]}
     */
    modelsForTab(tab) {
      const t = tab || this.activeTab;
      if (!t) return [];
      const a = this.agents.find((x) => x.name === t.agent);
      const list =
        (a?.models && a.models.length ? a.models : null) ||
        (a?.model ? [a.model] : []) ||
        [];
      const uniq = [...new Set(list.filter(Boolean))];
      if (t.model && !uniq.includes(t.model)) uniq.unshift(t.model);
      return uniq;
    },

    /**
     * Pick model for a new/blank tab: preferred (if agent supports it) → agent default.
     * @param {object} tab
     * @param {object} [agent]
     */
    resolveModelForTab(tab, agent) {
      const a =
        agent ||
        this.agents.find((x) => x.name === tab?.agent) ||
        this.agents[0];
      const list =
        (a?.models && a.models.length ? a.models : null) ||
        (a?.model ? [a.model] : []) ||
        [];
      const preferred = this.preferredModel;
      if (preferred && list.includes(preferred)) return preferred;
      // Also accept preferred even if not in list yet (still show/use it)
      if (preferred && (!list.length || preferred === a?.model)) return preferred;
      return a?.model || list[0] || preferred || tab?.model || '';
    },

    blankTab(id, name) {
      return {
        id,
        name: name || `Chat ${id}`,
        messages: [],
        input: '',
        waiting: false,
        isNew: true,
        // Filled from /api/agents on bootstrap (first listed agent, often sole real agent or "default")
        agent: '',
        model: '',
        allowlist: '',
        allowlistBaseline: '',
        allowlistOverridden: false,
        allowlistOpen: false,
        allowlistInited: false,
        // toolsEnabled: null = all tools (agent default); string[] = subset
        toolsEnabled: null,
        toolsEnabledBaseline: null,
        toolsEnabledOverridden: false,
        toolsPanelOpen: false,
        toolsCatalog: [],
        toolsCatalogLoading: false,
        toolsCatalogError: '',
        toolsInited: false,
        sessionId: null,
        contextWindow: 32768,
        tokensUsed: 0,
        tokensFromProvider: false,
        lastTotalTokens: 0,
      };
    },

    get activeTab() {
      if (!this.tabs?.length) return null;
      return this.tabs.find((t) => t.id === this.activeTabId) || this.tabs[0] || null;
    },
    get tabList() {
      return this.tabs || [];
    },
    /** True when at least one chat tab/session is open in the UI. */
    get showChat() {
      return (this.tabs || []).length > 0;
    },
    get activeMessages() {
      return this.activeTab?.messages || [];
    },
    get activeWaiting() {
      return Boolean(this.activeTab?.waiting);
    },

    /**
     * Header stopwatch text: live chat.ms while a chat turn is running or frozen;
     * fall back to viz.ms for search/think when chat.ms not set yet.
     */
    stopwatchText() {
      if (this.ms) return this.ms;
      try {
        const viz = M.store('viz');
        if (viz?.ms) return viz.ms;
      } catch {
        /* ignore */
      }
      return '';
    },

    /** Start / restart the chat-turn stopwatch (call on every user send / search seed). */
    startStopwatch() {
      this.stopStopwatch({ freeze: false });
      const t0 = performance.now();
      this._elapsedT0 = t0;
      const tick = () => {
        this.ms = ((performance.now() - t0) / 1000).toFixed(3) + 's';
      };
      tick();
      this._elapsedTimer = setInterval(tick, 32);
    },

    /**
     * Stop the stopwatch.
     * @param {{ freeze?: boolean }} [opts] freeze=true keeps final elapsed visible
     */
    stopStopwatch({ freeze = true } = {}) {
      if (this._elapsedTimer != null) {
        clearInterval(this._elapsedTimer);
        this._elapsedTimer = null;
      }
      if (freeze && this._elapsedT0) {
        this.ms = ((performance.now() - this._elapsedT0) / 1000).toFixed(3) + 's';
      } else if (!freeze) {
        this.ms = '';
        this._elapsedT0 = 0;
      }
    },
    get showWelcome() {
      const t = this.activeTab;
      return Boolean(
        this.showChat && t?.isNew && !(t.messages && t.messages.length),
      );
    },
    get historyList() {
      return this.history || [];
    },
    get allowlistOpen() {
      return Boolean(this.activeTab?.allowlistOpen);
    },
    get allowlistOverridden() {
      return Boolean(this.activeTab?.allowlistOverridden);
    },
    get toolsPanelOpen() {
      return Boolean(this.activeTab?.toolsPanelOpen);
    },
    get toolsEnabledOverridden() {
      return Boolean(this.activeTab?.toolsEnabledOverridden);
    },
    get toolsCatalog() {
      return this.activeTab?.toolsCatalog || [];
    },
    get toolsCatalogLoading() {
      return Boolean(this.activeTab?.toolsCatalogLoading);
    },
    get toolsCatalogError() {
      return this.activeTab?.toolsCatalogError || '';
    },
    get toolsEnabledCount() {
      const tab = this.activeTab;
      if (!tab) return 0;
      if (Array.isArray(tab.toolsEnabled)) return tab.toolsEnabled.length;
      return (tab.toolsCatalog || []).length;
    },
    get draftInput() {
      return this.activeTab?.input ?? '';
    },
    set draftInput(v) {
      if (this.activeTab) this.activeTab.input = v;
    },
    get allowlistText() {
      return this.activeTab?.allowlist ?? '';
    },
    set allowlistText(v) {
      if (this.activeTab) this.activeTab.allowlist = v;
      this.onAllowlistInput();
    },

    get modelOptions() {
      return this.modelsForTab(this.activeTab);
    },

    get pieFrac() {
      const tab = this.activeTab;
      if (!tab) return 0;
      const max = Number(tab.contextWindow) || 0;
      if (max <= 0) return 0;
      const used = Number(tab.tokensUsed) || 0;
      return Math.min(1, Math.max(0, used / max));
    },
    get pieDash() {
      const c = 2 * Math.PI * 15.5;
      const f = this.pieFrac;
      return `${(c * f).toFixed(2)} ${(c * (1 - f)).toFixed(2)}`;
    },
    get pieUsedK() {
      const used = Number(this.activeTab?.tokensUsed) || 0;
      if (used >= 1000) return `${(used / 1000).toFixed(used >= 10000 ? 0 : 1)}k`;
      return String(used || 0);
    },
    get contextTooltip() {
      const tab = this.activeTab;
      if (!tab) return 'context';
      const used = Number(tab.tokensUsed) || 0;
      const max = Number(tab.contextWindow) || 0;
      const pct = max ? Math.round((used / max) * 100) : 0;
      return `Context: ${used.toLocaleString()} / ${max.toLocaleString()} tokens (${pct}%)`;
    },

    renderMarkdown,
    renderUserMessage,
    /** Prefer cached HTML on the message; fall back to live render. */
    messageHtml(msg) {
      if (!msg) return '';
      if (msg.html) return msg.html;
      if (msg.kind === 'assistant' && msg.text && !msg.streaming) {
        msg.html = renderMarkdown(msg.text);
        return msg.html;
      }
      return '';
    },
    userMessageHtml(msg) {
      if (!msg) return '';
      if (msg.userHtml) return msg.userHtml;
      if (msg.kind === 'user' && msg.text) {
        msg.userHtml = renderUserMessage(msg.text);
        return msg.userHtml;
      }
      return '';
    },
    telemetryHtml(msg) {
      if (!msg) return '';
      if (msg.telemetryHtml) return msg.telemetryHtml;
      const h = this.formatTelemetryHtml(msg.telemetry, msg);
      if (h) msg.telemetryHtml = h;
      return h;
    },

    toolCallExplanation(msg) {
      return msg?.explanation || msg?.args?.explanation || '';
    },
    toolCallCode(msg) {
      if (!msg) return '';
      if (msg.args == null) return '';
      try {
        const a = { ...msg.args };
        delete a.explanation;
        return JSON.stringify(a, null, 2);
      } catch {
        return String(msg.args);
      }
    },

    /**
     * Speak an agent reply when the viz speaker toggle is on.
     * Dedupes by message id so stream finalize + seed don't double-play.
     */
    maybeSpeakAssistant(text, msgId) {
      const t = String(text || '').trim();
      if (!t || t === '(stopped)' || t.startsWith('Error:')) return;
      if (msgId) {
        this._spokenIds = this._spokenIds || new Set();
        if (this._spokenIds.has(msgId)) return;
        this._spokenIds.add(msgId);
        // Bound growth
        if (this._spokenIds.size > 200) {
          const first = this._spokenIds.values().next().value;
          this._spokenIds.delete(first);
        }
      }
      try {
        M.store('viz')?.speakText?.(t);
      } catch {
        /* viz not ready */
      }
    },

    fillSelect(el, items, selected) {
      if (!el) return;
      const cur = selected == null ? '' : String(selected);
      el.innerHTML = '';
      if (!items.length) {
        const o = document.createElement('option');
        o.value = '';
        o.textContent = '(none)';
        el.appendChild(o);
        return;
      }
      for (const item of items) {
        const value = typeof item === 'string' ? item : item.value;
        const label =
          typeof item === 'string' ? item : item.label || item.value;
        const o = document.createElement('option');
        o.value = value;
        o.textContent =
          String(label).length > 42
            ? `${String(label).slice(0, 20)}…${String(label).slice(-18)}`
            : label;
        if (value === cur) o.selected = true;
        el.appendChild(o);
      }
    },

    syncSelects() {
      queueMicrotask(() => {
        const agents = this.agents.map((a) => ({
          value: a.name,
          label: a.name,
        }));
        const models = this.modelOptions.map((m) => ({
          value: m,
          label: m,
        }));
        const agent = this.activeTab?.agent;
        const model = this.activeTab?.model;
        this.fillSelect(document.getElementById('viz-chat-agent'), agents, agent);
        this.fillSelect(document.getElementById('viz-chat-model'), models, model);
      });
    },

    async bootstrap() {
      try {
        const [healthR, agentsR, sessionsR] = await Promise.all([
          fetch('/api/health'),
          fetch('/api/agents'),
          fetch('/api/sessions'),
        ]);
        const health = healthR.ok ? await healthR.json() : {};
        const agentsRes = agentsR.ok ? await agentsR.json() : { agents: [] };
        const sessionsRes = sessionsR.ok
          ? await sessionsR.json()
          : { sessions: [] };
        this.projectRoot = health.projectRoot || '';
        this.agents = Array.isArray(agentsRes.agents) ? agentsRes.agents : [];
        this.history = sessionsRes.sessions || [];
        if (!this.agents.length) {
          console.warn(
            '[chat] no agents from',
            this.projectRoot,
            '— expected .angela/agents/*.coffee or Angela setDefaultAgent()',
          );
        }
        // Prefer first catalog agent when tab has none / stale name (e.g. "brain"
        // after only "default" remains, or empty blankTab).
        for (const tab of this.tabs || []) {
          this.applyAgentDefaults(tab);
        }
        this.syncSelects();
      } catch (err) {
        console.error('[chat] bootstrap failed', err);
      }
    },

    applyAgentDefaults(tab) {
      if (!tab || !this.agents.length) return;
      let a = this.agents.find((x) => x.name === tab.agent);
      if (!a) {
        // No stored selection / unknown name → first catalog entry.
        // listAgents already hides virtual "default" when any disk agent exists,
        // so a single real agent becomes the automatic selection.
        a = this.agents[0];
        tab.agent = a.name;
      }
      const opts = this.modelsForTab(tab);
      // Empty tab, or model not available for this agent → use remembered pick
      if (!tab.model || (opts.length && !opts.includes(tab.model))) {
        tab.model = this.resolveModelForTab(tab, a);
      }
      this.applyModelContextWindow(tab);
      if (!tab.allowlistInited || !tab.allowlistOverridden) {
        tab.allowlistInited = true;
        this.loadAgentAllowlist(tab);
      }
      if (!tab.toolsInited || !tab.toolsEnabledOverridden) {
        tab.toolsInited = true;
        this.loadAgentToolsEnabled(tab);
      }
    },

    applyModelContextWindow(tab) {
      const a = this.agents.find((x) => x.name === tab.agent);
      const model = tab.model;
      if (a?.contextWindows && model && a.contextWindows[model] != null) {
        tab.contextWindow = Number(a.contextWindows[model]);
        return;
      }
      if (a?.contextWindow) {
        tab.contextWindow = a.contextWindow;
        return;
      }
      if (model) {
        fetch(`/api/context-window?model=${encodeURIComponent(model)}`)
          .then((r) => r.json())
          .then((j) => {
            if (j.contextWindow && tab.model === model) {
              tab.contextWindow = j.contextWindow;
            }
          })
          .catch(() => {});
      }
    },

    loadAgentAllowlist(tab) {
      const a = this.agents.find((x) => x.name === tab.agent);
      const text = a?.allowlist || '';
      tab.allowlistBaseline = text;
      tab.allowlist = text;
      tab.allowlistOverridden = false;
    },

    /**
     * Apply agent coffee toolsEnabled default (null = all tools).
     * Does not fetch catalog; that happens when the wrench panel opens.
     */
    loadAgentToolsEnabled(tab) {
      const a = this.agents.find((x) => x.name === tab.agent);
      const te = a?.toolsEnabled ?? null;
      tab.toolsEnabledBaseline =
        te == null ? null : Array.isArray(te) ? [...te] : null;
      tab.toolsEnabled =
        te == null ? null : Array.isArray(te) ? [...te] : null;
      tab.toolsEnabledOverridden = false;
      tab.toolsCatalog = [];
      tab.toolsCatalogError = '';
    },

    /** Whether a catalog tool name is currently enabled for the active tab. */
    isToolEnabled(name) {
      const tab = this.activeTab;
      if (!tab) return true;
      if (tab.toolsEnabled == null) return true;
      return tab.toolsEnabled.includes(name);
    },

    onAgentChange(ev) {
      const tab = this.activeTab;
      if (!tab) return;
      const sel = ev?.target?.value;
      if (sel) tab.agent = sel;
      const a = this.agents.find((x) => x.name === tab.agent);
      if (a) {
        // Keep preferred model when the new agent supports it
        tab.model = this.resolveModelForTab(tab, a);
        this.applyModelContextWindow(tab);
        this.loadAgentAllowlist(tab);
        tab.allowlistInited = true;
        this.loadAgentToolsEnabled(tab);
        tab.toolsInited = true;
      }
      tab.sessionId = null;
      this.syncSelects();
    },

    onModelChange(ev) {
      const tab = this.activeTab;
      if (!tab) return;
      const sel = ev?.target?.value;
      if (sel) {
        tab.model = sel;
        this.savePreferredModel(sel);
      }
      this.applyModelContextWindow(tab);
      tab.sessionId = null;
      this.syncSelects();
    },

    onAllowlistInput() {
      const tab = this.activeTab;
      if (!tab) return;
      tab.allowlistOverridden =
        (tab.allowlist ?? '') !== (tab.allowlistBaseline ?? '');
    },

    toggleAllowlist() {
      const tab = this.activeTab;
      if (!tab) return;
      tab.allowlistOpen = !tab.allowlistOpen;
      if (tab.allowlistOpen) tab.toolsPanelOpen = false;
    },

    async onAllowlistBlur() {
      const tab = this.activeTab;
      if (!tab) return;
      this.onAllowlistInput();
      try {
        await fetch('/api/allowlist', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tabId: tab.id,
            sessionId: tab.sessionId || null,
            allowlist: tab.allowlist ?? '',
            overridden: Boolean(tab.allowlistOverridden),
          }),
        });
      } catch (err) {
        console.error('allowlist save failed', err);
      }
    },

    async toggleToolsPanel() {
      const tab = this.activeTab;
      if (!tab) return;
      tab.toolsPanelOpen = !tab.toolsPanelOpen;
      if (tab.toolsPanelOpen) {
        tab.allowlistOpen = false;
        await this.ensureToolsCatalog(tab);
      }
    },

    /**
     * Load MCP + builtin tool catalog for the tab's agent (cached per open).
     * Marks each row checked from toolsEnabled (null = all checked).
     */
    async ensureToolsCatalog(tab) {
      if (!tab) return;
      if (tab.toolsCatalogLoading) return;
      tab.toolsCatalogLoading = true;
      tab.toolsCatalogError = '';
      try {
        const q = new URLSearchParams({
          agent: tab.agent || this.agents[0]?.name || 'default',
        });
        if (tab.id != null) q.set('tabId', String(tab.id));
        const res = await fetch(`/api/tools?${q}`);
        const data = await res.json().catch(() => ({}));
        if (!res.ok) {
          throw new Error(data.error || res.statusText || 'tools fetch failed');
        }
        const tools = Array.isArray(data.tools) ? data.tools : [];
        tab.toolsCatalog = tools.map((t) => ({
          name: t.name,
          description: t.description || '',
        }));
        // If still on agent default and toolsEnabled is null, leave as null (all).
        // If agent defined a list, keep it. If UI already overrode, keep selection
        // but drop names that no longer exist.
        if (tab.toolsEnabled != null) {
          const known = new Set(tab.toolsCatalog.map((t) => t.name));
          tab.toolsEnabled = tab.toolsEnabled.filter((n) => known.has(n));
        } else if (
          !tab.toolsEnabledOverridden &&
          Array.isArray(data.toolsEnabled)
        ) {
          // Server reported agent default list
          tab.toolsEnabledBaseline = [...data.toolsEnabled];
          tab.toolsEnabled = [...data.toolsEnabled];
        }
        if (data.error && !tools.length) {
          tab.toolsCatalogError = String(data.error);
        }
      } catch (err) {
        console.error('tools catalog failed', err);
        tab.toolsCatalogError = err?.message || String(err);
      } finally {
        tab.toolsCatalogLoading = false;
      }
    },

    async setToolEnabled(name, checked) {
      const tab = this.activeTab;
      if (!tab || !name) return;
      const catalog = tab.toolsCatalog || [];
      const allNames = catalog.map((t) => t.name);
      let next;
      if (tab.toolsEnabled == null) {
        // Was "all tools" — materialize full list then toggle
        next = allNames.filter((n) => (n === name ? checked : true));
      } else {
        const set = new Set(tab.toolsEnabled);
        if (checked) set.add(name);
        else set.delete(name);
        next = allNames.filter((n) => set.has(n));
      }
      tab.toolsEnabled = next;
      tab.toolsEnabledOverridden = this._toolsEnabledDiffersFromBaseline(tab);
      await this.persistToolsEnabled(tab);
    },

    async enableAllTools() {
      const tab = this.activeTab;
      if (!tab) return;
      tab.toolsEnabled = (tab.toolsCatalog || []).map((t) => t.name);
      tab.toolsEnabledOverridden = this._toolsEnabledDiffersFromBaseline(tab);
      await this.persistToolsEnabled(tab);
    },

    async disableAllTools() {
      const tab = this.activeTab;
      if (!tab) return;
      tab.toolsEnabled = [];
      tab.toolsEnabledOverridden = this._toolsEnabledDiffersFromBaseline(tab);
      await this.persistToolsEnabled(tab);
    },

    async resetToolsEnabled() {
      const tab = this.activeTab;
      if (!tab) return;
      const base = tab.toolsEnabledBaseline;
      tab.toolsEnabled =
        base == null ? null : Array.isArray(base) ? [...base] : null;
      tab.toolsEnabledOverridden = false;
      await this.persistToolsEnabled(tab);
    },

    _toolsEnabledDiffersFromBaseline(tab) {
      const base = tab.toolsEnabledBaseline;
      const cur = tab.toolsEnabled;
      if (base == null && cur == null) return false;
      if (base == null && cur != null) {
        // null baseline = all tools; equal if cur is every catalog name
        const all = (tab.toolsCatalog || []).map((t) => t.name).sort();
        const sorted = [...cur].sort();
        return (
          all.length !== sorted.length ||
          all.some((n, i) => n !== sorted[i])
        );
      }
      if (base != null && cur == null) return true;
      const a = [...(base || [])].sort();
      const b = [...(cur || [])].sort();
      return a.length !== b.length || a.some((n, i) => n !== b[i]);
    },

    async persistToolsEnabled(tab) {
      if (!tab) return;
      try {
        await fetch('/api/tools-enabled', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tabId: tab.id,
            sessionId: tab.sessionId || null,
            toolsEnabled: tab.toolsEnabled,
            overridden: Boolean(tab.toolsEnabledOverridden),
          }),
        });
      } catch (err) {
        console.error('toolsEnabled save failed', err);
      }
    },

    /**
     * Create a tab if none exist. Returns the active tab.
     * @returns {object|null}
     */
    ensureTab() {
      if (this.tabs?.length) return this.activeTab;
      this.newTab();
      return this.activeTab;
    },

    newTab() {
      const id = this.nextTabId++;
      const tab = this.blankTab(id);
      this.tabs.push(tab);
      this.activeTabId = id;
      this.applyAgentDefaults(tab);
      this.syncSelects();
      syncDraftEditor(this);
      this.focusDraft();
      // Reveal the sidebar when a session is opened
      try {
        const viz = M.store('viz');
        if (viz) viz.collapsed = false;
      } catch {
        /* ignore */
      }
    },

    selectTab(id) {
      this.activeTabId = id;
      this.applyAgentDefaults(this.activeTab);
      this.activeSessionId = this.activeTab?.sessionId ?? null;
      this.syncSelects();
      this.stickToBottom = true;
      syncDraftEditor(this);
      queueMicrotask(() => this.scrollBottom(true));
      this.focusDraft();
    },

    async closeTab(id) {
      const idx = this.tabs.findIndex((t) => t.id === id);
      if (idx < 0) return;
      const tab = this.tabs[idx];
      if (tab.waiting) await this.stop({ soft: true });
      if (tab.sessionId) {
        try {
          await fetch(`/api/sessions/${encodeURIComponent(tab.sessionId)}`, {
            method: 'DELETE',
          });
        } catch {
          /* ignore */
        }
      }
      this.tabs.splice(idx, 1);
      if (!this.tabs.length) {
        this.activeTabId = null;
        this.syncSelects();
        return;
      }
      if (this.activeTabId === id) {
        const next = this.tabs[Math.max(0, idx - 1)] || this.tabs[0];
        this.activeTabId = next.id;
      }
      this.syncSelects();
      syncDraftEditor(this);
    },

    onKeydown(ev) {
      // Legacy — key handling lives on the mention contenteditable now.
      if (window.__CHAT_MENTION__?.isMenuOpen?.()) return;
      if (ev.key === 'Enter' && !ev.shiftKey) {
        ev.preventDefault();
        void this.send();
      }
    },

    focusDraft() {
      queueMicrotask(() => {
        const api = window.__CHAT_MENTION__;
        if (api?.focus) {
          api.focus();
          return;
        }
        const el = document.getElementById('viz-chat-draft');
        if (el && typeof el.focus === 'function') el.focus();
      });
    },

    elMessages() {
      return document.getElementById('viz-chat-messages');
    },

    onMessagesScroll() {
      if (this._scrollProgrammatic) return;
      const el = this.elMessages();
      if (!el) return;
      const dist = el.scrollHeight - el.scrollTop - el.clientHeight;
      this.stickToBottom = dist <= this.stickThresholdPx;
    },

    scrollBottom(force = false) {
      const el = this.elMessages();
      if (!el) return;
      if (!force && !this.stickToBottom) return;
      this._scrollProgrammatic = true;
      el.scrollTop = el.scrollHeight;
      requestAnimationFrame(() => {
        this._scrollProgrammatic = false;
      });
    },

    scheduleScrollBottom() {
      requestAnimationFrame(() => {
        this.scrollBottom();
        // Markdown bubbles may have just painted entity links — names + selection
        try {
          void M.store('viz')?.refreshEntityLabels?.();
        } catch {
          /* ignore */
        }
      });
    },

    ensureMessagesPinned() {
      const el = this.elMessages();
      if (!el || el.dataset.pinBound === '1') return;
      el.dataset.pinBound = '1';
      // MutationObserver keeps stick-to-bottom while streaming
      if (typeof MutationObserver !== 'undefined') {
        this._msgObserver?.disconnect?.();
        this._msgObserver = new MutationObserver(() => {
          if (this.stickToBottom) this.scheduleScrollBottom();
        });
        this._msgObserver.observe(el, {
          childList: true,
          subtree: true,
          characterData: true,
        });
      }
    },

    async stop({ soft = false } = {}) {
      const tab = this.activeTab;
      if (!tab) return;
      try {
        tab._chatAbort?.abort();
      } catch {
        /* ignore */
      }
      try {
        await fetch('/api/abort', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ tabId: tab.id }),
        });
      } catch {
        /* ignore */
      }
      if (!soft) {
        tab.waiting = false;
        // User stop: freeze elapsed at cancel time
        this.stopStopwatch({ freeze: true });
      }
    },

    async decideApproval(msg, decision) {
      const tab = this.activeTab;
      if (!msg?.approvalId) return;
      try {
        await fetch('/api/approve', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            id: msg.approvalId,
            decision,
            tabId: tab?.id,
          }),
        });
        msg.status = decision === 'allow' || decision === 'always' ? 'allowed' : 'denied';
      } catch (err) {
        console.error('approve failed', err);
      }
    },

    _escHtml(s) {
      return String(s ?? '')
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
    },

    _metricChip(icon, text) {
      return (
        `<span class="metric-chip">` +
        `<i class="ph ${icon}" aria-hidden="true"></i>` +
        `<span>${this._escHtml(text)}</span>` +
        `</span>`
      );
    },

    /** HTML metrics row with Phosphor icons (message gutter). */
    formatTelemetryHtml(t, msg) {
      const chips = [];
      if (t && typeof t === 'object') {
        if (t.tok_per_sec != null && Number.isFinite(Number(t.tok_per_sec))) {
          chips.push(
            this._metricChip(
              'ph-gauge',
              `${Number(t.tok_per_sec).toFixed(2)} tps`,
            ),
          );
        }
        if (t.completion_tokens != null && Number(t.completion_tokens) > 0) {
          chips.push(
            this._metricChip('ph-text-aa', `${t.completion_tokens} tok`),
          );
        }
        if (t.ttft_ms != null && Number.isFinite(Number(t.ttft_ms))) {
          chips.push(
            this._metricChip(
              'ph-timer',
              `${(Number(t.ttft_ms) / 1000).toFixed(2)}s ttft`,
            ),
          );
        } else if (
          t.duration_ms != null &&
          Number(t.duration_ms) > 0 &&
          t.completion_tokens
        ) {
          chips.push(
            this._metricChip(
              'ph-timer',
              `${(Number(t.duration_ms) / 1000).toFixed(2)}s`,
            ),
          );
        }
        const reason =
          t.finish_reason || t.stop_reason || (t.error ? 'error' : null);
        if (reason) {
          chips.push(this._metricChip('ph-flag-banner', `finish: ${reason}`));
        }
        if (t.error) {
          chips.push(
            this._metricChip('ph-warning', String(t.error).slice(0, 40)),
          );
        }
      }
      if (msg?.kind === 'tool_result' && msg.full_chars) {
        chips.push(
          this._metricChip(
            'ph-cloud-arrow-down',
            `${Number(msg.full_chars).toLocaleString()} B${msg.truncated ? ' (trunc)' : ''}`,
          ),
        );
      }
      return chips.length ? chips.join('') : '';
    },

    /**
     * tool_call ↔ tool_result pairs (FIFO per name) so eye-toggle hides both.
     * @returns {Map<object, object>}
     */
    toolPairPartnerByKey(messages) {
      /** @type {Map<string, object[]>} */
      const pending = new Map();
      /** @type {Map<object, object>} */
      const partnerOf = new Map();
      for (const m of messages || []) {
        if (m.kind === 'tool_call' && m.name) {
          if (!pending.has(m.name)) pending.set(m.name, []);
          pending.get(m.name).push(m);
        } else if (m.kind === 'tool_result' && m.name) {
          const q = pending.get(m.name);
          if (q?.length) {
            const call = q.shift();
            partnerOf.set(call, m);
            partnerOf.set(m, call);
          }
        }
      }
      return partnerOf;
    },

    visibilityGroup(msg) {
      if (!msg) return [];
      if (msg.kind !== 'tool_call' && msg.kind !== 'tool_result') return [msg];
      const tab = this.activeTab;
      const partners = this.toolPairPartnerByKey(tab?.messages);
      const other = partners.get(msg);
      return other ? [msg, other] : [msg];
    },

    async toggleMsgVisible(msg) {
      if (!msg) return;
      const next = msg.visible === false;
      const group = this.visibilityGroup(msg);
      for (const m of group) m.visible = next;

      const sessionId = this.activeTab?.sessionId;
      const eventId = msg.eventId || null;
      if (!sessionId || !eventId) return; // local-only until durable id arrives

      try {
        const res = await fetch('/api/event/meta', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            sessionId,
            eventId,
            visible: next,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
        const j = await res.json().catch(() => ({}));
        if (Array.isArray(j.eventIds) && j.eventIds.length) {
          const idSet = new Set(j.eventIds);
          for (const m of this.activeTab?.messages || []) {
            if (m.eventId && idSet.has(m.eventId)) m.visible = next;
          }
        }
      } catch (err) {
        console.error('[chat] visibility toggle failed', err);
        for (const m of group) m.visible = !next;
      }
    },

    ensureGoto(msg) {
      if (!msg) return { enabled: false, label: '' };
      if (!msg.goto || typeof msg.goto !== 'object') {
        msg.goto = { enabled: false, label: '' };
      }
      if (msg.goto.label == null) msg.goto.label = '';
      if (msg.goto.enabled == null) msg.goto.enabled = false;
      return msg.goto;
    },

    isGotoLabelDuplicate(msg) {
      if (!msg?.goto?.enabled) return false;
      const want = String(msg.goto.label || '')
        .trim()
        .toLowerCase();
      if (!want) return false;
      for (const m of this.activeTab?.messages || []) {
        if (m === msg) continue;
        if (!m.goto?.enabled) continue;
        const lab = String(m.goto.label || '')
          .trim()
          .toLowerCase();
        if (lab && lab === want) return true;
      }
      return false;
    },

    async toggleMsgGoto(msg) {
      if (!msg) return;
      const g = this.ensureGoto(msg);
      g.enabled = !g.enabled;
      if (!g.enabled) {
        await this.persistGoto(msg);
        return;
      }
      if (g.label.trim() && !this.isGotoLabelDuplicate(msg)) {
        await this.persistGoto(msg);
      }
    },

    onGotoLabelInput(msg, ev) {
      if (!msg) return;
      const g = this.ensureGoto(msg);
      g.label = ev?.target?.value ?? g.label ?? '';
      if (msg._gotoTimer) clearTimeout(msg._gotoTimer);
      msg._gotoTimer = setTimeout(() => {
        msg._gotoTimer = null;
        void this.persistGoto(msg);
      }, 1000);
    },

    async persistGoto(msg) {
      if (!msg) return;
      const g = this.ensureGoto(msg);
      if (g.enabled && this.isGotoLabelDuplicate(msg)) return;
      const sessionId = this.activeTab?.sessionId;
      const eventId = msg.eventId || null;
      if (!sessionId || !eventId) return;
      try {
        const res = await fetch('/api/event/meta', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            sessionId,
            eventId,
            goto: {
              enabled: Boolean(g.enabled),
              label: String(g.label || '').trim(),
            },
          }),
        });
        if (res.status === 409) return;
        if (!res.ok) throw new Error(await res.text());
        const j = await res.json().catch(() => ({}));
        if (j.goto) {
          msg.goto = {
            enabled: Boolean(j.goto.enabled),
            label: String(j.goto.label || ''),
          };
        }
      } catch (err) {
        console.error('[chat] goto meta save failed', err);
      }
    },

    /**
     * Build selection context from viz store for the agent prompt.
     * Full entity YAML when toggle is on (same shape as think/ontology).
     */
    async selectionContext() {
      try {
        const viz = M.store('viz');
        const slugs = viz?.selectedSlugs || viz?.routeSlugs || [];
        if (!slugs.length || !viz?.useSelection) return '';
        const list = slugs.slice(0, 24);
        const notice = `NOTICE: In the app, I have selected ${list.length} ${list.length === 1 ? 'entity' : 'entities'}.`;
        const res = await (
          await fetch(
            '/entity-context?slugs=' +
              list.map(encodeURIComponent).join(',') +
              '&tag=selected-entities&notice=' +
              encodeURIComponent(notice),
          )
        ).json();
        return res.text || '';
      } catch {
        return '';
      }
    },

    /**
     * Preload entities referenced via [[wiki-links]] in the user prompt.
     * @param {string} text
     * @param {string[]} [excludeSlugs] — e.g. already in selection
     */
    async referencedContext(text, excludeSlugs = []) {
      try {
        const exclude = new Set((excludeSlugs || []).filter(Boolean));
        const slugs = extractWikiSlugs(text)
          .filter((s) => !exclude.has(s))
          .slice(0, 32);
        if (!slugs.length) return '';
        const notice = `NOTICE: The user prompt references ${slugs.length} ${slugs.length === 1 ? 'entity' : 'entities'} via wiki-links (preloaded — prefer these over re-fetching).`;
        const res = await (
          await fetch(
            '/entity-context?slugs=' +
              slugs.map(encodeURIComponent).join(',') +
              '&tag=referenced-entities&notice=' +
              encodeURIComponent(notice),
          )
        ).json();
        return res.text || '';
      } catch {
        return '';
      }
    },

    async send(textOverride) {
      const tab = this.ensureTab();
      if (!tab) return;
      // Prefer live contenteditable serialization (wiki-links for pills)
      let raw =
        textOverride != null ? String(textOverride) : tab.input || '';
      if (textOverride == null) {
        try {
          const api = window.__CHAT_MENTION__;
          if (api?.getText) raw = api.getText();
          else {
            const el = document.getElementById('viz-chat-draft');
            if (el?.isContentEditable) raw = serializeEditor(el);
          }
        } catch {
          /* use tab.input */
        }
      }
      const content = raw.trim();
      if (!content) return;

      if (tab.waiting) await this.stop({ soft: true });

      // Expand the viz sidebar if it was tucked away
      try {
        const viz = M.store('viz');
        if (viz) viz.collapsed = false;
      } catch {
        /* ignore */
      }

      tab.input = '';
      try {
        window.__CHAT_MENTION__?.clear?.();
      } catch {
        /* ignore */
      }
      tab.isNew = false;
      tab.waiting = true;
      // Reset stopwatch for this user turn; ticks until stream done / stop
      this.startStopwatch();
      const chatGen = (tab._chatGen = (tab._chatGen || 0) + 1);
      try {
        tab._chatAbort?.abort();
      } catch {
        /* ignore */
      }
      tab._chatAbort = new AbortController();
      const chatSignal = tab._chatAbort.signal;

      tab.messages.push({
        id: crypto.randomUUID(),
        kind: 'user',
        text: content,
        visible: true,
        eventId: null,
        telemetry: null,
        goto: { enabled: false, label: '' },
      });
      for (const m of tab.messages) {
        if (m.streaming) m.streaming = false;
      }
      this.stickToBottom = true;
      const forceNew = !tab.sessionId;
      this.focusDraft();
      this.scheduleScrollBottom();

      try {
        const body = {
          tabId: tab.id,
          content,
          agent: tab.agent,
          model: tab.model,
          newSession: forceNew,
          sessionId: forceNew ? null : tab.sessionId,
        };
        if (tab.allowlistOverridden) {
          body.allowlist = tab.allowlist ?? '';
          body.allowlistOverridden = true;
        }
        if (tab.toolsEnabledOverridden) {
          body.toolsEnabled = tab.toolsEnabled ?? [];
          body.toolsEnabledOverridden = true;
        }
        // Selected entities (toggle) + wiki-referenced entities in the prompt
        try {
          const viz = M.store('viz');
          const selSlugs =
            viz?.useSelection
              ? viz?.selectedSlugs || viz?.routeSlugs || []
              : [];
          const [sel, ref] = await Promise.all([
            this.selectionContext(),
            this.referencedContext(content, selSlugs),
          ]);
          if (sel) body.selectionContext = sel;
          if (ref) body.referencedContext = ref;
        } catch {
          /* context preload is best-effort */
        }

        const res = await fetch('/api/chat', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
          signal: chatSignal,
        });

        if (!res.ok || !res.body) {
          const errText = await res.text();
          if (tab._chatGen === chatGen) {
            tab.messages.push({
              id: crypto.randomUUID(),
              kind: 'assistant',
              text: `Error: ${errText || res.statusText}`,
              visible: true,
            });
          }
          return;
        }

        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buf = '';
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          let nl;
          while ((nl = buf.indexOf('\n')) !== -1) {
            const line = buf.slice(0, nl).trim();
            buf = buf.slice(nl + 1);
            if (!line) continue;
            let msg;
            try {
              msg = JSON.parse(line);
            } catch {
              continue;
            }
            if (tab._chatGen !== chatGen) return;
            this.handleStreamMsg(tab, msg);
            this.scheduleScrollBottom();
          }
        }
      } catch (err) {
        const aborted =
          err?.name === 'AbortError' ||
          /aborted|user stop|interrupt/i.test(String(err?.message || ''));
        if (!aborted && tab._chatGen === chatGen) {
          tab.messages.push({
            id: crypto.randomUUID(),
            kind: 'assistant',
            text: `Network error: ${err.message}`,
            visible: true,
          });
        }
      } finally {
        if (tab._chatGen === chatGen) {
          tab.waiting = false;
          // Finish reason / stream end: freeze final elapsed (still visible)
          this.stopStopwatch({ freeze: true });
          // Transcript changed — drop cache so next history open is fresh
          if (tab.sessionId) invalidateSessionCache(tab.sessionId);
          // Freeze rendered HTML on finished assistant bubbles
          for (const m of tab.messages || []) {
            if (m.kind === 'assistant' && m.text && !m.streaming && !m.html) {
              m.html = renderMarkdown(m.text);
            }
            if (m.kind === 'user' && m.text && !m.userHtml) {
              m.userHtml = renderUserMessage(m.text);
            }
          }
          this.scheduleScrollBottom();
          this.focusDraft();
          void this.refreshHistory();
        }
      }
    },

    async refreshHistory() {
      try {
        const r = await fetch('/api/sessions');
        const j = await r.json();
        this.history = j.sessions || [];
        if (j.projectRoot) this.projectRoot = j.projectRoot;
      } catch {
        /* ignore */
      }
    },

    toggleHistory() {
      this.historyOpen = !this.historyOpen;
      if (this.historyOpen) void this.refreshHistory();
    },

    shortModel(model) {
      if (!model) return '';
      const s = String(model);
      const i = s.lastIndexOf('/');
      return i >= 0 ? s.slice(i + 1) : s.length > 28 ? s.slice(0, 26) + '…' : s;
    },

    /**
     * Apply session payload onto the active tab (shared by cache + network).
     */
    applySessionToTab(sessionId, meta, messages) {
      const tab = this.ensureTab();
      if (!tab) return null;
      tab.messages = decorateMessages(messages || []);
      tab.isNew = tab.messages.length === 0;
      tab.sessionId = sessionId;
      tab.waiting = false;
      tab.agent = meta.agent || tab.agent;
      tab.model = meta.model || tab.model;
      if (meta.contextWindow) tab.contextWindow = meta.contextWindow;
      if (meta.lastPromptTokens != null) {
        tab.tokensUsed = Number(meta.lastPromptTokens);
        tab.tokensFromProvider = true;
        tab.lastTotalTokens =
          Number(meta.lastTotalTokens) || tab.tokensUsed;
      } else {
        tab.tokensUsed = 0;
        tab.tokensFromProvider = false;
      }
      tab.name = String(meta.title || sessionId).slice(0, 24);
      this.activeSessionId = sessionId;
      this.stickToBottom = true;
      this.syncSelects();
      syncDraftEditor(this);
      queueMicrotask(() => {
        this.ensureMessagesPinned();
        this.scrollBottom(true);
        // Defer label hydration — don't block first paint on /labels batch
        setTimeout(() => {
          try {
            void M.store('viz')?.refreshEntityLabels?.();
          } catch {
            /* ignore */
          }
        }, 0);
      });
      return tab;
    },

    /**
     * Load a persisted session into the active tab (resume).
     * Uses an in-memory cache so flipping history items is O(paint), not
     * re-download + jsonl parse + marked over the whole transcript.
     * @param {string} sessionId
     */
    async openSession(sessionId) {
      if (!sessionId) return;
      const id = String(sessionId);
      try {
        // Fast path: already reconstructed this session in this page life
        const hit = sessionCache.get(id);
        if (hit) {
          this.applySessionToTab(id, hit.session, hit.messages);
          return;
        }

        const r = await fetch(`/api/sessions/${encodeURIComponent(id)}`);
        if (!r.ok) return;
        const j = await r.json();
        const meta = j.session || {};
        const messages = Array.isArray(j.messages) ? j.messages : [];
        // Decorate once, cache, then paint
        const decorated = decorateMessages(messages);
        cacheSession(id, meta, decorated);
        this.applySessionToTab(id, meta, decorated);
      } catch (err) {
        console.error('[chat] openSession failed', err);
      }
    },

    async deleteSession(id) {
      if (!id) return;
      if (!confirm('Delete this session?')) return;
      try {
        await fetch(`/api/sessions/${encodeURIComponent(id)}`, {
          method: 'DELETE',
        });
        invalidateSessionCache(id);
        if (this.activeSessionId === id) this.activeSessionId = null;
        for (const t of this.tabs || []) {
          if (t.sessionId === id) {
            t.sessionId = null;
            t.messages = [];
            t.isNew = true;
            t.name = `Chat ${t.id}`;
          }
        }
        await this.refreshHistory();
        syncDraftEditor(this);
      } catch (err) {
        console.error('[chat] deleteSession failed', err);
      }
    },

    async cleanAllSessions() {
      if (!confirm('Delete ALL chat sessions under db/.angela/sessions?')) {
        return;
      }
      try {
        await fetch('/api/sessions', { method: 'DELETE' });
        sessionCache.clear();
        this.history = [];
        this.activeSessionId = null;
        for (const t of this.tabs || []) {
          t.sessionId = null;
        }
      } catch (err) {
        console.error('[chat] cleanAllSessions failed', err);
      }
    },

    upsertHistorySession(partial, tab) {
      const id = partial?.id || partial?.sessionId;
      if (!id) return;
      const firstUser = (tab?.messages || []).find((m) => m.kind === 'user');
      const titleFromMsg = firstUser?.text
        ? String(firstUser.text).slice(0, 64) +
          (String(firstUser.text).length > 64 ? '…' : '')
        : '';
      const row = {
        id: String(id),
        title: titleFromMsg || partial.title || tab?.name || String(id),
        agent: partial.agent || tab?.agent,
        model: partial.model || tab?.model,
        updatedAt: Date.now(),
      };
      const i = this.history.findIndex((h) => h.id === row.id);
      if (i >= 0) this.history[i] = { ...this.history[i], ...row };
      else this.history.unshift(row);
      this.activeSessionId = String(id);
    },

    _cloneTelemetry(t) {
      if (!t || typeof t !== 'object') return null;
      try {
        return structuredClone(t);
      } catch {
        return { ...t };
      }
    },

    /** Kick stick-to-bottom + a light tick so stream tokens paint every frame. */
    noteStreamPaint() {
      this.scheduleScrollBottom();
      if (this._streamRaf) return;
      this._streamRaf = requestAnimationFrame(() => {
        this._streamRaf = 0;
        this.streamTick = (this.streamTick || 0) + 1;
      });
    },

    /**
     * Append a streaming token into the last open assistant/reasoning bubble.
     * Mutates through the reactive array slot so m.js x-text re-renders live.
     */
    appendStreamDelta(tab, kind, chunk) {
      if (!chunk) return;
      let idx = -1;
      for (let i = tab.messages.length - 1; i >= 0; i--) {
        if (tab.messages[i].kind === kind && tab.messages[i].streaming) {
          idx = i;
          break;
        }
      }
      if (idx < 0) {
        tab.messages.push({
          id: crypto.randomUUID(),
          kind,
          text: chunk,
          streaming: true,
          visible: true,
          telemetry: null,
          eventId: null,
          goto: { enabled: false, label: '' },
        });
      } else {
        // Read/write via index so set trap fires on the reactive proxy target.
        const cur = tab.messages[idx];
        cur.text = (cur.text || '') + chunk;
      }
      this.noteStreamPaint();
    },

    /**
     * Agent mutation tools: stash args on tool_call (for diagnostics).
     * Entity cache updates arrive as NDJSON `entity_changed` (full snapshot
     * from the server) — views re-render via entity-model subscribers.
     * @param {{ kind?: string, name?: string, args?: object, ok?: boolean, denied?: boolean, text?: string }} card
     */
    maybeRefreshInspectorAfterTool(card) {
      if (!card?.name) return;
      const name = String(card.name);
      const isMut =
        /(?:^|__)(?:put_entity|delete_entity|method_invoke)$/i.test(name);
      if (!isMut) return;
      if (card.kind === 'tool_call') {
        this._pendingToolMut = { name, args: card.args || {} };
        return;
      }
      if (card.kind === 'tool_result') {
        this._pendingToolMut = null;
      }
      // Prefer server entity_changed event (see handleStreamMsg). No ad-hoc fetch.
    },

    /**
     * Push an agent/server entity mutation into the SPA entity-model.
     * Inspector / labels re-render via model subscribers (no direct DOM fetch).
     * @param {{ slug?: string, entity?: object, deleted?: boolean, stale?: boolean }} msg
     */
    async applyEntityMutation(msg) {
      if (!msg?.slug) return;
      try {
        const { applyServerChange } = await import('/entity-model.js');
        await applyServerChange(msg);
      } catch (err) {
        console.warn('[chat] entity-model apply failed', err);
      }
    },

    handleStreamMsg(tab, msg) {
      if (msg.type === 'entity_changed') {
        void this.applyEntityMutation(msg);
        return;
      }
      if (msg.type === 'session') {
        if (msg.sessionId) {
          tab.sessionId = msg.sessionId;
          this.upsertHistorySession(
            { id: msg.sessionId, agent: msg.agent, model: msg.model },
            tab,
          );
        }
        if (msg.model) tab.model = msg.model;
        if (msg.contextWindow) tab.contextWindow = msg.contextWindow;
        if (msg.tokensUsed != null && !tab.tokensFromProvider) {
          tab.tokensUsed = msg.tokensUsed;
        }
        if (msg.agent) tab.agent = msg.agent;
        if (msg.sessionId && tab.isNew === false) {
          const firstUser = (tab.messages || []).find((m) => m.kind === 'user');
          if (firstUser?.text) tab.name = String(firstUser.text).slice(0, 24);
        }
        this.syncSelects();
        return;
      }
      if (msg.type === 'assistant_delta') {
        this.appendStreamDelta(tab, 'assistant', msg.text || '');
        return;
      }
      if (msg.type === 'reasoning_delta') {
        this.appendStreamDelta(tab, 'reasoning', msg.text || '');
        return;
      }
      if (msg.type === 'usage' || msg.type === 'gen_info') {
        if (msg.prompt_tokens != null && msg.prompt_tokens !== '') {
          const prompt = Number(msg.prompt_tokens);
          if (Number.isFinite(prompt) && prompt >= 0) {
            tab.tokensUsed = prompt;
            tab.tokensFromProvider = true;
            const tot = Number(msg.total_tokens);
            tab.lastTotalTokens = Number.isFinite(tot) ? tot : prompt;
          }
        }
        if (msg.contextWindow) tab.contextWindow = msg.contextWindow;
        // Snapshot for the in-flight assistant bubble (gutter metrics)
        tab._lastGenInfo = this._cloneTelemetry({
          completion_tokens: msg.completion_tokens,
          prompt_tokens: msg.prompt_tokens,
          total_tokens: msg.total_tokens,
          tok_per_sec: msg.tok_per_sec,
          ttft_ms: msg.ttft_ms,
          duration_ms: msg.duration_ms,
          finish_reason: msg.finish_reason,
          stop_reason: msg.stop_reason,
          error: msg.error,
          model: msg.model,
          event_id: msg.event_id,
        });
        return;
      }
      if (msg.type === 'context_event') {
        if (msg.role === 'user') {
          for (let i = tab.messages.length - 1; i >= 0; i--) {
            if (tab.messages[i].kind === 'user' && !tab.messages[i].eventId) {
              tab.messages[i].eventId = msg.event_id;
              tab.messages[i].visible = msg.visible !== false;
              this.ensureGoto(tab.messages[i]);
              break;
            }
          }
          return;
        }
        if (msg.role === 'assistant') {
          for (let i = tab.messages.length - 1; i >= 0; i--) {
            if (tab.messages[i].kind === 'assistant') {
              if (!tab.messages[i].eventId && msg.event_id) {
                tab.messages[i].eventId = msg.event_id;
              }
              tab.messages[i].visible = msg.visible !== false;
              if (!tab.messages[i].telemetry && msg.telemetry) {
                tab.messages[i].telemetry = this._cloneTelemetry(msg.telemetry);
              }
              this.ensureGoto(tab.messages[i]);
              break;
            }
          }
        }
        return;
      }
      // Only unwrap known nested event types (avoid double-handling deltas).
      if (msg.type === 'event' && msg.event?.type) {
        const t = msg.event.type;
        if (
          t === 'usage' ||
          t === 'gen_info' ||
          t === 'context_event' ||
          t === 'assistant_delta' ||
          t === 'reasoning_delta'
        ) {
          this.handleStreamMsg(tab, { type: t, ...msg.event });
        }
        return;
      }
      if (msg.type === 'card' && msg.card) {
        const c = msg.card;
        const snap =
          this._cloneTelemetry(c.telemetry) ||
          this._cloneTelemetry(tab._lastGenInfo) ||
          null;
        if (c.kind === 'tool_call' || c.kind === 'tool_result') {
          for (let i = tab.messages.length - 1; i >= 0; i--) {
            if (
              (tab.messages[i].kind === 'assistant' ||
                tab.messages[i].kind === 'reasoning') &&
              tab.messages[i].streaming
            ) {
              tab.messages[i].streaming = false;
              if (!tab.messages[i].telemetry && snap) {
                tab.messages[i].telemetry = snap;
              }
            }
          }
          this.noteStreamPaint();
          // put_entity / delete_entity / … → refresh inspector if slug selected
          this.maybeRefreshInspectorAfterTool(c);
        }
        if (c.kind === 'approval_needed' && c.id) {
          const existing = tab.messages.find(
            (m) => m.kind === 'approval_needed' && m.approvalId === c.id,
          );
          if (existing) {
            Object.assign(existing, {
              name: c.name,
              command: c.command,
              args: c.args,
              status: c.status || 'pending',
            });
            return;
          }
          tab.messages.push({
            id: crypto.randomUUID(),
            kind: 'approval_needed',
            approvalId: c.id,
            name: c.name,
            command: c.command,
            args: c.args,
            status: 'pending',
            visible: true,
            goto: { enabled: false, label: '' },
          });
          return;
        }
        if (c.kind === 'assistant') {
          let live = null;
          for (let i = tab.messages.length - 1; i >= 0; i--) {
            if (
              tab.messages[i].kind === 'assistant' &&
              tab.messages[i].streaming
            ) {
              live = tab.messages[i];
              break;
            }
          }
          let finalText = c.text != null ? String(c.text) : '';
          if (
            finalText.trim().startsWith('{') &&
            /"choices"\s*:/.test(finalText) &&
            /"usage"\s*:/.test(finalText)
          ) {
            finalText = '';
          }
          if (live) {
            if (finalText) live.text = finalText;
            if (!(live.text || '').trim()) {
              const idx = tab.messages.indexOf(live);
              if (idx >= 0) tab.messages.splice(idx, 1);
            } else {
              live.streaming = false;
              live.visible = c.visible !== false;
              if (snap) live.telemetry = snap;
              if (c.eventId && !live.eventId) live.eventId = c.eventId;
              this.ensureGoto(live);
              this.maybeSpeakAssistant(live.text, live.id);
            }
            return;
          }
          if (!finalText.trim()) return;
          // Fall through to push a finalized assistant card, then speak below
        }
        if (c.kind === 'reasoning') {
          let live = null;
          for (let i = tab.messages.length - 1; i >= 0; i--) {
            if (
              tab.messages[i].kind === 'reasoning' &&
              tab.messages[i].streaming
            ) {
              live = tab.messages[i];
              break;
            }
          }
          if (live) {
            if (c.text) live.text = String(c.text);
            live.streaming = false;
            if (snap) live.telemetry = snap;
            this.ensureGoto(live);
            return;
          }
        }
        const row = {
          ...c,
          id: crypto.randomUUID(),
          visible: c.visible !== false,
          streaming: false,
          approvalId: c.id,
          telemetry: snap,
          eventId: c.eventId || null,
          goto: { enabled: false, label: '' },
        };
        tab.messages.push(row);
        this.noteStreamPaint();
        if (c.kind === 'assistant' && row.text) {
          this.maybeSpeakAssistant(row.text, row.id);
        }
        return;
      }
      if (msg.type === 'done') {
        // Seal any still-streaming assistant bubble and speak once
        const snap = this._cloneTelemetry(msg.gen_info) ||
          this._cloneTelemetry(tab._lastGenInfo);
        for (const m of tab.messages) {
          if (m.kind === 'assistant' && m.streaming) {
            m.streaming = false;
            if (!m.telemetry && snap) m.telemetry = snap;
            this.ensureGoto(m);
            if ((m.text || '').trim()) this.maybeSpeakAssistant(m.text, m.id);
          } else if (
            (m.kind === 'assistant' || m.kind === 'reasoning') &&
            !m.telemetry &&
            snap
          ) {
            m.telemetry = snap;
          }
        }
        this.noteStreamPaint();
        if (msg.prompt_tokens != null && msg.prompt_tokens !== '') {
          const p = Number(msg.prompt_tokens);
          if (Number.isFinite(p) && p >= 0) {
            tab.tokensUsed = p;
            tab.tokensFromProvider = true;
          }
        }
        if (msg.contextWindow) tab.contextWindow = msg.contextWindow;
        if (msg.aborted) {
          tab.messages.push({
            id: crypto.randomUUID(),
            kind: 'assistant',
            text: '(stopped)',
            visible: true,
          });
        }
        if (msg.error) {
          tab.messages.push({
            id: crypto.randomUUID(),
            kind: 'assistant',
            text: `Error: ${msg.error}`,
            visible: true,
          });
        }
        for (const m of tab.messages) {
          if (m.streaming) m.streaming = false;
        }
      }
    },

    /**
     * Seed / complete a one-shot think/ontology turn so the user can continue.
     * If run() already opened a tab with the user bubble (waiting=true), append
     * the assistant answer there — do not create a second tab or drop the reply.
     */
    seedFromQuery({ question, answer, mode }) {
      if (!question && !answer) return;
      let t = this.activeTab || this.ensureTab();
      if (!t) return;

      const lastUser = [...(t.messages || [])]
        .reverse()
        .find((m) => m.kind === 'user');
      const alreadyHasQuestion =
        question && lastUser && String(lastUser.text) === String(question);

      // Different conversation already on this tab → new tab (unless we were
      // waiting on this exact query, which alreadyHasQuestion covers).
      if (
        !alreadyHasQuestion &&
        t.messages?.length &&
        !t.isNew &&
        !t.waiting
      ) {
        this.newTab();
        t = this.activeTab;
        if (!t) return;
      }

      t.isNew = false;
      t.waiting = false;
      // One-shot think/ontology/search finished — freeze elapsed at answer time
      this.stopStopwatch({ freeze: true });
      try {
        const viz = M.store('viz');
        if (viz) viz.collapsed = false;
      } catch {
        /* ignore */
      }

      if (question && !alreadyHasQuestion) {
        t.messages.push({
          id: crypto.randomUUID(),
          kind: 'user',
          text: question,
          visible: true,
          telemetry: null,
          eventId: null,
          goto: { enabled: false, label: '' },
        });
        t.name = String(question).slice(0, 24);
      }
      if (answer) {
        const asst = {
          id: crypto.randomUUID(),
          kind: 'assistant',
          text: answer,
          visible: true,
          seeded: true,
          mode: mode || '',
          telemetry: null,
          eventId: null,
          goto: { enabled: false, label: '' },
        };
        t.messages.push(asst);
        this.maybeSpeakAssistant(answer, asst.id);
      }
      // Force new angela session on next send (seeded messages aren't on disk)
      t.sessionId = null;
      this.stickToBottom = true;
      this.scheduleScrollBottom();
    },

    /** Clear waiting on the active tab (cancel / error paths). */
    clearWaiting() {
      const t = this.activeTab;
      if (t) t.waiting = false;
      this.stopStopwatch({ freeze: true });
    },
  });
}

export function chat() {
  return M.store('chat');
}
