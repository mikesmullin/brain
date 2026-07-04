# components.coffee — ECS-style "component methods": reusable behaviors attached to a
# schema Component, callable on any entity whose class uses that component.
#
# A component module lives at <cwd>/components/<Component>.coffee (per-dataset) or the
# built-in src/components/<Component>.coffee, and exports:
#
#   export methods =
#     Method__name:
#       description: 'what it does'
#       parameters: { arg: { type: 'string', description: '...' } }   # tool-style schema
#       required: ['arg']
#       fn: (e, k, args) -> { success: true, content: 'affirmation or proof' }
#
# The method fn signature is `fn(entity, alias, args)`:
#   - `entity` is the loaded entity object (mutate it in place to persist changes)
#   - `alias`  is the component's local alias on the class (a class may use a component
#              under multiple aliases; the alias tells the method which instance to touch)
#   - `args`   is the parsed parameter object
# Return the contract shape `{ success, error?, content, ... }` (content is always a
# string shown to the CLI/LLM). Returning nothing implies success with a generic message.
import { existsSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import yaml from 'js-yaml'
import { writeEntityFile, serializeEntity } from './storage.coffee'

builtinDir = join(dirname(fileURLToPath(import.meta.url)), 'components')

componentModule = (cwd, comp) ->
  userPath = join(cwd, 'components', "#{comp}.coffee")
  builtinPath = join(builtinDir, "#{comp}.coffee")
  path = if existsSync(userPath) then userPath else if existsSync(builtinPath) then builtinPath else null
  return null unless path
  await import(path)

# The exported method map for a component ({} if none).
export loadComponentMethods = (cwd, comp) ->
  mod = await componentModule(cwd, comp)
  mod?.methods or {}

# Every method applicable to an entity's class, across all component aliases it uses.
# Returns [{ method, alias, comp, def }] (def = the method definition object).
export applicableMethods = (cwd, schema, cls) ->
  out = []
  cdef = schema.classes?[cls]
  return out unless cdef
  for own alias, comp of (cdef.components or {})
    methods = await loadComponentMethods(cwd, comp)
    for own name, def of methods
      out.push({ method: name, alias, comp, def })
  out

# derive the storage dir that holds an entity file (source = <dir>/<Class>/<id>.md)
storageDirOf = (e) ->
  suffix = "/#{e.cls}/#{e.id}.md"
  if e.source and e.source.endsWith(suffix) then e.source.slice(0, e.source.length - suffix.length) else null

toStr = (v) -> if typeof v is 'string' then v else yaml.dump(v, { lineWidth: 120, sortKeys: false, noRefs: true }).trimEnd()

# normalize any method return into the { success, error?, content } contract.
# The LLM/combined view is `(if !success then "#{error}\n") + "#{content}"`, so:
#   - on success we only need `content` (defaults to 'ok')
#   - on failure we keep `error` and `content` separate (no copying); if a failure
#     supplies NEITHER, we fill `error` with a generic placeholder
UNSPECIFIED = 'an unspecified error occurred'
normalizeResult = (r) ->
  return { success: true, content: 'ok' } unless r?
  unless typeof r is 'object' and ('success' of r)
    return { success: true, content: toStr(r) }
  success = !!r.success
  error = if r.error? then toStr(r.error) else undefined
  content = if r.content? then toStr(r.content) else undefined
  if success
    content ?= 'ok'
  else
    error ?= UNSPECIFIED unless content   # a failure with neither gets a placeholder error
  { success, error, content, extra: r }

# Invoke one component method on a slug's entity; persists the entity if it changed.
# Returns { success, error?, content }.
export invokeMethod = (cwd, world, slug, methodName, args = {}) ->
  e = world.bySlug[slug]
  return { success: false, error: "not found: #{slug}", content: '' } unless e
  applicable = await applicableMethods(cwd, world.schema, e.cls)
  hit = applicable.find (m) -> m.method is methodName
  return { success: false, error: "#{e.cls} has no component method '#{methodName}'", content: '' } unless hit

  before = serializeEntity(e)
  result = normalizeResult(await hit.def.fn(e, hit.alias, args))
  # persist if the method mutated the entity
  if result.success and serializeEntity(e) isnt before
    dir = storageDirOf(e) or world.primaryStorageDir
    await writeEntityFile(dir, e)
  result

# Render a compact CLI signature for a method: "Method(p1: type, p2: type)".
export signatureOf = (name, def) ->
  params = for own p, pd of (def.parameters or {})
    t = pd?.type or 'any'
    if (def.required or []).includes(p) then "#{p}: #{t}" else "#{p}?: #{t}"
  "#{name}(#{params.join(', ')})"
