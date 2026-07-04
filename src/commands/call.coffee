# call.coffee (command) — invoke an ECS component method on an entity.
#   brain call <slug> <method> [<params>]   params = YAML-flow (outer {} optional)
#
# The method's `content` prints to stdout; a failure's `error` prints to stderr (exit 1).
import yaml from 'js-yaml'
import { loadWorld, resolveSlug } from '../world.coffee'
import { invokeMethod } from '../components.coffee'

parseParams = (s) ->
  return {} unless s and String(s).trim()
  str = String(s).trim()
  str = "{#{str}}" unless str.startsWith('{')
  yaml.load(str) or {}

export run = (argv, cwd = process.cwd()) ->
  method = argv[1]
  throw new Error("usage: brain call <slug> <method> [<params>]") unless method
  world = await loadWorld(cwd)
  e = resolveSlug(world, argv[0])
  throw new Error("not found: #{argv[0]}") unless e
  args = parseParams(argv.slice(2).join(' '))
  r = await invokeMethod(cwd, world, e.slug, method, args)
  process.stderr.write("#{r.error}\n") if r.error and not r.success   # diagnostic -> stderr
  process.stdout.write("#{r.content}\n") if r.content?                # content -> stdout
  if r.success then 0 else 1
