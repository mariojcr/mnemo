// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Token usage of a Codex turn. The transcript is not a stable API: never make up zeros.

use super::TurnUsage;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Tokens {
    input: u64,
    cached: u64,
    written: u64,
    output: u64,
}

impl Tokens {
    fn read(value: &Value) -> Option<Self> {
        Some(Self {
            input: value["input_tokens"].as_u64()?,
            cached: value["cached_input_tokens"].as_u64().unwrap_or(0),
            written: value["cache_write_input_tokens"].as_u64().unwrap_or(0),
            output: value["output_tokens"].as_u64()?,
        })
    }

    fn since(self, previous: Self) -> Self {
        Self {
            input: self.input.saturating_sub(previous.input),
            cached: self.cached.saturating_sub(previous.cached),
            written: self.written.saturating_sub(previous.written),
            output: self.output.saturating_sub(previous.output),
        }
    }

    fn add_to(self, usage: &mut TurnUsage) {
        // Codex counts cache in input and reasoning in output. Mnemo splits out the cache.
        usage.input += self
            .input
            .saturating_sub(self.cached)
            .saturating_sub(self.written);
        usage.cache_read += self.cached;
        usage.cache_creation += self.written;
        usage.output += self.output;
        usage.calls += 1;
    }
}

pub(super) fn turn_usage(transcript: &str, input: &Value) -> Option<TurnUsage> {
    let turn_id = input["turn_id"].as_str().filter(|id| !id.is_empty())?;
    let session_id = input["session_id"].as_str()?;
    let mut usage = TurnUsage {
        model: input["model"].as_str().unwrap_or("").into(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    // Another turn may already follow: never take "the last one" blindly.
    for line in transcript.lines().rev() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "turn_context" && payload["turn_id"] == turn_id && usage.model.is_empty()
        {
            usage.model = payload["model"].as_str().unwrap_or("").into();
        }
        if row["type"] != "token_usage_record" || payload["turn_id"] != turn_id {
            continue;
        }
        // Skip usage from subagents that share the root_turn_id.
        if payload["session_id"]
            .as_str()
            .is_some_and(|id| id != session_id)
            || payload["thread_id"]
                .as_str()
                .is_some_and(|id| id != session_id)
        {
            continue;
        }
        let Some(id) = payload["response_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if let Some(tokens) = Tokens::read(&payload["usage"]) {
            if seen.insert(id.to_string()) {
                tokens.add_to(&mut usage);
            }
        }
    }
    if usage.calls > 0 {
        return Some(usage);
    }

    // Older rollout format: rate-limit notices repeat token_count, and last_token_usage is not
    // incremental. Only the growth of the total during THIS turn counts.
    let mut active = false;
    let mut previous = Tokens::default();
    for line in transcript.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "turn_context"
            || (row["type"] == "event_msg" && payload["type"] == "task_started")
        {
            active = payload["turn_id"] == turn_id;
        }
        if row["type"] != "event_msg" || payload["type"] != "token_count" {
            continue;
        }
        if let Some(total) = Tokens::read(&payload["info"]["total_token_usage"]) {
            let delta = total.since(previous);
            if active && delta != Tokens::default() {
                delta.add_to(&mut usage);
            }
            previous = total;
        }
    }
    (usage.calls > 0).then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> Value {
        json!({"session_id":"session", "turn_id":"wanted", "model":"codex-model"})
    }

    fn record(turn: &str, response: &str, tokens: Value) -> Value {
        json!({"type":"token_usage_record", "payload":{
            "session_id":"session", "thread_id":"session", "turn_id":turn,
            "response_id":response, "usage":tokens
        }})
    }

    fn transcript(rows: &[Value]) -> String {
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn records_select_the_requested_turn_and_latest_response_usage() {
        let tokens = json!({"input_tokens":100, "cached_input_tokens":60,
            "cache_write_input_tokens":10, "output_tokens":20, "reasoning_output_tokens":8});
        let mut child = record("wanted", "child", tokens.clone());
        child["payload"]["session_id"] = json!("child-session");
        let rows = [
            record("previous", "before", tokens.clone()),
            record("wanted", "a", json!({"input_tokens":1,"output_tokens":1})),
            record("wanted", "a", tokens.clone()),
            child,
            record("wanted", "b", tokens),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{
                "total_token_usage":{"input_tokens":9999,"output_tokens":9999}
            }}}),
            record(
                "next",
                "after",
                json!({"input_tokens":9999,"output_tokens":9999}),
            ),
        ];
        assert_eq!(
            turn_usage(&transcript(&rows), &input()),
            Some(TurnUsage {
                model: "codex-model".into(),
                input: 60,
                cache_read: 120,
                cache_creation: 20,
                output: 40,
                calls: 2,
            })
        );
    }

    #[test]
    fn legacy_totals_exclude_previous_turns_and_duplicate_notifications() {
        let start =
            |turn| json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}});
        let count = |i, c, o| {
            json!({"type":"event_msg","payload":{"type":"token_count","info":{
                "total_token_usage":{"input_tokens":i,"cached_input_tokens":c,"output_tokens":o}
            }}})
        };
        let rows = [
            start("old"),
            count(1000, 500, 100),
            start("wanted"),
            count(1100, 560, 120),
            count(1100, 560, 120),
            count(1300, 660, 150),
            start("next"),
            count(9000, 8000, 1000),
        ];
        assert_eq!(
            turn_usage(&transcript(&rows), &input()),
            Some(TurnUsage {
                model: "codex-model".into(),
                input: 140,
                cache_read: 160,
                cache_creation: 0,
                output: 50,
                calls: 2,
            })
        );
    }

    #[test]
    fn unknown_usage_is_not_reported_as_zero_and_model_has_a_fallback() {
        assert_eq!(turn_usage("not json", &input()), None);
        assert_eq!(
            turn_usage(
                &record("other", "a", json!({"input_tokens":1,"output_tokens":1})).to_string(),
                &input()
            ),
            None
        );
        let rows = [
            json!({"type":"turn_context","payload":{"turn_id":"wanted","model":"actual-model"}}),
            record(
                "wanted",
                "a",
                json!({"input_tokens":10,"cached_input_tokens":20,"output_tokens":5}),
            ),
        ];
        let mut event = input();
        event["model"] = Value::Null;
        let usage = turn_usage(&transcript(&rows), &event).unwrap();
        assert_eq!(usage.model, "actual-model");
        assert_eq!(usage.input, 0);
        event["turn_id"] = json!("");
        assert_eq!(turn_usage(&transcript(&rows), &event), None);
    }
}
