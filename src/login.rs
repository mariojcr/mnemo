//! `mnemo login`: OAuth with PKCE against the provider an SSO-only server advertises (RFC 9728),
//! like any MCP client, so the plugin works without keys. Tokens go to `~/.mnemo/oauth.json`.

use crate::client::home;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A public client (PKCE, no secret) registered at the provider with `callback()` as redirect URI.
fn client_id() -> String {
    let v = std::env::var("MNEMO_LOGIN_CLIENT_ID").unwrap_or_default();
    if v.trim().is_empty() {
        "mnemo-cli".into()
    } else {
        v.trim().to_string()
    }
}

/// Fixed port: providers match the redirect URI exactly, port included.
fn port() -> u16 {
    std::env::var("MNEMO_LOGIN_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(7787)
}

fn callback() -> String {
    format!("http://127.0.0.1:{}/callback", port())
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Saved {
    pub server: String,
    pub resource: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    pub expires_at: u64,
}

fn path() -> PathBuf {
    home().join("oauth.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Plain http only on this machine, since the token travels over it. The host is compared whole
/// (`http://127.0.0.1.evil.com` is not loopback).
fn secure_url(u: &str) -> bool {
    if u.starts_with("https://") {
        return true;
    }
    let Some(rest) = u.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn load() -> Option<Saved> {
    serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()
}

/// 0600 from creation (not a later chmod), then a rename: never readable by others or partial.
fn save(s: &Saved) -> Result<(), String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let dir = home();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("oauth.json.{}", uuid::Uuid::new_v4().simple()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(
        serde_json::to_string_pretty(s)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path()).map_err(|e| e.to_string())
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn callback_params(request_line: &str) -> Option<Vec<(String, String)>> {
    let target = request_line.split_whitespace().nth(1)?;
    let (p, q) = target.split_once('?')?;
    if p != "/callback" {
        return None;
    }
    Some(
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (dec(k), dec(v)))
            .collect(),
    )
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let r = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{url}: {}", r.status()));
    }
    r.json().await.map_err(|e| format!("{url}: {e}"))
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<Value, String> {
    let r = http
        .post(endpoint)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("provider: {e}"))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        // Only the error code: the response body might echo what was sent.
        return Err(format!(
            "provider {}: {}",
            status.as_u16(),
            v["error"].as_str().unwrap_or("no detail")
        ));
    }
    Ok(v)
}

fn apply(s: &mut Saved, v: &Value) -> Result<(), String> {
    let at = v["access_token"].as_str().unwrap_or_default();
    if at.is_empty() {
        return Err("the provider returned no access token".into());
    }
    s.access_token = at.to_string();
    if let Some(rt) = v["refresh_token"].as_str().filter(|t| !t.is_empty()) {
        s.refresh_token = rt.to_string();
    }
    s.expires_at = now() + v["expires_in"].as_u64().unwrap_or(3600);
    Ok(())
}

pub async fn login(server: &str) -> Result<(), String> {
    let server = server.trim().trim_end_matches('/').to_string();
    if !secure_url(&server) {
        return Err(format!("{server}: https is required"));
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let meta = get_json(
        &http,
        &format!("{server}/.well-known/oauth-protected-resource/mcp"),
    )
    .await?;
    let resource = meta["resource"].as_str().unwrap_or_default().to_string();
    let issuer = meta["authorization_servers"][0]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    if resource.is_empty() || issuer.is_empty() {
        return Err(format!(
            "{server} advertises no OAuth provider: is it SSO-only?"
        ));
    }
    // A fake server could advertise the real server's resource and capture a token valid there
    // (RFC 9728 §3.3).
    if resource.trim_end_matches('/') != format!("{server}/mcp") {
        return Err(format!(
            "{server} advertises the resource {resource}, which is not its own: not signing in"
        ));
    }
    if !secure_url(&issuer) {
        return Err(format!("{issuer}: https is required"));
    }
    // A provider that ignores `resource` (Keycloak) issues tokens valid on any server trusting it:
    // MNEMO_LOGIN_ISSUERS limits which providers get the session.
    let allowed: Vec<String> = std::env::var("MNEMO_LOGIN_ISSUERS")
        .unwrap_or_default()
        .split([',', ' '])
        .map(|i| i.trim().trim_end_matches('/').to_string())
        .filter(|i| !i.is_empty())
        .collect();
    if !allowed.is_empty() && !allowed.contains(&issuer) {
        return Err(format!("{issuer} is not in MNEMO_LOGIN_ISSUERS"));
    }
    let disco = get_json(&http, &format!("{issuer}/.well-known/openid-configuration")).await?;
    if disco["issuer"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/')
        != issuer
    {
        return Err(format!(
            "{issuer}: its discovery document claims another issuer"
        ));
    }
    let (auth_ep, token_ep) = (
        disco["authorization_endpoint"].as_str().unwrap_or_default(),
        disco["token_endpoint"].as_str().unwrap_or_default(),
    );
    if !secure_url(auth_ep) || !secure_url(token_ep) {
        return Err("the provider does not publish its endpoints over https".into());
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port()))
        .await
        .map_err(|e| format!("port {}: {e}", port()))?;
    let (verifier, state) = (random(), random());
    let challenge = {
        use sha2::{Digest, Sha256};
        b64url(&Sha256::digest(verifier.as_bytes()))
    };
    let cid = client_id();
    let url = format!(
        "{auth_ep}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}&code_challenge={challenge}&code_challenge_method=S256&resource={}",
        enc(&cid),
        enc(&callback()),
        enc("openid email profile groups"),
        enc(&resource),
    );
    eprintln!("Open this in your browser to sign in to {server}:\n\n  {url}\n");
    let _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .or_else(|_| std::process::Command::new("open").arg(&url).spawn());

    let code = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 8192];
            // A silent connection cannot block the port for the whole wait.
            let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            let params = head.lines().next().and_then(callback_params);
            let get = |k: &str| {
                params
                    .as_ref()
                    .and_then(|p| p.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone()))
            };
            let (body, result) = match (get("state"), get("code"), get("error")) {
                (Some(s), Some(c), _) if s == state => ("Done: you can close this tab.", Some(Ok(c))),
                (Some(s), _, Some(e)) if s == state => (
                    "The provider refused the sign-in.",
                    Some(Err(format!("the provider refused the sign-in: {}", e.chars().take(80).collect::<String>()))),
                ),
                _ => ("Unrecognized request.", None),
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            if let Some(r) = result {
                return r;
            }
        }
    })
    .await
    .map_err(|_| "no return from the provider within five minutes".to_string())??;

    let v = token_request(
        &http,
        token_ep,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &callback()),
            ("client_id", &cid),
            ("code_verifier", &verifier),
            ("resource", &resource),
        ],
    )
    .await?;
    let mut s = Saved {
        server: server.clone(),
        resource,
        token_endpoint: token_ep.to_string(),
        client_id: cid,
        ..Default::default()
    };
    apply(&mut s, &v)?;
    let who = http
        .get(format!("{server}/v1/whoami"))
        .bearer_auth(&s.access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !who.status().is_success() {
        return Err(format!(
            "{server} does not accept the token ({}): are you in the group that grants access?",
            who.status()
        ));
    }
    let me: Value = who.json().await.unwrap_or(Value::Null);
    save(&s)?;
    eprintln!(
        "Signed in as {}. Token saved to {}{}.",
        me["subject"].as_str().unwrap_or("?"),
        path().display(),
        if s.refresh_token.is_empty() {
            " (no refresh token: when it expires, run mnemo login again)"
        } else {
            ""
        }
    );
    Ok(())
}

