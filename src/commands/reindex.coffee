# reindex.coffee (command) — the ONE .md -> pglite path. Explicit and
# user-initiated: nothing else (no search, no server start) triggers indexing.
#   brain reindex [--no-embed]
# Routes through the running server when one is up (queries stall while it
# runs); otherwise runs standalone — the one CLI operation allowed to open
# pglite directly, since no server owns it yet.
import { parseArgs } from '../args.coffee'
import { serverRunning } from '../server.coffee'
import { request } from '../client.coffee'

export run = (argv, cwd = process.cwd()) ->
  { flags } = parseArgs(argv, { booleans: ['no-embed'] })
  noEmbed = !!flags['no-embed']
  if serverRunning(cwd)
    console.log 'reindexing via running server (queries will wait) ...'
    res = await request(cwd, 'reindex', { no_embed: noEmbed })
  else
    { Core } = await import('../core.coffee')
    core = await new Core(cwd).init()
    console.log "reindexing#{if noEmbed then ' (no embeddings)' else ''} ..."
    res = await core.reindex({ noEmbed })
    await core.close()
  console.log "indexed #{res.entities} entities · #{res.links} links · embed #{res.provider}:#{res.model}" + (if res.dim then " · dim #{res.dim}" else '')
  0
