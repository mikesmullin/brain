# set.coffee — A-box (instance) mutations. `set` writes VALUES onto instances.
#   set <Class/id> <alias.field>=<yamlScalar> ...       # property setter
#   set --file <path> [--class <Class>]                 # file ingest
#       .yaml  => deterministic mode (must validate)     (LLM mode: phase 8)
import { loadWorld, resolveSlug } from '../world.coffee'
import { parseArgs } from '../args.coffee'
import { parseSlug, formatSlug } from '../slug.coffee'
import { isRelationKey } from '../storage.coffee'
import { upsertEntity } from '../upsert.coffee'
import { batchUpsert } from '../upsert.coffee'
import { canonicalizeIds, idFieldOf } from '../canonical.coffee'
import { calcResolver } from '../refine.coffee'
import { extractEntities } from '../extract.coffee'
import { readFile } from 'fs/promises'
import yaml from 'js-yaml'

# Turn a flattened yaml doc ({ _class, _id, <alias>: {...}, <REL>: [...] }) into an entity.
docToEntity = (doc, clsHint) ->
  cls = doc._class or clsHint
  id = doc._id
  throw new Error("ingest: missing _class (pass --class or set _class in the doc)") unless cls
  throw new Error("ingest: missing _id") unless id
  slug = parseSlug(formatSlug(cls, id)).slug
  components = {}
  relations = {}
  # accept either flattened keys OR nested { components: {}, relations: {} }
  src = {}
  if doc.components or doc.relations
    src[k] = v for own k, v of (doc.components or {})
    for own rel, targets of (doc.relations or {})
      src[rel] = targets
  else
    src[k] = v for own k, v of doc when k not in ['_class', '_id']
  for own k, v of src
    if isRelationKey(k)
      relations[k] = (if Array.isArray(v) then v else [v]).map (t) -> if typeof t is 'string' then { _to: t } else t
    else
      components[k] = v
  { slug, cls, id, components, relations, body: '' }

setFileDeterministic = (world, filePath, clsHint, opts) ->
  text = await readFile(filePath, 'utf-8')
  docs = yaml.loadAll(text).filter (d) -> d?
  entities = (docToEntity(doc, clsHint) for doc in docs)
  await batchUpsert(world, entities, opts)

# Apply `alias.field=value` (component) and `REL=Class/id` (relation) assignments onto an entity.
applyAssignments = (world, entity, assignments) ->
  for a in assignments
    eq = a.indexOf('=')
    throw new Error("assignment must be key=value, got '#{a}'") unless eq > 0
    key = a.slice(0, eq)
    rawVal = a.slice(eq + 1)
    if isRelationKey(key)
      target = resolveSlug(world, rawVal)?.slug or parseSlug(rawVal).slug
      entity.relations[key] ?= []
      entity.relations[key] = entity.relations[key].filter (t) -> t._to isnt target
      entity.relations[key].push({ _to: target })
    else
      [alias, field] = key.split('.')
      throw new Error("component assignment key must be alias.field, got '#{key}'") unless alias and field
      entity.components[alias] ?= {}
      entity.components[alias][field] = yaml.load(rawVal)
  entity

# `set <slug|Class> ...`:
#   - <Class/id> updates (or creates) that exact instance.
#   - <Class> (class-only) creates a new instance whose id is DERIVED from its idField
#     (e.g. EntityJournal + `BELONGS_TO=Person/jdoe` -> EntityJournal/jdoe).
export setInstance = (world, cwd, slugRaw, assignments) ->
  if slugRaw.indexOf('/') > 0
    { cls, id } = parseSlug(slugRaw)
    existing = resolveSlug(world, slugRaw)
    entity = if existing then JSON.parse(JSON.stringify(existing)) else { slug: formatSlug(cls, id).slug, cls, id, components: {}, relations: {}, body: '' }
  else
    cls = slugRaw
    throw new Error("unknown class '#{cls}'") unless world.schema.classes?[cls]
    throw new Error("class '#{cls}' has no idField; give an explicit id (#{cls}/<id>)") unless idFieldOf(world.schema, cls)
    entity = { slug: null, cls, id: null, components: {}, relations: {}, body: '' }
  applyAssignments(world, entity, assignments)
  unless entity.id
    await canonicalizeIds(world.schema, [entity], { calc: calcResolver(cwd) })
    throw new Error("could not derive an id for #{cls} (idField unresolved — set the id-source field/relation, or give an explicit id)") unless entity.id
  r = await upsertEntity(world, entity)
  { slug: entity.slug, path: r.path, warnings: r.warnings }

setFileLLM = (world, filePath, clsHint, opts) ->
  text = await readFile(filePath, 'utf-8')
  docs = await extractEntities(world.cwd, text, { schema: world.schema, class: clsHint, world })
  entities = []
  for doc in docs
    try entities.push(docToEntity(doc, clsHint))
    catch err then console.log "  ✗ skipped #{doc._class}/#{doc._id}: #{err.message}"
  await batchUpsert(world, entities, opts)

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['partial'] })
  world = await loadWorld(cwd)
  if flags.file
    filePath = flags.file
    isYaml = /\.ya?ml$/i.test(filePath)
    mode = if isYaml then 'deterministic' else 'LLM'
    # LLM bulk extraction is lenient by default (write partial; run `brain refine` after);
    # deterministic YAML stays strict unless --partial is given.
    opts = { lenient: (if isYaml then !!flags.partial else true) }
    written = if isYaml then await setFileDeterministic(world, filePath, flags.class, opts) else await setFileLLM(world, filePath, flags.class, opts)
    console.log "ingested #{written.length} instance(s) [#{mode}]:"
    console.log "  ✓ #{r.slug}" for r in written
    console.log "run `brain refine` to resolve incomplete entities, then `brain reindex`"
  else
    slug = _[0]
    throw new Error("usage: set <slug|Class> <alias.field>=<value> | <REL>=<slug> ...  OR  set --file <path>") unless slug
    r = await setInstance(world, cwd, slug, _.slice(1))
    console.log "set #{r.slug} -> #{r.path}"
    console.log "  warning: #{w}" for w in (r.warnings or [])
  0
