# ontology.coffee — LLM-driven typed relationship traversal.
# A small tool-using agent (not a single-decision microagent): it may search and
# traverse typed edges over several turns, then emit one typed answer. All graph
# access is deterministic (indexed pglite queries — O(degree) per hop, never a
# whole-world scan); the model only decides where to look.
import Agent from 'agl-ai'
import { hybridSearch } from './search.coffee'
import { thinkPrefix, buildPromptEntityContext } from './think.coffee'

# Longest practical wall-clock stamp for the system prompt: full weekday, month
# name, day, year, 12h time with seconds + ms, long timezone name, plus ISO-8601
# for unambiguous machine parse (local timezone via Intl).
formatSystemTime = (d = new Date()) ->
  human = new Intl.DateTimeFormat('en-US',
    weekday: 'long'
    era: 'long'
    year: 'numeric'
    month: 'long'
    day: 'numeric'
    hour: 'numeric'
    minute: '2-digit'
    second: '2-digit'
    fractionalSecondDigits: 3
    hour12: true
    timeZoneName: 'long'
  ).format(d)
  "#{human} (#{d.toISOString()})"

export ontologyQuery = (core, question, opts = {}) ->
  model = opts.model or core.cfg.think.model

  # Selected entities (toggle-on) + wiki-link references in the question
  # are preloaded into the system prompt so tools aren't needed for those.
  entCtx = await buildPromptEntityContext core,
    selection: opts.selection
    question: question
  system = """
#{thinkPrefix(model, opts.thinking)}
You're a research assistant.
You help me (a researcher) to (answer relational questions) by (traversing a typed knowledge graph).
I (the human operator) am interacting with you via (the ontology browser app).

Use your tools to navigate the ontology data to learn more about entities and their relationships.
When the system prompt already includes <selected-entities> or <referenced-entities>,
use those bodies first — do not re-fetch them unless you need fresher or related data.

## How to reply

IMPORTANT: Always refer to an entity by its slug (this will cause the app UI to auto-expand its name), using wikilinks in one of the following formats
  - `[[Class/id]]`
  - `[[Class/id|display text]]`
  - `[[REL:Class/id]]`
  - `[[REL:Class/id|display text]]`

If I refer to context you don't have, then ask me clarifying questions.

Be honest. If you don't know the answer, then say so. Use markdown in your replies.

You must reply using the `final_answer` tool; I will not accept other replies.

## Metadata

<data_origin>
The Panama Papers refer to a 2016 leak of 11.5 million documents (2.6 TB) from Panamanian law firm Mossack Fonseca, 
revealing details on over 214,000 offshore entities. The public dataset is the structured extract released by ICIJ, 
now part of the Offshore Leaks Database (over 810,000 entities across multiple investigations). 
It includes companies, officers, intermediaries, and addresses in graph form, available via searchable website, 
CSV downloads, or Neo4j dumps under an open license requiring ICIJ citation. 
The data is historical and does not imply wrongdoing.
</data_origin>

<system_time>#{formatSystemTime()}</system_time>

#{entCtx}
"""

  agent = await Agent.factory
    model: model
    stream: true   # prompt cancellation: see think.coffee — SSE abort frees the LM Studio slot
    retries: 0     # never auto-retry: cancel must stick (see think.coffee)
    system_prompt: system
    parallel_tools: true
    reasoning_effort: 'medium'
    output_tool:
      name: 'final_answer'
      description: 'Report the final answer with the resolved entity slugs.'
      parameters:
        answer: { type: 'string' }
        entities: { type: 'array', items: { type: 'string' } }
        reasoning: { type: 'string' }
      required: ['answer']
  opts.onAgent?(agent)   # cancellation hook: lets the server abort this inference
  if opts.isCancelled?()
    agent.abort('cancelled by user')
    throw new Error('cancelled by user')

  # Bound the traversal: after `budget` tool calls, tools return a sentinel that
  # forces the model to conclude via `answer` (prevents meandering / runaway loops).
  budget = opts.maxCalls or 15
  calls = 0
  guard = (fn) -> (ctx, args) ->
    if opts.isCancelled?()
      return JSON.stringify({ error: 'cancelled by user', note: 'call `answer` with whatever you have — the user stopped this query' })
    calls++
    return JSON.stringify({ note: "tool budget (#{budget}) exhausted — call `answer` NOW with your best conclusion from the evidence gathered so far" }) if calls > budget
    await fn(ctx, args)

  agent.Tool 'db__search', 'Hybrid search for seed entities by meaning/keywords. Returns slugs.',
    { query: { type: 'string' } }, ['query'],
    guard (ctx, { query }) ->
      res = await hybridSearch(core, query, { limit: 8 })
      JSON.stringify(res.map((r) -> r.slug))

  agent.Tool 'db__pathfind', 'Bidirectional BFS: find all paths from `from` (a slug) to entities of class `to_class` within max_hops (default 4). Returns {results: [{end, hops, path, via}], capped}.',
    { from: { type: 'string' }, to_class: { type: 'string' }, max_hops: { type: 'number' } }, ['from', 'to_class'],
    guard (ctx, { from, to_class, max_hops }) ->
      JSON.stringify(await core.findPaths(from, to_class, max_hops or 4))

  agent.Tool 'db__neighbors', 'List typed relations (in + out) for an entity slug. Optional rel filter.',
    { slug: { type: 'string' }, rel: { type: 'string' } }, ['slug'],
    guard (ctx, { slug, rel }) -> JSON.stringify(await core.idx.neighbors(slug, rel))

  agent.Tool 'db__get_entity', 'Get an entity\'s components and relations by slug.',
    { slug: { type: 'string' } }, ['slug'],
    guard (ctx, { slug }) ->
      e = await core.idx.fullEntity((await core.resolveSlug(slug)) or slug)
      if e then JSON.stringify({ slug: e.slug, components: e.components, relations: e.relations }) else JSON.stringify({ error: 'not found' })

  # Plain user text — no <question> wrapper (keeps the prompt natural for local models).
  r = await agent.run prompt: question
  throw new Error('cancelled by user') if opts.isCancelled?()
  { answer: r.answer, entities: r.entities or [], reasoning: r.reasoning or '' }
