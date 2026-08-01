# analysis.coffee — whole-graph maintenance analytics.
#
# Two operations, both run inside the server as USER-CONTROLLED maintenance
# (never on the query path) with results cached like the size counters:
#
#   computeComponents(idx)  connected components via union-find over one pass
#                           of `links` (undirected); writes component_id back
#                           onto entities (indexed) + a summary into meta.
#   vizLayout(idx, cwd)     2D/3D positions for every node: per-component BFS
#                           radial layout (component hub at the crown — the
#                           depth-from-hub heuristic doubles as the 3D z-axis),
#                           islands packed on a phyllotaxis spiral, singletons
#                           on an outer "dust ring". Cached to db/viz/*.
#
# Memory model: integer node ids over typed arrays (a slug index + CSR-style
# adjacency), so 2M nodes / 2.9M edges fits comfortably in a few hundred MB.
import { mkdir, writeFile } from 'fs/promises'
import { join } from 'path'
import { paths } from './config.coffee'

GOLDEN_ANGLE = 2.399963229728653

# ---- shared: load the integer universe (slug index + edge arrays) ----------

loadUniverse = (idx, log = ->) ->
  # Chunked result sets throughout: a single multi-million-row query result
  # overflows pglite's WASM heap. Slugs page by keyset over the PK index;
  # links page by LIMIT/OFFSET (no PK — sequential scans, bounded per batch).
  CHUNK = 100000
  slugs = []
  last = ''
  loop
    batch = (await idx.db.query 'SELECT slug FROM entities WHERE slug > $1 ORDER BY slug LIMIT ' + CHUNK, [last]).rows
    break unless batch.length
    slugs.push(r.slug) for r in batch
    last = batch[batch.length - 1].slug
    break if batch.length < CHUNK
  n = slugs.length
  index = new Map()
  index.set(slugs[i], i) for i in [0...n]
  log "universe: #{n} nodes"
  # LIMIT/OFFSET chunking is only stable if every scan starts at the table's
  # beginning — Postgres's synchronize_seqscans otherwise starts new scans
  # mid-table (piggybacking on prior ones), silently skipping/duplicating
  # rows across chunks. Found the hard way: two runs disagreed by 35k edges.
  await idx.db.exec 'SET synchronize_seqscans = off;'
  srcs = []
  dsts = []
  offset = 0
  loop
    batch = (await idx.db.query "SELECT from_slug, to_slug FROM links LIMIT #{CHUNK} OFFSET #{offset}").rows
    break unless batch.length
    for row in batch
      a = index.get(row.from_slug)
      b = index.get(row.to_slug)
      continue unless a? and b?
      srcs.push(a)
      dsts.push(b)
    offset += batch.length
    log "edges: loaded #{offset}" if offset % 500000 is 0
    break if batch.length < CHUNK
  m = srcs.length
  expected = (await idx.db.query 'SELECT count(*)::int AS n FROM links').rows[0].n
  log "edges: #{m} usable of #{expected}"
  throw new Error("edge scan incomplete: got #{offset} rows, links has #{expected} — aborting rather than computing wrong components") if offset < expected
  src = Int32Array.from(srcs)
  dst = Int32Array.from(dsts)
  { slugs, index, src, dst, n, m }

# Undirected CSR adjacency from directed edge arrays (both directions).
buildCsr = (n, src, dst) ->
  m = src.length
  deg = new Int32Array(n)
  for i in [0...m]
    deg[src[i]]++
    deg[dst[i]]++
  offsets = new Int32Array(n + 1)
  offsets[i + 1] = offsets[i] + deg[i] for i in [0...n]
  nbr = new Int32Array(2 * m)
  cursor = Int32Array.from(offsets.subarray(0, n))
  for i in [0...m]
    nbr[cursor[src[i]]++] = dst[i]
    nbr[cursor[dst[i]]++] = src[i]
  { offsets, nbr, deg }

# ---- union-find -------------------------------------------------------------

class DSU
  constructor: (n) ->
    @parent = new Int32Array(n)
    @parent[i] = i for i in [0...n]
    @sz = new Int32Array(n).fill(1)
  find: (x) ->
    root = x
    root = @parent[root] while @parent[root] isnt root
    while @parent[x] isnt root
      nxt = @parent[x]
      @parent[x] = root
      x = nxt
    root
  union: (a, b) ->
    ra = @find(a)
    rb = @find(b)
    return if ra is rb
    [ra, rb] = [rb, ra] if @sz[ra] < @sz[rb]
    @parent[rb] = ra
    @sz[ra] += @sz[rb]

