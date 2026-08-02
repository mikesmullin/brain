# viz.coffee (command) — real-time browser-based graph explorer.
#   brain viz [--port N] [--relayout]
#
# A thin adapter like every other client: it never opens pglite. Layout is a
# cached maintenance artifact computed by the SERVER (RPC viz_layout, gated
# like reindex); this process serves the m.js + three.js front-end from
# public/, proxies interactive lookups over the same RPC surface the CLI
# uses, and hosts m.js's HMR channel (/__m_hmr WebSocket + a watcher on
# public/) so edits to ui.js / scene.js / styles.css hot-reload in the
# browser without losing camera or store state. Runs until Ctrl-C.
import { readFile } from 'fs/promises'
import { existsSync, statSync, readdirSync, readFileSync, writeFileSync, unlinkSync } from 'fs'
import { join, dirname, extname, normalize } from 'path'
import { fileURLToPath } from 'url'
import yaml from 'js-yaml'
import Agent from 'agl-ai'
import { paths, loadConfig } from '../config.coffee'
import { parseArgs } from '../args.coffee'
import { serverRunning } from '../server.coffee'
import { request, noServerError } from '../client.coffee'
import { createChatApi } from '../viz-chat.mjs'
import { serializeEntity } from '../storage.coffee'
import { parseSlug } from '../slug.coffee'

MIME =
  '.html': 'text/html; charset=utf-8'
  '.js': 'text/javascript; charset=utf-8'
  '.mjs': 'text/javascript; charset=utf-8'
  '.css': 'text/css; charset=utf-8'
  '.json': 'application/json'
  '.svg': 'image/svg+xml'
  '.png': 'image/png'
  '.woff2': 'font/woff2'
  '.map': 'application/json'

