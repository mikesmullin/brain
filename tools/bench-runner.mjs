#!/usr/bin/env bun
// bench-runner.mjs — the warm benchmark suite (BENCHMARK.md Phase 3/4).
// Requires a running `brain server` on the bench db and db/bench/seeds.json.
//   cd /workspace/cli/brain && BRAIN_ROOT=/workspace/datasets/panama-brain/db bun tools/bench-runner.mjs
// Protocol per question: 1 unrecorded warmup + 10 measured iterations of the
// RPC round-trip (socket write -> full response parsed). Also one CLI-UX
// sample (whole `brain <cmd>` incl. bun spawn). Results ->
// db/bench/results/<ISO>.yaml and stdout.
import 'bun-coffeescript/register'
import net from 'net'
import { mkdir } from 'fs/promises'
import { join } from 'path'
import { execSync } from 'child_process'
import yaml from 'js-yaml'

const { paths } = await import('../src/config.coffee')
const p = paths(process.cwd())
const seeds = JSON.parse(await Bun.file(join(p.root, 'bench', 'seeds.json')).text())

let reqId = 0
function rpc(method, params = {}) {
  return new Promise((resolve, reject) => {
    const conn = net.connect(p.sock)
    let buf = ''
    conn.on('error', reject)
    conn.on('connect', () => conn.write(JSON.stringify({ id: ++reqId, method, params }) + '\n'))
    conn.on('data', (c) => {
      buf += c.toString('utf8')
      const nl = buf.indexOf('\n')
      if (nl < 0) return
      conn.end()
      const msg = JSON.parse(buf.slice(0, nl))
      msg.error ? reject(new Error(msg.error)) : resolve(msg.result)
    })
  })
}

const ITER = 10
const stats = (xs) => {
  const s = [...xs].sort((a, b) => a - b)
  return { min: s[0], median: s[Math.floor(s.length / 2)], p95: s[Math.min(s.length - 1, Math.ceil(s.length * 0.95) - 1)] }
}
const round = (o) => Object.fromEntries(Object.entries(o).map(([k, v]) => [k, Math.round(v * 100) / 100]))

// The paired questions (see BENCHMARK.md Phase 3 table). resultInfo extracts
// {count, capped} for correctness parity against pgGraph's row counts.
const QUESTIONS = [
  { id: 'q1-status', desc: 'graph loaded / how large', method: 'status', params: {},
    cli: ['server', 'status'],
    resultInfo: (r) => ({ count: r.entities, capped: false }) },
  { id: 'q2-search-mossack', desc: 'entities mentioning Mossack', method: 'search', params: { query: 'Mossack', limit: 20 },
    cli: ['search', '--limit', '20', 'Mossack'],
    resultInfo: (r) => ({ count: r.length, capped: false, sample: r.slice(0, 3).map((x) => x.slug) }) },
  { id: 'q3-two-hop', desc: `two-hop neighborhood of ${seeds.seedA}`, method: 'graph', params: { pattern: `${seeds.seedA} *2> *` },
    cli: ['graph', `${seeds.seedA} *2> *`],
    resultInfo: (r) => ({ count: r.matches.length, capped: r.capped }) },
  { id: 'q4-one-hop-projection', desc: `one-hop projection of ${seeds.seedA}`, method: 'graphql',
    params: { query: `${seeds.seedA} { info { name }, REGISTERED_ADDRESS { info { address } }, OFFICER_OF { info { name } }, INTERMEDIARY_OF { info { name } } }` },
    cli: null,
    resultInfo: (r) => ({ count: Object.values(r).filter(Array.isArray).reduce((a, x) => a + x.length, 0), capped: false }) },
  { id: 'q5-shortest-path', desc: `shortest path ${seeds.seedA} -> ${seeds.targetC}`, method: 'graph',
    params: { pattern: `${seeds.seedA} *6> ${seeds.targetC} --shortest` },
    cli: ['graph', `${seeds.seedA} *6> ${seeds.targetC} --shortest`],
    resultInfo: (r) => ({ count: r.matches.length, capped: r.capped, hops: r.matches[0] ? r.matches[0].path.length - 1 : null }) },
]

const results = []
for (const q of QUESTIONS) {
  await rpc(q.method, q.params)   // warmup (unrecorded)
  const times = []
  let last = null
  for (let i = 0; i < ITER; i++) {
    const t = performance.now()
    last = await rpc(q.method, q.params)
    times.push(performance.now() - t)
  }
  let cliMs = null
  if (q.cli) {
    const t = performance.now()
    execSync(`bun ${join(import.meta.dir, '..', 'bin', 'brain.mjs')} ${q.cli.map((a) => `'${a}'`).join(' ')}`, { stdio: 'ignore', env: process.env })
    cliMs = Math.round(performance.now() - t)
  }
  const rec = { id: q.id, desc: q.desc, rpc_ms: round(stats(times)), cli_ux_ms: cliMs, iterations: ITER, ...q.resultInfo(last) }
  results.push(rec)
  console.log(yaml.dump([rec], { lineWidth: 200, sortKeys: false }))
}

const status = await rpc('status')
const doc = {
  ran_at: new Date().toISOString(),
  machine: execSync('uname -srm').toString().trim(),
  brain_git: execSync('git -C ' + join(import.meta.dir, '..') + ' rev-parse --short HEAD').toString().trim() + '+dirty',
  db: { root: p.root, entities: status.entities, links: status.links, embed: status.embed, server_memory_mb: status.memory_mb },
  seeds,
  protocol: { warmup: 1, iterations: ITER, timing: 'RPC round-trip over unix socket (persistent server, warm)' },
  results,
}
const dir = join(p.root, 'bench', 'results')
await mkdir(dir, { recursive: true })
const file = join(dir, doc.ran_at.replace(/[:.]/g, '-') + '.yaml')
await Bun.write(file, yaml.dump(doc, { lineWidth: 200, sortKeys: false }))
console.log('wrote', file)
