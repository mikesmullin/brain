# link.coffee — A-box: add a relation edge between two instances.
#   link <Class/id> <REL> <Class/id> [qualifier=<yamlScalar> ...]
import { loadWorld, resolveSlug } from '../world.coffee'
import { parseArgs } from '../args.coffee'
import { parseSlug } from '../slug.coffee'
import { upsertEntity } from '../upsert.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  [fromRaw, rel, toRaw, quals...] = _
  throw new Error("usage: link <slug> <REL> <slug> [qual=value ...]") unless fromRaw and rel and toRaw
  world = await loadWorld(cwd)
  e = resolveSlug(world, fromRaw)
  throw new Error("source not found: #{fromRaw}") unless e
  from = e.slug
  to = resolveSlug(world, toRaw)?.slug or parseSlug(toRaw).slug
  entity = JSON.parse(JSON.stringify(e))
  target = { _to: to }
  for q in quals
    eq = q.indexOf('=')
    throw new Error("qualifier must be name=value, got '#{q}'") unless eq > 0
    target[q.slice(0, eq)] = yaml.load(q.slice(eq + 1))
  entity.relations[rel] ?= []
  # de-dupe by _to
  entity.relations[rel] = entity.relations[rel].filter (t) -> t._to isnt to
  entity.relations[rel].push(target)
  r = await upsertEntity(world, entity)
  console.log "linked #{from} -->|#{rel}| #{to}"
  0
