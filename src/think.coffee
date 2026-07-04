# think.coffee — search + LLM synthesis, as a single agl-ai microagent.
# One decision (synthesize a grounded answer), one output tool, typed result.
# All retrieval/formatting is deterministic; the model only synthesizes.
import Agent from 'agl-ai'
import { hybridSearch } from './search.coffee'
import { loadWorld } from './world.coffee'
import { loadConfig } from './config.coffee'
import { renderEntityText } from './index.coffee'

SYSTEM = """
You synthesize an answer about a knowledge graph using ONLY the retrieved context.
Rules:
- Cite supporting entities inline using their slug in square brackets, e.g. [Team/team-cloud].
- Do NOT invent entities, relations, or facts not present in the context.
- If the context is insufficient, say so and list what's missing in `gaps`.
- Keep `answer` concise and directly responsive to the question.
"""

export think = (cwd, question, opts = {}) ->
  cfg = await loadConfig(cwd)
  limit = opts.limit or 8
  results = await hybridSearch(cwd, question, { limit })
  world = await loadWorld(cwd)

  blocks = for r in results
    e = world.bySlug[r.slug]
    continue unless e
    "<entity slug=\"#{r.slug}\">\n#{renderEntityText(e)}\n</entity>"
  context = blocks.filter((b) -> b).join('\n')

  agent = await Agent.factory
    model: cfg.think.model
    system_prompt: SYSTEM
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
