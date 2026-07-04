# extract.coffee — LLM extraction microagent: read a document, emit entities that
# CONFORM to the existing schema. One decision, one typed output tool. The model
# only proposes structured entities; deterministic code validates + writes them.
import Agent from 'agl-ai'
import yaml from 'js-yaml'
import { loadConfig } from './config.coffee'
import { loadCalcId } from './refine.coffee'

# Compact, readable schema view for the prompt (optionally filtered to some classes).
export describeSchema = (schema, classes = null) ->
  names = if classes?.length then classes else Object.keys(schema.classes or {})
  out = { classes: {}, relations: {} }
  for cn in names
    cdef = schema.classes?[cn]
    continue unless cdef
    comps = {}
    for own alias, compName of (cdef.components or {})
      comps[alias] = schema.components?[compName]?.fields or {}
    out.classes[cn] = { components: comps, top: !!cdef.top }
  for own rel, rdef of (schema.relations or {})
    if (not classes?.length) or (rdef.domain in names) or (rdef.range in names)
      out.relations[rel] = rdef
  yaml.dump(out, { lineWidth: 100, sortKeys: false })

SYSTEM = """
You extract structured knowledge-graph instances from a document.
Hard rules:
- Use ONLY classes, components, fields, and relations defined in the provided schema.
- Never invent new classes/components/fields/relations. Never change the schema.
- Each instance needs `_class` (a defined class) and `_id` (a stable kebab-case slug, e.g. "team-cloud").
- Put field values under their component alias (e.g. components.naming.name).
- Reference other instances by slug "Class/id" (in ref fields and relations).
- Relation values are arrays whose items are either a "Class/id" string, or an object
  { "_to": "Class/id", "<qualifier>": <value> } when the relation has qualifiers.
- Only fill fields the SOURCE TEXT actually supports. Do NOT fabricate values for required
  fields you don't know (titles, dates, ids, etc.) — leave them ABSENT. Never invent placeholders
  like "Unknown", "N/A", or "1970-01-01". A downstream per-class refiner will resolve missing values.
- Declared types: string/bool/int/date/enum/ref/json.
- One document may yield MANY instances across MANY classes. Only extract what the text supports.
- Prefer fewer, well-grounded instances over speculative ones.
- DEDUP: before emitting an instance, call `existing` (class + email or name/id) to check if it already
  exists. If it exists with equal-or-better detail, DO NOT emit it. If it exists but you have genuinely
  NEW fields, emit only the new fields (they are additively merged, never overwriting). If not found,
  emit the full instance.
"""

export extractEntities = (cwd, text, opts = {}) ->
  cfg = await loadConfig(cwd)
  schema = opts.schema
  world = opts.world
  classes = opts.classes or (if opts.class then [opts.class] else null)
  focus = if classes?.length then "Focus ONLY on these class(es): #{classes.join(', ')} (you may emit multiple instances)." else "Choose the most appropriate class(es) for each fact."
  schemaDoc = describeSchema(schema, classes)

  agent = await Agent.factory
    model: cfg.think.model
    system_prompt: SYSTEM
    reasoning_effort: 'low'   # structured fill-in — depth not needed; keep it fast
    output_tool:
      name: 'entities'
      description: 'Report the extracted instances that conform to the schema.'
      parameters:
        entities:
          type: 'array'
          items:
            type: 'object'
            properties:
              _class: { type: 'string' }
              _id: { type: 'string' }
              components: { type: 'object' }
              relations: { type: 'object' }
            required: ['_class', '_id']
      required: ['entities']

  # DUPE_CHECK tool — resolve a candidate to its canonical id (via the class's CALCULATED_FIELD)
  # and return the existing record if present, so the model can skip/modify instead of duplicating.
  if world
    agent.Tool 'existing', 'Check if an entity already exists BEFORE creating it. Give class + any of {id, email, givenName, surname}. Returns the existing record (components/relations) or found:false.',
      { class: { type: 'string' }, id: { type: 'string' }, email: { type: 'string' }, givenName: { type: 'string' }, surname: { type: 'string' } }, ['class'],
      (ctx, args) ->
        cls = args.class
        stub = { cls, components: { contact: {}, identity: {} } }
        stub.components.contact.email = args.email if args.email
        stub.components.identity.givenName = args.givenName if args.givenName
        stub.components.identity.surname = args.surname if args.surname
        calc = await loadCalcId(cwd, cls)
        id = args.id or (calc and calc(stub)) or null
        return JSON.stringify({ found: false, reason: 'no id could be derived' }) unless id
        slug = "#{cls}/#{id}"
        e = world.bySlug[slug]
        if e then JSON.stringify({ found: true, slug, components: e.components, relations: e.relations }) else JSON.stringify({ found: false, would_be_slug: slug })

  prompt = """
    #{focus}

    <target-schema>
    #{schemaDoc}
    </target-schema>

    <source-doc>
    #{text}
    </source-doc>
  """
  r = await agent.run prompt: prompt
  r.entities or []

# Dry-run schema suggester: recommends NEW T-box definitions as copy/paste CLI lines.
SUGGEST_SYSTEM = """
You review a document against an existing knowledge-graph schema and recommend
NEW schema definitions that would be needed to structure the document's facts.
Output each recommendation as a ready-to-paste `brain def ...` CLI line:
  brain def component <Name> --fields '{field: {type: string, required: true}, ...}'
  brain def class <Name> --components alias:Component [--top]
  brain def relation <REL> <Domain> <cardinality> <Range> [--qualifiers '{name: {type: string}}']
Rules:
- Only suggest definitions that do NOT already exist in the provided schema.
- Field types: string | bool | int | date | enum | ref | json. Cardinality: oto | otn | nto | mtm.
- This is a DRY RUN — you only propose text; nothing is applied.
"""

export suggestSchema = (cwd, text, opts = {}) ->
  cfg = await loadConfig(cwd)
  agent = await Agent.factory
    model: cfg.think.model
    system_prompt: SUGGEST_SYSTEM
    reasoning_effort: 'low'
    output_tool:
      name: 'suggestions'
      description: 'Report suggested new schema definitions as brain def CLI lines.'
      parameters:
        suggestions: { type: 'array', items: { type: 'string' } }
        rationale: { type: 'string' }
      required: ['suggestions']
  r = await agent.run prompt: """
    <existing-schema>
    #{describeSchema(opts.schema)}
    </existing-schema>

    <source-doc>
    #{text}
    </source-doc>
  """
  { suggestions: r.suggestions or [], rationale: r.rationale or '' }
