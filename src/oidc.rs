// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Human login with OIDC (authorization code + PKCE); with `MNEMO_AUTH=sso`, MCP clients present
//! the same provider's access tokens. Login state and sessions live in the database, not process
//! memory: the callback may land on another replica and revocation must reach all of them.

use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type Res<T> = Result<T, String>;

#[derive(Clone, Debug, Default)]
pub struct Cfg {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub base_url: String,
    pub scopes: String,
    pub subject_claim: String,
    pub groups_claim: String,
    pub admin_emails: Vec<String>,
    pub admin_groups: Vec<String>,
    /// Enforced here (web and MCP) rather than trusting the provider to restrict access.
    pub allowed_groups: Vec<String>,
    pub resource: String,
    /// Default: only `resource`, so a token for another app of the same provider is rejected.
    pub audiences: Vec<String>,
    pub session_hours: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub subject: String,
    pub email: String,
    pub name: String,
    pub admin: bool,
    /// Empty for browser sessions.
    pub client: String,
}

#[derive(Clone, Debug)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    userinfo_endpoint: String,
}

pub struct Oidc {
    pub cfg: Cfg,
    http: reqwest::Client,
    disco: Mutex<Option<(Discovery, Instant)>>,
    jwks: Mutex<Option<(JwkSet, Instant)>>,
    /// Rate-limits forced JWKS reloads: an unknown `kid` must not cost a round trip per request.
    last_refresh: Mutex<Option<Instant>>,
    /// Keyed by token SHA-256, so MCP calls don't hit the provider (JWKS, userinfo) every time.
    tokens: Mutex<std::collections::HashMap<String, (Identity, i64)>>,
}

const CACHE: Duration = Duration::from_secs(3600);

fn list_raw(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_lowercase())
        .filter(|x| !x.is_empty())
        .collect()
}

pub fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}

pub fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Local paths only: anything else would be an open redirect behind our domain.
pub fn safe_next(next: &str) -> String {
    // Browsers strip tabs and newlines, so `/\t/evil.com` becomes `//evil.com`.
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && !next.chars().any(|c| c.is_control());
    if ok { next.to_string() } else { "/".into() }
}

impl Oidc {
    pub fn new(cfg: Cfg) -> Res<Oidc> {
        if cfg.issuer.is_empty() || cfg.client_id.is_empty() || cfg.base_url.is_empty() {
            return Err(
                "OIDC needs MNEMO_OIDC_ISSUER, MNEMO_OIDC_CLIENT_ID and MNEMO_BASE_URL".into(),
            );
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Oidc {
            cfg,
            http,
            disco: Mutex::new(None),
            jwks: Mutex::new(None),
            tokens: Mutex::new(Default::default()),
            last_refresh: Mutex::new(None),
        })
    }

