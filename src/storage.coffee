# storage.coffee — read/write flattened entity files and discover them on disk.
#
# Entity file = <storageDir>/<Class>/<id>.md with flattened YAML frontmatter:
#   - lowercase-first keys  => components (typed field bags)
#   - ALL_UPPERCASE keys    => relations   (outgoing typed edges)
# `_class` + `_id` are inferred from the path (Class/id.md), never stored.
import { join, relative, dirname, basename, sep } from 'path'
import { readFile, writeFile, mkdir, readdir, unlink } from 'fs/promises'
import { existsSync } from 'fs'
import yaml from 'js-yaml'
import { parseSlug, formatSlug } from './slug.coffee'

RELATION_KEY_RE = /^[A-Z][A-Z0-9_]*$/

export isRelationKey = (k) -> RELATION_KEY_RE.test(k)

# Split "---\n<yaml>\n---\n<body>" into { front, body }. Tolerant of no body.
splitFrontmatter = (text) ->
  return { front: '', body: text } unless text.startsWith('---')
  # find closing fence
  m = text.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/)
  return { front: '', body: text } unless m
  { front: m[1], body: m[2] ? '' }

# Wiki-style links in the body. Recognized (others are treated as plain page paths + ignored):
#   [[Class/id]]                        -> LINKS_TO Class/id
#   [[Class/id|Anchor text]]            -> LINKS_TO Class/id (anchor is display-only)
#   [[REL:Class/id]]                    -> REL Class/id
#   [[REL:Class/id|Anchor text]]        -> REL Class/id (anchor is display-only)
WIKILINK_RE = /\[\[\s*([^\]]+?)\s*\]\]/g
export extractWikiLinks = (body) ->
  links = []
  return links unless body
  while (m = WIKILINK_RE.exec(body))
    raw = m[1].trim()
    rel = 'LINKS_TO'
    target = raw
    ci = raw.indexOf(':')
    if ci > 0 and RELATION_KEY_RE.test(raw.slice(0, ci))
      rel = raw.slice(0, ci)
      target = raw.slice(ci + 1).trim()
    # strip optional "|Anchor text" (display-only)
    pipe = target.indexOf('|')
    target = target.slice(0, pipe).trim() if pipe >= 0
    continue unless target.indexOf('/') > 0
    try
      s = parseSlug(target)
      links.push({ rel, to: s.slug })
    catch
      # ignore unresolved/unqualified links (plain page paths like [[meetings/2026-01-01]])
  links

# Parse an entity file into { slug, cls, id, components, relations, body, source }.
export parseEntityFile = (filePath, storageDir) ->
  text = await readFile(filePath, 'utf-8')
  { front, body } = splitFrontmatter(text)
  data = if front.trim() then (yaml.load(front) or {}) else {}
  # infer class/id from path relative to the storage dir
  relPath = relative(storageDir, filePath)
  parts = relPath.split(sep)
  throw new Error("entity path must be <storageDir>/<Class>/<id>.md: #{relPath}") unless parts.length is 2
  cls = parts[0]
  id = basename(parts[1], '.md')
  slug = parseSlug(formatSlug(cls, id)).slug
  components = {}
  relations = {}
  for own k, v of data
    if isRelationKey(k)
      relations[k] = normalizeRelation(v)
    else
      components[k] = v
  { slug, cls, id, components, relations, body: (body ? '').trimEnd(), source: filePath }

# A relation value is a list of targets; each target is a slug string or { _to, ...qualifiers }.
normalizeRelation = (v) ->
  arr = if Array.isArray(v) then v else [v]
  arr.map (t) ->
    if typeof t is 'string' then { _to: t }
    else if t and typeof t is 'object' and t._to then t
    else throw new Error("relation target must be a slug string or { _to, ... }: #{JSON.stringify(t)}")

# Serialize an entity object back to file text (frontmatter + optional body).
export serializeEntity = (entity) ->
  front = {}
  for own k, v of (entity.components or {})
    front[k] = v
  for own rel, targets of (entity.relations or {})
    front[rel] = (targets or []).map (t) ->
      keys = Object.keys(t).filter (x) -> x isnt '_to'
      if keys.length is 0 then t._to else t
  dumped = yaml.dump(front, { lineWidth: 100, noRefs: true, sortKeys: false })
  body = if entity.body then "\n#{entity.body}\n" else "\n# #{entity.slug}\n"
  "---\n#{dumped}---\n#{body}"

export entityFilePath = (storageDir, cls, id) -> join(storageDir, cls, "#{id}.md")

# Reconcile body [[Class/id]] / [[REL:Class/id]] links into frontmatter relations.
# Frontmatter is authoritative; body links only ADD new unique edges (default rel LINKS_TO).
export reconcileBodyLinks = (entity) ->
  return entity unless entity.body
  for link in extractWikiLinks(entity.body)
    entity.relations ?= {}
    entity.relations[link.rel] ?= []
    exists = entity.relations[link.rel].some (t) -> t._to is link.to
    entity.relations[link.rel].push({ _to: link.to }) unless exists
  entity

export writeEntityFile = (storageDir, entity) ->
  fp = entityFilePath(storageDir, entity.cls, entity.id)
  await mkdir(dirname(fp), { recursive: true })
  await writeFile(fp, serializeEntity(entity), 'utf-8')
  fp

export removeEntityFile = (storageDir, cls, id) ->
  fp = entityFilePath(storageDir, cls, id)
  await unlink(fp) if existsSync(fp)
  fp

# Recursively find all `*.md` entity files under a storage dir (skips schema.yaml + pgdata).
export discoverEntityFiles = (storageDir) ->
  out = []
  return out unless existsSync(storageDir)
  walk = (dir) ->
    for ent in await readdir(dir, { withFileTypes: true })
      continue if ent.name is 'pgdata'   # pglite data dir lives inside .brain/ — never an entity
      full = join(dir, ent.name)
      if ent.isDirectory()
        await walk(full)
      else if ent.name.endsWith('.md')
        out.push(full)
  await walk(storageDir)
  out

# Load every entity across all storage dirs. Duplicate slugs are reported (not thrown).
export loadEntities = (storageDirs) ->
  entities = []
  bySlug = {}
  duplicates = []
  for dir in storageDirs
    for fp in await discoverEntityFiles(dir)
      try
        e = await parseEntityFile(fp, dir)
      catch err
        entities.push({ error: err.message, source: fp })
        continue
      if bySlug[e.slug]
        duplicates.push({ slug: e.slug, sources: [bySlug[e.slug].source, e.source] })
      else
        bySlug[e.slug] = e
        entities.push(e)
  { entities: entities.filter((e) -> not e.error), errors: entities.filter((e) -> e.error), bySlug, duplicates }
