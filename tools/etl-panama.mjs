#!/usr/bin/env bun
// etl-panama.mjs — one-shot ETL: ICIJ Offshore Leaks CSVs -> a brain dataset.
// Implements the BRAIN_OPTIMIZE.md §3.2 decisions:
//   A: separate entity per category CSV + SAME_NODE_ID links on collisions
//   B: auto-generated ALL_CAPS relations, cardinality mtm, no domain/range (wildcard)
//   C: rich typed per-column components (dates kept as string — ICIJ uses 18-NOV-1999)
//   D: hub nodes uncapped (top-10 largest files reported at the end)
// Edges attach to the START node's frontmatter; endpoints resolve to the FIRST
// class a node_id appeared in; (rel,target) pairs dedup per entity (brain edge
// semantics are set-like); rows with unknown endpoints are dropped and counted.
//
// Usage:  cd /workspace/cli/brain && bun tools/etl-panama.mjs <raw-csv-dir> <out-root>
//   e.g.  bun tools/etl-panama.mjs /workspace/tmp/pgGraph/sandbox/benchmark/datasets/panama/raw /workspace/datasets/panama-brain
import { mkdir } from 'fs/promises'
import { join } from 'path'
import yaml from 'js-yaml'

const [rawDir, outRoot] = process.argv.slice(2)
if (!rawDir || !outRoot) {
  console.error('usage: bun tools/etl-panama.mjs <raw-csv-dir> <out-root>')
  process.exit(1)
}
const dbDir = join(outRoot, 'db')
const t0 = Date.now()
const elapsed = () => ((Date.now() - t0) / 1000).toFixed(1) + 's'

// ---- RFC4180-ish CSV parser (handles quoted fields with commas/newlines) ----
function parseCsv(text) {
  const rows = []
  let row = [], field = '', inQ = false
  for (let i = 0; i < text.length; i++) {
    const c = text[i]
    if (inQ) {
      if (c === '"') {
        if (text[i + 1] === '"') { field += '"'; i++ } else inQ = false
      } else field += c
    } else if (c === '"') inQ = true
    else if (c === ',') { row.push(field); field = '' }
    else if (c === '\n') { row.push(field.endsWith('\r') ? field.slice(0, -1) : field); rows.push(row); row = []; field = '' }
    else field += c
  }
  if (field.length || row.length) { row.push(field); rows.push(row) }
  return rows
}

async function readCsv(name) {
  const text = await Bun.file(join(rawDir, name)).text()
  const rows = parseCsv(text)
  const header = rows.shift()
  if (rows.length && rows[rows.length - 1].length === 1 && rows[rows.length - 1][0] === '') rows.pop()
  return { header, rows }
}

// ---- Pass A: node CSVs -> in-memory records + first-class map ---------------
const CATEGORIES = [
  ['Entity', 'nodes-entities.csv', 'EntityInfo'],
  ['Officer', 'nodes-officers.csv', 'OfficerInfo'],
  ['Intermediary', 'nodes-intermediaries.csv', 'IntermediaryInfo'],
  ['Address', 'nodes-addresses.csv', 'AddressInfo'],
  ['Other', 'nodes-others.csv', 'OtherInfo'],
]
const firstClass = new Map()          // node_id -> Class (first category seen)
const nodes = []                      // { cls, id, fields }
const collisions = []                 // { id, cls1, cls2 }
const componentFields = {}            // CompName -> Set(field)
for (const [cls, file, comp] of CATEGORIES) {
  const { header, rows } = await readCsv(file)
  const idCol = header.indexOf('node_id')
  componentFields[comp] = header.filter((h, i) => i !== idCol && h)
  let kept = 0
  for (const r of rows) {
    const id = (r[idCol] || '').trim()
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(id)) continue
    const fields = {}
    for (let i = 0; i < header.length; i++) {
      if (i === idCol || !header[i]) continue
      const v = (r[i] || '').trim()
      if (v) fields[header[i]] = v
    }
    if (firstClass.has(id)) collisions.push({ id, cls1: firstClass.get(id), cls2: cls })
    else firstClass.set(id, cls)
    nodes.push({ cls, id, fields })
    kept++
  }
  console.log(`[${elapsed()}] ${file}: ${kept} rows -> class ${cls}`)
}
console.log(`[${elapsed()}] nodes total=${nodes.length} unique_ids=${firstClass.size} collisions=${collisions.length}`)

