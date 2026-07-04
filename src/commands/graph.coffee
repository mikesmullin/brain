# graph.coffee (command) — structural graph-match query (Mermaid syntax).
#   graph 'Team -->|USES_SYSTEM| System'
#   graph '* --> *'
import { runQuery } from '../graphmatch.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  pattern = _.join(' ')
  throw new Error("usage: graph '<Subject> -->|REL| <Object>'  (see help)") unless pattern
  matches = await runQuery(cwd, pattern)
  console.log yaml.dump({ pattern, count: matches.length, matches }, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
