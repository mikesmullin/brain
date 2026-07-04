# slug.coffee — a slug is always `Class/id` (class-prefixed, no bare ids).
# This removes the V1 ambiguity where a bare `apollo` couldn't be resolved to a class.

# ProperCase class names (e.g. Person, TeamMember); ids are lowercase-ish tokens.
CLASS_RE = /^[A-Z][A-Za-z0-9]*$/
ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*$/

# Parse "Class/id" -> { cls, id, slug }. Throws on anything not class-prefixed.
export parseSlug = (s) ->
  throw new Error("slug is required") unless s
  s = String(s).trim()
  idx = s.indexOf('/')
  if idx <= 0 or idx >= s.length - 1
    throw new Error("invalid slug '#{s}': expected 'Class/id' (class prefix required)")
  cls = s.slice(0, idx)
  id = s.slice(idx + 1)
  throw new Error("invalid class '#{cls}' in slug '#{s}': expected ProperCase") unless CLASS_RE.test(cls)
  throw new Error("invalid id '#{id}' in slug '#{s}'") unless ID_RE.test(id)
  { cls, id, slug: "#{cls}/#{id}" }

export formatSlug = (cls, id) -> "#{cls}/#{id}"

export isSlug = (s) ->
  try
    parseSlug(s)
    true
  catch
    false

export { CLASS_RE, ID_RE }
