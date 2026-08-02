# brain — knowledge-graph agent for brain viz chat
# Template installed into <project>/.angela/agents/brain.coffee on first viz run.

module.exports = (ctx) ->
  name: 'brain'
  description: 'Explore and edit the local knowledge graph via brain MCP tools'
  model: process.env.FAV_LOCAL_LLM or 'lm-studio:google/gemma-4-26b-a4b-qat'
  models: Array.from(new Set([
    process.env.FAV_LOCAL_LLM or null
    'lm-studio:google/gemma-4-26b-a4b-qat'
    'copilot:gpt-5.6-luna'
  ].filter(Boolean)))
  mcp: [
    {
      name: 'brain'
      command: process.execPath
      args: ['__BRAIN_BIN__', 'mcp']
      cwd: '__BRAIN_CWD__'
    }
  ]
  system: '''
    You are a knowledge-graph assistant embedded in the brain viz explorer.
    Use brain MCP tools (search, graph, graphql, get_entity, put_entity, …)
    to answer questions about entities and their relationships.
    Prefer precise tool use over guessing. Cite entities as Class/id slugs.
    When the user has graph nodes selected, treat them as deictic context
    ("this", "these", "the selected node"). Be concise; use Markdown.
  '''
  allowlist: '''
    brain__search
    brain__search:.*
    brain__think
    brain__think:.*
    brain__ontology
    brain__ontology:.*
    brain__graph
    brain__graph:.*
    brain__graphql
    brain__graphql:.*
    brain__get_entity
    brain__get_entity:.*
    brain__put_entity
    brain__put_entity:.*
    brain__delete_entity
    brain__delete_entity:.*
    brain__schema_methods
    brain__schema_methods:.*
    brain__method_invoke
    brain__method_invoke:.*
    brain__schema_orphans
    brain__schema_orphans:.*
  '''
  policyMode: 'ask'
  starters: [
    'What tools do you have? List them briefly.'
    'Summarize what this knowledge graph is about.'
    { label: 'Explore selection', prompt: 'Inspect the currently selected entities and summarize who/what they are and how they connect.' }
  ]
