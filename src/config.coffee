# config.coffee — resolves the cwd-local `db/` root and its config.
#
# Layout (relative to CWD):
#   brain.yaml                    the config (optional), sibling of db/
#   db/<Class>/<id>.md            authoritative entities (git-tracked)
#   db/schema.yaml                T-box schema (git-tracked)
#   db/pgdata/                    pglite data dir (gitignored, rebuildable)
#
# The runtime graph is the union of `db/` and any additional storage
# directories listed in brain.yaml (enabling per-repo/per-ACL knowledge bases).
#
# Named brains (aliases) live in ~/.config/brain/brains.yaml:
#   <alias>: <project-root>   # directory that contains db/
#   current: <alias|none>
# Any command that resolves a real db/ auto-registers it there if missing
# (see ensureBrainRegistered) so `brain use` stays in sync with reality.
import { homedir } from 'os'
import { resolve, join, isAbsolute, dirname, basename } from 'path'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'fs'
import yaml from 'js-yaml'
import { readFile } from 'fs/promises'

export BRAINS_CONFIG_PATH = join(homedir(), '.config', 'brain', 'brains.yaml')

# Expand ~ and resolve relative alias paths against the brains.yaml directory.
export resolveBrainProjectPath = (value) ->
  path = String(value).trim().replace(/^~(?=\/|$)/, homedir())
  if isAbsolute(path) then path else resolve(dirname(BRAINS_CONFIG_PATH), path)

export loadBrainsConfig = ->
  return {} unless existsSync(BRAINS_CONFIG_PATH)
  raw = yaml.load(readFileSync(BRAINS_CONFIG_PATH, 'utf-8')) or {}
  unless raw? and typeof raw is 'object' and not Array.isArray(raw)
    throw new Error("brain aliases file must contain a YAML mapping: #{BRAINS_CONFIG_PATH}")
  raw

export saveBrainsConfig = (brains) ->
  mkdirSync(dirname(BRAINS_CONFIG_PATH), { recursive: true })
  writeFileSync(BRAINS_CONFIG_PATH, yaml.dump(brains, { sortKeys: false, lineWidth: 120 }), 'utf-8')

# Alias values point at the project root (parent of db/); brainRoot is the db/.
export projectRootFromDb = (dbRoot) -> dirname(resolve(dbRoot))

# If the resolved db/ isn't already listed under any alias in brains.yaml,
# add it (alias = project-directory basename, with -2/-3… on collision).
# Does NOT change `current`. Best-effort: never throws into the caller —
# alias bookkeeping must not break search/server/viz.
# Returns the alias used (existing or newly written), or undefined on skip/fail.
export ensureBrainRegistered = (cwd = process.cwd()) ->
  try
    dbRoot = resolve(brainRoot(cwd))
    return unless existsSync(dbRoot)
    projectRoot = projectRootFromDb(dbRoot)

    brains = loadBrainsConfig()
    for name, path of brains when name isnt 'current' and typeof path is 'string'
      try
        return name if resolve(resolveBrainProjectPath(path)) is projectRoot
      catch
        continue

    base = basename(projectRoot).replace(/[^\w.-]+/g, '-') or 'brain'
    # Avoid clobbering the reserved `current` key if someone names a dir that.
    base = 'brain' if base is 'current'
    alias = base
    n = 2
    while Object.prototype.hasOwnProperty.call(brains, alias)
      alias = "#{base}-#{n}"
      n++

    brains[alias] = projectRoot
    saveBrainsConfig(brains)
    alias
  catch err
    if process.env.DEBUG
      console.error "ensureBrainRegistered: #{err.message}"
    undefined

configuredBrainRoot = ->
  try
    raw = loadBrainsConfig()
    alias = raw.current
    return unless alias? and alias isnt 'none' and typeof raw[alias] is 'string'
    join(resolveBrainProjectPath(raw[alias]), 'db')
  catch
    undefined

# A selected alias points at a brain project root containing `db/`. BRAIN_ROOT
# remains available for tests and backwards compatibility.
export brainRoot = (cwd = process.cwd()) ->
  return process.env.BRAIN_ROOT if process.env.BRAIN_ROOT
  selected = configuredBrainRoot()
  return selected if selected
  join(cwd, 'db')

export paths = (cwd = process.cwd()) ->
  root = brainRoot(cwd)
  {
    root
    config: join(dirname(root), 'brain.yaml')   # sibling of db/: <cwd>/brain.yaml
    storage: root                               # entities + schema live directly in <cwd>/db/
    pgdata: join(root, 'pgdata')
    lock: join(root, '.lock')                   # brain server singleton guard (PID + start ts)
    sock: join(root, '.sock')                   # brain server unix socket (JSON-RPC over NDJSON)
  }

export DEFAULT_CONFIG =
  embed:
    model: 'copilot:text-embedding-3-small'
  think: {}
  search:
    reranker: 'off'   # placeholder — no cross-encoder reranker wired yet
  refine:
    maxPasses: 4       # how many iterative passes `brain refine` runs (bounds the manager-chain recursion)
  storage: []          # additional storage dirs to aggregate

export loadConfig = (cwd = process.cwd()) ->
  p = paths(cwd)
  cfg = JSON.parse(JSON.stringify(DEFAULT_CONFIG))
  if existsSync(p.config)
    raw = yaml.load(await readFile(p.config, 'utf-8')) or {}
    cfg.embed = Object.assign({}, cfg.embed, raw.embed) if raw.embed
    cfg.think = Object.assign({}, cfg.think, raw.think) if raw.think
    cfg.search = Object.assign({}, cfg.search, raw.search) if raw.search
    cfg.refine = Object.assign({}, cfg.refine, raw.refine) if raw.refine
    cfg.storage = raw.storage if Array.isArray(raw.storage)
  cfg

# Ordered, de-duplicated list of storage dirs: the local `db/` first,
# then any additional dirs from config (resolved relative to CWD unless absolute).
export storageDirs = (cwd = process.cwd(), cfg = null) ->
  cfg ?= await loadConfig(cwd)
  p = paths(cwd)
  dirs = [p.storage]
  for d in (cfg.storage or [])
    abs = if isAbsolute(d) then d else resolve(cwd, d)
    dirs.push(abs) unless abs in dirs
  dirs

export exists = (cwd = process.cwd()) -> existsSync(brainRoot(cwd))

export ensureLayout = (cwd = process.cwd()) ->
  p = paths(cwd)
  mkdirSync(p.storage, { recursive: true })
  mkdirSync(p.pgdata, { recursive: true })
  p
