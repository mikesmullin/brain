# schema.coffee (command) — inspect the T-box via the live server (pglite).
# Never loadWorld / entity .md — schema structure comes from the server's
# resident schema; instance counts for `graph` come from SQL.
#   schema graph                 yaml+mermaid view with per-class counts
#   schema uniq                  unique component / class / relation names
#   schema components [<name>]   component(s): their fields + methods
#   schema classes [<name>]      class(es): components / top / idField / displayField
#   schema methods <class>       component methods applicable to a class
#   schema orphans [--long]      entities with zero relations (streamed like ls)
#
# Class / component / relation names are painted with the shared soft-rainbow
# 24-bit token colors (same hash as `brain ls`) so the schema scans visually.
import { request, requestStream } from '../client.coffee'
import { loadComponentMethods, signatureOf } from '../components.coffee'
import { formatCount } from '../schema.coffee'
import { parseArgs } from '../args.coffee'
import { runGroupedIdList } from './list-format.coffee'
import {
  useColor
  paintToken
  paintClass
  paintClassLabel
  paintMermaidEdge
} from '../ansi-color.coffee'

# Relation idFields are ALL_UPPERCASE; color those as relation tokens.
paintIdField = (field, colorOn) ->
  s = String(field ? '')
  return s unless s
  if /^[A-Z][A-Z0-9_]*$/.test(s) then paintToken(s, colorOn) else s

fieldDefStr = (fd) ->
  parts = ["type: #{fd.type}"]
  parts.push('required: true') if fd.required
  parts.push('list: true') if fd.list
  parts.push("values: [#{(fd.values or []).join(', ')}]") if fd.values
  parts.push("allowedTypes: [#{(fd.allowedTypes or []).join(', ')}]") if fd.allowedTypes
  "{ #{parts.join(', ')} }"

# alias: ComponentType map — paint the component type names.
inlineMap = (obj, colorOn) ->
  pairs = ("#{k}: #{paintToken(v, colorOn)}" for own k, v of (obj or {}))
  if pairs.length then "{ #{pairs.join(', ')} }" else '{}'

# allowedTypes: [Person, Team] — paint each class token inside the list.
colorAllowedTypes = (fd, colorOn) ->
  return fieldDefStr(fd) unless colorOn and fd.allowedTypes?.length
  parts = ["type: #{fd.type}"]
  parts.push('required: true') if fd.required
  parts.push('list: true') if fd.list
  parts.push("values: [#{(fd.values or []).join(', ')}]") if fd.values
  painted = (paintClass(t, colorOn) for t in fd.allowedTypes).join(', ')
  parts.push("allowedTypes: [#{painted}]")
  "{ #{parts.join(', ')} }"

renderComponents = (cwd, schema, names, colorOn) ->
  lines = ['components:']
  for name in names.sort()
    comp = schema.components?[name]
    throw new Error("unknown component '#{name}'") unless comp
    lines.push "- #{paintToken(name, colorOn)}:"
    fnames = Object.keys(comp.fields or {})
    if fnames.length
      lines.push '  fields:'
      for fn in fnames
        lines.push "  - #{fn}: #{colorAllowedTypes(comp.fields[fn], colorOn)}"
    methods = await loadComponentMethods(cwd, name)
    mnames = Object.keys(methods)
    if mnames.length
      lines.push '  methods:'
      lines.push "  - #{signatureOf(m, methods[m])}" for m in mnames
  lines.join('\n')

renderClasses = (schema, names, colorOn) ->
  lines = ['classes:']
  tops = []
  for name in names.sort()
    cdef = schema.classes?[name]
    throw new Error("unknown class '#{name}'") unless cdef
    lines.push "- #{paintClass(name, colorOn)}:"
    lines.push "  components: #{inlineMap(cdef.components, colorOn)}"
    lines.push "  idField: #{paintIdField(cdef.idField, colorOn)}" if cdef.idField
    lines.push "  displayField: #{cdef.displayField}" if cdef.displayField
    tops.push(name) if cdef.top
  paintedTop = (paintClass(n, colorOn) for n in tops.sort()).join(', ')
  lines.push "top: [#{paintedTop}]"
  lines.join('\n')

