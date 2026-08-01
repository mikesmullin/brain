# graphmatch.coffee — the single, Mermaid-flavored graph-match syntax.
#
#   Subject -->|PREDICATE| Object          exact class + exact relation
#   *  -->|SUPPORTS| Product               `*` = any class, one hop
#   ** --> ***                             `**`/`***` = any class within 2/3 hops
#   *N --> ...                             `*N` = any class within N hops (any N)
#   >  is shorthand for the unlabeled arrow -->
#   patterns chain:  * -->|SUPPORTS| * -->|OWNS| *
#
# Inline options (work in both CLI and MCP, since the pattern is one string):
#   --shortest       return only the minimum-hop path(s) found
#   --max-nodes N    traversal circuit breaker (default 100000)
#
# Every result set carries `capped`: true when a circuit breaker — not the
# graph's natural edge — stopped the search (pgGraph's clearest API idea).
#
# Traversal runs entirely against the indexed pglite `links` table: one batched
# SQL query per hop for the whole frontier (O(degree) per node), never a
# whole-world scan and never a disk read.

DEFAULT_MAX_NODES = 100000

# Pull inline option tokens out of the pattern string.
extractOptions = (pattern) ->
  opts = { shortest: false, maxNodes: DEFAULT_MAX_NODES }
  s = ' ' + pattern + ' '
  if /\s--shortest\s/.test(s)
    opts.shortest = true
    s = s.replace(/\s--shortest\s/, ' ')
  m = s.match(/\s--max-nodes[= ](\d+)\s/)
  if m
    opts.maxNodes = parseInt(m[1], 10)
    s = s.replace(m[0], ' ')
  { pattern: s.trim(), opts }

# Tokenize into alternating [node, edge, node, edge, node...].
export parsePattern = (pattern) ->
  s = pattern.trim()
  tokens = []
  # split on arrow segments: -->|REL|  or  -->  or  >  — with an optional *N
  # hop-count prefix bound to the arrow (`A *6> B` = A to B within 6 hops)
  re = /\s*(?:\*(\d+)\s*)?(-->\|[^|]+\||-->|>)\s*/g
  lastIndex = 0
  m = null
  while (m = re.exec(s))
    tokens.push({ type: 'node', raw: s.slice(lastIndex, m.index).trim() })
    hops = if m[1] then parseInt(m[1], 10) else null
    lbl = m[2]
    rel = null
    if lbl.startsWith('-->|') then rel = lbl.slice(4, -1).trim()
    tokens.push({ type: 'edge', rel, hops })
    lastIndex = re.lastIndex
  tokens.push({ type: 'node', raw: s.slice(lastIndex).trim() })
  # parse node raws
  nodes = []
  edges = []
  for t in tokens
    if t.type is 'node'
      raw = t.raw
      if /^\*+$/.test(raw)
        nodes.push({ kind: 'wild', degree: raw.length })          # * / ** / ***
      else if (m2 = raw.match(/^\*(\d+)$/))
        nodes.push({ kind: 'wild', degree: parseInt(m2[1], 10) }) # *N — any hop count
      else if raw is ''
        nodes.push({ kind: 'wild', degree: 1 })
      else if raw.indexOf('/') > 0
        nodes.push({ kind: 'node', slug: raw })   # concrete Class/id (case-insensitive)
      else
        nodes.push({ kind: 'class', name: raw })
    else
      edges.push({ rel: t.rel, hops: t.hops })
  { nodes, edges }

