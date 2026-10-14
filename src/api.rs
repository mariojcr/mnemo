//! The HTTP API: authentication, routing and middleware; each route's work is in [`crate::ops`].

use crate::app::{App, IngestItem, RememberReq};
use crate::auth::{Kind, Principal};
use crate::model::Entity;
use crate::ops::{self, OpError, RecallInput};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (
        code,
        Json(json!({"error": {"message": msg.into(), "code": code.as_u16()}})),
    )
        .into_response()
}

/// Details carry internal URLs, addresses and SQL: they go to the log; the client gets a bare 500.
pub fn internal(e: impl std::fmt::Display) -> Response {
    log::warn!("500: {}", e);
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// The network error carries its URL: logged; the client learns what failed, not where it is.
pub fn upstream(what: &str, e: impl std::fmt::Display) -> Response {
    log::warn!("502 {what}: {e}");
    err(StatusCode::BAD_GATEWAY, format!("{what} is not responding"))
}

fn op_err(e: OpError) -> Response {
    let code = match &e {
        OpError::Bad(_) => StatusCode::BAD_REQUEST,
        OpError::Forbidden(_) => StatusCode::FORBIDDEN,
        OpError::NotFound(_) => StatusCode::NOT_FOUND,
        OpError::Upstream(_) => StatusCode::BAD_GATEWAY,
        OpError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // Internal and upstream details carry internal URLs and addresses: log them, never send them.
    match &e {
        OpError::Internal(d) | OpError::Upstream(d) => {
            log::warn!("{}: {d:?}", code.as_u16());
            err(code, "internal error")
        }
        _ => err(code, e.to_string()),
    }
}

/// Always a user, even for an admin: administration happens in the browser with the person
/// present, not through a token that passes through a third party.
fn oauth_principal(id: crate::oidc::Identity) -> Principal {
    Principal {
        kind: Kind::User,
        subject: id.subject.clone(),
        via: crate::auth::Via::OAuth(id.subject),
    }
}

/// Memory operations and reads only; account and admin routes need the browser session, so a token
/// that passes through a third party (or leaks) cannot erase the whole memory.
fn oauth_route_allowed(path: &str) -> bool {
    const ROUTES: [&str; 14] = [
        "/mcp",
        "/v1/whoami",
        "/v1/recall",
        "/v1/memories",
        "/v1/forget",
        "/v1/episodes",
        "/v1/cursors",
        "/v1/entities",
        "/v1/records",
        "/v1/turns",
        "/v1/globe",
        "/v1/galaxy",
        "/v1/stats",
        "/v1/cost",
    ];
    ROUTES.iter().any(|r| {
        path == *r
            || path
                .strip_prefix(r)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Set by a `service` key to act as a person: their subject, without the `user:` prefix.
pub const ACT_AS: &str = "x-mnemo-act-as";

/// On EVERY response to a delegated request. The front end must drop a response without it: a
/// mnemo that ignored X-Mnemo-Act-As answered as the `service` key, which reads any memory.
/// Checked on every response in case mnemo is rolled back mid-way.
pub const ACTING_AS: &str = "x-mnemo-acting-as";

fn mark_acting_as(r: &mut Response, subject: &str) {
    if let Ok(v) = axum::http::HeaderValue::from_str(subject) {
        r.headers_mut().insert(ACTING_AS, v);
    }
}

/// It must form a valid scope `user:<id>`: no `:` or `,`.
fn valid_act_as(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s
            .chars()
            .any(|c| c == ':' || c == ',' || c.is_whitespace() || c.is_control())
}

/// The header from anyone but a `service` key is a 403, never ignored: whoever sets it would
/// believe they are limited to one person when they are not.
fn act_as(
    p: Option<Principal>,
    headers: &HeaderMap,
) -> Result<Principal, (StatusCode, &'static str)> {
    let key_id = match p {
        Some(Principal {
            kind: Kind::Service,
            via: crate::auth::Via::Key(id),
            ..
        }) => id,
        _ => {
            return Err((
                StatusCode::FORBIDDEN,
                "X-Mnemo-Act-As only works with a service key",
            ));
        }
    };
    let mut values = headers.get_all(ACT_AS).iter();
    let subject = match (values.next(), values.next()) {
        (Some(v), None) => v.to_str().ok().filter(|s| valid_act_as(s)),
        _ => None,
    };
    let Some(subject) = subject else {
        return Err((
            StatusCode::BAD_REQUEST,
            "X-Mnemo-Act-As: a single subject, without `user:`, `:`, `,` or whitespace, and at most 256 bytes",
        ));
    };
    Ok(Principal {
        kind: Kind::User,
        subject: subject.to_string(),
        via: crate::auth::Via::Delegated(key_id),
    })
}

/// Whoever acts for a person only shows them their memory; the account and the rest stay theirs.
fn delegated_route_refused(path: &str) -> bool {
    const ROUTES: [&str; 8] = [
        "/v1/keys",
        "/v1/me",
        "/v1/admin",
        "/v1/owners",
        "/v1/cost",
        "/v1/cursors",
        "/v1/turns",
        "/mcp",
    ];
    ROUTES.iter().any(|r| {
        path == *r
            || path
                .strip_prefix(r)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

pub fn bearer(h: &HeaderMap) -> String {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_start_matches("Bearer ").trim().to_string())
        .unwrap_or_default()
}

/// With a cookie, writes need `X-Mnemo-UI` (and our `Origin`, if sent): a foreign form can make
/// the browser send the cookie but not that header, and without CORS no foreign script can set it.
async fn auth(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/healthz"
        || path == "/"
        || ASSETS.iter().any(|a| a.0 == path)
        || path.starts_with("/auth/")
        || path.starts_with("/.well-known/oauth-protected-resource")
    {
        return next.run(req).await;
    }
    if app.cfg.sso_only && (path.starts_with("/v1/keys") || path.starts_with("/v1/me/keys")) {
        return err(
            StatusCode::FORBIDDEN,
            "this mnemo is SSO-only: there are no keys",
        );
    }
    let key = bearer(&headers);
    let secure = app.oidc.as_ref().is_some_and(|o| o.secure_cookie());
    let cookie = crate::oidc::cookie_for(&headers, crate::oidc::SESSION_COOKIE, secure);
    // Three dot-separated parts: an OIDC access token (an MCP client after SSO), not a mnemo key.
    let jwt = key.split('.').count() == 3;
    let oidc_bearer = app.oidc.as_ref().filter(|_| !key.is_empty() && jwt);
    let p = if let Some(o) = oidc_bearer {
        match o.bearer(&key).await {
            Ok(_) if !oauth_route_allowed(path) => {
                return err(
                    StatusCode::FORBIDDEN,
                    "an app's OAuth token only reaches memory operations; manage your account in the browser",
                );
            }
            Ok(id) => {
                // Enforced here: the provider cannot revoke the token for the person, and the
                // client refreshes it on its own.
                if app.connection_blocked(&id.subject, &id.client).await {
                    return err(
                        StatusCode::FORBIDDEN,
                        "this app was disconnected from your mnemo; allow it again on the web, under Connections",
                    );
                }
                let agent = headers
                    .get(axum::http::header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                app.note_connection(&id.subject, &id.client, agent).await;
                app.note_identity(&id.subject, &id.email, &id.name).await;
                Some(oauth_principal(id))
            }
            Err(e) => {
                // Debug-escaped: token text in the error (a made-up kid) could forge log lines.
                log::warn!("OAuth token rejected: {e:?}");
                None
            }
        }
    } else if !key.is_empty() && app.cfg.sso_only {
        None
    } else if !key.is_empty() {
        app.principal(&key).await
    } else if !cookie.is_empty() {
        match app.session_principal(&cookie).await {
            Some(p) => {
                let write = !matches!(
                    *req.method(),
                    axum::http::Method::GET | axum::http::Method::HEAD
                );
                if write && !same_origin_ui(&headers) {
                    return err(
                        StatusCode::FORBIDDEN,
                        "browser request without X-Mnemo-UI or from another origin",
                    );
                }
                Some(p)
            }
            None => app.principal("").await,
        }
    } else {
        app.principal("").await
    };
    let p = if headers.contains_key(ACT_AS) {
        match act_as(p, &headers) {
            Ok(d) if delegated_route_refused(path) => {
                let mut r = err(
                    StatusCode::FORBIDDEN,
                    "acting for another person reaches only their memory, not the account or administration",
                );
                mark_acting_as(&mut r, &d.subject);
                return r;
            }
            Ok(d) => Some(d),
            Err((code, msg)) => return err(code, msg),
        }
    } else {
        p
    };
    match p {
        Some(p) => {
            let acting = matches!(p.via, crate::auth::Via::Delegated(_)).then(|| p.subject.clone());
            req.extensions_mut().insert(p);
            let mut r = next.run(req).await;
            if let Some(subject) = acting {
                mark_acting_as(&mut r, &subject);
            }
            r
        }
        None => {
            let mut r = err(
                StatusCode::UNAUTHORIZED,
                if app.cfg.sso_only {
                    "sign in with SSO (a session or an OAuth token from the provider)"
                } else {
                    "missing a valid key (Authorization: Bearer mnemo_…) or a session"
                },
            );
            // RFC 9728: tells an MCP client which provider to authenticate with.
            if let Some(o) = &app.oidc {
                if let Ok(v) =
                    format!("Bearer resource_metadata=\"{}\"", o.resource_metadata_url()).parse()
                {
                    r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                }
            }
            r
        }
    }
}

/// Set here so mnemo does not depend on a reverse proxy adding them.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.entry(k).or_insert_with(|| HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'none'; script-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; font-src 'self'; manifest-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    );
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "no-referrer");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    resp
}

async fn protected_resource(State(app): State<Arc<App>>) -> Response {
    match &app.oidc {
        Some(o) => Json(o.resource_metadata()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no OAuth resource without OIDC"),
    }
}

pub(crate) fn same_origin_ui(h: &HeaderMap) -> bool {
    if h.get("x-mnemo-ui").and_then(|v| v.to_str().ok()) != Some("1") {
        return false;
    }
    match (
        h.get("origin").and_then(|v| v.to_str().ok()),
        h.get("host").and_then(|v| v.to_str().ok()),
    ) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin
            .split_once("://")
            .map(|(_, rest)| rest == host)
            .unwrap_or(false),
        (Some(_), None) => false,
    }
}

/// On a replica, writes that must be sequenced in one place go to the writer as-is, with the
/// caller's credentials; the writer authorizes them again.
async fn forward_writes(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let Some(writer) = app.writer.clone() else {
        return next.run(req).await;
    };
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let write_route = matches!(
        (method.as_str(), path.as_str()),
        (
            "POST",
            "/v1/memories" | "/v1/forget" | "/v1/entities" | "/v1/records"
        )
    ) || (method == "POST"
        && matches!(
            path.as_str(),
            "/v1/me/connections" | "/v1/admin/connections" | "/v1/admin/reembed"
        ))
        || (method == "DELETE" && path.starts_with("/v1/owners/"));
    if !write_route && !(method == "POST" && path == "/mcp") {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("body: {e}")),
    };
    if !write_route && !crate::mcp::writes(&bytes) {
        return next
            .run(Request::from_parts(parts, axum::body::Body::from(bytes)))
            .await;
    }
    let url = format!(
        "{writer}{}",
        parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or(&path)
    );
    let mut fwd = app.http.request(method, url).body(bytes);
    // Including X-Mnemo-Act-As: without it the writer sees the bare `service` key, which writes to
    // any scope.
    for name in [
        "authorization",
        "content-type",
        "cookie",
        "x-mnemo-ui",
        ACT_AS,
    ] {
        if let Some(v) = parts.headers.get(name) {
            fwd = fwd.header(name, v.as_bytes());
        }
    }
    match fwd.send().await {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ctype = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            (
                status,
                [("content-type", ctype)],
                r.bytes().await.unwrap_or_default(),
            )
                .into_response()
        }
        Err(e) => upstream("the writer", e),
    }
}

fn csv(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

async fn recall(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<RecallInput>,
) -> Response {
    match ops::recall(&app, &p, r).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

async fn remember(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<RememberReq>,
) -> Response {
    match ops::remember(&app, &p, r).await {
        Ok(out) => Json(out).into_response(),
        Err(e) => op_err(e),
    }
}

#[derive(Deserialize)]
struct CursorSet {
    name: String,
    value: i64,
}

#[derive(Deserialize)]
struct IngestReq {
    #[serde(default)]
    episodes: Vec<IngestItem>,
    /// Advances ONLY if the whole batch was stored.
    #[serde(default)]
    cursor: Option<CursorSet>,
}

async fn ingest(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<IngestReq>,
) -> Response {
    match ops::ingest(&app, &p, r.episodes, r.cursor.map(|c| (c.name, c.value))).await {
        Ok(n) => Json(json!({"stored": n})).into_response(),
        Err(e) => op_err(e),
    }
}

async fn get_cursor(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> Response {
    match ops::cursor(&app, &p, &name).await {
        Ok(v) => Json(json!({"name": name, "value": v})).into_response(),
        Err(e) => op_err(e),
    }
}

#[derive(Deserialize)]
struct EntitiesReq {
    entities: Vec<Entity>,
}

async fn upsert_entities(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<EntitiesReq>,
) -> Response {
    match ops::upsert_entities(&app, &p, r.entities).await {
        Ok(n) => Json(json!({"upserted": n})).into_response(),
        Err(e) => op_err(e),
    }
}

#[derive(Deserialize)]
struct ScopesQ {
    scopes: Option<String>,
    as_of: Option<i64>,
    around: Option<u32>,
    max: Option<usize>,
}

async fn get_entity(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ScopesQ>,
) -> Response {
    match ops::entity_card(&app, &p, &id, &csv(&q.scopes)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

async fn get_memory(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ScopesQ>,
) -> Response {
    match ops::memory(&app, &p, &id, &csv(&q.scopes)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

async fn get_episode(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ScopesQ>,
) -> Response {
    match ops::episode(&app, &p, &id, &csv(&q.scopes), q.around.unwrap_or(3)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

#[derive(Deserialize)]
struct ForgetReq {
    id: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    reason: String,
}

async fn forget(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<ForgetReq>,
) -> Response {
    match ops::forget(&app, &p, &r.id, &r.scopes, &r.reason).await {
        Ok(()) => Json(json!({"closed": r.id})).into_response(),
        Err(e) => op_err(e),
    }
}

async fn purge_owner(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match ops::purge(&app, &p, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

async fn globe(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Query(q): Query<ScopesQ>,
) -> Response {
    Json(ops::globe(
        &app,
        &p,
        &csv(&q.scopes),
        q.as_of,
        q.max.unwrap_or(3000),
    ))
    .into_response()
}

async fn stats(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    let distill = json!({"paused": app.distill_wait.lock().unwrap().is_some(), "waiting_for": *app.distill_wait.lock().unwrap()});
    if p.kind != crate::auth::Kind::Admin {
        let g = app.globe.read().unwrap();
        let own = p.own_scope();
        let (live, total) = g
            .scope_counts()
            .into_iter()
            .find(|c| c.0 == own)
            .map(|c| (c.1, c.2))
            .unwrap_or((0, 0));
        let entities = g.entity_count_in(&own);
        return Json(
            json!({"live": live, "memories": total, "entities": entities, "distill": distill}),
        )
        .into_response();
    }
    let (e, m) = app.globe.read().unwrap().counts();
    match app.db.stats().await {
        Ok(mut v) => {
            v["globe"] = json!({"entities": e, "memories": m});
            v["distill"] = distill;
            Json(v).into_response()
        }
        Err(x) => internal(x),
    }
}

#[derive(Deserialize)]
struct GalaxyQ {
    z: u32,
    x: u32,
    y: u32,
    scopes: Option<String>,
    #[serde(default)]
    old: Option<bool>,
}

async fn galaxy_tile(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Query(q): Query<GalaxyQ>,
) -> Response {
    let scopes = p.read_scopes(&csv(&q.scopes));
    if q.z > 22 || q.x >= (1u32 << q.z.min(22)) || q.y >= (1u32 << q.z.min(22)) {
        return err(StatusCode::BAD_REQUEST, "tile outside the map");
    }
    let g = app.globe.read().unwrap();
    Json(crate::galaxy::tile(
        &g,
        q.z,
        q.x,
        q.y,
        &scopes,
        q.old.unwrap_or(false),
    ))
    .into_response()
}

#[derive(Deserialize)]
struct KeepReq {
    #[serde(default)]
    scope: Option<String>,
    records: Vec<ops::KeepItem>,
}

async fn keep(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<KeepReq>,
) -> Response {
    match ops::keep(&app, &p, r.records, r.scope).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

#[derive(Deserialize)]
struct RecordsQ {
    key: String,
    kind: Option<String>,
    from: Option<String>,
    until: Option<String>,
    limit: Option<u32>,
    scopes: Option<String>,
}

/// Most recent first.
async fn records(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Query(q): Query<RecordsQ>,
) -> Response {
    let Some(raw) = app.raw.clone() else {
        return err(StatusCode::NOT_FOUND, "this server does not store records");
    };
    let scopes = p.read_scopes(&csv(&q.scopes));
    let key = crate::records::normalize_key(&q.key);
    let query = crate::records::Query {
        scopes: scopes.clone(),
        key: key.clone(),
        kind: q.kind.filter(|k| !k.is_empty()),
        from: q.from.as_deref().and_then(crate::query::parse_iso),
        until: q.until.as_deref().and_then(crate::query::parse_iso),
        limit: q.limit.unwrap_or(50).min(1000),
    };
    let subject = app
        .things
        .read()
        .unwrap()
        .values()
        .find(|s| s.key == key && scopes.contains(&s.scope))
        .cloned();
    match raw.records(&query).await {
        Ok(rs) => Json(json!({"subject": subject, "records": rs})).into_response(),
        Err(e) => internal(e),
    }
}

async fn turn(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(t): Json<ops::TurnInput>,
) -> Response {
    match ops::turn(&app, &p, t).await {
        Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Err(e) => op_err(e),
    }
}

#[derive(Deserialize)]
struct CostQ {
    days: Option<i64>,
    subject: Option<String>,
}

async fn cost(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Query(q): Query<CostQ>,
) -> Response {
    match ops::cost(&app, &p, q.days.unwrap_or(7), q.subject).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => op_err(e),
    }
}

async fn whoami(State(app): State<Arc<App>>, Extension(p): Extension<Principal>) -> Response {
    let via = match &p.via {
        crate::auth::Via::AdminKey => "admin_key",
        crate::auth::Via::Key(_) => "key",
        crate::auth::Via::Session(_) => "session",
        crate::auth::Via::OAuth(_) => "oauth",
        crate::auth::Via::Delegated(_) => "delegated",
    };
    // The subject is the IdP's opaque id; name and email are what make the person recognisable.
    let who = app.db.identity(&p.subject).await.ok().flatten();
    Json(
        // `version` lets the plugin flag an outdated client; only authenticated callers get it.
        json!({"kind": p.kind.as_str(), "subject": p.subject, "scopes": p.own_scopes(), "via": via, "version": crate::version(),
        "name": who.as_ref().map(|w| w.name.clone()).unwrap_or_default(),
        "email": who.as_ref().map(|w| w.email.clone()).unwrap_or_default()}),
    )
    .into_response()
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
    kind: String,
    subject: String,
    /// `None`: never expires.
    #[serde(default)]
    days: Option<i64>,
}

fn expiry(days: Option<i64>) -> i64 {
    days.filter(|d| *d > 0)
        .map(|d| crate::app::now_ms() + d.min(3650) * 86_400_000)
        .unwrap_or(0)
}

fn admin_only(p: &Principal) -> Option<Response> {
    (p.kind != Kind::Admin).then(|| err(StatusCode::FORBIDDEN, "only an admin manages keys"))
}

async fn create_key(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Json(r): Json<NewKey>,
) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    let Some(kind) = Kind::parse(&r.kind) else {
        return err(StatusCode::BAD_REQUEST, "kind: admin | service | user");
    };
    if r.subject.trim().is_empty() || r.subject.contains(':') {
        return err(StatusCode::BAD_REQUEST, "subject is empty or contains ':'");
    }
    match app
        .create_key(&r.name, kind, r.subject.trim(), expiry(r.days), &p.subject)
        .await
    {
        Ok((row, key)) => {
            Json(json!({"id": row.id, "key": key, "kind": row.kind, "subject": row.subject}))
                .into_response()
        }
        Err(e) => internal(e),
    }
}

async fn revoke_key(
    State(app): State<Arc<App>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    if let Some(e) = admin_only(&p) {
        return e;
    }
    match app.revoke_key(&id).await {
        Ok(true) => Json(json!({"revoked": id})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no active key with that id"),
        Err(e) => internal(e),
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

/// Served from here: the strict CSP blocks anything inline or from another origin.
/// (path, content type, content, versioned: changes with each release, so it is not cached).
pub(crate) const ASSETS: &[(&str, &str, &[u8], bool)] = &[
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/app.js"),
        true,
    ),
    (
        "/app.css",
        "text/css; charset=utf-8",
        include_bytes!("../web/app.css"),
        true,
    ),
    (
        "/theme.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/theme.js"),
        true,
    ),
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../web/manifest.webmanifest"),
        true,
    ),
    (
        "/fonts/geist.woff2",
        "font/woff2",
        include_bytes!("../web/fonts/geist.woff2"),
        false,
    ),
    (
        "/fonts/geist-mono.woff2",
        "font/woff2",
        include_bytes!("../web/fonts/geist-mono.woff2"),
        false,
    ),
    (
        "/icon.svg",
        "image/svg+xml",
        include_bytes!("../web/icons/icon.svg"),
        false,
    ),
    (
        "/favicon.ico",
        "image/x-icon",
        include_bytes!("../web/icons/favicon.ico"),
        false,
    ),
    (
        "/icon-192.png",
        "image/png",
        include_bytes!("../web/icons/icon-192.png"),
        false,
    ),
    (
        "/icon-512.png",
        "image/png",
        include_bytes!("../web/icons/icon-512.png"),
        false,
    ),
    (
        "/icon-maskable.png",
        "image/png",
        include_bytes!("../web/icons/maskable-512.png"),
        false,
    ),
    (
        "/apple-touch-icon.png",
        "image/png",
        include_bytes!("../web/icons/apple-touch-icon.png"),
        false,
    ),
];

async fn asset(uri: axum::http::Uri) -> Response {
    match ASSETS.iter().find(|a| a.0 == uri.path()) {
        Some(&(_, ctype, body, versioned)) => (
            [
                (header::CONTENT_TYPE, ctype),
                (
                    header::CACHE_CONTROL,
                    if versioned {
                        "no-cache"
                    } else {
                        "public, max-age=604800"
                    },
                ),
            ],
            body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub fn router(app: Arc<App>) -> Router {
    let mut r = Router::new().route("/", get(index));
    for a in ASSETS {
        r = r.route(a.0, get(asset));
    }
    r.route(
        "/.well-known/oauth-protected-resource",
        get(protected_resource),
    )
    .route(
        "/.well-known/oauth-protected-resource/mcp",
        get(protected_resource),
    )
    .route("/healthz", get(|| async { "ok" }))
    .route("/v1/whoami", get(whoami))
    .route("/v1/recall", post(recall))
    .route("/v1/memories", post(remember))
    .route("/v1/memories/{id}", get(get_memory))
    .route("/v1/forget", post(forget))
    .route("/v1/episodes", post(ingest))
    .route("/v1/episodes/{id}", get(get_episode))
    .route("/v1/cursors/{name}", get(get_cursor))
    .route("/v1/entities", post(upsert_entities))
    .route("/v1/entities/{id}", get(get_entity))
    .route("/v1/owners/{id}", delete(purge_owner))
    .route("/v1/globe", get(globe))
    .route("/v1/galaxy", get(galaxy_tile))
    .route("/v1/records", post(keep).get(records))
    .route("/v1/stats", get(stats))
    .route("/v1/turns", post(turn))
    .route("/v1/cost", get(cost))
    .route("/v1/keys", post(create_key).get(crate::admin::all_keys))
    .route("/v1/keys/{id}", delete(revoke_key))
    .route(
        "/v1/me/keys",
        get(crate::admin::my_keys).post(crate::admin::create_my_key),
    )
    .route("/v1/me/keys/{id}", delete(crate::admin::revoke_my_key))
    .route("/v1/admin/overview", get(crate::admin::overview))
    .route("/v1/admin/people", get(crate::admin::people))
    .route("/v1/admin/sessions", get(crate::admin::sessions))
    .route("/v1/admin/reembed", post(crate::admin::reembed))
    .route(
        "/v1/admin/connections",
        get(crate::admin::all_connections).post(crate::admin::set_any_connection),
    )
    .route(
        "/v1/me/connections",
        get(crate::admin::my_connections).post(crate::admin::set_my_connection),
    )
    .route(
        "/v1/me/sessions/{id}",
        delete(crate::admin::revoke_my_session),
    )
    .route(
        "/v1/admin/sessions/{id}",
        delete(crate::admin::revoke_session),
    )
    .route("/auth/config", get(crate::admin::auth_config))
    .route("/auth/login", get(crate::admin::login))
    .route("/auth/callback", get(crate::admin::callback))
    .route("/auth/logout", post(crate::admin::logout))
    .route("/mcp", post(crate::mcp::http).get(crate::mcp::http_get))
    // Order matters: the last layer is the outermost, so auth runs before forward_writes.
    .layer(middleware::from_fn_with_state(app.clone(), forward_writes))
    .layer(middleware::from_fn_with_state(app.clone(), auth))
    .layer(middleware::from_fn(security_headers))
    .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
    .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_writes_need_our_header_and_origin() {
        let mut h = HeaderMap::new();
        h.insert("host", "mnemo.example.com".parse().unwrap());
        assert!(!same_origin_ui(&h), "without X-Mnemo-UI, no");
        h.insert("x-mnemo-ui", "1".parse().unwrap());
        assert!(
            same_origin_ui(&h),
            "with the header and no Origin (same site), yes"
        );
        h.insert("origin", "https://mnemo.example.com".parse().unwrap());
        assert!(same_origin_ui(&h));
        h.insert("origin", "https://evil.example.com".parse().unwrap());
        assert!(!same_origin_ui(&h), "another origin, no");
    }

    #[test]
    fn bearer_and_csv() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer  mnemo_x ".parse().unwrap());
        assert_eq!(bearer(&h), "mnemo_x");
        assert_eq!(
            csv(&Some("global, user:a,".into())),
            vec!["global", "user:a"]
        );
    }

    #[test]
    fn the_page_works_under_a_strict_csp() {
        let html = include_str!("../web/index.html");
        assert!(
            !html.contains("<script>") && !html.contains("<style>"),
            "nothing inline"
        );
        assert!(html.contains(r#"<script src="app.js" defer></script>"#));
        assert!(html.contains(r#"<link rel="stylesheet" href="app.css">"#));
        for (name, text) in [
            ("index.html", html),
            ("app.js", include_str!("../web/app.js")),
        ] {
            assert!(
                !text.contains("style=\""),
                "{name}: inline style attributes"
            );
            for h in ["onclick=\"", "onload=\"", "onsubmit=\""] {
                assert!(!text.contains(h), "{name}: inline handler {h}");
            }
        }
    }

    #[tokio::test]
    async fn a_disconnected_app_is_refused_and_the_others_keep_working() {
        use crate::app::{App, Cfg};
        use crate::oidc::tests::{keypair, token};
        use crate::storage::RawStore;
        use crate::storage::mem::Mem;
        let db = Arc::new(Mem::new());
        let mut app = App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg {
                sso_only: true,
                ..Default::default()
            },
        );
        let oidc = crate::oidc::Oidc::new(crate::oidc::Cfg {
            issuer: "https://idp".into(),
            client_id: "mnemo".into(),
            base_url: "https://mnemo.example".into(),
            resource: "https://mnemo.example/mcp".into(),
            subject_claim: "sub".into(),
            groups_claim: "groups".into(),
            allowed_groups: vec!["mnemo-users".into()],
            ..Default::default()
        })
        .unwrap();
        let (k, set) = keypair("k1");
        oidc.set_keys_for_test(set);
        app.oidc = Some(oidc);
        let app = Arc::new(app);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let a2 = app.clone();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(a2).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let now = crate::app::now_ms() / 1000;
        let tok = |client: &str| {
            token(
                &k,
                "k1",
                json!({"iss": "https://idp", "aud": "https://mnemo.example/mcp", "sub": "ana", "groups": ["mnemo-users"], "client_id": client, "iat": now, "exp": now + 600}),
            )
        };
        let (grok, cli) = (
            tok("https://grok.com/oauth/mcp-client.json"),
            tok("mnemo-cli"),
        );
        let c = reqwest::Client::new();
        let get = |t: String| {
            let c = c.clone();
            async move {
                c.get(format!("http://{addr}/v1/whoami"))
                    .bearer_auth(t)
                    .header("user-agent", "Grok/1.0")
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        assert_eq!(get(grok.clone()).await, 200);
        assert_eq!(get(cli.clone()).await, 200);
        let apps = app.connections_of(Some("ana")).await;
        assert_eq!(apps.len(), 2, "both apps are recorded");
        assert!(apps.iter().any(|c| c.agent == "Grok/1.0"));
        let r = c
            .post(format!("http://{addr}/v1/me/connections"))
            .bearer_auth(cli.clone())
            .json(&json!({"client": "https://grok.com/oauth/mcp-client.json", "blocked": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert!(
            app.set_connection_blocked("ana", "https://grok.com/oauth/mcp-client.json", true)
                .await
                .unwrap()
        );
        assert_eq!(get(grok.clone()).await, 403);
        assert_eq!(get(cli.clone()).await, 200);
        // A stale row write (another instance, a race) plus a reload must not undo the block.
        let stale = apps
            .iter()
            .find(|c| c.client.starts_with("https://grok"))
            .unwrap()
            .clone();
        app.db
            .put_connection(&crate::storage::ConnectionRow {
                last_ms: crate::app::now_ms(),
                ..stale
            })
            .await
            .unwrap();
        app.forget_connections().await;
        assert_eq!(
            get(grok.clone()).await,
            403,
            "the block survives reloading the database"
        );
        app.set_connection_blocked("ana", "https://grok.com/oauth/mcp-client.json", false)
            .await
            .unwrap();
        assert_eq!(get(grok).await, 200);
        assert!(
            !app.set_connection_blocked("ana", "never-seen", true)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_page_assets_are_public_and_typed() {
        use crate::app::{App, Cfg};
        use crate::storage::RawStore;
        use crate::storage::mem::Mem;
        let db = Arc::new(Mem::new());
        let app = App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg {
                sso_only: true,
                ..Default::default()
            },
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(Arc::new(app)).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let c = reqwest::Client::new();
        let html = include_str!("../web/index.html");
        for (path, ctype, _, versioned) in ASSETS {
            let r = c.get(format!("http://{addr}{path}")).send().await.unwrap();
            assert_eq!(r.status(), 200, "{path}");
            assert_eq!(r.headers()["content-type"], *ctype, "{path}");
            let cache = r.headers()["cache-control"].to_str().unwrap();
            assert_eq!(cache == "no-cache", *versioned, "{path}: {cache}");
            assert!(!r.bytes().await.unwrap().is_empty(), "{path}");
        }
        for path in [
            "app.js",
            "app.css",
            "theme.js",
            "manifest.webmanifest",
            "icon.svg",
            "favicon.ico",
            "apple-touch-icon.png",
            "fonts/geist.woff2",
        ] {
            assert!(
                html.contains(&format!("\"{path}\"")),
                "index.html does not request {path}"
            );
            assert!(ASSETS.iter().any(|a| a.0 == format!("/{path}")), "{path}");
        }
        let css = include_str!("../web/app.css");
        assert!(
            css.contains("url(\"fonts/geist.woff2\")")
                && css.contains("url(\"fonts/geist-mono.woff2\")")
        );
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../web/manifest.webmanifest")).unwrap();
        for icon in manifest["icons"].as_array().unwrap() {
            let src = icon["src"].as_str().unwrap();
            assert!(
                ASSETS.iter().any(|a| a.0 == format!("/{src}")),
                "the manifest requests {src}, which is not served"
            );
        }
        assert_eq!(
            c.get(format!("http://{addr}/v1/stats"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }

    #[tokio::test]
    async fn sso_only_rejects_keys_and_points_mcp_clients_to_the_provider() {
        use crate::app::{App, Cfg};
        use crate::storage::RawStore;
        use crate::storage::mem::Mem;
        let db = Arc::new(Mem::new());
        let mut app = App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg {
                sso_only: true,
                admin_key: String::new(),
                ..Default::default()
            },
        );
        app.oidc = Some(
            crate::oidc::Oidc::new(crate::oidc::Cfg {
                issuer: "https://id.example".into(),
                client_id: "mnemo".into(),
                base_url: "https://mnemo.example".into(),
                scopes: "openid email profile groups".into(),
                resource: "https://mnemo.example/mcp".into(),
                ..Default::default()
            })
            .unwrap(),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(Arc::new(app)).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let c = reqwest::Client::new();
        let base = format!("http://{addr}");
        let r = c
            .post(format!("{base}/mcp"))
            .bearer_auth("mnemo_abc")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let www = r.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            www,
            r#"Bearer resource_metadata="https://mnemo.example/.well-known/oauth-protected-resource/mcp""#
        );
        let r = c
            .get(format!("{base}/v1/stats"))
            .bearer_auth("a.b.c")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let r = c.get(format!("{base}/v1/me/keys")).send().await.unwrap();
        assert_eq!(r.status(), 403);
        let m: serde_json::Value = c
            .get(format!("{base}/.well-known/oauth-protected-resource/mcp"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(m["resource"], "https://mnemo.example/mcp");
        assert_eq!(m["authorization_servers"][0], "https://id.example");
        let cfg: serde_json::Value = c
            .get(format!("{base}/auth/config"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            cfg["keys"], false,
            "the web page does not offer key sign-in"
        );
        let r = c.post(format!("{base}/auth/logout")).send().await.unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .post(format!("{base}/auth/logout"))
            .header("x-mnemo-ui", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = c.get(format!("{base}/healthz")).send().await.unwrap();
        assert!(
            r.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    }

    #[tokio::test]
    async fn an_oauth_token_reaches_memory_but_not_the_account() {
        use crate::app::{App, Cfg};
        use crate::storage::RawStore;
        use crate::storage::mem::Mem;
        let db = Arc::new(Mem::new());
        let mut app = App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg {
                sso_only: true,
                admin_key: String::new(),
                ..Default::default()
            },
        );
        let oidc = crate::oidc::Oidc::new(crate::oidc::Cfg {
            issuer: "https://id.example".into(),
            client_id: "mnemo".into(),
            base_url: "https://mnemo.example".into(),
            scopes: "openid email profile groups".into(),
            resource: "https://mnemo.example/mcp".into(),
            subject_claim: "sub".into(),
            groups_claim: "groups".into(),
            ..Default::default()
        })
        .unwrap();
        let (k, set) = crate::oidc::tests::keypair("k1");
        oidc.set_keys_for_test(set);
        app.oidc = Some(oidc);
        let now = crate::app::now_ms() / 1000;
        let tok = crate::oidc::tests::token(
            &k,
            "k1",
            serde_json::json!({"iss": "https://id.example", "aud": "https://mnemo.example/mcp", "sub": "u-1",
                "email": "ana@example.com", "email_verified": true, "groups": ["mnemo-users"], "exp": now + 300,
                "iat": now, "client_id": "some-mcp-client"}),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(Arc::new(app)).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let c = reqwest::Client::new();
        let base = format!("http://{addr}");
        let r = c
            .get(format!("{base}/v1/whoami"))
            .bearer_auth(&tok)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "the token works for memory");
        for (method, path) in [
            ("DELETE", "/v1/owners/u-1"),
            ("GET", "/v1/me/connections"),
            ("DELETE", "/v1/me/sessions/x"),
            ("GET", "/v1/admin/overview"),
        ] {
            let r = c
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .bearer_auth(&tok)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{method} {path}");
        }
    }

    #[test]
    fn oauth_routes_match_whole_segments() {
        assert!(oauth_route_allowed("/mcp") && oauth_route_allowed("/v1/memories/m1"));
        assert!(!oauth_route_allowed("/v1/owners/u-1") && !oauth_route_allowed("/v1/me/keys"));
        assert!(!oauth_route_allowed("/v1/recallx") && !oauth_route_allowed("/mcpx"));
    }

    /// Localhost included: behind a proxy or a tunnel, "local" could be anyone.
    #[tokio::test]
    async fn without_credentials_nothing_opens_not_even_from_localhost() {
        use crate::app::{App, Cfg};
        use crate::storage::RawStore;
        use crate::storage::mem::Mem;
        let db = Arc::new(Mem::new());
        let app = App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg::default(),
        );
        assert!(app.principal("").await.is_none());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(Arc::new(app)).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let r = reqwest::Client::new()
            .get(format!("http://{addr}/v1/whoami"))
            .header("host", "127.0.0.1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }

    #[test]
    fn an_oauth_token_never_administers() {
        let p = oauth_principal(crate::oidc::Identity {
            subject: "s".into(),
            email: String::new(),
            name: String::new(),
            admin: true,
            client: "x".into(),
        });
        assert_eq!(p.kind, Kind::User);
        assert_eq!(p.via, crate::auth::Via::OAuth("s".into()));
    }

    async fn serve_app(app: crate::app::App) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(Arc::new(app)).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        format!("http://{addr}")
    }

    fn mem_app(db: Arc<crate::storage::mem::Mem>, cfg: crate::app::Cfg) -> crate::app::App {
        use crate::storage::RawStore;
        crate::app::App::new(
            db.clone(),
            Some(db as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            cfg,
        )
    }

    #[tokio::test]
    async fn a_service_key_acts_for_one_person_and_nobody_else_can() {
        use crate::app::Cfg;
        use crate::storage::{Db, SessionRow};
        let db = Arc::new(crate::storage::mem::Mem::new());
        let app = mem_app(
            db.clone(),
            Cfg {
                admin_key: "adm-key".into(),
                ..Default::default()
            },
        );
        let (_, svc) = app
            .create_key("plane", Kind::Service, "plane", 0, "test")
            .await
            .unwrap();
        let (_, ana_key) = app
            .create_key("ana", Kind::User, "ana", 0, "test")
            .await
            .unwrap();
        let now = crate::app::now_ms();
        db.create_session(&SessionRow {
            id: "s1".into(),
            hash: crate::auth::hash("carol-cookie"),
            subject: "carol".into(),
            kind: "user".into(),
            created_ms: now,
            expires_ms: now + 3_600_000,
            ..Default::default()
        })
        .await
        .unwrap();
        let base = serve_app(app).await;
        let c = reqwest::Client::new();
        let get = |path: &str, key: &str, act: Option<&str>| {
            let mut r = c.get(format!("{base}{path}"));
            if !key.is_empty() {
                r = r.bearer_auth(key);
            }
            if let Some(a) = act {
                r = r.header(ACT_AS, a);
            }
            r
        };

        let r = c
            .post(format!("{base}/v1/memories"))
            .bearer_auth(&svc)
            .json(&json!({"scope": "user:ana", "what": "ana keeps the deploy keys in the blue folder"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let ana_mem = r.json::<serde_json::Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let path = format!("/v1/memories/{ana_mem}?scopes=user:ana");
        let r = get(&path, &svc, None).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            r.headers().get(ACTING_AS).is_none(),
            "no X-Mnemo-Acting-As without delegation"
        );
        let who: serde_json::Value = get("/v1/whoami", &svc, None)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            who["kind"], "service",
            "without the header it is still the service"
        );

        let r = get("/v1/whoami", &svc, Some("bob")).send().await.unwrap();
        assert_eq!(
            r.headers().get(ACTING_AS).and_then(|v| v.to_str().ok()),
            Some("bob")
        );
        let who: serde_json::Value = r.json().await.unwrap();
        assert_eq!(who["kind"], "user");
        assert_eq!(who["subject"], "bob");
        assert_eq!(who["via"], "delegated");
        assert_eq!(who["scopes"], json!(["user:bob"]));
        assert_eq!(
            get(&path, &svc, Some("bob")).send().await.unwrap().status(),
            404,
            "bob does not see ana's memory even when asking for her scope"
        );
        let write = |scope: &str| {
            c.post(format!("{base}/v1/memories"))
                .bearer_auth(&svc)
                .header(ACT_AS, "bob")
                .json(&json!({"scope": scope, "what": "bob prefers the night shift on fridays"}))
        };
        assert_eq!(write("user:ana").send().await.unwrap().status(), 403);
        let r = write("user:bob").send().await.unwrap();
        assert_eq!(r.status(), 200);
        let bob_mem = r.json::<serde_json::Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mine = get(&format!("/v1/memories/{bob_mem}"), &svc, Some("bob"))
            .send()
            .await
            .unwrap();
        assert_eq!(mine.status(), 200);
        assert_eq!(
            mine.headers().get(ACTING_AS).and_then(|v| v.to_str().ok()),
            Some("bob")
        );

        for (who, key) in [
            ("user", ana_key.as_str()),
            ("admin", "adm-key"),
            ("nobody", ""),
        ] {
            let r = get("/v1/whoami", key, Some("bob")).send().await.unwrap();
            assert_eq!(r.status(), 403, "{who}");
        }
        let r = c
            .get(format!("{base}/v1/whoami"))
            .header("cookie", "mnemo_session=carol-cookie")
            .header(ACT_AS, "bob")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "session");
        let r = c
            .get(format!("{base}/v1/whoami"))
            .header("cookie", "mnemo_session=carol-cookie")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);

        let long = "x".repeat(257);
        for bad in ["", "user:bob", "a,b", "a b", "a\tb", long.as_str()] {
            let r = get("/v1/whoami", &svc, Some(bad)).send().await.unwrap();
            assert_eq!(r.status(), 400, "{bad:?}");
        }
        assert_eq!(
            get("/v1/whoami", &svc, Some(&"x".repeat(256)))
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "256 bytes fit"
        );
        let r = c
            .get(format!("{base}/v1/whoami"))
            .bearer_auth(&svc)
            .header(ACT_AS, "bob")
            .header(ACT_AS, "ana")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "two subjects");

        for (method, path) in [
            ("GET", "/v1/keys"),
            ("DELETE", "/v1/keys/k1"),
            ("GET", "/v1/me/keys"),
            ("GET", "/v1/me/connections"),
            ("DELETE", "/v1/me/sessions/s1"),
            ("GET", "/v1/admin/overview"),
            ("DELETE", "/v1/owners/bob"),
            ("GET", "/v1/cost"),
            ("GET", "/v1/cursors/x"),
            ("POST", "/v1/turns"),
            ("POST", "/mcp"),
        ] {
            let r = c
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .bearer_auth(&svc)
                .header(ACT_AS, "bob")
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{method} {path}");
        }
        for path in ["/v1/stats", "/v1/galaxy?z=0&x=0&y=0", "/v1/globe"] {
            let r = get(path, &svc, Some("bob")).send().await.unwrap();
            assert_eq!(r.status(), 200, "{path}");
        }
    }

    #[test]
    fn delegated_routes_match_whole_segments() {
        assert!(delegated_route_refused("/v1/me") && delegated_route_refused("/v1/me/keys"));
        assert!(delegated_route_refused("/mcp"));
        assert!(!delegated_route_refused("/v1/memories/m1") && !delegated_route_refused("/v1/mem"));
        assert!(!delegated_route_refused("/v1/costs") && !delegated_route_refused("/mcpx"));
    }

    #[tokio::test]
    async fn a_delegated_write_through_a_replica_stays_delegated() {
        use crate::app::Cfg;
        let db = Arc::new(crate::storage::mem::Mem::new());
        let writer = mem_app(db.clone(), Cfg::default());
        let (_, svc) = writer
            .create_key("plane", Kind::Service, "plane", 0, "test")
            .await
            .unwrap();
        let writer_url = serve_app(writer).await;
        let mut replica = mem_app(db, Cfg::default());
        replica.writer = Some(writer_url);
        let base = serve_app(replica).await;
        let r = reqwest::Client::new()
            .post(format!("{base}/v1/memories"))
            .bearer_auth(&svc)
            .header(ACT_AS, "bob")
            .json(&json!({"scope": "user:ana", "what": "written by someone acting for bob"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            403,
            "the writer also treats it as bob's: it does not write to ana's scope"
        );
    }

    /// An absolute path would escape a prefix stripped by a proxy (e.g. served at /mnemo/).
    #[test]
    fn every_web_path_is_relative() {
        let files = [
            ("index.html", include_str!("../web/index.html")),
            ("app.js", include_str!("../web/app.js")),
            ("app.css", include_str!("../web/app.css")),
            ("theme.js", include_str!("../web/theme.js")),
            (
                "manifest.webmanifest",
                include_str!("../web/manifest.webmanifest"),
            ),
        ];
        let absolute = [
            "src=\"/",
            "href=\"/",
            "action=\"/",
            "url(/",
            "url(\"/",
            "url('/",
            "fetch(\"/",
            "fetch('/",
            "fetch(`/",
            "api(\"/",
            "api('/",
            "api(`/",
            "location.href = \"/",
            "location.assign(\"/",
            "\"start_url\": \"/",
            "\"scope\": \"/",
            "\"src\": \"/",
        ];
        for (name, text) in files {
            for a in absolute {
                assert!(!text.contains(a), "{name} has an absolute path: {a}");
            }
        }
    }

    #[test]
    fn the_page_works_under_a_prefix() {
        let html = include_str!("../web/index.html");
        let base = reqwest::Url::parse("http://plane.example/mnemo/").unwrap();
        let mut seen = 0;
        for attr in ["src=\"", "href=\""] {
            for part in html.split(attr).skip(1) {
                let target = part.split('"').next().unwrap();
                if target.starts_with('#') {
                    continue;
                }
                let url = base.join(target).unwrap();
                let rest = url
                    .path()
                    .strip_prefix("/mnemo")
                    .unwrap_or_else(|| panic!("{target} escapes the prefix"));
                if rest.starts_with("/auth/") {
                    continue;
                }
                assert!(
                    ASSETS.iter().any(|a| a.0 == rest),
                    "{target} → {rest}, which is not served"
                );
                seen += 1;
            }
        }
        assert!(
            seen >= 8,
            "only {seen} paths: did the index.html format change?"
        );
    }
}
