# ansi-color.coffee — shared 24-bit truecolor (soft rainbow) for schema tokens.
#
# Stable hash(name) → hue on a muted HSL wheel so the same identifier
# (class, component, or relation) is the same color in `brain ls`,
# `brain schema *`, etc. Paint helpers wrap each token with SGR open + reset
# so colors don't bleed across unrelated text.

export SGR_RESET = '\x1b[0m'

fg24 = (r, g, b) -> "\x1b[38;2;#{r};#{g};#{b}m"

# Color when stdout is a TTY (or FORCE_COLOR) and NO_COLOR is unset.
export useColor = ->
  return false if process.env.NO_COLOR? and process.env.NO_COLOR isnt ''
  return true if process.env.FORCE_COLOR? and process.env.FORCE_COLOR isnt '0'
  !!(process.stdout.isTTY)

# FNV-1a 32-bit over the string, then MurmurHash3 fmix32 finalizer.
# Bare FNV on short class names clusters on the hue wheel (e.g. Address and
# Intermediary landed 7° apart); the finalizer avalanche-mixes so nearby
# strings spread across the full rainbow.
fmix32 = (h) ->
  h = h >>> 0
  h = Math.imul(h ^ (h >>> 16), 0x85ebca6b) >>> 0
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35) >>> 0
  (h ^ (h >>> 16)) >>> 0

hash32 = (s) ->
  h = 0x811c9dc5
  for i in [0...String(s).length]
    h ^= String(s).charCodeAt(i)
    h = Math.imul(h, 0x01000193) >>> 0
  fmix32(h)

# Soft ("less angry") rainbow: HSL → RGB with muted saturation and mid-high
# lightness so text stays readable on dark and light terminals.
# Hue is continuous over the full 360° wheel (not a fixed N-color palette).
hslToRgb = (h, s, l) ->
  hue2rgb = (p, q, t) ->
    t += 1 if t < 0
    t -= 1 if t > 1
    return p + (q - p) * 6 * t if t < 1 / 6
    return q if t < 1 / 2
    return p + (q - p) * (2 / 3 - t) * 6 if t < 2 / 3
    p
  if s is 0
    v = Math.round(l * 255)
    return [v, v, v]
  q = if l < 0.5 then l * (1 + s) else l + s - l * s
  p = 2 * l - q
  r = hue2rgb(p, q, h + 1 / 3)
  g = hue2rgb(p, q, h)
  b = hue2rgb(p, q, h - 1 / 3)
  [Math.round(r * 255), Math.round(g * 255), Math.round(b * 255)]

# Map any identifier → hue in [0, 1) via the full 32-bit hash (not hash % 360,
# which wastes entropy on the high bits).
tokenHue = (name) -> hash32(name) / 0x100000000

# SGR open only (for `brain ls` open-at-header / carry-through style).
export classColorOpen = (cls, colorOn = true) ->
  return '' unless colorOn and cls?
  [r, g, b] = hslToRgb(tokenHue(cls), 0.55, 0.62)
  fg24(r, g, b)

# Paint any schema token — class, component, or relation (open + text + reset).
export paintToken = (name, colorOn = useColor()) ->
  return String(name ? '') unless colorOn
  s = String(name ? '')
  return s unless s
  classColorOpen(s, true) + s + SGR_RESET

# Back-compat alias used by `brain ls` and class-specific call sites.
export paintClass = paintToken

# Color each segment of a domain/range stub: "Entity|Officer|Address".
export paintStub = (stub, colorOn = useColor()) ->
  s = String(stub ? '')
  return s unless colorOn and s
  # Don't treat mermaid arrows as stubs.
  return s if s.indexOf('-->') >= 0
  parts = s.split('|')
  return paintClass(s, colorOn) if parts.length is 1
  (paintClass(p, colorOn) for p in parts).join('|')

# "Entity" or "Entity (814,344)" — color the class name only; leave count plain.
export paintClassLabel = (label, colorOn = useColor()) ->
  s = String(label ? '')
  return s unless colorOn and s
  m = /^([A-Za-z_][\w]*)(\s*\(.*\))\s*$/.exec(s)
  return paintClass(m[1], colorOn) + m[2] if m
  # Multi-class stub with optional shared count is uncommon; color as stub.
  if s.indexOf('|') >= 0
    return paintStub(s, colorOn)
  paintClass(s, colorOn)

# Mermaid edge: "DomainStub -->|REL| RangeStub"
# Classes/stubs and the relation name all get rainbow colors (same hash scheme).
export paintMermaidEdge = (edge, colorOn = useColor()) ->
  s = String(edge ? '')
  return s unless colorOn and s
  # Note: `|` must be escaped — unescaped `|` is regex alternation.
  m = /^(.+?)( -->\|)([A-Za-z0-9_]+)(\| )(.+)$/.exec(s)
  return s unless m
  paintStub(m[1], colorOn) + m[2] + paintToken(m[3], colorOn) + m[4] + paintStub(m[5], colorOn)

# Entity slug "Class/id" — color the class prefix only.
export paintSlug = (slug, colorOn = useColor()) ->
  s = String(slug ? '')
  return s unless colorOn and s
  i = s.indexOf('/')
  return paintClass(s, colorOn) if i <= 0
  paintClass(s.slice(0, i), colorOn) + s.slice(i)
