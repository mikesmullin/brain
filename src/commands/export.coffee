# export.coffee (command) — the ONE pglite -> .md path (mirror image of reindex).
# `.md` files are the human-readable, git-friendly snapshot of the live index;
# the user decides when to take one. Runs via the server when one is up
# (queries stall during it), or standalone otherwise.
import { parseArgs } from '../args.coffee'
import { serverRunning } from '../server.coffee'
import { request } from '../client.coffee'

export run = (argv, cwd = process.cwd()) ->
  { flags } = parseArgs(argv, { booleans: ['prune'] })
  if serverRunning(cwd)
    res = await request(cwd, 'export', { prune: !!flags.prune })
  else
    { Core } = await import('../core.coffee')
    core = await new Core(cwd).init()
    unless await core.isIndexed()
      await core.close()
      throw new Error('no index found — run `brain reindex` first')
    res = await core.exportMd({ prune: !!flags.prune })
    await core.close()
  console.log "exported #{res.written} entities to .md" + (if res.pruned then " · pruned #{res.pruned} stale files" else '')
  console.log "  pruned: #{s}" for s in (res.prunedSlugs or [])
  0
