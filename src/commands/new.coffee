# new.coffee — A-box: instantiate a NEW entity instance of an existing class.
#   new <Class> <alias.field>=<value> | <REL>=<slug> ...
# The id is DERIVED from the class idField (e.g. EntityJournal + `BELONGS_TO=Person/jdoe`
# -> EntityJournal/jdoe). To pick the id explicitly, use `set <Class>/<id> ...`.
import { loadWorld } from '../world.coffee'
import { parseArgs } from '../args.coffee'
import { setInstance } from './set.coffee'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  cls = _[0]
  throw new Error("usage: new <Class> <alias.field>=<value> | <REL>=<slug> ...") unless cls
  throw new Error("new expects a bare <Class> (id is derived); use `set #{cls} ...` to pick an explicit id") if cls.indexOf('/') > 0
  world = await loadWorld(cwd)
  r = await setInstance(world, cwd, cls, _.slice(1))
  console.log "new #{r.slug} -> #{r.path}"
  console.log "  warning: #{w}" for w in (r.warnings or [])
  0
