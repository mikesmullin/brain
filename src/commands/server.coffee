# server.coffee (command) — manage the long-running brain server (SERVER MODE).
#   brain server start            run the server in the foreground (owns pglite)
#   brain server stop             graceful shutdown of the running server
#   brain server status           PID, uptime, index counts, memory
#   brain server vacuum           VACUUM ANALYZE the live index
#   brain server reindex          rebuild the index (queries stall while it runs)
#   brain server export           materialize pglite -> .md (see `brain export`)
import yaml from 'js-yaml'
import { parseArgs } from '../args.coffee'
import { startServer, serverRunning, readLock } from '../server.coffee'
import { request } from '../client.coffee'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['prune', 'no-embed'] })
  sub = _[0] or 'status'
  switch sub
    when 'start'
      await startServer(cwd)
    when 'stop'
      running = serverRunning(cwd)
      unless running
        console.log 'no brain server running'
        return 0
      try
        await request(cwd, 'stop')
        console.log "stopped brain server (PID #{running.pid})"
      catch
        # socket unreachable but PID alive — fall back to a signal
        process.kill(running.pid, 'SIGTERM')
        console.log "sent SIGTERM to brain server (PID #{running.pid})"
      0
    when 'status'
      unless serverRunning(cwd)
        stale = readLock(cwd)
        console.log 'no brain server running' + (if stale then " (stale lock: PID #{stale.pid})" else '')
        return 1
      console.log yaml.dump(await request(cwd, 'status'), { lineWidth: 120, sortKeys: false, noRefs: true })
      0
    when 'vacuum'
      console.log yaml.dump(await request(cwd, 'vacuum'), { sortKeys: false })
      0
    when 'components'
      console.log 'computing connected components (queries wait while this runs) ...'
      s = await request(cwd, 'components')
      console.log "components: #{s.components}"
      console.log "  largest:   #{s.largest.size} nodes (#{s.largest.pct}%)   component_id=#{s.largest.component_id}"
      console.log "  next 4:    #{(s.next or []).join(' · ') or '(none)'}"
      console.log "  isolated (size 1): #{s.isolated}    <- superset of `brain schema orphans`"
      console.log "summary cached · per-entity component_id queryable · took #{(s.ms / 1000).toFixed(1)}s"
      console.log 'refresh manually after big ingests, like the size counters'
      0
    when 'reindex'
      res = await request(cwd, 'reindex', { no_embed: !!flags['no-embed'] })
      console.log "reindexed #{res.entities} entities · #{res.links} links · embed #{res.provider}:#{res.model}"
      0
    when 'export'
      res = await request(cwd, 'export', { prune: !!flags.prune })
      console.log "exported #{res.written} entities to .md" + (if res.pruned then " · pruned #{res.pruned} stale files" else '')
      0
    else
      throw new Error("usage: server <start|stop|status|vacuum|reindex|export|components>")
