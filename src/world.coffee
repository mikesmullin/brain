# world.coffee — the read side: load config, schema, and all entities into memory.
import { loadConfig, storageDirs, paths } from './config.coffee'
import { loadSchema } from './schema.coffee'
import { loadEntities } from './storage.coffee'

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
