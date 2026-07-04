# refine.coffee — per-class auto-refiner microagents + the iterative `refine` runner.
#
# A refiner is a .coffee module exporting `refine(cwd, entity, { errors, schema, world })`
# that resolves missing/invalid values for ONE entity (e.g. look a Person up in LDAP).
# Lookup order: <cwd>/.brain/refiners/<Class>.coffee, then built-in src/refiners/<Class>.coffee.
#
# `refineAll` is the batch-level driver (recursion lives HERE, not in any refiner):
#   - find invalid / placeholder-id entities whose class has a refiner -> refine them
#   - find unresolved relation targets whose class has a refiner -> create stubs -> refine them
#   - canonicalize ids (rename placeholder -> resolved, merge into any existing entity)
#   - repeat up to maxPasses (bounds the manager-chain recursion)
import { existsSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import Agent from 'agl-ai'
import yaml from 'js-yaml'
import { loadConfig } from './config.coffee'
import { loadWorld } from './world.coffee'
import { validateData } from './validate.coffee'
import { parseSlug } from './slug.coffee'
import { writeEntityFile, removeEntityFile, serializeEntity } from './storage.coffee'
import { canonicalizeIds, mergeEntities, idFieldOf, isPlaceholderId, setField } from './canonical.coffee'

builtinDir = join(dirname(fileURLToPath(import.meta.url)), 'refiners')

refinerModule = (cwd, cls) ->
  # cwd-local refiners (per-dataset, may be proprietary) take precedence over the
  # global built-ins bundled in src/refiners/ (which are .gitignored in this repo).
  userPath = join(cwd, 'refiner', "#{cls}.coffee")
  builtinPath = join(builtinDir, "#{cls}.coffee")
  path = if existsSync(userPath) then userPath else if existsSync(builtinPath) then builtinPath else null
  return null unless path
  await import(path)

export loadRefiner = (cwd, cls) ->
  mod = await refinerModule(cwd, cls)
  mod?.refine or mod?.default

# CALCULATED_FIELD resolver: a per-class deterministic id calculator, if the refiner defines one.
export loadCalcId = (cwd, cls) ->
  mod = await refinerModule(cwd, cls)
  mod?.calcId

# Build the calc resolver passed to canonicalizeIds: (cls, entity) -> canonical id | null
export calcResolver = (cwd) ->
  cache = {}
  (cls, entity) ->
    cache[cls] = (await loadCalcId(cwd, cls)) or null unless cls of cache
    fn = cache[cls]
    if fn then fn(entity) else null

# derive the storage dir that holds an entity file (source = <dir>/<Class>/<id>.md)
storageDirOf = (source, cls, id) ->
  suffix = "/#{cls}/#{id}.md"
  if source and source.endsWith(suffix) then source.slice(0, source.length - suffix.length) else null

# an entity carries no usable data: every component field is empty and it has no relations
isEmptyEntity = (e) ->
  for own comp, fields of (e.components or {})
    for own k, v of (fields or {})
      return false if v? and String(v).trim()
  for own rel, ts of (e.relations or {})
    return false if ts?.length
  true

# One iterative resolution run. Returns a summary.
export refineAll = (cwd, opts = {}) ->
  cfg = await loadConfig(cwd)
  maxPasses = opts.maxPasses or cfg.refine?.maxPasses or 4
  onlyClass = opts.class
  primary = null
  refinedCount = 0
  createdCount = 0
  renamedCount = 0
  deletedCount = 0
  attempted = {}   # slugs refined with no net change — never re-invoke the LLM again this run
  pass = 0

  while pass < maxPasses
    pass++
    world = await loadWorld(cwd)
    primary or= world.primaryStorageDir
    res = validateData(world, { lenient: false })

    # 1) existing entities that are invalid or carry a placeholder id (and have a refiner)
    targets = {}   # slug -> { entity, errors, stub? }
    for e in world.entities
      continue if onlyClass and e.cls isnt onlyClass
      errs = res.errors.filter (m) -> m.startsWith("#{e.slug}:")
      if (errs.length or isPlaceholderId(e.id)) and (await loadRefiner(cwd, e.cls))
        targets[e.slug] = { entity: e, errors: errs }

    # 2) unresolved relation targets whose class has a refiner -> create stubs
    for e in world.entities
      for own rel, ts of (e.relations or {})
        for t in ts
          slug = t._to
          continue if world.bySlug[slug] or targets[slug]
          parsed = null
          try parsed = parseSlug(slug) catch then parsed = null
          continue unless parsed
          { cls, id } = parsed
          continue if onlyClass and cls isnt onlyClass
          continue unless await loadRefiner(cwd, cls)
          stub = { slug, cls, id, components: {}, relations: {}, body: '' }
          idField = idFieldOf(world.schema, cls)
          setField(stub, idField, id) if idField   # target slug id IS the canonical id
          targets[slug] = { entity: stub, errors: ["#{slug}: (auto-created relation target)"], stub: true }

    slugs = Object.keys(targets)
    break if slugs.length is 0

    # slugs referenced by any other entity's relations (never auto-delete these)
    referenced = {}
    for e in world.entities
      for own rel, ts of (e.relations or {})
        referenced[t._to] = true for t in ts when t?._to

    changedThisPass = false
    total = slugs.length
    i = 0
    for slug in slugs
      i++
      { entity, errors, stub } = targets[slug]
      refiner = await loadRefiner(cwd, entity.cls)
      continue unless refiner
      continue if attempted[slug]   # already tried this run and it didn't change

      # deterministic prune: an empty record with only a placeholder id and nothing
      # referencing it can never be resolved — delete it without invoking the LLM
      if not stub and entity.source and isPlaceholderId(entity.id) and isEmptyEntity(entity) and not referenced[slug]
        dir = storageDirOf(entity.source, entity.cls, entity.id) or primary
        await removeEntityFile(dir, entity.cls, entity.id)
        deletedCount++
        changedThisPass = true
        process.stderr.write("refine: pass #{pass} [#{i}/#{total}] #{slug} — deleted (empty placeholder record)\n")
        continue

      process.stderr.write("refine: pass #{pass} [#{i}/#{total}] #{slug}#{if stub then ' (new)' else ''}\n")
      for e in errors when e
        process.stderr.write("        - #{e.replace("#{slug}: ", '')}\n")
      try
        refined = await refiner(cwd, JSON.parse(JSON.stringify(entity)), { errors, schema: world.schema, world, cfg, Agent, yaml })
      catch err
        process.stderr.write("refiner(#{entity.cls}) failed for #{slug}: #{err.message}\n")
        continue
      refined or= entity

      # report the refiner's rationale + any unresolved gaps, then drop the transient note
      note = refined._note
      delete refined._note if refined._note?
      if note?.summary
        process.stderr.write("        \u2713 #{note.summary}\n")
      for g in (note?.gaps or []) when g
        process.stderr.write("        \u2717 gap: #{g}\n")

      # canonicalize this single entity (placeholder/calc -> resolved id)
      await canonicalizeIds(world.schema, [refined], { calc: calcResolver(cwd) })
      newSlug = refined.slug
      renamed = newSlug isnt slug

      # only persist + count when the refiner actually changed something (or renamed);
      # otherwise mark it attempted so unresolvable records aren't re-tried every pass
      unless renamed or serializeEntity(refined) isnt serializeEntity(entity)
        attempted[slug] = true
        process.stderr.write("        · no change\n")
        continue

      if stub then createdCount++ else refinedCount++
      dir = storageDirOf(entity.source, entity.cls, entity.id) or primary

      if renamed
        renamedCount++
        await removeEntityFile(dir, entity.cls, entity.id) if entity.source
        if world.bySlug[newSlug]
          refined = mergeEntities(world.bySlug[newSlug], refined)
          dir = storageDirOf(world.bySlug[newSlug].source, refined.cls, refined.id) or dir
      await writeEntityFile(dir, refined)
      changedThisPass = true

      # propagate the rename to every OTHER entity that referenced the old slug, so a
      # name-slug edge (Person/jane-doe) is rewritten to the resolved id (Person/jdoe)
      # and de-duplicated — otherwise the stale edge keeps re-creating the stub each pass
      if renamed
        for other in world.entities when other.slug isnt slug and other.slug isnt newSlug
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
          odir = storageDirOf(other.source, other.cls, other.id) or primary
          await writeEntityFile(odir, other)

    break unless changedThisPass

  { passes: pass, refined: refinedCount, created: createdCount, renamed: renamedCount, deleted: deletedCount }
