# refine.coffee — per-class auto-refiner microagents + the iterative `refine` runner.
#
# A refiner is a .coffee module exporting `refine(cwd, entity, { errors, schema, world })`
# that resolves missing/invalid values for ONE entity (e.g. look a Person up in LDAP).
# Lookup order: <cwd>/refiner/<Class>.coffee, then built-in src/refiners/<Class>.coffee.
#
# Runs against the LIVE pglite index via the brain server — never loadWorld / entity .md.
import { existsSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import Agent from 'agl-ai'
import yaml from 'js-yaml'
import { loadConfig } from './config.coffee'
import { loadSchemaContext } from './world.coffee'
import { request } from './client.coffee'
import { parseSlug } from './slug.coffee'
import { serializeEntity } from './storage.coffee'
import { canonicalizeIds, mergeEntities, idFieldOf, isPlaceholderId, setField } from './canonical.coffee'

builtinDir = join(dirname(fileURLToPath(import.meta.url)), 'refiners')

refinerModule = (cwd, cls) ->
  userPath = join(cwd, 'refiner', "#{cls}.coffee")
  builtinPath = join(builtinDir, "#{cls}.coffee")
  path = if existsSync(userPath) then userPath else if existsSync(builtinPath) then builtinPath else null
  return null unless path
  await import(path)

export loadRefiner = (cwd, cls) ->
  mod = await refinerModule(cwd, cls)
  mod?.refine or mod?.default

export loadCalcId = (cwd, cls) ->
  mod = await refinerModule(cwd, cls)
  mod?.calcId

export calcResolver = (cwd) ->
  cache = {}
  (cls, entity) ->
    cache[cls] = (await loadCalcId(cwd, cls)) or null unless cls of cache
    fn = cache[cls]
    if fn then fn(entity) else null

isEmptyEntity = (e) ->
  for own comp, fields of (e.components or {})
    for own k, v of (fields or {})
      return false if v? and String(v).trim()
  for own rel, ts of (e.relations or {})
    return false if ts?.length
  true

# Flatten entity → YAML content for put_entity.
entityToContent = (e) ->
  data = {}
  data[k] = v for own k, v of (e.components or {})
  for own rel, ts of (e.relations or {})
    data[rel] = for t in (ts or [])
      if Object.keys(t).length is 1 and t._to? then t._to else t
  # body is preserved by put_entity when omitted from content
  yaml.dump(data, { lineWidth: 120, sortKeys: false, noRefs: true })

# Load full entity from server into the in-memory shape refiners expect.
fetchEntity = (cwd, slug) ->
  raw = await request(cwd, 'get_entity', { slug, include_links: false })
  {
    slug: raw.slug
    cls: parseSlug(raw.slug).cls
    id: parseSlug(raw.slug).id
    components: raw.components or {}
    relations: raw.relations or {}
    body: raw.body or ''
  }

putEntity = (cwd, entity) ->
  await request(cwd, 'put_entity', { slug: entity.slug, content: entityToContent(entity), overwrite: true })

deleteEntity = (cwd, slug) ->
  await request(cwd, 'delete_entity', { slug })

# Collect slugs mentioned in validate messages that look like Class/id.
slugsFromMessages = (messages) ->
  out = {}
  re = /\b([A-Za-z_][\w]*)\/([^\s:'"]+)/g
  for m in messages
    re.lastIndex = 0
    while (hit = re.exec(m))
      out["#{hit[1]}/#{hit[2]}"] = true
  Object.keys(out)

# One iterative resolution run against the live index. Returns a summary.
export refineAll = (cwd, opts = {}) ->
  cfg = await loadConfig(cwd)
  maxPasses = opts.maxPasses or cfg.refine?.maxPasses or 4
  onlyClass = opts.class
  refinedCount = 0
  createdCount = 0
  renamedCount = 0
  deletedCount = 0
  attempted = {}
  pass = 0
  # Minimal world stub for refiner callbacks (schema only — no entity list).
  schemaWorld = await loadSchemaContext(cwd)

  while pass < maxPasses
    pass++
    res = await request(cwd, 'validate', {})
    schemaWorld = await loadSchemaContext(cwd)  # pick up def changes if any

    targets = {}   # slug -> { entity?, errors, stub? }

    # 1) entities with validation errors or placeholder ids (message-driven)
    for msg in (res.errors or []).concat(res.warnings or [])
      m = /^([^:\s]+):/.exec(msg)
      continue unless m
      slug = m[1]
      continue unless slug.indexOf('/') > 0
      try parseSlug(slug) catch then continue
      { cls } = parseSlug(slug)
      continue if onlyClass and cls isnt onlyClass
      continue unless await loadRefiner(cwd, cls)
      (targets[slug] ?= { errors: [], stub: false }).errors.push(msg)

    # 2) unresolved relation targets referenced in errors
    for slug in slugsFromMessages(res.errors or [])
      continue if targets[slug]
      try { cls, id } = parseSlug(slug) catch then continue
      continue if onlyClass and cls isnt onlyClass
      continue unless await loadRefiner(cwd, cls)
      # only stub if it doesn't exist in the index
      try
        await fetchEntity(cwd, slug)
      catch
        stub = { slug, cls, id, components: {}, relations: {}, body: '' }
        idField = idFieldOf(schemaWorld.schema, cls)
        setField(stub, idField, id) if idField
        targets[slug] = { entity: stub, errors: ["#{slug}: (auto-created relation target)"], stub: true }

    # hydrate non-stub targets from pglite
    for own slug, t of targets when not t.stub
      try
        t.entity = await fetchEntity(cwd, slug)
      catch
        delete targets[slug]

    slugs = Object.keys(targets)
    break if slugs.length is 0

    changedThisPass = false
    total = slugs.length
    i = 0
    for slug in slugs
      i++
      { entity, errors, stub } = targets[slug]
      refiner = await loadRefiner(cwd, entity.cls)
      continue unless refiner
      continue if attempted[slug]

      if not stub and isPlaceholderId(entity.id) and isEmptyEntity(entity)
        # Only delete if nothing points at it — cheap check via get --links style?
        # Skip auto-delete without a full graph scan; leave for manual rm.
        undefined

      process.stderr.write("refine: pass #{pass} [#{i}/#{total}] #{slug}#{if stub then ' (new)' else ''}\n")
      for e in errors when e
        process.stderr.write("        - #{e.replace("#{slug}: ", '')}\n")
      try
        refined = await refiner(cwd, JSON.parse(JSON.stringify(entity)), {
          errors, schema: schemaWorld.schema, world: schemaWorld, cfg, Agent, yaml
        })
      catch err
        process.stderr.write("refiner(#{entity.cls}) failed for #{slug}: #{err.message}\n")
        continue
      refined or= entity

      note = refined._note
      delete refined._note if refined._note?
      if note?.summary
        process.stderr.write("        \u2713 #{note.summary}\n")
      for g in (note?.gaps or []) when g
        process.stderr.write("        \u2717 gap: #{g}\n")

      await canonicalizeIds(schemaWorld.schema, [refined], { calc: calcResolver(cwd) })
      newSlug = refined.slug
      renamed = newSlug isnt slug

      unless renamed or serializeEntity(refined) isnt serializeEntity(entity)
        attempted[slug] = true
        process.stderr.write("        · no change\n")
        continue

      if stub then createdCount++ else refinedCount++

      if renamed
        renamedCount++
        try await deleteEntity(cwd, slug) catch then undefined
        try
          existing = await fetchEntity(cwd, newSlug)
          refined = mergeEntities(existing, refined)
        catch then undefined

      await putEntity(cwd, refined)
      changedThisPass = true

      # Propagate rename: rewrite edges on entities that failed validation mentioning old slug
      if renamed
        for otherSlug in slugsFromMessages(res.errors or [])
          continue if otherSlug is slug or otherSlug is newSlug
          try
            other = await fetchEntity(cwd, otherSlug)
          catch
            continue
          touched = false
          for own rel, ts of (other.relations or {})
            for t in ts when t._to is slug
              t._to = newSlug
              touched = true
          continue unless touched
          for own rel, ts of other.relations
            seen = {}
            other.relations[rel] = ts.filter (t) ->
              return false if seen[t._to]
              seen[t._to] = true
              true
          await putEntity(cwd, other)

    break unless changedThisPass

  { passes: pass, refined: refinedCount, created: createdCount, renamed: renamedCount, deleted: deletedCount }
