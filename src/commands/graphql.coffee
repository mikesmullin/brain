# graphql.coffee (command) — deterministic GraphQL-ish traversal.
#   graphql 'Team/team-cloud { naming, USES_SYSTEM { info } }'
#   graphql < query.graphql            # read query from stdin (also when arg is '-')
import { runGraphql } from '../graphqlish.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

readStdin = ->
  new Promise (resolve, reject) ->
    chunks = []
    process.stdin.on 'data', (c) -> chunks.push c
    process.stdin.on 'end', -> resolve Buffer.concat(chunks).toString('utf8')
    process.stdin.on 'error', reject

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  query = _.join(' ').trim()
  # No query arg (or explicit '-') => read the query from stdin (handy with heredocs / pipes).
  query = (await readStdin()).trim() if not query or query is '-'
  throw new Error("usage: graphql '<Class/id> { field, REL { ... } }'  (or pipe/heredoc the query via stdin)") unless query
  result = await runGraphql(cwd, query)
  console.log yaml.dump(result, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
