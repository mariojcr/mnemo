//! HTTP client behind `mnemo mcp` and `mnemo hook`. It only talks to a configured server
//! (`MNEMO_URL`): the plugin never starts anything on the local machine.

use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7700";

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("MNEMO_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".mnemo")
}

pub struct Client {
    pub base: String,
    pub key: String,
    http: reqwest::Client,
}

impl Client {
    pub fn from_env(timeout: Duration) -> Result<Client, String> {
        Client::new(
            &std::env::var("MNEMO_URL").unwrap_or_default(),
            &std::env::var("MNEMO_KEY").unwrap_or_default(),
            timeout,
        )
    }

    pub fn new(url: &str, key: &str, timeout: Duration) -> Result<Client, String> {
        let base = url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(
                "MNEMO_URL is not set: point the plugin at your mnemo server (in the environment or in ~/.mnemo/config.env)"
                    .into(),
            );
        }
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Ok(Client {
            base,
            key: key.to_string(),
            http,
        })
    }

    async fn healthy(&self) -> bool {
        self.http
            .get(format!("{}/healthz", self.base))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    pub async fn ensure(&self) -> Result<(), String> {
        if self.healthy().await {
            Ok(())
        } else {
            Err(format!("mnemo is not answering at {}", self.base))
        }
    }

    /// The `mnemo login` token is fetched on every call because the MCP bridge outlives the token.
    pub async fn auth(&self) -> Option<String> {
        if !self.key.is_empty() {
            return Some(self.key.clone());
        }
        crate::login::bearer(&self.base, &self.http).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(self.http.post(format!("{}{}", self.base, path)).json(body))
            .await
    }

    pub async fn get(&self, path: &str) -> Result<Value, String> {
        self.send(self.http.get(format!("{}{}", self.base, path)))
            .await
    }

    async fn send(&self, mut req: reqwest::RequestBuilder) -> Result<Value, String> {
        if let Some(t) = self.auth().await {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "mnemo {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            ));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_needs_a_configured_server() {
        assert!(Client::new("", "", Duration::from_secs(1)).is_err());
        assert!(Client::new("   ", "", Duration::from_secs(1)).is_err());
        let c = Client::new(" https://mnemo.example.com/ ", "k", Duration::from_secs(1)).unwrap();
        assert_eq!(c.base, "https://mnemo.example.com");
    }
}
