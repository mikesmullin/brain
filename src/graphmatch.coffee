# graphmatch.coffee — the single, Mermaid-flavored graph-match syntax.
#
#   Subject -->|PREDICATE| Object          exact class + exact relation
#   *  -->|SUPPORTS| Product               `*` = any class, one hop
#   ** --> ***                             `**`/`***` = any class within 2/3 hops
#   >  is shorthand for the unlabeled arrow -->
#   patterns chain:  * -->|SUPPORTS| * -->|OWNS| *
#
# A node is a ClassName (exact) or a wildcard whose star-count is the max hop
# distance allowed to reach it from the previous node.
import { loadWorld } from './world.coffee'

# Tokenize into alternating [node, edge, node, edge, node...].
export parsePattern = (pattern) ->
  # normalize: `>` (not part of -->) already covered; split on arrows keeping labels
  s = pattern.trim()
  tokens = []
  # split on arrow segments: -->|REL|  or  -->  or  >
  re = /\s*(-->\|[^|]+\||-->|>)\s*/g
  lastIndex = 0
  m = null
  while (m = re.exec(s))
    tokens.push({ type: 'node', raw: s.slice(lastIndex, m.index).trim() })
    lbl = m[1]
    rel = null
    if lbl.startsWith('-->|') then rel = lbl.slice(4, -1).trim()
    tokens.push({ type: 'edge', rel })
    lastIndex = re.lastIndex
  tokens.push({ type: 'node', raw: s.slice(lastIndex).trim() })
  # parse node raws
  nodes = []
  edges = []
  for t in tokens
    if t.type is 'node'
      raw = t.raw
      if /^\*+$/.test(raw)
        nodes.push({ kind: 'wild', degree: raw.length })
      else if raw is ''
        nodes.push({ kind: 'wild', degree: 1 })
      else if raw.indexOf('/') > 0
        nodes.push({ kind: 'node', slug: raw })   # concrete Class/id (case-insensitive)
      else
        nodes.push({ kind: 'class', name: raw })
    else
      edges.push({ rel: t.rel })
  { nodes, edges }

nodeMatches = (world, node, slug) ->
  return true if node.kind is 'wild'
  e = world.bySlug[slug]
  return false unless e
  return e.slug.toLowerCase() is node.slug.toLowerCase() if node.kind is 'node'
  e.cls is node.name

# BFS up to maxHops from `slug`, following outgoing relations. The FIRST hop is
# constrained to `rel` (if given); later hops use any relation.
reachable = (world, slug, rel, maxHops) ->
  out = []      # [{ slug, path:[...], via:[...] }]
  seen = {}
  frontier = [{ slug, path: [slug], via: [] }]
  hop = 0
  while hop < maxHops and frontier.length
    next = []
    useRel = if hop is 0 then rel else null
    for f in frontier
      e = world.bySlug[f.slug]
      continue unless e
      for own r, targets of (e.relations or {})
        continue if useRel and r isnt useRel
        for t in targets
          key = t._to
          continue if seen[key]
          seen[key] = true
          rec = { slug: key, path: f.path.concat(key), via: f.via.concat(r) }
          out.push(rec)
          next.push(rec)
    frontier = next
    hop++
  out

export matchPattern = (world, pattern) ->
  { nodes, edges } = parsePattern(pattern)
  # seed candidates from node[0]
  paths = ({ slug: e.slug, path: [e.slug], via: [] } for e in world.entities when nodeMatches(world, nodes[0], e.slug))
  for edge, i in edges
    node = nodes[i + 1]
    maxHops = if node.kind is 'wild' then node.degree else 1
    nextPaths = []
    for p in paths
      for r in reachable(world, p.slug, edge.rel, maxHops)
        continue unless nodeMatches(world, node, r.slug)
        nextPaths.push({ slug: r.slug, path: p.path.concat(r.path.slice(1)), via: p.via.concat(r.via) })
    paths = nextPaths
  # de-dup by full path
  seen = {}
  uniq = []
  for p in paths
    key = p.path.join(' -> ')
    unless seen[key]
      seen[key] = true
      uniq.push({ path: p.path, via: p.via, end: p.slug })
  uniq

export runQuery = (cwd, pattern) ->
  world = await loadWorld(cwd)
  matchPattern(world, pattern)
