# use.coffee (command) — select a named brain for the current shell.
import { homedir } from 'os'
import { dirname, isAbsolute, join, resolve } from 'path'
import { readFile, writeFile, mkdir } from 'fs/promises'
import { existsSync } from 'fs'
import yaml from 'js-yaml'

CONFIG_PATH = join(homedir(), '.config', 'brain', 'brains.yaml')

loadBrains = ->
  throw new Error("brain aliases file not found: #{CONFIG_PATH}") unless existsSync(CONFIG_PATH)
  raw = yaml.load(await readFile(CONFIG_PATH, 'utf-8')) or {}
  throw new Error("brain aliases file must contain a YAML mapping: #{CONFIG_PATH}") unless raw? and typeof raw is 'object' and not Array.isArray(raw)
  raw

saveBrains = (brains) ->
  await mkdir(dirname(CONFIG_PATH), { recursive: true })
  await writeFile(CONFIG_PATH, yaml.dump(brains, { sortKeys: false, lineWidth: 120 }), 'utf-8')

resolveBrainPath = (value) ->
  path = String(value).trim()
  path = path.replace(/^~(?=\/|$)/, homedir())
  if isAbsolute(path) then path else resolve(dirname(CONFIG_PATH), path)

export run = (argv) ->
  alias = argv[0]
  throw new Error('usage: brain use [alias]') if argv.length > 1

  if alias is 'none'
    brains = await loadBrains()
    brains.current = 'none'
    await saveBrains(brains)
    console.log 'selected brain: none'
    return 0

  brains = await loadBrains()
  unless alias?
    console.log 'Usage: brain use [alias]'
    console.log ''
    console.log 'Available brains:'
    for name, path of brains when name isnt 'current'
      selected = if brains.current is name then '  (currently selected)' else ''
      console.log "#{name}: #{resolveBrainPath(path)}#{selected}"
    console.log "none#{if brains.current is 'none' then '  (currently selected)' else ''}"
    return 0

  unless Object.prototype.hasOwnProperty.call(brains, alias)
    throw new Error("unknown brain alias '#{alias}' (run `brain use` to list available brains)")

  brains.current = alias
  await saveBrains(brains)
  console.log "selected brain: #{alias} (#{resolveBrainPath(brains[alias])})"
  0
