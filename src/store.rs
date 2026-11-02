//! store.rs — the index, in one PostgreSQL schema per brain.
//!
//! Tables (all inside `<brain>.`):
//!
//! | table      | what it holds |
//! |------------|---------------|
//! | `entities` | one row per entity; `components` jsonb is authoritative, `tsv` is a GENERATED banded tsvector |
//! | `links`    | one row per directed edge, with qualifiers |
//! | `embeds`   | one pgvector per entity (absent when indexed `--no-embed`) |
//! | `terms`    | document-frequency cache, so keyword scoring has a real IDF |
//! | `comps`    | connected-component label per entity (a maintenance artifact) |
//! | `meta`     | cached counters + the schema fingerprint |
//!
//! Two decisions worth stating plainly:
//!
//! * **The tables are `UNLOGGED`.** The index is derived from `.md` on disk and
//!   is rebuilt with one command, exactly as brain1 treated `db/pgdata/`. Not
//!   writing WAL for a rebuildable artifact is most of why the Panama reindex
//!   is fast. A clean `brain pg stop` preserves them; a hard crash truncates
//!   them, and `brain reindex` is the recovery procedure.
//! * **`tsv` is a generated column, not a trigger or a follow-up UPDATE.**
//!   Postgres computes the four weighted bands in C during `COPY`, so bulk
//!   load pays for the tsvector once and never rewrites the heap.

use anyhow::{Context, Result};
use deadpool_postgres::{Config as PoolConfig, ManagerConfig, Pool, RecyclingMethod, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use tokio_postgres::NoTls;

use crate::abox::{Edge, Entity};
use crate::config::PgConfig;

/// `ts_rank_cd` takes its weights as `{D, C, B, A}`.
/// D = relation lines + body, C = own fields, B = roles, A = name/slug/class.
pub const BAND_WEIGHTS: &str = "{0.05,0.2,0.5,1.0}";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub entities: i64,
    pub links: i64,
    pub classes: i64,
    pub relations: i64,
    pub embedded: i64,
    pub components: i64,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub indexed_at: String,
    #[serde(default)]
    pub embed_model: String,
}

#[derive(Debug, Clone)]
pub struct EntityRow {
    pub slug: String,
    pub cls: String,
    pub id: String,
    pub label: String,
    pub components: Value,
    pub body: String,
}

impl EntityRow {
    pub fn to_entity(&self) -> Entity {
        let mut e = Entity::new(self.cls.clone(), self.id.clone());
        e.body = self.body.clone();
        if let Value::Object(m) = &self.components {
            for (alias, v) in m {
                if let Value::Object(f) = v {
                    e.components.insert(alias.clone(), f.clone());
                }
            }
        }
        e
    }
}

#[derive(Debug, Clone)]
pub struct LinkRow {
    pub from: String,
    pub rel: String,
    pub to: String,
    pub qualifiers: Value,
}

#[derive(Clone)]
pub struct Store {
    pub pool: Pool,
    pub schema: String,
    pub dims: usize,
}