pub fn logout() -> Result<(), String> {
    match std::fs::remove_file(path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// `None` sends the request without a token: the server's 401 is the honest outcome.
pub async fn bearer(server: &str, http: &reqwest::Client) -> Option<String> {
    let mut s = load()?;
    if s.server != server.trim_end_matches('/') {
        return None;
    }
    if s.expires_at > now() + 60 {
        return Some(s.access_token);
    }
    if s.refresh_token.is_empty() || !secure_url(&s.token_endpoint) {
        return None;
    }
    let rt = s.refresh_token.clone();
    match token_request(
        http,
        &s.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &rt),
            ("client_id", &s.client_id),
            ("resource", &s.resource),
        ],
    )
    .await
    {
        Ok(v) if apply(&mut s, &v).is_ok() => {
            if let Err(e) = save(&s) {
                log::warn!("could not save the refreshed token: {e}");
            }
            Some(s.access_token)
        }
        r => {
            // Another process (a hook beside the MCP bridge) may have refreshed first and spent a
            // rotating refresh token: use what it saved.
            if let Err(e) = r {
                log::warn!("token refresh: {e}");
            }
            load()
                .filter(|n| n.server == s.server && n.expires_at > now() + 60)
                .map(|n| n.access_token)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_is_parsed_and_only_on_its_path() {
        let p = callback_params("GET /callback?code=a%2Bb&state=x+y HTTP/1.1").unwrap();
        assert_eq!(
            p,
            vec![
                ("code".into(), "a+b".into()),
                ("state".into(), "x y".into())
            ]
        );
        assert!(callback_params("GET /other?code=a HTTP/1.1").is_none());
        assert!(callback_params("GET /callback HTTP/1.1").is_none());
        assert_eq!(dec("100%"), "100%");
    }

    #[test]
    fn tokens_only_travel_over_https_or_loopback() {
        assert!(secure_url("https://mnemo.example.com"));
        assert!(secure_url("http://127.0.0.1:7700"));
        assert!(!secure_url("http://mnemo.example.com"));
        assert!(!secure_url("http://127.0.0.1.evil.com/x"));
        assert!(!secure_url("http://localhost@evil.com"));
        assert!(secure_url("http://[::1]:7700/"));
    }
}