    pub fn from_env(env: impl Fn(&str) -> String) -> Res<Option<Oidc>> {
        let issuer = env("MNEMO_OIDC_ISSUER");
        if issuer.is_empty() {
            return Ok(None);
        }
        let or = |k: &str, d: &str| {
            let v = env(k);
            if v.is_empty() { d.to_string() } else { v }
        };
        Oidc::new(Cfg {
            issuer: issuer.trim_end_matches('/').to_string(),
            client_id: env("MNEMO_OIDC_CLIENT_ID"),
            client_secret: env("MNEMO_OIDC_CLIENT_SECRET"),
            base_url: env("MNEMO_BASE_URL").trim_end_matches('/').to_string(),
            scopes: or("MNEMO_OIDC_SCOPES", "openid email profile"),
            subject_claim: or("MNEMO_OIDC_SUBJECT_CLAIM", "email"),
            groups_claim: or("MNEMO_OIDC_GROUPS_CLAIM", "groups"),
            admin_emails: list(&env("MNEMO_ADMIN_EMAILS")),
            admin_groups: list(&env("MNEMO_ADMIN_GROUPS")),
            allowed_groups: list(&env("MNEMO_OIDC_ALLOWED_GROUPS")),
            resource: or(
                "MNEMO_OAUTH_RESOURCE",
                &format!("{}/mcp", env("MNEMO_BASE_URL").trim_end_matches('/')),
            ),
            audiences: list_raw(&env("MNEMO_OAUTH_AUDIENCES")),
            session_hours: or("MNEMO_SESSION_HOURS", "12")
                .parse()
                .unwrap_or(12)
                .clamp(1, 24 * 30),
        })
        .map(Some)
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.cfg.base_url)
    }

    pub fn secure_cookie(&self) -> bool {
        self.cfg.base_url.starts_with("https://")
    }

    async fn discovery(&self) -> Res<Discovery> {
        if let Some((d, at)) = self.disco.lock().unwrap().clone() {
            if at.elapsed() < CACHE {
                return Ok(d);
            }
        }
        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer);
        let v: Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("OIDC discovery: {e}"))?
            .json()
            .await
            .map_err(|e| format!("OIDC discovery: {e}"))?;
        let s = |k: &str| {
            v[k].as_str()
                .map(String::from)
                .ok_or(format!("OIDC discovery lacks {k}"))
        };
        let d = Discovery {
            authorization_endpoint: s("authorization_endpoint")?,
            token_endpoint: s("token_endpoint")?,
            jwks_uri: s("jwks_uri")?,
            userinfo_endpoint: s("userinfo_endpoint").unwrap_or_default(),
        };
        *self.disco.lock().unwrap() = Some((d.clone(), Instant::now()));
        Ok(d)
    }

    #[cfg(test)]
    pub fn set_keys_for_test(&self, set: JwkSet) {
        *self.jwks.lock().unwrap() = Some((set, Instant::now()));
    }

    async fn keys(&self, refresh: bool) -> Res<JwkSet> {
        let refresh = refresh && {
            let mut last = self.last_refresh.lock().unwrap();
            let ok = last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
            if ok {
                *last = Some(Instant::now());
            }
            ok
        };
        if !refresh {
            if let Some((k, at)) = self.jwks.lock().unwrap().clone() {
                if at.elapsed() < CACHE {
                    return Ok(k);
                }
            }
        }
        let d = self.discovery().await?;
        let set: JwkSet = self
            .http
            .get(&d.jwks_uri)
            .send()
            .await
            .map_err(|e| format!("JWKS: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JWKS: {e}"))?;
        *self.jwks.lock().unwrap() = Some((set.clone(), Instant::now()));
        Ok(set)
    }

    pub async fn login_url(&self, state: &str, nonce: &str, verifier: &str) -> Res<String> {
        let d = self.discovery().await?;
        let mut u = reqwest::Url::parse(&d.authorization_endpoint).map_err(|e| e.to_string())?;
        u.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &self.redirect_uri())
            .append_pair("scope", &self.cfg.scopes)
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", &challenge(verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(u.to_string())
    }

    pub async fn finish(&self, code: &str, verifier: &str, nonce: &str) -> Res<Identity> {
        let d = self.discovery().await?;
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", self.redirect_uri()),
            ("client_id", self.cfg.client_id.clone()),
            ("code_verifier", verifier.to_string()),
        ];
        if !self.cfg.client_secret.is_empty() {
            form.push(("client_secret", self.cfg.client_secret.clone()));
        }
        let resp = self
            .http
            .post(&d.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("OIDC token: {e}"))?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| format!("OIDC token: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "OIDC token {}: {}",
                status.as_u16(),
                v["error_description"]
                    .as_str()
                    .or(v["error"].as_str())
                    .unwrap_or("")
            ));
        }
        let token = v["id_token"]
            .as_str()
            .ok_or("OIDC: the response has no id_token")?;
        let keys = self.keys(false).await?;
        let claims = match verify(&keys, token, &self.cfg.issuer, &self.cfg.client_id, nonce) {
            // A newly rotated issuer key is not cached yet: reload once.
            Err(e) if e.contains("kid") => verify(
                &self.keys(true).await?,
                token,
                &self.cfg.issuer,
                &self.cfg.client_id,
                nonce,
            )?,
            other => other?,
        };
        identity(&self.cfg, &claims)
    }

    fn token_audiences(&self) -> Vec<String> {
        if self.cfg.audiences.is_empty() {
            vec![self.cfg.resource.clone()]
        } else {
            self.cfg.audiences.clone()
        }
    }

    pub async fn bearer(&self, token: &str) -> Res<Identity> {
        let key = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(token.as_bytes()))
        };
        let now = chrono_now_s();
        if let Some((id, exp)) = self.tokens.lock().unwrap().get(&key).cloned() {
            if exp > now {
                return Ok(id);
            }
        }
        let auds = self.token_audiences();
        let claims = match verify_jwt(&self.keys(false).await?, token, &self.cfg.issuer, &auds) {
            Err(e) if e.contains("kid") => {
                verify_jwt(&self.keys(true).await?, token, &self.cfg.issuer, &auds)?
            }
            other => other?,
        };
        // Reject id_tokens (they carry a nonce) even when the audience matches, e.g. a client_id
        // listed in MNEMO_OAUTH_AUDIENCES.
        if !claims.get("nonce").is_none_or(|v| v.is_null()) {
            return Err("OIDC: that is an id_token, not an access token".into());
        }
        let mut merged = claims.clone();
        let need = |c: &Value, k: &str| c.get(k).is_none_or(|v| v.is_null());
        if need(&claims, &self.cfg.subject_claim) || need(&claims, &self.cfg.groups_claim) {
            let d = self.discovery().await?;
            if d.userinfo_endpoint.is_empty() {
                return Err(
                    "OIDC: the token carries no identity and the provider has no userinfo".into(),
                );
            }
            let info: Value = self
                .http
                .get(&d.userinfo_endpoint)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| format!("userinfo: {e}"))?
                .error_for_status()
                .map_err(|e| format!("userinfo: {e}"))?
                .json()
                .await
                .map_err(|e| format!("userinfo: {e}"))?;
            if info["sub"] != claims["sub"] {
                return Err("userinfo: its sub does not match the token's".into());
            }
            if let (Some(m), Some(i)) = (merged.as_object_mut(), info.as_object()) {
                for (k, v) in i {
                    if k != "sub" && k != "iss" && k != "aud" {
                        m.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        let id = identity(&self.cfg, &merged)?;
        // At most 5 min, so removal from a group or at the provider takes effect within that time.
        let exp = claims["exp"].as_i64().unwrap_or(now + 60).min(now + 300);
        let mut cache = self.tokens.lock().unwrap();
        if cache.len() > 1000 {
            cache.retain(|_, (_, e)| *e > now);
        }
        cache.insert(key, (id.clone(), exp));
        Ok(id)
    }

    pub fn forget_tokens(&self) {
        self.tokens.lock().unwrap().clear();
    }

    pub fn resource_metadata(&self) -> Value {
        serde_json::json!({
            "resource": self.cfg.resource,
            "authorization_servers": [self.cfg.issuer],
            "scopes_supported": self.cfg.scopes.split_whitespace().collect::<Vec<_>>(),
            "bearer_methods_supported": ["header"],
            "resource_name": "mnemo",
        })
    }

    pub fn resource_metadata_url(&self) -> String {
        format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            self.cfg.base_url
        )
    }
}

