# use.coffee (command) — select a named brain for the current shell.
import { existsSync } from 'fs'
import {
  BRAINS_CONFIG_PATH
  loadBrainsConfig
  saveBrainsConfig
  resolveBrainProjectPath
  registerBrainProject
} from '../config.coffee'
import { parseArgs } from '../args.coffee'

loadBrains = ->
  throw new Error("brain aliases file not found: #{BRAINS_CONFIG_PATH}") unless existsSync(BRAINS_CONFIG_PATH)
  loadBrainsConfig()

USAGE = 'usage: brain use [alias|.] | brain use --rm <alias>'

# 24-bit ANSI truecolor helpers (ESC[38;2;R;G;Bm … ESC[0m)
fg = (r, g, b, s) -> "\x1b[38;2;#{r};#{g};#{b}m#{s}\x1b[0m"
# bold + color in one code (so bold doesn't reset the green)
fgBold = (r, g, b, s) -> "\x1b[1;38;2;#{r};#{g};#{b}m#{s}\x1b[0m"
dim     = (s) -> fg(120, 120, 130, s)       # muted chrome
label   = (s) -> fg(160, 170, 190, s)       # section headers
aliasC  = (s) -> fg(130, 180, 255, s)       # brain alias (cool blue)
pathC   = (s) -> fg(180, 185, 195, s)       # filesystem path
noneC   = (s) -> fg(150, 150, 160, s)       # "none" sentinel
green   = (s) -> fg(126, 226, 168, s)       # selected mark / success
greenB  = (s) -> fgBold(126, 226, 168, s)   # selected alias (bold green)
okC     = green

SELECTED_MARK = '  ' + green('(currently selected)')

selectAlias = (brains, alias) ->
  brains.current = alias
  saveBrainsConfig(brains)
  path = resolveBrainProjectPath(brains[alias])
  console.log "#{okC('selected brain:')} #{greenB(alias)} #{dim('(')}#{pathC(path)}#{dim(')')}"
  0

# Remove a memorized alias from brains.yaml. If it was selected, fall back to none.
removeAlias = (name) ->
  throw new Error(USAGE) unless name? and name isnt true and String(name).length > 0
  name = String(name)
  throw new Error("cannot remove reserved name '#{name}'") if name in ['current', 'none']

  brains = loadBrains()
  unless Object.prototype.hasOwnProperty.call(brains, name) and name isnt 'current'
    throw new Error("unknown brain alias '#{name}' (run `brain use` to list available brains)")
  unless typeof brains[name] is 'string'
    throw new Error("unknown brain alias '#{name}' (run `brain use` to list available brains)")

  path = (() ->
    try resolveBrainProjectPath(brains[name])
    catch then brains[name]
  )()
  wasCurrent = brains.current is name
  delete brains[name]
  brains.current = 'none' if wasCurrent
  saveBrainsConfig(brains)
  console.log "#{okC('removed brain:')} #{aliasC(name)} #{dim('(')}#{pathC(path)}#{dim(')')}"
  if wasCurrent
    console.log "#{dim('current is now')} #{noneC('none')}"
  0

export run = (argv) ->
  { _: positionals, flags } = parseArgs(argv)

  if flags.rm?
    # brain use --rm <alias>  |  brain use --rm=<alias>
    name = if flags.rm is true then positionals[0] else flags.rm
    throw new Error(USAGE) if flags.rm is true and positionals.length isnt 1
    throw new Error(USAGE) if flags.rm isnt true and positionals.length isnt 0
    return removeAlias(name)

  throw new Error(USAGE) if Object.keys(flags).length > 0
  throw new Error(USAGE) if positionals.length > 1

  alias = positionals[0]

  if alias is 'none'
    brains = loadBrains()
    brains.current = 'none'
    saveBrainsConfig(brains)
    console.log "#{okC('selected brain:')} #{noneC('none')}"
    return 0

  # `brain use .` — register cwd (basename = alias) and select it.
  # Overwrites any existing alias→path mapping for that basename.
  if alias is '.'
    { alias: name, brains, created, updated } = registerBrainProject(process.cwd(), { overwrite: true })
    path = resolveBrainProjectPath(brains[name])
    if created
      console.log "#{dim('registered')} #{aliasC(name)} #{dim('->')} #{pathC(path)}"
    else if updated
      console.log "#{dim('updated')} #{aliasC(name)} #{dim('->')} #{pathC(path)}"
    return selectAlias(brains, name)

  brains = loadBrains()
  unless alias?
    # Usage lines: pad on the visible (uncolored) left side so `#` comments align.
    usageLine = (plain, colored, comment) ->
      pad = Math.max(1, 34 - plain.length)
      console.log "  #{colored}#{dim("#{' '.repeat(pad)}# #{comment}")}"
    console.log label('Usage:')
    usageLine 'brain use',              dim('brain use'),                        'list available brains'
    usageLine 'brain use <alias>',      "#{dim('brain use')} #{aliasC('<alias>')}", 'select a memorized brain'
    usageLine 'brain use .',            "#{dim('brain use')} #{aliasC('.')}",      'register + select cwd (basename = alias)'
    usageLine 'brain use none',         "#{dim('brain use')} #{noneC('none')}",    'use the cwd-local db/'
    usageLine 'brain use --rm <alias>', "#{dim('brain use --rm')} #{aliasC('<alias>')}", 'forget a memorized alias'
    console.log ''
    console.log label('Available brains:')
    for name, path of brains when name isnt 'current'
      isSel = brains.current is name
      nameStr = if isSel then greenB(name) else aliasC(name)
      pathStr = pathC(resolveBrainProjectPath(path))
      sel = if isSel then SELECTED_MARK else ''
      console.log "  #{nameStr}#{dim(':')} #{pathStr}#{sel}"
    noneStr = if brains.current is 'none' then greenB('none') else noneC('none')
    noneMark = if brains.current is 'none' then SELECTED_MARK else ''
    console.log "  #{noneStr}#{noneMark}"
    return 0

  unless Object.prototype.hasOwnProperty.call(brains, alias)
    throw new Error("unknown brain alias '#{alias}' (run `brain use` to list available brains)")

  selectAlias(brains, alias)

