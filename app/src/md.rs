//! Markdown -> HTML for note bodies.
//!
//! Hand-rolled on purpose: the client is one Rust binary with no JavaScript,
//! and the corpus of docs uses a fixed small subset of Markdown. A crate
//! engine would be more general and more moving parts (dependency tree,
//! vendoring, wasm surface area) than the notes justify.
//!
//! Supported blocks:
//!   * ATX headings, `#` through `######`,
//!   * paragraphs,
//!   * bullet lists (`-`, `*`, `+`) and numbered lists (`1.`, `1)`),
//!     including wrapped continuation lines and indentation-based nesting,
//!   * fenced code blocks, ``` ... ``` (and ~~~ ... ~~~, for future docs).
//!
//! Supported inline spans:
//!   * backtick code spans,
//!   * `**bold**`,
//!   * `*italic*`,
//!   * `[text](url)` links,
//!   * `[[Class/id]]`, `[[Class/id|display label]]`, and label-only
//!     `[[Entity Label]]` entity wikilinks, rendered as clickable cube pills.
//!
//! Everything else is HTML-escaped, so a note can contain `<placeholder>`
//! angle tokens, `&`, or stray `<` `>` without turning into markup.

/// The note body, as HTML.
///
/// A leading `# {slug}` heading - the file's own title, which `to_markdown`
/// writes and the page header already echoes (class kicker + slug) - is
/// dropped; every heading after the first is rendered.
///
/// `db` is the brain this page belongs to. It is part of every entity link's
/// href (`/alpha/e/Product/example`), because the wiki serves several databases and a
/// note's own links must stay inside the one it came from. An empty `db` gives
/// the old single-database shape (`/e/...`) and exists for tests.
pub fn render(md: &str, slug: &str, db: &str) -> String {
    let base = if db.is_empty() { String::new() } else { format!("/{db}") };
    let lines: Vec<&str> = md.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    let title = format!("# {}", slug);

    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(level) = heading_level(lines[i]) {
            if level == 1 && lines[i].trim() == title {
                i += 1; // the file's own title; the header already shows it.
                continue;
            }
            let s = lines[i].trim_start();
            out.push_str(&heading_tag(level, s[level + 1..].trim_end(), &base));
            i += 1;
        } else if let Some(fence) = fence_start(lines[i]) {
            i += 1;
            let mut body: Vec<&str> = Vec::new();
            while i < lines.len() && lines[i].trim_start() != fence {
                body.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1; // skip the closing fence.
            }
            // A fence with no closer emits the remainder; better a code block
            // than eating the rest of the note.
            out.push_str("<pre><code>");
            for line in &body {
                out.push_str(&escape(line));
                out.push('\n');
            }
            out.push_str("</code></pre>");
        } else if let Some(marker) = list_marker(lines[i]) {
            let (html, consumed) = parse_list(&lines, i, &marker, &base);
            out.push_str(&html);
            i += consumed;
        } else {
            // Paragraph: a run of lines that are not blank / heading / fence /
            // list. Soft-wrapped lines join into one paragraph.
            let mut run: Vec<String> = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.is_empty()
                    || heading_level(t).is_some()
                    || fence_start(t).is_some()
                    || list_marker(t).is_some()
                {
                    break;
                }
                run.push(inline(t, &base));
                i += 1;
            }
            if !run.is_empty() {
                out.push_str(&format!("<p>{}</p>", run.join(" ")));
            }
        }
    }
    out
}

fn heading_tag(level: usize, text: &str, base: &str) -> String {
    format!("<h{level}>{}</h{level}>", inline(text, base))
}

/// An ATX heading's level (1..=6) if the line is a heading.
fn heading_level(line: &str) -> Option<usize> {
    let s = line.trim_start();
    let n = s.chars().take_while(|c| *c == '#').count();
    if n == 0 || n > 6 {
        return None;
    }
    // Must be followed by a space (ATX rule). Byte-safe check.
    let after = &s[n..];
    match after.as_bytes().get(0) {
        Some(&b' ') | Some(&b'\t') => Some(n),
        _ => None,
    }
}

/// The closer that ends a fence opened by `line` (``` or ~~~, with or without
/// a language annotation).
fn fence_start(line: &str) -> Option<&'static str> {
    let s = line.trim_start();
    if s.starts_with("```") {
        return Some("```");
    }
    if s.starts_with("~~~") {
        return Some("~~~");
    }
    None
}

struct Marker {
    indent: usize,
    ordered: bool,
    /// Text after the marker.
    content: String,
}

