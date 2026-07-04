# schema.coffee — the T-box (ECS-style ontology schema).
#
# schema.yaml (plain YAML, one per storage dir, merged) shape:
#   components: { <Component>: { fields: { <field>: <FieldDef> } } }
#   classes:    { <Class>: { components: { <localAlias>: <Component> }, top: bool } }
#   relations:  { <REL>: { domain, range, cardinality, qualifiers: { <name>: <FieldDef> } } }
#
# FieldDef: { type, required?, list?, allowedTypes?[, values?] }
#   type in: string | bool | int | date | enum | ref | json   (date = ISO 8601 datetime)
#   allowedTypes: [Class] (ref constraint)   values: [str] (enum constraint)
import { join } from 'path'
import { existsSync } from 'fs'
import { readFile } from 'fs/promises'
import yaml from 'js-yaml'

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

# A compact mermaid+yaml view of the schema graph (for `schema graph`).
# When `counts` (class -> instance count) is given, names render as "Name (n)".
export schemaGraph = (schema, counts = null) ->
  edges = []
  for own rel, def of (schema.relations or {})
    edges.push("#{def.domain} -->|#{rel}| #{def.range}")
  label = (name) -> if counts then "#{name} (#{counts[name] or 0})" else name
  {
    graph: edges.join('\n')
    top: (label(n) for n in topClasses(schema))
    types: (label(n) for n in Object.keys(schema.classes or {}))
  }