// ---- Pass B: relationships -> edges grouped by start slug -------------------
const RELKEY = /^[A-Z][A-Z0-9_]*$/
const sanitizeRel = (s) => s.trim().replace(/[^A-Za-z0-9]+/g, '_').replace(/^_+|_+$/g, '').toUpperCase()
const { header: rh, rows: relRows } = await readCsv('relationships.csv')
const [cS, cE, cT] = [rh.indexOf('node_id_start'), rh.indexOf('node_id_end'), rh.indexOf('rel_type')]
const edgesByStart = new Map()        // "Class/id" -> Map(REL -> Set(targetSlug))
const relTypes = new Map()            // REL -> count kept
let dropUnknown = 0, dropBadRel = 0, dupEdges = 0, keptEdges = 0
for (const r of relRows) {
  const s = (r[cS] || '').trim(), e = (r[cE] || '').trim()
  const rel = sanitizeRel(r[cT] || '')
  if (!rel || !RELKEY.test(rel)) { dropBadRel++; continue }
  const sc = firstClass.get(s), ec = firstClass.get(e)
  if (!sc || !ec) { dropUnknown++; continue }
  const startSlug = `${sc}/${s}`, endSlug = `${ec}/${e}`
  let rels = edgesByStart.get(startSlug)
  if (!rels) edgesByStart.set(startSlug, rels = new Map())
  let targets = rels.get(rel)
  if (!targets) rels.set(rel, targets = new Set())
  if (targets.has(endSlug)) { dupEdges++; continue }
  targets.add(endSlug)
  relTypes.set(rel, (relTypes.get(rel) || 0) + 1)
  keptEdges++
}
console.log(`[${elapsed()}] edges kept=${keptEdges} dup=${dupEdges} unknown_endpoint=${dropUnknown} bad_rel=${dropBadRel}`)
console.log(`[${elapsed()}] relation types: ${[...relTypes.entries()].map(([k, v]) => `${k}=${v}`).join(' ')}`)

// SAME_NODE_ID sibling links (decision A) — both directions for outgoing-only traversal
let sameLinks = 0
for (const { id, cls1, cls2 } of collisions) {
  for (const [a, b] of [[`${cls1}/${id}`, `${cls2}/${id}`], [`${cls2}/${id}`, `${cls1}/${id}`]]) {
    let rels = edgesByStart.get(a)
    if (!rels) edgesByStart.set(a, rels = new Map())
    let t = rels.get('SAME_NODE_ID')
    if (!t) rels.set('SAME_NODE_ID', t = new Set())
    if (!t.has(b)) { t.add(b); sameLinks++ }
  }
}
console.log(`[${elapsed()}] SAME_NODE_ID links added: ${sameLinks}`)

// ---- schema.yaml + brain.yaml ----------------------------------------------
const schema = { components: {}, classes: {}, relations: {} }
for (const [cls, , comp] of CATEGORIES) {
  schema.components[comp] = { fields: Object.fromEntries(componentFields[comp].map((f) => [f, { type: 'string' }])) }
  schema.classes[cls] = { components: { info: comp }, ...(cls === 'Entity' || cls === 'Officer' ? { top: true } : {}) }
}
for (const rel of [...relTypes.keys()].sort()) schema.relations[rel] = { cardinality: 'mtm' }
schema.relations['SAME_NODE_ID'] = { cardinality: 'mtm' }
await mkdir(dbDir, { recursive: true })
for (const [cls] of CATEGORIES) await mkdir(join(dbDir, cls), { recursive: true })
await Bun.write(join(dbDir, 'schema.yaml'), yaml.dump(schema, { lineWidth: 120, noRefs: true, sortKeys: false }))
await Bun.write(join(outRoot, 'brain.yaml'), yaml.dump({ embed: { model: 'copilot:text-embedding-3-small' }, search: { reranker: 'off' }, storage: [] }, { sortKeys: false }))
console.log(`[${elapsed()}] schema.yaml: ${Object.keys(schema.relations).length} relations, ${CATEGORIES.length} classes`)

// ---- Pass C: emit .md files (bounded concurrency) ---------------------------
const CONCURRENCY = 256
let written = 0
const biggest = []   // top-10 largest files: [bytes, slug]
let pending = []
for (const n of nodes) {
  const slug = `${n.cls}/${n.id}`
  const front = { info: n.fields }
  const rels = edgesByStart.get(slug)
  if (rels) for (const [rel, targets] of rels) front[rel] = [...targets]
  const text = `---\n${yaml.dump(front, { lineWidth: 100, noRefs: true, sortKeys: false })}---\n\n# ${slug}\n`
  if (biggest.length < 10 || text.length > biggest[9][0]) {
    biggest.push([text.length, slug]); biggest.sort((a, b) => b[0] - a[0]); biggest.length = Math.min(biggest.length, 10)
  }
  pending.push(Bun.write(join(dbDir, n.cls, `${n.id}.md`), text))
  if (pending.length >= CONCURRENCY) { await Promise.all(pending); pending = [] }
  if (++written % 100000 === 0) console.log(`[${elapsed()}] wrote ${written}/${nodes.length} .md files`)
}
await Promise.all(pending)
console.log(`[${elapsed()}] DONE: ${written} entities, ${keptEdges + sameLinks} edges, out=${dbDir}`)
console.log('largest .md files (hub nodes, decision D):')
for (const [bytes, slug] of biggest) console.log(`  ${(bytes / 1024).toFixed(0).padStart(6)} KB  ${slug}`)
