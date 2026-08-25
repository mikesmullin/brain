import { describe, expect, test } from 'bun:test'
import * as advice from './mcp-advice.mjs'

const {
  attachQueryAdvice,
  graphEmpty,
  graphqlEmpty,
  ontologyEmpty,
  searchEmpty,
  searchHasVectors,
  thinkEmpty,
} = advice

describe('mcp query advice', () => {
  test('search empty → try think then ontology, not graph', () => {
    const out = attachQueryAdvice('search', [])
    expect(searchEmpty(out.results)).toBe(true)
    expect(out.advice).toMatch(/No matches/)
    expect(out.advice).toMatch(/think next/)
    expect(out.advice).toMatch(/then ontology/)
    expect(out.advice).toMatch(/ontology > think > search/)
  })

  test('search keyword-only hits warn they may be irrelevant', () => {
    const hits = [{ slug: 'Note/x', score: 0.01, cosine: null }]
    expect(searchHasVectors(hits)).toBe(false)
    const out = attachQueryAdvice('search', hits)
    expect(out.results).toHaveLength(1)
    expect(out.advice).toMatch(/keyword-only/)
    expect(out.advice).toMatch(/try think/)
    expect(out.advice).toMatch(/then ontology/)
  })

  test('search hybrid hits still allow upgrade to think/ontology', () => {
    const hits = [{ slug: 'Person/msmullin', score: 0.2, cosine: 0.71 }]
    expect(searchHasVectors(hits)).toBe(true)
    const out = attachQueryAdvice('search', hits)
    expect(out.advice).toMatch(/hybrid retrieval/)
    expect(out.advice).not.toMatch(/try search/)
  })

  test('think empty does not recommend search', () => {
    const out = attachQueryAdvice('think', { answer: '', citations: [], retrieved: [], gaps: [] })
    expect(thinkEmpty(out)).toBe(true)
    expect(out.advice).toMatch(/Do not retry search/)
    expect(out.advice).toMatch(/Try ontology next/)
  })

  test('think with gaps points at ontology', () => {
    const out = attachQueryAdvice('think', {
      answer: 'Not sure about cousins.',
      citations: ['Person/msmullin'],
      retrieved: ['Person/msmullin'],
      gaps: ['extended family'],
    })
    expect(thinkEmpty(out)).toBe(false)
    expect(out.advice).toMatch(/gaps/)
    expect(out.advice).toMatch(/try ontology/)
    expect(out.advice).toMatch(/Do not fall back to search/)
  })

  test('ontology empty suggests think/search/get_entity', () => {
    const payload = { answer: "I don't know.", entities: [] }
    expect(ontologyEmpty(payload)).toBe(true)
    const out = attachQueryAdvice('ontology', payload)
    expect(out.advice).toMatch(/think or search/)
    expect(out.advice).toMatch(/get_entity/)
  })

  test('graph empty says this is not English', () => {
    const out = attachQueryAdvice('graph', { matches: [], capped: false })
    expect(graphEmpty(out)).toBe(true)
    expect(out.advice).toMatch(/not English/)
    expect(out.advice).toMatch(/ontology/)
  })

  test('graphql not-found gets NL advice', () => {
    const out = attachQueryAdvice('graphql', { slug: 'Person/nope', error: 'not found' })
    expect(graphqlEmpty(out)).toBe(true)
    expect(out.advice).toMatch(/not a natural-language search/)
  })

  test('graphql success has no advice footer', () => {
    const out = attachQueryAdvice('graphql', { identity: { name: 'Mike' } })
    expect(out.advice).toBeUndefined()
  })

  test('graph success without cap has no advice', () => {
    const out = attachQueryAdvice('graph', { matches: [{ path: ['Person/a'] }], capped: false })
    expect(out.advice).toBeUndefined()
  })
})