/// One list item marker, or `None`.
fn list_marker(line: &str) -> Option<Marker> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    if let Some(content) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        return Some(Marker {
            indent,
            ordered: false,
            content: content.to_string(),
        });
    }
    // Numbered: one-or-more digits, then `.` or `)`, then end of line or space.
    if let Some(pos) = rest.find(|c: char| c == '.' || c == ')') {
        if pos > 0
            && rest[..pos].chars().all(|c| c.is_ascii_digit())
            && (pos + 1 == rest.len() || rest.as_bytes().get(pos + 1) == Some(&b' '))
        {
            return Some(Marker {
                indent,
                ordered: true,
                content: rest[pos + 1..].to_string(),
            });
        }
    }
    None
}

/// Parse the list block beginning at `lines[i]` (whose item marker is `root`).
/// Returns the HTML and the number of lines the block consumed.
///
/// Items are detected by marker at `root`'s indent. A marker indented deeper
/// than an item opens a nested list. Indented non-marker lines are wrapped
/// continuation of the current item. A blank line ends the list.
fn parse_list(lines: &[&str], i: usize, root: &Marker, base: &str) -> (String, usize) {
    let mut out = String::new();
    out.push_str(if root.ordered { "<ol>" } else { "<ul>" });

    let mut next = i;
    let mut item: Vec<String> = Vec::new();

    macro_rules! emit_item {
        () => {
            if !item.is_empty() {
                out.push_str("<li>");
                out.push_str(&item.join(" "));
                out.push_str("</li>");
                item.clear();
            }
        };
    }

    while next < lines.len() {
        let line = lines[next];
        if line.trim().is_empty() {
            emit_item!();
            break;
        }
        if let Some(m) = list_marker(line) {
            if m.indent == root.indent {
                // New item: flush the previous one, start this one.
                emit_item!();
                item.push(inline(&m.content, base));
                next += 1;
                continue;
            } else if m.indent > root.indent {
                // Nested list under the current item.
                emit_item!();
                let (nested, consumed) = parse_list(lines, next, &m, base);
                out.push_str(&nested);
                item.clear();
                next += consumed;
                continue;
            }
        }
        // Continuation of the current item (wrapped line).
        if !item.is_empty() {
            item.push(inline(line.trim_start(), base));
            next += 1;
            continue;
        }
        // A markerless line with no current item: ends the list.
        break;
    }
    emit_item!();

    out.push_str(if root.ordered { "</ol>" } else { "</ul>" });
    (out, next - i)
}

/// HTML-escape; the only place literal text becomes markup-safe.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Inline spans: code, wiki links, markdown links, bold, italic, escape.
/// Code spans win: their contents are escaped and immune to everything else.
fn inline(s: &str, base: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    loop {
        if i >= s.len() {
            break;
        }
        match s[i..].find('`') {
            Some(k) => {
                let start = i + k;
                match s[start + 1..].find('`') {
                    Some(end) => {
                        out.push_str(&emph(&s[i..start], &base));
                        out.push_str(&format!(
                            "<code>{}</code>",
                            escape(&s[start + 1..start + 1 + end])
                        ));
                        i = start + 2 + end;
                    }
                    None => {
                        // Unmatched backtick: emit the rest, with it literal.
                        out.push_str(&emph(&s[i..start], &base));
                        out.push_str(&escape(&s[start..]));
                        break;
                    }
                }
            }
            None => {
                out.push_str(&emph(&s[i..], &base));
                break;
            }
        }
    }
    out
}

/// Emphasis, links, and escape on text with no code spans.
/// Emits text up to (and including) each token, in order - so no text before
/// a later token (e.g. a wiki link) is ever dropped.
/// Strip the small amount of Markdown punctuation LLMs commonly leak into
/// wikilink tokens. The result is escaped later, when it enters HTML.
fn strip_markup(s: &str) -> String {
    s.replace("**", "")
        .replace("__", "")
        .replace('`', "")
        .trim()
        .to_string()
}

/// Keep a wikilink navigable and safe as an internal `/e/...` path.
fn sanitize_slug(raw: &str) -> Option<String> {
    let raw = strip_markup(raw);
    if raw.is_empty() || raw.contains('#') {
        return None;
    }
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/') {
            if c == '/' && out.ends_with('/') {
                continue;
            }
            out.push(c);
        } else if !out.is_empty() && !out.ends_with(['-', '/']) {
            out.push('-');
        }
    }
    let slug = out.trim_matches(|c| c == '-' || c == '/').to_string();
    (!slug.is_empty()).then_some(slug)
}