export run = (argv, cwd = process.cwd()) ->
  { flags } = parseArgs(argv, { booleans: ['relayout'] })
  throw noServerError(cwd) unless serverRunning(cwd)
  port = if flags.port then parseInt(flags.port, 10) else 4321

  # Singleton guard, mirroring `brain server start`: one viz per db/. A second
  # instance would fight over the port and double-broadcast HMR. db/.viz.lock
  # holds { pid, started, port }; stale locks (dead PID, e.g. kill -9) are
  # cleaned up automatically.
  vizLock = join(paths(cwd).root, '.viz.lock')
  if existsSync(vizLock)
    existing = try JSON.parse(readFileSync(vizLock, 'utf-8')) catch then null
    alive = existing?.pid and (try process.kill(existing.pid, 0); true catch then false)
    if alive
      console.error "brain viz already running for #{paths(cwd).root} (PID #{existing.pid}, port #{existing.port ? '?'})."
      console.error 'Stop it first (Ctrl-C or kill), or use a different db/.'
      return 1
    console.log "brain viz: removing stale lock (PID #{existing?.pid ? '?'} is not alive)"
    unlinkSync(vizLock)
  writeFileSync(vizLock, JSON.stringify({ pid: process.pid, started: Date.now(), port }))
  releaseLock = ->
    try unlinkSync(vizLock) if existsSync(vizLock)
  process.on 'exit', releaseLock
  for sig in ['SIGINT', 'SIGTERM']
    process.on sig, ->
      releaseLock()
      process.exit(0)

  unless (await request(cwd, 'components_stats')).components?
    console.log 'brain viz: components not yet computed — running `components` first (one-time) ...'
    await request(cwd, 'components')

  console.log 'brain viz: checking layout cache ...'
  meta = await request(cwd, 'viz_layout', { force: !!flags.relayout })
  if meta.cached
    console.log "  layout: cached (#{meta.nodes} positions, generated #{meta.generated_at})"
  else
    console.log "  layout: #{meta.nodes} positions in #{(meta.ms / 1000).toFixed(1)}s (component-wise BFS shells, packed islands; reused next time)"

  # LLM model roster for think/ontology: db/config.yaml (`models:` list of
  # provider:model specs) when present, else brain.yaml's think.model, else
  # '' = let agl-ai pick its default chat model (never the EMBEDDING model).
  cfg = await loadConfig(cwd)
  models = if cfg.think.model then [cfg.think.model] else ['']
  dbConfigPath = join(paths(cwd).root, 'config.yaml')
  if existsSync(dbConfigPath)
    raw = yaml.load(await readFile(dbConfigPath, 'utf-8')) or {}
    models = raw.models if Array.isArray(raw.models) and raw.models.length
  # FAV_LOCAL_LLM (env): the user's preferred local model — auto-included at
  # the head of the roster, so it wins as the first-startup default (a
  # localStorage-persisted pick still takes precedence client-side). Deduped
  # in case the roster already lists the same spec.
  fav = (process.env.FAV_LOCAL_LLM or '').trim()
  models = [fav, models...] if fav
  models = Array.from(new Set(models))

  vizDir = join(paths(cwd).root, 'viz')
  publicDir = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'public')
  slugs = (await readFile(join(vizDir, 'slugs.txt'), 'utf-8')).split('\n')
  slugIndex = new Map()
  slugIndex.set(slugs[i], i) for i in [0...slugs.length]

  toIndices = (results) ->
    for r in results
      idx = slugIndex.get(r.slug)
      Object.assign({}, r, { i: (idx ? -1) })

  # Deterministic display name for entity links (prefer info.name / meta.name /
  # any component `.name` / `.title`; fall back to the slug).
  entityDisplayName = (entity, slug) ->
    comps = entity?.components or {}
    for key in ['info', 'meta', 'profile', 'identity']
      n = comps[key]?.name
      return String(n).trim() if n? and String(n).trim()
      t = comps[key]?.title
      return String(t).trim() if t? and String(t).trim()
    for own _alias, fields of comps when fields and typeof fields is 'object'
      for fname in ['name', 'title', 'label', 'display_name', 'full_name']
        v = fields[fname]
        return String(v).trim() if v? and typeof v isnt 'object' and String(v).trim()
    slug or entity?.slug or ''

  # YAML (or JSON fallback) body for <entity> blocks in LLM context.
  formatEntityYaml = (out) ->
    try
      s = parseSlug(out.slug)
      return serializeEntity({
        slug: s.slug, cls: s.cls, id: s.id
        components: out.components or {}
        relations: out.relations or {}
        body: out.body or ''
      }).trimEnd()
    catch
      JSON.stringify({
        components: out.components or {}
        relations: out.relations or {}
        body: out.body or ''
        incoming: out.incoming
      }, null, 2)

  json = (obj) -> new Response(JSON.stringify(obj), { headers: { 'content-type': 'application/json', 'cache-control': 'no-store' } })

  # angela multi-session chat (cowork-compatible /api/* surface).
  # Agents + session logs live under the active brain db/ so the DB admin
  # owns .angela/ next to entities (paths(cwd).root), not the process CWD.
  dbRoot = paths(cwd).root
  chatApi = createChatApi({ projectRoot: dbRoot, brainCwd: cwd })
  console.log "brain viz: angela project root #{chatApi.projectRoot} (db/.angela/agents)"

  # static files from public/ (traversal-guarded, no-store so HMR always wins)
  serveStatic = (pathname) ->
    rel = normalize(decodeURIComponent(pathname)).replace(/^[/\\]+/, '')
    rel = 'index.html' if rel is '' or rel is '.'
    abs = join(publicDir, rel)
    return null unless abs.startsWith(publicDir)
    return null unless existsSync(abs) and statSync(abs).isFile()
    new Response Bun.file(abs), { headers: { 'content-type': MIME[extname(abs).toLowerCase()] or 'application/octet-stream', 'cache-control': 'no-store' } }

  hmrClients = new Set()

  # Recursively list public/ files for HMR (chat/*.js etc.).
  listPublicFiles = (dir, base = '') ->
    out = []
    try
      for f in readdirSync(dir)
        continue if f is 'vendor' or f is 'node_modules'
        abs = join(dir, f)
        rel = if base then "#{base}/#{f}" else f
        try st = statSync(abs) catch then continue
        if st.isDirectory()
          out = out.concat(listPublicFiles(abs, rel))
        else if /\.(html|js|mjs|css|json)$/.test(f)
          out.push(rel)
    catch
      # ignore
    out

  srv = Bun.serve
    port: port
    # LLM think/ontology streams can sit quiet for a long time while the model
    # "thinks" (no SSE tokens yet). The default 10s idle kill was aborting the
    # browser→viz socket mid-inference, which (with GET) triggered cancel +
    # browser retry loops. 255s is Bun's max idleTimeout.
    idleTimeout: 255
    fetch: (req, server) ->
      url = new URL(req.url)
      # m.js HMR websocket
      if url.pathname is '/__m_hmr'
        return undefined if server.upgrade(req)
        return new Response('WebSocket upgrade failed', { status: 400 })
      try
        # angela chat API (sessions, stream, approve, …)
        if url.pathname.startsWith('/api/')
          return await chatApi.handle(req)

        switch url.pathname
          when '/meta.json' then new Response(Bun.file(join(vizDir, 'meta.json')), { headers: { 'cache-control': 'no-store' } })
          # agl_default: the model an empty '' spec resolves to (agl-ai's
          # runtime default) — lets the UI name the actual model instead of
          # showing an opaque "default (agl)" label.
          when '/config.json' then json({ models, default: models[0], agl_default: Agent.default.model })
          when '/data.bin'
            new Response(Bun.file(join(vizDir, 'layout.bin')), { headers: { 'content-type': 'application/octet-stream' } })
          when '/node'
            i = parseInt(url.searchParams.get('i') or '-1', 10)
            slug = slugs[i]
            return json({ error: 'bad index' }) unless slug
            out = await request(cwd, 'get_entity', { slug, include_links: true })
            json(Object.assign(out, { i, slug }))
          when '/nodes'
            # Multi-entity fetch for the Inspector (comma-separated slugs).
            raw = url.searchParams.get('slugs') or ''
            list = raw.split(',').map((s) -> decodeURIComponent(s.trim())).filter(Boolean)
            list = list.slice(0, 64)
            entities = []
            for slug in list
              try
                out = await request(cwd, 'get_entity', { slug, include_links: true })
                entities.push(Object.assign(out, { slug, i: (slugIndex.get(slug) ? -1) }))
              catch err
                entities.push({ slug, error: err.message or String(err) })
            json({ entities })
          when '/labels'
            # Lightweight display-name lookup for entity links (batch).
            # Returns { labels: { "Class/id": "Human Name", ... } } — never fails a
            # whole request; missing entities map to the slug itself.
            raw = url.searchParams.get('slugs') or ''
            list = raw.split(',').map((s) -> decodeURIComponent(s.trim())).filter(Boolean)
            list = list.slice(0, 100)
            labels = {}
            for slug in list
              try
                out = await request(cwd, 'get_entity', { slug, include_links: false })
                labels[slug] = entityDisplayName(out, slug)
              catch
                labels[slug] = slug
            json({ labels })
          when '/entity-context'
            # Preload entity YAML for LLM prompts (selected or wiki-referenced).
            # GET ?slugs=a,b&tag=referenced-entities&notice=optional+line
            raw = url.searchParams.get('slugs') or ''
            tag = url.searchParams.get('tag') or 'referenced-entities'
            tag = tag.replace(/[^\w-]/g, '') or 'referenced-entities'
            notice = url.searchParams.get('notice') or ''
            list = raw.split(',').map((s) -> decodeURIComponent(s.trim())).filter(Boolean)
            list = list.slice(0, 32)
            entities = []
            for slug in list
              try
                out = await request(cwd, 'get_entity', { slug, include_links: true })
                entities.push(Object.assign({ slug }, out))
              catch
                # skip missing — count matches emitted bodies
            n = entities.length
            countLine = if notice then notice else "NOTICE: #{n} entit#{if n is 1 then 'y' else 'ies'}."
            unless n
              return json({ text: "\n\n#{countLine}\n", slugs: list, found: 0 })
            blocks = for e in entities
              "<entity slug=\"#{e.slug}\">\n#{formatEntityYaml(e)}\n</entity>"
            text = "\n\n#{countLine}\n\n<#{tag}>\n#{blocks.join('\n')}\n</#{tag}>\n"
            json({ text, slugs: list, found: n })
          when '/entity/set'
            # Inspector field write: { slugs: [...], assignments: ['alias.field=value', ...] }
            # Applies set_instance to every selected slug (multi-edit).
            return json({ error: 'POST required' }) unless req.method is 'POST'
            body = {}
            try body = await req.json() catch then body = {}
            slugList = body.slugs or []
            slugList = [slugList] unless Array.isArray(slugList)
            assignments = body.assignments or []
            assignments = [assignments] unless Array.isArray(assignments)
            return json({ error: 'slugs and assignments required' }) unless slugList.length and assignments.length
            results = []
            for slug in slugList
              try
                res = await request(cwd, 'set_instance', { slug, assignments })
                results.push(Object.assign({ slug, ok: true }, res))
              catch err
                results.push({ slug, ok: false, error: err.message or String(err) })
            json({ ok: results.every((r) -> r.ok), results })
          when '/search'
            res = await request(cwd, 'search', {
              query: url.searchParams.get('q') or '', limit: parseInt(url.searchParams.get('limit') or '25', 10)
              strategy: url.searchParams.get('strategy') or 'hybrid'
              expand: url.searchParams.get('expand') isnt 'false'
            })
            json(toIndices(res))
          when '/think', '/ontology'
            # LLM modes — POST only (never GET).
            # Browsers may auto-retry failed GETs as "idempotent", which re-ran
            # Agent.factory with the same qid after a dropped connection (the
            # start→cancel→start pattern with identical qids in the DEBUG log).
            # POST is not auto-retried.
            #
            # Client disconnect → server cancel so a refresh/tab-close frees the
            # LM Studio slot (viz would otherwise keep the RPC alive).
            mode = url.pathname.slice(1)   # 'think' | 'ontology'
            body = {}
            if req.method is 'POST'
              try body = await req.json() catch then body = {}
            else
              # legacy GET query-string (CLI experiments); prefer POST from the UI
              body =
                q: url.searchParams.get('q') or ''
                qid: url.searchParams.get('qid') or undefined
                model: url.searchParams.get('model') or undefined
                think: url.searchParams.get('think') is 'true'
                sel: if url.searchParams.has('sel') then (url.searchParams.get('sel') or '') else undefined
            qid = body.qid or undefined
            cancelledByClient = false
            onAbort = ->
              return unless qid
              cancelledByClient = true
              request(cwd, 'cancel', { qid }).catch ->
            req.signal?.addEventListener?('abort', onAbort, { once: true })
            try
              payload =
                question: body.q or body.question or ''
                qid: qid
                model: body.model or undefined
                thinking: body.think is true or body.thinking is true
                # sel present (even empty) = selection toggle ON → selectionContext
                # emits deictic blurb + count + optional entity YAML.
                # sel absent = toggle OFF → selectionContext emits nothing.
                selection: if body.sel? then String(body.sel).split(',').filter(Boolean) else undefined
              payload.limit = 8 if mode is 'think'
              res = await request(cwd, mode, payload)
              if mode is 'think'
                res.citation_nodes = ({ slug: s, i: (slugIndex.get(s) ? -1) } for s in (res.citations or []))
              else
                res.entity_nodes = ({ slug: s, i: (slugIndex.get(s) ? -1) } for s in (res.entities or []))
              json(res)
            catch err
              # disconnect abort is expected — don't log as a hard failure
              throw err unless cancelledByClient or /cancel/i.test(err.message or '')
              json({ error: err.message or 'cancelled by user' })
            finally
              try req.signal?.removeEventListener?('abort', onAbort)
          when '/graphql'
            json(await request(cwd, 'graphql', { query: url.searchParams.get('q') or '' }))
          when '/graph'
            res = await request(cwd, 'graph', { pattern: url.searchParams.get('pattern') or '' })
            matches = res.matches.slice(0, 500).map (m) ->
              { path: m.path, via: m.via, end: m.end, pathIdx: (slugIndex.get(s) ? -1 for s in m.path) }
            json({ count: res.matches.length, capped: res.capped, matches })
          when '/inflight'
            # refresh recovery: UI re-adopts a still-running LLM query
            json(await request(cwd, 'inflight', {}))
          when '/cancel'
            # abort an in-flight think/ontology inference (see server `cancel`).
            # empty qid = cancel ALL (used when UI lost the qid after refresh).
            json(await request(cwd, 'cancel', { qid: url.searchParams.get('qid') or '' }))
          when '/resolve'
            slug = url.searchParams.get('slug') or ''
            i = slugIndex.get(slug)
            json({ slug, i: (i ? -1) })
          when '/speak'
            # voice toggle: vocalize an LLM answer through Ada (`ada voice` —
            # her configured preset + avatar closed captions). Fire-and-forget;
            # degrades gracefully when the `ada` CLI isn't installed (the
            # client shows an install hint instead of failing silently).
            text = (url.searchParams.get('text') or '').slice(0, 4000)
            return json({ error: 'no text' }) unless text.trim()
            return json({ error: 'ada_not_installed' }) unless Bun.which('ada')
            try
              Bun.spawn(['ada', 'voice', text], { stdout: 'ignore', stderr: 'ignore' })
              json({ ok: true })
            catch err
              json({ error: "ada_failed: #{err.message}" })
          else
            # SPA fallback: /e/<slugs> permalinks are client-side routes —
            # serve the shell and let the m.js Router restore the selection.
            spa = serveStatic('/index.html') if url.pathname.startsWith('/e/')
            serveStatic(url.pathname) or spa or new Response('not found', { status: 404 })
      catch err
        json({ error: err.message })
    websocket:
      open: (ws) ->
        hmrClients.add(ws)
        ws.send JSON.stringify({ type: 'connected', version: 'brain-viz' })
      close: (ws) -> hmrClients.delete(ws)
      message: ->

  # HMR watcher: recursive public/ (except vendor/) via 300ms mtime poll.
  # fs.watch proved unreliable here; stat'ing a handful of files is free.
  mtimes = new Map()
  setInterval (->
    try
      for f in listPublicFiles(publicDir)
        try m = statSync(join(publicDir, f)).mtimeMs catch then continue
        prev = mtimes.get(f)
        mtimes.set(f, m)
        continue unless prev? and m > prev
        payload = JSON.stringify({ type: 'change', path: '/' + f })
        for ws from hmrClients
          try ws.send(payload)
        console.log "[hmr] #{f} -> #{hmrClients.size} client(s)"
    catch
      # public/ momentarily unreadable — try again next tick
  ), 300

  console.log "brain viz: serving http://127.0.0.1:#{port}   (Ctrl-C to stop)"
  console.log "  nodes: #{meta.nodes} · components: #{meta.components} (#{meta.isolated} isolated) · color: component · size: log-degree"
  console.log "  HMR: watching #{publicDir} (incl. chat/*) · chat: angela multi-session"
  console.log "  connected to brain server for hover/search lookups"
  await new Promise(->)   # serve until Ctrl-C
  0
