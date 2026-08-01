# rm.coffee — remove one or more instances by slug (case-insensitive lookup).
# Removes from the LIVE index; the .md file goes away on `brain export --prune`.
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  throw new Error("usage: rm <slug> [<slug> ...]") unless _.length
  for raw in _
    try
      r = await request(cwd, 'delete_entity', { slug: raw })
      console.log "removed #{r.slug} (live index; `brain export --prune` also removes the .md)"
    catch err
      if /not found/.test(err.message)
        console.log "skip (not found): #{raw}"
      else
        throw err
  0
