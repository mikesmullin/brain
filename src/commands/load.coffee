# load.coffee (command) — import an ALREADY-STRUCTURED dataset (no LLM calls).
#
#   brain load <path> [--no-embed]
#
# <path> must contain a schema.yaml and <Class>/<id>.md entity dirs (i.e. a
# ready-made db/, e.g. a benchmark/playground dataset like the Panama Papers
# ETL output, a fixture, or an onboarding example). Files are copied into the
# active db/ (same-named files are overwritten) and the index is rebuilt.
# For raw unstructured documents that need LLM extraction, use `ingest`.
import { join, basename } from 'path'
import { existsSync } from 'fs'
import { readdir, cp } from 'fs/promises'
import { parseArgs } from '../args.coffee'
import { paths } from '../config.coffee'
import { serverRunning } from '../server.coffee'
import { request } from '../client.coffee'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['no-embed'] })
  src = _[0]
  throw new Error("usage: load <path> [--no-embed]") unless src
  throw new Error("not found: #{src}") unless existsSync(src)
  throw new Error("#{src} does not look like a brain dataset (no schema.yaml)") unless existsSync(join(src, 'schema.yaml'))
  dest = paths(cwd).storage
  copied = 0
  for ent in await readdir(src, { withFileTypes: true })
    continue if ent.name in ['pgdata', '.lock', '.sock']
    from = join(src, ent.name)
    to = join(dest, ent.name)
    await cp(from, to, { recursive: true, force: true })
    copied++
  console.log "copied #{copied} top-level entries from #{src} into #{dest}"
  noEmbed = !!flags['no-embed']
  console.log 'reindexing' + (if noEmbed then ' (no embeddings)' else '') + ' ...'
  if serverRunning(cwd)
    res = await request(cwd, 'reindex', { no_embed: noEmbed })
  else
    { Core } = await import('../core.coffee')
    core = await new Core(cwd).init()
    res = await core.reindex({ noEmbed })
    await core.close()
  console.log "indexed #{res.entities} entities · #{res.links} links · embed #{res.provider}:#{res.model}"
  console.log 'start (or restart) the server to serve queries:  brain server start'
  0
