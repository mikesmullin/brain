/**
 * brain viz chat API — browser multi-session chat over angela (library).
 *
 * Mounted under /api/* by `brain viz`. Project root = the brain project root
 * (parent of db/, sibling of brain.yaml). Agents and session logs live in
 * `<project>/.angela/` — not under db/.
 *
 * Brain registers an Angela setDefaultAgent() factory (name "default") so
 * projects without .angela/agents/*.coffee still get a working agent. When any
 * disk agent exists, "default" is omitted from the UI catalog.
 *
 * POST /api/chat          → NDJSON stream
 * POST /api/approve
 * POST /api/abort
 * GET  /api/health
 * GET  /api/agents
 * GET  /api/sessions
 * DELETE /api/sessions/:id
 * DELETE /api/sessions
 * GET  /api/sessions/:id
 * POST /api/session/new
 * POST /api/allowlist
 * GET  /api/tools?agent=&tabId=
 * POST /api/tools-enabled
 * GET  /api/context-window
 * POST /api/event/meta
 */
import { resolve, dirname, join } from 'node:path';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  Angela,
  ApprovalQueue,
  DEFAULT_LUCY_ALLOWLIST,
  loadAgent,
  listAgents,
  SessionStore,
  resolveProjectRoot,
  ensureAngelaLayout,
  defaultMcpRoot,
  resolveContextWindow,
  resolveContextWindowAsync,
  normalizeToolsEnabled,
  setDefaultAgent,
} from 'angela';
import { buildOntologySystemPrompt } from './ontology.coffee';

const __dirname = dirname(fileURLToPath(import.meta.url));
const BRAIN_PKG = resolve(__dirname, '..');
/** Package-relative CLI entry (fallback when argv[1] is not brain.mjs). */
const BRAIN_BIN_PKG = join(BRAIN_PKG, 'bin', 'brain.mjs');

/**
 * Resolve the running `brain` CLI script path for MCP child processes.
 * Prefer process.argv[1] (same entry that launched this process); fall back to
 * this package's bin/brain.mjs next to the installed sources.
 *
 * @returns {string} absolute path to brain.mjs
 */
export function resolveBrainBin() {
  try {
    const argv1 = process.argv[1] ? resolve(String(process.argv[1])) : '';
    if (argv1 && existsSync(argv1)) {
      const base = argv1.split(/[/\\]/).pop() || '';
      // bun bin/brain.mjs  |  linked `brain` shim that ends in brain.mjs
      if (/^brain(\.mjs)?$/i.test(base) || base === 'brain') {
        return argv1;
      }
    }
  } catch {
    /* ignore */
  }
  return existsSync(BRAIN_BIN_PKG) ? BRAIN_BIN_PKG : BRAIN_BIN_PKG;
}

/**
 * Host default agent for Angela (name: "default").
 * Used when a brain project has no .angela/agents/*.coffee, or when a file
 * agent omits keys (allowlist / mcp / …). Not written to disk.
 *
 * System prompt is intentionally omitted here — built lazily from
 * ontology.coffee `buildOntologySystemPrompt` right before each chat turn
 * (see applyBrainSystemPrompt). That is the authoritative browser prompt.
 *
 * MCP `brain` server is always defined here with dynamic paths:
 *   - command/args → current runtime + resolveBrainBin()
 *   - cwd → active brain CLI cwd (same scope as `brain viz` / server RPC)
 *
 * @param {{ brainBin?: string, brainCwd?: string }} [cfg]
 */
export function createBrainDefaultAgent(cfg = {}) {
  const bin = resolve(
    cfg.brainBin || resolveBrainBin(),
  ).replaceAll('\\', '/');
  // Prefer explicit brainCwd (from viz CLI), else process.cwd() — the same
  // directory brain commands use for paths()/socket resolution.
  const cwd = resolve(
    cfg.brainCwd || process.cwd(),
  ).replaceAll('\\', '/');
  return {
    name: 'default',
    description:
      'Explore and edit the local knowledge graph via brain MCP tools',
    mcp: [
      {
        name: 'brain',
        command: process.execPath,
        args: [bin, 'mcp'],
        cwd,
      },
    ],
    // system: omitted — applyBrainSystemPrompt() before each session.run
    // Edit tools intentionally off allowlist → approval prompt
    allowlist: [
      'brain__search',
      'brain__search:.*',
      'brain__think',
      'brain__think:.*',
      'brain__ontology',
      'brain__ontology:.*',
      'brain__graph',
      'brain__graph:.*',
      'brain__graphql',
      'brain__graphql:.*',
      'brain__get_entity',
      'brain__get_entity:.*',
      'brain__schema_methods',
      'brain__schema_methods:.*',
      'brain__method_invoke',
      'brain__method_invoke:.*',
      'brain__schema_orphans',
      'brain__schema_orphans:.*',
    ].join('\n'),
    policyMode: 'ask',
    // starters omitted — welcome chips off until a project agent opts in
  };
}

/**
 * Lazy system prompt for viz chat — same identity as ontology.coffee, freeform
 * (no final_answer tool). Call immediately before session.run / Agent.factory.
 *
 * @param {{
 *   model?: string|null,
 *   thinking?: boolean,
 *   entCtx?: string,
 * }} [opts]
 * @returns {string}
 */
export function buildBrainChatSystemPrompt(opts = {}) {
  return buildOntologySystemPrompt({
    model: opts.model,
    thinking: opts.thinking,
    entCtx: opts.entCtx || '',
    freeform: true,
  });
}

/**
 * Apply authoritative ontology system prompt onto a live Angela harness.
 * Safe before first run (updates create config) or after (mutates agent).
 *
 * @param {import('angela').Angela} harness
 * @param {{
 *   model?: string|null,
 *   thinking?: boolean,
 *   reasoning_effort?: string|null,
 *   entCtx?: string,
 * }} [opts]
 */
export function applyBrainSystemPrompt(harness, opts = {}) {
  if (!harness) return;
  // Base ontology prompt (no <|think|>) — Angela applies think/effort augmentation.
  const system = buildBrainChatSystemPrompt(opts);
  if (typeof harness.setThink === 'function') {
    harness.setThink(Boolean(opts.thinking));
  }
  if (typeof harness.setReasoningEffort === 'function') {
    harness.setReasoningEffort(opts.reasoning_effort ?? opts.reasoningEffort ?? 'medium');
  }
  if (typeof harness.setSystemPrompt === 'function') {
    harness.setSystemPrompt(system);
  } else if (harness.session) {
    for (const id of harness.session.list?.() || []) {
      const s = harness.session.get?.(id);
      if (s?.agent) s.agent.system_prompt = system;
    }
  }
  return system;
}

/**
 * @param {{
 *   projectRoot: string,
 *   autoApprove?: boolean,
 *   mcpRoot?: string,
 *   brainCwd?: string,
 *   fetchEntity?: (slug: string) => Promise<object|null>,
 * }} opts
 * fetchEntity — optional; after put_entity/method_invoke, load a snapshot for
 * the SPA entity-model via NDJSON `entity_changed`.
 */
