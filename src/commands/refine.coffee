# refine.coffee (command) — resolve/repair invalid entities via per-class refiners.
# Idempotent second step after bulk ingest; also a general repair tool.
#   refine [--class <Class>] [--max-passes N]
import { refineAll } from '../refine.coffee'
import { parseArgs } from '../args.coffee'

export run = (argv, cwd = process.cwd()) ->
  { flags } = parseArgs(argv)
  maxPasses = if flags['max-passes'] then parseInt(flags['max-passes'], 10) else undefined
  r = await refineAll(cwd, { class: flags.class, maxPasses })
  console.log "refine: #{r.passes} pass(es) · #{r.refined} refined · #{r.created} created · #{r.renamed} renamed · #{r.deleted} deleted"
  console.log "run `brain validate` and `brain reindex` to finish"
  0
