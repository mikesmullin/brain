# think.coffee — search + LLM synthesis, as a single agl-ai microagent.
# One decision (synthesize a grounded answer), one output tool, typed result.
# All retrieval/formatting is deterministic (pglite-only); the model only synthesizes.
import Agent from 'agl-ai'
import { hybridSearch } from './search.coffee'
import { renderEntityText } from './index.coffee'
import { serializeEntity } from './storage.coffee'

SYSTEM = """
You synthesize an answer about a knowledge graph using ONLY the retrieved context.
Rules:
- Cite supporting entities inline using their slug in square brackets, e.g. [Team/team-cloud].
- Do NOT invent entities, relations, or facts not present in the context.
- If the context is insufficient, say so and list what's missing in `gaps`.
- Keep `answer` concise and directly responsive to the question.
"""

# `<|think|>` prefix enables reasoning on lm-studio models (off by default —
# performance-first). Only injected for that provider; other providers ignore
# or would mis-handle the token, so it's gated on the model spec's prefix.
export thinkPrefix = (model, thinking) ->
  if thinking and String(model or '').startsWith('lm-studio:') then '<|think|>\n' else ''

# Selection block for system prompts (think / ontology).
#
# Only when the viz "include selection" toggle is ON does the client pass
# `selection` (possibly an empty array). Toggle OFF → `selection` is undefined
# and we emit nothing (no deictic blurb, no count, no entities).
#
#   undefined / null  → toggle off: omit entirely
#   [] / [slugs...]   → toggle on: deictic guidance + count + optional YAML bodies
export selectionContext = (core, selection) ->
  return '' unless selection?

  slugs = if Array.isArray(selection) then selection.filter(Boolean) else []
  # Resolve entities first so the count matches what we actually emit
  # (missing slugs are skipped rather than hallucinated as empty shells).
  entities = []
  for slug in slugs
    e = await core.idx.fullEntity(slug)
    entities.push(e) if e
  n = entities.length
  countLine = "NOTICE: In the app, I have selected #{n} #{if n is 1 then 'entity' else 'entities'}."

  return "\n\n#{countLine}\n" unless n

  blocks = for e in entities
    yaml = serializeEntity(e).trimEnd()
    """
<entity slug="#{e.slug}">
#{yaml}
</entity>
"""
  """

#{countLine}

<selected-entities>
#{blocks.join('\n')}
</selected-entities>
"""

export think = (core, question, opts = {}) ->
  limit = opts.limit or 8
  model = opts.model or core.cfg.think.model
  results = await hybridSearch(core, question, { limit })
  throw new Error('cancelled by user') if opts.isCancelled?()

  blocks = for r in results
    e = await core.idx.fullEntity(r.slug)
    continue unless e
    "<entity slug=\"#{r.slug}\">\n#{renderEntityText(e)}\n</entity>"
  context = blocks.filter((b) -> b).join('\n')

  selCtx = await selectionContext(core, opts.selection)
  throw new Error('cancelled by user') if opts.isCancelled?()
  agent = await Agent.factory
    model: model
    # stream: cancellation only works promptly over SSE — LM Studio ignores a
    # dead client on non-streaming requests and decodes to completion (zombie
    # slots eating the GPU); with streaming, abort closes the socket and the
    # slot is freed within a token. Response shape is identical either way.
    stream: true
    # 0 retries: a cancelled/stuck inference must not re-fire (default AGL is 5).
    # Honored centrally by withProviderRetry for every provider.
    retries: 0
    system_prompt: thinkPrefix(model, opts.thinking) + SYSTEM + selCtx
    output_tool:
      name: 'answer'
      description: 'Report the synthesized, grounded answer with citations and gaps.'
      parameters:
        answer: { type: 'string' }
        citations: { type: 'array', items: { type: 'string' } }
        gaps: { type: 'array', items: { type: 'string' } }
        reasoning: { type: 'string' }
      required: ['answer']
  opts.onAgent?(agent)   # cancellation hook: lets the server abort this inference
  # Re-check after construction: cancel may have landed during factory/init
  if opts.isCancelled?()
    agent.abort('cancelled by user')
    throw new Error('cancelled by user')
  # Plain user text (no <question> wrapper) + retrieved context block.
  r = await agent.run prompt: "#{question}\n\n<retrieved-context>\n#{context}\n</retrieved-context>"
  throw new Error('cancelled by user') if opts.isCancelled?()
  {
    answer: r.answer
    citations: r.citations or []
    gaps: r.gaps or []
    reasoning: r.reasoning or ''
    retrieved: (x.slug for x in results)
  }
