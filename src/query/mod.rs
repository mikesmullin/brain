//! query — the six ways to ask.
//!
//! | mode | kind | what decides the answer |
//! |---|---|---|
//! | `search`   | deterministic | BM25-shaped keyword + vector, fused with RRF |
//! | `graph`    | deterministic | pattern match over `links` (Mermaid-ish) |
//! | `graphql`  | deterministic | projection from a known root |
//! | `think`    | model         | retrieve, then one grounded synthesis |
//! | `ontology` | model         | an agent calls a deterministic paths tool |
//! | `crawl`    | model         | one typed decision per hop, over real edges |
//!
//! `crawl` is brain2's default. The first three cannot be wrong about the graph
//! but need you to know what to ask; the last three accept a sentence. What
//! separates `crawl` from `think` and `ontology` is where the model's freedom
//! lives: `think` is handed text and writes prose, `ontology` picks tool calls,
//! and `crawl` only ever picks among options the engine has already fetched.

pub mod crawl;
pub mod graph;
pub mod graphql;
pub mod ontology;
pub mod search;
pub mod think;
