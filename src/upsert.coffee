# upsert.coffee — the single sanctioned write path for A-box instances.
# Validates the candidate world in-memory FIRST, and only writes .md on success
# (write-then-validate-then-rollback guarantee, done proactively).
import { writeEntityFile } from './storage.coffee'
import { reconcileBodyLinks } from './storage.coffee'
import { validateData } from './validate.coffee'
import { canonicalizeIds, mergeEntities } from './canonical.coffee'
import { calcResolver } from './refine.coffee'

# Merge a candidate entity into a shallow copy of the world's indexes.
candidateWorld = (world, entity) ->
  bySlug = Object.assign({}, world.bySlug)
  existing = bySlug[entity.slug]
  bySlug[entity.slug] = entity
  entities = world.entities.filter (e) -> e.slug isnt entity.slug
  entities.push(entity)
  { schema: world.schema, entities, bySlug, duplicates: [] }

# Validate only; return { valid, errors, warnings } (errors scoped to this slug).
export validateCandidate = (world, entity) ->
  res = validateData(candidateWorld(world, entity))
  slugErrors = res.errors.filter (m) -> m.startsWith("#{entity.slug}:") or m.indexOf("'#{entity.slug}'") >= 0
  { valid: slugErrors.length is 0, errors: slugErrors, allErrors: res.errors, warnings: res.warnings }

# Upsert an entity: validate candidate, then write to the target storage dir.
export upsertEntity = (world, entity, opts = {}) ->
  existing = world.bySlug[entity.slug]
  storageDir = opts.storageDir
  storageDir or= dirOfSource(existing.source, entity.cls, entity.id) if existing?.source
  storageDir or= world.schema.classDirs?[entity.cls]
  storageDir or= world.primaryStorageDir
  reconcileBodyLinks(entity)
  res = validateCandidate(world, entity)
  unless res.valid
    err = new Error("validation failed for #{entity.slug}:\n  " + res.errors.join('\n  '))
    err.validation = res
    throw err
  fp = await writeEntityFile(storageDir, entity)
  { path: fp, warnings: res.warnings.filter((m) -> m.indexOf(entity.slug) >= 0) }

# Batch upsert: canonicalize ids (idField-derived, placeholders where unknown),
# validate ALL candidates together (so forward references resolve), then write.
# Refinement is NOT done here — run `brain refine` afterwards to resolve/repair.
export batchUpsert = (world, entities, opts = {}) ->
  storageDir = opts.storageDir or world.primaryStorageDir
  reconcileBodyLinks(e) for e in entities
  await canonicalizeIds(world.schema, entities, { calc: calcResolver(world.cwd) })
  # de-dup batch by slug (merge duplicates that canonicalized to the same id)
  bySlugLocal = {}
  deduped = []
  for e in entities
    if bySlugLocal[e.slug]
      merged = mergeEntities(bySlugLocal[e.slug], e)
      idx = deduped.indexOf(bySlugLocal[e.slug])
      deduped[idx] = merged
      bySlugLocal[e.slug] = merged
    else
      bySlugLocal[e.slug] = e
      deduped.push(e)
  entities = deduped
  # DATA SAFETY: never clobber an existing record. If the canonical slug already
  # exists, keep all existing values and only ADD net-new fields (existing-wins).
  for e, i in entities when world.bySlug[e.slug]
    entities[i] = mergeEntities(e, world.bySlug[e.slug])   # base=incoming, existing applied on top -> existing wins
  newSlugs = (e.slug for e in entities)

  bySlug = Object.assign({}, world.bySlug)
  bySlug[e.slug] = e for e in entities
  merged = world.entities.filter((e) -> e.slug not in newSlugs).concat(entities)
  res = validateData({ schema: world.schema, entities: merged, bySlug, duplicates: [] }, { lenient: opts.lenient })
  errs = res.errors.filter (m) -> newSlugs.some (s) -> m.startsWith("#{s}:") or m.indexOf("'#{s}'") >= 0
  if errs.length
    err = new Error("batch validation failed:\n  " + errs.join('\n  '))
    err.errors = errs
    throw err
  written = []
  for e in entities
    # write existing entities back to their own storage dir; route NEW ones to the dir
    # that defines their class (falls back to the primary storage dir)
    dir = if world.bySlug[e.slug]?.source then dirOfSource(world.bySlug[e.slug].source, e.cls, e.id) or storageDir else (world.schema.classDirs?[e.cls] or storageDir)
    written.push({ slug: e.slug, path: await writeEntityFile(dir, e) })
  written

dirOfSource = (source, cls, id) ->
  suffix = "/#{cls}/#{id}.md"
  if source and source.endsWith(suffix) then source.slice(0, source.length - suffix.length) else null