renderMethods = (cwd, schema, classNames, colorOn) ->
  lines = ['classes:']
  for cls in classNames.sort()
    cdef = schema.classes?[cls]
    continue unless cdef
    compEntries = []
    for own alias, comp of (cdef.components or {})
      methods = await loadComponentMethods(cwd, comp)
      mnames = Object.keys(methods)
      compEntries.push({ alias, comp, methods, mnames }) if mnames.length
    continue unless compEntries.length
    lines.push "- #{paintClass(cls, colorOn)}:"
    lines.push '  components:'
    for ce in compEntries
      lines.push "  - #{paintToken(ce.comp, colorOn)}: # alias: #{ce.alias}"
      lines.push '    methods: |-'
      for m in ce.mnames
        desc = if ce.methods[m].description then "  # #{ce.methods[m].description}" else ''
        lines.push "      - #{signatureOf(m, ce.methods[m])}#{desc}"
  lines.join('\n')

SCHEMA_HELP = """
brain schema — inspect the T-box (schema)

  brain schema graph                     yaml+mermaid graph view (with per-class counts)
  brain schema uniq                      unique component / class / relation names
  brain schema components [<Component>]  component(s): fields + methods
  brain schema classes [<Class>]         class(es): components (+ the top-class list)
  brain schema methods <Class>           component methods applicable to a class
  brain schema orphans                   # orphan ids, columns (like brain ls)
  brain schema orphans --long            # full Class/id slugs, one per line
"""

# Fetch T-box from the running brain server (authoritative while server is up).
loadSchemaFromServer = (cwd) -> request(cwd, 'schema_info')

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['long'] })
  sub = _[0]
  arg = _[1]
  unless sub
    console.log SCHEMA_HELP
    return 0

  colorOn = useColor()

  if sub is 'orphans'
    # Same streaming list UI as `brain ls` (columns / --long / per-class color).
    return await runGroupedIdList(
      ((onItem) -> requestStream(cwd, 'schema_orphans', {}, onItem))
      { long: !!flags.long, colorOn }
    )

  if sub is 'graph'
    g = await request(cwd, 'schema_graph')
    edges = (g.graph or '').split('\n').filter((l) -> l).sort()
    top = (g.top or []).sort()
    types = (g.types or []).sort()
    lines = ['graph: |-']
    lines.push("  #{paintMermaidEdge(e, colorOn)}") for e in edges
    paintedTop = (paintClassLabel(t, colorOn) for t in top).join(', ')
    lines.push "top: [#{paintedTop}]"
    lines.push 'types:'
    lines.push("- #{paintClassLabel(t, colorOn)}") for t in types
    # Graph-wide totals (nodes = entities, relationships = links, components = T-box types).
    t = g.totals or {}
    lines.push 'totals:'
    lines.push "  nodes: #{formatCount(t.nodes ? 0)}"
    lines.push "  relationships: #{formatCount(t.relationships ? 0)}"
    lines.push "  components: #{formatCount(t.components ? 0)}"
    console.log lines.join('\n')
    return 0

  schema = await loadSchemaFromServer(cwd)
  switch sub
    when 'uniq'
      comps = Object.keys(schema.components or {}).sort()
      classes = Object.keys(schema.classes or {}).sort()
      rels = Object.keys(schema.relations or {}).sort()
      # Flow-style lists — classes, components, and relations all rainbow-hashed.
      paintedComps = (paintToken(c, colorOn) for c in comps).join(', ')
      paintedClasses = (paintClass(c, colorOn) for c in classes).join(', ')
      paintedRels = (paintToken(r, colorOn) for r in rels).join(', ')
      console.log "components: [#{paintedComps}]"
      console.log "classes: [#{paintedClasses}]"
      console.log "relations: [#{paintedRels}]"
    when 'components'
      names = if arg then [arg] else Object.keys(schema.components or {})
      console.log await renderComponents(cwd, schema, names, colorOn)
    when 'classes'
      names = if arg then [arg] else Object.keys(schema.classes or {})
      console.log renderClasses(schema, names, colorOn)
    when 'methods'
      throw new Error("unknown class '#{arg}'") if arg and not schema.classes?[arg]
      names = if arg then [arg] else Object.keys(schema.classes or {})
      console.log await renderMethods(cwd, schema, names, colorOn)
    else
      throw new Error("unknown schema subcommand '#{sub}' (graph|uniq|components|classes|methods|orphans)")
  0
