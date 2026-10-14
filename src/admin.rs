// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! What the UI needs beyond the globe: OIDC login, own keys, connections, sessions, admin panel.
//! Configuration is shown, not edited: editing it here would be a second source of truth.

use crate::api::err;
use crate::app::{App, now_ms};
use crate::auth::{Kind, Principal, Via};
use crate::oidc::{self, SESSION_COOKIE};
use crate::storage::{LoginRow, SessionRow};
use axum::{
    Extension, Json,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

const DAY_MS: i64 = 86_400_000;

fn admin_only(p: &Principal) -> Option<Response> {
    (p.kind != Kind::Admin).then(|| err(StatusCode::FORBIDDEN, "admin only"))
}

fn internal(e: impl std::fmt::Display) -> Response {
    crate::api::internal(e)
}

pub async fn auth_config(State(app): State<Arc<App>>) -> Response {
    Json(json!({"oidc": app.oidc.is_some(), "keys": !app.cfg.sso_only})).into_response()
}

#[derive(Deserialize)]
pub struct LoginQ {
    next: Option<String>,
}

/// Rate-limit identity: the socket IP or, behind a local proxy, the `X-Forwarded-For` entry `hops`
/// from the end (written by the closest trusted proxy); earlier entries can be set by anyone.
pub fn client_of(peer: IpAddr, headers: &HeaderMap, hops: usize) -> String {
    let local = match peer {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    };
    if local
        && hops > 0
        && let Some(fwd) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').nth(hops - 1))
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return fwd.to_string();
    }
    peer.to_string()
}

pub async fn login(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<LoginQ>,
) -> Response {
    let Some(o) = &app.oidc else {
        return err(StatusCode::NOT_FOUND, "this server has no OIDC configured");
    };
    if !app.allow_login(&client_of(peer.ip(), &headers, app.forwarded_hops)) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many login attempts: try again in a minute",
        );
    }
    let (state, nonce, verifier) = (
        oidc::random_hex(24),
        oidc::random_hex(24),
        oidc::random_hex(48),
    );
    let row = LoginRow {
        state_hash: crate::auth::hash(&state),
        verifier: verifier.clone(),
        nonce: nonce.clone(),
        return_to: oidc::safe_next(q.next.as_deref().unwrap_or("/")),
        expires_ms: now_ms() + 10 * 60_000,
    };
    if let Err(e) = app.db.put_login(&row).await {
        return internal(e);
    }
    match o.login_url(&state, &nonce, &verifier).await {
        Ok(url) => (
            [(
                header::SET_COOKIE,
                oidc::login_cookie(&state, 600, o.secure_cookie()),
            )],
            Redirect::to(&url),
        )
            .into_response(),
        Err(e) => crate::api::upstream("the identity provider", e),
    }
}

#[derive(Deserialize)]
pub struct CallbackQ {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQ>,
) -> Response {
    let Some(o) = &app.oidc else {
        return err(StatusCode::NOT_FOUND, "this server has no OIDC configured");
    };
    if let Some(e) = q.error {
        return err(
            StatusCode::UNAUTHORIZED,
            format!(
                "the provider rejected the login: {e} {}",
                q.error_description.unwrap_or_default()
            ),
        );
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return err(StatusCode::BAD_REQUEST, "missing code and state");
    };
    // Login CSRF: only the browser that started the login may finish it, or an attacker's
    // callback URL would sign this person into the attacker's account.
    let secure = app.oidc.as_ref().is_some_and(|o| o.secure_cookie());
    let browser = oidc::cookie_for(&headers, oidc::LOGIN_COOKIE, secure);
    if browser.is_empty() || !crate::auth::same(browser.as_bytes(), state.as_bytes()) {
        return err(
            StatusCode::BAD_REQUEST,
            "this login did not start in this browser: sign in again",
        );
    }
    let login = match app
        .db
        .take_login(&crate::auth::hash(&state), now_ms())
        .await
    {
        Ok(Some(l)) => l,
        Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                "login expired or already used: sign in again",
            );
        }
        Err(e) => return internal(e),
    };
    let id = match o.finish(&code, &login.verifier, &login.nonce).await {
        Ok(id) => id,
        Err(e) => {
            log::warn!("oidc: {e}");
            return err(StatusCode::UNAUTHORIZED, e);
        }
    };
    let value = format!("mnemo_s_{}", oidc::random_hex(32));
    let now = now_ms();
    let ttl = o.cfg.session_hours * 3_600_000;
    let row = SessionRow {
        id: oidc::random_hex(8),
        hash: crate::auth::hash(&value),
        subject: id.subject.clone(),
        email: id.email.clone(),
        name: id.name.clone(),
        kind: if id.admin { "admin" } else { "user" }.into(),
        created_ms: now,
        expires_ms: now + ttl,
        revoked: false,
    };
    if let Err(e) = app.db.create_session(&row).await {
        return internal(e);
    }
    app.note_identity(&id.subject, &id.email, &id.name).await;
    log::info!(
        "{}",
        json!({"kind": "login", "subject": id.subject, "admin": id.admin})
    );
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                oidc::set_cookie(&value, ttl / 1000, o.secure_cookie()),
            ),
            (header::LOCATION, login.return_to),
        ],
    )
        .into_response()
}

