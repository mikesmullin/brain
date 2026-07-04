# rm.coffee — remove one or more instances by slug (case-insensitive lookup).
import { loadWorld, resolveSlug } from '../world.coffee'
import { parseArgs } from '../args.coffee'
import { removeEntityFile } from '../storage.coffee'
import { dirname } from 'path'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  throw new Error("usage: rm <slug> [<slug> ...]") unless _.length
  world = await loadWorld(cwd)
  for raw in _
    e = resolveSlug(world, raw)
    unless e
      console.log "skip (not found): #{raw}"
      continue
    # remove from whichever storage dir the entity actually lives in
    storageDir = e.source.slice(0, e.source.length - "/#{e.cls}/#{e.id}.md".length)
    await removeEntityFile(storageDir, e.cls, e.id)
    console.log "removed #{e.slug}"
  0
