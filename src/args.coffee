# args.coffee — tiny argv parser. Supports --flag, --flag value, --flag=value,
# repeatable flags (returns array when repeated), and positional args.
export parseArgs = (argv, opts = {}) ->
  booleans = new Set(opts.booleans or [])
  positionals = []
  flags = {}
  i = 0
  setFlag = (k, v) ->
    if flags[k]?
      flags[k] = [flags[k]] unless Array.isArray(flags[k])
      flags[k].push(v)
    else
      flags[k] = v
  while i < argv.length
    a = argv[i]
    if a.startsWith('--')
      body = a.slice(2)
      eq = body.indexOf('=')
      if eq >= 0
        setFlag(body.slice(0, eq), body.slice(eq + 1))
      else if booleans.has(body)
        setFlag(body, true)
      else if i + 1 < argv.length and not argv[i + 1].startsWith('--')
        setFlag(body, argv[i + 1]); i++
      else
        setFlag(body, true)
    else
      positionals.push(a)
    i++
  { _: positionals, flags }

export asArray = (v) ->
  return [] unless v?
  if Array.isArray(v) then v else [v]

# Peel a global `--use <alias>` / `--use=alias` from anywhere in argv.
# Does not persist; the dispatcher applies it via setCliUseOverride.
# `brain use` (the persist command) is a subcommand, not this flag.
export peelUseFlag = (argv) ->
  out = []
  use = undefined
  i = 0
  while i < argv.length
    a = argv[i]
    if a is '--use'
      next = argv[i + 1]
      if not next? or String(next).startsWith('-')
        throw new Error('usage: brain --use <alias> <command> ...')
      use = next
      i++
    else if a.startsWith('--use=')
      use = a.slice(6)
      throw new Error('usage: brain --use <alias> <command> ...') unless use
    else
      out.push(a)
    i++
  { argv: out, use }
