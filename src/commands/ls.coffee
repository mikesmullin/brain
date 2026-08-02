# ls.coffee (command) — list A-box instances of a class, streaming entity-at-a-time.
#   brain ls <Class>     entity ids in columns under a "Class/" header
#   brain ls             all classes, grouped
#   brain ls [...] --long   one full slug per line
import { requestStream } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import { runGroupedIdList } from './list-format.coffee'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['long'] })
  await runGroupedIdList(
    ((onItem) -> requestStream(cwd, 'ls', { class: _[0] or null }, onItem))
    { long: !!flags.long }
  )
