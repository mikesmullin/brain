# ingest.coffee (command) — one-time bulk import of a directory of Markdown.
#   ingest <dir> [--extract Class ...]   LLM extraction into .brain/storage
#   ingest <dir> --suggest               dry-run: print recommended new schema
# (Markdown-only for v1; pdf/docx/other formats are a later-phase placeholder.)
import { loadWorld } from '../world.coffee'
import { parseArgs, asArray } from '../args.coffee'
import { extractEntities, suggestSchema } from '../extract.coffee'
import { batchUpsert } from '../upsert.coffee'
import { parseSlug, formatSlug } from '../slug.coffee'
import { isRelationKey } from '../storage.coffee'
import { join, extname } from 'path'
import { readdir, readFile } from 'fs/promises'

docToEntity = (doc) ->
  cls = doc._class; id = doc._id
  return null unless cls and id
  slug = parseSlug(formatSlug(cls, id)).slug
  components = {}; relations = {}
  src = {}
  if doc.components or doc.relations
    src[k] = v for own k, v of (doc.components or {})
    (src[rel] = t) for own rel, t of (doc.relations or {})
  else
    src[k] = v for own k, v of doc when k not in ['_class', '_id']
  for own k, v of src
    if isRelationKey(k)
      relations[k] = (if Array.isArray(v) then v else [v]).map (t) -> if typeof t is 'string' then { _to: t } else t
    else components[k] = v
  { slug, cls, id, components, relations, body: '' }

mdFiles = (dir, excludes = []) ->
  out = []
  isExcluded = (p) -> excludes.some (x) -> p.indexOf(x) >= 0
  walk = (d) ->
    return if isExcluded(d)
    for ent in await readdir(d, { withFileTypes: true })
      full = join(d, ent.name)
      continue if isExcluded(full)
      if ent.isDirectory() then await walk(full)
      else if extname(ent.name).toLowerCase() is '.md' then out.push(full)
  await walk(dir)
  out

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['suggest', 'partial'] })
  dir = _[0]
  throw new Error("usage: ingest <dir> [--extract Class ...] [--exclude <path> ...] [--partial] [--suggest]") unless dir
  excludes = asArray(flags.exclude)
  files = await mdFiles(dir, excludes)
  throw new Error("no .md files found under #{dir}") unless files.length
  world = await loadWorld(cwd)

  if flags.suggest
    text = (for f in files then await readFile(f, 'utf-8')).join('\n\n---\n\n')
    { suggestions, rationale } = await suggestSchema(cwd, text, { schema: world.schema })
    console.log "# suggested new schema (dry-run — nothing applied)"
    console.log "# #{rationale}" if rationale
    console.log s for s in suggestions
    return 0

  classes = asArray(flags.extract)
  total = 0; written = 0
  w = world
  for f in files
    text = await readFile(f, 'utf-8')
    docs = await extractEntities(cwd, text, { schema: w.schema, classes, world: w })
    entities = []
    for doc in docs
      total++
      e = docToEntity(doc)
      entities.push(e) if e
    try
      res = await batchUpsert(w, entities, { lenient: true })
      written += res.length
      console.log "  ✓ #{r.slug}  (from #{f})" for r in res
      w = await loadWorld(cwd)
    catch err
      console.log "  ✗ batch from #{f} failed: #{err.message}"
  console.log "ingested #{written}/#{total} extracted instance(s) from #{files.length} file(s)"
  console.log "run `brain reindex` to update the search index"
  0
