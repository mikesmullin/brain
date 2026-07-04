#!/usr/bin/env bun
// brain — CLI entry.
// Preloads the CoffeeScript loader so the .coffee sources run under Bun, then
// hands off to the dispatcher in src/cli.coffee.
import 'bun-coffeescript/register'

const { main } = await import('../src/cli.coffee')
await main(process.argv.slice(2))
