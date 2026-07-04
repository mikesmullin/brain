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
