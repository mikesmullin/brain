# think.coffee (command) — search + LLM synthesis via the brain server.
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv)
  question = _.join(' ')
  throw new Error("usage: think [--limit N] <question>") unless question
  limit = if flags.limit then parseInt(flags.limit, 10) else 8
  res = await request(cwd, 'think', { question, limit })
  console.log yaml.dump(res, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