pub async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    // CSRF: without our header and origin, a form on another site could sign out its visitors.
    if !crate::api::same_origin_ui(&headers) {
        return err(
            StatusCode::FORBIDDEN,
            "signing out only works from the web app itself",
        );
    }
    let secure = app.oidc.as_ref().is_some_and(|o| o.secure_cookie());
    let value = oidc::cookie_for(&headers, SESSION_COOKIE, secure);
    if !value.is_empty() {
        if let Ok(Some(s)) = app.db.session_by_hash(&crate::auth::hash(&value)).await {
            let _ = app.db.revoke_session(&s.id).await;
            // Only with a real session: an arbitrary caller must not flush everyone's caches.
            app.forget_principals();
        }
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, oidc::set_cookie("", 0, secure))],
        Json(json!({"ok": true})),
    )
        .into_response()
}

fn key_json(k: &crate::storage::KeyRow, last: Option<&i64>) -> Value {
    json!({"id": k.id, "name": k.name, "kind": k.kind, "subject": k.subject, "created_ms": k.created_ms,
        "expires_ms": k.expires_ms, "revoked": k.revoked, "created_by": k.created_by, "last_used_ms": last})
}

pub async fn my_keys(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    let (keys, used) = match (app.db.list_keys().await, app.db.keys_last_used().await) {
        (Ok(k), Ok(u)) => (k, u),
        (Err(e), _) | (_, Err(e)) => return internal(e),
    };
    let mine: Vec<Value> = keys
        .iter()
        .filter(|k| k.subject == p.subject && k.kind == "user")
        .map(|k| key_json(k, used.get(&k.id)))
        .collect();
    Json(json!({"keys": mine})).into_response()
}

#[derive(Deserialize)]
pub struct NewMyKey {
    name: String,
    #[serde(default)]
    days: Option<i64>,
}

/// Only from a browser session: a key cannot create keys, so a leaked one does not multiply.
pub async fn create_my_key(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<NewMyKey>,
) -> Response {
    if !matches!(p.via, Via::Session(_)) {
        return err(
            StatusCode::FORBIDDEN,
            "personal keys are created from a browser session, not with another key",
        );
    }
    if p.subject.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "no personal key without an identity",
        );
    }
    // At most 90 days: a personal key is not reconciled with the identity provider, so it outlives
    // a person's access there until it expires.
    let days = r.days.unwrap_or(90).clamp(1, 90);
    let name = if r.name.trim().is_empty() {
        "personal".to_string()
    } else {
        r.name.trim().chars().take(80).collect()
    };
    match app
        .create_key(
            &name,
            Kind::User,
            &p.subject,
            now_ms() + days * DAY_MS,
            &p.subject,
        )
        .await
    {
        Ok((row, key)) => {
            Json(json!({"key": key, "id": row.id, "expires_ms": row.expires_ms})).into_response()
        }
        Err(e) => internal(e),
    }
}

