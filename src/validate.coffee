# validate.coffee — schema validation + lint (including orphan detection).
import { parseSlug } from './slug.coffee'
import { classFields } from './schema.coffee'
import { idFieldOf, isPlaceholderId, getField, idValueOf, isRelationIdField } from './canonical.coffee'

checkScalar = (fdef, val) ->
  switch fdef.type
    when 'string' then typeof val is 'string'
    when 'bool' then typeof val is 'boolean'
    when 'int' then typeof val is 'number' and Number.isInteger(val)
    when 'date'
      # ISO 8601 datetime: a string parseable by Date, OR a Date object (js-yaml
      # auto-parses unquoted ISO timestamps into Date instances on load)
      if val instanceof Date then not isNaN(val.getTime())
      else typeof val is 'string' and not isNaN(Date.parse(val))
    when 'enum' then typeof val is 'string' and (fdef.values or []).includes(val)
    when 'ref' then typeof val is 'string' and isSlugRef(val)
    when 'json'
      return true if val? and typeof val is 'object'   # already a JSON value (object/array)
      return false unless typeof val is 'string'
      try (JSON.parse(val); true) catch then false
    else false

isSlugRef = (s) ->
  try (parseSlug(s); true) catch then false

# Validate one field's value against its definition; push errors into `errs`.
validateField = (errs, slug, key, fdef, val, bySlug) ->
  if fdef.list
    unless Array.isArray(val)
      errs.push("#{slug}: #{key} must be a list")
      return
    for item, i in val
      unless checkScalar(fdef, item)
        errs.push("#{slug}: #{key}[#{i}] is not a valid #{fdef.type}")
      else
        checkRefTarget(errs, slug, "#{key}[#{i}]", fdef, item, bySlug)
  else
    unless checkScalar(fdef, val)
      errs.push("#{slug}: #{key} is not a valid #{fdef.type}")
    else
      checkRefTarget(errs, slug, key, fdef, val, bySlug)

checkRefTarget = (errs, slug, key, fdef, val, bySlug) ->
  return unless fdef.type is 'ref'
  s = parseSlug(val)
  if fdef.allowedTypes and fdef.allowedTypes.length and not fdef.allowedTypes.includes(s.cls)
    errs.push("#{slug}: #{key} ref '#{val}' class '#{s.cls}' not in allowedTypes [#{fdef.allowedTypes.join(', ')}]")
  errs.push("#{slug}: #{key} ref '#{val}' does not resolve to an existing entity") unless bySlug[s.slug]

export validateData = ({ schema, entities, bySlug, duplicates }, opts = {}) ->
  errors = []
  warnings = []
  degree = {}   # slug -> in+out relation count (for orphan detection)
  bump = (s) -> degree[s] = (degree[s] or 0) + 1
  # in lenient mode, missing-required is a warning (partial ingest), not an error
  requiredSink = if opts.lenient then warnings else errors

  for d in (duplicates or [])
    errors.push("duplicate slug '#{d.slug}' in: #{d.sources.join(', ')}")

  for e in entities
    cdef = schema.classes?[e.cls]
    unless cdef
      errors.push("#{e.slug}: unknown class '#{e.cls}'")
      continue
    fields = classFields(schema, e.cls)

    # filename must be all-lowercase (basename + extension)
    base = if e.source then e.source.split('/').pop() else "#{e.id}.md"
    if base isnt base.toLowerCase()
      errors.push("#{e.slug}: filename '#{base}' must be all-lowercase (rename to '#{base.toLowerCase()}')")

    # components + fields
    for own alias, values of (e.components or {})
      unless cdef.components?[alias]
        errors.push("#{e.slug}: component '#{alias}' is not declared on class '#{e.cls}'")
        continue
      compName = cdef.components[alias]
      comp = schema.components?[compName]
      for own field, val of (values or {})
        key = "#{alias}.#{field}"
        meta = fields[key]
        unless meta
          errors.push("#{e.slug}: unknown field '#{key}' (component #{compName})")
          continue
        validateField(errors, e.slug, key, meta.def, val, bySlug)
    # required fields present
    for own key, meta of fields when meta.def.required
      present = e.components?[meta.alias]?[meta.field]?
      requiredSink.push("#{e.slug}: required field '#{key}' is missing") unless present

    # idField (canonical id derivation) consistency
    idField = idFieldOf(schema, e.cls)
    if idField
      # a relation-based idField must resolve from exactly ONE target
      if isRelationIdField(idField)
        n = (e.relations?[idField] or []).length
        errors.push("#{e.slug}: idField relation '#{idField}' must have exactly one target (has #{n})") if n > 1
      v = idValueOf(e, idField)
      if isPlaceholderId(e.id)
        requiredSink.push("#{e.slug}: placeholder id — unresolved #{idField} (run `brain refine`)")
      else if not v?
        warnings.push("#{e.slug}: id-field #{idField} is unset (should equal id '#{e.id}')")
      else if String(v).toLowerCase() isnt String(e.id).toLowerCase()
        errors.push("#{e.slug}: id must equal #{idField} value ('#{v}') — rename to #{e.cls}/#{String(v).toLowerCase()}")

    # relations
    hasRel = false
    for own rel, targets of (e.relations or {})
      rdef = schema.relations?[rel]
      unless rdef
        if rel is 'LINKS_TO'
          # reserved implicit system relation (wildcard domain/range, from body [[..]] links)
          for t in targets
            hasRel = true
            bump(e.slug)
            try
              ts = parseSlug(t._to)
              bump(ts.slug)
              errors.push("#{e.slug}: LINKS_TO target '#{t._to}' does not resolve") unless bySlug[ts.slug]
            catch err
              errors.push("#{e.slug}: LINKS_TO target invalid: #{err.message}")
          continue
        errors.push("#{e.slug}: unknown relation '#{rel}'")
        continue
      if rdef.domain and rdef.domain isnt e.cls
        errors.push("#{e.slug}: relation '#{rel}' domain is '#{rdef.domain}', not '#{e.cls}'")
      for t in targets
        hasRel = true
        bump(e.slug)
        try
          ts = parseSlug(t._to)
          bump(ts.slug)
          if rdef.range and rdef.range isnt ts.cls
            errors.push("#{e.slug}: relation '#{rel}' -> '#{t._to}' range is '#{rdef.range}', not '#{ts.cls}'")
          errors.push("#{e.slug}: relation '#{rel}' target '#{t._to}' does not resolve") unless bySlug[ts.slug]
        catch err
          errors.push("#{e.slug}: relation '#{rel}' target invalid: #{err.message}")
        # qualifiers
        for own qk, qv of t when qk isnt '_to'
          qdef = rdef.qualifiers?[qk]
          unless qdef
            errors.push("#{e.slug}: relation '#{rel}' has unknown qualifier '#{qk}'")
            continue
          validateField(errors, e.slug, "#{rel}.#{qk}", qdef, qv, bySlug)

  # orphan detection (lint warning): no incoming or outgoing relations at all
  for e in entities
    warnings.push("orphan: #{e.slug} has no relations (incoming or outgoing)") unless degree[e.slug]

  {
    valid: errors.length is 0
    errors
    warnings
    counts: { entities: entities.length, classes: Object.keys(schema.classes or {}).length, relations: Object.keys(schema.relations or {}).length }
  }
