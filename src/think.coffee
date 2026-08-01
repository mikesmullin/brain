# think.coffee — search + LLM synthesis, as a single agl-ai microagent.
# One decision (synthesize a grounded answer), one output tool, typed result.
# All retrieval/formatting is deterministic (pglite-only); the model only synthesizes.
import Agent from 'agl-ai'
import { hybridSearch } from './search.coffee'
import { renderEntityText } from './index.coffee'

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

# Selection context (viz "include selection" toggle): tells the model which
# entities the user currently has selected, so questions like "how are these
# related?" resolve against the selection.
export selectionContext = (selection) ->
  if selection?.length then "\nthe user has selected: #{selection.join(', ')}" else ''

export think = (core, question, opts = {}) ->
  limit = opts.limit or 8
  model = opts.model or core.cfg.think.model
  results = await hybridSearch(core, question, { limit })

  blocks = for r in results
    e = await core.idx.fullEntity(r.slug)
    continue unless e
    "<entity slug=\"#{r.slug}\">\n#{renderEntityText(e)}\n</entity>"
  context = blocks.filter((b) -> b).join('\n')

  agent = await Agent.factory
    model: model
    system_prompt: thinkPrefix(model, opts.thinking) + SYSTEM + selectionContext(opts.selection)
    output_tool:
      name: 'answer'
      description: 'Report the synthesized, grounded answer with citations and gaps.'
      parameters:
        answer: { type: 'string' }
        citations: { type: 'array', items: { type: 'string' } }
        gaps: { type: 'array', items: { type: 'string' } }
        reasoning: { type: 'string' }
      required: ['answer']
  r = await agent.run prompt: "<question>#{question}</question>\n<retrieved-context>\n#{context}\n</retrieved-context>"
  {
    answer: r.answer
    citations: r.citations or []
    gaps: r.gaps or []
    reasoning: r.reasoning or ''
    retrieved: (x.slug for x in results)
  }
