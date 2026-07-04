# def.coffee — T-box (schema) mutations. `def` declares STRUCTURE, never values.
#   def component <Name> --fields '<yamlflow>'
#   def class <Name> [--components alias:Component ...] [--top]
#   def relation <REL> <domain> <cardinality> <range> [--qualifiers <name> '<yamlflow>' ...]
import { paths } from '../config.coffee'
import { loadSchema, writeSchema, schemaGraph, FIELD_TYPES, CARDINALITIES } from '../schema.coffee'
import { storageDirs } from '../config.coffee'
import { parseArgs, asArray } from '../args.coffee'
import yaml from 'js-yaml'

parseFlow = (s) ->
  return {} unless s
  v = yaml.load(s)
  throw new Error("expected a YAML mapping, got: #{s}") unless v and typeof v is 'object'
  v

validateFieldDef = (name, fdef) ->
  throw new Error("field '#{name}': missing type") unless fdef.type
  throw new Error("field '#{name}': invalid type '#{fdef.type}' (one of: #{FIELD_TYPES.join(', ')})") unless FIELD_TYPES.includes(fdef.type)

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['top'] })
  kind = _[0]
  dirs = await storageDirs(cwd)
  schema = await loadSchema(dirs)
  primary = paths(cwd).storage

  switch kind
    when 'component'
      name = _[1]
      throw new Error("usage: def component <Name> --fields '<yamlflow>'") unless name
      fields = parseFlow(flags.fields)
      validateFieldDef(fn, fd) for own fn, fd of fields
      schema.components[name] = { fields }
      await writeSchema(primary, schema)
      console.log "defined component #{name} (#{Object.keys(fields).length} fields)"

    when 'class'
      name = _[1]
      throw new Error("usage: def class <Name> [--components alias:Component ...] [--top]") unless name
      components = {}
      for spec in asArray(flags.components)
        [alias, comp] = String(spec).split(':')
        throw new Error("--components expects alias:Component, got '#{spec}'") unless alias and comp
        throw new Error("component '#{comp}' is not defined (run: def component #{comp} ...)") unless schema.components[comp]
        components[alias] = comp
      def = { components }
      def.top = true if flags.top
      schema.classes[name] = def
      await writeSchema(primary, schema)
      console.log "defined class #{name}#{if def.top then ' (top-level)' else ''} with components: #{Object.keys(components).join(', ') or '(none)'}"

    when 'relation'
      [name, domain, cardinality, range] = [_[1], _[2], _[3], _[4]]
      throw new Error("usage: def relation <REL> <domain> <cardinality> <range> [--qualifiers '<yamlflow>']") unless name and domain and cardinality and range
      throw new Error("invalid cardinality '#{cardinality}' (one of: #{CARDINALITIES.join(', ')})") unless CARDINALITIES.includes(cardinality)
      qualifiers = parseFlow(flags.qualifiers)
      validateFieldDef(qn, qd) for own qn, qd of qualifiers
      rdef = { domain, range, cardinality }
      rdef.qualifiers = qualifiers if Object.keys(qualifiers).length
      schema.relations[name] = rdef
      await writeSchema(primary, schema)
      console.log "defined relation #{name}: #{domain} --|#{name}|--> #{range} (#{cardinality})"

    when 'graph', undefined
      console.log yaml.dump(schemaGraph(schema), { lineWidth: 120, sortKeys: false })

    else
      throw new Error("unknown def target '#{kind}' (component|class|relation|graph)")
  0