fn q(ident: &str) -> String {
    // schema names come from config::sanitize_ident, so they never need quoting;
    // this is belt-and-braces for anything that reaches here by another route.
    if ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        ident.to_string()
    } else {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

impl Store {
    pub async fn connect(cfg: &PgConfig, schema: &str, dims: usize) -> Result<Store> {
        let mut pc = PoolConfig::new();
        pc.host = Some(cfg.host.clone());
        pc.port = Some(cfg.port);
        pc.dbname = Some(cfg.dbname.clone());
        pc.user = Some(cfg.user.clone());
        pc.password = Some(cfg.password.clone().unwrap_or_else(|| "brain".into()));
        pc.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });
        pc.options = Some(format!("-c search_path={},public", schema));
        let pool = pc
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .context("creating the postgres pool")?;
        // fail fast with a message that says what to do
        let probe = pool.get().await.map_err(|e| {
            anyhow::anyhow!(
                "cannot reach postgres at {}:{} ({e})\n\
                 start the shared server with `brain pg start` (first time: `brain pg install`)",
                cfg.host,
                cfg.port
            )
        })?;
        drop(probe);
        Ok(Store { pool, schema: schema.to_string(), dims })
    }

    async fn client(&self) -> Result<deadpool_postgres::Object> {
        self.pool.get().await.context("checking out a postgres connection")
    }

    pub fn s(&self) -> String {
        q(&self.schema)
    }

    // ── DDL ──────────────────────────────────────────────────────────────────

    pub async fn ensure_schema(&self) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        c.batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
            .await
            .context("creating the pgvector extension (the image must be pgvector/pgvector)")?;
        c.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm").await.ok();
        c.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {s}")).await?;
        c.batch_execute(&format!(
            r#"
CREATE UNLOGGED TABLE IF NOT EXISTS {s}.entities (
  slug        text PRIMARY KEY,
  cls         text NOT NULL,
  id          text NOT NULL,
  label       text NOT NULL DEFAULT '',
  roles       text NOT NULL DEFAULT '',
  rels        text NOT NULL DEFAULT '',
  body        text NOT NULL DEFAULT '',
  components  jsonb NOT NULL DEFAULT '{{}}'::jsonb,
  tsv tsvector GENERATED ALWAYS AS (
      setweight(to_tsvector('english'::regconfig,
                 label || ' ' || replace(replace(id,'_',' '),'-',' ') || ' ' || cls), 'A')
   || setweight(to_tsvector('english'::regconfig, roles), 'B')
   || setweight(to_tsvector('english'::regconfig, components::text), 'C')
   || setweight(to_tsvector('english'::regconfig, rels || ' ' || body), 'D')
  ) STORED
);
CREATE UNLOGGED TABLE IF NOT EXISTS {s}.links (
  from_slug  text NOT NULL,
  rel        text NOT NULL,
  to_slug    text NOT NULL,
  qualifiers jsonb NOT NULL DEFAULT '{{}}'::jsonb
);
CREATE UNLOGGED TABLE IF NOT EXISTS {s}.terms (
  term text PRIMARY KEY,
  df   bigint NOT NULL
);
CREATE UNLOGGED TABLE IF NOT EXISTS {s}.comps (
  slug text PRIMARY KEY,
  comp integer NOT NULL
);
CREATE TABLE IF NOT EXISTS {s}.meta (
  k text PRIMARY KEY,
  v jsonb NOT NULL
);
"#
        ))
        .await
        .context("creating the brain tables")?;
        self.ensure_embeds(self.dims).await?;
        self.ensure_indexes().await?;
        Ok(())
    }

    async fn ensure_embeds(&self, dims: usize) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        let have: Option<i32> = c
            .query_opt(
                "SELECT a.atttypmod FROM pg_attribute a
                   JOIN pg_class t ON t.oid = a.attrelid
                   JOIN pg_namespace n ON n.oid = t.relnamespace
                  WHERE n.nspname = $1 AND t.relname = 'embeds' AND a.attname = 'vec'",
                &[&self.schema],
            )
            .await?
            .map(|r| r.get(0));
        if let Some(mod_) = have {
            if mod_ as usize == dims {
                return Ok(());
            }
            // the configured embedding model changed shape; the vectors are stale
            c.batch_execute(&format!("DROP TABLE IF EXISTS {s}.embeds")).await?;
        }
        c.batch_execute(&format!(
            "CREATE UNLOGGED TABLE IF NOT EXISTS {s}.embeds (
               slug text PRIMARY KEY,
               vec  vector({dims}) NOT NULL
             )"
        ))
        .await?;
        Ok(())
    }

    pub async fn ensure_indexes(&self) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        for stmt in [
            format!("CREATE INDEX IF NOT EXISTS entities_tsv_idx ON {s}.entities USING gin(tsv)"),
            format!("CREATE INDEX IF NOT EXISTS entities_cls_idx ON {s}.entities (cls)"),
            format!("CREATE INDEX IF NOT EXISTS entities_label_idx ON {s}.entities (lower(label))"),
            format!("CREATE INDEX IF NOT EXISTS entities_slug_lower_idx ON {s}.entities (lower(slug))"),
            format!("CREATE INDEX IF NOT EXISTS links_from_idx ON {s}.links (from_slug)"),
            format!("CREATE INDEX IF NOT EXISTS links_to_idx ON {s}.links (to_slug)"),
            format!("CREATE INDEX IF NOT EXISTS links_rel_idx ON {s}.links (rel)"),
            format!("CREATE UNIQUE INDEX IF NOT EXISTS links_pk_idx ON {s}.links (from_slug, rel, to_slug)"),
        ] {
            c.batch_execute(&stmt)
                .await
                .with_context(|| format!("creating index: {stmt}"))?;
        }
        Ok(())
    }

    /// HNSW over the embeddings. Built only when there are vectors to index.
    pub async fn ensure_vector_index(&self) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        let n: i64 = c
            .query_one(&format!("SELECT count(*) FROM {s}.embeds"), &[])
            .await?
            .get(0);
        if n == 0 {
            return Ok(());
        }
        c.batch_execute(&format!(
            "CREATE INDEX IF NOT EXISTS embeds_hnsw_idx ON {s}.embeds
               USING hnsw (vec vector_cosine_ops) WITH (m = 16, ef_construction = 64)"
        ))
        .await
        .context("building the HNSW index")?;
        Ok(())
    }



    /// Every brain schema present on the shared server, with its row counts —
    /// what `brain pg status` prints so you can see who the neighbours are.
    pub async fn list_schemas(&self) -> Result<Vec<(String, i64, i64)>> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT n.nspname FROM pg_namespace n
                  WHERE EXISTS (SELECT 1 FROM pg_class t
                                 WHERE t.relnamespace = n.oid AND t.relname = 'entities')
                  ORDER BY n.nspname",
                &[],
            )
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let name: String = r.get(0);
            let sq = q(&name);
            let e: i64 = c
                .query_one(&format!("SELECT count(*) FROM {sq}.entities"), &[])
                .await
                .map(|r| r.get(0))
                .unwrap_or(0);
            let l: i64 = c
                .query_one(&format!("SELECT count(*) FROM {sq}.links"), &[])
                .await
                .map(|r| r.get(0))
                .unwrap_or(0);
            out.push((name, e, l));
        }
        Ok(out)
    }

    // ── meta / stats ─────────────────────────────────────────────────────────

    pub async fn put_meta(&self, k: &str, v: &Value) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            &format!(
                "INSERT INTO {}.meta (k,v) VALUES ($1,$2)
                   ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
                self.s()
            ),
            &[&k, &v],
        )
        .await?;
        Ok(())
    }

    pub async fn get_meta(&self, k: &str) -> Result<Option<Value>> {
        let c = self.client().await?;
        Ok(c.query_opt(&format!("SELECT v FROM {}.meta WHERE k = $1", self.s()), &[&k])
            .await?
            .map(|r| r.get(0)))
    }

    /// O(1): reads the counters a maintenance op last wrote. This is brain1's
    /// run-2 fix (268 ms -> 3.9 ms on 2M rows) kept intact — invalidation stays
    /// entirely under the user's control via `reindex` / `server vacuum`.
    pub async fn stats(&self) -> Result<Stats> {
        match self.get_meta("stats").await? {
            Some(v) => Ok(serde_json::from_value(v).unwrap_or_default()),
            None => self.refresh_stats().await,
        }
    }

    pub async fn refresh_stats(&self) -> Result<Stats> {
        let c = self.client().await?;
        let s = self.s();
        let row = c
            .query_one(
                &format!(
                    "SELECT (SELECT count(*) FROM {s}.entities),
                            (SELECT count(*) FROM {s}.links),
                            (SELECT count(DISTINCT cls) FROM {s}.entities),
                            (SELECT count(DISTINCT rel) FROM {s}.links),
                            (SELECT count(*) FROM {s}.embeds),
                            (SELECT count(DISTINCT comp) FROM {s}.comps)"
                ),
                &[],
            )
            .await?;
        let prev = self.get_meta("stats").await?.and_then(|v| serde_json::from_value::<Stats>(v).ok());
        let st = Stats {
            entities: row.get(0),
            links: row.get(1),
            classes: row.get(2),
            relations: row.get(3),
            embedded: row.get(4),
            components: row.get(5),
            fingerprint: prev.as_ref().map(|p| p.fingerprint.clone()).unwrap_or_default(),
            indexed_at: prev.as_ref().map(|p| p.indexed_at.clone()).unwrap_or_default(),
            embed_model: prev.as_ref().map(|p| p.embed_model.clone()).unwrap_or_default(),
        };
        self.put_meta("stats", &serde_json::to_value(&st)?).await?;
        Ok(st)
    }

    pub async fn set_stats(&self, st: &Stats) -> Result<()> {
        self.put_meta("stats", &serde_json::to_value(st)?).await
    }

    pub async fn is_empty(&self) -> Result<bool> {
        let c = self.client().await?;
        let n: i64 = c
            .query_one(&format!("SELECT count(*) FROM (SELECT 1 FROM {}.entities LIMIT 1) z", self.s()), &[])
            .await?
            .get(0);
        Ok(n == 0)
    }

    // ── bulk load ────────────────────────────────────────────────────────────

    /// Truncate the index in preparation for a full rebuild. Indexes are
    /// dropped too: creating them after the COPY is several times faster than
    /// maintaining them during it.
    pub async fn begin_rebuild(&self) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        c.batch_execute(&format!(
            "TRUNCATE {s}.entities, {s}.links, {s}.terms, {s}.comps;
             DROP INDEX IF EXISTS {s}.entities_tsv_idx;
             DROP INDEX IF EXISTS {s}.entities_cls_idx;
             DROP INDEX IF EXISTS {s}.entities_label_idx;
             DROP INDEX IF EXISTS {s}.entities_slug_lower_idx;
             DROP INDEX IF EXISTS {s}.links_from_idx;
             DROP INDEX IF EXISTS {s}.links_to_idx;
             DROP INDEX IF EXISTS {s}.links_rel_idx;
             DROP INDEX IF EXISTS {s}.links_pk_idx;"
        ))
        .await?;
        Ok(())
    }

    pub async fn finish_rebuild(&self) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        self.ensure_indexes().await?;
        c.batch_execute(&format!("ANALYZE {s}.entities; ANALYZE {s}.links;")).await?;
        Ok(())
    }

    /// Stream one batch of entities + their edges into the index with `COPY`.
    /// Call it from several tasks at once: concurrent COPY into the same table
    /// is how the Panama load saturates the box.
    pub async fn copy_batch(&self, entities: &[Entity]) -> Result<(u64, u64)> {
        use futures_util::SinkExt;
        let c = self.client().await?;
        let s = self.s();

        let mut ebuf = String::with_capacity(entities.len() * 512);
        let mut lbuf = String::with_capacity(entities.len() * 128);
        let mut nlinks = 0u64;
        for e in entities {
            let slug = e.slug();
            let label = e.label();
            let roles = roles_text(e);
            let rels = rels_text(e);
            let comps = components_json(e);
            push_row(
                &mut ebuf,
                &[
                    slug.as_str(),
                    e.cls.as_str(),
                    e.id.as_str(),
                    label.as_str(),
                    roles.as_str(),
                    rels.as_str(),
                    e.body.trim(),
                    comps.as_str(),
                ],
            );
            for (rel, edges) in &e.relations {
                for ed in edges {
                    let qj = if ed.qualifiers.is_empty() {
                        "{}".to_string()
                    } else {
                        serde_json::to_string(&ed.qualifiers).unwrap_or_else(|_| "{}".into())
                    };
                    push_row(&mut lbuf, &[slug.as_str(), rel.as_str(), ed.to.as_str(), qj.as_str()]);
                    nlinks += 1;
                }
            }
        }

        let mut sink = std::pin::pin!(
            c.copy_in(&format!(
                "COPY {s}.entities (slug,cls,id,label,roles,rels,body,components) FROM STDIN"
            ))
            .await?
        );
        sink.send(bytes::Bytes::from(ebuf)).await?;
        sink.finish().await?;

        if nlinks > 0 {
            let mut sink = std::pin::pin!(
                c.copy_in(&format!("COPY {s}.links (from_slug,rel,to_slug,qualifiers) FROM STDIN"))
                    .await?
            );
            sink.send(bytes::Bytes::from(lbuf)).await?;
            sink.finish().await?;
        }
        Ok((entities.len() as u64, nlinks))
    }

    /// Incremental write of a single entity (the interactive path).
    pub async fn upsert(&self, e: &Entity) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        let slug = e.slug();
        let comps: Value = serde_json::from_str(&components_json(e)).unwrap_or(Value::Object(Default::default()));
        c.execute(
            &format!(
                "INSERT INTO {s}.entities (slug,cls,id,label,roles,rels,body,components)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
                 ON CONFLICT (slug) DO UPDATE SET
                   cls=EXCLUDED.cls, id=EXCLUDED.id, label=EXCLUDED.label,
                   roles=EXCLUDED.roles, rels=EXCLUDED.rels,
                   body=EXCLUDED.body, components=EXCLUDED.components"
            ),
            &[
                &slug,
                &e.cls,
                &e.id,
                &e.label(),
                &roles_text(e),
                &rels_text(e),
                &e.body.trim(),
                &comps,
            ],
        )
        .await?;
        c.execute(&format!("DELETE FROM {s}.links WHERE from_slug = $1"), &[&slug]).await?;
        for (rel, edges) in &e.relations {
            for ed in edges {
                let qj: Value = if ed.qualifiers.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::to_value(&ed.qualifiers)?
                };
                c.execute(
                    &format!(
                        "INSERT INTO {s}.links (from_slug,rel,to_slug,qualifiers) VALUES ($1,$2,$3,$4)
                         ON CONFLICT (from_slug,rel,to_slug) DO UPDATE SET qualifiers = EXCLUDED.qualifiers"
                    ),
                    &[&slug, rel, &ed.to, &qj],
                )
                .await?;
            }
        }
        Ok(())
    }

    pub async fn delete(&self, slug: &str) -> Result<bool> {
        let c = self.client().await?;
        let s = self.s();
        let n = c
            .execute(&format!("DELETE FROM {s}.entities WHERE lower(slug) = lower($1)"), &[&slug])
            .await?;
        c.execute(
            &format!("DELETE FROM {s}.links WHERE from_slug = $1 OR to_slug = $1"),
            &[&slug],
        )
        .await?;
        c.execute(&format!("DELETE FROM {s}.embeds WHERE lower(slug) = lower($1)"), &[&slug]).await?;
        Ok(n > 0)
    }

    pub async fn put_embeddings(&self, rows: &[(String, Vec<f32>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        use futures_util::SinkExt;
        let c = self.client().await?;
        let s = self.s();
        let mut buf = String::with_capacity(rows.len() * (self.dims * 8));
        for (slug, v) in rows {
            let lit = vector_literal(v);
            push_row(&mut buf, &[slug.as_str(), lit.as_str()]);
        }
        let sink = c.copy_in(&format!("COPY {s}.embeds (slug,vec) FROM STDIN")).await?;
        let mut sink = std::pin::pin!(sink);
        sink.send(bytes::Bytes::from(buf)).await?;
        sink.finish().await?;
        Ok(())
    }

    // ── point reads ──────────────────────────────────────────────────────────

    /// Fold a user-typed slug to the canonical stored one. Every other read
    /// takes a canonical slug, so case folding costs one index probe per
    /// command rather than one per edge query.
    pub async fn resolve(&self, slug: &str) -> Result<Option<String>> {
        let c = self.client().await?;
        let s = self.s();
        if let Some(r) = c
            .query_opt(&format!("SELECT slug FROM {s}.entities WHERE slug = $1"), &[&slug])
            .await?
        {
            return Ok(Some(r.get(0)));
        }
        Ok(c.query_opt(
            &format!("SELECT slug FROM {s}.entities WHERE lower(slug) = lower($1)"),
            &[&slug],
        )
        .await?
        .map(|r| r.get(0)))
    }

    pub async fn get(&self, slug: &str) -> Result<Option<EntityRow>> {
        let c = self.client().await?;
        let s = self.s();
        let r = c
            .query_opt(
                &format!(
                    "SELECT slug,cls,id,label,components,body FROM {s}.entities
                      WHERE lower(slug) = lower($1)"
                ),
                &[&slug],
            )
            .await?;
        Ok(r.map(row_to_entity))
    }

    pub async fn get_many(&self, slugs: &[String]) -> Result<Vec<EntityRow>> {
        if slugs.is_empty() {
            return Ok(vec![]);
        }
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!(
                    "SELECT slug,cls,id,label,components,body FROM {s}.entities
                      WHERE lower(slug) = ANY($1)"
                ),
                &[&slugs.iter().map(|x| x.to_lowercase()).collect::<Vec<_>>()],
            )
            .await?;
        Ok(rows.into_iter().map(row_to_entity).collect())
    }

    pub async fn labels_of(&self, slugs: &[String]) -> Result<HashMap<String, String>> {
        if slugs.is_empty() {
            return Ok(HashMap::new());
        }
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!("SELECT slug,label FROM {s}.entities WHERE lower(slug) = ANY($1)"),
                &[&slugs.iter().map(|x| x.to_lowercase()).collect::<Vec<_>>()],
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.get::<_, String>(0), r.get::<_, String>(1))).collect())
    }

    /// Entities whose label matches `name` exactly (case-insensitive). The
    /// crawl needs *all* of them: three dstwiki pages are titled "Mandrake".
    pub async fn by_label(&self, name: &str, limit: i64) -> Result<Vec<EntityRow>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!(
                    "SELECT slug,cls,id,label,components,body FROM {s}.entities
                      WHERE lower(label) = lower($1) OR lower(id) = lower($1)
                      ORDER BY length(slug) LIMIT $2"
                ),
                &[&name, &limit],
            )
            .await?;
        Ok(rows.into_iter().map(row_to_entity).collect())
    }

    pub async fn class_counts(&self) -> Result<Vec<(String, i64)>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(&format!("SELECT cls, count(*) FROM {s}.entities GROUP BY cls ORDER BY cls"), &[])
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn relation_counts(&self) -> Result<Vec<(String, i64)>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(&format!("SELECT rel, count(*) FROM {s}.links GROUP BY rel ORDER BY rel"), &[])
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn list_class(&self, cls: Option<&str>, limit: i64, offset: i64) -> Result<Vec<(String, String, String)>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = match cls {
            Some(cl) => {
                c.query(
                    &format!(
                        "SELECT slug,cls,label FROM {s}.entities WHERE lower(cls) = lower($1)
                          ORDER BY id LIMIT $2 OFFSET $3"
                    ),
                    &[&cl, &limit, &offset],
                )
                .await?
            }
            None => {
                c.query(
                    &format!("SELECT slug,cls,label FROM {s}.entities ORDER BY cls, id LIMIT $1 OFFSET $2"),
                    &[&limit, &offset],
                )
                .await?
            }
        };
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
    }

    pub async fn out_edges(&self, slug: &str) -> Result<Vec<LinkRow>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!(
                    "SELECT from_slug,rel,to_slug,qualifiers FROM {s}.links
                      WHERE from_slug = $1 ORDER BY rel, to_slug"
                ),
                &[&slug],
            )
            .await?;
        Ok(rows.into_iter().map(row_to_link).collect())
    }

    pub async fn in_edges(&self, slug: &str) -> Result<Vec<LinkRow>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!(
                    "SELECT from_slug,rel,to_slug,qualifiers FROM {s}.links
                      WHERE to_slug = $1 ORDER BY rel, from_slug"
                ),
                &[&slug],
            )
            .await?;
        Ok(rows.into_iter().map(row_to_link).collect())
    }

    /// One frontier expansion. `out = true` walks `from -> to`.
    ///
    /// Qualifiers are deliberately NOT selected. Traversal reads thethree text
    /// columns and nothing else, and a jsonb per edge is decoded into a
    /// `serde_json::Value` on the client for every row — 75,000 of them on one
    /// two-hop walk of a 36k-degree hub. `out_edges` / `in_edges` fetch them,
    /// because a page that renders an edge is the only thing that wants them.
    ///
    /// `rels` narrows the walk server-side. A projection that names one
    /// relation should not drag back 36,284 edges to find 30 of them.
    pub async fn expand(&self, frontier: &[String], out: bool, cap: i64) -> Result<Vec<LinkRow>> {
        self.expand_rel(frontier, out, cap, None).await
    }

    pub async fn expand_rel(
        &self,
        frontier: &[String],
        out: bool,
        cap: i64,
        rels: Option<&[String]>,
    ) -> Result<Vec<LinkRow>> {
        if frontier.is_empty() {
            return Ok(vec![]);
        }
        let c = self.client().await?;
        let s = self.s();
        let col = if out { "from_slug" } else { "to_slug" };
        let rows = match rels {
            None => {
                c.query(
                    &format!("SELECT from_slug,rel,to_slug FROM {s}.links WHERE {col} = ANY($1) LIMIT $2"),
                    &[&frontier, &cap],
                )
                .await?
            }
            Some(r) => {
                c.query(
                    &format!(
                        "SELECT from_slug,rel,to_slug FROM {s}.links
                          WHERE {col} = ANY($1) AND rel = ANY($2) LIMIT $3"
                    ),
                    &[&frontier, &r, &cap],
                )
                .await?
            }
        };
        Ok(rows.into_iter().map(row_to_edge).collect())
    }

    /// Both directions in one round trip — what `graph`'s undirected mode and
    /// the search expansion want. No qualifiers, for the reason above.
    pub async fn expand_both(&self, frontier: &[String], cap: i64) -> Result<Vec<LinkRow>> {
        if frontier.is_empty() {
            return Ok(vec![]);
        }
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!(
                    "SELECT from_slug,rel,to_slug FROM {s}.links
                      WHERE from_slug = ANY($1) OR to_slug = ANY($1) LIMIT $2"
                ),
                &[&frontier, &cap],
            )
            .await?;
        Ok(rows.into_iter().map(row_to_edge).collect())
    }

    pub async fn top_degree(&self, n: i64) -> Result<Vec<(String, i64)>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!("SELECT from_slug, count(*) c FROM {s}.links GROUP BY from_slug ORDER BY c DESC LIMIT $1"),
                &[&n],
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    // ── retrieval ────────────────────────────────────────────────────────────

    /// IDF per query term, cached in `terms`. The cache is populated lazily —
    /// a `ts_stat` pass over 2M documents at index time costs more than every
    /// query it would ever serve.
    pub async fn idf(&self, terms: &[String]) -> Result<HashMap<String, f64>> {
        let mut out = HashMap::new();
        if terms.is_empty() {
            return Ok(out);
        }
        let c = self.client().await?;
        let s = self.s();
        let n: i64 = self.stats().await?.entities.max(1);
        let known = c
            .query(&format!("SELECT term, df FROM {s}.terms WHERE term = ANY($1)"), &[&terms])
            .await?;
        let mut dfs: HashMap<String, i64> =
            known.into_iter().map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1))).collect();
        for t in terms {
            if dfs.contains_key(t) {
                continue;
            }
            let df: i64 = c
                .query_one(
                    &format!(
                        "SELECT count(*) FROM {s}.entities WHERE tsv @@ plainto_tsquery('english'::regconfig, $1)"
                    ),
                    &[t],
                )
                .await?
                .get(0);
            c.execute(
                &format!("INSERT INTO {s}.terms (term,df) VALUES ($1,$2) ON CONFLICT (term) DO UPDATE SET df = EXCLUDED.df"),
                &[t, &df],
            )
            .await
            .ok();
            dfs.insert(t.clone(), df);
        }
        for (t, df) in dfs {
            // Robertson/Sparck-Jones shape, floored so a term in every document
            // still contributes a little rather than cancelling the row.
            let idf = (1.0 + (n as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
            out.insert(t, idf.max(0.05));
        }
        Ok(out)
    }

    /// Keyword search: `sum over terms of idf(t) * ts_rank_cd(bands, tsv, t)`.
    ///
    /// Both halves of BM25's shape matter and brain1 had neither: without IDF
    /// the terms are effectively OR'd with equal say, and without field weights
    /// a name match ranks level with a mention three hops away in someone
    /// else's relation list.
    pub async fn keyword(&self, query: &str, limit: i64) -> Result<Vec<(String, f64)>> {
        let terms = tokenize(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let idf = self.idf(&terms).await?;
        let weights: Vec<f64> = terms.iter().map(|t| *idf.get(t).unwrap_or(&1.0)).collect();
        let c = self.client().await?;
        let s = self.s();
        let sql = format!(
            "WITH q AS (
               SELECT t.term, t.w, plainto_tsquery('english'::regconfig, t.term) AS tq
                 FROM unnest($1::text[], $2::float8[]) AS t(term, w)
             )
             SELECT e.slug, sum(q.w * ts_rank_cd('{BAND_WEIGHTS}', e.tsv, q.tq))::float8 AS score
               FROM q JOIN {s}.entities e ON e.tsv @@ q.tq
              GROUP BY e.slug
              ORDER BY score DESC, e.slug
              LIMIT $3"
        );
        let rows = c.query(&sql, &[&terms, &weights, &limit]).await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn vector_search(&self, vec: &[f32], limit: i64) -> Result<Vec<(String, f64)>> {
        let c = self.client().await?;
        let s = self.s();
        let lit = vector_literal(vec);
        let rows = c
            .query(
                &format!(
                    "SELECT slug, (1 - (vec <=> $1::vector))::float8 AS sim
                       FROM {s}.embeds ORDER BY vec <=> $1::vector LIMIT $2"
                ),
                &[&lit, &limit],
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Substring/trigram fallback, which is what pgGraph's `contains` mode does.
    pub async fn contains(&self, needle: &str, limit: i64) -> Result<Vec<(String, f64)>> {
        let c = self.client().await?;
        let s = self.s();
        let pat = format!("%{}%", needle.to_lowercase());
        let rows = c
            .query(
                &format!(
                    "SELECT slug, 1.0::float8 FROM {s}.entities
                      WHERE lower(label) LIKE $1 OR lower(slug) LIKE $1
                      ORDER BY length(label), slug LIMIT $2"
                ),
                &[&pat, &limit],
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    // ── maintenance artifacts ────────────────────────────────────────────────

    /// Label connected components (undirected). Enables an O(1) "there is no
    /// path" answer and gives the wiki a cheap notion of neighbourhood.
    pub async fn label_components(&self) -> Result<i64> {
        let c = self.client().await?;
        let s = self.s();
        // Union-find in the client: one streaming pass over links beats any
        // recursive-CTE formulation by a wide margin at a few million edges.
        let slugs: Vec<String> = c
            .query(&format!("SELECT slug FROM {s}.entities"), &[])
            .await?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        let mut idx: HashMap<String, usize> = HashMap::with_capacity(slugs.len());
        for (i, sl) in slugs.iter().enumerate() {
            idx.insert(sl.clone(), i);
        }
        let mut parent: Vec<usize> = (0..slugs.len()).collect();
        fn find(p: &mut Vec<usize>, mut x: usize) -> usize {
            while p[x] != x {
                p[x] = p[p[x]];
                x = p[x];
            }
            x
        }
        let rows = c.query(&format!("SELECT from_slug, to_slug FROM {s}.links"), &[]).await?;
        for r in rows {
            let a: String = r.get(0);
            let b: String = r.get(1);
            if let (Some(&ia), Some(&ib)) = (idx.get(&a), idx.get(&b)) {
                let (ra, rb) = (find(&mut parent, ia), find(&mut parent, ib));
                if ra != rb {
                    parent[ra] = rb;
                }
            }
        }
        let mut comp_of: Vec<i32> = vec![0; slugs.len()];
        let mut seen: HashMap<usize, i32> = HashMap::new();
        let mut next = 0i32;
        for i in 0..slugs.len() {
            let r = find(&mut parent, i);
            let c = *seen.entry(r).or_insert_with(|| {
                next += 1;
                next
            });
            comp_of[i] = c;
        }
        use futures_util::SinkExt;
        c.batch_execute(&format!("TRUNCATE {s}.comps")).await?;
        let mut buf = String::with_capacity(slugs.len() * 32);
        for (i, sl) in slugs.iter().enumerate() {
            let n = comp_of[i].to_string();
            push_row(&mut buf, &[sl.as_str(), n.as_str()]);
        }
        let sink = c.copy_in(&format!("COPY {s}.comps (slug,comp) FROM STDIN")).await?;
        let mut sink = std::pin::pin!(sink);
        sink.send(bytes::Bytes::from(buf)).await?;
        sink.finish().await?;
        Ok(next as i64)
    }

    pub async fn component_of(&self, slug: &str) -> Result<Option<i32>> {
        let c = self.client().await?;
        let s = self.s();
        Ok(c.query_opt(&format!("SELECT comp FROM {s}.comps WHERE slug = $1"), &[&slug])
            .await?
            .map(|r| r.get(0)))
    }

    pub async fn orphans(&self, limit: i64) -> Result<Vec<(String, String)>> {
        let c = self.client().await?;
        let s = self.s();
        let rows = c
            .query(
                &format!(
                    "SELECT e.slug, e.cls FROM {s}.entities e
                      WHERE NOT EXISTS (SELECT 1 FROM {s}.links l WHERE l.from_slug = e.slug)
                        AND NOT EXISTS (SELECT 1 FROM {s}.links l WHERE l.to_slug = e.slug)
                      ORDER BY e.cls, e.id LIMIT $1"
                ),
                &[&limit],
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }


    pub async fn vacuum(&self) -> Result<()> {
        let c = self.client().await?;
        let s = self.s();
        c.batch_execute(&format!("VACUUM ANALYZE {s}.entities; VACUUM ANALYZE {s}.links;")).await?;
        Ok(())
    }


}

// ── helpers ──────────────────────────────────────────────────────────────────

fn row_to_entity(r: tokio_postgres::Row) -> EntityRow {
    EntityRow {
        slug: r.get(0),
        cls: r.get(1),
        id: r.get(2),
        label: r.get(3),
        components: r.get(4),
        body: r.get(5),
    }
}

fn row_to_link(r: tokio_postgres::Row) -> LinkRow {
    LinkRow { from: r.get(0), rel: r.get(1), to: r.get(2), qualifiers: r.get(3) }
}

fn row_to_edge(r: tokio_postgres::Row) -> LinkRow {
    LinkRow { from: r.get(0), rel: r.get(1), to: r.get(2), qualifiers: Value::Null }
}

/// Postgres text-format COPY: tab-separated, `\N` for null, backslash escapes.
fn push_row(buf: &mut String, cols: &[&str]) {
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            buf.push('\t');
        }
        for ch in c.chars() {
            match ch {
                '\\' => buf.push_str("\\\\"),
                '\t' => buf.push_str("\\t"),
                '\n' => buf.push_str("\\n"),
                '\r' => buf.push_str("\\r"),
                c => buf.push(c),
            }
        }
    }
    buf.push('\n');
}

pub fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 9 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x:.6}"));
    }
    s.push(']');
    s
}

/// Band B: verbatim role/category lists — the dstwiki convention where a wiki
/// category that is not a structural type stays an attribute.
fn roles_text(e: &Entity) -> String {
    let mut out: Vec<String> = Vec::new();
    for fields in e.components.values() {
        for key in ["roles", "tags", "categories", "category"] {
            match fields.get(key) {
                Some(Value::Array(a)) => {
                    for i in a {
                        if let Some(s) = i.as_str() {
                            out.push(s.to_string());
                        }
                    }
                }
                Some(Value::String(s)) => out.push(s.clone()),
                _ => {}
            }
        }
    }
    out.join(" ")
}

/// Band D: `REL target` lines, capped so a 36k-degree hub cannot produce a
/// multi-megabyte tsvector input.
fn rels_text(e: &Entity) -> String {
    let mut out = String::new();
    for (rel, edges) in &e.relations {
        for ed in edges {
            out.push_str(rel);
            out.push(' ');
            let tail = ed.to.split('/').next_back().unwrap_or(&ed.to);
            out.push_str(&tail.replace('_', " "));
            out.push(' ');
            if out.len() > 8192 {
                return out;
            }
        }
    }
    out
}

fn components_json(e: &Entity) -> String {
    let m: BTreeMap<&String, Value> = e
        .components
        .iter()
        .map(|(k, v)| (k, Value::Object(v.clone())))
        .collect();
    serde_json::to_string(&m).unwrap_or_else(|_| "{}".into())
}

/// Query tokenizer: lowercase alphanumeric runs, stopwords and 1–2 char tokens
/// dropped. Short tokens are dropped because brain-db has a `Team/sre`, and a
/// 3-char id is a real thing to search for while "of" is not.
pub fn tokenize(q: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "has", "her",
        "was", "one", "our", "out", "who", "its", "did", "yes", "how", "what", "which", "does",
        "with", "from", "that", "this", "there", "their", "them", "then", "than", "into", "about",
        "when", "where", "will", "would", "could", "should", "been", "being", "have", "were",
        "why", "whom", "whose", "each", "most", "some", "such", "only", "very", "much", "many",
        "list", "show", "give", "tell", "name", "names", "please",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let push = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 3 && !STOP.contains(&cur.as_str()) && !out.contains(cur) {
            out.push(cur.clone());
        }
        cur.clear();
    };
    for ch in q.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch.to_lowercase().next().unwrap_or(ch));
        } else {
            push(&mut cur, &mut out);
        }
    }
    push(&mut cur, &mut out);
    out
}

