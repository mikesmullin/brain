# graphqlish.coffee — a small, deterministic GraphQL-ish traversal DSL (start simple).
#   Team/team-cloud { naming, USES_SYSTEM { info } }
# Component aliases return their values; UPPERCASE relation names traverse to
# targets and recurse the nested selection.
import { loadWorld, resolveSlug } from './world.coffee'
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

resolve = (world, slug, sel) ->
  e = resolveSlug(world, slug)
  return { slug, error: 'not found' } unless e
  unless sel
    return { slug: e.slug, components: e.components, relations: e.relations }
  out = { slug: e.slug }
  for item in sel
    if isRelationKey(item.name)
      targets = e.relations?[item.name] or []
      out[item.name] = for t in targets
        if item.children then resolve(world, t._to, item.children) else t._to
    else
      out[item.name] = projectValue(e.components?[item.name] ? null, item.children)
  out

export runGraphql = (cwd, query) ->
  world = await loadWorld(cwd)
  { slug, sel } = parseQuery(query)
  resolve(world, slug, sel)