# BFS up to maxHops from a SET of start slugs, following outgoing relations.
# The FIRST hop is constrained to `rel` (if given); later hops use any relation.
# One SQL query per hop for the union frontier; per-start visited sets preserve
# the original per-seed BFS semantics. Returns { byStart: {start: [{slug, path, via}]}, visited }.
reachableBatch = (idx, starts, rel, maxHops, budget) ->
  byStart = {}
  state = for s in starts
    seen = {}
    byStart[s] = []
    { start: s, seen, frontier: [{ slug: s, path: [s], via: [] }] }
  hop = 0
  visited = 0
  while hop < maxHops and budget.nodes > 0
    all = []
    for st in state
      all.push(f.slug) for f in st.frontier
    frontierSlugs = [...new Set(all)]
    break unless frontierSlugs.length
    useRel = if hop is 0 then rel else null
    rows = await idx.frontierOut(frontierSlugs, useRel)
    bySrc = {}
    (bySrc[r.from_slug] ?= []).push(r) for r in rows
    for st in state
      next = []
      for f in st.frontier
        for row in (bySrc[f.slug] or [])
          continue if st.seen[row.to_slug]
          st.seen[row.to_slug] = true
          rec = { slug: row.to_slug, path: f.path.concat(row.to_slug), via: f.via.concat(row.rel) }
          byStart[st.start].push(rec)
          next.push(rec)
          visited++
          budget.nodes--
          if budget.nodes <= 0
            budget.capped = true
            break
        break if budget.nodes <= 0
      st.frontier = next
      break if budget.nodes <= 0
    hop++
  { byStart, visited }

nodeMatchesFactory = (idx) ->
  # class lookups are batched per stage via classesOf; concrete-slug matches are string compares
  (node, slug, classes) ->
    return true if node.kind is 'wild'
    return slug.toLowerCase() is node.slug.toLowerCase() if node.kind is 'node'
    classes[slug] is node.name

# --shortest fast path for `A *N> B`-shaped patterns (two concrete endpoints,
# one unlabeled edge): bidirectional BFS over the directed links — expand the
# smaller frontier each round, EXIT at the first meeting level instead of
# enumerating the whole N-hop neighborhood and filtering (which is what made
# shortest-path ~90x slower than pgGraph's CSR BFS in benchmark run 1).
shortestDirected = (idx, from, to, maxHops, budget) ->
  return { matches: [{ path: [from], via: [], end: from }], capped: false, shortest: true } if from is to
  fwdParents = {}; fwdParents[from] = null      # node -> { prev, rel }
  bwdParents = {}; bwdParents[to] = null        # node -> { next, rel }
  fwdFrontier = [from]
  bwdFrontier = [to]
  depthUsed = 0
  meets = []
  while depthUsed < maxHops and fwdFrontier.length and bwdFrontier.length and not meets.length
    if fwdFrontier.length <= bwdFrontier.length
      rows = await idx.frontierOut(fwdFrontier)
      next = []
      for row in rows
        continue if fwdParents[row.to_slug] isnt undefined
        fwdParents[row.to_slug] = { prev: row.from_slug, rel: row.rel }
        next.push(row.to_slug)
        meets.push(row.to_slug) if bwdParents[row.to_slug] isnt undefined
        budget.nodes -= 1
        if budget.nodes <= 0
          budget.capped = true
          break
      fwdFrontier = next
    else
      rows = await idx.frontierIn(bwdFrontier)
      next = []
      for row in rows
        continue if bwdParents[row.from_slug] isnt undefined
        bwdParents[row.from_slug] = { next: row.to_slug, rel: row.rel }
        next.push(row.from_slug)
        meets.push(row.from_slug) if fwdParents[row.from_slug] isnt undefined
        budget.nodes -= 1
        if budget.nodes <= 0
          budget.capped = true
          break
      bwdFrontier = next
    depthUsed++
    break if budget.capped
  matches = []
  seen = {}
  for m in meets
    # forward half: m -> ... -> from (reversed), then backward half: m -> ... -> to
    path = [m]; via = []
    cur = m
    while fwdParents[cur]
      via.unshift(fwdParents[cur].rel)
      cur = fwdParents[cur].prev
      path.unshift(cur)
    cur = m
    while bwdParents[cur]
      via.push(bwdParents[cur].rel)
      cur = bwdParents[cur].next
      path.push(cur)
    continue if path.length - 1 > maxHops
    key = path.join(' -> ')
    continue if seen[key]
    seen[key] = true
    matches.push({ path, via, end: path[path.length - 1] })
  if matches.length
    minLen = Math.min((m.path.length for m in matches)...)
    matches = matches.filter (m) -> m.path.length is minLen
  { matches, capped: budget.capped, shortest: true }

