# schema.coffee — the T-box (ECS-style ontology schema).
#
# schema.yaml (plain YAML, one per storage dir, merged) shape:
#   components: { <Component>: { fields: { <field>: <FieldDef> } } }
#   classes:    { <Class>: {
#                   components: { <localAlias>: <Component> },
#                   top?: bool,
#                   idField?: "alias.field" | RELATION,   # slug/id source (see canonical.coffee)
#                   displayField?: "alias.field",         # human label for UI anchors / labels API
#                 } }
#   relations:  { <REL>: { domain, range, cardinality, qualifiers: { <name>: <FieldDef> } } }
#
# FieldDef: { type, required?, list?, allowedTypes?[, values?] }
#   type in: string | bool | int | date | enum | ref | json   (date = ISO 8601 datetime)
#   allowedTypes: [Class] (ref constraint)   values: [str] (enum constraint)
#
# displayField — which component field is the entity's display name (UI chips, /labels,
# search titles). Path form is the same as idField component paths: "alias.field"
# (e.g. Address → info.address, Person → identity.name). Prefer this over heuristics.
import { join } from 'path'
import { existsSync } from 'fs'
import { readFile } from 'fs/promises'
import yaml from 'js-yaml'
import { getField } from './canonical.coffee'
import { parseSlug } from './slug.coffee'

export FIELD_TYPES = ['string', 'bool', 'int', 'date', 'enum', 'ref', 'json']
export CARDINALITIES = ['oto', 'otn', 'nto', 'mtm']

# Merge schema.yaml from each storage dir into one schema object.
# `classDirs` records which storage dir DEFINED each class, so new instances of that
# class are written back to the dir that owns it (e.g. a private class -> its private dir).
export loadSchema = (storageDirs) ->
  schema = { components: {}, classes: {}, relations: {}, classDirs: {}, sources: [] }
  for dir in storageDirs
    fp = join(dir, 'schema.yaml')
    continue unless existsSync(fp)
    raw = yaml.load(await readFile(fp, 'utf-8')) or {}
    schema.sources.push(fp)
    Object.assign(schema.components, raw.components or {})
    Object.assign(schema.classes, raw.classes or {})
    Object.assign(schema.relations, raw.relations or {})
    schema.classDirs[name] = dir for own name of (raw.classes or {})
  schema

export schemaPath = (storageDir) -> join(storageDir, 'schema.yaml')

# Persist a schema object to a single storage dir's schema.yaml.
export writeSchema = (storageDir, schema) ->
  { writeFile, mkdir } = await import('fs/promises')
  await mkdir(storageDir, { recursive: true })
  out = { components: schema.components or {}, classes: schema.classes or {}, relations: schema.relations or {} }
  await writeFile(schemaPath(storageDir), yaml.dump(out, { lineWidth: 100, noRefs: true, sortKeys: false }), 'utf-8')
  schemaPath(storageDir)

export topClasses = (schema) ->
  (name for own name, def of (schema.classes or {}) when def?.top)

# Resolve the flat field set for a class: { <localAlias>.<field>: FieldDef }.
export classFields = (schema, cls) ->
  out = {}
  cdef = schema.classes?[cls]
  return out unless cdef
  for own alias, compName of (cdef.components or {})
    comp = schema.components?[compName]
    continue unless comp
    for own field, fdef of (comp.fields or {})
      out["#{alias}.#{field}"] = { comp: compName, alias, field, def: fdef }
  out

# Class display-name path ("alias.field"), or null when unset.
export displayFieldOf = (schema, cls) ->
  path = schema?.classes?[cls]?.displayField
  return null unless path? and String(path).trim()
  String(path).trim()

# Map of Class -> displayField path for every class that declares one.
export displayFieldMap = (schema) ->
  out = {}
  for own cls, cdef of (schema?.classes or {})
    path = cdef?.displayField
    out[cls] = String(path).trim() if path? and String(path).trim()
  out

# Resolve a scalar component path value to a trimmed string, or ''.
_displayScalar = (v) ->
  return '' unless v?
  return '' if typeof v is 'object'
  s = String(v).trim()
  s

# Human display name for an entity.
# 1) schema displayField for the entity's class (authoritative when set)
# 2) common name/title heuristics across component bags
# 3) slug fallback
export entityDisplayName = (schema, entity, slug = null) ->
  s = slug or entity?.slug or ''
  cls = entity?.cls
  unless cls
    try cls = parseSlug(s).cls catch then cls = null
  comps = entity?.components or {}

  path = if schema and cls then displayFieldOf(schema, cls) else null
  if path
    v = getField({ components: comps }, path)
    t = _displayScalar(v)
    return t if t

  for key in ['info', 'meta', 'profile', 'identity', 'naming']
    bag = comps[key]
    continue unless bag and typeof bag is 'object'
    for fname in ['name', 'title', 'label', 'display_name', 'full_name', 'address']
      t = _displayScalar(bag[fname])
      return t if t

  for own _alias, fields of comps when fields and typeof fields is 'object'
    for fname in ['name', 'title', 'label', 'display_name', 'full_name', 'address']
      t = _displayScalar(fields[fname])
      return t if t

  s or entity?.slug or ''

# Group thousands for display counts: 814344 → "814,344" (en-US style).
export formatCount = (n) ->
  num = Number(n) or 0
  Math.trunc(num).toLocaleString('en-US')

# A compact mermaid+yaml view of the schema graph (for `schema graph` / ontology).
# When `counts` (class -> instance count) is given, names render as "Name (n)"
# with thousands separators (e.g. Entity (814,344)).
#
# Relations may omit domain/range (wildcard — common in bulk ETLs). Never render
# the JS string "undefined"; fall back to the known class stubs joined by `|`
# so the LLM still sees real class names (e.g. Entity|Officer|… -->|REL| …).
export schemaGraph = (schema, counts = null) ->
  classNames = Object.keys(schema.classes or {})
  classStub = classNames.join('|') or '*'
  endpoint = (v) ->
    s = if v? then String(v).trim() else ''
    if s and s isnt 'undefined' then s else classStub
  edges = []
  for own rel, def of (schema.relations or {})
    edges.push("#{endpoint(def.domain)} -->|#{rel}| #{endpoint(def.range)}")
  label = (name) -> if counts then "#{name} (#{formatCount(counts[name] or 0)})" else name
  {
    graph: edges.join('\n')
    top: (label(n) for n in topClasses(schema))
    types: (label(n) for n in classNames)
  }
