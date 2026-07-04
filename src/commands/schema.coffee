# schema.coffee (command) — inspect the T-box.
#   schema graph                 yaml+mermaid view (graph:, top:, types:) with per-class counts
#   schema uniq                  unique component / class / relation names
#   schema components [<name>]   component(s): their fields + methods
#   schema classes [<name>]      class(es): their components / top / idField
#   schema methods <class>       component methods applicable to a class
import { loadWorld } from '../world.coffee'
import { schemaGraph } from '../schema.coffee'
import { loadComponentMethods, signatureOf } from '../components.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

fieldDefStr = (fd) ->
  parts = ["type: #{fd.type}"]
  parts.push('required: true') if fd.required
  parts.push('list: true') if fd.list
  parts.push("values: [#{(fd.values or []).join(', ')}]") if fd.values
  parts.push("allowedTypes: [#{(fd.allowedTypes or []).join(', ')}]") if fd.allowedTypes
  "{ #{parts.join(', ')} }"

inlineMap = (obj) ->
  pairs = ("#{k}: #{v}" for own k, v of (obj or {}))
  if pairs.length then "{ #{pairs.join(', ')} }" else '{}'

# compact bullet outline for one component: fields (inline defs) + methods (signatures)
renderComponents = (cwd, schema, names) ->
  lines = ['components:']
  for name in names.sort()
    comp = schema.components?[name]
    throw new Error("unknown component '#{name}'") unless comp
    lines.push "- #{name}:"
    fnames = Object.keys(comp.fields or {})
    if fnames.length
      lines.push '  fields:'
      lines.push "  - #{fn}: #{fieldDefStr(comp.fields[fn])}" for fn in fnames
    methods = await loadComponentMethods(cwd, name)
    mnames = Object.keys(methods)
    if mnames.length
      lines.push '  methods:'
      lines.push "  - #{signatureOf(m, methods[m])}" for m in mnames
  lines.join('\n')

renderClasses = (schema, names) ->
  lines = ['classes:']
  tops = []
  for name in names.sort()
    cdef = schema.classes?[name]
    throw new Error("unknown class '#{name}'") unless cdef
    lines.push "- #{name}:"
    lines.push "  components: #{inlineMap(cdef.components)}"
    tops.push(name) if cdef.top
  lines.push "top: [#{tops.sort().join(', ')}]"
  lines.join('\n')

# class -> components (that have methods) -> alias + method signatures (as a block scalar)
renderMethods = (cwd, schema, classNames) ->
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
    lines.push "- #{cls}:"
    lines.push '  components:'
    for ce in compEntries
      lines.push "  - #{ce.comp}: # alias: #{ce.alias}"
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
"""

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  sub = _[0]
  arg = _[1]
  unless sub
    console.log SCHEMA_HELP
    return 0
  world = await loadWorld(cwd)
  schema = world.schema
  switch sub
    when 'graph'
      counts = {}
      counts[e.cls] = (counts[e.cls] or 0) + 1 for e in world.entities
      g = schemaGraph(schema, counts)
      edges = (g.graph or '').split('\n').filter((l) -> l).sort()
      top = (g.top or []).sort()
      types = (g.types or []).sort()
      lines = ['graph: |-']
      lines.push("  #{e}") for e in edges
      lines.push "top: [#{top.join(', ')}]"
      lines.push 'types:'
      lines.push("- #{t}") for t in types
      console.log lines.join('\n')
    when 'uniq'
      out =
        components: Object.keys(schema.components or {}).sort()
        classes: Object.keys(schema.classes or {}).sort()
        relations: Object.keys(schema.relations or {}).sort()
      console.log yaml.dump(out, { sortKeys: false, flowLevel: 1, lineWidth: -1 })
    when 'components'
      names = if arg then [arg] else Object.keys(schema.components or {})
      console.log await renderComponents(cwd, schema, names)
    when 'classes'
      names = if arg then [arg] else Object.keys(schema.classes or {})
      console.log renderClasses(schema, names)
    when 'methods'
      throw new Error("unknown class '#{arg}'") if arg and not schema.classes?[arg]
      names = if arg then [arg] else Object.keys(schema.classes or {})
      console.log await renderMethods(cwd, schema, names)
    else
      throw new Error("unknown schema subcommand '#{sub}' (graph|uniq|components|classes|methods)")
  0