# Label every node with a component id, ordered by component size descending
# (component 0 = the giant). Returns { comp: Int32Array, sizes: [..desc..] }.
labelComponents = (n, src, dst) ->
  dsu = new DSU(n)
  dsu.union(src[i], dst[i]) for i in [0...src.length]
  rootToId = new Map()
  sizes = []
  comp = new Int32Array(n)
  order = []   # [root, size] sorted desc later
  for i in [0...n]
    r = dsu.find(i)
    order.push([r, dsu.sz[r]]) unless rootToId.has(r)
    rootToId.set(r, -1) unless rootToId.has(r)
  order.sort (a, b) -> b[1] - a[1]
  for [root, size], id in order
    rootToId.set(root, id)
    sizes.push(size)
  comp[i] = rootToId.get(dsu.find(i)) for i in [0...n]
  { comp, sizes }

# ---- components maintenance op ---------------------------------------------

export computeComponents = (idx, log = ->) ->
  t0 = Date.now()
  { slugs, src, dst, n, m } = await loadUniverse(idx, log)
  log "union-find over #{m} links ..."
  { comp, sizes } = labelComponents(n, src, dst)
  isolated = sizes.filter((s) -> s is 1).length
  log "labeling entities with component_id (#{n} rows) ..."
  await idx.db.exec 'ALTER TABLE entities ADD COLUMN IF NOT EXISTS component_id integer;'
  await idx.db.exec 'DROP TABLE IF EXISTS _comp_stage; CREATE TABLE _comp_stage (slug text PRIMARY KEY, cid integer);'
  await idx._insertMany '_comp_stage', ['slug', 'cid'], ([slugs[i], comp[i]] for i in [0...n])
  await idx.db.exec 'UPDATE entities e SET component_id = s.cid FROM _comp_stage s WHERE e.slug = s.slug;'
  await idx.db.exec 'DROP TABLE _comp_stage; CREATE INDEX IF NOT EXISTS entities_comp_idx ON entities (component_id);'
  summary = {
    components: sizes.length
    largest: { component_id: 0, size: sizes[0] or 0, pct: if n then Math.round((sizes[0] or 0) / n * 1000) / 10 else 0 }
    next: sizes.slice(1, 5)
    isolated
    nodes: n
    links: m
    refreshed_at: new Date().toISOString()
    ms: Date.now() - t0
  }
  await idx.setMeta 'components_summary', JSON.stringify(summary)
  await idx.db.exec 'VACUUM ANALYZE entities;'
  summary

export componentsStats = (idx) ->
  raw = await idx.meta('components_summary')
  return null unless raw
  JSON.parse(raw)

# ---- viz layout -------------------------------------------------------------
#
# Per component: BFS from the highest-degree node (the hub). Ring d holds all
# nodes at depth d, radius grows to fit the ring's population; angles follow
# the parent's angle so subtrees stay contiguous. z = (maxDepth - depth) so
# the hub sits at the crown (the "Christmas tree" heuristic). Islands packed
# by descending size on a phyllotaxis spiral; size-1 components form a dust
# ring outside everything.

# Fibonacci-sphere direction i of k: golden-angle spiral over the full sphere,
# so a BFS shell's nodes spread evenly in ALL directions ("mold grown in
# ballistic gel"): each component is a volumetric ball around its hub, not a
# flat disk. The client's 2D mode simply flattens z (disk projection).
fibDir = (i, k) ->
  z = 1 - (2 * (i + 0.5)) / Math.max(1, k)
  r = Math.sqrt(Math.max(0, 1 - z * z))
  a = i * GOLDEN_ANGLE
  [Math.cos(a) * r, Math.sin(a) * r, z]

