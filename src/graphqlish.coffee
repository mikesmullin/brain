# graphqlish.coffee — a small, deterministic GraphQL-ish traversal DSL.
#   Team/team-cloud { naming, USES_SYSTEM { info } }
# Component aliases return their values; UPPERCASE relation names traverse to
# targets and recurse the nested selection. Every lookup is an indexed pglite
# point query (O(degree) per relation hop) — no world loads, no disk reads.
import { isRelationKey } from './storage.coffee'

tokenize = (s) ->
  toks = []
  re = /\s*([{}(),]|[A-Za-z0-9_/.\-]+)\s*/g
  m = null
  while (m = re.exec(s))
    toks.push(m[1])
  toks

parseSelSet = (toks, pos) ->
  items = []
  # expects toks[pos] === '{'
  pos++
  while pos < toks.length and toks[pos] isnt '}'
    name = toks[pos]; pos++
    node = { name }
    if toks[pos] is '{'
      [children, pos] = parseSelSet(toks, pos)
      node.children = children
    items.push(node)
    pos++ if toks[pos] is ','
  pos++ if toks[pos] is '}'
  [items, pos]

parseQuery = (s) ->
  toks = tokenize(s)
  slug = toks[0]
  sel = null
  if toks[1] is '{'
    [sel, _] = parseSelSet(toks, 1)
  { slug, sel }

# Project a plain component value by a nested selection set (e.g. `identity { name }`).
# No children => return the whole value; children on a non-object => value as-is.
projectValue = (val, children) ->
  return val unless children and val? and typeof val is 'object' and not Array.isArray(val)
  out = {}
  out[child.name] = projectValue(val[child.name] ? null, child.children) for child in children
  out

# Level-batched resolution: ONE entities query + ONE links query per selection
# level (per chunk), instead of 2 point queries per traversed node — so
# hydrating a high-degree hub's neighborhood stays in milliseconds.
resolveMany = (core, slugs, sel, depth = 0) ->
  throw new Error("graphql traversal exceeded max nesting depth (32)") if depth > 32
  ents = await core.idx.fullEntities(slugs)
  # recurse once per relation-with-children, over the UNION of all targets
  childMaps = {}
  for item in sel when isRelationKey(item.name) and item.children
    targets = []
    for slug in slugs
      e = ents[slug]
      continue unless e
      targets.push(t._to) for t in (e.relations?[item.name] or [])
    childMaps[item.name] = await resolveMany(core, [...new Set(targets)], item.children, depth + 1)
  results = {}
  for slug in slugs
    e = ents[slug]
    unless e
      results[slug] = { slug, error: 'not found' }
      continue
    row = { slug: e.slug }
    for item in sel
      if isRelationKey(item.name)
        ts = e.relations?[item.name] or []
        row[item.name] = for t in ts
          if item.children then (childMaps[item.name][t._to] or { slug: t._to, error: 'not found' }) else t._to
      else
        row[item.name] = projectValue(e.components?[item.name] ? null, item.children)
    results[slug] = row
  results

export runGraphql = (core, query) ->
  { slug, sel } = parseQuery(query)
  canonical = await core.resolveSlug(slug)
  return { slug, error: 'not found' } unless canonical
  unless sel
    e = await core.idx.fullEntity(canonical)
    return { slug: e.slug, components: e.components, relations: e.relations }
  (await resolveMany(core, [canonical], sel))[canonical]
