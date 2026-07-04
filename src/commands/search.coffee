# search.coffee (command) — hybrid search; prints YAML.
import { hybridSearch } from '../search.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['explain'] })
  query = _.join(' ')
  throw new Error("usage: search [--limit N] [--explain] <query>") unless query
  limit = if flags.limit then parseInt(flags.limit, 10) else 10
  results = await hybridSearch(cwd, query, { limit, explain: !!flags.explain })
  console.log yaml.dump({ query, results }, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
