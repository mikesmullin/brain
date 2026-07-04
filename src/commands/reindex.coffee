# reindex.coffee (command) — rebuild the pglite index from authoritative .md.
import { loadWorld } from '../world.coffee'
import { Index } from '../index.coffee'

export run = (argv, cwd = process.cwd()) ->
  world = await loadWorld(cwd)
  model = world.cfg.embed.model
  idx = new Index(cwd)
  console.log "reindexing #{world.entities.length} entities with #{model} ..."
  res = await idx.reindex(world, model)
  await idx.close()
  console.log "indexed #{res.entities} entities · embed #{res.provider}:#{res.model} · dim #{res.dim}"
  0