/// `[[Class/id]]`, `[[Class/id|display label]]`, or `[[Entity Label]]` as
/// a navigable entity pill. Label-only targets are resolved by the SPA when
/// clicked; canonical Class/id targets can navigate immediately.
fn wikilink_pill(raw: &str, base: &str) -> Option<String> {
    let (raw_target, raw_label) = raw.split_once('|').unwrap_or((raw, ""));
    let target = strip_markup(raw_target);
    if target.is_empty() || target.contains('#') {
        return None;
    }
    let explicit = strip_markup(raw_label);
    if target.contains('/') {
        let slug = sanitize_slug(&target)?;
        let label = if explicit.is_empty() {
            slug.rsplit('/').next().unwrap_or(&slug).to_string()
        } else {
            explicit
        };
        let escaped_slug = escape(&slug);
        return Some(format!(
            r#"<a class="entity-link entity-pill entity-badge" href="{base}/e/{escaped_slug}" data-entity="{escaped_slug}" title="{escaped_slug}"><i class="ph ph-cube entity-pill-icon" aria-hidden="true"></i><span class="entity-pill-label">{}</span></a>"#,
            escape(&label)
        ));
    }
    let label = if explicit.is_empty() {
        target.clone()
    } else {
        explicit
    };
    let escaped_target = escape(&target);
    Some(format!(
        r#"<a class="entity-link entity-pill entity-badge" href="{base}/e/{escaped_target}" data-entity="{escaped_target}" data-entity-label="true" title="{escaped_target}"><i class="ph ph-cube entity-pill-icon" aria-hidden="true"></i><span class="entity-pill-label">{}</span></a>"#,
        escape(&label)
    ))
}

/// One line of note prose as HTML — a search snippet.
///
/// A snippet is a fragment cut out of a note, so it is rendered with the same
/// inline grammar the note itself uses: `[[Class/id|Label]]` becomes an entity
/// badge, `**bold**` and `` `code` `` render, everything else is escaped. No
/// block structure, though: headings, lists and fences in a fragment are
/// truncation artifacts, and rendering them would put a half-list inside a
/// paragraph.
pub fn snippet(text: &str, db: &str) -> String {
    let base = if db.is_empty() { String::new() } else { format!("/{db}") };
    inline(trim_dangling_wikilink(text), &base)
}

/// A snippet is cut at a character budget, so the last `[[Class/id|Label` can be
/// missing its `]]`. Rendering that as prose shows the syntax instead of the
/// entity, and no badge can be built from half a slug — so the fragment ends
/// before it. Only the *last* opener is considered: earlier ones are closed or
/// are genuine literal text.
fn trim_dangling_wikilink(text: &str) -> &str {
    match text.rfind("[[") {
        Some(open) if !text[open..].contains("]]") => &text[..open],
        _ => text,
    }
}

