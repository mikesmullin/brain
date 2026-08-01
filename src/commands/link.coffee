# link.coffee — A-box: add a relation edge between two instances (live index).
#   link <Class/id> <REL> <Class/id> [qualifier=<yamlScalar> ...]
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  [fromRaw, rel, toRaw, quals...] = _
  throw new Error("usage: link <slug> <REL> <slug> [qual=value ...]") unless fromRaw and rel and toRaw
  qualifiers = {}
  for q in quals
    eq = q.indexOf('=')
    throw new Error("qualifier must be name=value, got '#{q}'") unless eq > 0
    qualifiers[q.slice(0, eq)] = yaml.load(q.slice(eq + 1))
  r = await request(cwd, 'link', { from: fromRaw, rel, to: toRaw, qualifiers })
  console.log "linked #{r.from} -->|#{r.rel}| #{r.to}"
  console.log "  warning: #{w}" for w in (r.warnings or [])
  console.log "  invalid: #{e}" for e in (r.validationErrors or [])
  0
