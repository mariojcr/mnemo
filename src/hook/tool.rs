// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The search before a tool runs (PreToolUse). An agent needs what it knew most often in the middle
//! of the work, not when the person writes: on 56 needs from real sessions, the memory that was
//! needed came in the top 3 for 3 when searching with the prompt and for 16 with the turn's tool
//! calls. The query is what the call says it is for: a command's description (never the command),
//! a path, a pattern, a URL.

use serde_json::Value;

/// Shorter queries find noise.
const MIN_QUERY_CHARS: usize = 6;

/// What the call is about, or None when it says nothing a memory could answer.
pub fn query(input: &Value) -> Option<String> {
    // Inside a subagent: it was briefed by the main conversation.
    if input["agent_id"].as_str().is_some_and(|a| !a.is_empty()) {
        return None;
    }
    let i = &input["tool_input"];
    let f = |k: &str| i[k].as_str().unwrap_or("").trim().to_string();
    let q = match input["tool_name"].as_str().unwrap_or("") {
        "Bash" => f("description"),
        "Read" | "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => f("file_path"),
        "Grep" => format!("{} in {}", f("pattern"), f("path"))
            .trim()
            .to_string(),
        "Glob" => f("pattern"),
        "WebSearch" => f("query"),
        "WebFetch" => f("url"),
        "Agent" | "Task" => f("description"),
        _ => String::new(),
    };
    (q.chars().count() >= MIN_QUERY_CHARS).then_some(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_query_is_what_the_call_is_for() {
        let q = |v: Value| query(&v);
        assert_eq!(
            q(json!({"tool_name": "Bash", "tool_input": {"command": "curl -H 'Authorization: x' …", "description": "Check the gateway health"}})).as_deref(),
            Some("Check the gateway health"),
            "the description, never the command"
        );
        assert_eq!(
            q(json!({"tool_name": "Read", "tool_input": {"file_path": "/r/Nomad/dns.tf"}}))
                .as_deref(),
            Some("/r/Nomad/dns.tf")
        );
        assert_eq!(
            q(json!({"tool_name": "Grep", "tool_input": {"pattern": "REL_MIN", "path": "src"}}))
                .as_deref(),
            Some("REL_MIN in src")
        );
        for none in [
            json!({"tool_name": "TodoWrite", "tool_input": {"todos": []}}),
            json!({"tool_name": "mcp__plugin_mnemo_mnemo__recall", "tool_input": {"query": "gateway"}}),
            json!({"tool_name": "Bash", "tool_input": {"description": "ls"}}),
            json!({"tool_name": "Bash", "agent_id": "a1", "tool_input": {"description": "Check the gateway health"}}),
        ] {
            assert_eq!(q(none.clone()), None, "{none}");
        }
    }
}