pub async fn revoke_my_key(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let keys = match app.db.list_keys().await {
        Ok(k) => k,
        Err(e) => return internal(e),
    };
    if !keys
        .iter()
        .any(|k| k.id == id && k.subject == p.subject && k.kind == "user" && !k.revoked)
    {
        return err(StatusCode::NOT_FOUND, "you have no active key with that id");
    }
    match app.revoke_key(&id).await {
        Ok(_) => Json(json!({"revoked": id})).into_response(),
        Err(e) => internal(e),
    }
}

pub async fn overview(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    let stats = app.db.stats().await.unwrap_or_else(|e| json!({"error": e}));
    let writer = app
        .db
        .writer_status()
        .await
        .ok()
        .flatten()
        .map(|(i, until)| json!({"instance": i, "until_ms": until, "alive": until > now_ms()}));
    let (ents, mems) = app.globe.read().unwrap().counts();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config": app.info,
        "writer": writer,
        "stats": stats,
        "globe": {"entities": ents, "memories": mems},
        "muted": app.muted.read().unwrap().len(),
    }))
    .into_response()
}

pub async fn all_keys(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    match (app.db.list_keys().await, app.db.keys_last_used().await) {
        (Ok(keys), Ok(used)) => Json(
            json!({"keys": keys.iter().map(|k| key_json(k, used.get(&k.id))).collect::<Vec<_>>()}),
        )
        .into_response(),
        (Err(e), _) | (_, Err(e)) => internal(e),
    }
}

/// Counts, not content: an admin does not read other people's memories.
pub async fn people(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    let now = now_ms();
    let (keys, sessions) = match (app.db.list_keys().await, app.db.list_sessions(now).await) {
        (Ok(k), Ok(s)) => (k, s),
        (Err(e), _) | (_, Err(e)) => return internal(e),
    };
    let mut people: BTreeMap<String, String> = BTreeMap::new();
    for k in keys.iter().filter(|k| k.active(now)) {
        people
            .entry(k.subject.clone())
            .or_insert_with(|| k.kind.clone());
    }
    for s in &sessions {
        let e = people
            .entry(s.subject.clone())
            .or_insert_with(|| s.kind.clone());
        if s.kind == "admin" {
            *e = "admin".into();
        }
    }
    let counts: BTreeMap<String, (u64, u64)> = app
        .globe
        .read()
        .unwrap()
        .scope_counts()
        .into_iter()
        .filter_map(|(s, live, total)| {
            s.strip_prefix("user:")
                .map(|u| (u.to_string(), (live, total)))
        })
        .collect();
    for u in counts.keys() {
        people.entry(u.clone()).or_insert_with(|| "-".into());
    }
    let who: BTreeMap<String, crate::storage::IdentityRow> = match app.db.identities().await {
        Ok(v) => v.into_iter().map(|i| (i.subject.clone(), i)).collect(),
        Err(e) => return internal(e),
    };
    for i in who.values() {
        people
            .entry(i.subject.clone())
            .or_insert_with(|| "-".into());
    }
    Json(json!({
        "people": people.iter().map(|(s, k)| {
            let (live, total) = counts.get(s).copied().unwrap_or((0, 0));
            let w = who.get(s);
            json!({"subject": s, "kind": k, "live": live, "total": total,
                "name": w.map(|w| w.name.clone()).unwrap_or_default(),
                "email": w.map(|w| w.email.clone()).unwrap_or_default(),
                "last_ms": w.map(|w| w.last_ms)})
        }).collect::<Vec<_>>(),
    }))
    .into_response()
}

pub async fn sessions(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    match app.db.list_sessions(now_ms()).await {
        Ok(s) => Json(json!({"sessions": s.iter().map(|x| json!({"id": x.id, "subject": x.subject, "email": x.email, "name": x.name, "kind": x.kind, "created_ms": x.created_ms, "expires_ms": x.expires_ms})).collect::<Vec<_>>()})).into_response(),
        Err(e) => internal(e),
    }
}

fn conn_json(c: &crate::storage::ConnectionRow) -> Value {
    json!({"client": c.client, "first_ms": c.first_ms, "last_ms": c.last_ms, "agent": c.agent, "blocked": c.blocked_ms > 0, "blocked_ms": c.blocked_ms})
}

