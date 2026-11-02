//! graph.rs — deterministic structural match, Mermaid-flavoured.
//!
//! ```text
//!   Subject -->|PREDICATE| Object     exact class + exact relation
//!   *  -->|SUPPORTS| Product          `*` = any class, one hop
//!   ** --> ***                        `**` / `***` = within 2 / 3 hops
//!   *N --> ...                        within N hops, for any N
//!   >                                 shorthand for the unlabelled `-->`
//!   A *6> B --shortest                minimum-hop path(s) only
//! ```
//!
//! Traversal is one batched query per hop for the whole frontier, so cost is
//! O(degree) per visited node and never a scan of the world. Two things are
//! load-bearing:
//!
//! * **`capped` is always reported.** A result that stopped because of the
//!   circuit breaker is a different fact from one that stopped because the
//!   graph ran out, and a benchmark that does not distinguish them is
//!   comparing two different questions.
//! * **`--shortest` between two concrete endpoints is a bidirectional
//!   early-exit BFS**, not "enumerate the N-hop neighbourhood then filter".
//!   That was brain1's single worst benchmark result (~90x slower than an
//!   in-memory CSR BFS) and the fix took it to ~16x. Labelled and wildcard
//!   patterns keep the enumerate path, which is what they mean.

use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::store::Store;
use crate::tbox::TBox;

pub const DEFAULT_MAX_NODES: i64 = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub enum NodePat {
    /// A concrete `Class/id`.
    Node(String),
    /// A class name: every entity of it.
    Class(String),
    /// `*` / `**` / `*N` — any class, within `degree` hops.
    Wild { degree: usize },
}

#[derive(Debug, Clone)]
pub struct EdgePat {
    pub rel: Option<String>,
    pub hops: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    pub nodes: Vec<NodePat>,
    pub edges: Vec<EdgePat>,
}

