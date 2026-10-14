//! Integration against a running server (`MNEMO_TEST_URL`, admin key in `MNEMO_TEST_KEY`). Without
//! the URL it FAILS rather than skips: a test that talks to nothing proves nothing.
#![cfg(feature = "integration")]

use serde_json::{Value, json};

struct Svc {
    base: String,
    key: String,
    http: reqwest::Client,
}

impl Svc {
    fn new() -> Svc {
        let base = std::env::var("MNEMO_TEST_URL")
            .expect("MNEMO_TEST_URL: start a server with `mnemo serve`");
        Svc {
            base: base.trim_end_matches('/').to_string(),
            key: std::env::var("MNEMO_TEST_KEY").unwrap_or_default(),
            http: reqwest::Client::new(),
        }
    }

    fn with_key(&self, key: &str) -> Svc {
        Svc {
            base: self.base.clone(),
            key: key.to_string(),
            http: self.http.clone(),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if !self.key.is_empty() {
            req = req.bearer_auth(&self.key);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("service request");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, None).await
    }
}

#[tokio::test]
async fn memories_live_die_and_are_found_by_their_six_ws() {
    let s = Svc::new();
    let run = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let owner = format!("it-{run}");
    let scope = format!("user:{owner}");
    let scopes = json!(["global", scope]);
    let gw = format!("system:gw-{run}");
    let lb = format!("system:lb-{run}");

    let (st, _) = s
        .post(
            "/v1/entities",
            json!({"entities": [
                {"id": gw, "scope": scope, "kind": "system", "name": format!("gateway {run}"), "aliases": [format!("gw{run}")]},
                {"id": lb, "scope": scope, "kind": "system", "name": format!("balancer {run}"), "aliases": [format!("lb{run}")], "parent": gw},
            ]}),
        )
        .await;
    assert_eq!(st, 200);

    let base = json!({"scope": scope, "owner_id": owner, "reader_scopes": scopes, "kind": "event",
        "what": format!("Long requests through gw{run} were cut at 120 seconds by lb{run}"),
        "why": "The balancer had timeout_sec set to 120", "where": [format!("lb{run}")], "occurred_at": 1_790_000_000_000i64});
    let (st, first) = s.post("/v1/memories", base.clone()).await;
    assert_eq!(st, 200, "{first}");
    assert_eq!(first["outcome"], "inserted");
    let id = first["id"].as_str().unwrap().to_string();

    let (_, fresh) = s
        .post("/v1/memories", json!({"scope": scope, "owner_id": owner, "reader_scopes": scopes, "what": format!("The nebula{run} cache is warmed every morning"), "where": [format!("nebula{run} cache")]}))
        .await;
    assert_eq!(fresh["outcome"], "inserted", "{fresh}");
    let (st, card) = s
        .get(&format!(
            "/v1/entities/concept:nebula{run}-cache@{scope}?scopes=global,{scope}"
        ))
        .await;
    assert_eq!(st, 200, "{card}");
    assert_eq!(card["memories"][0]["id"], fresh["id"]);

    let (_, again) = s.post("/v1/memories", base).await;
    assert_eq!(again["outcome"], "duplicate", "{again}");
    assert_eq!(again["id"], id);

    // Asks about gw, the parent of lb: the memory is found through containment.
    let (st, rec) = s.post("/v1/recall", json!({"query": format!("¿Por qué se cortaban las peticiones de gw{run}?"), "scopes": scopes, "raw": "off"})).await;
    assert_eq!(st, 200);
    assert_eq!(rec["parsed"]["w"], "why");
    assert_eq!(rec["items"][0]["id"], id, "{rec}");
    assert!(rec["text"].as_str().unwrap().contains(&format!("[m:{id}]")));
    assert!(
        rec["fast_ms"].as_f64().unwrap() < 50.0,
        "the fast path: {}",
        rec["fast_ms"]
    );

    let (_, other) = s
        .post(
            "/v1/recall",
            json!({"query": format!("gw{run} lb{run}"), "scopes": ["user:nobody"], "raw": "off"}),
        )
        .await;
    assert!(other["items"].as_array().unwrap().is_empty(), "{other}");

    // gw's card includes its children's memories.
    let (st, card) = s
        .get(&format!("/v1/entities/{gw}?scopes=global,{scope}"))
        .await;
    assert_eq!(st, 200);
    assert_eq!(card["memories"][0]["id"], id);

    // forget closes rather than deletes: still visible as of the past.
    let (st, _) = s
        .post(
            "/v1/forget",
            json!({"id": id, "scopes": [scope], "reason": "fixed"}),
        )
        .await;
    assert_eq!(st, 200);
    let (_, now) = s.get(&format!("/v1/memories/{id}?scopes={scope}")).await;
    assert_eq!(now["memory"]["live"], false);
    let (_, past) = s.post("/v1/recall", json!({"query": format!("lb{run}"), "scopes": scopes, "as_of": 1_790_000_000_001i64, "raw": "off"})).await;
    assert_eq!(past["items"][0]["id"], id, "seen as it was then");

    let eps: Vec<Value> = (0..3)
        .map(|i| json!({"episode_id": format!("it:{run}:{i}"), "thread": format!("session:{run}"), "source": "chat", "scope": scope,
            "owner_id": owner, "actor_id": owner, "role": if i % 2 == 0 { "user" } else { "assistant" },
            "occurred_at": 1_790_000_000_000i64 + i * 1000, "text": format!("message {i} about quasar{run}"), "reader_scopes": scopes}))
        .collect();
    let (st, stored) = s
        .post(
            "/v1/episodes",
            json!({"episodes": eps, "cursor": {"name": format!("it-{run}"), "value": 42}}),
        )
        .await;
    assert_eq!(st, 200, "{stored}");
    let (_, cur) = s.get(&format!("/v1/cursors/it-{run}")).await;
    assert_eq!(cur["value"], 42);
    let (_, ctx) = s
        .get(&format!("/v1/episodes/it:{run}:1?scopes={scope}&around=1"))
        .await;
    assert_eq!(ctx["episodes"].as_array().unwrap().len(), 3, "{ctx}");
    let (_, slow) = s
        .post(
            "/v1/recall",
            json!({"query": format!("quasar{run}"), "scopes": scopes, "raw": "on"}),
        )
        .await;
    assert!(
        !slow["raw"].as_array().unwrap().is_empty(),
        "raw episodes are found by text: {slow}"
    );

    let (st, created) = s
        .post(
            "/v1/keys",
            json!({"name": "it", "kind": "user", "subject": format!("u-{run}")}),
        )
        .await;
    assert_eq!(st, 200, "{created}");
    let u = s.with_key(created["key"].as_str().unwrap());
    let (_, who) = u.get("/v1/whoami").await;
    assert_eq!(who["subject"], format!("u-{run}"));
    let (st, _) = u
        .post(
            "/v1/recall",
            json!({"query": format!("quasar{run}"), "scopes": [scope], "raw": "on"}),
        )
        .await;
    assert_eq!(st, 403, "asking for someone else's scope does not open it");
    let (st, _) = u
        .post(
            "/v1/memories",
            json!({"scope": "global", "what": "something global and long"}),
        )
        .await;
    assert_eq!(st, 403, "a user does not write to global");
    let (st, mine) = u.post("/v1/memories", json!({"scope": format!("team:t-{run}"), "what": format!("The team t-{run} owns the billing pipeline")})).await;
    assert_eq!(st, 200, "{mine}");
    let (st, _) = u
        .post(
            "/v1/keys",
            json!({"name": "x", "kind": "admin", "subject": "x"}),
        )
        .await;
    assert_eq!(st, 403, "a user does not create keys");
    let (_, init) = u.post("/mcp", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "mnemo");
    let (_, found) = u.post("/mcp", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "recall", "arguments": {"query": format!("who owns the billing pipeline of t-{run}?")}}})).await;
    assert!(
        found["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("billing pipeline"),
        "{found}"
    );
    let (st, _) = s
        .call(
            reqwest::Method::DELETE,
            &format!("/v1/keys/{}", created["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(st, 200);
    let (st, _) = u.get("/v1/whoami").await;
    assert_eq!(st, 401, "a revoked key stops working");
    let (st, _) = s
        .call(
            reqwest::Method::DELETE,
            &format!("/v1/owners/u-{run}"),
            None,
        )
        .await;
    assert_eq!(st, 200);

    let (st, _) = s
        .call(
            reqwest::Method::DELETE,
            &format!("/v1/owners/{owner}"),
            None,
        )
        .await;
    assert_eq!(st, 200);
    let (st, _) = s.get(&format!("/v1/memories/{id}?scopes={scope}")).await;
    assert_eq!(st, 404);
    let (_, gone) = s
        .post(
            "/v1/recall",
            json!({"query": format!("quasar{run} gw{run}"), "scopes": scopes, "raw": "on"}),
        )
        .await;
    assert!(
        gone["items"].as_array().unwrap().is_empty() && gone["raw"].as_array().unwrap().is_empty(),
        "{gone}"
    );
}
