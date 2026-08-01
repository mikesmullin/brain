# ontology.coffee (command) — LLM typed-graph traversal via the brain server.
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'
import yaml from 'js-yaml'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  question = _.join(' ')
  throw new Error("usage: ontology <question>") unless question
  res = await request(cwd, 'ontology', { question })
  console.log yaml.dump(res, { lineWidth: 120, sortKeys: false, noRefs: true })
  0
