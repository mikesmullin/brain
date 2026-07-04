# canonical.coffee — canonical id derivation for classes that declare an `idField`.
#
# A class may declare `idField: "alias.field"` in its schema (e.g. Person ->
# "identity.username"). The entity's id (and thus its filename/slug) is DERIVED
# from that field, which eliminates the name-vs-username duplication problem.
#
# When the idField value is not yet known (e.g. a person seen by name in a doc,
# before LDAP resolution), a DETERMINISTIC placeholder id is used: an 8-hex SHA-1
# of stable identifying content. Placeholder ids are recognized by regex and only
# apply to idField classes, so they never collide with hash-ids of non-idField
# classes (Task/Queue). A later `brain refine` resolves the real id and renames.
import { createHash } from 'crypto'
import { formatSlug, parseSlug } from './slug.coffee'

PLACEHOLDER_RE = /^[0-9a-f]{8}$/
export isPlaceholderId = (id) -> PLACEHOLDER_RE.test(String(id ? ''))

export placeholderId = (parts...) ->
  s = parts.filter((p) -> p?).join('|').toLowerCase().trim()
  createHash('sha1').update(s or String(Math.random())).digest('hex').slice(0, 8)

export idFieldOf = (schema, cls) -> schema.classes?[cls]?.idField

# An idField may be a component field ("alias.field") OR a relation name (ALL_UPPERCASE),
# in which case the id derives from the basename of that relation's single target.
RELATION_IDFIELD_RE = /^[A-Z][A-Z0-9_]*$/
export isRelationIdField = (idField) -> !!idField and RELATION_IDFIELD_RE.test(idField)

export getField = (entity, path) ->
  return undefined unless path
  [a, f] = path.split('.')
  entity.components?[a]?[f]

# Resolve the id-source value for either kind of idField.
export idValueOf = (entity, idField) ->
  return undefined unless idField
  if isRelationIdField(idField)
    targets = entity.relations?[idField]
    return undefined unless targets?.length
    try (parseSlug(targets[0]._to).id) catch then undefined
  else
    getField(entity, idField)

export setField = (entity, path, val) ->
  [a, f] = path.split('.')
  entity.components ?= {}
  entity.components[a] ?= {}
  entity.components[a][f] = val

# Stable identifying content for an entity's placeholder id.
identity = (e) ->
  nm = getField(e, 'identity.name') or e.components?.naming?.name
  em = getField(e, 'contact.email')
  placeholderId(e.cls, nm, em, (if not nm and not em then e.id else null))

# Assign canonical ids to entities of idField classes; returns renameMap {oldSlug -> newSlug}.
# id resolution order for an entity whose idField VALUE is absent:
#   1) opts.calc(cls, entity)  — CALCULATED_FIELD (deterministic, e.g. email->username)
#   2) keep an existing placeholder id
#   3) derive a new hash placeholder (last resort)
# When calc yields a value, it is written back into the idField (so it's consistent).
export canonicalizeIds = (schema, entities, opts = {}) ->
  calc = opts.calc
  renameMap = {}
  for e in entities
    idField = idFieldOf(schema, e.cls)
    continue unless idField
    v = idValueOf(e, idField)
    newId = null
    if v? and String(v).trim()
      newId = String(v).trim()
    else
      computed = if calc then (await calc(e.cls, e)) else null
      if computed
        setField(e, idField, computed) unless isRelationIdField(idField)
        newId = computed
      else if isPlaceholderId(e.id)
        newId = e.id
      else
        newId = identity(e)
    # the id (filename/slug basename) is ALWAYS lowercase, even though the stored idField
    # value keeps its original casing (e.g. identity.username 'JDoe' -> id 'jdoe')
    newId = String(newId).toLowerCase() if newId?
    if newId isnt e.id
      old = e.slug
      e.id = newId
      e.slug = formatSlug(e.cls, newId)
      renameMap[old] = e.slug
  for e in entities
    for own rel, targets of (e.relations or {})
      t._to = renameMap[t._to] for t in targets when renameMap[t._to]
  renameMap

# Merge `incoming` into `base` (incoming wins on scalar fields; relations are unioned by _to).
export mergeEntities = (base, incoming) ->
  out = { slug: base.slug, cls: base.cls, id: base.id, body: base.body or incoming.body or '', components: {}, relations: {} }
  for src in [base, incoming]
    for own comp, fields of (src.components or {})
      out.components[comp] ?= {}
      out.components[comp][k] = v for own k, v of (fields or {}) when v?
    for own rel, targets of (src.relations or {})
      out.relations[rel] ?= []
      for t in targets
        out.relations[rel].push(t) unless out.relations[rel].some((x) -> x._to is t._to)
  out
