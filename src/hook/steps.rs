// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Agent steps as episodes, opt-in with `MNEMO_STEPS=1`: what each tool was asked and the start of
//! its answer, and the agent's notes between tools. In an audit of real sessions, 20 of 54 facts an
//! agent needed and had never stored were in an earlier tool output or note.
//!
//! Tool outputs carry secrets far more often than prompts (`cat .env`, a token in a header), so
//! everything is masked here, on the person's machine, before it is sent.

use serde_json::Value;

/// Tools whose calls say nothing about the work itself.
const SKIP: [&str; 17] = [
    "ToolSearch",
    "TodoWrite",
    "TaskOutput",
    "TaskStop",
    "Skill",
    "Monitor",
    "ScheduleWakeup",
    "CronCreate",
    "CronDelete",
    "CronList",
    "ReadNotifications",
    "SendMessage",
    "ListAgents",
    "PushNotification",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
];

const NOTE_CHARS: usize = 1500;

pub fn enabled() -> bool {
    std::env::var("MNEMO_STEPS").is_ok_and(|v| v.trim() == "1")
}

fn out_chars(tool: &str) -> usize {
    match tool {
        "Agent" | "Task" => 2500,
        "WebFetch" => 1500,
        "WebSearch" => 1200,
        _ => 800,
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn call_line(name: &str, i: &Value) -> String {
    let f = |k: &str| i[k].as_str().unwrap_or("").to_string();
    match name {
        "Bash" => format!("Bash: {}", f("description")),
        "Read" | "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => {
            format!("{name}: {}", f("file_path"))
        }
        "Grep" => format!("Grep: {} in {}", f("pattern"), f("path")),
        "Glob" => format!("Glob: {}", f("pattern")),
        "WebSearch" => format!("WebSearch: {}", f("query")),
        "WebFetch" => format!("WebFetch: {}", f("url")),
        "Agent" | "Task" => format!("{name}: {}", f("description")),
        _ => name.to_string(),
    }
}

fn parts_text(a: &[Value]) -> String {
    a.iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the tool answered, whatever its shape.
fn response_text(r: &Value) -> String {
    match r {
        Value::String(s) => s.clone(),
        Value::Array(a) => parts_text(a),
        Value::Object(o) => {
            let std: Vec<&str> = ["stdout", "stderr"]
                .iter()
                .filter_map(|k| o.get(*k).and_then(|v| v.as_str()))
                .filter(|s| !s.trim().is_empty())
                .collect();
            if !std.is_empty() {
                return std.join("\n");
            }
            match (o.get("content"), o.get("file"), o.get("output")) {
                (Some(Value::String(s)), ..) => s.clone(),
                (Some(Value::Array(a)), ..) => parts_text(a),
                (_, Some(f), _) if f["content"].is_string() => {
                    f["content"].as_str().unwrap_or("").to_string()
                }
                (_, _, Some(Value::String(s))) => s.clone(),
                _ => r.to_string(),
            }
        }
        _ => String::new(),
    }
}

/// The episode text for one PostToolUse event, or None if it is not worth keeping.
pub fn step(input: &Value) -> Option<String> {
    // Inside a subagent: its result comes back to the main conversation through the Agent tool.
    if input["agent_id"].as_str().is_some_and(|a| !a.is_empty()) {
        return None;
    }
    let name = input["tool_name"].as_str().unwrap_or("");
    if name.is_empty() || SKIP.contains(&name) || name.starts_with("mcp__plugin_mnemo") {
        return None;
    }
    let mut text = format!("[tool] {}", call_line(name, &input["tool_input"]));
    // An edit is in the repository already; only where it went matters.
    if !matches!(name, "Edit" | "Write" | "NotebookEdit" | "MultiEdit") {
        let out = mask(response_text(&input["tool_response"]).trim());
        if !out.is_empty() {
            text.push('\n');
            text.push_str(&clip(&out, out_chars(name)));
        }
    }
    Some(mask(&text))
}

/// The main conversation's messages of the last turn, except the final one (`last`, stored as the
/// answer). The transcript may lag behind: the final message is recognised by its text.
pub fn notes(transcript: &str, last: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for line in transcript.lines().rev() {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if d["isSidechain"] == true || d["isMeta"] == true {
            continue;
        }
        let content = &d["message"]["content"];
        match d["type"].as_str() {
            Some("user") => {
                let tool_result = content
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["type"] == "tool_result"));
                if !tool_result {
                    break;
                }
            }
            Some("assistant") => {
                let t = content
                    .as_array()
                    .map(|a| parts_text(a))
                    .unwrap_or_default();
                if !t.trim().is_empty() {
                    texts.push(t);
                }
            }
            _ => {}
        }
    }
    texts.reverse();
    texts
        .iter()
        .filter(|t| t.trim() != last.trim())
        .map(|t| format!("[agent note] {}", clip(&mask(t.trim()), NOTE_CHARS)))
        .collect()
}

