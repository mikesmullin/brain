# world.coffee — disk load helpers.
#
# `loadWorld` reads every entity .md into memory. That is ONLY for
# `brain reindex` (the sanctioned .md → pglite import). All query / validate /
# schema commands must use the server + pglite instead.
#
# `loadSchemaContext` loads config + T-box schema.yaml only (no entities) —
# for import tools that need the schema while writing via put_entity / .md.
import { loadConfig, storageDirs, paths } from './config.coffee'
import { loadSchema } from './schema.coffee'
import { loadEntities } from './storage.coffee'

# T-box + paths only. entities/bySlug are empty — never scans Class/*.md.
export loadSchemaContext = (cwd = process.cwd()) ->
  cfg = await loadConfig(cwd)
  dirs = await storageDirs(cwd, cfg)
  schema = await loadSchema(dirs)
  {
    cwd, cfg
    storageDirs: dirs
    primaryStorageDir: paths(cwd).storage
    schema
    entities: []
    bySlug: {}
    duplicates: []
    parseErrors: []
  }

# FULL A-box load from disk. Reserved for reindex (and nothing else).
export loadWorld = (cwd = process.cwd()) ->
  cfg = await loadConfig(cwd)
  dirs = await storageDirs(cwd, cfg)
  schema = await loadSchema(dirs)
  { entities, errors, bySlug, duplicates } = await loadEntities(dirs)
  {
    cwd, cfg
    storageDirs: dirs
    primaryStorageDir: paths(cwd).storage   # where new writes land by default
    schema
    entities, bySlug, duplicates, parseErrors: errors
  }

# Resolve a user-supplied slug to an existing entity, tolerating case. Classes are
# ProperCase and ids are lowercase, so `brain get person/JDOE` == `Person/jdoe`:
# we match the class name case-insensitively and lowercase the id. Returns the entity
# (with its canonical .slug) or undefined.
export resolveSlug = (world, input) ->
  raw = String(input ? '').trim()
  return world.bySlug[raw] if world.bySlug[raw]        # exact fast-path
  i = raw.indexOf('/')
  return undefined if i <= 0
  clsLower = raw.slice(0, i).toLowerCase()
  id = raw.slice(i + 1).toLowerCase()
  classes = new Set(Object.keys(world.schema?.classes or {}))
  classes.add(e.cls) for e in world.entities
  for cls from classes when cls.toLowerCase() is clsLower
    hit = world.bySlug["#{cls}/#{id}"]
    return hit if hit
  undefined
