# use.coffee (command) — select a named brain for the current shell.
import { existsSync } from 'fs'
import {
  BRAINS_CONFIG_PATH
  loadBrainsConfig
  saveBrainsConfig
  resolveBrainProjectPath
} from '../config.coffee'

loadBrains = ->
  throw new Error("brain aliases file not found: #{BRAINS_CONFIG_PATH}") unless existsSync(BRAINS_CONFIG_PATH)
  loadBrainsConfig()

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

export run = (argv) ->
  alias = argv[0]
  throw new Error('usage: brain use [alias]') if argv.length > 1

  if alias is 'none'
    brains = loadBrains()
    brains.current = 'none'
    saveBrainsConfig(brains)
    console.log "#{okC('selected brain:')} #{noneC('none')}"
    return 0

  brains = loadBrains()
  unless alias?
    console.log "#{label('Usage:')} #{dim('brain use')} #{aliasC('[alias]')}"
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

  brains.current = alias
  saveBrainsConfig(brains)
  path = resolveBrainProjectPath(brains[alias])
  console.log "#{okC('selected brain:')} #{greenB(alias)} #{dim('(')}#{pathC(path)}#{dim(')')}"
  0