const MASK: &str = "<masked>";

const KEYS: [&str; 12] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
    "credential",
    "authorization",
    "bearer",
];

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '=')
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// A run that looks like a key rather than a word, a name or an id people read.
fn secret_like(s: &str) -> bool {
    if is_uuid(s) || (s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())) {
        return false; // ids and git commits: what the memory is for
    }
    let (mut upper, mut lower, mut digit) = (false, false, false);
    for c in s.chars() {
        upper |= c.is_ascii_uppercase();
        lower |= c.is_ascii_lowercase();
        digit |= c.is_ascii_digit();
    }
    (s.len() >= 16 && upper && lower && digit) || (s.len() >= 32 && digit && (upper || lower))
}

/// Masks private keys, values after a secret-looking name, and key-like strings.
pub fn mask(s: &str) -> String {
    let mut s = s.to_string();
    // PEM private keys, whole.
    while let Some(start) = s.find("-----BEGIN") {
        let Some(head_end) = s[start + 10..].find("-----").map(|i| start + 10 + i + 5) else {
            break;
        };
        if !s[start..head_end].contains("PRIVATE KEY") {
            // Not a key (a certificate): leave it, look further.
            let rest = mask(&s[head_end..]);
            return format!("{}{}", &s[..head_end], rest);
        }
        let end = s[head_end..]
            .find("-----END")
            .and_then(|i| {
                let e = head_end + i + 8;
                s[e..].find("-----").map(|j| e + j + 5)
            })
            .unwrap_or(s.len());
        s.replace_range(start..end, MASK);
    }
    // name=value, name: value, "name": "value", Bearer value.
    let lower = s.to_ascii_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for key in KEYS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(key) {
            let after = from + i + key.len();
            from = after;
            // Rest of the name (PASSWORD_FILE, tokens…), then quotes, spaces and one separator.
            let mut j = after;
            let b = s.as_bytes();
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
                j += 1;
            }
            let mut k = j;
            while k < b.len() && matches!(b[k], b'"' | b'\'' | b' ' | b'\t') {
                k += 1;
            }
            let sep = k < b.len() && matches!(b[k], b'=' | b':');
            if sep {
                k += 1;
            } else if key != "bearer" {
                continue;
            }
            while k < b.len() && matches!(b[k], b'"' | b'\'' | b' ' | b'\t') {
                k += 1;
            }
            // "Authorization: Bearer x": the scheme is not the secret.
            if lower[k..].starts_with("bearer ") || lower[k..].starts_with("basic ") {
                k = lower[k..].find(' ').map(|i| k + i + 1).unwrap_or(k);
            }
            let mut e = k;
            while e < b.len()
                && !matches!(
                    b[e],
                    b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'\r' | b',' | b';'
                )
            {
                e += 1;
            }
            if e > k {
                cuts.push((k, e));
            }
        }
    }
    cuts.sort();
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for (a, b) in cuts {
        if a < at {
            continue;
        }
        out.push_str(&s[at..a]);
        out.push_str(MASK);
        at = b;
    }
    out.push_str(&s[at..]);
    // Key-like runs anywhere else.
    let mut res = String::with_capacity(out.len());
    let mut run = String::new();
    let flush = |run: &mut String, res: &mut String| {
        if secret_like(run) {
            res.push_str(MASK);
        } else {
            res.push_str(run);
        }
        run.clear();
    };
    for c in out.chars() {
        if token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut res);
            res.push(c);
        }
    }
    flush(&mut run, &mut res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_masked_and_ids_are_kept() {
        let text = "export CLICKHOUSE_PASSWORD=s3cr3t-value\nAuthorization: Bearer abc.def\n\
            {\"api_key\": \"k-123\"}\ntoken ghp_A1b2C3d4E5f6G7h8I9j0KlMnOpQrStUv\n\
            hex 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08\n\
            id 54b554da-5563-4972-a3e9-6170a0fc485d commit 941e44c0123456789abcdef0123456789abcdef0\n\
            path /home/sysdev/Proyectos/altherium-nomad/scripts/connect.sh on 10.10.100.12:8123";
        let m = mask(text);
        for leak in ["s3cr3t", "abc.def", "k-123", "ghp_A1b2", "9f86d0818"] {
            assert!(!m.contains(leak), "{leak} leaked: {m}");
        }
        for kept in [
            "CLICKHOUSE_PASSWORD=",
            "54b554da-5563-4972-a3e9-6170a0fc485d",
            "941e44c0123456789abcdef0123456789abcdef0",
            "/home/sysdev/Proyectos/altherium-nomad/scripts/connect.sh",
            "10.10.100.12:8123",
        ] {
            assert!(m.contains(kept), "{kept} lost: {m}");
        }
    }

    #[test]
    fn private_keys_go_whole_and_certificates_stay() {
        let m = mask(
            "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\nNzaC1\n-----END OPENSSH PRIVATE KEY-----\nz",
        );
        assert_eq!(m, "a\n<masked>\nz");
        let c = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(mask(c), c);
    }

    #[test]
    fn a_step_says_what_was_asked_and_the_start_of_the_answer() {
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "nomad status", "description": "List Nomad jobs"},
            "tool_response": {"stdout": "mnemo running\nDB_PASSWORD=hunter2", "stderr": ""}});
        assert_eq!(
            step(&bash).unwrap(),
            "[tool] Bash: List Nomad jobs\nmnemo running\nDB_PASSWORD=<masked>"
        );
        let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "/r/src/a.rs"}, "tool_response": {"content": "x"}});
        assert_eq!(step(&edit).unwrap(), "[tool] Edit: /r/src/a.rs");
        let long = json!({"tool_name": "Read", "tool_input": {"file_path": "/r/a"}, "tool_response": {"type": "text", "file": {"content": "y".repeat(5000)}}});
        assert_eq!(
            step(&long).unwrap().chars().count(),
            "[tool] Read: /r/a\n".len() + 800
        );
        for skipped in [
            json!({"tool_name": "TodoWrite", "tool_input": {}}),
            json!({"tool_name": "mcp__plugin_mnemo_mnemo__recall", "tool_input": {}}),
            json!({"tool_name": "Bash", "agent_id": "sub-1", "tool_input": {}}),
        ] {
            assert_eq!(step(&skipped), None, "{skipped}");
        }
    }

    #[test]
    fn notes_are_the_turns_messages_but_the_last() {
        let t = [
            json!({"type": "user", "message": {"content": "old prompt"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "old answer"}]}}),
            json!({"type": "user", "message": {"content": "fix it"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Checking the job."}, {"type": "tool_use", "id": "t"}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "subagent"}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "The port is 8443."}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Done."}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(
            notes(&t, "Done."),
            vec![
                "[agent note] Checking the job.".to_string(),
                "[agent note] The port is 8443.".to_string()
            ]
        );
    }
}
