# think.coffee (command) — search + LLM synthesis.
import { think } from '../think.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv)
  question = _.join(' ')
  throw new Error("usage: think [--limit N] <question>") unless question
  limit = if flags.limit then parseInt(flags.limit, 10) else 8
  res = await think(cwd, question, { limit })
  console.log yaml.dump(res, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
