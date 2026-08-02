# set.coffee — A-box (instance) mutations. `set` writes VALUES onto instances.
#   set <Class/id> <alias.field>=<yamlScalar> ...       # property setter (live index, via server)
#   set --file <path> [--class <Class>]                 # bulk file ingest (writes .md; reindex after)
#       .yaml  => deterministic mode (must validate)     (LLM mode: extraction)
#
# Single-instance sets are pglite-first: they land in the running server's
# live index immediately (searchable at once) and reach .md on `brain export`.
# Bulk --file ingest is an out-of-band maintenance flow: it writes .md files
# directly and requires an explicit `brain reindex` to go live.
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import { isRelationKey } from '../storage.coffee'
import { parseSlug, formatSlug } from '../slug.coffee'
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
  { batchUpsert } = await import('../upsert.coffee')
  text = await readFile(filePath, 'utf-8')
  docs = yaml.loadAll(text).filter (d) -> d?
  entities = (docToEntity(doc, clsHint) for doc in docs)
  await batchUpsert(world, entities, opts)

setFileLLM = (world, filePath, clsHint, opts) ->
  { batchUpsert } = await import('../upsert.coffee')
  { extractEntities } = await import('../extract.coffee')
  text = await readFile(filePath, 'utf-8')
  docs = await extractEntities(world.cwd, text, { schema: world.schema, class: clsHint, world })
  entities = []
  for doc in docs
    try entities.push(docToEntity(doc, clsHint))
    catch err then console.log "  ✗ skipped #{doc._class}/#{doc._id}: #{err.message}"
  await batchUpsert(world, entities, opts)

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['partial'] })
  if flags.file
    # Bulk file ingest: read the *input* file only. Schema from schema.yaml;
    # never loadWorld (that walks every entity .md). Writes still land as .md
    # for reindex to pick up — prefer single-entity `set` for live pglite writes.
    { loadSchemaContext } = await import('../world.coffee')
    world = await loadSchemaContext(cwd)
    filePath = flags.file
    isYaml = /\.ya?ml$/i.test(filePath)
    mode = if isYaml then 'deterministic' else 'LLM'
    # LLM bulk extraction is lenient by default (write partial; run `brain refine` after);
    # deterministic YAML stays strict unless --partial is given.
    opts = { lenient: (if isYaml then !!flags.partial else true) }
    written = if isYaml then await setFileDeterministic(world, filePath, flags.class, opts) else await setFileLLM(world, filePath, flags.class, opts)
    console.log "ingested #{written.length} instance(s) [#{mode}]:"
    console.log "  ✓ #{r.slug}" for r in written
    console.log "run `brain refine` to resolve incomplete entities, then `brain reindex` to go live"
  else
    slug = _[0]
    throw new Error("usage: set <slug|Class> <alias.field>=<value> | <REL>=<slug> ...  OR  set --file <path>") unless slug
    r = await request(cwd, 'set_instance', { slug, assignments: _.slice(1) })
    console.log "set #{r.slug} (live index; run `brain export` to materialize .md)"
    console.log "  warning: #{w}" for w in (r.warnings or [])
    console.log "  invalid: #{e}" for e in (r.validationErrors or [])
  0
