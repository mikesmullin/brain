# validate.coffee (command) — validate + lint all instances against the schema.
# Runs entirely against the live pglite index via the brain server (no entity .md load).
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'

export run = (argv, cwd = process.cwd()) ->
  { flags } = parseArgs(argv, { booleans: ['quiet'] })
  res = await request(cwd, 'validate', {})
  unless flags.quiet
    console.log "#{res.counts.entities} entities · #{res.counts.classes} classes · #{res.counts.relations} relations"
  if res.warnings.length
    console.log "\nwarnings (#{res.warnings.length}):"
    console.log "  ⚠ #{w}" for w in res.warnings
  if res.errors.length
    console.log "\nerrors (#{res.errors.length}):"
    console.log "  ✗ #{e}" for e in res.errors
    console.log "\nINVALID"
    return 1
  console.log "\nOK — valid"
  0
