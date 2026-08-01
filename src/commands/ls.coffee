# ls.coffee (command) — list A-box instances of a class, ls-style (via server).
#   brain ls <Class>     entity ids (basenames) of a class, in columns, under a "Class/" header
#   brain ls             all classes, grouped
#   brain ls [...] --long   one full slug per line instead of columns
import { request } from '../client.coffee'
import { parseArgs } from '../args.coffee'

# ls-style column-major layout (fill down each column), left-indented.
columnize = (items, indent, width) ->
  return '' unless items.length
  gap = 2
  maxw = Math.max((i.length for i in items)...)
  colw = maxw + gap
  cols = Math.max(1, Math.floor((width - indent) / colw))
  rows = Math.ceil(items.length / cols)
  lines = []
  for r in [0...rows]
    parts = []
    for c in [0...cols]
      idx = c * rows + r
      break if idx >= items.length
      last = (idx + rows) >= items.length
      parts.push(if last then items[idx] else items[idx].padEnd(colw))
    lines.push(' '.repeat(indent) + parts.join('').trimEnd())
  lines.join('\n')

export run = (argv, cwd = process.cwd()) ->
  { _, flags } = parseArgs(argv, { booleans: ['long'] })
  byClass = await request(cwd, 'ls', { class: _[0] or null })
  classes = Object.keys(byClass).sort()
  width = process.stdout.columns or 80
  first = true
  for cls in classes
    ids = (byClass[cls] or []).slice().sort()
    console.log '' unless first
    first = false
    if flags.long
      console.log "#{cls}/#{id}" for id in ids
    else
      console.log "#{cls}/"
      out = columnize(ids, 2, width)
      console.log out if out
  0
