# enrich.coffee (command) — rewrite a Markdown document (or a tree of them) in place,
# inserting [[wiki-links]] to brain entities.
#
#   brain enrich <path>            <path> is a .md file or a directory (recursed)
#   brain enrich <path> --ingest   also CREATE missing entities (else: link existing only)
#
# A microagent reads each doc, identifies entities/relationships (like `ingest`), then
# calls `replace_entire_file` with the fully-linked markdown. Without `--ingest` it links
# only to entities that already exist; with `--ingest` new entities are created leniently
# (resolve later with `brain refine`).
import { readFile, writeFile, readdir, stat } from 'fs/promises'
import { join, extname } from 'path'
import Agent from 'agl-ai'
import yaml from 'js-yaml'
import { loadWorld } from '../world.coffee'
import { loadConfig } from '../config.coffee'
import { parseArgs } from '../args.coffee'
import { describeSchema } from '../extract.coffee'
import { hybridSearch } from '../search.coffee'
import { batchUpsert } from '../upsert.coffee'
import { parseSlug, formatSlug } from '../slug.coffee'
import { isRelationKey } from '../storage.coffee'

mdFiles = (root) ->
  st = await stat(root)
  return [root] if st.isFile() and extname(root).toLowerCase() is '.md'
  out = []
  walk = (d) ->
    for ent in await readdir(d, { withFileTypes: true })
      continue if ent.name is 'pgdata' or ent.name.startsWith('.')
      full = join(d, ent.name)
      if ent.isDirectory() then await walk(full)
      else if extname(ent.name).toLowerCase() is '.md' then out.push(full)
  await walk(root)
  out

systemPrompt = (allowIngest) ->
  create =
    if allowIngest
      """- If a clearly-referenced entity does NOT exist, call `create_entity` to add it (minimal fields
  are fine — it will be refined later), then link to the returned slug."""
    else
      """- If a referenced entity does NOT already exist in the graph, do NOT create it — leave that
  mention as plain prose (no link). Only link to entities that `search` confirms already exist."""
  """
You enrich a Markdown document by inserting wiki-links to a knowledge graph.
- Identify entities the document mentions that match the schema's classes (people, teams,
  products, systems, services, etc.) and the relationships between them.
- For each mention, use `search` to see whether the entity already exists. Prefer linking to
  an existing entity over creating a new one.
#{create}
- Insert links using this exact wiki syntax (the `.md` body, not YAML):
    [[Class/id]]                 a plain mention
    [[Class/id|display text]]    keep the document's original wording as display text
    [[REL:Class/id]]             when the sentence expresses a typed relationship (REL is a schema relation)
- Do NOT invent classes, relations, or ids that aren't in the schema / returned by tools.
- Preserve the document's prose, structure, and meaning; only add links (and keep original wording
  via the |display form). When done, call `replace_entire_file` ONCE with the full new document,
  then report via `done`.
"""

buildEntity = (cls, id, front) ->
  slug = parseSlug(formatSlug(cls, id)).slug
  components = {}; relations = {}
  for own k, v of (front or {}) when k not in ['_class', '_id']
    if isRelationKey(k)
      relations[k] = (if Array.isArray(v) then v else [v]).map (t) -> if typeof t is 'string' then { _to: t } else t
    else components[k] = v
  { slug, cls, id, components, relations, body: '' }

enrichFile = (cwd, cfg, filePath, allowIngest) ->
  text = await readFile(filePath, 'utf-8')
  world = await loadWorld(cwd)
  schemaDoc = describeSchema(world.schema)
  created = []
  wrote = { done: false }

  agent = await Agent.factory
    model: cfg.think.model
    reasoning_effort: 'low'
    system_prompt: systemPrompt(allowIngest)
    parallel_tools: true
    output_tool:
      name: 'done'
      description: 'Report the enrichment result.'
      parameters:
        summary: { type: 'string' }
        links_added: { type: 'integer' }
      required: []

  agent.Tool 'search', 'Search the knowledge graph for an existing entity by name/keywords. Returns candidate slugs.',
    { query: { type: 'string' } }, ['query'],
    (c, { query }) ->
      res = await hybridSearch(cwd, query, { limit: 8 })
      yaml.dump(res, { lineWidth: 120, sortKeys: false, noRefs: true })

  if allowIngest
    agent.Tool 'create_entity', 'Create a NEW entity (only when it does not already exist). `frontmatter` is optional flattened YAML (lowercase keys=components, UPPERCASE=relations). Returns the created slug.',
      { class: { type: 'string' }, id: { type: 'string' }, frontmatter: { type: 'string' } }, ['class', 'id'],
      (c, args) ->
        try
          front = if args.frontmatter then (yaml.load(args.frontmatter) or {}) else {}
          e = buildEntity(args.class, args.id, front)
          [res] = await batchUpsert(world, [e], { lenient: true })
          world = await loadWorld(cwd)   # refresh so later tools see the new entity
          created.push(res.slug)
          "created #{res.slug}"
        catch err
          "error: #{err.message}"

  agent.Tool 'replace_entire_file', 'Overwrite the document with your enriched markdown (with [[wiki-links]] inserted). Call once when finished.',
    { content: { type: 'string' } }, ['content'],
    (c, { content }) ->
      await writeFile(filePath, content, 'utf-8')
      wrote.done = true
      'file written'

  prompt = """
    <schema>
    #{schemaDoc}
    </schema>
    <document path="#{filePath}">
    #{text}
    </document>
    Enrich the document with wiki-links per the rules, then call `replace_entire_file` and `done`.
  """
  r = await agent.run prompt: prompt
  { file: filePath, written: wrote.done, created, summary: r?.summary }

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['ingest'] })
  target = _[0]
  throw new Error("usage: brain enrich <path> [--ingest]") unless target
  allowIngest = !!flags.ingest
  cfg = await loadConfig(cwd)
  files = await mdFiles(target)
  throw new Error("no .md files found at #{target}") unless files.length
  for f in files
    res = await enrichFile(cwd, cfg, f, allowIngest)
    mark = if res.written then '✓' else '·'
    extra = if res.created.length then " (+#{res.created.length} new: #{res.created.join(', ')})" else ''
    console.log "#{mark} #{f}#{extra}"
  tail = if allowIngest then " — run `brain reindex` to pick up new entities" else ''
  console.log "enriched #{files.length} file(s)#{tail}"
  0