export function createChatApi(opts) {
  const PROJECT_ROOT = resolveProjectRoot(
    opts.projectRoot || process.cwd(),
  );
  ensureAngelaLayout(PROJECT_ROOT);

  // CLI / db scope for MCP brain server (paths(), socket). Prefer caller
  // (viz passes its command cwd); else process.cwd().
  const brainCwd = opts.brainCwd || process.cwd();
  /** @type {((slug: string) => Promise<object|null>)|null} */
  const fetchEntity =
    typeof opts.fetchEntity === 'function' ? opts.fetchEntity : null;
  // Register Angela app-level default agent (virtual name "default").
  // Appears in the UI only when no disk agents exist under .angela/agents/.
  // MCP paths are resolved at factory call time (running bin + active cwd).
  setDefaultAgent(() =>
    createBrainDefaultAgent({
      brainBin: resolveBrainBin(),
      brainCwd,
    }),
  );

  const MCP_ROOT = opts.mcpRoot || process.env.ANGELA_MCP_ROOT || defaultMcpRoot();
  const AUTO_APPROVE =
    opts.autoApprove === true || process.env.BRAIN_VIZ_AUTO_APPROVE === '1';

  /** tabId → live harness entry */
  const liveTabs = new Map();
  /** tabId → ApprovalQueue while a run is in flight */
  const tabQueues = new Map();
  /** tabId → { text, overridden } stash before first chat creates a session */
  const pendingAllowlists = new Map();
  /** tabId → { enabled: string[]|null, overridden } stash before first chat */
  const pendingToolsEnabled = new Map();
  /** agentName → tool catalog cache (listToolCatalog shape) */
  const toolsCatalogCache = new Map();

  function store() {
    return new SessionStore(PROJECT_ROOT);
  }

  async function loadAgentSafe(name) {
    try {
      return await loadAgent(name, {
        projectRoot: PROJECT_ROOT,
        mcpRoot: MCP_ROOT,
      });
    } catch (err) {
      console.error('[viz-chat] loadAgent failed:', name, err?.message || err);
      return null;
    }
  }

  async function handleAgents() {
    const agents = await listAgents(PROJECT_ROOT);
    const allModels = new Set();
    for (const a of agents) {
      for (const m of a.models || (a.model ? [a.model] : [])) allModels.add(m);
      if (a.model) allModels.add(a.model);
    }
    /** @type {Map<string, number>} */
    const resolved = new Map();
    await Promise.all(
      [...allModels].map(async (m) => {
        const n = await resolveContextWindowAsync(m, {
          default: 32_768,
          force: true,
        });
        resolved.set(m, n);
      }),
    );

    return Response.json({
      projectRoot: PROJECT_ROOT,
      agents: agents.map((a) => {
        const models = a.models || (a.model ? [a.model] : []);
        /** @type {Record<string, number>} */
        const contextWindows = {};
        for (const m of models) {
          contextWindows[m] =
            resolved.get(m) ??
            resolveContextWindow(m, { default: a.contextWindow || 32_768 });
        }
        const defaultModel = a.model || models[0] || null;
        const contextWindow = defaultModel
          ? (resolved.get(defaultModel) ??
            resolveContextWindow(defaultModel, {
              default: a.contextWindow || 32_768,
            }))
          : a.contextWindow || 32_768;
        return {
          name: a.name,
          file: a.file,
          model: a.model,
          models,
          contextWindow,
          contextWindows,
          description: a.description || '',
          allowlist: a.allowlist || '',
          // null = all tools enabled by default (agent coffee omitted toolsEnabled)
          toolsEnabled: a.toolsEnabled ?? null,
          starters: Array.isArray(a.starters) ? a.starters : null,
        };
      }),
    });
  }

  async function handleSessionsList() {
    const meta = store().listMeta();
    return Response.json({
      projectRoot: PROJECT_ROOT,
      sessions: meta.map((s) => ({
        id: s.id,
        title: s.title || s.agent || s.id,
        agent: s.agent,
        model: s.model,
        createdAt: s.createdAt,
        updatedAt: s.updatedAt,
        status: s.status,
        turns: s.turns || 0,
        contextWindow: s.contextWindow ?? null,
      })),
    });
  }

  function cloneTelemetry(t) {
    if (!t || typeof t !== 'object') return null;
    try {
      return structuredClone(t);
    } catch {
      return { ...t };
    }
  }

  function gotoFromMeta(wrap) {
    const g = wrap?.meta?.goto;
    if (!g || typeof g !== 'object') {
      return { enabled: false, label: '' };
    }
    return {
      enabled: Boolean(g.enabled),
      label: String(g.label || ''),
    };
  }

  function reconstructMessagesFromLog(id) {
    const events = store().listEvents(id);
    const messages = [];
    let lastGen = null;
    for (const wrap of events) {
      const p = wrap.payload;
      if (wrap.event_type === 'gen_info' && p) {
        lastGen = wrap.meta?.telemetry || p;
        continue;
      }
      if (!p) continue;
      const frozen = wrap.meta?.telemetry || null;
      if (
        wrap.event_type === 'context_window' &&
        p.role &&
        p.content != null &&
        p.kind !== 'provider_messages_snapshot'
      ) {
        const telemetry =
          frozen || (p.role === 'assistant' ? lastGen : null);
        messages.push({
          id: wrap.event_id || `${id}-${messages.length}`,
          eventId: wrap.event_id || null,
          kind: p.role === 'user' ? 'user' : 'assistant',
          text: String(p.content),
          visible: wrap.meta?.visible !== false,
          telemetry: cloneTelemetry(telemetry),
          goto: gotoFromMeta(wrap),
        });
      } else if (
        (wrap.event_type === 'reasoning' || p?.type === 'reasoning') &&
        (p?.text || p?.content)
      ) {
        messages.push({
          id: wrap.event_id || `${id}-rs-${messages.length}`,
          eventId: wrap.event_id || null,
          kind: 'reasoning',
          text: String(p.text || p.content || ''),
          visible: wrap.meta?.visible !== false,
          telemetry: cloneTelemetry(wrap.meta?.telemetry || frozen || lastGen),
          goto: gotoFromMeta(wrap),
        });
      } else if (wrap.event_type === 'tool_call' && p.name) {
        messages.push({
          id: wrap.event_id || `${id}-tc-${messages.length}`,
          eventId: wrap.event_id || null,
          kind: 'tool_call',
          name: p.name,
          args: p.args,
          explanation: p.explanation || null,
          visible: wrap.meta?.visible !== false,
          telemetry: cloneTelemetry(frozen || lastGen),
          full_chars: p.full_chars,
          truncated: p.truncated,
          goto: gotoFromMeta(wrap),
        });
      } else if (wrap.event_type === 'tool_result' && p.name) {
        messages.push({
          id: wrap.event_id || `${id}-tr-${messages.length}`,
          eventId: wrap.event_id || null,
          kind: 'tool_result',
          name: p.name,
          ok: p.ok,
          denied: p.denied,
          text: p.text,
          explanation: p.explanation || null,
          visible: wrap.meta?.visible !== false,
          telemetry: cloneTelemetry(frozen || lastGen),
          full_chars: p.full_chars,
          truncated: p.truncated,
          goto: gotoFromMeta(wrap),
        });
      }
    }
    return messages;
  }

  async function handleEventMeta(req) {
    const body = await req.json().catch(() => ({}));
    const sessionId = String(body.sessionId || '');
    const eventId = String(body.eventId || '');
    if (!sessionId || !eventId) {
      return Response.json(
        { error: 'sessionId and eventId required' },
        { status: 400 },
      );
    }
    if (Object.prototype.hasOwnProperty.call(body, 'visible')) {
      const result = store().setEventVisible(
        sessionId,
        eventId,
        Boolean(body.visible),
      );
      if (!result) {
        return Response.json({ error: 'event not found' }, { status: 404 });
      }
      const vis = Boolean(body.visible);
      for (const entry of liveTabs.values()) {
        if (entry?.session?.id === sessionId && entry.session.setContextVisible) {
          entry.session.setContextVisible(result.eventIds, vis);
        }
      }
      return Response.json({
        ok: true,
        event: result.events[0] || null,
        events: result.events,
        eventIds: result.eventIds,
        paired: result.eventIds.length > 1,
      });
    }

    // Goto bookmark: { enabled, label } stored in meta.goto (cowork-compatible)
    if (Object.prototype.hasOwnProperty.call(body, 'goto')) {
      const gIn = body.goto && typeof body.goto === 'object' ? body.goto : {};
      const goto = {
        enabled: Boolean(gIn.enabled),
        label: String(gIn.label ?? '').trim(),
      };
      if (goto.enabled && goto.label) {
        const want = goto.label.toLowerCase();
        for (const wrap of store().listEvents(sessionId)) {
          const og = wrap.meta?.goto;
          if (!og?.enabled) continue;
          const lab = String(og.label || '')
            .trim()
            .toLowerCase();
          if (!lab || lab !== want) continue;
          if ((wrap.event_id || null) === eventId) continue;
          return Response.json(
            {
              error: 'duplicate_goto_label',
              message: `Label "${goto.label}" is already used by another enabled bookmark`,
            },
            { status: 409 },
          );
        }
      }
      const updated = store().updateEventMeta(sessionId, eventId, { goto });
      if (!updated) {
        return Response.json({ error: 'event not found' }, { status: 404 });
      }
      for (const entry of liveTabs.values()) {
        if (entry?.session?.id === sessionId && entry.session.setGotoMeta) {
          entry.session.setGotoMeta(eventId, goto);
        }
      }
      return Response.json({
        ok: true,
        event: updated,
        events: [updated],
        eventIds: [eventId],
        goto,
      });
    }

    return Response.json({ error: 'unsupported meta patch' }, { status: 400 });
  }

  async function handleSessionGet(id) {
    const meta = store().load(id);
    if (!meta) return Response.json({ error: 'not found' }, { status: 404 });
    return Response.json({
      session: meta,
      messages: reconstructMessagesFromLog(id),
    });
  }

  async function handleSessionDelete(id) {
    const ok = store().delete(id);
    for (const [tabId, entry] of liveTabs) {
      if (entry?.session?.id === id) {
        try {
          await entry.harness?.close();
        } catch {
          /* ignore */
        }
        liveTabs.delete(tabId);
      }
    }
    return Response.json({ ok, id });
  }

  async function handleSessionsClean() {
    for (const [, entry] of liveTabs) {
      try {
        await entry.harness?.close();
      } catch {
        /* ignore */
      }
    }
    liveTabs.clear();
    tabQueues.clear();
    const n = store().clean();
    return Response.json({ ok: true, removed: n });
  }

  async function closeLiveTab(tabId) {
    const entry = liveTabs.get(tabId);
    if (!entry) return;
    try {
      await entry.harness?.close();
    } catch {
      /* ignore */
    }
    liveTabs.delete(tabId);
  }

  function applyAllowlistToEntry(
    entry,
    text,
    { persist = false, source = 'ui' } = {},
  ) {
    const al = text == null ? '' : String(text);
    if (entry.harness?.policy) {
      if (typeof entry.harness.policy.setAllowlist === 'function') {
        entry.harness.policy.setAllowlist(al);
      } else {
        entry.harness.policy.allowlist = al;
      }
    }
    entry.allowlist = al;
    entry.allowlistSource = source;
    entry.allowlistOverridden = source === 'ui' || source.startsWith('file:');

    if (persist && entry.session?.id && entry.harness?.sessionStore) {
      const st = entry.harness.sessionStore.load(entry.session.id);
      const prevSource = st?.allowlistSource || 'agent';
      const overrides = { ...(st?.overrides || {}) };
      if (source === 'ui' || source.startsWith('file:')) {
        overrides.allowlist = {
          from: prevSource === 'ui' ? 'agent' : prevSource,
          to: source,
        };
      } else {
        delete overrides.allowlist;
      }
      entry.harness.sessionStore.updateMeta(entry.session.id, {
        allowlistSource: source,
        allowlist: source === 'ui' || source.startsWith('file:') ? al : null,
        overrides: Object.keys(overrides).length ? overrides : null,
      });
      entry.harness.sessionStore.appendEvent(entry.session.id, {
        event_type: 'session',
        payload: {
          kind: 'allowlist_updated',
          source,
          bytes: al.length,
        },
      });
    }
  }

  /**
   * Apply toolsEnabled to a live tab entry (filters AGL agent.tools).
   * @param {object} entry
   * @param {string[]|null} enabled null = all tools
   * @param {{ persist?: boolean, source?: string }} [opts]
   */
  function applyToolsEnabledToEntry(
    entry,
    enabled,
    { persist = false, source = 'ui' } = {},
  ) {
    const te = enabled == null ? null : normalizeToolsEnabled(enabled) ?? [];
    entry.toolsEnabled = te;
    entry.toolsEnabledSource = source;
    entry.toolsEnabledOverridden =
      source === 'ui' || String(source || '').startsWith('file:');
    if (entry.harness?.setToolsEnabled) {
      entry.harness.setToolsEnabled(te);
    }
    if (persist && entry.session?.id && entry.harness?.sessionStore) {
      const st = entry.harness.sessionStore.load(entry.session.id);
      const overrides = { ...(st?.overrides || {}) };
      if (entry.toolsEnabledOverridden) {
        overrides.toolsEnabled = {
          from: st?.toolsEnabledSource || 'agent',
          to: source,
        };
      } else {
        delete overrides.toolsEnabled;
      }
      entry.harness.sessionStore.updateMeta(entry.session.id, {
        toolsEnabledSource: source,
        toolsEnabled: entry.toolsEnabledOverridden ? te : null,
        overrides: Object.keys(overrides).length ? overrides : null,
      });
      entry.harness.sessionStore.appendEvent(entry.session.id, {
        event_type: 'session',
        payload: {
          kind: 'tools_enabled_updated',
          source,
          count: te == null ? null : te.length,
        },
      });
    }
  }

  /**
   * Resolve full tool catalog for an agent (MCP + angela builtins).
   * Prefers a live tab's session; otherwise spins an ephemeral harness.
   */
  async function resolveToolCatalog(agentName, tabId = null) {
    if (tabId && liveTabs.has(tabId)) {
      const entry = liveTabs.get(tabId);
      if (entry?.session?.listToolCatalog) {
        try {
          const tools = await entry.session.listToolCatalog();
          toolsCatalogCache.set(entry.agentName || agentName, tools);
          return {
            tools,
            toolsEnabled: entry.toolsEnabled ?? null,
            toolsEnabledSource: entry.toolsEnabledSource || 'agent',
            agent: entry.agentName,
            live: true,
          };
        } catch (err) {
          console.warn(
            '[viz-chat] listToolCatalog on live tab failed:',
            err?.message || err,
          );
        }
      }
    }

    if (toolsCatalogCache.has(agentName)) {
      const def = await loadAgentSafe(agentName);
      return {
        tools: toolsCatalogCache.get(agentName),
        toolsEnabled: def?.toolsEnabled ?? null,
        toolsEnabledSource: 'agent',
        agent: agentName,
        live: false,
        cached: true,
      };
    }

    const def = await loadAgentSafe(agentName);
    if (!def) {
      return {
        tools: [],
        toolsEnabled: null,
        toolsEnabledSource: 'agent',
        agent: agentName,
        error: `agent not found: ${agentName}`,
      };
    }

    const mcp = (def.mcp || []).filter((entry) => {
      const script = entry?.args?.[0];
      if (script && !existsSync(script)) return false;
      return true;
    });

    let harness = null;
    try {
      harness = await Angela.create({
        model: def.model,
        mcp,
        allowlist: def.allowlist || DEFAULT_LUCY_ALLOWLIST,
        toolsEnabled: def.toolsEnabled ?? null,
        policyMode: def.policyMode || 'ask',
        system: def.system || 'catalog probe',
        projectRoot: PROJECT_ROOT,
        agentName: def.name,
        persistSessions: false,
        stream: false,
      });
      const session = await harness.session.create({ agent: def.name });
      const tools = await session.listToolCatalog();
      toolsCatalogCache.set(agentName, tools);
      return {
        tools,
        toolsEnabled: def.toolsEnabled ?? null,
        toolsEnabledSource: 'agent',
        agent: def.name,
        live: false,
      };
    } catch (err) {
      console.error(
        '[viz-chat] tool catalog probe failed:',
        agentName,
        err?.message || err,
      );
      return {
        tools: [],
        toolsEnabled: def.toolsEnabled ?? null,
        toolsEnabledSource: 'agent',
        agent: agentName,
        error: err?.message || String(err),
      };
    } finally {
      try {
        await harness?.close();
      } catch {
        /* ignore */
      }
    }
  }

  async function ensureLiveTab({
    tabId,
    agentName,
    model,
    allowlist,
    allowlistOverridden = false,
    toolsEnabled = undefined,
    toolsEnabledOverridden = false,
    forceNew,
    resumeSessionId = null,
  }) {
    const resumeId =
      !forceNew && resumeSessionId ? String(resumeSessionId).trim() : '';

    const existing = liveTabs.get(tabId);
    if (
      existing &&
      !forceNew &&
      existing.agentName === agentName &&
      existing.model === model &&
      (!resumeId || existing.session?.id === resumeId)
    ) {
      if (allowlistOverridden && allowlist != null) {
        applyAllowlistToEntry(existing, allowlist, {
          persist: true,
          source: 'ui',
        });
      }
      if (toolsEnabledOverridden) {
        applyToolsEnabledToEntry(existing, toolsEnabled ?? [], {
          persist: true,
          source: 'ui',
        });
      }
      return existing;
    }

    await closeLiveTab(tabId);

    let resumeState = null;
    if (resumeId) {
      resumeState = store().load(resumeId);
      if (!resumeState) {
        console.warn('[viz-chat] resume session missing on disk:', resumeId);
      }
    }

    const def = await loadAgentSafe(
      (resumeState?.agent && String(resumeState.agent)) || agentName,
    );
    if (!def) {
      throw new Error(
        `Agent not found: ${agentName} (project ${PROJECT_ROOT}/.angela/agents/)`,
      );
    }

    const overrides = {};
    let effectiveModel = def.model;
    const modelHint =
      (model && String(model).trim()) ||
      (resumeState?.model && String(resumeState.model).trim()) ||
      '';
    if (modelHint) {
      effectiveModel = modelHint;
      if (effectiveModel !== def.model) {
        overrides.model = { from: def.model || null, to: effectiveModel };
      }
    }

    let effectiveAllowlist = def.allowlist || DEFAULT_LUCY_ALLOWLIST;
    let allowlistSource = def.allowlist ? 'agent' : 'default';
    if (allowlistOverridden && allowlist != null) {
      effectiveAllowlist = String(allowlist);
      allowlistSource = 'ui';
      overrides.allowlist = {
        from: def.allowlist ? 'agent' : 'default',
        to: 'ui',
      };
    } else if (resumeState?.allowlistSource === 'ui' && resumeState.allowlist) {
      effectiveAllowlist = String(resumeState.allowlist);
      allowlistSource = 'ui';
    }

    // toolsEnabled: UI override > session resume > agent def (null = all tools)
    let effectiveToolsEnabled =
      def.toolsEnabled == null ? null : [...def.toolsEnabled];
    let toolsEnabledSource = def.toolsEnabled != null ? 'agent' : 'default';
    if (toolsEnabledOverridden) {
      effectiveToolsEnabled =
        toolsEnabled == null ? [] : normalizeToolsEnabled(toolsEnabled) ?? [];
      toolsEnabledSource = 'ui';
      overrides.toolsEnabled = {
        from: def.toolsEnabled != null ? 'agent' : 'default',
        to: 'ui',
      };
    } else if (
      resumeState?.toolsEnabledSource === 'ui' &&
      resumeState.toolsEnabled != null
    ) {
      effectiveToolsEnabled = normalizeToolsEnabled(resumeState.toolsEnabled) ?? [];
      toolsEnabledSource = 'ui';
    }

    const contextWindow = await resolveContextWindowAsync(effectiveModel, {
      default: resumeState?.contextWindow || def.contextWindow || 32_768,
      force: true,
    });

    const mcp = (def.mcp || []).filter((entry) => {
      const script = entry?.args?.[0];
      if (script && !existsSync(script)) {
        console.warn('[viz-chat] skip MCP (missing script):', entry.name, script);
        return false;
      }
      return true;
    });

    // System prompt is NOT taken from agent coffee / createBrainDefaultAgent.
    // It is applied lazily via applyBrainSystemPrompt() immediately before
    // each session.run (ontology.coffee is authoritative).
    const harness = await Angela.create({
      model: effectiveModel,
      mcp,
      allowlist: effectiveAllowlist,
      allowlistSource,
      toolsEnabled: effectiveToolsEnabled,
      policyMode: def.policyMode || 'ask',
      system: null,
      contextWindow,
      projectRoot: PROJECT_ROOT,
      agentName: def.name,
      agentFile: def.path,
      parallel_tools: def.parallel_tools ?? true,
      // Live token streaming for the viz chat UI (angela default is also on).
      stream: true,
      overrides: Object.keys(overrides).length ? overrides : null,
      onApproval: null,
    });

    let session;
    if (resumeState) {
      session = await harness.session.open(resumeId);
    } else {
      session = await harness.session.create({
        title: null,
        agent: def.name,
      });
    }

    const entry = {
      harness,
      session,
      agentName: def.name,
      model: effectiveModel,
      allowlist: effectiveAllowlist,
      allowlistSource,
      allowlistOverridden: allowlistSource === 'ui',
      agentAllowlist: def.allowlist || DEFAULT_LUCY_ALLOWLIST,
      toolsEnabled: effectiveToolsEnabled,
      toolsEnabledSource,
      toolsEnabledOverridden: toolsEnabledSource === 'ui',
      agentToolsEnabled: def.toolsEnabled ?? null,
      contextWindow,
      tokensUsed: Number(resumeState?.lastPromptTokens) || 0,
      lastPromptTokens:
        resumeState?.lastPromptTokens != null
          ? Number(resumeState.lastPromptTokens)
          : null,
      lastTotalTokens:
        resumeState?.lastTotalTokens != null
          ? Number(resumeState.lastTotalTokens)
          : null,
      models: def.models || (def.model ? [def.model] : []),
      mcpNames: mcp.map((m) => m.name),
    };
    const pending = pendingAllowlists.get(tabId);
    if (pending?.overridden && !allowlistOverridden) {
      applyAllowlistToEntry(entry, pending.text, {
        persist: true,
        source: 'ui',
      });
    }
    pendingAllowlists.delete(tabId);

    const pendingTe = pendingToolsEnabled.get(tabId);
    if (pendingTe?.overridden && !toolsEnabledOverridden) {
      applyToolsEnabledToEntry(entry, pendingTe.enabled ?? [], {
        persist: true,
        source: 'ui',
      });
    }
    pendingToolsEnabled.delete(tabId);

    liveTabs.set(tabId, entry);
    return entry;
  }

  async function handleChatStream(req) {
    const body = await req.json();
    const tabId = String(body.tabId || '1');
    const content = String(body.content || '').trim();
    const agentName = String(body.agent || 'default');
    const model = body.model ? String(body.model) : null;
    let allowlist =
      body.allowlist != null ? String(body.allowlist) : undefined;
    let allowlistOverridden = Boolean(body.allowlistOverridden);
    if (!allowlistOverridden && pendingAllowlists.has(tabId)) {
      const p = pendingAllowlists.get(tabId);
      if (p?.overridden) {
        allowlist = p.text;
        allowlistOverridden = true;
      }
    }
    let toolsEnabled =
      body.toolsEnabled != null
        ? normalizeToolsEnabled(body.toolsEnabled)
        : undefined;
    let toolsEnabledOverridden = Boolean(body.toolsEnabledOverridden);
    if (!toolsEnabledOverridden && pendingToolsEnabled.has(tabId)) {
      const p = pendingToolsEnabled.get(tabId);
      if (p?.overridden) {
        toolsEnabled = p.enabled;
        toolsEnabledOverridden = true;
      }
    }
    const forceNew = Boolean(body.newSession);
    const resumeSessionId = body.sessionId ? String(body.sessionId) : null;

    if (!content) {
      return Response.json({ error: 'empty content' }, { status: 400 });
    }

    const queue = new ApprovalQueue();
    tabQueues.set(tabId, queue);

    const encoder = new TextEncoder();
    const stream = new ReadableStream({
      async start(controller) {
        const send = (obj) => {
          try {
            controller.enqueue(encoder.encode(JSON.stringify(obj) + '\n'));
          } catch {
            /* closed */
          }
        };

        const cards = [];
        const pushCard = (card) => {
          cards.push(card);
          send({ type: 'card', card });
        };

        queue.subscribe((reqItem) => {
          send({
            type: 'approval_needed',
            id: reqItem.id,
            tool: reqItem.tool,
            command: reqItem.command,
            args: reqItem.args,
            reason: reqItem.reason,
            tabId,
          });
          pushCard({
            kind: 'approval_needed',
            id: reqItem.id,
            name: reqItem.tool,
            command: reqItem.command,
            args: reqItem.args,
            status: 'pending',
          });
        });

        const onApproval = AUTO_APPROVE
          ? async () => 'allow'
          : queue.createApprover();

        let entry;
        try {
          const prior = liveTabs.get(tabId);
          if (prior?.session?.running && !forceNew) {
            try {
              await prior.session.abort('interrupted by new prompt');
            } catch {
              /* ignore */
            }
          }

          entry = await ensureLiveTab({
            tabId,
            agentName,
            model,
            allowlist,
            allowlistOverridden,
            toolsEnabled,
            toolsEnabledOverridden,
            forceNew,
            resumeSessionId: forceNew ? null : resumeSessionId,
          });

          entry.harness.policy.onApproval = onApproval;
          if (allowlistOverridden && allowlist != null) {
            applyAllowlistToEntry(entry, allowlist, {
              persist: Boolean(entry.session?.id),
              source: 'ui',
            });
          }
          if (toolsEnabledOverridden) {
            applyToolsEnabledToEntry(entry, toolsEnabled ?? [], {
              persist: Boolean(entry.session?.id),
              source: 'ui',
            });
          }

          if (entry.harness.sessionStore) {
            const st = entry.harness.sessionStore.load(entry.session.id);
            if (st && !st.title) {
              st.title =
                content.slice(0, 64) + (content.length > 64 ? '…' : '');
              entry.harness.sessionStore.save(st);
            }
          }

          let lastGenInfo = null;
          // Stream NDJSON as soon as angela emits events (token deltas, tools).
          // Do NOT also wrap every event as {type:'event'} for deltas/tools —
          // the client handles typed messages once (matches cowork).
          const onEvent = (ev) => {
            if (ev.type === 'usage' || ev.type === 'gen_info') {
              if (ev.prompt_tokens != null && ev.prompt_tokens !== '') {
                const prompt = Number(ev.prompt_tokens);
                if (Number.isFinite(prompt) && prompt >= 0) {
                  entry.tokensUsed = prompt;
                  entry.lastPromptTokens = prompt;
                  const total = Number(ev.total_tokens);
                  entry.lastTotalTokens = Number.isFinite(total)
                    ? total
                    : prompt;
                }
              }
              lastGenInfo = {
                completion_tokens: ev.completion_tokens ?? null,
                prompt_tokens:
                  ev.prompt_tokens != null
                    ? Number(ev.prompt_tokens)
                    : entry.lastPromptTokens ?? null,
                total_tokens:
                  ev.total_tokens != null
                    ? Number(ev.total_tokens)
                    : entry.lastTotalTokens ?? null,
                tok_per_sec: ev.tok_per_sec ?? null,
                ttft_ms: ev.ttft_ms ?? null,
                duration_ms: ev.duration_ms ?? null,
                finish_reason: ev.finish_reason ?? null,
                stop_reason: ev.stop_reason ?? null,
                error: ev.error ?? null,
                model: ev.model || entry.model,
                event_id: ev.event_id || null,
                usage_source: ev.usage_source ?? null,
              };
              if (
                entry.lastPromptTokens != null &&
                (ev.type === 'usage' || ev.usage_source === 'provider')
              ) {
                send({
                  type: 'usage',
                  prompt_tokens: entry.lastPromptTokens,
                  completion_tokens: lastGenInfo.completion_tokens,
                  total_tokens: entry.lastTotalTokens,
                  contextWindow: entry.contextWindow,
                  sessionId: entry.session?.id,
                  model: lastGenInfo.model,
                  ...lastGenInfo,
                });
              }
              send({
                type: 'gen_info',
                ...lastGenInfo,
                sessionId: entry.session?.id,
                contextWindow: entry.contextWindow,
              });
              return;
            }
            if (ev.type === 'context_event') {
              send({
                type: 'context_event',
                event_id: ev.event_id,
                role: ev.role,
                content: ev.content,
                visible: ev.visible !== false,
                telemetry: cloneTelemetry(ev.telemetry || lastGenInfo),
              });
              return;
            }
            if (ev.type === 'assistant_delta') {
              send({
                type: 'assistant_delta',
                text: ev.text || '',
                sessionId: entry.session?.id,
              });
              return;
            }
            if (ev.type === 'reasoning_delta') {
              send({
                type: 'reasoning_delta',
                text: ev.text || '',
                sessionId: entry.session?.id,
              });
              return;
            }
            if (ev.type === 'reasoning') {
              pushCard({
                kind: 'reasoning',
                text: ev.text || '',
                eventId: ev.event_id || null,
                telemetry: cloneTelemetry(ev.telemetry || lastGenInfo),
                visible: true,
              });
              return;
            }
            if (ev.type === 'tool_call') {
              entry._lastToolCall = {
                name: ev.name,
                args: ev.args && typeof ev.args === 'object' ? ev.args : {},
              };
              pushCard({
                kind: 'tool_call',
                name: ev.name,
                args: ev.args,
                explanation: ev.explanation || null,
                eventId: ev.event_id || null,
                telemetry: cloneTelemetry(ev.telemetry || lastGenInfo),
                visible: true,
              });
              return;
            }
            if (ev.type === 'tool_result') {
              pushCard({
                kind: 'tool_result',
                name: ev.name,
                ok: ev.ok,
                denied: ev.denied,
                text: ev.text,
                explanation: ev.explanation || null,
                eventId: ev.event_id || null,
                telemetry: cloneTelemetry(ev.telemetry || lastGenInfo),
                visible: true,
                full_chars: ev.full_chars,
                truncated: ev.truncated,
              });
              // SPA entity cache: push snapshot after graph mutations
              void emitEntityChanged(entry, ev, send, fetchEntity);
              return;
            }
            // Pass-through for other harness events (status, final, error, …)
            send({ type: 'event', event: ev });
          };
          entry.harness.onEvent = onEvent;

          const tools = (await entry.harness.listMcpTools()).map((t) => t.name);
          const usedBefore = entry.lastPromptTokens ?? 0;

          send({
            type: 'session',
            tabId,
            sessionId: entry.session.id,
            agent: entry.agentName,
            model: entry.model,
            models: entry.models,
            contextWindow: entry.contextWindow,
            tokensUsed: usedBefore,
            tools,
            mcp: entry.mcpNames || [],
            projectRoot: PROJECT_ROOT,
          });

          // Optional entity YAML for the system prompt (same blocks ontology uses):
          //   selectionContext   — graph multi-select (toggle)
          //   referencedContext  — wiki-link entities in the prompt (preloaded YAML)
          const entCtxParts = [];
          if (body.selectionContext && String(body.selectionContext).trim()) {
            entCtxParts.push(String(body.selectionContext).trim());
          }
          if (body.referencedContext && String(body.referencedContext).trim()) {
            entCtxParts.push(String(body.referencedContext).trim());
          }
          if (body.entityContext && String(body.entityContext).trim()) {
            entCtxParts.push(String(body.entityContext).trim());
          }
          const entCtx = entCtxParts.join('\n');

          // Lazy system prompt (ontology.coffee) — right before Agent.factory / run.
          // think + reasoning_effort are applied by Angela harness augmentation.
          applyBrainSystemPrompt(entry.harness, {
            model: entry.model,
            thinking: Boolean(body.thinking),
            reasoning_effort:
              body.reasoning_effort || body.reasoningEffort || 'medium',
            entCtx,
          });

          // User turn is the raw message only (entity bodies live in system now)
          const prompt = content;

          const result = await entry.session.run({ prompt });
          const text =
            typeof result === 'string'
              ? result
              : result?.summary ||
                result?.choices?.[0]?.message?.content ||
                (result != null ? JSON.stringify(result) : '');

          const tokensUsed =
            entry.lastPromptTokens != null ? entry.lastPromptTokens : usedBefore;
          entry.tokensUsed = tokensUsed;

          let asstTelemetry = cloneTelemetry(lastGenInfo);
          try {
            const snap = entry.harness?.sessionStore?.lastGenTelemetry?.(
              entry.session.id,
            );
            if (snap) asstTelemetry = cloneTelemetry(snap);
          } catch {
            /* ignore */
          }

          const asstText = String(text || '').trim();
          const looksLikeProviderDump =
            asstText.startsWith('{') &&
            /"choices"\s*:/.test(asstText) &&
            /"usage"\s*:/.test(asstText);
          if (asstText && !looksLikeProviderDump) {
            pushCard({
              kind: 'assistant',
              text: asstText,
              telemetry: asstTelemetry,
              streaming: false,
            });
          } else {
            send({
              type: 'card',
              card: {
                kind: 'assistant',
                text: '',
                telemetry: asstTelemetry,
                streaming: false,
              },
            });
          }
          send({
            type: 'done',
            ok: true,
            cards,
            model: entry.model,
            sessionId: entry.session.id,
            contextWindow: entry.contextWindow,
            tokensUsed,
            prompt_tokens: entry.lastPromptTokens ?? null,
            total_tokens: entry.lastTotalTokens ?? null,
            gen_info: cloneTelemetry(lastGenInfo),
          });
        } catch (err) {
          const msg = err?.message || String(err);
          const aborted =
            entry?.session?._aborted ||
            err?.aborted === true ||
            /abort|user stop|interrupted/i.test(msg);
          if (aborted) {
            send({
              type: 'done',
              ok: true,
              aborted: true,
              cards,
              model: entry?.model || model,
              sessionId: entry?.session?.id,
            });
          } else {
            console.error('[viz-chat] chat error:', err);
            send({
              type: 'done',
              ok: false,
              aborted: false,
              error: msg,
              cards,
              model: entry?.model || model,
              sessionId: entry?.session?.id,
            });
          }
        } finally {
          queue.clear('deny');
          tabQueues.delete(tabId);
          try {
            controller.close();
          } catch {
            /* ignore */
          }
        }
      },
    });

    return new Response(stream, {
      headers: {
        'Content-Type': 'application/x-ndjson; charset=utf-8',
        'Cache-Control': 'no-cache',
        'X-Accel-Buffering': 'no',
      },
    });
  }

  async function handleApprove(req) {
    const body = await req.json();
    const id = String(body.id || '');
    const decision = body.decision || 'deny';
    const tabId = body.tabId != null ? String(body.tabId) : null;

    let ok = false;
    if (tabId && tabQueues.has(tabId)) {
      ok = tabQueues.get(tabId).resolve(id, decision);
    } else {
      for (const q of tabQueues.values()) {
        if (q.resolve(id, decision)) {
          ok = true;
          break;
        }
      }
    }
    return Response.json({ ok, id, decision });
  }

  async function handlePending(req) {
    const url = new URL(req.url);
    const tabId = url.searchParams.get('tabId');
    if (tabId && tabQueues.has(tabId)) {
      return Response.json({ pending: tabQueues.get(tabId).list() });
    }
    const all = [];
    for (const [tid, q] of tabQueues) {
      for (const r of q.list()) all.push({ ...r, tabId: tid });
    }
    return Response.json({ pending: all });
  }

  async function handleAbort(req) {
    const body = await req.json().catch(() => ({}));
    const tabId = String(body.tabId || '1');
    const q = tabQueues.get(tabId);
    if (q) q.clear('deny');
    const entry = liveTabs.get(tabId);
    if (entry?.session) await entry.session.abort('user stop');
    return Response.json({ ok: true, sessionId: entry?.session?.id });
  }

  async function handleNewSession(req) {
    const body = await req.json().catch(() => ({}));
    const tabId = String(body.tabId || '1');
    await closeLiveTab(tabId);
    return Response.json({ ok: true });
  }

  async function handleToolsCatalog(req) {
    const url = new URL(req.url);
    const agentName = url.searchParams.get('agent') || 'brain';
    const tabId = url.searchParams.get('tabId') || null;
    const result = await resolveToolCatalog(agentName, tabId);
    return Response.json(result);
  }

  async function handleToolsEnabled(req) {
    const body = await req.json().catch(() => ({}));
    const tabId = String(body.tabId || '1');
    const sessionId = body.sessionId ? String(body.sessionId) : null;
    const overridden = body.overridden !== false;
    /** @type {string[]|null} */
    let enabled = null;
    if (overridden) {
      if (body.toolsEnabled == null) {
        enabled = [];
      } else {
        enabled = normalizeToolsEnabled(body.toolsEnabled) ?? [];
      }
    }

    const entry = liveTabs.get(tabId);
    if (entry) {
      if (!overridden) {
        const agentBase = entry.agentToolsEnabled ?? null;
        applyToolsEnabledToEntry(entry, agentBase, {
          persist: true,
          source: entry.agentToolsEnabled != null ? 'agent' : 'default',
        });
        return Response.json({
          ok: true,
          sessionId: entry.session?.id,
          toolsEnabled: entry.toolsEnabled,
          toolsEnabledSource: entry.toolsEnabledSource,
          overridden: false,
        });
      }
      applyToolsEnabledToEntry(entry, enabled, {
        persist: true,
        source: 'ui',
      });
      return Response.json({
        ok: true,
        sessionId: entry.session?.id,
        toolsEnabled: entry.toolsEnabled,
        toolsEnabledSource: 'ui',
        overridden: true,
      });
    }

    // Stash until ensureLiveTab / first chat
    if (overridden) {
      pendingToolsEnabled.set(tabId, { enabled, overridden: true });
    } else {
      pendingToolsEnabled.delete(tabId);
    }

    if (sessionId) {
      const s = store();
      const st = s.load(sessionId);
      if (!st) {
        return Response.json({ error: 'session not found' }, { status: 404 });
      }
      if (!overridden) {
        s.updateMeta(sessionId, {
          toolsEnabledSource: st.agent ? 'agent' : 'default',
          toolsEnabled: null,
          overrides: (() => {
            const o = { ...(st.overrides || {}) };
            delete o.toolsEnabled;
            return Object.keys(o).length ? o : null;
          })(),
        });
        return Response.json({
          ok: true,
          sessionId,
          toolsEnabledSource: 'agent',
          overridden: false,
        });
      }
      const overrides = { ...(st.overrides || {}) };
      overrides.toolsEnabled = {
        from: st.toolsEnabledSource || 'agent',
        to: 'ui',
      };
      s.updateMeta(sessionId, {
        toolsEnabledSource: 'ui',
        toolsEnabled: enabled,
        overrides,
      });
      return Response.json({
        ok: true,
        sessionId,
        toolsEnabled: enabled,
        toolsEnabledSource: 'ui',
        overridden: true,
      });
    }

    return Response.json({
      ok: true,
      sessionId: null,
      pending: true,
      toolsEnabled: enabled,
      toolsEnabledSource: overridden ? 'ui' : null,
      overridden,
    });
  }

  async function handleAllowlist(req) {
    const body = await req.json().catch(() => ({}));
    const tabId = String(body.tabId || '1');
    const sessionId = body.sessionId ? String(body.sessionId) : null;
    const text = body.allowlist != null ? String(body.allowlist) : '';
    const overridden = body.overridden !== false;

    const entry = liveTabs.get(tabId);

    if (entry) {
      const agentBase = entry.agentAllowlist ?? entry.allowlist;
      if (!overridden) {
        applyAllowlistToEntry(entry, agentBase, {
          persist: true,
          source: entry.agentAllowlist ? 'agent' : 'default',
        });
        return Response.json({
          ok: true,
          sessionId: entry.session?.id,
          allowlistSource: entry.allowlistSource,
          overridden: false,
          bytes: String(entry.allowlist || '').length,
        });
      }
      applyAllowlistToEntry(entry, text, { persist: true, source: 'ui' });
      return Response.json({
        ok: true,
        sessionId: entry.session?.id,
        allowlistSource: 'ui',
        overridden: true,
        bytes: text.length,
      });
    }

    if (sessionId) {
      const s = store();
      const st = s.load(sessionId);
      if (!st) return Response.json({ error: 'session not found' }, { status: 404 });
      if (!overridden) {
        s.updateMeta(sessionId, {
          allowlistSource: st.agent ? 'agent' : 'default',
          allowlist: null,
          overrides: (() => {
            const o = { ...(st.overrides || {}) };
            delete o.allowlist;
            return Object.keys(o).length ? o : null;
          })(),
        });
        return Response.json({
          ok: true,
          sessionId,
          allowlistSource: 'agent',
          overridden: false,
        });
      }
      const overrides = { ...(st.overrides || {}) };
      overrides.allowlist = {
        from: st.allowlistSource || 'agent',
        to: 'ui',
      };
      s.updateMeta(sessionId, {
        allowlistSource: 'ui',
        allowlist: text,
        overrides,
      });
      s.appendEvent(sessionId, {
        event_type: 'session',
        payload: { kind: 'allowlist_updated', source: 'ui', bytes: text.length },
      });
      return Response.json({
        ok: true,
        sessionId,
        allowlistSource: 'ui',
        overridden: true,
        bytes: text.length,
      });
    }

    if (overridden) {
      pendingAllowlists.set(tabId, { text, overridden: true });
    } else {
      pendingAllowlists.delete(tabId);
    }

    return Response.json({
      ok: true,
      sessionId: null,
      pending: true,
      allowlistSource: overridden ? 'ui' : null,
      overridden,
      bytes: text.length,
    });
  }

  async function handleHealth() {
    let agents = [];
    try {
      agents = await listAgents(PROJECT_ROOT);
    } catch {
      /* ignore */
    }
    return Response.json({
      ok: true,
      projectRoot: PROJECT_ROOT,
      autoApprove: AUTO_APPROVE,
      agents: agents.map((a) => a.name),
      mcpRoot: MCP_ROOT,
    });
  }

  async function handleContextWindow(req) {
    const url = new URL(req.url);
    const model = url.searchParams.get('model') || '';
    if (!model) {
      return Response.json({ error: 'model required' }, { status: 400 });
    }
    const contextWindow = await resolveContextWindowAsync(model, {
      default: 32_768,
      force: true,
    });
    return Response.json({ model, contextWindow });
  }

  /**
   * Route a request if it matches /api/* chat surface.
   * @returns {Promise<Response|null>}
   */
  async function handle(req) {
    const url = new URL(req.url);
    const path = url.pathname;
    if (!path.startsWith('/api/')) return null;

    if (req.method === 'POST' && path === '/api/chat') return handleChatStream(req);
    if (req.method === 'POST' && path === '/api/approve') return handleApprove(req);
    if (req.method === 'GET' && path === '/api/pending') return handlePending(req);
    if (req.method === 'POST' && path === '/api/abort') return handleAbort(req);
    if (req.method === 'POST' && path === '/api/session/new') return handleNewSession(req);
    if (req.method === 'POST' && path === '/api/allowlist') return handleAllowlist(req);
    if (req.method === 'GET' && path === '/api/tools') return handleToolsCatalog(req);
    if (req.method === 'POST' && path === '/api/tools-enabled') {
      return handleToolsEnabled(req);
    }
    if (req.method === 'GET' && path === '/api/health') return handleHealth();
    if (req.method === 'GET' && path === '/api/agents') return handleAgents();
    if (req.method === 'GET' && path === '/api/context-window') return handleContextWindow(req);
    if (req.method === 'POST' && path === '/api/event/meta') return handleEventMeta(req);
    if (req.method === 'GET' && path === '/api/sessions') return handleSessionsList();
    if (req.method === 'DELETE' && path === '/api/sessions') return handleSessionsClean();
    if (req.method === 'GET' && path.startsWith('/api/sessions/')) {
      const id = decodeURIComponent(path.slice('/api/sessions/'.length));
      return handleSessionGet(id);
    }
    if (req.method === 'DELETE' && path.startsWith('/api/sessions/')) {
      const id = decodeURIComponent(path.slice('/api/sessions/'.length));
      return handleSessionDelete(id);
    }
    return Response.json({ error: 'not found' }, { status: 404 });
  }

  async function close() {
    for (const [, entry] of liveTabs) {
      try {
        await entry.harness?.close();
      } catch {
        /* ignore */
      }
    }
    liveTabs.clear();
    tabQueues.clear();
  }

  return {
    projectRoot: PROJECT_ROOT,
    handle,
    close,
  };
}

