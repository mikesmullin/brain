# init.coffee — scaffold the cwd-local .brain/ layout.
import { paths, ensureLayout } from '../config.coffee'
import { schemaPath } from '../schema.coffee'
import { existsSync } from 'fs'
import { writeFile } from 'fs/promises'
import yaml from 'js-yaml'

STARTER_CONFIG =
  embed: { model: 'copilot:text-embedding-3-small' }
  search: { reranker: 'off' }
  storage: []

STARTER_SCHEMA =
  components: {}
  classes: {}
  relations: {}

export run = (argv, cwd = process.cwd()) ->
  p = ensureLayout(cwd)
  unless existsSync(p.config)
    await writeFile(p.config, yaml.dump(STARTER_CONFIG, { sortKeys: false }), 'utf-8')
  sp = schemaPath(p.storage)
  unless existsSync(sp)
    await writeFile(sp, yaml.dump(STARTER_SCHEMA, { sortKeys: false }), 'utf-8')
  console.log "initialized brain at #{p.root}"
  console.log "  config:  #{p.config}"
  console.log "  storage: #{p.storage}"
  console.log "  schema:  #{sp}"
  console.log "  pgdata:  #{p.pgdata} (gitignored)"
  0
