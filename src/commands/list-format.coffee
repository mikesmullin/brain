# list-format.coffee — shared streaming list UI for entity ids (brain ls,
# schema orphans, …): row-major columns, per-class color, --long slugs.
import { SGR_RESET, useColor, classColorOpen } from '../ansi-color.coffee'

INDENT = 2
GAP = 2

# stdout write with backpressure; treats EPIPE (e.g. `| head`) as a clean stop.
export writeOut = (s) ->
  new Promise (resolve, reject) ->
    try
      ok = process.stdout.write s, (err) ->
        if err?
          if err.code is 'EPIPE' then resolve(false) else reject(err)
        else
          resolve(true)
      if ok is false
        onDrain = ->
          cleanup()
          resolve(true)
        onError = (err) ->
          cleanup()
          if err.code is 'EPIPE' then resolve(false) else reject(err)
        cleanup = ->
          process.stdout.off 'drain', onDrain
          process.stdout.off 'error', onError
        process.stdout.once 'drain', onDrain
        process.stdout.once 'error', onError
    catch err
      if err.code is 'EPIPE' then resolve(false) else reject(err)

colsFor = (maxw, width) ->
  colw = maxw + GAP
  Math.max(1, Math.floor((width - INDENT) / colw))

formatRow = (ids) ->
  return '' unless ids.length
  maxw = Math.max((i.length for i in ids)...)
  colw = maxw + GAP
  parts = for id, i in ids
    if i is ids.length - 1 then id else id.padEnd(colw)
  ' '.repeat(INDENT) + parts.join('').trimEnd() + '\n'

# Row-major column packer: buffers at most one screen-row of ids.
export class RowPacker
  constructor: (@width) ->
    @buf = []

  push: (id) ->
    @buf.push(id)
    while @buf.length
      maxw = Math.max((s.length for s in @buf)...)
      cols = colsFor(maxw, @width)
      break if @buf.length < cols
      row = @buf.splice(0, cols)
      return false unless await writeOut(formatRow(row))
    true

  flush: ->
    return true unless @buf.length
    row = @buf
    @buf = []
    await writeOut(formatRow(row))

# Consume an async stream of {cls, id} items (via onItem callback registration
# pattern: await streamFn(onItem)) and print like `brain ls`.
export runGroupedIdList = (streamFn, opts = {}) ->
  long = !!opts.long
  colorOn = if opts.colorOn? then opts.colorOn else useColor()
  envCols = parseInt(process.env.COLUMNS, 10)
  width = opts.width or process.stdout.columns or (if envCols > 0 then envCols else 80)
  currentCls = null
  packer = if long then null else new RowPacker(width)
  aborted = false
  colored = false

  onStdoutErr = (err) ->
    if err.code is 'EPIPE'
      aborted = true
    else
      throw err
  process.stdout.on 'error', onStdoutErr

  abortIf = (ok) ->
    unless ok
      aborted = true
      throw new Error('aborted')

  openClass = (cls) ->
    sgr = classColorOpen(cls, colorOn)
    colored = true if sgr
    sgr

  try
    await streamFn (item) ->
      throw new Error('aborted') if aborted
      if long
        if item.cls isnt currentCls
          currentCls = item.cls
          abortIf await writeOut("#{openClass(item.cls)}#{item.cls}/#{item.id}\n")
        else
          abortIf await writeOut("#{item.cls}/#{item.id}\n")
        return

      if item.cls isnt currentCls
        if currentCls?
          abortIf await packer.flush()
          abortIf await writeOut('\n')
        currentCls = item.cls
        abortIf await writeOut("#{openClass(item.cls)}#{item.cls}/\n")
      abortIf await packer.push(item.id)

    unless long or aborted
      abortIf await packer.flush()
    if colored and not aborted
      abortIf await writeOut(SGR_RESET)
    0
  catch err
    return 0 if aborted or err?.message is 'aborted' or err?.code is 'EPIPE'
    throw err
  finally
    if colored and aborted
      try process.stdout.write(SGR_RESET) catch then undefined
    process.stdout.off 'error', onStdoutErr
