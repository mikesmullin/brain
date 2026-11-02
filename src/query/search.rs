//! search.rs — hybrid retrieval: keyword + vector, fused, then expanded.
//!
//! Three stages, each independently skippable because on a big keyword-only
//! index two of them are pure cost:
//!
//! 1. **keyword** — `sum_t idf(t) * ts_rank_cd({D,C,B,A}, tsv, t)` over a
//!    banded tsvector. Both halves of BM25's shape are present: an IDF so rare
//!    terms carry the row, and field weights so a name match outranks a
//!    mention in someone else's relation list. brain1 had neither, which is
//!    how `"wurt"` put `Character/Wurt` at rank 10 behind `Animal/Merm`.
//! 2. **vector** — cosine over pgvector/HNSW. Skipped entirely when the brain
//!    is configured `embed.model: none`; no provider is contacted.
//! 3. **expand** — one hop out of the top seeds, so a question answered by a
//!    neighbour still surfaces the neighbour.

use anyhow::Result;
use serde::Serialize;

use crate::llm::Embedder;
use crate::store::{self, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Hybrid,
    Keyword,
    Vector,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Strategy> {
        match s {
            "hybrid" => Some(Strategy::Hybrid),
            "keyword" => Some(Strategy::Keyword),
            "vector" => Some(Strategy::Vector),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Hybrid => "hybrid",
            Strategy::Keyword => "keyword",
            Strategy::Vector => "vector",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub slug: String,
    pub cls: String,
    pub label: String,
    pub score: f64,
    /// Where this row came from: `keyword`, `vector`, `fused`, `expand:<REL>`.
    pub via: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub query: String,
    pub strategy: &'static str,
    pub expanded: bool,
    pub hits: Vec<Hit>,
    pub ms: u128,
}

pub struct SearchOpts {
    pub limit: i64,
    pub strategy: Strategy,
    pub expand: bool,
    pub explain: bool,
    pub snippets: bool,
}

impl Default for SearchOpts {
    fn default() -> Self {
        SearchOpts { limit: 10, strategy: Strategy::Hybrid, expand: true, explain: false, snippets: false }
    }
}

pub async fn run(
    store: &Store,
    embedder: Option<&Embedder>,
    query: &str,
    opts: &SearchOpts,
) -> Result<SearchResult> {
    let t0 = std::time::Instant::now();
    let pool = (opts.limit * 4).max(40);

    // A degenerate query (all stopwords, or one short token) still has to find
    // the thing the user named, so fall through to a substring scan.
    let mut keyword: Vec<(String, f64)> = Vec::new();
    if opts.strategy != Strategy::Vector {
        keyword = store.keyword(query, pool).await?;
        if keyword.is_empty() {
            keyword = store.contains(query.trim(), pool).await?;
        }
    }

    let mut vector: Vec<(String, f64)> = Vec::new();
    if opts.strategy != Strategy::Keyword {
        if let Some(emb) = embedder {
            if let Ok(v) = emb.embed(&[query.to_string()]).await {
                if let Some(first) = v.first() {
                    vector = store.vector_search(first, pool).await?;
                }
            }
        }
    }

    let kw_rank: std::collections::HashMap<&str, usize> =
        keyword.iter().enumerate().map(|(i, (s, _))| (s.as_str(), i + 1)).collect();
    let vec_rank: std::collections::HashMap<&str, usize> =
        vector.iter().enumerate().map(|(i, (s, _))| (s.as_str(), i + 1)).collect();

    // One ranked list means no fusion to do: a single strategy keeps its own
    // scores, which read better in `--explain` than a reciprocal rank would.
    let (ranked, via): (Vec<(String, f64)>, &'static str) = match opts.strategy {
        Strategy::Keyword => (keyword.clone(), "keyword"),
        Strategy::Vector => (vector.clone(), "vector"),
        Strategy::Hybrid => {
            if vector.is_empty() {
                (keyword.clone(), "keyword")
            } else if keyword.is_empty() {
                (vector.clone(), "vector")
            } else {
                (store::rrf(&[keyword.clone(), vector.clone()], 60.0), "fused")
            }
        }
    };

    let mut seeds: Vec<(String, f64, String)> = ranked
        .into_iter()
        .take(opts.limit as usize)
        .map(|(s, sc)| (s, sc, via.to_string()))
        .collect();

    // 1-hop expansion of the top seeds, appended below them.
    if opts.expand && !seeds.is_empty() {
        let top: Vec<String> = seeds.iter().take(5).map(|(s, _, _)| s.clone()).collect();
        let edges = store.expand_both(&top, 200).await?;
        let have: std::collections::HashSet<String> =
            seeds.iter().map(|(s, _, _)| s.to_lowercase()).collect();
        let mut added: Vec<(String, String)> = Vec::new();
        for e in edges {
            for cand in [(&e.to, &e.rel), (&e.from, &e.rel)] {
                if !have.contains(&cand.0.to_lowercase())
                    && !added.iter().any(|(s, _)| s == cand.0)
                {
                    added.push((cand.0.clone(), cand.1.clone()));
                }
            }
        }
        let room = (opts.limit as usize).saturating_sub(seeds.len()).max(opts.limit as usize / 2);
        for (slug, rel) in added.into_iter().take(room) {
            seeds.push((slug, 0.0, format!("expand:{rel}")));
        }
    }

    let slugs: Vec<String> = seeds.iter().map(|(s, _, _)| s.clone()).collect();
    let rows = store.get_many(&slugs).await?;
    let by_slug: std::collections::HashMap<String, &crate::store::EntityRow> =
        rows.iter().map(|r| (r.slug.to_lowercase(), r)).collect();

    let mut hits = Vec::new();
    for (slug, score, via) in seeds {
        let row = by_slug.get(&slug.to_lowercase());
        hits.push(Hit {
            cls: row.map(|r| r.cls.clone()).unwrap_or_default(),
            label: row.map(|r| r.label.clone()).unwrap_or_else(|| crate::abox::pretty_slug(&slug)),
            snippet: if opts.snippets {
                row.map(|r| snippet(r, query))
            } else {
                None
            },
            keyword_rank: if opts.explain { kw_rank.get(slug.as_str()).copied() } else { None },
            vector_rank: if opts.explain { vec_rank.get(slug.as_str()).copied() } else { None },
            slug,
            score,
            via,
        });
    }

    Ok(SearchResult {
        query: query.to_string(),
        strategy: opts.strategy.as_str(),
        expanded: opts.expand,
        hits,
        ms: t0.elapsed().as_millis(),
    })
}

/// A SERP-style extract: the first stretch of text that mentions a query term,
/// else the start of the record.
pub fn snippet(row: &crate::store::EntityRow, query: &str) -> String {
    let e = row.to_entity();
    let mut text = crate::abox::render_fields(&e, 100_000);
    if !row.body.trim().is_empty() {
        text.push('\n');
        text.push_str(row.body.trim());
    }
    let terms = store::tokenize(query);
    let lower = text.to_lowercase();
    let at = terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .unwrap_or(0);
    let start = text[..at].rfind(['\n', '.', ';']).map(|i| i + 1).unwrap_or(0);
    let mut out: String = text[start..].chars().take(260).collect();
    if text[start..].chars().count() > 260 {
        out.push('…');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
