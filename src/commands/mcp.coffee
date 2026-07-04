# mcp.coffee (command) — start the MCP server.
#   mcp            (stdio, default)
#   mcp --stdio
import { startStdio } from '../mcp.coffee'
import { parseArgs } from '../args.coffee'

export run = (argv, cwd = process.cwd()) ->
  { flags } = parseArgs(argv, { booleans: ['stdio', 'http'] })
  if flags.http
    throw new Error("--http transport is a later-phase placeholder; use stdio for now")
  await startStdio(cwd)
  # keep the process alive; stdio transport drives lifecycle
  await new Promise(->)
  0
