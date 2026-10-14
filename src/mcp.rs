// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! MCP at `POST /mcp`; `mnemo mcp` only bridges stdio to it, so no logic is duplicated.
//! Tool descriptions and results are in English: a model reads them.

use crate::app::{App, RememberReq};
use crate::auth::Principal;
use crate::ops::{self, OpError, RecallInput};
use axum::{
    Extension, Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::sync::Arc;

const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub const INSTRUCTIONS: &str = "mnemo is a long-term memory shared across conversations, tools and agents. Memories answer what, who, when, where, why and how, and cite their sources. Use `recall` before investigating something that may have been looked at before, `remember` for conclusions worth keeping, `forget` when something stops being true, and `recall_episode` to read the original record behind a citation.";

pub fn tools() -> Value {
    json!([
        {
            "name": "recall",
            "description": "Search long-term memory: what was learned in earlier conversations, sessions and runs, as memories that answer what, who, when, where, why and how, plus excerpts of the original records. Ask in natural language and name the people, systems, files and dates you care about; ask why/how/who/when/where directly. Results cite [m:<id>] memories and [e:<id>] original records.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "The question in natural language."},
                "limit": {"type": "integer", "description": "How many memories at most (default 10)."},
                "as_of": {"type": "string", "description": "YYYY-MM-DD: only what was true on that day."},
                "include_raw": {"type": "boolean", "description": "Also search the original records, not only distilled memories."}
            }, "required": ["query"]}
        },
        {
            "name": "remember",
            "description": "Save something worth knowing in future conversations: a durable fact, a decision and its reason, the cause of a problem and its fix, a procedure or a preference. Conversations sent to mnemo are already distilled automatically; use this when asked to remember something or when you reach a conclusion worth keeping. Fill why/how only when they were stated. Use kind=state for things that are true now but transient.",
            "inputSchema": {"type": "object", "properties": {
                "what": {"type": "string", "description": "What happened or is true, as one self-contained sentence in English. Name people and systems."},
                "why": {"type": "string", "description": "The reason or cause, only if it was stated."},
                "how": {"type": "string", "description": "The mechanism, fix or procedure, only if it was stated."},
                "kind": {"type": "string", "enum": ["fact", "event", "decision", "preference", "procedure", "state"]},
                "who": {"type": "array", "items": {"type": "string"}, "description": "People, teams or accounts involved, by name."},
                "where": {"type": "array", "items": {"type": "string"}, "description": "Systems, environments, repos, files, channels or projects, by name."},
                "when": {"type": "string", "description": "When it happened, YYYY-MM-DD, if known."},
            }, "required": ["what"]}
        },
        {
            "name": "forget",
            "description": "Mark a memory as no longer true. It is not erased: it stays in the history as what was true until now.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string", "description": "The memory id as cited: m:<id> or just <id>."},
                "reason": {"type": "string", "description": "Why it is no longer true."}
            }, "required": ["id"]}
        },
        {
            "name": "keep",
            "description": "Keep something you read or observed so it is really stored and found again later: a web page or document you read (kind snapshot, with its useful text), or an event about a thing (an alert firing or resolving, a deploy starting or finishing). The key identifies the thing (its URL, the alert name, the job): keeping the same key again adds an event or a new version, never a duplicate, and an unchanged page is not stored twice.",
            "inputSchema": {"type": "object", "properties": {
                "key": {"type": "string", "description": "What identifies the thing: a URL, an alert name, a job or PR id."},
                "title": {"type": "string", "description": "A readable name for it."},
                "text": {"type": "string", "description": "The content worth keeping (for a page, its useful text; for an event, what happened)."},
                "kind": {"type": "string", "enum": ["snapshot", "event"], "description": "snapshot (default) for content, event for something that happened."},
                "state": {"type": "string", "enum": ["open", "close", "point"], "description": "For events: open (starts, fires), close (ends, resolves) or point."},
                "status": {"type": "string", "description": "The source's own label, e.g. firing, resolved, merged."},
                "stream": {"type": "string", "description": "Where it comes from, e.g. web, slack:#oncall, github:org/repo (default web for snapshots, agent for events)."},
                "when": {"type": "string", "description": "When it happened, ISO 8601, if not now."}
            }, "required": ["key", "text"]}
        },
        {
            "name": "recall_episode",
            "description": "Open an original record cited as [e:<id>], with the records around it in the same conversation or run.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string", "description": "The record id as cited: e:<id> or just <id>."},
                "around": {"type": "integer", "description": "How many records before and after (default 3)."}
            }, "required": ["id"]}
        }
    ])
}

