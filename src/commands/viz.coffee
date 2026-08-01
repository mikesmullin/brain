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
import { existsSync, statSync, watch } from 'fs'
import { join, dirname, extname, normalize } from 'path'
import { fileURLToPath } from 'url'
import yaml from 'js-yaml'
import { paths, loadConfig } from '../config.coffee'
import { parseArgs } from '../args.coffee'
import { serverRunning } from '../server.coffee'
import { request, noServerError } from '../client.coffee'

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

  vizDir = join(paths(cwd).root, 'viz')
  publicDir = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'public')
  slugs = (await readFile(join(vizDir, 'slugs.txt'), 'utf-8')).split('\n')
  slugIndex = new Map()
  slugIndex.set(slugs[i], i) for i in [0...slugs.length]

  toIndices = (results) ->
    for r in results
      idx = slugIndex.get(r.slug)
      Object.assign({}, r, { i: (idx ? -1) })

  json = (obj) -> new Response(JSON.stringify(obj), { headers: { 'content-type': 'application/json', 'cache-control': 'no-store' } })

  # static files from public/ (traversal-guarded, no-store so HMR always wins)
  serveStatic = (pathname) ->
    rel = normalize(decodeURIComponent(pathname)).replace(/^[/\\]+/, '')
    rel = 'index.html' if rel is '' or rel is '.'
    abs = join(publicDir, rel)
    return null unless abs.startsWith(publicDir)
    return null unless existsSync(abs) and statSync(abs).isFile()
    new Response Bun.file(abs), { headers: { 'content-type': MIME[extname(abs).toLowerCase()] or 'application/octet-stream', 'cache-control': 'no-store' } }

  hmrClients = new Set()

  srv = Bun.serve
    port: port
    idleTimeout: 120
    fetch: (req, server) ->
      url = new URL(req.url)
      # m.js HMR websocket
      if url.pathname is '/__m_hmr'
        return undefined if server.upgrade(req)
        return new Response('WebSocket upgrade failed', { status: 400 })
      try
        switch url.pathname
          when '/meta.json' then new Response(Bun.file(join(vizDir, 'meta.json')), { headers: { 'cache-control': 'no-store' } })
          when '/config.json' then json({ models, default: models[0] })
          when '/data.bin'
            new Response(Bun.file(join(vizDir, 'layout.bin')), { headers: { 'content-type': 'application/octet-stream' } })
          when '/node'
            i = parseInt(url.searchParams.get('i') or '-1', 10)
            slug = slugs[i]
            return json({ error: 'bad index' }) unless slug
            out = await request(cwd, 'get_entity', { slug, include_links: true })
            json(Object.assign(out, { i, slug }))
          when '/search'
            res = await request(cwd, 'search', {
              query: url.searchParams.get('q') or '', limit: parseInt(url.searchParams.get('limit') or '25', 10)
              strategy: url.searchParams.get('strategy') or 'hybrid'
              expand: url.searchParams.get('expand') isnt 'false'
            })
            json(toIndices(res))
          when '/think'
            res = await request(cwd, 'think', {
              question: url.searchParams.get('q') or '', limit: 8
              model: url.searchParams.get('model') or undefined
              thinking: url.searchParams.get('think') is 'true'
              selection: (url.searchParams.get('sel') or '').split(',').filter(Boolean)
            })
            res.citation_nodes = ({ slug: s, i: (slugIndex.get(s) ? -1) } for s in (res.citations or []))
            json(res)
          when '/ontology'
            res = await request(cwd, 'ontology', {
              question: url.searchParams.get('q') or ''
              model: url.searchParams.get('model') or undefined
              thinking: url.searchParams.get('think') is 'true'
              selection: (url.searchParams.get('sel') or '').split(',').filter(Boolean)
            })
            res.entity_nodes = ({ slug: s, i: (slugIndex.get(s) ? -1) } for s in (res.entities or []))
            json(res)
          when '/graphql'
            json(await request(cwd, 'graphql', { query: url.searchParams.get('q') or '' }))
          when '/graph'
            res = await request(cwd, 'graph', { pattern: url.searchParams.get('pattern') or '' })
            matches = res.matches.slice(0, 500).map (m) ->
              { path: m.path, via: m.via, end: m.end, pathIdx: (slugIndex.get(s) ? -1 for s in m.path) }
            json({ count: res.matches.length, capped: res.capped, matches })
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
            serveStatic(url.pathname) or new Response('not found', { status: 404 })
      catch err
        json({ error: err.message })
    websocket:
      open: (ws) ->
        hmrClients.add(ws)
        ws.send JSON.stringify({ type: 'connected', version: 'brain-viz' })
      close: (ws) -> hmrClients.delete(ws)
      message: ->

  # HMR watcher: top-level public/ files only (index.html, app/ui/scene.js,
  # styles.css) — vendor/ is not watched. Debounced broadcast in m.js's format.
  pending = new Set()
  debounce = null
  watcher = watch publicDir, (event, filename) ->
    return unless filename and not filename.includes('/')
    # only real assets — atomic-save editors emit tmp-suffixed names too
    return unless /\.(html|js|mjs|css|json)$/.test(filename)
    pending.add(filename)
    clearTimeout(debounce)
    debounce = setTimeout (->
      for f from pending
        payload = JSON.stringify({ type: 'change', path: '/' + f })
        for ws from hmrClients
          try ws.send(payload)
        console.log "[hmr] #{f} -> #{hmrClients.size} client(s)"
      pending.clear()
    ), 50

  console.log "brain viz: serving http://127.0.0.1:#{port}   (Ctrl-C to stop)"
  console.log "  nodes: #{meta.nodes} · components: #{meta.components} (#{meta.isolated} isolated) · color: component · size: log-degree"
  console.log "  HMR: watching #{publicDir} (edit ui.js / scene.js / styles.css live)"
  console.log "  connected to brain server for hover/search lookups"
  await new Promise(->)   # serve until Ctrl-C
  0
