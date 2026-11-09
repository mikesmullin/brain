//! llm.rs — the three model endpoints brain2 talks to, and nothing else.
//!
//! | client | endpoint | what it is for |
//! |---|---|---|
//! | [`Slot`] | `POST {slot_url}/v1/systemone` | the non-autoregressive **decision slot**: typed questions (`choice`, `noul`, `score`) answered in ~30–60 ms |
//! | [`Chat`] | `POST {chat_url}/v1/chat/completions` | phrasing an answer (`think`, the crawl's finalizer) and the agentic `ontology` traversal |
//! | [`Embedder`] | `POST {url}` (OpenAI `embeddings` shape) | one vector per entity, batched |
//!
//! A `slot_url` that already names a path (`…/v1/decisions`) is used verbatim,
//! so a provider that spells the slot endpoint differently needs no fork.
//!
//! All three send `Authorization: Bearer <token>` when the brain has a token —
//! see [`crate::config::api_key`] for where one comes from, and nothing at all
//! when it has none, which is every local model server.
//!
//! The division of labour is the whole architecture: **facts live in the
//! engine, judgements live in the model.** The slot never sees a free-text
//! field it can invent an entity into — every question it answers is a choice
//! over options the engine fetched, or a truth value about one node the engine
//! already read. That is why a hallucinated path is not merely unlikely here;
//! it is unrepresentable.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{self, LlmConfig};

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("building the http client")
}

/// Attach the provider's bearer token, if this brain has one. Local servers
/// (llama.cpp, ollama, systemone) do not check it, so `None` is the normal
/// case and sends no header rather than an empty one.
fn auth(
    req: reqwest::RequestBuilder,
    key: &Option<String>,
) -> reqwest::RequestBuilder {
    match key {
        Some(k) => req.bearer_auth(k),
        None => req,
    }
}

/// `{base}/v1/{path}`, unless `base` already carries a versioned path — then
/// it is the full endpoint and gets used as written.
fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.contains("/v1/") {
        base.to_string()
    } else {
        format!("{base}/{path}")
    }
}

// ── the decision slot ────────────────────────────────────────────────────────

/// One typed question. `criteria` is only read for `choice`.
#[derive(Debug, Clone, Serialize)]
pub struct Question {
    pub r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    pub instructions: String,
}

impl Question {
    pub fn choice(stage: &str, criteria: BTreeMap<String, String>, instructions: String) -> Question {
        Question {
            r#type: "choice",
            criteria: Some(criteria),
            stage: Some(stage.to_string()),
            instructions,
        }
    }
    pub fn noul(instructions: String) -> Question {
        Question { r#type: "noul", criteria: None, stage: None, instructions }
    }
}

/// One answer. Exactly one field is populated, decided by the question type.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Answer {
    #[serde(default)]
    pub choice: Option<String>,
    #[serde(default)]
    pub noul: Option<f64>,
    #[serde(default)]
    pub score: Option<f64>,
    /// The slot reports its own confidence for a `choice`.
    #[serde(default)]
    pub p: Option<f64>,
}

impl Answer {
    pub fn fmt(&self) -> String {
        if let Some(c) = &self.choice {
            return match self.p {
                Some(p) => format!("choice={c} p={p:.3}"),
                None => format!("choice={c}"),
            };
        }
        if let Some(n) = self.noul {
            return format!("noul={n:.4}");
        }
        if let Some(s) = self.score {
            return format!("score={s:.2}");
        }
        "<no answer>".into()
    }
}

#[derive(Debug, Deserialize)]
struct SlotResponse {
    #[serde(default)]
    answers: BTreeMap<String, Answer>,
}

#[derive(Clone)]
pub struct Slot {
    url: String,
    key: Option<String>,
    http: reqwest::Client,
    pub calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub tokens: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Slot {
    pub fn new(cfg: &LlmConfig) -> Result<Slot> {
        Ok(Slot {
            url: endpoint(&cfg.slot_url, "v1/systemone"),
            key: config::api_key("slot", cfg.api_key.as_deref()),
            http: client()?,
            calls: Default::default(),
            tokens: Default::default(),
        })
    }

    /// One round trip answering every question for this turn at once.
    pub async fn ask(
        &self,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<BTreeMap<String, Answer>> {
        use std::sync::atomic::Ordering;
        if questions.is_empty() {
            return Ok(BTreeMap::new());
        }
        let body = json!({ "model": "systemone:default", "state": state, "questions": questions });
        // A rough token count, for the cost report; the slot does not return usage.
        let approx = (state.len()
            + questions.values().map(|q| q.instructions.len()).sum::<usize>())
            / 4;
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.tokens.fetch_add(approx as u64, Ordering::Relaxed);
        let res = auth(self.http.post(&self.url), &self.key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {} (is the decision slot running?)", self.url))?;
        let status = res.status();
        let txt = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("slot {}: {}", status.as_u16(), txt.chars().take(300).collect::<String>());
        }
        let parsed: SlotResponse = serde_json::from_str(&txt)
            .with_context(|| format!("slot returned non-JSON: {}", txt.chars().take(200).collect::<String>()))?;
        Ok(parsed.answers)
    }

    pub fn counters(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (self.calls.load(Ordering::Relaxed), self.tokens.load(Ordering::Relaxed))
    }
}

// ── chat ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(c: impl Into<String>) -> Self {
        ChatMessage { role: "system".into(), content: c.into() }
    }
    pub fn user(c: impl Into<String>) -> Self {
        ChatMessage { role: "user".into(), content: c.into() }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        ChatMessage { role: "assistant".into(), content: c.into() }
    }
}

