// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! What memory costs and whether it helps. Rows go to ClickHouse without the writer (no sequence
//! number, so any replica can write them); a metering failure never breaks what is measured.

use crate::storage::Db;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LlmCall {
    pub at: i64,
    /// extract | judge | summary | embed | embed_query | eval | chat
    pub kind: String,
    pub model: String,
    pub subject: String,
    pub thread: String,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
    pub ms: u64,
    pub ok: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Injection {
    pub id: String,
    pub at: i64,
    /// session_start | prompt | mcp | api
    pub surface: String,
    pub subject: String,
    /// Already namespaced by the caller: `<subject>/<id>`.
    pub session: String,
    pub query: String,
    pub memory_ids: Vec<String>,
    pub entity_ids: Vec<String>,
    pub chars: u64,
    pub tokens: u64,
    /// Relevant memories skipped because they were already in the session's context.
    pub repeated: u64,
    pub muted: u64,
    /// A fresh context (new session, `/clear`, compaction): earlier injections are gone.
    pub reset: bool,
    /// Why nothing was injected: empty | budget | nothing
    pub skipped: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub at: i64,
    pub subject: String,
    pub session: String,
    /// claude-code | codex
    pub source: String,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub calls: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Eval {
    pub at: i64,
    pub injection_id: String,
    pub memory_id: String,
    pub session: String,
    pub subject: String,
    pub used: bool,
    /// cited | decisions | llm
    pub method: String,
    pub p: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    pub memory_ids: Vec<String>,
    pub entity_ids: Vec<String>,
    pub tokens: u64,
    /// The last reset, or 0: the session was resumed whole and its entire thread is in front.
    pub since: i64,
}

/// Expects the session's injections in time order.
pub fn window_of(rows: &[Injection]) -> Window {
    let mut w = Window::default();
    let start = rows.iter().rposition(|r| r.reset);
    w.since = start.map(|i| rows[i].at).unwrap_or(0);
    for r in &rows[start.unwrap_or(0)..] {
        w.tokens += r.tokens;
        for id in &r.memory_ids {
            if !w.memory_ids.contains(id) {
                w.memory_ids.push(id.clone());
            }
        }
        for id in &r.entity_ids {
            if !w.entity_ids.contains(id) {
                w.entity_ids.push(id.clone());
            }
        }
    }
    w
}

/// No model tokenizer at hand: ~3.5 characters per token, rounded up so as not to fall short.
pub fn tokens_of(chars: usize) -> u64 {
    (chars as u64 * 2).div_ceil(7)
}

static DB: OnceLock<Arc<dyn Db>> = OnceLock::new();

pub fn install(db: Arc<dyn Db>) {
    let _ = DB.set(db);
}

#[derive(Clone, Debug, Default)]
pub struct Tag {
    pub kind: &'static str,
    pub subject: String,
    pub thread: String,
}

tokio::task_local! {
    static TAG: Tag;
}

pub async fn tagged<F: std::future::Future>(tag: Tag, f: F) -> F::Output {
    TAG.scope(tag, f).await
}

/// Only the kind changes: a judge call inside a distillation still belongs to its thread.
pub fn retag(kind: &'static str) -> Tag {
    let mut t = TAG.try_with(|t| t.clone()).unwrap_or_default();
    t.kind = kind;
    t
}

fn spawn(f: impl std::future::Future<Output = Result<(), String>> + Send + 'static) {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move {
            if let Err(e) = f.await {
                log::warn!("metering: {e}");
            }
        });
    }
}

/// `usage` may come in OpenAI or Anthropic shape (via a gateway): both field names are read.
pub fn llm(
    default_kind: &'static str,
    model: &str,
    usage: Option<&serde_json::Value>,
    ms: u64,
    ok: bool,
) {
    let Some(db) = DB.get().cloned() else { return };
    let tag = TAG.try_with(|t| t.clone()).unwrap_or_default();
    let u = |path: &str| {
        usage
            .and_then(|u| u.pointer(path))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    let prompt = u("/prompt_tokens").max(u("/input_tokens"));
    let cached = u("/prompt_tokens_details/cached_tokens").max(u("/cache_read_input_tokens"));
    let call = LlmCall {
        at: crate::app::now_ms(),
        kind: if tag.kind.is_empty() {
            default_kind
        } else {
            tag.kind
        }
        .to_string(),
        model: model.to_string(),
        subject: tag.subject,
        thread: tag.thread,
        prompt_tokens: prompt,
        cached_tokens: cached,
        cache_creation_tokens: u("/cache_creation_input_tokens")
            .max(u("/prompt_tokens_details/cache_creation_tokens")),
        completion_tokens: u("/completion_tokens").max(u("/output_tokens")),
        ms,
        ok,
    };
    spawn(async move { db.record_llm(&[call]).await });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(tokens_of(0), 0);
        assert_eq!(tokens_of(1), 1);
        assert_eq!(tokens_of(7), 2);
        assert_eq!(tokens_of(3500), 1000);
    }

    #[tokio::test]
    async fn tags_flow_into_nested_work() {
        let t = Tag {
            kind: "extract",
            subject: "ana".into(),
            thread: "t1".into(),
        };
        let inner = tagged(t, async { retag("judge") }).await;
        assert_eq!(
            (inner.kind, inner.subject.as_str(), inner.thread.as_str()),
            ("judge", "ana", "t1")
        );
        assert_eq!(retag("eval").subject, "", "outside a tag there is nobody");
    }
}
