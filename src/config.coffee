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
import { homedir } from 'os'
import { resolve, join, isAbsolute, dirname } from 'path'
import { existsSync, mkdirSync, readFileSync } from 'fs'
import yaml from 'js-yaml'
import { readFile } from 'fs/promises'

BRAIN_CONFIG_PATH = join(homedir(), '.config', 'brain', 'brains.yaml')

configuredBrainRoot = ->
  return unless existsSync(BRAIN_CONFIG_PATH)
  try
    raw = yaml.load(readFileSync(BRAIN_CONFIG_PATH, 'utf-8')) or {}
    alias = raw.current
    return unless alias? and alias isnt 'none' and typeof raw[alias] is 'string'
    selected = String(raw[alias]).trim().replace(/^~(?=\/|$)/, homedir())
    selected = if isAbsolute(selected) then selected else resolve(dirname(BRAIN_CONFIG_PATH), selected)
    join(selected, 'db')
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