/// Emphasis, links, wikilinks, and escape on text with no code spans.
/// Emits text up to (and including) each token, in order.
fn emph(s: &str, base: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    loop {
        if i >= s.len() {
            break;
        }
        let rest = &s[i..];
        // Earliest token start in the remaining text.
        let mut best = None;
        for ch in ['[', '*'] {
            if let Some(p) = rest.find(ch) {
                best = Some(p.min(best.unwrap_or(p)));
            }
        }
        match best {
            None => {
                out.push_str(&escape(rest));
                break;
            }
            Some(p) => {
                // Preserve ordinary prose before the token. This matters when
                // a paragraph contains several spans, e.g. "see [[Product/example]]".
                if p > 0 {
                    out.push_str(&escape(&rest[..p]));
                }
                let b = rest.as_bytes()[p];
                if b == b'[' {
                    if rest[p..].starts_with("[[") {
                        if let Some(close) = rest[p + 2..].find("]]") {
                            let token = &rest[p + 2..p + 2 + close];
                            if let Some(pill) = wikilink_pill(token, base) {
                                out.push_str(&pill);
                                i += p + 2 + close + 2;
                                continue;
                            }
                        }
                    }
                    if let Some(rb) = rest[p + 1..].find(']') {
                        let rb = p + 1 + rb;
                        if rb + 1 < rest.len() && rest.as_bytes()[rb + 1] == b'(' {
                            if let Some(rp) = rest[rb + 1..].find(')') {
                                let text = emph(&rest[p + 1..rb], base);
                                let raw_url = &rest[rb + 2..rb + 1 + rp];
                                // A root-relative entity link inside a note
                                // belongs to this note's brain, not to whichever
                                // one the reader switched to.
                                let prefixed;
                                let url = escape(if raw_url.starts_with("/e/") {
                                    prefixed = format!("{base}{raw_url}");
                                    prefixed.as_str()
                                } else {
                                    raw_url
                                });
                                out.push_str(&format!("<a href=\"{url}\">{text}</a>"));
                                i += rb + 1 + rp + 1;
                                continue;
                            }
                        }
                    }
                    // Not a link: literal [
                    out.push('[');
                    i += p + 1;
                } else {
                    if rest[p..].starts_with("**") {
                        if let Some(end) = rest[p + 2..].find("**") {
                            let close = p + 2 + end;
                            out.push_str(&format!(
                                "<strong>{}</strong>",
                                emph(&rest[p + 2..close], base)
                            ));
                            i += close + 2;
                            continue;
                        }
                    }
                    if p + 1 < rest.len() && rest.as_bytes()[p + 1] != b'*' {
                        if let Some(end) = rest[p + 1..].find('*') {
                            let close = p + 1 + end;
                            if close + 1 >= rest.len() || rest.as_bytes()[close + 1] != b'*' {
                                out.push_str(&format!("<em>{}</em>", emph(&rest[p + 1..close], base)));
                                i += close + 1;
                                continue;
                            }
                        }
                    }
                    // Unpaired *
                    out.push('*');
                    i += p + 1;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_leading_slug_title() {
        let src = "# Item/mandrake\n\nMandrake is a vegetable.\n\n## Cooking\n\nRoast it.\n";
        let html = render(src, "Item/mandrake", "dst");
        assert!(!html.starts_with("<h1>"), "{html}");
        assert!(html.contains("<h2>Cooking</h2>"), "{html}");
        assert!(html.contains("<p>Mandrake is a vegetable."), "{html}");
    }

    #[test]
    fn renders_square_wikilinks_as_entity_pills() {
        let src = "See [[Item/mandrake]] and [[Food/mandrake_soup|Mandrake Soup]].";
        let html = render(src, "Item/mandrake", "dst");
        assert!(
            html.contains("class=\"entity-link entity-pill entity-badge\""),
            "{html}"
        );
        assert!(html.contains("ph ph-cube entity-pill-icon"), "{html}");
        assert!(html.contains("href=\"/dst/e/Item/mandrake\""), "{html}");
        assert!(
            html.contains("<span class=\"entity-pill-label\">mandrake</span>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"entity-pill-label\">Mandrake Soup</span>"),
            "{html}"
        );
        assert!(html.contains("See "), "{html}");
    }

    #[test]
    fn snippet_renders_wikilinks_as_badges_and_escapes_the_rest() {
        let src = "Mandrake Soup via [[Item/mandrake|Mandrake]] and [[Food/mandrake_soup]] <b>&";
        let html = snippet(src, "dst");
        assert!(
            html.contains("href=\"/dst/e/Item/mandrake\""),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"entity-pill-label\">Mandrake</span>"),
            "{html}"
        );
        assert!(html.contains("entity-badge"), "{html}");
        assert!(html.contains("&lt;b&gt;&amp;"), "{html}");
        // A snippet is inline: no paragraph wrapper, no block elements.
        assert!(!html.contains("<p>"), "{html}");
    }

    #[test]
    fn snippet_drops_a_wikilink_cut_mid_token() {
        let src = "naming.name: Mandrake Soup # Food/mandrake_soup [[Item/mandr";
        let html = snippet(src, "dst");
        assert!(!html.contains("[["), "{html}");
        assert!(html.contains("Mandrake Soup # Food/mandrake_soup"), "{html}");
        assert!(!html.contains("Item/mandr&lt;"), "{html}");
    }

    #[test]
    fn renders_headings_lists_code_bold_italic_links() {
        let src = "## Head\n\n`code` span and **bold** and *ital* and [ext](https://x) [[slug]] and <tag> angle\n- bullet one\n- bullet two with **bold**\n\n1. ordered\n2. two\n\n`fence` end\n```fence1```\nblock code <tag> line\n```fence1```\n";
        let html = render(src, "unrelated", "");
        assert!(html.contains("<h2>Head</h2>"), "{html}");
        assert!(html.contains("<code>code</code>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("<em>ital</em>"), "{html}");
        assert!(html.contains("&lt;tag&gt;"), "{html}");
        assert!(html.contains("<ul>"), "{html}");
        assert!(html.contains("<ol>"), "{html}");
        assert!(html.contains("https://x"), "{html}");
        assert!(html.contains("/e/slug"), "{html}");
        assert!(html.contains("<pre><code>"), "{html}");
    }
}
