// Agent-facing coaching on MCP *query* tool results.
// CLI dumps stay clean; only the stdio adapter attaches `advice`.
// Relevance preference for natural-language questions: ontology > think > search.

const isBlank = (s) => !String(s || '').trim()

const unknownAnswer = (s) =>
  /not (enough|found)|don't know|do not know|no (relevant )?(entit|path|information|matches)|insufficient|nothing in (the )?(graph|index|context)|gaps?:/i.test(
    String(s || ''),
  )

export function searchEmpty(results) {
  return results == null || (Array.isArray(results) && results.length === 0)
}

export function searchHasVectors(results) {
  return (
    Array.isArray(results) &&
    results.some((h) => h?.cosine != null && Number(h.cosine) > 0)
  )
}

export function thinkEmpty(r) {
  if (!r || typeof r !== 'object') return true
  const retrieved = r.retrieved || []
  const citations = r.citations || []
  return retrieved.length === 0 && citations.length === 0
}

export function ontologyEmpty(r) {
  if (!r || typeof r !== 'object') return true
  const ents = r.entities || []
  const ans = String(r.answer || '').trim()
  if (ents.length === 0 && isBlank(ans)) return true
  if (ents.length === 0 && unknownAnswer(ans)) return true
  return false
}

export function graphEmpty(r) {
  return !r || !Array.isArray(r.matches) || r.matches.length === 0
}

export function graphqlEmpty(r) {
  if (!r || typeof r !== 'object') return true
  return Boolean(r.error)
}

function searchAdvice(empty, results) {
  const lexical = !searchHasVectors(results)
  if (empty) {
    return 'No matches. search is retrieval (keyword FTS; vectors only if this index has embeddings) — query tokens must appear in stored text or be close in embedding space. It does not synthesize an answer or walk typed edges. For a natural-language question try think next (retrieval + grounded answer/citations/gaps), then ontology (LLM multi-hop graph walk). Relevance order: ontology > think > search. If you already know a Class/id slug, use get_entity.'
  }
  if (lexical) {
    return 'Matches found, but this index looks keyword-only (no vector scores) — hits can be lexically true and still irrelevant. If they do not answer the question, try think (synthesis + gaps), then ontology. Do not treat the first hit as a final answer. Relevance order: ontology > think > search.'
  }
  return 'Matches found (hybrid retrieval). Rank is not a guarantee of meaning. If these hits do not answer the question, try think, then ontology. Relevance order: ontology > think > search.'
}

function thinkAdvice(empty, r) {
  const gaps = r?.gaps || []
  if (empty) {
    return 'think had nothing to cite — it runs search then synthesizes, so an empty retrieval yields no grounded answer. Do not retry search with the same wording (think already searched). Try ontology next for a multi-hop graph walk, or rephrase and call think again. If you know a slug, use get_entity.'
  }
  if (gaps.length) {
    return `think returned an answer with gaps: ${gaps.join('; ')}. If that does not fully answer the question, try ontology (richer NL graph walk). Do not fall back to search — think already used it.`
  }
  return 'think synthesized from retrieved hits. If the answer feels off or incomplete, try ontology next (not search).'
}

function ontologyAdvice(empty) {
  if (empty) {
    return 'ontology did not land a graph walk (few/no matching edges, or the question is not relational). Try think or search with different wording, or get_entity if you know the Class/id slug. ontology is the richest NL query; if it is empty, retrieval may still work on node text.'
  }
  return 'ontology is the richest natural-language graph walk. If this still does not answer the question, rephrase the question or use get_entity on a known slug — do not expect search to be more relevant.'
}

function graphAdvice(empty, r) {
  if (empty) {
    return 'No graph matches. graph is a deterministic Mermaid-ish pattern (e.g. "Person/msmullin -->|REL| Person"), not English. For a natural-language question use ontology, then think, then search. Relevance order: ontology > think > search.'
  }
  if (r?.capped) {
    return 'Matches found, but the traversal hit a node cap (capped: true) — this may be incomplete. Narrow the pattern or raise --max-nodes. For English questions prefer ontology > think > search.'
  }
  return null
}

function graphqlAdvice(empty) {
  if (empty) {
    return 'graphql did not resolve (missing slug or bad selection). This is a GraphQL-ish projection from a known Class/id, not a natural-language search. For English questions use ontology, then think, then search. If you have a slug, fix the query or use get_entity.'
  }
  return null
}

/** Wrap a query RPC payload with `advice` for MCP only. search is a bare array from core. */
export function attachQueryAdvice(name, payload) {
  switch (name) {
    case 'search': {
      const results = Array.isArray(payload) ? payload : payload?.results || []
      return { results, advice: searchAdvice(searchEmpty(results), results) }
    }
    case 'think': {
      const body =
        payload && typeof payload === 'object' ? { ...payload } : { answer: payload }
      body.advice = thinkAdvice(thinkEmpty(payload), payload)
      return body
    }
    case 'ontology': {
      const body =
        payload && typeof payload === 'object' ? { ...payload } : { answer: payload }
      body.advice = ontologyAdvice(ontologyEmpty(payload))
      return body
    }
    case 'graph': {
      const body =
        payload && typeof payload === 'object' ? { ...payload } : { matches: [] }
      const a = graphAdvice(graphEmpty(payload), payload)
      if (a) body.advice = a
      return body
    }
    case 'graphql': {
      const body =
        payload && typeof payload === 'object' ? { ...payload } : { error: 'empty' }
      const a = graphqlAdvice(graphqlEmpty(payload))
      if (a) body.advice = a
      return body
    }
    default:
      return payload
  }
}
