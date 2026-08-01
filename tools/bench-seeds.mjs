#!/usr/bin/env bun
// bench-seeds.mjs — derive the benchmark seeds from the indexed Panama db.
// Run STANDALONE (no brain server holding pglite): after reindex, before server start.
//   cd /workspace/cli/brain && BRAIN_ROOT=/workspace/datasets/panama-brain/db bun tools/bench-seeds.mjs
// Writes db/bench/seeds.json: { seedA (highest out-degree), targetB (adjacent),
// targetC (3 hops out), degreeA }. Both systems must use the same node_ids.
import 'bun-coffeescript/register'
import { mkdir } from 'fs/promises'
import { join } from 'path'

const { Index } = await import('../src/index.coffee')
const { paths } = await import('../src/config.coffee')

const idx = new Index(process.cwd())
await idx.open()
if (!(await idx.isIndexed())) { console.error('not indexed'); process.exit(1) }

// Seed A: highest out-degree node (same derivation pgGraph's harness uses)
const top = await idx.db.query(
  'SELECT from_slug, count(*)::int AS n FROM links GROUP BY from_slug ORDER BY n DESC LIMIT 5')
const seedA = top.rows[0].from_slug
console.log('top out-degree:', top.rows.map((r) => `${r.from_slug}(${r.n})`).join(' '))

// Target B: first adjacent node of Seed A
const adj1 = await idx.db.query('SELECT to_slug FROM links WHERE from_slug = $1 ORDER BY to_slug LIMIT 1', [seedA])
const targetB = adj1.rows[0].to_slug

// Target C: a node exactly 3 outgoing hops from Seed A (BFS, first found at depth 3)
let frontier = [seedA]
const seen = new Set(frontier)
let targetC = null
for (let d = 1; d <= 3 && !targetC; d++) {
  const rows = (await idx.db.query('SELECT from_slug, to_slug FROM links WHERE from_slug = ANY($1) LIMIT 200000', [frontier])).rows
  const next = []
  for (const r of rows) {
    if (seen.has(r.to_slug)) continue
    seen.add(r.to_slug)
    next.push(r.to_slug)
    if (d === 3) { targetC = r.to_slug; break }
  }
  frontier = next
  if (!frontier.length) break
}

const out = { seedA, degreeA: top.rows[0].n, targetB, targetC }
const dir = join(paths(process.cwd()).root, 'bench')
await mkdir(dir, { recursive: true })
await Bun.write(join(dir, 'seeds.json'), JSON.stringify(out, null, 2))
console.log('seeds.json:', JSON.stringify(out))
await idx.close()