#[derive(Clone)]
pub struct Chat {
    url: String,
    model: String,
    key: Option<String>,
    http: reqwest::Client,
}

impl Chat {
    pub fn new(cfg: &LlmConfig) -> Result<Chat> {
        Ok(Chat {
            url: endpoint(&cfg.chat_url, "v1/chat/completions"),
            model: cfg.chat_model.clone(),
            key: config::api_key("chat", cfg.api_key.as_deref()),
            http: client()?,
        })
    }

    pub async fn complete(&self, messages: &[ChatMessage], temperature: f64) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
            "stream": false,
        });
        let res = auth(self.http.post(&self.url), &self.key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {} (is the chat model running?)", self.url))?;
        let status = res.status();
        let txt = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("chat {}: {}", status.as_u16(), txt.chars().take(300).collect::<String>());
        }
        let v: Value = serde_json::from_str(&txt)
            .with_context(|| format!("chat returned non-JSON: {}", txt.chars().take(200).collect::<String>()))?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| v["choices"][0]["text"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        Ok(content.trim().to_string())
    }

    /// A completion the caller needs as JSON. One retry that says "JSON only",
    /// then give up — a third attempt has never produced a different outcome.
    pub async fn complete_json(&self, messages: &[ChatMessage]) -> Result<Value> {
        let txt = self.complete(messages, 0.0).await?;
        if let Some(v) = extract_json(&txt) {
            return Ok(v);
        }
        let mut again = messages.to_vec();
        again.push(ChatMessage::assistant(txt.clone()));
        again.push(ChatMessage::user(
            "That was not valid JSON. Reply with the JSON object only — no prose, no code fence.",
        ));
        let txt2 = self.complete(&again, 0.0).await?;
        extract_json(&txt2)
            .ok_or_else(|| anyhow::anyhow!("model did not return JSON: {}", txt2.chars().take(200).collect::<String>()))
    }
}

/// Pull the first JSON object or array out of a reply, tolerating code fences
/// and leading prose.
pub fn extract_json(s: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(s.trim()) {
        return Some(v);
    }
    let bytes = s.as_bytes();
    for (open, close) in [(b'{', b'}'), (b'[', b']')] {
        let Some(start) = bytes.iter().position(|b| *b == open) else { continue };
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        for (i, b) in bytes.iter().enumerate().skip(start) {
            if esc {
                esc = false;
                continue;
            }
            match *b {
                b'\\' if in_str => esc = true,
                b'"' => in_str = !in_str,
                c if c == open && !in_str => depth += 1,
                c if c == close && !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = serde_json::from_str::<Value>(&s[start..=i]) {
                            return Some(v);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

// ── embeddings ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Embedder {
    url: String,
    model: String,
    key: Option<String>,
    http: reqwest::Client,
    pub dims: usize,
}

impl Embedder {
    /// `None` when the brain is configured `embed.model: none` — then the whole
    /// vector half of search is skipped, no provider is ever contacted, and
    /// `search` degrades to keyword-only. That is the mode the Panama benchmark
    /// runs in, matching brain1's `--no-embed`.
    pub fn new(cfg: &crate::config::EmbedConfig) -> Option<Embedder> {
        if cfg.model.trim().is_empty() || cfg.model == "none" {
            return None;
        }
        let model = cfg.model.split_once(':').map(|(_, m)| m).unwrap_or(&cfg.model).to_string();
        Some(Embedder {
            url: cfg.url.clone(),
            model,
            key: config::api_key("embed", cfg.api_key.as_deref()),
            http: client().ok()?,
            dims: cfg.dims,
        })
    }

    /// Embed a batch in one request. The endpoint accepts an array for `input`,
    /// which is worth roughly an order of magnitude on an index build versus
    /// one request per entity.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let body = json!({ "model": self.model, "input": texts });
        let res = auth(self.http.post(&self.url), &self.key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {} (embedding provider)", self.url))?;
        let status = res.status();
        let txt = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("embeddings {}: {}", status.as_u16(), txt.chars().take(300).collect::<String>());
        }
        let v: Value = serde_json::from_str(&txt).context("embeddings returned non-JSON")?;
        let arr = v["data"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            let vec: Vec<f32> = item["embedding"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect())
                .unwrap_or_default();
            out.push(vec);
        }
        if out.len() != texts.len() {
            bail!("embeddings: asked for {} vectors, got {}", texts.len(), out.len());
        }
        Ok(out)
    }

    /// Probe the real dimensionality, so `brain.yaml` does not have to be
    /// right about it.
    pub async fn probe_dims(&self) -> Result<usize> {
        let v = self.embed(&["dimension probe".to_string()]).await?;
        v.first().map(|x| x.len()).filter(|n| *n > 0).context("provider returned an empty vector")
    }
}
