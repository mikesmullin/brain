# ontology.coffee — LLM-driven typed relationship traversal.
# A small tool-using agent (not a single-decision microagent): it may search and
# traverse typed edges over several turns, then emit one typed answer. All graph
# access is deterministic (indexed pglite queries — O(degree) per hop, never a
# whole-world scan); the model only decides where to look.
import Agent from 'agl-ai'
import { hybridSearch } from './search.coffee'
import { thinkPrefix, selectionContext } from './think.coffee'
import { schemaGraph } from './schema.coffee'

export ontologyQuery = (core, question, opts = {}) ->
  sg = schemaGraph(core.schema)
  model = opts.model or core.cfg.think.model

  system = """
    You answer relational questions by traversing a typed knowledge graph.

    METHOD (follow this):
    1. `search` for the seed entity/entities named in the question (e.g. a Product, Team, Franchise).
       A search usually returns SEVERAL candidates (e.g. variant editions of one product).
    2. Call `paths(from_seed_slug, target_class, max_hops)` to find how the seed connects to the class
       the question asks about. This bidirectional BFS answers most multi-hop questions in ONE call.
       Do NOT keyword-`search` for the answer class — traverse to it with `paths`.
       IMPORTANT: a base/deprecated entity may have NO relations, so `paths` from it returns empty.
       If `paths` from one seed is empty, immediately try `paths` from the OTHER search candidates
       (e.g. the variant products) before doing any more searching. Do not fall back to keyword search.
    3. Read the `via` chain of each result to understand the relationship. Edges are `REL>` (outgoing)
       or `<REL` (incoming). The chain tells you the role:
         - `... <LEADER_OF`            -> this Person LEADS that team (the "lead").
         - `... <LEADER_OF <REPORTS_TO`-> this Person REPORTS TO the lead (a subordinate / NON-lead member).
    4. Use `get`/`neighbors` only to confirm a specific edge or read an entity's fields.
    5. Call `answer` with the resolved slugs and a short explanation.

    Example: "which non-lead SRE supports <product>?" -> search the product, then
    paths(Product/<id>, 'Person', 5); the lead is the `<LEADER_OF` hit, the non-lead SREs are the
    `<LEADER_OF <REPORTS_TO` hits. Only assert facts confirmed via tools; cite entities by slug.

    Schema graph:
    #{sg.graph}
    Top-level (seed) classes: #{(sg.top or []).join(', ') or '(none marked)'}
  """

  agent = await Agent.factory
    model: model
    system_prompt: thinkPrefix(model, opts.thinking) + system + selectionContext(opts.selection)
    parallel_tools: true
    reasoning_effort: 'medium'
    output_tool:
      name: 'answer'
      description: 'Report the final answer with the resolved entity slugs.'
      parameters:
        answer: { type: 'string' }
        entities: { type: 'array', items: { type: 'string' } }
        reasoning: { type: 'string' }
      required: ['answer']

  # Bound the traversal: after `budget` tool calls, tools return a sentinel that
  # forces the model to conclude via `answer` (prevents meandering / runaway loops).
  budget = opts.maxCalls or 15
  calls = 0
  guard = (fn) -> (ctx, args) ->
    calls++
    return JSON.stringify({ note: "tool budget (#{budget}) exhausted — call `answer` NOW with your best conclusion from the evidence gathered so far" }) if calls > budget
    await fn(ctx, args)

  agent.Tool 'search', 'Hybrid search for seed entities by meaning/keywords. Returns slugs.',
    { query: { type: 'string' } }, ['query'],
    guard (ctx, { query }) ->
      res = await hybridSearch(core, query, { limit: 8 })
      JSON.stringify(res.map((r) -> r.slug))

  agent.Tool 'paths', 'Bidirectional BFS: find all paths from `from` (a slug) to entities of class `to_class` within max_hops (default 4). Returns {results: [{end, hops, path, via}], capped}.',
    { from: { type: 'string' }, to_class: { type: 'string' }, max_hops: { type: 'number' } }, ['from', 'to_class'],
    guard (ctx, { from, to_class, max_hops }) ->
      JSON.stringify(await core.findPaths(from, to_class, max_hops or 4))

  agent.Tool 'neighbors', 'List typed relations (in + out) for an entity slug. Optional rel filter.',
    { slug: { type: 'string' }, rel: { type: 'string' } }, ['slug'],
    guard (ctx, { slug, rel }) -> JSON.stringify(await core.idx.neighbors(slug, rel))

  agent.Tool 'get', 'Get an entity\'s components and relations by slug.',
    { slug: { type: 'string' } }, ['slug'],
    guard (ctx, { slug }) ->
      e = await core.idx.fullEntity((await core.resolveSlug(slug)) or slug)
      if e then JSON.stringify({ slug: e.slug, components: e.components, relations: e.relations }) else JSON.stringify({ error: 'not found' })

  r = await agent.run prompt: "<question>#{question}</question>"
  { answer: r.answer, entities: r.entities or [], reasoning: r.reasoning or '' }
