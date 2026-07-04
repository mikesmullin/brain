# embed.coffee — embeddings via agl-ai's pluggable providers.
# Default provider/model is configured in .brain/config.yaml (embed.model),
# e.g. "copilot:text-embedding-3-small" (1536d) or "lm-studio:...nomic..." (768d).
import Agent from 'agl-ai'

export embedTexts = (model, texts) ->
  return [] unless texts.length
  res = await Agent.embed({ model, input: texts })
  res.data.map (d) -> d.embedding

export embedOne = (model, text) ->
  [v] = await embedTexts(model, [text])
  v

# Probe the model to discover its output dimension (pgvector columns are fixed-dim).
export detectDim = (model) ->
  v = await embedOne(model, 'dimension probe')
  v.length

export providerModel = (spec) ->
  idx = spec.indexOf(':')
  { provider: spec.slice(0, idx), model: spec.slice(idx + 1) }