layoutComponent = (nodes, csr, positions) ->
  # hub = max degree
  hub = nodes[0]
  for i in nodes
    hub = i if csr.deg[i] > csr.deg[hub]
  depth = new Map()
  order = new Map()   # parent's placement order — keeps subtrees angularly adjacent
  depth.set(hub, 0)
  order.set(hub, 0)
  rings = [[hub]]
  frontier = [hub]
  while frontier.length
    next = []
    for u in frontier
      for k in [csr.offsets[u]...csr.offsets[u + 1]]
        v = csr.nbr[k]
        continue if depth.has(v)
        depth.set(v, depth.get(u) + 1)
        order.set(v, order.get(u))   # provisional: inherit parent order
        next.push(v)
    rings.push(next) if next.length
    frontier = next
  SPACING2 = 2.6    # ~area per node on a shell
  BASE = 3.0
  r = 0
  radii = [0]
  for ring, d in rings when d > 0
    # shell of area 4πr² must hold ring.length nodes
    needed = Math.sqrt(ring.length * SPACING2 / (4 * Math.PI))
    r = Math.max(r + BASE, needed)
    radii.push(r)
  R = (radii[radii.length - 1] or 0) + BASE
  positions[3 * hub] = 0
  positions[3 * hub + 1] = 0
  positions[3 * hub + 2] = 0
  for ring, d in rings when d > 0
    ring.sort (a, b) -> order.get(a) - order.get(b)
    for u, j in ring
      order.set(u, j)
      [dx, dy, dz] = fibDir(j, ring.length)
      positions[3 * u] = dx * radii[d]
      positions[3 * u + 1] = dy * radii[d]
      positions[3 * u + 2] = dz * radii[d]
  R

export vizLayout = (idx, cwd, log = ->) ->
  t0 = Date.now()
  { slugs, src, dst, n, m } = await loadUniverse(idx, log)
  log 'labeling components ...'
  { comp, sizes } = labelComponents(n, src, dst)
  log 'building adjacency ...'
  csr = buildCsr(n, src, dst)
  # group node ids per component (skip singletons — they go to the dust ring)
  members = new Map()
  singles = []
  for i in [0...n]
    if sizes[comp[i]] is 1
      singles.push(i)
    else
      arr = members.get(comp[i])
      unless arr
        arr = []
        members.set(comp[i], arr)
      arr.push(i)
  positions = new Float32Array(3 * n)
  log "laying out #{members.size} multi-node components ..."
  # layout each component around local origin, collect its radius
  ids = [...members.keys()].sort (a, b) -> sizes[b] - sizes[a]
  placed = []   # { id, R, cx, cy }
  cumArea = 0
  for id, k in ids
    nodes = members.get(id)
    R = layoutComponent(nodes, csr, positions)
    if k is 0
      cx = 0; cy = 0
    else
      cumArea += Math.PI * R * R * 2.2
      rr = Math.sqrt(cumArea / Math.PI) + (placed[0]?.R or 0)
      a = k * GOLDEN_ANGLE
      cx = Math.cos(a) * rr
      cy = Math.sin(a) * rr
    for u in nodes
      positions[3 * u] += cx
      positions[3 * u + 1] += cy
    placed.push({ id, R, cx, cy })
    log "  placed #{k + 1}/#{ids.length} components" if (k + 1) % 20000 is 0
  # dust ring of singletons, just outside everything placed
  maxR = 0
  maxR = Math.max(maxR, Math.hypot(p.cx, p.cy) + p.R) for p in placed
  dustR = maxR * 1.06 + 20
  for u, j in singles
    a = (j / Math.max(1, singles.length)) * 2 * Math.PI + (j % 7) * 0.011
    rr = dustR + (j % 13) * 1.1
    positions[3 * u] = Math.cos(a) * rr
    positions[3 * u + 1] = Math.sin(a) * rr
    positions[3 * u + 2] = 0
  # node size channel: log2(1+degree)
  psize = new Float32Array(n)
  psize[i] = Math.log2(1 + csr.deg[i]) for i in [0...n]
  compArr = new Uint32Array(comp)
  # ---- write cache files ----
  dir = join(paths(cwd).root, 'viz')
  await mkdir(dir, { recursive: true })
  buf = Buffer.concat([
    Buffer.from(positions.buffer, 0, positions.byteLength)
    Buffer.from(compArr.buffer, 0, compArr.byteLength)
    Buffer.from(psize.buffer, 0, psize.byteLength)
  ])
  await writeFile join(dir, 'layout.bin'), buf
  await writeFile join(dir, 'slugs.txt'), slugs.join('\n')
  meta = {
    nodes: n
    links: m
    components: sizes.length
    isolated: singles.length
    largest: sizes[0] or 0
    world_radius: Math.round(dustR + 40)
    layout: 'sphere-v1'   # volumetric BFS shells; 2D mode = flattened projection
    generated_at: new Date().toISOString()
    ms: Date.now() - t0
  }
  await writeFile join(dir, 'meta.json'), JSON.stringify(meta, null, 2)
  meta