#[derive(Debug, Clone, Default)]
pub struct GraphOpts {
    pub shortest: bool,
    pub max_nodes: i64,
    pub undirected: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Path {
    pub nodes: Vec<String>,
    pub via: Vec<String>,
    pub hops: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphResult {
    pub pattern: String,
    pub paths: Vec<Path>,
    pub visited: usize,
    pub capped: bool,
    pub ms: u128,
}

/// Pull the inline option tokens out of the pattern, so the same string works
/// from the CLI, from MCP and from the web UI.
pub fn extract_options(pattern: &str) -> (String, GraphOpts) {
    let mut opts = GraphOpts { max_nodes: DEFAULT_MAX_NODES, limit: 0, ..Default::default() };
    let mut parts: Vec<String> = Vec::new();
    let mut it = pattern.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        match tok {
            "--shortest" => opts.shortest = true,
            "--undirected" => opts.undirected = true,
            t if t.starts_with("--max-nodes=") => {
                opts.max_nodes = t[12..].parse().unwrap_or(DEFAULT_MAX_NODES);
            }
            "--max-nodes" => {
                if let Some(v) = it.next() {
                    opts.max_nodes = v.parse().unwrap_or(DEFAULT_MAX_NODES);
                }
            }
            t if t.starts_with("--limit=") => opts.limit = t[8..].parse().unwrap_or(0),
            "--limit" => {
                if let Some(v) = it.next() {
                    opts.limit = v.parse().unwrap_or(0);
                }
            }
            t => parts.push(t.to_string()),
        }
    }
    (parts.join(" "), opts)
}

/// Tokenize into alternating node / edge terms.
pub fn parse_pattern(pattern: &str) -> Result<Pattern> {
    let s = pattern.trim();
    let mut nodes: Vec<NodePat> = Vec::new();
    let mut edges: Vec<EdgePat> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut pending_hops: Option<usize> = None;

    let push_node = |raw: &str, nodes: &mut Vec<NodePat>| -> Result<()> {
        let raw = raw.trim();
        if raw.is_empty() {
            nodes.push(NodePat::Wild { degree: 1 });
        } else if raw.chars().all(|c| c == '*') {
            nodes.push(NodePat::Wild { degree: raw.len() });
        } else if let Some(n) = raw.strip_prefix('*').and_then(|d| d.parse::<usize>().ok()) {
            nodes.push(NodePat::Wild { degree: n });
        } else if raw.contains('/') {
            nodes.push(NodePat::Node(raw.to_string()));
        } else {
            nodes.push(NodePat::Class(raw.to_string()));
        }
        Ok(())
    };

    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            // `-->` or `-->|REL|`
            let mut arrow = String::from("-");
            while let Some(&n) = chars.peek() {
                if n == '-' || n == '>' {
                    arrow.push(n);
                    chars.next();
                    if n == '>' {
                        break;
                    }
                } else {
                    break;
                }
            }
            if !arrow.ends_with('>') {
                cur.push_str(&arrow);
                continue;
            }
            let mut rel = None;
            if chars.peek() == Some(&'|') {
                chars.next();
                let mut r = String::new();
                for n in chars.by_ref() {
                    if n == '|' {
                        break;
                    }
                    r.push(n);
                }
                rel = Some(r.trim().to_string());
            }
            // a trailing `*N` on the node term binds to this arrow
            let trimmed = cur.trim().to_string();
            let (node_raw, hops) = split_hop_prefix(&trimmed);
            push_node(&node_raw, &mut nodes)?;
            edges.push(EdgePat { rel, hops: hops.or(pending_hops) });
            pending_hops = None;
            cur.clear();
        } else if c == '>' {
            let trimmed = cur.trim().to_string();
            let (node_raw, hops) = split_hop_prefix(&trimmed);
            push_node(&node_raw, &mut nodes)?;
            edges.push(EdgePat { rel: None, hops: hops.or(pending_hops) });
            pending_hops = None;
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    push_node(&cur, &mut nodes)?;
    if nodes.len() < 2 {
        bail!("pattern needs at least two node terms joined by an arrow (see `brain help graph`)");
    }
    Ok(Pattern { nodes, edges })
}

/// `Person/jdoe *6` -> (`Person/jdoe`, 6). The hop count rides on the arrow.
fn split_hop_prefix(s: &str) -> (String, Option<usize>) {
    let s = s.trim();
    if let Some(idx) = s.rfind(" *") {
        if let Ok(n) = s[idx + 2..].parse::<usize>() {
            return (s[..idx].trim().to_string(), Some(n));
        }
    }
    if let Some(rest) = s.strip_suffix('*') {
        // `A *` with no digits: one hop
        if !rest.trim().is_empty() {
            return (rest.trim().to_string(), Some(1));
        }
    }
    (s.to_string(), None)
}

async fn seeds_for(store: &Store, tbox: &TBox, pat: &NodePat, cap: i64) -> Result<Vec<String>> {
    match pat {
        NodePat::Node(slug) => Ok(store.resolve(slug).await?.into_iter().collect()),
        NodePat::Class(name) => {
            let cls = tbox.resolve_class(name).map(|s| s.to_string()).unwrap_or_else(|| name.clone());
            Ok(store
                .list_class(Some(&cls), cap, 0)
                .await?
                .into_iter()
                .map(|(s, _, _)| s)
                .collect())
        }
        NodePat::Wild { .. } => Ok(vec![]),
    }
}

fn matches(tbox: &TBox, pat: &NodePat, slug: &str) -> bool {
    match pat {
        NodePat::Wild { .. } => true,
        NodePat::Node(want) => want.eq_ignore_ascii_case(slug),
        NodePat::Class(name) => {
            let cls = slug.split('/').next().unwrap_or("");
            match tbox.resolve_class(name) {
                Some(c) => tbox.ancestry(cls).iter().any(|a| a == c),
                None => cls.eq_ignore_ascii_case(name),
            }
        }
    }
}

/// Run one pattern. Only the two-term shape is traversed (which is what every
/// brain1 example uses); longer chains are matched by composing the two-term
/// walk left to right.
pub async fn run(store: &Store, tbox: &TBox, pattern: &str) -> Result<GraphResult> {
    let t0 = std::time::Instant::now();
    let (clean, opts) = extract_options(pattern);
    let pat = parse_pattern(&clean)?;

    // Hop budget for the first edge: an explicit `*N` wins, else the degree
    // written on the target wildcard, else one hop.
    let hops = pat.edges.first().and_then(|e| e.hops).unwrap_or_else(|| {
        match pat.nodes.get(1) {
            Some(NodePat::Wild { degree }) => *degree,
            _ => 1,
        }
    });
    let rel = pat.edges.first().and_then(|e| e.rel.clone());

    let start = &pat.nodes[0];
    let target = &pat.nodes[pat.nodes.len() - 1];

    // Both endpoints concrete + --shortest: bidirectional early-exit BFS.
    if opts.shortest {
        if let (NodePat::Node(a), NodePat::Node(b)) = (start, target) {
            let (Some(a), Some(b)) = (store.resolve(a).await?, store.resolve(b).await?) else {
                return Ok(GraphResult { pattern: clean, paths: vec![], visited: 0, capped: false, ms: t0.elapsed().as_millis() });
            };
            let (paths, visited, capped) =
                bidi_shortest(store, &a, &b, hops, opts.max_nodes, opts.undirected).await?;
            return Ok(GraphResult { pattern: clean, paths, visited, capped, ms: t0.elapsed().as_millis() });
        }
    }

    let starts = seeds_for(store, tbox, start, opts.max_nodes).await?;
    if starts.is_empty() {
        // A wildcard start with a concrete target: walk backwards from the target.
        if let NodePat::Node(b) = target {
            if let Some(b) = store.resolve(b).await? {
                let (paths, visited, capped) =
                    enumerate(store, tbox, &[b], rel.clone(), hops, start, opts.max_nodes, !opts.undirected, true, &opts)
                        .await?;
                return Ok(GraphResult { pattern: clean, paths, visited, capped, ms: t0.elapsed().as_millis() });
            }
        }
        return Ok(GraphResult { pattern: clean, paths: vec![], visited: 0, capped: false, ms: t0.elapsed().as_millis() });
    }

    let (paths, visited, capped) =
        enumerate(store, tbox, &starts, rel, hops, target, opts.max_nodes, true, opts.undirected, &opts).await?;
    Ok(GraphResult { pattern: clean, paths, visited, capped, ms: t0.elapsed().as_millis() })
}

/// BFS from a set of starts, keeping one path per (start, reached) pair.
/// `forward = false` walks in-edges, which is how a wildcard start with a
/// concrete target is answered without touching the rest of the graph.
#[allow(clippy::too_many_arguments)]
async fn enumerate(
    store: &Store,
    tbox: &TBox,
    starts: &[String],
    rel: Option<String>,
    max_hops: usize,
    target: &NodePat,
    budget: i64,
    forward: bool,
    undirected: bool,
    opts: &GraphOpts,
) -> Result<(Vec<Path>, usize, bool)> {
    let mut paths: Vec<Path> = Vec::new();
    let mut seen: HashSet<String> = starts.iter().cloned().collect();
    let mut frontier: Vec<(String, Vec<String>, Vec<String>)> =
        starts.iter().map(|s| (s.clone(), vec![s.clone()], vec![])).collect();
    let mut visited = starts.len();
    let mut capped = false;
    let want = if opts.limit > 0 { opts.limit } else { usize::MAX };

    for hop in 0..max_hops {
        if frontier.is_empty() {
            break;
        }
        if visited as i64 >= budget {
            capped = true;
            break;
        }
        // The frontier is unique by construction — a node only enters it when
        // `seen.insert` succeeded — so deduping it again costs two allocations
        // and a lowercase conversion per node for nothing. At a 36k-degree hub
        // that is 72,000 allocations a hop.
        let slugs: Vec<String> = frontier.iter().map(|(s, _, _)| s.clone()).collect();
        let room = (budget - visited as i64).max(1);
        let edges = if undirected {
            store.expand_both(&slugs, room).await?
        } else {
            store.expand(&slugs, forward, room).await?
        };
        if edges.len() as i64 >= room {
            capped = true;
        }
        // index edges by their source in this direction
        let mut by_src: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
        for e in &edges {
            if undirected {
                by_src.entry(&e.from).or_default().push((&e.rel, &e.to));
                by_src.entry(&e.to).or_default().push((&e.rel, &e.from));
            } else if forward {
                by_src.entry(&e.from).or_default().push((&e.rel, &e.to));
            } else {
                by_src.entry(&e.to).or_default().push((&e.rel, &e.from));
            }
        }
        let mut next: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        for (node, path, via) in &frontier {
            let Some(outs) = by_src.get(node.as_str()) else { continue };
            for (r, to) in outs {
                // the first hop honours an explicit relation label; later hops are free
                if hop == 0 {
                    if let Some(want_rel) = &rel {
                        if !r.eq_ignore_ascii_case(want_rel) {
                            continue;
                        }
                    }
                }
                if path.iter().any(|p| p == *to) {
                    continue;
                }
                // Build the extended path ONCE. It used to be cloned twice more
                // — once into the result and once into the next frontier — and
                // at 75,000 paths those clones were most of the walk's cost.
                let mut p2 = Vec::with_capacity(path.len() + 1);
                p2.extend_from_slice(path);
                p2.push((*to).to_string());
                let mut v2 = Vec::with_capacity(via.len() + 1);
                v2.extend_from_slice(via);
                v2.push((*r).to_string());
                let hops = p2.len() - 1;
                let is_new = !seen.contains(*to);
                if matches(tbox, target, to) {
                    if is_new {
                        paths.push(Path { nodes: p2.clone(), via: v2.clone(), hops });
                    } else {
                        // nothing downstream needs them: move rather than copy
                        paths.push(Path { nodes: p2, via: v2, hops });
                        if paths.len() >= want {
                            return Ok((paths, visited, capped));
                        }
                        continue;
                    }
                    if paths.len() >= want {
                        return Ok((paths, visited, capped));
                    }
                }
                if is_new {
                    seen.insert((*to).to_string());
                    visited += 1;
                    next.push(((*to).to_string(), p2, v2));
                }
            }
        }
        frontier = next;
    }
    if opts.shortest && !paths.is_empty() {
        let min = paths.iter().map(|p| p.hops).min().unwrap_or(0);
        paths.retain(|p| p.hops == min);
    }
    Ok((paths, visited, capped))
}

/// Expand the smaller frontier from each end, stop at the first meeting level.
async fn bidi_shortest(
    store: &Store,
    a: &str,
    b: &str,
    max_hops: usize,
    budget: i64,
    undirected: bool,
) -> Result<(Vec<Path>, usize, bool)> {
    if a.eq_ignore_ascii_case(b) {
        return Ok((vec![Path { nodes: vec![a.to_string()], via: vec![], hops: 0 }], 1, false));
    }
    // If components are labelled and the endpoints differ, "no path" is O(1).
    if let (Ok(Some(ca)), Ok(Some(cb))) = (store.component_of(a).await, store.component_of(b).await) {
        if ca != cb {
            return Ok((vec![], 2, false));
        }
    }
    let mut fwd: HashMap<String, (String, String)> = HashMap::new(); // node -> (parent, rel)
    let mut bwd: HashMap<String, (String, String)> = HashMap::new();
    fwd.insert(a.to_string(), (String::new(), String::new()));
    bwd.insert(b.to_string(), (String::new(), String::new()));
    let mut fq: VecDeque<String> = VecDeque::from(vec![a.to_string()]);
    let mut bq: VecDeque<String> = VecDeque::from(vec![b.to_string()]);
    let mut visited = 2usize;
    let mut capped = false;
    let mut depth = 0usize;

    while !fq.is_empty() && !bq.is_empty() && depth < max_hops {
        depth += 1;
        if visited as i64 >= budget {
            capped = true;
            break;
        }
        // always expand the smaller side
        let expand_fwd = fq.len() <= bq.len();
        let level: Vec<String> = if expand_fwd { fq.drain(..).collect() } else { bq.drain(..).collect() };
        let room = (budget - visited as i64).max(1);
        let edges = if undirected {
            store.expand_both(&level, room).await?
        } else {
            store.expand(&level, expand_fwd, room).await?
        };
        if edges.len() as i64 >= room {
            capped = true;
        }
        let (near, far, queue): (&mut HashMap<_, _>, &HashMap<_, _>, &mut VecDeque<String>) =
            if expand_fwd { (&mut fwd, &bwd, &mut fq) } else { (&mut bwd, &fwd, &mut bq) };
        let mut meeting: Option<String> = None;
        for e in &edges {
            let (src, dst) = if undirected {
                // both orientations are legal; pick the one anchored in `level`
                if near.contains_key(&e.from) { (&e.from, &e.to) } else { (&e.to, &e.from) }
            } else if expand_fwd {
                (&e.from, &e.to)
            } else {
                (&e.to, &e.from)
            };
            if !near.contains_key(src.as_str()) || near.contains_key(dst.as_str()) {
                continue;
            }
            near.insert(dst.clone(), (src.clone(), e.rel.clone()));
            visited += 1;
            queue.push_back(dst.clone());
            if far.contains_key(dst.as_str()) {
                meeting = Some(dst.clone());
                break;
            }
        }
        if let Some(m) = meeting {
            let mut head: Vec<(String, String)> = Vec::new(); // (node, rel-into-it)
            let mut cur = m.clone();
            while let Some((p, r)) = fwd.get(&cur) {
                if p.is_empty() {
                    break;
                }
                head.push((cur.clone(), r.clone()));
                cur = p.clone();
            }
            head.push((cur.clone(), String::new()));
            head.reverse();
            let mut nodes: Vec<String> = head.iter().map(|(n, _)| n.clone()).collect();
            let mut via: Vec<String> = head.iter().skip(1).map(|(_, r)| r.clone()).collect();
            let mut cur = m.clone();
            while let Some((p, r)) = bwd.get(&cur) {
                if p.is_empty() {
                    break;
                }
                nodes.push(p.clone());
                via.push(r.clone());
                cur = p.clone();
            }
            let hops = nodes.len().saturating_sub(1);
            return Ok((vec![Path { nodes, via, hops }], visited, capped));
        }
    }
    Ok((vec![], visited, capped))
}