/// Managed in the browser: a connected client must not disconnect another one, nor unblock itself.
fn browser_only(p: &Principal) -> Option<Response> {
    (!matches!(p.via, Via::Session(_) | Via::AdminKey)).then(|| {
        err(
            StatusCode::FORBIDDEN,
            "connections and sessions are managed from the web, signed in with SSO",
        )
    })
}

pub async fn my_connections(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
) -> Response {
    let apps: Vec<Value> = app
        .connections_of(Some(&p.subject))
        .await
        .iter()
        .map(conn_json)
        .collect();
    let current = match &p.via {
        Via::Session(id) => id.as_str(),
        _ => "",
    };
    let sessions = match app.db.list_sessions(now_ms()).await {
        Ok(s) => s
            .iter()
            .filter(|x| x.subject == p.subject)
            .map(|x| json!({"id": x.id, "created_ms": x.created_ms, "expires_ms": x.expires_ms, "current": x.id == current}))
            .collect::<Vec<_>>(),
        Err(e) => return internal(e),
    };
    Json(json!({"apps": apps, "sessions": sessions})).into_response()
}

#[derive(Deserialize)]
pub struct SetConnection {
    #[serde(default)]
    subject: String,
    client: String,
    blocked: bool,
}

pub async fn set_my_connection(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<SetConnection>,
) -> Response {
    if let Some(e) = browser_only(&p) {
        return e;
    }
    match app
        .set_connection_blocked(&p.subject, &r.client, r.blocked)
        .await
    {
        Ok(true) => Json(json!({"client": r.client, "blocked": r.blocked})).into_response(),
        Ok(false) => err(
            StatusCode::NOT_FOUND,
            "no app with that id has connected to your mnemo",
        ),
        Err(e) => internal(e),
    }
}

pub async fn revoke_my_session(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    if let Some(e) = browser_only(&p) {
        return e;
    }
    let mine = match app.db.list_sessions(now_ms()).await {
        Ok(s) => s.iter().any(|x| x.id == id && x.subject == p.subject),
        Err(e) => return internal(e),
    };
    // Someone else's session gets the same answer as a missing one: ownership is not revealed.
    if !mine {
        return err(
            StatusCode::NOT_FOUND,
            "no active session of yours with that id",
        );
    }
    match app.db.revoke_session(&id).await {
        Ok(_) => {
            app.forget_principals();
            Json(json!({"revoked": id})).into_response()
        }
        Err(e) => internal(e),
    }
}

/// An admin sees who uses what, never content.
pub async fn all_connections(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    let who: BTreeMap<String, crate::storage::IdentityRow> = match app.db.identities().await {
        Ok(v) => v.into_iter().map(|i| (i.subject.clone(), i)).collect(),
        Err(e) => return internal(e),
    };
    let apps: Vec<Value> = app
        .connections_of(None)
        .await
        .iter()
        .map(|c| {
            let mut v = conn_json(c);
            let i = who.get(&c.subject);
            v["subject"] = json!(c.subject);
            v["name"] = json!(i.map(|i| i.name.as_str()).unwrap_or(""));
            v["email"] = json!(i.map(|i| i.email.as_str()).unwrap_or(""));
            v
        })
        .collect();
    Json(json!({"apps": apps})).into_response()
}

pub async fn set_any_connection(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<SetConnection>,
) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    match app
        .set_connection_blocked(&r.subject, &r.client, r.blocked)
        .await
    {
        Ok(true) => Json(json!({"subject": r.subject, "client": r.client, "blocked": r.blocked}))
            .into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "that person has no app with that id"),
        Err(e) => internal(e),
    }
}

/// For when the same model served elsewhere yields a different space: resetting the fingerprint
/// makes the vector loop redo every vector.
pub async fn reembed(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    if let Some(e) = browser_only(&p) {
        return e;
    }
    match app.db.set_cursor("embed_model", 0).await {
        Ok(()) => Json(json!({"reembed": "scheduled", "memories": app.globe.read().unwrap().memories().count()})).into_response(),
        Err(e) => internal(e),
    }
}

pub async fn revoke_session(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    match app.db.revoke_session(&id).await {
        Ok(true) => {
            app.forget_principals();
            Json(json!({"revoked": id})).into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "no active session with that id"),
        Err(e) => internal(e),
    }
}
