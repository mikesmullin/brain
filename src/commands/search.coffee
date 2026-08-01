# search.coffee (command) — hybrid search via the brain server; prints YAML.
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['explain', 'no-expand'] })
  query = _.join(' ')
  throw new Error("usage: search [--limit N] [--strategy hybrid|keyword|vector] [--no-expand] [--explain] <query>") unless query
  limit = if flags.limit then parseInt(flags.limit, 10) else 10
  results = await request(cwd, 'search', {
    query, limit, explain: !!flags.explain
    strategy: flags.strategy or 'hybrid'
    expand: not flags['no-expand']
  })
  console.log yaml.dump({ query, results }, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
