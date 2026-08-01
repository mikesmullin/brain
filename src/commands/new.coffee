# new.coffee — A-box: instantiate a NEW entity instance of an existing class.
#   new <Class> <alias.field>=<value> | <REL>=<slug> ...
# The id is DERIVED from the class idField (e.g. EntityJournal + `BELONGS_TO=Person/jdoe`
# -> EntityJournal/jdoe). To pick the id explicitly, use `set <Class>/<id> ...`.
# Writes land in the server's live index; `brain export` materializes .md.
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'

export run = (argv, cwd = process.cwd()) ->
  { _ } = parseArgs(argv)
  cls = _[0]
  throw new Error("usage: new <Class> <alias.field>=<value> | <REL>=<slug> ...") unless cls
  throw new Error("new expects a bare <Class> (id is derived); use `set #{cls} ...` to pick an explicit id") if cls.indexOf('/') > 0
  r = await request(cwd, 'set_instance', { slug: cls, assignments: _.slice(1) })
  console.log "new #{r.slug} (live index; run `brain export` to materialize .md)"
  console.log "  warning: #{w}" for w in (r.warnings or [])
  console.log "  invalid: #{e}" for e in (r.validationErrors or [])
  0