/// Reciprocal Rank Fusion. `k = 60` is the value the original RRF paper
/// recommends and what brain1 shipped; keeping it means the two systems' search
/// results stay comparable.
pub fn rrf(lists: &[Vec<(String, f64)>], k: f64) -> Vec<(String, f64)> {
    let mut acc: HashMap<String, f64> = HashMap::new();
    for l in lists {
        for (rank, (slug, _)) in l.iter().enumerate() {
            *acc.entry(slug.clone()).or_insert(0.0) += 1.0 / (k + (rank as f64 + 1.0));
        }
    }
    let mut v: Vec<(String, f64)> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    v
}


/// Reconstruct a full `Entity` (components + both edge directions) for display.
pub async fn hydrate(store: &Store, slug: &str) -> Result<Option<(Entity, Vec<LinkRow>)>> {
    let Some(row) = store.get(slug).await? else { return Ok(None) };
    let mut e = row.to_entity();
    for l in store.out_edges(&row.slug).await? {
        let quals: BTreeMap<String, Value> = match l.qualifiers {
            Value::Object(m) => m.into_iter().collect(),
            _ => BTreeMap::new(),
        };
        e.relations.entry(l.rel).or_default().push(Edge { to: l.to, qualifiers: quals });
    }
    let incoming = store.in_edges(&row.slug).await?;
    Ok(Some((e, incoming)))
}

/// Does the index predate the files on disk? Only asked when the user asks —
/// the point of opt-in is that nothing spends time detecting staleness unless
/// it was told to.
pub fn disk_fingerprint(roots: &[std::path::PathBuf]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for r in roots {
        for e in walkdir::WalkDir::new(r).sort_by_file_name().into_iter().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md")
                && p.file_name().and_then(|x| x.to_str()) != Some("schema.yaml")
            {
                continue;
            }
            if let Ok(md) = e.metadata() {
                h.update(p.to_string_lossy().as_bytes());
                h.update(md.len().to_le_bytes());
                if let Ok(m) = md.modified() {
                    if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                        h.update(d.as_secs().to_le_bytes());
                    }
                }
            }
        }
    }
    hex::encode(h.finalize())[..16].to_string()
}
