# get.coffee — print one entity's source file VERBATIM (like `cat`), optionally + incoming links.
import { readFile } from 'fs/promises'
import { loadWorld, resolveSlug } from '../world.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['links'] })
  throw new Error("usage: get <slug> [--links]") unless _[0]
  world = await loadWorld(cwd)
  e = resolveSlug(world, _[0])
  throw new Error("not found: #{_[0]}") unless e
  slug = e.slug

  # verbatim on-disk file contents (authoritative representation)
  process.stdout.write(await readFile(e.source, 'utf-8'))

  if flags.links
    incoming = []
    for other in world.entities
      for own rel, targets of (other.relations or {})
        for t in targets when t._to is slug
          incoming.push({ from: other.slug, rel })
    console.log yaml.dump({ incoming }, { lineWidth: 120, sortKeys: false, noRefs: true }) if incoming.length
  0