export matchPattern = (core, rawPattern) ->
  idx = core.idx
  { pattern, opts } = extractOptions(rawPattern)
  { nodes, edges } = parsePattern(pattern)
  budget = { nodes: opts.maxNodes, capped: false }
  nodeMatches = nodeMatchesFactory(idx)

  # --shortest between two concrete endpoints over an unlabeled edge: take the
  # early-exit bidirectional BFS instead of enumerate-then-filter. Labeled or
  # wildcard-endpoint patterns keep the general path below.
  if opts.shortest and nodes.length is 2 and edges.length is 1 and
      nodes[0].kind is 'node' and nodes[1].kind is 'node' and not edges[0].rel
    from = await idx.resolveSlugDb(nodes[0].slug)
    to = await idx.resolveSlugDb(nodes[1].slug)
    return { matches: [], capped: false, shortest: true } unless from and to
    # O(1) impossibility check: different connected components -> no path can
    # exist. Only available after `brain server components` has labeled nodes.
    try
      r = await idx.db.query 'SELECT slug, component_id FROM entities WHERE slug = ANY($1)', [[from, to]]
      cids = {}
      cids[row.slug] = row.component_id for row in r.rows
      if cids[from]? and cids[to]? and cids[from] isnt cids[to]
        return { matches: [], capped: false, shortest: true, no_path: 'different_components' }
    catch
      # component_id column absent (components never computed) — fall through
    maxHops = edges[0].hops or 1
    return await shortestDirected(idx, from, to, maxHops, budget)

  # seed candidates from node[0] — index-backed, never a world scan
  seedSlugs = switch nodes[0].kind
    when 'node'
      s = await idx.resolveSlugDb(nodes[0].slug)
      if s then [s] else []
    when 'class'
      await idx.classSlugs(nodes[0].name)
    else
      r = await idx.db.query 'SELECT slug FROM entities ORDER BY slug LIMIT $1', [opts.maxNodes]
      budget.capped = true if r.rows.length >= opts.maxNodes
      (row.slug for row in r.rows)
  paths = ({ slug: s, path: [s], via: [] } for s in seedSlugs)

  for edge, i in edges
    node = nodes[i + 1]
    maxHops = edge.hops or (if node.kind is 'wild' then node.degree else 1)
    starts = [...new Set(paths.map (p) -> p.slug)]
    { byStart } = await reachableBatch(idx, starts, edge.rel, maxHops, budget)
    # batch-resolve classes for all candidate endpoints in one query
    candidates = []
    for s in starts
      candidates.push(r.slug) for r in (byStart[s] or [])
    candidates = [...new Set(candidates)]
    classes = if node.kind is 'class' then await idx.classesOf(candidates) else {}
    nextPaths = []
    for p in paths
      for r in (byStart[p.slug] or [])
        continue unless nodeMatches(node, r.slug, classes)
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

  if opts.shortest and uniq.length
    minLen = Math.min((u.path.length for u in uniq)...)
    uniq = uniq.filter (u) -> u.path.length is minLen

  { matches: uniq, capped: budget.capped, shortest: opts.shortest }

export runQuery = (core, pattern, opts = {}) ->
  pattern = "#{pattern} --shortest" if opts.shortest and not /--shortest/.test(pattern)
  pattern = "#{pattern} --max-nodes #{opts.maxNodes}" if opts.maxNodes and not /--max-nodes/.test(pattern)
  await matchPattern(core, pattern)