fn day_ms(s: &str) -> Option<i64> {
    crate::query::parse_iso(s.trim()).filter(|_| s.trim().len() >= 10)
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

pub async fn call_tool(
    app: &App,
    p: &Principal,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let fail = |e: OpError| e.to_string();
    match name {
        "recall" => {
            let query = s(args, "query");
            if query.trim().is_empty() {
                return Err("query is required".into());
            }
            let limit = args
                .get("limit")
                .and_then(|x| x.as_u64())
                .map(|x| x.clamp(1, 30) as usize)
                .unwrap_or(10);
            let as_of = day_ms(s(args, "as_of")).map(|t| t + crate::query::DAY_MS - 1);
            let raw = if args
                .get("include_raw")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
            {
                "on"
            } else {
                "auto"
            };
            let out = ops::recall(
                app,
                p,
                RecallInput {
                    query: query.into(),
                    surface: Some("mcp".into()),
                    budget_tokens: Some(2500),
                    limit: Some(limit),
                    as_of,
                    raw: Some(raw.into()),
                    deadline_ms: Some(3000),
                    ..Default::default()
                },
            )
            .await
            .map_err(fail)?;
            let text = out["text"].as_str().unwrap_or("").to_string();
            Ok(if text.trim().is_empty() {
                "No relevant memories.".into()
            } else {
                text
            })
        }
        "keep" => {
            let kind = match s(args, "kind") {
                "" => "snapshot",
                k => k,
            };
            let stream = match (s(args, "stream"), kind) {
                ("", "snapshot") => "web",
                ("", _) => "agent",
                (x, _) => x,
            };
            let item = ops::KeepItem {
                record: crate::records::Record {
                    stream: stream.into(),
                    key: s(args, "key").into(),
                    kind: kind.into(),
                    state: s(args, "state").into(),
                    status: s(args, "status").into(),
                    at: crate::query::parse_iso(s(args, "when")).unwrap_or(0),
                    title: s(args, "title").into(),
                    text: s(args, "text").into(),
                    ..Default::default()
                },
                thing_kind: String::new(),
            };
            let out = ops::keep(app, p, vec![item], None).await.map_err(fail)?;
            Ok(if out["stored"].as_u64() == Some(0) {
                "Already kept: nothing new (same event, or the page has not changed).".to_string()
            } else {
                format!(
                    "Kept. It will come back when you recall {}.",
                    s(args, "key")
                )
            })
        }
        "remember" => {
            let what = s(args, "what");
            if what.trim().is_empty() {
                return Err("what is required".into());
            }
            let opt = |k: &str| Some(s(args, k).to_string()).filter(|x| !x.trim().is_empty());
            let req = RememberReq {
                what: what.to_string(),
                why: opt("why"),
                how: opt("how"),
                kind: opt("kind"),
                who: strs(args, "who"),
                where_: strs(args, "where"),
                occurred_at: day_ms(s(args, "when")),
                ..Default::default()
            };
            let out = ops::remember(app, p, req).await.map_err(fail)?;
            Ok(match out.outcome.as_str() {
                "duplicate" => format!(
                    "Already known as [m:{}]; this was added as a source.",
                    out.id
                ),
                "superseded" => format!(
                    "Saved as [m:{}]. It replaces {}, now marked as no longer true.",
                    out.id,
                    out.closed
                        .iter()
                        .map(|c| format!("[m:{c}]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => format!("Saved as [m:{}].", out.id),
            })
        }
        "forget" => {
            let id = s(args, "id").trim().trim_start_matches("m:").to_string();
            if id.is_empty() {
                return Err("id is required".into());
            }
            match ops::forget(app, p, &id, &[], s(args, "reason")).await {
                Ok(()) => Ok(format!("[m:{id}] is now marked as no longer true.")),
                Err(OpError::NotFound(_)) => {
                    Ok(format!("No current memory [m:{id}] that you can change."))
                }
                Err(e) => Err(e.to_string()),
            }
        }
        "recall_episode" => {
            let id = s(args, "id").trim().trim_start_matches("e:").to_string();
            if id.is_empty() {
                return Err("id is required".into());
            }
            let around = args
                .get("around")
                .and_then(|x| x.as_u64())
                .map(|x| x.clamp(1, 10) as u32)
                .unwrap_or(3);
            match ops::episode(app, p, &id, &[], around).await {
                Ok(v) => {
                    let mut out = String::new();
                    for ep in v["episodes"].as_array().into_iter().flatten() {
                        let mark = if ep["episode_id"] == id.as_str() {
                            ">"
                        } else {
                            " "
                        };
                        let text: String = ep["text"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(4000)
                            .collect();
                        let when = crate::episode::fmt_ts(ep["occurred_at"].as_i64().unwrap_or(0));
                        out.push_str(&format!(
                            "{mark} [e:{}] {} {}: {}\n",
                            ep["episode_id"].as_str().unwrap_or(""),
                            when,
                            ep["role"].as_str().unwrap_or(""),
                            text
                        ));
                    }
                    Ok(out)
                }
                Err(OpError::NotFound(_)) => Ok(format!("No record [e:{id}] visible to you.")),
                Err(e) => Err(e.to_string()),
            }
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

fn rpc_error(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// Writing tools: on a replica the writer must handle them, so a new one must be listed here.
pub fn writes(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|m| {
        m["method"] == "tools/call"
            && matches!(
                m["params"]["name"].as_str(),
                Some("remember" | "forget" | "keep")
            )
    })
}

pub async fn handle(app: &App, p: &Principal, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let version = if VERSIONS.contains(&asked) {
                asked
            } else {
                VERSIONS[0]
            };
            json!({"protocolVersion": version, "capabilities": {"tools": {}}, "serverInfo": {"name": "mnemo", "version": env!("CARGO_PKG_VERSION")}, "instructions": INSTRUCTIONS})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !tools()
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == name)
            {
                return Some(rpc_error(&id, -32602, &format!("unknown tool: {name}")));
            }
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(app, p, name, &args).await {
                Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        _ => {
            return Some(rpc_error(
                &id,
                -32601,
                &format!("method not found: {method}"),
            ));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub async fn http(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(msg): Json<Value>,
) -> Response {
    // No batches: the 2025-06-18 transport dropped them.
    match handle(&app, &p, &msg).await {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// No server event stream: the spec allows rejecting the transport's GET.
pub async fn http_get() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

/// Logs go to stderr: stdout carries the protocol.
pub async fn stdio_bridge(c: &crate::client::Client) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let base = c.base.as_str();
    let http = reqwest::Client::new();
    let url = format!("{}/mcp", base.trim_end_matches('/'));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let reply = rpc_error(&Value::Null, -32700, &format!("parse error: {e}"));
                out.write_all(format!("{reply}\n").as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let mut req = http.post(&url).json(&msg);
        if let Some(t) = c.auth().await {
            req = req.bearer_auth(t);
        }
        let reply = match req.send().await {
            Ok(r) if r.status() == StatusCode::ACCEPTED => None,
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                id.as_ref().map(|id| {
                    rpc_error(
                        id,
                        -32000,
                        &format!(
                            "mnemo {status}: {}",
                            body.chars().take(300).collect::<String>()
                        ),
                    )
                })
            }
            Err(e) => id.as_ref().map(|id| {
                rpc_error(
                    id,
                    -32000,
                    &format!("mnemo is not responding at {base}: {e}"),
                )
            }),
        };
        if let Some(v) = reply {
            out.write_all(format!("{v}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            out.flush().await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Cfg;
    use crate::auth::Kind;
    use crate::judge::Judge;
    use crate::storage::RawStore;
    use crate::storage::mem::Mem;

    async fn app() -> App {
        let db = Arc::new(Mem::new());
        App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            Judge::Heuristic,
            Cfg::default(),
        )
    }

    #[test]
    fn only_write_tools_go_to_the_writer() {
        assert!(writes(
            br#"{"method":"tools/call","params":{"name":"remember"}}"#
        ));
        assert!(writes(
            br#"{"method":"tools/call","params":{"name":"forget"}}"#
        ));
        assert!(!writes(
            br#"{"method":"tools/call","params":{"name":"recall"}}"#
        ));
        assert!(!writes(br#"{"method":"tools/list"}"#));
        assert!(!writes(b"not json"));
    }

    #[tokio::test]
    async fn speaks_mcp_end_to_end() {
        let a = app().await;
        let p = Principal {
            kind: Kind::User,
            subject: "ana".into(),
            ..Default::default()
        };
        let init = handle(&a, &p, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).await.unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "mnemo");
        assert!(
            handle(
                &a,
                &p,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .await
            .is_none(),
            "a notification gets no reply"
        );
        let list = handle(
            &a,
            &p,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await
        .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);
        let saved = handle(&a, &p, &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "remember", "arguments": {"what": "The staging database is rebuilt every Monday", "why": "To keep test data fresh"}}})).await.unwrap();
        assert_eq!(saved["result"]["isError"], false, "{saved}");
        let text = saved["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("Saved as [m:"), "{text}");
        let found = handle(&a, &p, &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "recall", "arguments": {"query": "why is staging rebuilt?"}}})).await.unwrap();
        assert!(
            found["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("To keep test data fresh"),
            "{found}"
        );
        let bad = handle(&a, &p, &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "remember", "arguments": {}}})).await.unwrap();
        assert_eq!(bad["result"]["isError"], true);
        let unknown = handle(
            &a,
            &p,
            &json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "nope"}}),
        )
        .await
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(
            handle(
                &a,
                &p,
                &json!({"jsonrpc": "2.0", "id": 7, "method": "nope"})
            )
            .await
            .unwrap()["error"]["code"],
            -32601
        );
    }

    #[tokio::test]
    async fn a_page_read_is_kept_once_and_recalled_with_its_content() {
        let a = app().await;
        let p = Principal {
            kind: Kind::User,
            subject: "ana".into(),
            ..Default::default()
        };
        let call = |id: i64, name: &str, args: Value| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}});
        let page = json!({"key": "https://docs.example.com/nomad/timeouts/", "title": "Nomad job timeouts", "text": "Migration jobs may run for up to 30 minutes; every other job keeps the 10 minute timeout."});
        let first = handle(&a, &p, &call(1, "keep", page.clone()))
            .await
            .unwrap();
        let t = first["result"]["content"][0]["text"].as_str().unwrap();
        assert!(t.starts_with("Kept."), "{first}");
        let again = handle(&a, &p, &call(2, "keep", page)).await.unwrap();
        assert!(
            again["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Already kept"),
            "read again unchanged: {again}"
        );
        let found = handle(
            &a,
            &p,
            &call(
                3,
                "recall",
                json!({"query": "how long can migration jobs run in Nomad?"}),
            ),
        )
        .await
        .unwrap();
        let text = found["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("Latest saved version") && text.contains("30 minutes"),
            "{text}"
        );
        let by_url = handle(
            &a,
            &p,
            &call(
                4,
                "recall",
                json!({"query": "docs.example.com/nomad/timeouts"}),
            ),
        )
        .await
        .unwrap();
        assert!(
            by_url["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Nomad job timeouts"),
            "{by_url}"
        );
        let fire = json!({"key": "gw-latency", "kind": "event", "state": "open", "status": "firing", "text": "p99 above 2s", "stream": "slack:#oncall", "title": "Gateway latency"});
        handle(&a, &p, &call(5, "keep", fire)).await.unwrap();
        let st = a
            .things
            .read()
            .unwrap()
            .values()
            .find(|s| s.key == "gw-latency")
            .cloned()
            .unwrap();
        assert!(st.open_since.is_some() && st.events == 1);
    }
}
