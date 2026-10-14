// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Inference against any OpenAI-compatible endpoint. Embeddings are computed in-process (onnx.rs).

use serde_json::{Value, json};
use std::time::Duration;

pub type Res<T> = Result<T, String>;

pub const TRUNCATED: &str = "llm /chat/completions: output cut off by max_tokens";

#[derive(Clone)]
pub struct Llm {
    base: String,
    key: String,
    http: reqwest::Client,
    /// llama.cpp turns `json_object` into a grammar: a small model cannot return broken JSON.
    json: bool,
    /// `enable_thinking=false`: Qwen on vLLM thinks by default, which makes extraction several
    /// times slower and can leave the JSON empty.
    no_think: bool,
}

fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

impl Llm {
    /// `base` is the OpenAI root, e.g. `http://localhost:11434/v1`.
    pub fn new(base: &str, key: &str) -> Res<Llm> {
        // No client-wide timeout: it would win over each call's own and cut long extractions.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|x| x.to_string())?;
        Ok(Llm {
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http,
            json: false,
            no_think: false,
        })
    }

    pub fn with_json(mut self, on: bool) -> Llm {
        self.json = on;
        self
    }

    pub fn without_thinking(mut self, on: bool) -> Llm {
        self.no_think = on;
        self
    }

    async fn post(&self, path: &str, body: Value, timeout: Duration) -> Res<Value> {
        let kind = match path {
            "/decisions" => "decide",
            _ => "chat",
        };
        let model = body["model"].as_str().unwrap_or("").to_string();
        let started = std::time::Instant::now();
        let res = self.send(path, body, timeout).await;
        let ms = started.elapsed().as_millis() as u64;
        match &res {
            Ok(v) => crate::meter::llm(kind, &model, v.get("usage"), ms, true),
            Err(_) => crate::meter::llm(kind, &model, None, ms, false),
        }
        res
    }

    async fn send(&self, path: &str, body: Value, timeout: Duration) -> Res<Value> {
        let mut req = self
            .http
            .post(format!("{}{}", self.base, path))
            .timeout(timeout)
            .json(&body);
        if !self.key.is_empty() {
            req = req.bearer_auth(&self.key);
        }
        let resp = req.send().await.map_err(|x| format!("llm {path}: {x}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|x| x.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "llm {path} {}: {}",
                status.as_u16(),
                text.chars().take(400).collect::<String>()
            ));
        }
        serde_json::from_str(&text).map_err(|x| format!("llm {path}: non-JSON response: {x}"))
    }

    /// In JSON mode, `schema` becomes a `json_schema` grammar: a small model given JSON input tends
    /// to copy it instead of answering, and with the schema it can only write the answer.
    pub async fn chat(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
        timeout: Duration,
        schema: Option<(&str, Value)>,
    ) -> Res<String> {
        let mut body = json!({
            "model": model,
            "stream": false,
            "max_tokens": max_tokens,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        });
        if self.no_think {
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
        if self.json {
            // Only here: newer Claude models reject it; JSON extraction should be deterministic.
            body["response_format"] = match schema {
                Some((name, schema)) => {
                    json!({"type": "json_schema", "json_schema": {"name": name, "strict": true, "schema": schema}})
                }
                None => json!({"type": "json_object"}),
            };
            body["temperature"] = json!(0);
        }
        let v = self.post("/chat/completions", body, timeout).await?;
        let content = v
            .pointer("/choices/0/message/content")
            .map(content_text)
            .unwrap_or_default();
        if content.trim().is_empty() {
            return Err("llm /chat/completions: empty response".into());
        }
        // Cut off by `max_tokens`: half a JSON is useless, and the caller can ask for less.
        if v.pointer("/choices/0/finish_reason")
            .and_then(|f| f.as_str())
            == Some("length")
        {
            return Err(TRUNCATED.into());
        }
        Ok(content)
    }

    pub async fn ready(&self, timeout: Duration) -> Res<()> {
        let root = self.base.trim_end_matches("/v1").to_string();
        let mut last = String::new();
        for url in [format!("{}/models", self.base), format!("{root}/health")] {
            let mut req = self.http.get(&url).timeout(timeout);
            if !self.key.is_empty() {
                req = req.bearer_auth(&self.key);
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) => last = format!("{url}: {}", r.status().as_u16()),
                Err(e) => last = format!("{url}: {e}"),
            }
        }
        Err(last)
    }

    pub async fn decide(
        &self,
        model: &str,
        state: Value,
        questions: Value,
        timeout: Duration,
    ) -> Res<Value> {
        let v = self
            .post(
                "/decisions",
                json!({"model": model, "state": state, "questions": questions}),
                timeout,
            )
            .await?;
        v.get("answers")
            .cloned()
            .ok_or_else(|| "llm /decisions: no answers".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_accepts_string_or_parts() {
        assert_eq!(content_text(&json!("hola")), "hola");
        assert_eq!(
            content_text(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
            "ab"
        );
        assert_eq!(content_text(&json!(null)), "");
    }
}