fn chrono_now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn verify_jwt(keys: &JwkSet, token: &str, issuer: &str, audiences: &[String]) -> Res<Value> {
    let header = decode_header(token).map_err(|e| format!("token: {e}"))?;
    if !matches!(
        header.alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::PS256
            | Algorithm::PS384
            | Algorithm::PS512
            | Algorithm::EdDSA
    ) {
        return Err(format!("token: unsupported algorithm {:?}", header.alg));
    }
    let jwk = match &header.kid {
        Some(kid) => keys
            .find(kid)
            .ok_or_else(|| format!("token: kid {kid} is not in the JWKS"))?,
        None if keys.keys.len() == 1 => &keys.keys[0],
        None => return Err("token without kid and the JWKS has several keys".into()),
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("JWK: {e}"))?;
    let mut v = Validation::new(header.alg);
    v.set_audience(audiences);
    v.set_issuer(&[issuer]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = 60;
    let claims = decode::<Value>(token, &key, &v)
        .map(|d| d.claims)
        .map_err(|e| format!("token: {e}"))?;
    // Re-check `exp` as a number: a library that skips a non-numeric `exp` (GHSA-h395-gr6q-cpjc)
    // would otherwise accept a token that never expires.
    let exp = claims["exp"].as_f64().ok_or("token: exp is not a number")?;
    if exp < (chrono_now_s() as f64) - 60.0 {
        return Err("token: expired".into());
    }
    Ok(claims)
}

pub fn verify(
    keys: &JwkSet,
    token: &str,
    issuer: &str,
    client_id: &str,
    nonce: &str,
) -> Res<Value> {
    let claims = verify_jwt(keys, token, issuer, &[client_id.to_string()])
        .map_err(|e| e.replacen("token", "id_token", 1))?;
    if claims["nonce"].as_str() != Some(nonce) {
        return Err("id_token: the nonce does not match".into());
    }
    Ok(claims)
}

pub fn identity(cfg: &Cfg, claims: &Value) -> Res<Identity> {
    let email = claims["email"].as_str().unwrap_or("").to_lowercase();
    // No fallback (e.g. preferred_username): a self-registered user could pick another account's
    // subject as their name.
    let subject = claims[cfg.subject_claim.as_str()]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if subject.is_empty() || subject.contains(':') || subject.contains(',') {
        return Err(format!(
            "OIDC: the claim '{}' cannot serve as an identity ({subject:?})",
            cfg.subject_claim
        ));
    }
    // An unverified, editable email would let someone claim another user's address and memory.
    if cfg.subject_claim == "email" && claims["email_verified"] != true {
        return Err(
            "OIDC: the email is not verified (or use MNEMO_OIDC_SUBJECT_CLAIM=sub instead)".into(),
        );
    }
    let groups: Vec<String> = claims[cfg.groups_claim.as_str()]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|g| g.as_str())
                .map(|g| g.trim_start_matches('/').to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    if !cfg.allowed_groups.is_empty() && !groups.iter().any(|g| cfg.allowed_groups.contains(g)) {
        return Err("OIDC: you are not in any group with access to this mnemo".into());
    }
    let admin = (!email.is_empty()
        && claims["email_verified"] == true
        && cfg.admin_emails.contains(&email))
        || groups.iter().any(|g| cfg.admin_groups.contains(g));
    Ok(Identity {
        subject,
        email,
        name: claims["name"].as_str().unwrap_or("").to_string(),
        admin,
        client: client_of(claims),
    })
}

/// Sanitized: it comes from the provider but ends up in the database, the logs and the page.
pub fn client_of(claims: &Value) -> String {
    let raw = claims["client_id"]
        .as_str()
        .or_else(|| claims["azp"].as_str())
        .unwrap_or("");
    raw.chars().filter(|c| !c.is_control()).take(300).collect()
}

pub fn cookie(headers: &axum::http::HeaderMap, name: &str) -> String {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

pub const SESSION_COOKIE: &str = "mnemo_session";

/// `__Host-` over https: browsers require Secure, no Domain and Path=/, so a sibling site cannot
/// plant the cookie. Plain http (local) gets no prefix: the browser would reject it.
fn cookie_name(base: &str, secure: bool) -> String {
    if secure {
        format!("__Host-{base}")
    } else {
        base.to_string()
    }
}

/// Over https, ONLY the `__Host-` cookie: accepting the bare name would reopen session fixation
/// and login CSRF from a sibling site.
pub fn cookie_for(headers: &axum::http::HeaderMap, base: &str, secure: bool) -> String {
    cookie(headers, &cookie_name(base, secure))
}
/// Binds `state` to the browser that started the flow: otherwise an attacker could log a victim
/// into the attacker's account with their own callback URL.
pub const LOGIN_COOKIE: &str = "mnemo_login";

pub fn login_cookie(state: &str, max_age_s: i64, secure: bool) -> String {
    format!(
        "{}={state}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_s}{}",
        cookie_name(LOGIN_COOKIE, secure),
        if secure { "; Secure" } else { "" }
    )
}

pub fn set_cookie(value: &str, max_age_s: i64, secure: bool) -> String {
    format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_s}{}",
        cookie_name(SESSION_COOKIE, secure),
        if secure { "; Secure" } else { "" }
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn with_https_only_the_host_prefixed_cookie_counts() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            "cookie",
            "mnemo_session=planted; __Host-mnemo_session=real"
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_for(&h, SESSION_COOKIE, true), "real");
        let mut only_bare = axum::http::HeaderMap::new();
        only_bare.insert("cookie", "mnemo_session=planted".parse().unwrap());
        assert_eq!(cookie_for(&only_bare, SESSION_COOKIE, true), "");
        assert_eq!(cookie_for(&only_bare, SESSION_COOKIE, false), "planted");
        assert!(set_cookie("v", 10, true).starts_with("__Host-mnemo_session=v; Path=/;"));
    }
    use jsonwebtoken::{EncodingKey, Header, encode};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::EncodePrivateKey;
    use serde_json::json;

    pub(crate) fn keypair(kid: &str) -> (EncodingKey, JwkSet) {
        let sk = SigningKey::random(&mut rand_core::OsRng);
        let pem = sk.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
        let mut jwk: Value =
            serde_json::from_str(&p256::PublicKey::from(sk.verifying_key()).to_jwk_string())
                .unwrap();
        jwk["kid"] = json!(kid);
        jwk["alg"] = json!("ES256");
        jwk["use"] = json!("sig");
        let set: JwkSet = serde_json::from_value(json!({"keys": [jwk]})).unwrap();
        (EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(), set)
    }

    pub(crate) fn token(key: &EncodingKey, kid: &str, claims: Value) -> String {
        let mut h = Header::new(Algorithm::ES256);
        h.kid = Some(kid.into());
        encode(&h, &claims, key).unwrap()
    }

    fn claims(extra: Value) -> Value {
        let now = crate::app::now_ms() / 1000;
        let mut c = json!({"iss": "https://idp", "aud": "mnemo", "sub": "u-1", "email_verified": true, "exp": now + 300, "iat": now, "nonce": "n1", "email": "Ana@Example.com", "name": "Ana",
            "groups": ["/plat", "/mnemo-admins", "/team-data/sub"]});
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        c
    }

    #[test]
    fn a_good_token_verifies_and_maps_to_an_identity() {
        let (k, set) = keypair("k1");
        let c = verify(
            &set,
            &token(&k, "k1", claims(json!({}))),
            "https://idp",
            "mnemo",
            "n1",
        )
        .unwrap();
        let cfg = Cfg {
            subject_claim: "email".into(),
            groups_claim: "groups".into(),
            admin_groups: vec!["mnemo-admins".into()],
            ..Default::default()
        };
        let id = identity(&cfg, &c).unwrap();
        assert_eq!(
            id.subject, "ana@example.com",
            "lower-cased: the identity does not depend on how the IdP spells it"
        );
        assert!(id.admin, "admin by group");
    }

    #[test]
    fn an_access_token_must_be_for_mnemo_and_from_an_allowed_group() {
        let (k, set) = keypair("k1");
        let res = "https://mnemo.example/mcp".to_string();
        let mut c = claims(json!({}));
        c["aud"] = json!(res);
        let tok = token(&k, "k1", c.clone());
        assert!(verify_jwt(&set, &tok, "https://idp", std::slice::from_ref(&res)).is_ok());
        let mut other = c.clone();
        other["aud"] = json!("other-app");
        let other = token(&k, "k1", other);
        assert!(verify_jwt(&set, &other, "https://idp", std::slice::from_ref(&res)).is_err());
        let mut nosub = c.clone();
        nosub.as_object_mut().unwrap().remove("sub");
        assert!(
            verify_jwt(
                &set,
                &token(&k, "k1", nosub),
                "https://idp",
                std::slice::from_ref(&res)
            )
            .is_err()
        );
        let mut old = c.clone();
        old["exp"] = json!(1);
        assert!(
            verify_jwt(
                &set,
                &token(&k, "k1", old),
                "https://idp",
                std::slice::from_ref(&res)
            )
            .is_err()
        );
        let cfg = Cfg {
            subject_claim: "email".into(),
            groups_claim: "groups".into(),
            allowed_groups: vec!["mnemo-admins".into()],
            ..Default::default()
        };
        assert!(identity(&cfg, &c).is_ok());
        let mut outsider = c.clone();
        outsider["groups"] = json!(["/plat"]);
        assert!(identity(&cfg, &outsider).unwrap_err().contains("group"));
    }

    #[test]
    fn tampered_or_foreign_tokens_are_rejected() {
        let (k, set) = keypair("k1");
        let good = token(&k, "k1", claims(json!({})));
        assert!(
            verify(&set, &good, "https://idp", "other-client", "n1").is_err(),
            "audience of another client"
        );
        assert!(
            verify(&set, &good, "https://evil", "mnemo", "n1").is_err(),
            "another issuer"
        );
        assert!(
            verify(&set, &good, "https://idp", "mnemo", "n2")
                .unwrap_err()
                .contains("nonce"),
            "nonce from another login"
        );
        let expired = token(
            &k,
            "k1",
            claims(json!({"exp": crate::app::now_ms() / 1000 - 3600})),
        );
        assert!(
            verify(&set, &expired, "https://idp", "mnemo", "n1").is_err(),
            "expired"
        );
        let (other, _) = keypair("k1");
        assert!(
            verify(
                &set,
                &token(&other, "k1", claims(json!({}))),
                "https://idp",
                "mnemo",
                "n1"
            )
            .is_err(),
            "signed with another key"
        );
        assert!(
            verify(
                &set,
                &token(&k, "k9", claims(json!({}))),
                "https://idp",
                "mnemo",
                "n1"
            )
            .unwrap_err()
            .contains("kid")
        );
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(claims(json!({"email": "admin@example.com"})).to_string());
        parts[1] = &forged;
        assert!(
            verify(&set, &parts.join("."), "https://idp", "mnemo", "n1").is_err(),
            "claims changed after signing"
        );
    }

    #[test]
    fn identities_that_cannot_be_scopes_are_refused() {
        let cfg = Cfg {
            subject_claim: "email".into(),
            groups_claim: "groups".into(),
            ..Default::default()
        };
        assert!(identity(&cfg, &json!({"email": ""})).is_err());
        assert!(
            identity(&cfg, &json!({"email": "a:b@x"})).is_err(),
            "a ':' would break the scope user:<subject>"
        );
        assert!(identity(&cfg, &json!({"email": "a@x", "email_verified": false})).is_err());
        assert!(
            identity(&cfg, &json!({"email": "a@x"})).is_err(),
            "email as identity without being marked verified: rejected (someone else could set it)"
        );
        assert!(
            !identity(&cfg, &json!({"email": "a@x", "email_verified": true}))
                .unwrap()
                .admin
        );
        let by_sub = Cfg {
            subject_claim: "sub".into(),
            ..cfg.clone()
        };
        assert_eq!(
            identity(&by_sub, &json!({"sub": "0f6c-uuid", "email": "a@x"}))
                .unwrap()
                .subject,
            "0f6c-uuid",
            "with sub, the identity is immutable even if the email is not verified"
        );
        assert!(
            identity(&cfg, &json!({"preferred_username": "ana"})).is_err(),
            "without the configured claim, no identity: no fallback to another claim"
        );
        let admins = Cfg {
            admin_emails: vec!["a@x".into()],
            ..cfg.clone()
        };
        assert!(
            identity(&admins, &json!({"email": "a@x"})).is_err(),
            "admin by email only with a verified email (an unverified one does not even get in)"
        );
        assert!(
            identity(&admins, &json!({"email": "a@x", "email_verified": true}))
                .unwrap()
                .admin
        );
    }

    #[test]
    fn next_only_points_home_and_pkce_matches_the_rfc() {
        assert_eq!(safe_next("/admin"), "/admin");
        for bad in [
            "//evil.com",
            "https://evil.com",
            "evil",
            "/\\evil",
            "/\t/evil.com",
            "/\n/evil.com",
        ] {
            assert_eq!(safe_next(bad), "/", "{bad}");
        }
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_hex(32).len(), 64);
    }

    #[test]
    fn cookies_are_parsed_and_set_safely() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("cookie", "a=1; mnemo_session=abc; b=2".parse().unwrap());
        assert_eq!(cookie(&h, SESSION_COOKIE), "abc");
        assert_eq!(cookie(&h, "zzz"), "");
        let c = set_cookie("v", 60, true);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax") && c.ends_with("Secure"));
    }
}
