# graph.coffee (command) — structural graph-match query (Mermaid syntax).
#   graph 'Team -->|USES_SYSTEM| System'
#   graph 'Person/jdoe *6> Person/asmith --shortest'
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['shortest'] })
  pattern = _.join(' ')
  throw new Error("usage: graph '<Subject> -->|REL| <Object>'  (see help)") unless pattern
  res = await request(cwd, 'graph', {
    pattern
    shortest: !!flags.shortest
    max_nodes: if flags['max-nodes'] then parseInt(flags['max-nodes'], 10) else undefined
  })
  console.log yaml.dump({ pattern, count: res.matches.length, capped: res.capped, matches: res.matches }, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
