# get.coffee — print one entity in its canonical .md form, from the LIVE index
# (while the server runs, pglite is authoritative; the on-disk .md may lag
# until `brain export`). Optionally list incoming links (indexed lookup).
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['links'] })
  throw new Error("usage: get <slug> [--links]") unless _[0]
  { text } = await request(cwd, 'render_entity', { slug: _[0] })
  process.stdout.write(text)
  if flags.links
    out = await request(cwd, 'get_entity', { slug: _[0], include_links: true })
    incoming = out.incoming or []
    console.log yaml.dump({ incoming }, { lineWidth: 120, sortKeys: false, noRefs: true }) if incoming.length
  0