/**
 * After put_entity / delete_entity / method_invoke succeed, notify the SPA so
 * entity-model can upsert/delete without the UI re-fetching ad hoc.
 *
 * @param {object} entry live tab
 * @param {object} ev tool_result event
 * @param {(obj: object) => void} send NDJSON writer
 * @param {((slug: string) => Promise<object|null>)|null} fetchEntity
 */
async function emitEntityChanged(entry, ev, send, fetchEntity) {
  if (!ev || ev.ok === false || ev.denied) return;
  const name = String(ev.name || '');
  const isPut = /(?:^|__)put_entity$/i.test(name);
  const isDel = /(?:^|__)delete_entity$/i.test(name);
  const isMethod = /(?:^|__)method_invoke$/i.test(name);
  if (!isPut && !isDel && !isMethod) return;

  const pending = entry?._lastToolCall;
  const args =
    pending && pending.name === ev.name
      ? pending.args || {}
      : {};
  let slug =
    args.slug != null && String(args.slug).trim()
      ? String(args.slug).trim()
      : '';
  if (!slug && ev.text) {
    const m = String(ev.text).match(/(?:^|\n)\s*slug:\s*['"]?([^\s'"#,]+)/i);
    if (m) slug = m[1].trim();
  }
  if (!slug) return;

  try {
    if (isDel) {
      send({ type: 'entity_changed', slug, deleted: true });
      return;
    }
    if (fetchEntity) {
      try {
        const entity = await fetchEntity(slug);
        if (entity && !entity.error) {
          send({
            type: 'entity_changed',
            slug,
            entity: { ...entity, slug },
          });
          return;
        }
      } catch (err) {
        console.warn(
          '[viz-chat] fetchEntity after mutation failed:',
          err?.message || err,
        );
      }
    }
    // Client will refetch via /nodes
    send({ type: 'entity_changed', slug, stale: true });
  } catch (err) {
    console.warn('[viz-chat] entity_changed emit failed:', err?.message || err);
  }
}

export default createChatApi;
