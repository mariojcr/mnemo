// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Shared service state and the operations that span several parts.

use crate::auth::{self, Kind, Principal, Via};
use crate::distill::{self, Ctx};
use crate::episode::Episode;
use crate::globe::{Globe, RecallOpts, RecallOut};
use crate::judge::{self, Judge, Outcome, Verdict};
use crate::llm::Llm;
use crate::meter;
use crate::model::{Entity, Link, Memory, Role, W};
use crate::query::DAY_MS;
use crate::storage::{ApplyError, Db, KeyRow, RawStore, Thread, WritePlan};
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub type Res<T> = Result<T, String>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Default)]
struct Trail {
    /// (instant, entity cursor, memory cursor) per read. The next floor is the last mark older than
    /// one window: a single writer hands out `seq` in order, so everything below it is visible.
    marks: std::collections::VecDeque<(i64, i64, i64)>,
    /// Fetched `seq` above the floor, sent as ranges: a list of 100k would not fit in a URL.
    seen: std::collections::BTreeSet<i64>,
}

impl Trail {
    async fn floor(&mut self, db: &dyn Db, now: i64, window: i64) -> Res<(i64, i64)> {
        if self.marks.is_empty() {
            let f = db.log_floor(now - window).await?;
            self.marks.push_back((now - window, f, f));
        }
        while self.marks.len() > 1 && self.marks[1].0 <= now - window {
            self.marks.pop_front();
        }
        let (_, ef, mf) = self.marks[0];
        self.seen = self.seen.split_off(&(ef.min(mf) + 1));
        Ok((ef, mf))
    }

    fn ranges(&self) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = Vec::new();
        for &q in &self.seen {
            match out.last_mut() {
                Some((_, b)) if *b + 1 == q => *b = q,
                _ => out.push((q, q)),
            }
        }
        out
    }
}

#[derive(Clone, Debug, Default)]
pub struct Cfg {
    /// Never stored in the database.
    pub admin_key: String,
    /// Neither database keys nor the admin key: only a browser session or an OAuth token.
    pub sso_only: bool,
    pub extract_model: String,
    /// Tokens; 0 = 16384.
    pub llm_context: u32,
    /// Short passes keep outputs short: at ~12 tokens/s a 4000-token output takes 5 minutes.
    pub extract_budget_tokens: u32,
    /// 0 = 600.
    pub llm_timeout_s: u64,
    /// Per thread; 0 = 4. With 4 threads at once that is up to 16 sequences on the model server.
    pub distill_parallel: u32,
    pub embed_model: String,
    pub settle_ms: i64,
    /// Only for the automatic surfaces: `recall` still answers. 0 = no limit.
    pub session_budget_tokens: u64,
    /// Judged this many times without ever being used: no longer injected automatically. 0 = never.
    pub mute_after: u64,
    /// Fraction of turns the LLM judges when there is no decisions service (which judges all).
    pub eval_sample: f64,
    /// Per million tokens: model → (input, cache read, cache write, output).
    pub prices: HashMap<String, [f64; 4]>,
}

pub struct App {
    pub db: Arc<dyn Db>,
    pub raw: Option<Arc<dyn RawStore>>,
    pub llm: Option<Llm>,
    pub embedder: Option<Embedder>,
    pub judge: Judge,
    /// Trusted proxies in front: the client address is the one the closest of them wrote in
    /// `X-Forwarded-For`. 0 = ignore the header.
    pub forwarded_hops: usize,
    pub writer: Option<String>,
    pub http: reqwest::Client,
    pub cfg: Cfg,
    pub oidc: Option<crate::oidc::Oidc>,
    /// Shown in the dashboard: no secrets.
    pub info: serde_json::Value,
    pub globe: RwLock<Globe>,
    /// Muted per person, not globally: otherwise anyone reading a shared memory could mute it for
    /// everyone.
    pub muted: RwLock<HashSet<(String, String)>>,
    pub things: RwLock<HashMap<String, crate::records::Subject>>,
    refresh_lock: tokio::sync::Mutex<Trail>,
    refreshed_head: Mutex<Option<u64>>,
    keys: Mutex<HashMap<String, (Cached, Instant)>>,
    /// subject|email|name → when: one row per hour, not per request.
    seen_identities: Mutex<HashMap<String, Instant>>,
    touched: Mutex<HashMap<String, Instant>>,
    logins: Mutex<HashMap<String, std::collections::VecDeque<Instant>>>,
    /// `model`, `embedder` or `judge`; the details with the URL stay in the log.
    pub distill_wait: Mutex<Option<&'static str>>,
    /// Reloaded at most once a minute; every token request checks it for a disconnected app.
    conns: tokio::sync::Mutex<ConnCache>,
    names: Mutex<NameCache>,
}

#[derive(Default)]
struct NameCache {
    vecs: HashMap<String, Vec<f32>>,
    asked: HashSet<String>,
}

/// Blocks are kept apart from the activity rows: an activity write never touches them.
#[derive(Default)]
struct ConnCache {
    at: Option<Instant>,
    rows: HashMap<(String, String), crate::storage::ConnectionRow>,
    blocked: HashMap<(String, String), i64>,
}

/// Per client, so a flood does not lock everyone else out. [`LOGINS_PER_MINUTE`] caps the total.
pub const LOGINS_PER_CLIENT_MINUTE: usize = 10;
/// Each attempt writes a row to the logins table.
pub const LOGINS_PER_MINUTE: usize = 600;

/// By rank, no cosine threshold: related memories span cosine 0.81–0.91 and unrelated ones sit
/// around 0.81. The three nearest cover 95% of 106 gold relations, against 90% with a threshold.
const JUDGE_VECTOR_NEIGHBORS: usize = 3;

type Cached = Option<(Principal, i64)>;

#[derive(serde::Deserialize, Default, Debug)]
pub struct RememberReq {
    pub scope: String,
    #[serde(default)]
    pub owner_id: String,
    #[serde(default)]
    pub reader_scopes: Vec<String>,
    pub what: String,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub how: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub who: Vec<String>,
    #[serde(default, rename = "where")]
    pub where_: Vec<String>,
    #[serde(default)]
    pub about: Vec<String>,
    #[serde(default)]
    pub occurred_at: Option<i64>,
    #[serde(default)]
    pub episode_id: Option<String>,
}

#[derive(serde::Serialize, Debug)]
pub struct RememberOut {
    pub id: String,
    /// inserted | duplicate | superseded | candidate
    pub outcome: String,
    pub closed: Vec<String>,
    pub judge: String,
}

/// A thread row is keyed `{thread}|{scope}`, and a subject may itself hold a `|` (`auth0|123`):
/// the known scope is cut off the end, not the last `|`.
fn thread_parts(th: &Thread) -> (String, String) {
    if !th.scope.is_empty()
        && let Some(t) = th.thread.strip_suffix(&format!("|{}", th.scope))
    {
        return (t.to_string(), th.scope.clone());
    }
    th.thread
        .rsplit_once('|')
        .map(|(t, s)| (t.to_string(), s.to_string()))
        .unwrap_or((th.thread.clone(), th.scope.clone()))
}

/// EmbeddingGemma's trained retrieval prompts: without them query-to-memory similarity is worse.
pub fn embed_prefix(query: bool) -> &'static str {
    if query {
        "task: search result | query: "
    } else {
        "title: none | text: "
    }
}

impl App {
    pub fn new(
        db: Arc<dyn Db>,
        raw: Option<Arc<dyn RawStore>>,
        llm: Option<Llm>,
        embedder: Option<Embedder>,
        judge: Judge,
        cfg: Cfg,
    ) -> App {
        App {
            db,
            raw,
            llm,
            embedder,
            judge,
            forwarded_hops: 0,
            writer: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
            cfg,
            globe: RwLock::new(Globe::default()),
            muted: RwLock::new(HashSet::new()),
            things: RwLock::new(HashMap::new()),
            oidc: None,
            info: serde_json::Value::Null,
            refresh_lock: tokio::sync::Mutex::new(Trail::default()),
            refreshed_head: Mutex::new(None),
            keys: Mutex::new(HashMap::new()),
            seen_identities: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
            logins: Mutex::new(HashMap::new()),
            distill_wait: Mutex::new(None),
            conns: tokio::sync::Mutex::new(ConnCache::default()),
            names: Mutex::new(NameCache::default()),
        }
    }

    /// Cached for a minute, so a revoked key works up to that long; expiry is checked every time.
    pub async fn principal(&self, bearer: &str) -> Option<Principal> {
        if bearer.is_empty() {
            return None;
        }
        if !self.cfg.admin_key.is_empty()
            && auth::same(bearer.as_bytes(), self.cfg.admin_key.as_bytes())
        {
            return Some(Principal {
                kind: Kind::Admin,
                subject: "admin".into(),
                via: Via::AdminKey,
            });
        }
        let h = auth::hash(bearer);
        let cached = self.cached(&h);
        let found = match cached {
            Some(c) => c,
            None => {
                let found = match self.db.key_by_hash(&h).await {
                    Ok(Some(k)) if !k.revoked => Kind::parse(&k.kind).map(|kind| {
                        (
                            Principal {
                                kind,
                                subject: k.subject.clone(),
                                via: Via::Key(k.id.clone()),
                            },
                            k.expires_ms,
                        )
                    }),
                    Ok(_) => None,
                    Err(e) => {
                        warn!("keys: {e}");
                        return None;
                    }
                };
                // Misses are not cached: anyone can send made-up keys nonstop.
                if found.is_some() {
                    self.remember_principal(h, found.clone());
                }
                found
            }
        };
        let (p, expires) = found?;
        if expires != 0 && now_ms() >= expires {
            return None;
        }
        if let Via::Key(id) = &p.via {
            self.touch(id);
        }
        Some(p)
    }

    pub async fn session_principal(&self, cookie: &str) -> Option<Principal> {
        if cookie.is_empty() {
            return None;
        }
        let h = format!("s:{}", auth::hash(cookie));
        let found = match self.cached(&h) {
            Some(c) => c,
            None => {
                let found = match self.db.session_by_hash(&h[2..]).await {
                    Ok(Some(x)) if !x.revoked => Some((
                        Principal {
                            kind: if x.kind == "admin" {
                                Kind::Admin
                            } else {
                                Kind::User
                            },
                            subject: x.subject.clone(),
                            via: Via::Session(x.id.clone()),
                        },
                        x.expires_ms,
                    )),
                    Ok(_) => None,
                    Err(e) => {
                        warn!("sessions: {e}");
                        return None;
                    }
                };
                self.remember_principal(h, found.clone());
                found
            }
        };
        let (p, expires) = found?;
        (now_ms() < expires).then_some(p)
    }

    /// A failed reload keeps the old list: stale beats letting a disconnected application in.
    async fn conns(&self) -> tokio::sync::MutexGuard<'_, ConnCache> {
        let mut g = self.conns.lock().await;
        if g.at.is_none_or(|at| at.elapsed() > Duration::from_secs(60)) {
            match (self.db.connections().await, self.db.blocks().await) {
                (Ok(rows), Ok(blocks)) => {
                    g.rows = rows
                        .into_iter()
                        .map(|c| ((c.subject.clone(), c.client.clone()), c))
                        .collect();
                    g.blocked = blocks.into_iter().map(|(s, c, b)| ((s, c), b)).collect();
                    g.at = Some(Instant::now());
                }
                (Err(e), _) | (_, Err(e)) => warn!("connections: {e}"),
            }
        }
        g
    }

    pub async fn connection_blocked(&self, subject: &str, client: &str) -> bool {
        self.conns()
            .await
            .blocked
            .get(&(subject.to_string(), client.to_string()))
            .is_some_and(|b| *b > 0)
    }

    /// At most every five minutes even if the User-Agent changed: the client sets it, so writing a
    /// row per change would let it cost one insert per request.
    pub async fn note_connection(&self, subject: &str, client: &str, agent: &str) {
        if subject.is_empty() || client.is_empty() {
            return;
        }
        let agent: String = agent
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        let now = now_ms();
        let row = {
            let mut g = self.conns().await;
            let key = (subject.to_string(), client.to_string());
            let c = g
                .rows
                .entry(key)
                .or_insert_with(|| crate::storage::ConnectionRow {
                    subject: subject.to_string(),
                    client: client.to_string(),
                    first_ms: now,
                    ..Default::default()
                });
            if c.last_ms > now - 300_000 {
                return;
            }
            c.last_ms = now;
            c.agent = agent;
            c.clone()
        };
        if let Err(e) = self.db.put_connection(&row).await {
            warn!("connection: {e}");
        }
    }

    pub async fn connections_of(
        &self,
        subject: Option<&str>,
    ) -> Vec<crate::storage::ConnectionRow> {
        let g = self.conns().await;
        let mut v: Vec<_> = g
            .rows
            .values()
            .filter(|c| subject.is_none_or(|s| c.subject == s))
            .map(|c| crate::storage::ConnectionRow {
                blocked_ms: g
                    .blocked
                    .get(&(c.subject.clone(), c.client.clone()))
                    .copied()
                    .unwrap_or(0),
                ..c.clone()
            })
            .collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.last_ms));
        v
    }

    pub async fn set_connection_blocked(
        &self,
        subject: &str,
        client: &str,
        blocked: bool,
    ) -> Res<bool> {
        let key = (subject.to_string(), client.to_string());
        let mut g = self.conns().await;
        if !g.rows.contains_key(&key) {
            return Ok(false);
        }
        let at = if blocked { now_ms() } else { 0 };
        self.db.put_block(subject, client, at).await?;
        g.blocked.insert(key, at);
        Ok(true)
    }

    pub async fn merge_duplicate_entities(&self) -> usize {
        let groups = self.globe.read().unwrap().duplicate_groups();
        self.merge_groups(groups).await
    }

    async fn merge_groups(&self, groups: Vec<(Entity, Vec<Entity>)>) -> usize {
        if groups.is_empty() {
            return 0;
        }
        let mut rows = Vec::new();
        let mut merged = 0;
        for (mut keep, drop) in groups {
            for d in &drop {
                for a in std::iter::once(&d.name).chain(d.aliases.iter()) {
                    if !keep.aliases.iter().any(|x| x.eq_ignore_ascii_case(a))
                        && !a.eq_ignore_ascii_case(&keep.name)
                    {
                        keep.aliases.push(a.clone());
                    }
                }
                let mut d = d.clone();
                d.same_as = Some(keep.id.clone());
                info!(
                    "entities: {:?} ({:?}) is {:?} ({:?})",
                    d.id, d.kind, keep.id, keep.kind
                );
                rows.push(d);
                merged += 1;
            }
            keep.aliases.truncate(20);
            rows.push(keep);
        }
        match self.db.upsert_entities(&rows, now_ms()).await {
            Ok(_) => {
                let _ = self.refresh().await;
                merged
            }
            Err(e) => {
                warn!("merging entities: {e}");
                0
            }
        }
    }

    /// The name vector only proposes: alone it cannot tell "cortafuegos ~ firewall" (0.87) from
    /// "firewall ~ reverse proxy" (0.88) or "mnemo ~ mnemo login" (0.94). The judge decides.
    pub async fn merge_translated_entities(&self) -> usize {
        const NAME_SIM: f32 = 0.80;
        // The two most similar, not just the first: "reverse proxy" (0.875) is closer to "firewall"
        // than "cortafuegos" (0.870) is.
        const NAME_PROPOSALS: usize = 2;
        const NAME_BUCKET_MAX: usize = 500;
        const NAME_ASK_MAX: usize = 50;
        let ents = self.globe.read().unwrap().mergeable_entities();
        if ents.len() < 2 {
            return 0;
        }
        let key = |e: &Entity| format!("{}\u{1f}{}", e.id, e.name);
        let mut sizes: HashMap<(&str, &str), usize> = HashMap::new();
        for (e, _) in &ents {
            *sizes.entry((&e.scope, &e.kind)).or_default() += 1;
        }
        let missing: Vec<(String, String)> = {
            let c = self.names.lock().unwrap();
            ents.iter()
                .filter(|(e, _)| {
                    let n = sizes[&(e.scope.as_str(), e.kind.as_str())];
                    (2..=NAME_BUCKET_MAX).contains(&n) && !c.vecs.contains_key(&key(e))
                })
                .map(|(e, _)| (key(e), format!("{}: {}", e.kind, e.name)))
                .collect()
        };
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|(_, t)| t.clone()).collect();
            match self
                .embed_checked("embed_entity", &texts, Duration::from_secs(60))
                .await
            {
                Ok(Some(vs)) => {
                    let mut c = self.names.lock().unwrap();
                    for ((k, _), v) in missing.into_iter().zip(vs) {
                        c.vecs.insert(k, v);
                    }
                }
                Ok(None) => return 0,
                Err(e) => {
                    warn!("translated names: {e}");
                    return 0;
                }
            }
        }
        // Only within each (scope, kind) group, skipping huge ones: anyone can create entities in
        // their scope, and pairwise comparison every ten minutes is quadratic.
        let mut buckets: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, (e, _)) in ents.iter().enumerate() {
            buckets.entry((&e.scope, &e.kind)).or_default().push(i);
        }
        let pairs: Vec<(usize, usize)> = {
            let mut c = self.names.lock().unwrap();
            let live: HashSet<String> = ents.iter().map(|(e, _)| key(e)).collect();
            c.vecs.retain(|k, _| live.contains(k));
            c.asked.retain(|k| {
                k.split_once("\u{1e}")
                    .is_some_and(|(a, b)| live.contains(a) && live.contains(b))
            });
            let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for ((scope, kind), idx) in &buckets {
                if idx.len() > NAME_BUCKET_MAX {
                    warn!(
                        "translated names: {} {kind} entities in {scope}, too many to compare",
                        idx.len()
                    );
                    continue;
                }
                for &i in idx {
                    let a = &ents[i].0;
                    let Some(va) = c.vecs.get(&key(a)) else {
                        continue;
                    };
                    let mut near: Vec<(usize, f32)> = idx
                        .iter()
                        .filter(|&&j| {
                            j != i && !ents[j].0.name.trim().eq_ignore_ascii_case(a.name.trim())
                        })
                        .filter_map(|&j| c.vecs.get(&key(&ents[j].0)).map(|vb| (j, dot(va, vb))))
                        .filter(|(_, sim)| *sim >= NAME_SIM)
                        .collect();
                    near.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
                    for (j, _) in near.into_iter().take(NAME_PROPOSALS) {
                        let p = (i.min(j), i.max(j));
                        let k = format!("{}\u{1e}{}", key(&ents[p.0].0), key(&ents[p.1].0));
                        if seen.insert(p) && !c.asked.contains(&k) {
                            out.push(p);
                        }
                    }
                }
            }
            out.sort_unstable();
            out.truncate(NAME_ASK_MAX);
            out
        };
        let mut groups: Vec<(Entity, Vec<Entity>)> = Vec::new();
        let mut taken: HashSet<String> = HashSet::new();
        for (i, j) in pairs {
            let ((a, wa), (b, wb)) = (&ents[i], &ents[j]);
            let p = match judge::same_name(&self.judge, &a.kind, &a.name, &b.name).await {
                Ok(Some(p)) => p,
                Ok(None) => return 0,
                Err(e) => {
                    // Not marked as asked: it is retried next round.
                    warn!("translated names: {e}");
                    continue;
                }
            };
            self.names
                .lock()
                .unwrap()
                .asked
                .insert(format!("{}\u{1e}{}", key(a), key(b)));
            if p < judge::REL_MIN || taken.contains(&a.id) || taken.contains(&b.id) {
                continue;
            }
            let (keep, drop) = if (wb, &a.id) > (wa, &b.id) {
                (b, a)
            } else {
                (a, b)
            };
            info!(
                "entities: {:?} and {:?} are the same (p={p:.2})",
                drop.name, keep.name
            );
            taken.insert(a.id.clone());
            taken.insert(b.id.clone());
            groups.push((keep.clone(), vec![drop.clone()]));
        }
        self.merge_groups(groups).await
    }

    pub async fn merge_entities_loop(&self) {
        loop {
            self.merge_duplicate_entities().await;
            self.merge_translated_entities().await;
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    }

    pub async fn forget_connections(&self) {
        self.conns.lock().await.at = None;
    }

    /// The `sub` as an alias lets distillation link them as a person instead of inventing a name.
    pub async fn ensure_person(&self, subject: &str, email: &str, name: &str) {
        let name: String = name.chars().filter(|c| !c.is_control()).take(120).collect();
        if subject.is_empty() || name.trim().is_empty() {
            return;
        }
        let scope = format!("user:{subject}");
        if !crate::model::valid_scope(&scope) {
            return;
        }
        let exists = self
            .globe
            .read()
            .unwrap()
            .resolve_name(subject, std::slice::from_ref(&scope), Some("person"))
            .is_some();
        if exists {
            return;
        }
        let e = Entity {
            id: format!("person:{subject}@{scope}"),
            scope: scope.clone(),
            kind: "person".into(),
            name: name.trim().to_string(),
            aliases: [subject, email]
                .iter()
                .filter(|a| !a.is_empty())
                .map(|a| a.chars().filter(|c| !c.is_control()).take(120).collect())
                .collect(),
            source: "directory".into(),
            ..Default::default()
        };
        match self.db.upsert_entities(&[e], now_ms()).await {
            Ok(_) => {
                let _ = self.refresh().await;
            }
            Err(err) => warn!("person card: {err}"),
        }
    }

    pub async fn note_identity(&self, subject: &str, email: &str, name: &str) {
        if subject.is_empty() {
            return;
        }
        let key = format!("{subject}|{email}|{name}");
        {
            let mut seen = self.seen_identities.lock().unwrap();
            if seen
                .get(&key)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(3600))
            {
                return;
            }
            if seen.len() >= 10_000 {
                seen.retain(|_, at| at.elapsed() < Duration::from_secs(3600));
            }
            seen.insert(key, Instant::now());
        }
        let row = crate::storage::IdentityRow {
            subject: subject.to_string(),
            email: email.to_string(),
            name: name.to_string(),
            last_ms: now_ms(),
        };
        self.ensure_person(subject, email, name).await;
        if let Err(e) = self.db.touch_identity(&row).await {
            warn!("identity: {e}");
        }
    }

    fn cached(&self, h: &str) -> Option<Cached> {
        let keys = self.keys.lock().unwrap();
        keys.get(h)
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(60))
            .map(|(p, _)| p.clone())
    }

    fn remember_principal(&self, h: String, p: Cached) {
        let mut keys = self.keys.lock().unwrap();
        if keys.len() >= 10_000 {
            keys.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(60));
        }
        keys.insert(h, (p, Instant::now()));
    }

    /// Also records the attempt.
    pub fn allow_login(&self, client: &str) -> bool {
        let window = Duration::from_secs(60);
        let mut all = self.logins.lock().unwrap();
        all.retain(|_, q| {
            while q.front().is_some_and(|t| t.elapsed() > window) {
                q.pop_front();
            }
            !q.is_empty()
        });
        let total: usize = all.values().map(|q| q.len()).sum();
        if total >= LOGINS_PER_MINUTE {
            return false;
        }
        let q = all.entry(client.to_string()).or_default();
        if q.len() >= LOGINS_PER_CLIENT_MINUTE {
            return false;
        }
        q.push_back(Instant::now());
        true
    }

    /// Revocations take effect here at once; other replicas wait out their one-minute cache.
    pub fn forget_principals(&self) {
        self.keys.lock().unwrap().clear();
        if let Some(o) = &self.oidc {
            o.forget_tokens();
        }
    }

    fn touch(&self, id: &str) {
        let mut t = self.touched.lock().unwrap();
        if t.get(id)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(300))
        {
            return;
        }
        t.insert(id.to_string(), Instant::now());
        let (db, id) = (self.db.clone(), id.to_string());
        tokio::spawn(async move {
            if let Err(e) = db.touch_key(&id, now_ms()).await {
                warn!("key usage: {e}");
            }
        });
    }

    /// The key text is returned only here: the database keeps only its hash.
    pub async fn create_key(
        &self,
        name: &str,
        kind: Kind,
        subject: &str,
        expires_ms: i64,
        created_by: &str,
    ) -> Res<(KeyRow, String)> {
        let key = auth::new_key();
        let row = KeyRow {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            hash: auth::hash(&key),
            name: name.to_string(),
            kind: kind.as_str().into(),
            subject: subject.to_string(),
            // Teams are unused: the column is always empty.
            created_ms: now_ms(),
            revoked: false,
            expires_ms,
            created_by: created_by.to_string(),
        };
        self.db.create_key(&row).await?;
        Ok((row, key))
    }

    pub async fn revoke_key(&self, id: &str) -> Res<bool> {
        let ok = self.db.revoke_key(id).await?;
        self.forget_principals();
        Ok(ok)
    }

    /// On a replicated ClickHouse row 11 can show up before row 10: reading `rev > cursor` would
    /// skip row 10 forever, so it rereads from the cursor of one window ago. The globe compares
    /// `seq`, so a late old version does not overwrite a newer one.
    pub async fn refresh(&self) -> Res<usize> {
        let mut trail = self.refresh_lock.lock().await;
        // Taken before reading: whatever is written meanwhile is picked up by the next refresh.
        let head = self.own_log_head().await;
        let window = self.db.lag_window_ms();
        let floor = if window > 0 {
            Some(trail.floor(&*self.db, now_ms(), window).await?)
        } else {
            None
        };
        let mut n = 0;
        loop {
            let (er, mr) = {
                let g = self.globe.read().unwrap();
                match floor {
                    Some((ef, mf)) => (g.ent_rev.min(ef), g.mem_rev.min(mf)),
                    None => (g.ent_rev, g.mem_rev),
                }
            };
            let skip = trail.ranges();
            let ents = self.db.entities_since(er, &skip, 5000).await?;
            let mems = self.db.memories_since(mr, &skip, 2000).await?;
            if ents.is_empty() && mems.is_empty() {
                break;
            }
            n += ents.len() + mems.len();
            let mut g = self.globe.write().unwrap();
            for (e, d, r) in ents {
                if let Some((ef, mf)) = floor
                    && r > ef.min(mf)
                {
                    trail.seen.insert(r);
                }
                g.apply_entity(e, d, r);
            }
            for (m, d, r) in mems {
                if let Some((ef, mf)) = floor
                    && r > ef.min(mf)
                {
                    trail.seen.insert(r);
                }
                g.apply_memory(m, d, r);
            }
            g.finish_batch();
        }
        if floor.is_some() {
            let g = self.globe.read().unwrap();
            trail.marks.push_back((now_ms(), g.ent_rev, g.mem_rev));
        }
        *self.refreshed_head.lock().unwrap() = head;
        self.expire_states();
        Ok(n)
    }

    /// States end on their own, with no new log row to pick up.
    fn expire_states(&self) {
        let mut g = self.globe.write().unwrap();
        if g.expire(now_ms()) > 0 {
            g.finish_batch();
        }
    }

    /// Every write goes through the writer, so it skips ClickHouse when its log has not moved
    /// (`None`); a replica cannot know that, so it asks.
    pub async fn refresh_if_changed(&self) -> Res<Option<usize>> {
        if let Some(head) = self.own_log_head().await
            && *self.refreshed_head.lock().unwrap() == Some(head)
        {
            self.expire_states();
            return Ok(None);
        }
        self.refresh().await.map(Some)
    }

    /// Only on the writer: a replica's head does not move with what another process writes.
    async fn own_log_head(&self) -> Option<u64> {
        if self.writer.is_some() {
            return None;
        }
        self.db.log_head().await
    }

    pub async fn embed(
        &self,
        kind: &'static str,
        texts: &[String],
        timeout: Duration,
    ) -> Option<Vec<Vec<f32>>> {
        match self.embed_checked(kind, texts, timeout).await {
            Ok(v) => v,
            Err(e) => {
                warn!("embeddings: {e}");
                None
            }
        }
    }

    /// `Ok(None)` = no embedder. In the background a memory stored without a vector cannot be found
    /// later, so the pass waits on an error instead.
    pub async fn embed_checked(
        &self,
        kind: &'static str,
        texts: &[String],
        timeout: Duration,
    ) -> Res<Option<Vec<Vec<f32>>>> {
        let prefix = embed_prefix(kind == "embed_query");
        // A search needs no more: a query of tens of KB would tie up the model for nothing.
        let cap = if kind == "embed_query" {
            2_000
        } else {
            usize::MAX
        };
        let texts: Vec<String> = texts
            .iter()
            .map(|t| format!("{prefix}{}", t.chars().take(cap).collect::<String>()))
            .collect();
        meter::tagged(meter::retag(kind), self.embed_untagged(&texts, timeout)).await
    }

    async fn embed_untagged(
        &self,
        texts: &[String],
        timeout: Duration,
    ) -> Res<Option<Vec<Vec<f32>>>> {
        let Some(gw) = self.embedder.as_ref() else {
            return Ok(None);
        };
        if self.cfg.embed_model.is_empty() || texts.is_empty() {
            return Ok(None);
        }
        Ok(Some(gw.embed(texts, timeout).await?))
    }

    pub async fn backends_ready(&self) -> Res<()> {
        let probe = Duration::from_secs(5);
        let mut checks: Vec<(&str, &'static str, &Llm)> = Vec::new();
        if let Some(l) = &self.llm {
            checks.push(("the extraction model", "model", l));
        }
        if let Some(j) = self.judge.backend() {
            checks.push(("the judge", "judge", j));
        }
        for (name, code, llm) in checks {
            if let Err(e) = llm.ready(probe).await {
                *self.distill_wait.lock().unwrap() = Some(code);
                return Err(format!("{name} is not responding ({e})"));
            }
        }
        if let Some(e) = &self.embedder
            && !self.cfg.embed_model.is_empty()
            && let Err(err) = e.ready(probe).await
        {
            *self.distill_wait.lock().unwrap() = Some("embedder");
            return Err(format!("the embedder is not responding ({err})"));
        }
        *self.distill_wait.lock().unwrap() = None;
        Ok(())
    }

    /// Semantic (the `bool`) when the embedder answers in time, otherwise the fast path alone.
    pub async fn recall(&self, opts: RecallArgs) -> (RecallOut, bool) {
        let now = now_ms();
        let run = |qv: Option<&[f32]>| {
            let g = self.globe.read().unwrap();
            g.recall(&RecallOpts {
                query: &opts.query,
                scopes: &opts.scopes,
                now,
                as_of: opts.as_of,
                limit: opts.limit,
                w: opts.w,
                entities: &opts.entities,
                query_vec: qv,
                since: opts.since,
                until: opts.until,
            })
        };
        // By meaning whenever possible: the fast path can fill the page through entities and words
        // while the memory that answers is linked to something else.
        let want_sem = match opts.semantic.as_str() {
            "on" => true,
            "off" => false,
            _ => self.embedder.is_some() && !self.cfg.embed_model.is_empty(),
        };
        if want_sem {
            match self
                .embed_checked(
                    "embed_query",
                    std::slice::from_ref(&opts.query),
                    Duration::from_millis(opts.semantic_deadline_ms),
                )
                .await
            {
                Ok(Some(v)) if !v.is_empty() => return (run(Some(&v[0])), true),
                // A missed deadline returns the fast answer: a search neither waits nor warns.
                Ok(_) => {}
                Err(e) => log::debug!("search without a vector: {e}"),
            }
        }
        (run(None), false)
    }

    /// Always from the same scope: one person's memories are never merged with another's.
    fn neighbors_of(&self, cand: &Memory) -> Vec<Memory> {
        let g = self.globe.read().unwrap();
        let ids: Vec<String> = cand.links.iter().map(|l| l.entity.clone()).collect();
        // The same topic in other words shares no entities or text: only the vector finds it.
        let mut idx: Vec<u32> = cand
            .embedding
            .as_deref()
            .map(|v| g.similar_vec(v, &cand.scope, JUDGE_VECTOR_NEIGHBORS))
            .unwrap_or_default()
            .into_iter()
            .map(|x| x.0)
            .collect();
        // "bug X was fixed" may share words but no entities with the "X is vulnerable" it closes.
        for i in g
            .similar_text(&cand.what, &cand.scope, 3)
            .into_iter()
            .chain(g.neighbors(&ids, &cand.scope, 5))
        {
            if !idx.contains(&i) && idx.len() < 8 {
                idx.push(i);
            }
        }
        idx.into_iter()
            .take(8)
            .map(|i| g.memory_at(i).clone())
            .collect()
    }

    fn link_mentions(&self, m: &mut Memory, reader_scopes: &[String]) {
        let g = self.globe.read().unwrap();
        let mut found = g.link_entities(&m.what, reader_scopes);
        for t in [&m.why, &m.how].into_iter().flatten() {
            for e in g.link_entities(t, reader_scopes) {
                if !found.contains(&e) {
                    found.push(e);
                }
            }
        }
        for e in found {
            let id = g.entity_at(e).id.clone();
            if !m.links.iter().any(|l| l.entity == id) {
                m.links.push(Link {
                    entity: id,
                    role: Role::About,
                    subrole: "mentioned".into(),
                });
            }
        }
    }

    /// Long sources are cut to the fragments most similar to the candidate: in a long report the
    /// why is often far from the start.
    async fn judge_sources(
        &self,
        cands: &[Memory],
        episodes: &HashMap<String, String>,
    ) -> Vec<String> {
        let texts: Vec<Vec<&str>> = cands
            .iter()
            .map(|c| {
                c.source_episodes
                    .iter()
                    .filter_map(|e| episodes.get(e).map(String::as_str))
                    .collect()
            })
            .collect();
        let mut out: Vec<String> = texts.iter().map(|t| t.join("\n")).collect();
        let long: Vec<usize> = (0..cands.len())
            .filter(|&i| out[i].chars().count() > judge::SOURCE_CHARS)
            .collect();
        if long.is_empty() {
            return out;
        }
        let mut segs: Vec<String> = Vec::new();
        let mut of_episode: HashMap<&str, std::ops::Range<usize>> = HashMap::new();
        for &i in &long {
            for e in &cands[i].source_episodes {
                if let (Some(t), false) = (episodes.get(e), of_episode.contains_key(e.as_str())) {
                    let start = segs.len();
                    segs.extend(judge::segments(t));
                    of_episode.insert(e.as_str(), start..segs.len());
                }
            }
        }
        let vecs = match self
            .embed_checked("embed_source", &segs, Duration::from_secs(60))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("judge source: {e}; choosing by words");
                None
            }
        };
        for i in long {
            let idx: Vec<usize> = cands[i]
                .source_episodes
                .iter()
                .filter_map(|e| of_episode.get(e.as_str()).cloned())
                .flatten()
                .collect();
            let scores: Vec<f32> = idx
                .iter()
                .map(|&k| match (&vecs, &cands[i].embedding) {
                    (Some(v), Some(c)) if v.get(k).is_some_and(|x| x.len() == c.len()) => {
                        v[k].iter().zip(c).map(|(a, b)| a * b).sum()
                    }
                    _ => judge::lexical_score(&cands[i], &segs[k]),
                })
                .collect();
            let local: Vec<String> = idx.iter().map(|&k| segs[k].clone()).collect();
            out[i] = judge::pick(&local, &scores, judge::SOURCE_CHARS);
        }
        out
    }

    /// `explicit`: someone is waiting, so a down judge falls back to the heuristic, not an error.
    async fn judge_into_plan(
        &self,
        cands: Vec<Memory>,
        sources: &[String],
        explicit: bool,
        plan: &mut WritePlan,
    ) -> Res<Vec<(String, Outcome, Verdict)>> {
        let mut jobs = Vec::new();
        for (i, c) in cands.iter().enumerate() {
            let neighbors = self.neighbors_of(c);
            let source = sources.get(i).map(String::as_str).unwrap_or_default();
            jobs.push(async move {
                let v = meter::tagged(meter::retag("judge"), async {
                    if explicit {
                        Ok(judge::judge(&self.judge, c, &neighbors, source, true).await)
                    } else {
                        judge::judge_strict(&self.judge, c, &neighbors, source).await
                    }
                })
                .await;
                (neighbors, v)
            });
        }
        let mut verdicts = Vec::with_capacity(cands.len());
        for (neighbors, v) in futures::future::join_all(jobs).await {
            verdicts.push((neighbors, v?));
        }
        let mut results = Vec::new();
        for (mut c, (neighbors, v)) in cands.into_iter().zip(verdicts) {
            let out = judge::outcome(&v, explicit);
            c.judge = if explicit && v.judge == "heuristic" {
                "explicit".into()
            } else {
                v.judge.into()
            };
            c.confidence = if v.judge == "decisions" {
                v.worth.min(1.0)
            } else if explicit {
                0.9
            } else {
                0.6
            };
            if v.why_ok < judge::WHY_MIN {
                c.why = None;
            }
            if v.durable < judge::DURABLE_MIN
                && c.kind != "state"
                && c.kind != "event"
                && c.kind != "decision"
            {
                c.kind = "state".into();
            }
            if c.kind == "state" && c.valid_to.is_none() {
                c.valid_to = Some(c.valid_from + 7 * DAY_MS);
            }
            match &out {
                Outcome::Insert => plan.inserts.push(c.clone()),
                Outcome::Supersede(ids) => {
                    for id in ids {
                        let start = neighbors
                            .iter()
                            .find(|n| &n.id == id)
                            .map(|n| n.valid_from)
                            .unwrap_or(0);
                        plan.closes
                            .push((id.clone(), c.valid_from.max(start), Some(c.id.clone())));
                    }
                    plan.inserts.push(c.clone());
                }
                Outcome::Duplicate(id) => plan
                    .add_sources
                    .push((id.clone(), c.source_episodes.clone())),
                Outcome::Drop => {}
            }
            results.push((c.id.clone(), out, v));
        }
        Ok(results)
    }

    /// `Stale` if someone wrote about the same entities meanwhile: the caller judges again.
    async fn apply(&self, plan: &WritePlan) -> Result<i64, ApplyError> {
        let r = self.db.apply(plan, &self.cfg.embed_model, now_ms()).await;
        if r.is_ok() {
            if let Err(e) = self.refresh().await {
                warn!("refresh after write: {e}");
            }
        }
        r
    }

    pub async fn remember(&self, req: RememberReq) -> Res<RememberOut> {
        if !crate::model::valid_scope(&req.scope) || req.what.trim().chars().count() < 4 {
            return Err("invalid scope or what".into());
        }
        let now = now_ms();
        let mut readers = req.reader_scopes.clone();
        if !readers.contains(&req.scope) {
            readers.push(req.scope.clone());
        }
        let mut links = Vec::new();
        let mut new_ents: Vec<Entity> = Vec::new();
        {
            let g = self.globe.read().unwrap();
            for (list, role) in [
                (&req.who, Role::Who),
                (&req.where_, Role::Where),
                (&req.about, Role::About),
            ] {
                for name in list {
                    let id = g
                        .entity(name)
                        .filter(|e| readers.contains(&e.scope))
                        .map(|e| e.id.clone())
                        .or_else(|| {
                            g.resolve_name(name, &readers, None)
                                .map(|i| g.entity_at(i).id.clone())
                        });
                    // An unknown name becomes an entity here, or its link would be lost.
                    let id = match id {
                        Some(id) => id,
                        None => {
                            let name = name.trim();
                            if name.chars().count() < 2 || name.chars().count() > 80 {
                                continue;
                            }
                            let id = crate::distill::entity_id("concept", name, &req.scope);
                            if !new_ents.iter().any(|e: &Entity| e.id == id) {
                                new_ents.push(Entity {
                                    id: id.clone(),
                                    scope: req.scope.clone(),
                                    kind: "concept".into(),
                                    name: name.to_string(),
                                    source: "explicit".into(),
                                    ..Default::default()
                                });
                            }
                            id
                        }
                    };
                    if !links
                        .iter()
                        .any(|l: &Link| l.entity == id && l.role == role)
                    {
                        links.push(Link {
                            entity: id,
                            role,
                            subrole: String::new(),
                        });
                    }
                }
            }
        }
        let kind = req
            .kind
            .clone()
            .filter(|k| crate::model::KINDS.contains(&k.as_str()))
            .unwrap_or_else(|| "fact".into());
        let mut m = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            scope: req.scope.clone(),
            owner_id: req.owner_id.clone(),
            kind,
            what: req.what.trim().to_string(),
            why: req.why.clone().filter(|s| !s.trim().is_empty()),
            how: req.how.clone().filter(|s| !s.trim().is_empty()),
            occurred_from: req.occurred_at,
            occurred_to: None,
            known_at: now,
            valid_from: req.occurred_at.unwrap_or(now).min(now),
            valid_to: None,
            superseded_by: None,
            confidence: 0.9,
            source_episodes: vec![
                req.episode_id
                    .clone()
                    .unwrap_or_else(|| format!("explicit:{}", uuid::Uuid::new_v4())),
            ],
            links,
            judge: String::new(),
            embedding: None,
        };
        self.link_mentions(&mut m, &readers);
        if let Some(v) = self
            .embed("embed", &[m.index_text()], Duration::from_secs(10))
            .await
        {
            m.embedding = v.into_iter().next();
        }
        for attempt in 0..3 {
            let seen = self.globe.read().unwrap().mem_rev;
            let mut plan = WritePlan::default();
            let results = self
                .judge_into_plan(vec![m.clone()], &[], true, &mut plan)
                .await?;
            // New entities go in only if the memory does: a duplicate leaves no orphan nodes.
            if !plan.inserts.is_empty() {
                plan.new_entities = new_ents.clone();
            }
            plan.stale_check = Some((
                m.scope.clone(),
                m.links.iter().map(|l| l.entity.clone()).collect(),
                seen,
            ));
            match self.apply(&plan).await {
                Ok(_) => {
                    let (id, out, v) = &results[0];
                    let (outcome, closed, id) = match out {
                        Outcome::Insert => ("inserted", Vec::new(), id.clone()),
                        Outcome::Supersede(c) => ("superseded", c.clone(), id.clone()),
                        Outcome::Duplicate(d) => ("duplicate", Vec::new(), d.clone()),
                        Outcome::Drop => ("dropped", Vec::new(), id.clone()),
                    };
                    return Ok(RememberOut {
                        id,
                        outcome: outcome.into(),
                        closed,
                        judge: v.judge.into(),
                    });
                }
                Err(ApplyError::Stale) if attempt < 2 => {
                    let _ = self.refresh().await;
                }
                Err(ApplyError::Stale) => {
                    return Err("write conflict: try again".into());
                }
                Err(ApplyError::Db(e)) => return Err(e),
            }
        }
        Err("write conflict".into())
    }

    pub async fn distill_thread(&self, th: &Thread) -> Res<usize> {
        let raw = self
            .raw
            .as_ref()
            .ok_or("no raw store: nothing to distill")?;
        let gw = self
            .llm
            .as_ref()
            .ok_or("cannot extract without MNEMO_LLM_URL")?;
        if self.cfg.extract_model.is_empty() {
            return Err("MNEMO_EXTRACT_MODEL is not set".into());
        }
        let (thread_id, scope) = thread_parts(th);
        let new = raw
            .thread_after(&thread_id, &scope, th.upto_ms, &th.upto_id, 60)
            .await?;
        if new.is_empty() {
            let plan = WritePlan {
                thread_advance: Some((th.thread.clone(), th.upto_ms, th.upto_id.clone(), i32::MAX)),
                ..Default::default()
            };
            self.apply(&plan).await.map_err(|e| format!("{e:?}"))?;
            return Ok(0);
        }
        let before = raw
            .thread_before(&thread_id, &scope, th.upto_ms, &th.upto_id, 6)
            .await?;
        let mut readers = th.reader_scopes.clone();
        if !readers.contains(&scope) {
            readers.push(scope.clone());
        }

        let (known, names) = {
            let g = self.globe.read().unwrap();
            let mut idx: Vec<u32> = Vec::new();
            let text: String = new
                .iter()
                .chain(before.iter())
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            for e in new
                .iter()
                .flat_map(|e| e.entities.iter())
                .chain(new.iter().map(|e| &e.actor_id))
            {
                if let Some(i) = entity_or_person(&g, e, &scope, &readers) {
                    idx.push(i);
                }
            }
            idx.extend(g.link_entities(&text, &readers));
            let mut all = Vec::new();
            for i in idx {
                if !all.contains(&i) {
                    all.push(i);
                }
                for a in g.ancestors(i) {
                    if !all.contains(&a) {
                        all.push(a);
                    }
                }
            }
            all.truncate(150);
            let readers_set: HashSet<&str> = readers.iter().map(|s| s.as_str()).collect();
            let known: HashMap<String, Entity> = all
                .into_iter()
                .map(|i| g.entity_at(i).clone())
                .filter(|e| readers_set.contains(e.scope.as_str()))
                .map(|e| (e.id.clone(), e))
                .collect();
            // Without the speaker's person entity the model sees only the `sub` and picks a name
            // from wherever it can, such as the account name in home directory paths.
            let names: HashMap<String, String> = new
                .iter()
                .chain(before.iter())
                .filter_map(|e| {
                    entity_or_person(&g, &e.actor_id, &scope, &readers)
                        .map(|i| g.entity_at(i))
                        .filter(|p| p.kind == "person" && readers_set.contains(p.scope.as_str()))
                        .map(|p| (e.actor_id.clone(), p.name.clone()))
                })
                .collect();
            (known, names)
        };
        let mut known_list: Vec<&Entity> = known.values().collect();
        known_list.sort_by(|a, b| a.id.cmp(&b.id));
        // ≈3 chars per token (conservative). What does not fit waits for the next pass.
        let ctx = if self.cfg.llm_context == 0 {
            16_384
        } else {
            self.cfg.llm_context
        } as usize;
        // Under a schema a Qwen model writes ~2 JSON tokens per episode token at ~12 tokens/s, so
        // passes are small; a cut-off output is retried with half the episodes.
        let max_out = 3_000.min(ctx / 4) as u32;
        let reserve = max_out as usize + distill::SYSTEM_PROMPT.len() / 3 + 1024;
        let pass = match self.cfg.extract_budget_tokens {
            0 => 4_000,
            n => n as usize,
        }
        .min(ctx.saturating_sub(reserve))
        .max(1_000);
        let budget_chars = pass.saturating_mul(3);
        let timeout = Duration::from_secs(match self.cfg.llm_timeout_s {
            0 => 600,
            s => s,
        });

        // Passes run concurrently (on an integrated GPU 16 sequences give 3.5x the output of one)
        // but settle in order: the cursor only advances over what settled with no gap.
        let parallel = match self.cfg.distill_parallel {
            0 => 4,
            n => n as usize,
        };
        let mut chunks: Vec<(Vec<Episode>, Vec<Episode>)> = Vec::new();
        let mut rest = new;
        let mut prev = before;
        while !rest.is_empty() && chunks.len() < parallel {
            let (_, n) = distill::prompt(
                &known_list,
                &prev,
                &rest,
                &names,
                budget_chars,
                distill::EPISODE_CHARS,
            );
            let n = n.clamp(1, rest.len());
            let chunk: Vec<Episode> = rest.drain(..n).collect();
            let tail = chunk[chunk.len().saturating_sub(6)..].to_vec();
            chunks.push((std::mem::replace(&mut prev, tail), chunk));
        }
        let calls = chunks.iter().map(|(b, c)| {
            self.extract_pass(
                gw,
                &known_list,
                b,
                c,
                &names,
                budget_chars,
                max_out,
                timeout,
            )
        });
        let outs = futures::future::join_all(calls).await;

        let mut inserted = 0;
        for (i, ((_, chunk), out)) in chunks.iter().zip(outs).enumerate() {
            match out {
                Ok(Pass::Done { covered, text }) => {
                    let whole = covered.len() == chunk.len();
                    inserted += self
                        .settle_pass(th, &scope, &readers, &known, &covered, &text)
                        .await?;
                    if !whole {
                        // Only partly fit: settling the following chunks now would leave a gap
                        // below the cursor.
                        break;
                    }
                }
                // Cut off even with little text: skip it rather than leave the thread stuck.
                Ok(Pass::Skip(ep)) => {
                    warn!(
                        "distill {:?}: skipping episode {:?}, the output does not fit even with 500 characters",
                        th.thread, ep.episode_id
                    );
                    let plan = WritePlan {
                        thread_advance: Some((
                            th.thread.clone(),
                            ep.occurred_at,
                            ep.episode_id.clone(),
                            1,
                        )),
                        ..Default::default()
                    };
                    self.apply(&plan).await.map_err(|e| format!("{e:?}"))?;
                    break;
                }
                Err(e) if i == 0 => return Err(e),
                Err(e) => {
                    // Earlier chunks are already settled; this one is retried with the thread.
                    warn!("distill {:?}: pass {i}: {e}", th.thread);
                    break;
                }
            }
        }
        Ok(inserted)
    }

    #[allow(clippy::too_many_arguments)]
    async fn extract_pass(
        &self,
        gw: &Llm,
        known_list: &[&Entity],
        before: &[Episode],
        chunk: &[Episode],
        names: &HashMap<String, String>,
        budget_chars: usize,
        max_out: u32,
        timeout: Duration,
    ) -> Res<Pass> {
        let mut new = chunk.to_vec();
        let mut budget_chars = budget_chars;
        // Whole episodes: a lower cap would leave the end of long pieces unread (subagent reports
        // put their findings last) while the cursor moved past it.
        let mut max_chars = distill::EPISODE_CHARS;
        loop {
            let (user, included) =
                distill::prompt(known_list, before, &new, names, budget_chars, max_chars);
            new.truncate(included.max(1));
            match gw
                .chat(
                    &self.cfg.extract_model,
                    distill::SYSTEM_PROMPT,
                    &user,
                    max_out,
                    timeout,
                    Some(("memories", distill::output_schema())),
                )
                .await
            {
                Ok(text) => return Ok(Pass::Done { covered: new, text }),
                Err(e) if e == crate::llm::TRUNCATED && new.len() > 1 => {
                    new.truncate(new.len() / 2);
                    budget_chars = usize::MAX;
                }
                Err(e) if e == crate::llm::TRUNCATED && max_chars > 500 => {
                    warn!(
                        "extract {:?}: the output does not fit; reading only its first {} characters",
                        new[0].episode_id,
                        max_chars / 2
                    );
                    max_chars /= 2;
                    budget_chars = usize::MAX;
                }
                Err(e) if e == crate::llm::TRUNCATED => {
                    return Ok(Pass::Skip(Box::new(new.swap_remove(0))));
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn settle_pass(
        &self,
        th: &Thread,
        scope: &str,
        readers: &[String],
        known: &HashMap<String, Entity>,
        new: &[Episode],
        text: &str,
    ) -> Res<usize> {
        let started = Instant::now();
        let last = new.last().ok_or("pass without episodes")?;
        let advance = (
            th.thread.clone(),
            last.occurred_at,
            last.episode_id.clone(),
            new.len() as i32,
        );
        let out = distill::parse_output(text)?;
        let now = now_ms();
        let (ents, cands) = {
            let g = self.globe.read().unwrap();
            let resolve = |name: &str, kind: &str| {
                g.resolve_name(name, readers, Some(kind))
                    .or_else(|| g.resolve_name(name, readers, None))
                    .map(|i| g.entity_at(i).id.clone())
            };
            distill::build(
                out,
                &Ctx {
                    scope,
                    owner: &th.owner_id,
                    now,
                    episodes: new,
                    known,
                },
                &resolve,
            )
        };
        let mut cands = distill::dedupe_within(cands);
        for c in &mut cands {
            self.link_mentions(c, readers);
            c.scope = scope.to_string();
        }
        if let Some(vs) = self
            .embed_checked(
                "embed",
                &cands.iter().map(|c| c.index_text()).collect::<Vec<_>>(),
                Duration::from_secs(30),
            )
            .await?
        {
            for (c, v) in cands.iter_mut().zip(vs) {
                c.embedding = Some(v);
            }
        }
        let episodes: HashMap<String, String> = new
            .iter()
            .map(|e| (e.episode_id.clone(), e.text.clone()))
            .collect();
        let sources = self.judge_sources(&cands, &episodes).await;

        for attempt in 0..3 {
            let seen = self.globe.read().unwrap().mem_rev;
            let mut plan = WritePlan {
                thread_advance: Some(advance.clone()),
                ..Default::default()
            };
            let results = self
                .judge_into_plan(cands.clone(), &sources, false, &mut plan)
                .await?;
            let used: HashSet<String> = plan
                .inserts
                .iter()
                .flat_map(|m| m.links.iter().map(|l| l.entity.clone()))
                .collect();
            plan.new_entities = ents
                .iter()
                .filter(|e| used.contains(&e.id))
                .cloned()
                .collect();
            plan.stale_check = Some((
                scope.to_string(),
                cands
                    .iter()
                    .flat_map(|c| c.links.iter().map(|l| l.entity.clone()))
                    .collect(),
                seen,
            ));
            match self.apply(&plan).await {
                Ok(_) => {
                    let inserted = plan.inserts.len();
                    info!(
                        "{}",
                        serde_json::json!({"kind": "distill", "thread": th.thread, "episodes": new.len(), "candidates": results.len(),
                            "inserted": inserted, "duplicates": plan.add_sources.len(), "closed": plan.closes.len(),
                            "new_entities": plan.new_entities.len(), "ms": started.elapsed().as_millis() as u64})
                    );
                    let touched: Vec<String> = plan
                        .inserts
                        .iter()
                        .flat_map(|m| m.links.iter().map(|l| l.entity.clone()))
                        .collect();
                    self.regen_summaries(&touched).await;
                    return Ok(inserted);
                }
                Err(ApplyError::Stale) if attempt < 2 => {
                    let _ = self.refresh().await;
                }
                Err(e) => return Err(format!("{e:?}")),
            }
        }
        Err("persistent conflict while distilling".into())
    }

    pub async fn regen_summaries(&self, ids: &[String]) {
        let summaries: Vec<(String, String)> = {
            let g = self.globe.read().unwrap();
            let mut seen = HashSet::new();
            ids.iter()
                .filter(|id| seen.insert(id.to_string()))
                .filter_map(|id| {
                    let e = g.entity_idx(id)?;
                    let scope = g.entity_at(e).scope.clone();
                    let mut parts = Vec::new();
                    for i in g.memories_of(e, &[scope]) {
                        let m = g.memory_at(i);
                        // The card is injected automatically: no third-party text in it.
                        if m.is_live() && !m.is_saved_copy() && parts.len() < 5 {
                            parts.push(m.what.clone());
                        }
                    }
                    let s: String = parts.join(" ").chars().take(700).collect();
                    (!s.is_empty()).then(|| (id.clone(), s))
                })
                .collect()
        };
        if let Err(e) = self.db.set_summaries(&summaries).await {
            warn!("cards: {e}");
        }
    }

    pub async fn refresh_things(&self) {
        let Some(raw) = &self.raw else { return };
        match raw.subjects().await {
            Ok(subs) => {
                *self.things.write().unwrap() = subs
                    .into_iter()
                    .filter(|s| !s.thing.is_empty())
                    .map(|s| (s.thing.clone(), s))
                    .collect();
            }
            Err(e) => warn!("state of things: {e}"),
        }
    }

    pub async fn things_loop(&self) {
        loop {
            self.refresh_things().await;
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }

    /// On a model change ALL vectors are recomputed first: two deployments of the "same" model gave
    /// cosine -0.02 for the same text, and a foreign vector silently hides its memory from search.
    pub async fn backfill_embeddings(&self) {
        const CURSOR: &str = "embed_model";
        let fingerprint = model_fingerprint(&self.cfg.embed_model);
        let mut redo: Option<Vec<(String, String)>> = None;
        loop {
            if self.embedder.is_some() && !self.cfg.embed_model.is_empty() {
                if redo.is_none() && self.db.cursor(CURSOR).await.is_ok_and(|c| c != fingerprint) {
                    let all: Vec<(String, String)> = self
                        .globe
                        .read()
                        .unwrap()
                        .memories()
                        .map(|m| (m.id.clone(), m.index_text()))
                        .collect();
                    info!(
                        "vectors: the embedding model changed; recomputing {} memories",
                        all.len()
                    );
                    redo = Some(all);
                }
                if let Some(todo) = redo.as_mut() {
                    if todo.is_empty() {
                        if self.db.set_cursor(CURSOR, fingerprint).await.is_ok() {
                            info!("vectors: recomputed with {}", self.cfg.embed_model);
                            redo = None;
                        }
                        continue;
                    }
                    let n = todo.len().min(64);
                    let batch: Vec<(String, String)> = todo[..n].to_vec();
                    let texts: Vec<String> = batch.iter().map(|b| b.1.clone()).collect();
                    match self.embed("embed", &texts, Duration::from_secs(60)).await {
                        Some(vs) if vs.len() == batch.len() => {
                            let items: Vec<(String, Vec<f32>)> =
                                batch.into_iter().map(|b| b.0).zip(vs).collect();
                            match self.db.set_embeddings(&items, true).await {
                                Ok(_) => {
                                    todo.drain(..n);
                                }
                                Err(e) => warn!("vectors: {e}"),
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        _ => tokio::time::sleep(Duration::from_secs(30)).await,
                    }
                    continue;
                }
            }
            let batch = self.globe.read().unwrap().missing_vectors(64);
            if batch.is_empty() || self.embedder.is_none() || self.cfg.embed_model.is_empty() {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
            let texts: Vec<String> = batch.iter().map(|b| b.1.clone()).collect();
            match self.embed("embed", &texts, Duration::from_secs(60)).await {
                Some(vs) => {
                    let items: Vec<(String, Vec<f32>)> =
                        batch.into_iter().map(|b| b.0).zip(vs).collect();
                    match self.db.set_embeddings(&items, false).await {
                        Ok(n) => info!("vectors: {n} memories filled in"),
                        Err(e) => warn!("vectors: {e}"),
                    }
                    // Let the refresh bring them into the globe, or the next batch repeats them.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                None => tokio::time::sleep(Duration::from_secs(30)).await,
            }
        }
    }

    pub async fn distill_loop(&self) {
        let mut paused: Option<Duration> = None;
        loop {
            match self.backends_ready().await {
                Ok(()) => {
                    if paused.take().is_some() {
                        info!("distillation resumed: every backend responds");
                    }
                }
                Err(e) => {
                    let wait = match paused {
                        None => {
                            warn!("distillation paused: {e}");
                            Duration::from_secs(30)
                        }
                        Some(w) => (w * 2).min(Duration::from_secs(300)),
                    };
                    paused = Some(wait);
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
            let now = now_ms();
            match self
                .db
                .claim_threads(now, self.cfg.settle_ms, 4, 15 * 60_000)
                .await
            {
                Ok(threads) if !threads.is_empty() => {
                    let jobs = threads.iter().map(|t| async move {
                        let tag = meter::Tag {
                            kind: "extract",
                            subject: t.owner_id.clone(),
                            thread: t.thread.clone(),
                        };
                        match meter::tagged(tag, self.distill_thread(t)).await {
                            Ok(_) => true,
                            Err(e) => {
                                warn!("distill {:?}: {e}", t.thread);
                                // Keep the lease (15 min): releasing it would reclaim the thread at
                                // once and turn a permanent error (a prompt that never fits) into
                                // thousands of calls per minute.
                                false
                            }
                        }
                    });
                    let results = futures::future::join_all(jobs).await;
                    if results.iter().all(|ok| *ok) {
                        continue;
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("claiming threads: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    pub async fn ingest(&self, items: Vec<IngestItem>) -> Res<usize> {
        let raw = self
            .raw
            .as_ref()
            .ok_or("no raw store to keep episodes in")?;
        let mut eps = Vec::new();
        let mut threads: HashMap<String, (Thread, i64, i32)> = HashMap::new();
        for it in items {
            let ep = it.ep;
            if !crate::model::valid_scope(&ep.scope)
                || ep.episode_id.is_empty()
                || ep.thread.is_empty()
                || ep.text.trim().is_empty()
            {
                continue;
            }
            let key = format!("{}|{}", ep.thread, ep.scope);
            let parts = crate::episode::chunk(&ep);
            let entry = threads.entry(key.clone()).or_insert_with(|| {
                (
                    Thread {
                        thread: key,
                        scope: ep.scope.clone(),
                        owner_id: ep.owner_id.clone(),
                        reader_scopes: it.reader_scopes.clone(),
                        upto_ms: 0,
                        upto_id: String::new(),
                    },
                    0,
                    0,
                )
            });
            entry.1 = entry.1.max(ep.occurred_at);
            entry.2 += parts.len() as i32;
            if !it.reader_scopes.is_empty() {
                entry.0.reader_scopes = it.reader_scopes;
            }
            eps.extend(parts);
        }
        raw.insert(&eps, now_ms()).await?;
        for (t, last, n) in threads.values() {
            self.db.touch_thread(t, *last, *n).await?;
        }
        Ok(eps.len())
    }

    pub async fn purge_owner(&self, owner: &str) -> Res<serde_json::Value> {
        // Deleting someone also removes their access: their keys stop working.
        let mut revoked = 0;
        for k in self.db.list_keys().await? {
            if k.subject == owner && !k.revoked && self.db.revoke_key(&k.id).await? {
                revoked += 1;
            }
        }
        self.forget_principals();
        let (m, e) = self.db.purge_owner(owner).await?;
        self.forget_connections().await;
        if let Some(raw) = &self.raw {
            raw.purge_owner(owner).await?;
        }
        self.refresh().await?;
        Ok(
            serde_json::json!({"memories": m, "entities": e, "episodes": "deleted", "keys_revoked": revoked}),
        )
    }
}

/// Stable across processes (FNV-1a): it is persisted to detect a model change.
fn model_fingerprint(model: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in model.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h >> 1) as i64
}

fn entity_or_person(g: &Globe, e: &str, scope: &str, readers: &[String]) -> Option<u32> {
    if e.is_empty() {
        return None;
    }
    let id = if e.contains(':') {
        e.to_string()
    } else {
        format!("person:{e}")
    };
    g.entity_idx(&id)
        .or_else(|| g.entity_idx(&format!("{id}@{scope}")))
        // By alias only in the person's own scope: otherwise anyone could name someone else by
        // creating a person with the other's `sub` as an alias in their own scope.
        .or_else(|| {
            let own = format!("user:{e}");
            (!e.contains(':') && readers.contains(&own))
                .then(|| g.resolve_name(e, std::slice::from_ref(&own), Some("person")))
                .flatten()
        })
}

/// `Fake(None)` does not respond.
pub enum Embedder {
    Local(Arc<crate::onnx::Local>),
    #[cfg(test)]
    Fake(Option<Vec<f32>>),
}

impl Embedder {
    pub async fn embed(&self, texts: &[String], timeout: Duration) -> Res<Vec<Vec<f32>>> {
        match self {
            Embedder::Local(o) => {
                match tokio::time::timeout(timeout, o.embed(texts, timeout)).await {
                    Ok(r) => r,
                    Err(_) => Err(format!("embeddings: more than {} s", timeout.as_secs())),
                }
            }
            #[cfg(test)]
            Embedder::Fake(v) => v
                .as_ref()
                .map(|v| vec![v.clone(); texts.len()])
                .ok_or_else(|| "embeddings: not responding".to_string()),
        }
    }

    pub async fn ready(&self, _timeout: Duration) -> Res<()> {
        match self {
            Embedder::Local(o) => o.ready(),
            #[cfg(test)]
            Embedder::Fake(v) => v
                .as_ref()
                .map(|_| ())
                .ok_or_else(|| "embeddings: not responding".to_string()),
        }
    }
}

enum Pass {
    /// Possibly fewer episodes than the chunk held.
    Done { covered: Vec<Episode>, text: String },
    /// Does not fit even when trimmed: the cursor must move past it.
    Skip(Box<Episode>),
}

#[derive(serde::Deserialize, Debug)]
pub struct IngestItem {
    #[serde(flatten)]
    pub ep: Episode,
    /// They bound the entities offered to the extractor.
    #[serde(default)]
    pub reader_scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RecallArgs {
    pub query: String,
    pub scopes: Vec<String>,
    pub limit: usize,
    pub as_of: Option<i64>,
    pub w: Option<W>,
    pub entities: Vec<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub semantic: String,
    pub semantic_deadline_ms: u64,
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_subject_with_a_pipe_keeps_its_thread() {
        use crate::storage::Thread;
        let th = |thread: &str, scope: &str| Thread {
            thread: thread.into(),
            scope: scope.into(),
            owner_id: String::new(),
            reader_scopes: Vec::new(),
            upto_ms: 0,
            upto_id: String::new(),
        };
        let parts = |t: Thread| super::thread_parts(&t);
        assert_eq!(
            parts(th("s1|user:auth0|123", "user:auth0|123")),
            ("s1".into(), "user:auth0|123".into())
        );
        assert_eq!(
            parts(th("a|b|user:ana", "user:ana")),
            ("a|b".into(), "user:ana".into()),
            "a thread id with a pipe too"
        );
        assert_eq!(
            parts(th("s1|user:ana", "")),
            ("s1".into(), "user:ana".into()),
            "a row without its scope still splits"
        );
    }

    #[tokio::test]
    async fn the_passes_of_one_thread_fly_at_once_and_settle_in_order() {
        use crate::app::{App, Cfg};
        use crate::episode::Episode;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore};
        use axum::{Router, routing::post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (i2, p2, c2) = (inflight.clone(), peak.clone(), calls.clone());
        let fake = Router::new().route(
            "/chat/completions",
            post(move || {
                let (i, p, c) = (i2.clone(), p2.clone(), c2.clone());
                async move {
                    let n = i.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(n, Ordering::SeqCst);
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    i.fetch_sub(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"choices": [{"finish_reason": "stop",
                        "message": {"content": "{\"entities\": [], \"memories\": []}"}}]}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });

        let db = Arc::new(Mem::new());
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            Some(crate::llm::Llm::new(&base, "").unwrap().with_json(true)),
            None,
            crate::judge::Judge::Heuristic,
            Cfg {
                extract_model: "fake".into(),
                // 3000-character passes: one or two 1200-character episodes each.
                extract_budget_tokens: 1_000,
                distill_parallel: 8,
                ..Default::default()
            },
        );
        let items: Vec<super::IngestItem> = (0..8)
            .map(|i| super::IngestItem {
                ep: Episode {
                    episode_id: format!("e{i}"),
                    thread: "t1".into(),
                    source: "chat".into(),
                    scope: "user:ana".into(),
                    owner_id: "ana".into(),
                    actor_id: "ana".into(),
                    role: "user".into(),
                    occurred_at: 1_000 + i,
                    text: format!("episodio {i} ") + &"palabra ".repeat(150),
                    ..Default::default()
                },
                reader_scopes: vec![],
            })
            .collect();
        app.ingest(items).await.unwrap();
        let now = 1_000_000;
        let th = db
            .claim_threads(now, 1_000, 4, 60_000)
            .await
            .unwrap()
            .pop()
            .expect("the thread is settled");
        app.distill_thread(&th).await.unwrap();
        let calls = calls.load(Ordering::SeqCst);
        assert!(calls >= 4, "several passes: {calls}");
        assert_eq!(peak.load(Ordering::SeqCst), calls, "all at once");
        assert!(
            db.claim_threads(now + 1, 1_000, 4, 60_000)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_speaker_is_named_by_their_identity() {
        use crate::app::{App, Cfg};
        use crate::episode::Episode;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore};
        use axum::{Router, routing::post};
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = seen.clone();
        let fake = Router::new().route(
            "/chat/completions",
            post(move |body: String| {
                let s = s2.clone();
                async move {
                    *s.lock().unwrap() = body;
                    axum::Json(serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": "{\"entities\": [], \"memories\": []}"}}]}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });
        let db = Arc::new(Mem::new());
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            Some(crate::llm::Llm::new(&base, "").unwrap().with_json(true)),
            None,
            crate::judge::Judge::Heuristic,
            Cfg {
                extract_model: "fake".into(),
                ..Default::default()
            },
        );
        app.note_identity("ana-sub", "ana@example.com", "Ana Pérez")
            .await;
        app.ingest(vec![super::IngestItem {
            ep: Episode {
                episode_id: "e0".into(),
                thread: "t1".into(),
                source: "chat".into(),
                scope: "user:ana-sub".into(),
                owner_id: "ana-sub".into(),
                actor_id: "ana-sub".into(),
                role: "user".into(),
                occurred_at: 1_000,
                text: "We moved the deploys to Nomad because compose could not roll back.".into(),
                ..Default::default()
            },
            reader_scopes: vec![],
        }])
        .await
        .unwrap();
        let th = db
            .claim_threads(1_000_000, 1_000, 4, 60_000)
            .await
            .unwrap()
            .pop()
            .unwrap();
        app.distill_thread(&th).await.unwrap();
        let body = seen.lock().unwrap().clone();
        assert!(
            body.contains("user Ana Pérez: We moved"),
            "the episode line names the speaker: {body}"
        );
        assert!(!body.contains("user ana-sub:"), "and not by their sub");
        assert!(
            body.contains("person:ana-sub@user:ana-sub"),
            "and their person card is among the known entities"
        );
        app.note_identity("ana-sub", "ana@example.com", "Ana Pérez")
            .await;
        let g = app.globe.read().unwrap();
        let i = g
            .resolve_name("ana-sub", &["user:ana-sub".to_string()], Some("person"))
            .unwrap();
        assert_eq!(g.entity_at(i).id, "person:ana-sub@user:ana-sub");
        assert_eq!(
            g.ents.iter().filter(|n| n.e.kind == "person").count(),
            1,
            "a single person card"
        );
    }

    #[tokio::test]
    async fn the_judge_sees_the_passage_behind_the_candidate_and_never_source_with_neighbors() {
        use crate::app::{App, Cfg};
        use crate::episode::Episode;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore};
        use axum::{Router, routing::post};
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let fake = Router::new()
            .route(
                "/chat/completions",
                post(|| async {
                    let out = serde_json::json!({"entities": [], "memories": [{"kind": "decision",
                        "what": "The gateway timeout was raised to 600 s.",
                        "why": "The load balancer cut long streams at 120 s.",
                        "who": [], "where": [], "about": [], "episodes": ["e0"]}]});
                    axum::Json(serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": out.to_string()}}]}))
                }),
            )
            .route(
                "/decisions",
                post(move |body: axum::Json<serde_json::Value>| {
                    let s = s2.clone();
                    async move {
                        s.lock().unwrap().push(body.0);
                        axum::Json(serde_json::json!({"answers": {
                            "durable": {"noul": 0.9}, "worth": {"noul": 0.9}, "why_ok": {"noul": 0.9}}}))
                    }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });
        let db = Arc::new(Mem::new());
        let llm = crate::llm::Llm::new(&base, "").unwrap().with_json(true);
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            Some(llm.clone()),
            None,
            crate::judge::Judge::Decisions(llm, "fake".into(), std::time::Duration::from_secs(10)),
            Cfg {
                extract_model: "fake".into(),
                ..Default::default()
            },
        );
        let filler = "We went through the logs line by line and nothing stood out. ".repeat(50);
        app.ingest(vec![super::IngestItem {
            ep: Episode {
                episode_id: "e0".into(),
                thread: "t1".into(),
                source: "chat".into(),
                scope: "user:ana".into(),
                owner_id: "ana".into(),
                actor_id: "ana".into(),
                role: "user".into(),
                occurred_at: 1_000,
                text: format!(
                    "{filler}\nWe raised the gateway timeout to 600 s because the load balancer cut long streams at 120 s."
                ),
                ..Default::default()
            },
            reader_scopes: vec![],
        }])
        .await
        .unwrap();
        let th = db
            .claim_threads(1_000_000, 1_000, 4, 60_000)
            .await
            .unwrap()
            .pop()
            .unwrap();
        app.distill_thread(&th).await.unwrap();
        let reqs = seen.lock().unwrap().clone();
        assert!(!reqs.is_empty(), "the judge did not ask");
        for r in &reqs {
            assert!(
                !(r["state"].get("source").is_some() && r["state"].get("existing").is_some()),
                "source and neighbours in the same request: {r}"
            );
        }
        let src = reqs
            .iter()
            .find_map(|r| r["state"]["source"].as_str())
            .expect("the source reaches why_ok");
        assert!(src.chars().count() <= crate::judge::SOURCE_CHARS);
        assert!(src.contains("cut long streams at 120 s"), "{src}");
    }

    #[tokio::test]
    async fn translated_entity_names_are_merged_when_the_judge_says_they_are_the_same() {
        use crate::app::{App, Cfg};
        use crate::model::Entity;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore};
        use axum::{Router, routing::post};
        use std::sync::{Arc, Mutex};
        let asked: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let a2 = asked.clone();
        let fake = Router::new().route(
            "/decisions",
            post(move |b: axum::Json<serde_json::Value>| {
                let a = a2.clone();
                async move {
                    let (x, y) = (
                        b.0["state"]["NAME A"].as_str().unwrap_or("").to_string(),
                        b.0["state"]["NAME B"].as_str().unwrap_or("").to_string(),
                    );
                    let same = [x.as_str(), y.as_str()].contains(&"juez")
                        && [x.as_str(), y.as_str()].contains(&"Judge");
                    a.lock().unwrap().push((x, y));
                    axum::Json(serde_json::json!({"answers": {"same": {"noul": if same { 0.9 } else { 0.1 }}}}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });
        let db = Arc::new(Mem::new());
        let ent = |id: &str, name: &str| Entity {
            id: format!("system:{id}@user:ana"),
            scope: "user:ana".into(),
            kind: "system".into(),
            name: name.into(),
            ..Default::default()
        };
        db.upsert_entities(
            &[
                ent("judge", "Judge"),
                ent("juez", "juez"),
                ent("nomad", "Nomad"),
            ],
            1,
        )
        .await
        .unwrap();
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            None,
            // The same vector for everything: every pair is a candidate and the judge decides.
            Some(super::Embedder::Fake(Some(vec![1.0, 0.0, 0.0]))),
            crate::judge::Judge::Decisions(
                crate::llm::Llm::new(&base, "").unwrap(),
                "fake".into(),
                std::time::Duration::from_secs(10),
            ),
            Cfg {
                embed_model: "fake-embed".into(),
                ..Default::default()
            },
        );
        app.refresh().await.unwrap();
        assert_eq!(app.merge_translated_entities().await, 1);
        {
            let g = app.globe.read().unwrap();
            let juez = g
                .resolve_name("juez", &["user:ana".to_string()], None)
                .unwrap();
            let judge = g
                .resolve_name("Judge", &["user:ana".to_string()], None)
                .unwrap();
            assert_eq!(juez, judge, "'juez' resolves to the same entity as 'Judge'");
            let nomad = g
                .resolve_name("Nomad", &["user:ana".to_string()], None)
                .unwrap();
            assert_ne!(
                nomad, judge,
                "what the judge says is something else is not merged"
            );
        }
        let before = asked.lock().unwrap().len();
        assert!(before >= 1);
        app.merge_translated_entities().await;
        let after = asked.lock().unwrap().len();
        assert!(
            asked.lock().unwrap()[before..]
                .iter()
                .all(|(a, b)| !(a == "Nomad" || b == "Nomad")),
            "a pair already asked is not asked again: {before} → {after}"
        );
    }

    #[tokio::test]
    async fn a_new_embedding_model_recalculates_every_vector() {
        use crate::app::{App, Cfg};
        use crate::globe::tests::mem;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore, WritePlan};
        use std::sync::Arc;
        let db = Arc::new(Mem::new());
        let mut old_live = mem("m1", (2026, 10, 1), "live memory", None, &[]);
        old_live.embedding = Some(vec![0.0, 1.0, 0.0]);
        let mut old_closed = mem("m2", (2026, 10, 1), "closed memory", None, &[]);
        old_closed.embedding = Some(vec![0.0, 1.0, 0.0]);
        old_closed.valid_to = Some(old_closed.valid_from + 1);
        db.apply(
            &WritePlan {
                inserts: vec![old_live, old_closed],
                ..Default::default()
            },
            "old-model",
            1,
        )
        .await
        .unwrap();
        let app = Arc::new(App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            None,
            Some(super::Embedder::Fake(Some(vec![1.0, 0.0, 0.0]))),
            crate::judge::Judge::Heuristic,
            Cfg {
                embed_model: "new-model".into(),
                ..Default::default()
            },
        ));
        app.refresh().await.unwrap();
        let a2 = app.clone();
        let loop_task = tokio::spawn(async move { a2.backfill_embeddings().await });
        let want = super::model_fingerprint("new-model");
        let mut done = false;
        for _ in 0..100 {
            if db.cursor("embed_model").await.unwrap() == want {
                done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        loop_task.abort();
        assert!(done, "the new model's fingerprint is recorded");
        let rows = db.memories_since(0, &[], 100).await.unwrap();
        for id in ["m1", "m2"] {
            let last = rows.iter().rfind(|(m, _, _)| m.id == id).unwrap();
            assert_eq!(
                last.0.embedding.as_deref(),
                Some(&[1.0, 0.0, 0.0][..]),
                "{id} with the new vector"
            );
        }
    }

    #[tokio::test]
    async fn existing_duplicates_are_merged_into_the_busier_one() {
        use crate::app::{App, Cfg};
        use crate::globe::tests::mem;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore, WritePlan};
        use std::sync::Arc;
        let db = Arc::new(Mem::new());
        let e = |id: &str, kind: &str| crate::model::Entity {
            id: id.into(),
            scope: "user:ana".into(),
            kind: kind.into(),
            name: "Nomad".into(),
            ..Default::default()
        };
        db.upsert_entities(
            &[
                e("system:nomad@user:ana", "system"),
                e("repo:nomad@user:ana", "repo"),
            ],
            1,
        )
        .await
        .unwrap();
        let mut m = |id: &str, ent: &str| {
            let mut x = mem(
                id,
                (2026, 10, 1),
                id,
                None,
                &[(ent, crate::model::Role::About)],
            );
            x.scope = "user:ana".into();
            x
        };
        db.apply(
            &WritePlan {
                inserts: vec![
                    m("a", "system:nomad@user:ana"),
                    m("b", "system:nomad@user:ana"),
                    m("c", "repo:nomad@user:ana"),
                ],
                ..Default::default()
            },
            "",
            2,
        )
        .await
        .unwrap();
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg::default(),
        );
        app.refresh().await.unwrap();
        assert_eq!(app.merge_duplicate_entities().await, 1);
        let g = app.globe.read().unwrap();
        assert_eq!(
            g.same_as_of("repo:nomad@user:ana").as_deref(),
            Some("system:nomad@user:ana"),
            "the most linked one is kept"
        );
        let sys = g.entity_idx("system:nomad@user:ana").unwrap();
        assert_eq!(g.memories_of(sys, &["user:ana".to_string()]).len(), 3);
        drop(g);
        let _ = &mut m;
    }

    async fn closed_port() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        base
    }

    #[tokio::test]
    async fn the_background_waits_while_a_backend_is_down() {
        use crate::app::{App, Cfg};
        use crate::episode::Episode;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore};
        use axum::{Router, routing::post};
        use std::sync::Arc;

        let fake = Router::new().route(
            "/chat/completions",
            post(|| async {
                axum::Json(serde_json::json!({"choices": [{"finish_reason": "stop",
                    "message": {"content": "{\"entities\": [], \"memories\": [{\"what\": \"Ana prefers tabs\", \"kind\": \"fact\", \"who\": [], \"when\": \"\", \"where\": \"\", \"why\": \"\", \"how\": \"\", \"sources\": [\"e0\"]}]}"}}]}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });
        let down = closed_port().await;

        let app_with = |llm: &str, embed_up: bool| {
            let db = Arc::new(Mem::new());
            let app = App::new(
                db.clone() as Arc<dyn Db>,
                Some(db.clone() as Arc<dyn RawStore>),
                Some(crate::llm::Llm::new(llm, "").unwrap().with_json(true)),
                Some(super::Embedder::Fake(embed_up.then(|| vec![1.0, 0.0, 0.0]))),
                crate::judge::Judge::Heuristic,
                Cfg {
                    extract_model: "fake".into(),
                    embed_model: "fake-embed".into(),
                    ..Default::default()
                },
            );
            (db, app)
        };

        let (_, app) = app_with(&down, true);
        let e = app.backends_ready().await.unwrap_err();
        assert!(e.contains("extraction model"), "{e}");

        let (db, app) = app_with(&up, false);
        app.ingest(vec![super::IngestItem {
            ep: Episode {
                episode_id: "e0".into(),
                thread: "t1".into(),
                source: "chat".into(),
                scope: "user:ana".into(),
                owner_id: "ana".into(),
                actor_id: "ana".into(),
                role: "user".into(),
                occurred_at: 1_000,
                text: "Ana: I prefer tabs over spaces".into(),
                ..Default::default()
            },
            reader_scopes: vec![],
        }])
        .await
        .unwrap();
        let now = 1_000_000;
        let th = db
            .claim_threads(now, 1_000, 4, 60_000)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            app.distill_thread(&th).await.is_err(),
            "without an embedder the pass does not settle"
        );
        assert_eq!(db.stats().await.unwrap()["memories"], 0);
        assert_eq!(
            db.claim_threads(now + 61_000, 1_000, 4, 60_000)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn the_writer_reads_the_log_only_when_it_moved() {
        use crate::app::{App, Cfg};
        use crate::model::Entity;
        use crate::storage::Db;
        use crate::storage::mem::Mem;
        use std::sync::Arc;

        let db = Arc::new(Mem::new());
        let new_app = || {
            App::new(
                db.clone() as Arc<dyn Db>,
                None,
                None,
                None,
                crate::judge::Judge::Heuristic,
                Cfg::default(),
            )
        };
        let app = new_app();
        assert_eq!(
            app.refresh_if_changed().await.unwrap(),
            Some(0),
            "the first time it reads"
        );
        assert_eq!(
            app.refresh_if_changed().await.unwrap(),
            None,
            "without writes it does not read"
        );
        let gw = Entity {
            id: "system:gw".into(),
            kind: "system".into(),
            name: "gateway".into(),
            ..Default::default()
        };
        db.upsert_entities(&[gw], 1).await.unwrap();
        assert_eq!(
            app.refresh_if_changed().await.unwrap(),
            Some(1),
            "what was written arrives"
        );
        assert_eq!(app.refresh_if_changed().await.unwrap(), None);
        assert!(app.globe.read().unwrap().entity_idx("system:gw").is_some());

        let mut replica = new_app();
        replica.writer = Some("http://writer".into());
        replica.refresh().await.unwrap();
        assert_eq!(
            replica.refresh_if_changed().await.unwrap(),
            Some(0),
            "a replica does not know what another one writes: it asks"
        );
    }

    #[test]
    fn what_was_read_is_sent_as_ranges() {
        let mut t = super::Trail::default();
        t.seen
            .extend((10..=5_009).chain(5_011..=9_000).chain([9_500]));
        assert_eq!(
            t.ranges(),
            vec![(10, 5_009), (5_011, 9_000), (9_500, 9_500)]
        );
    }

    #[tokio::test]
    async fn a_row_that_arrives_late_from_another_replica_is_not_skipped() {
        use crate::app::{App, Cfg};
        use crate::model::Memory;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, WritePlan};
        use std::sync::Arc;

        let db = Arc::new(Mem::replicated(60_000));
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            None,
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg::default(),
        );
        let memory = |id: &str, what: &str| Memory {
            id: id.into(),
            scope: "user:ana".into(),
            what: what.into(),
            known_at: 1,
            valid_from: 1,
            ..Default::default()
        };
        let write = |m: Memory| {
            let db = db.clone();
            async move {
                db.apply(
                    &WritePlan {
                        inserts: vec![m],
                        ..Default::default()
                    },
                    "",
                    1,
                )
                .await
                .unwrap()
            }
        };
        let first = write(memory("m1", "The gateway cut at 120 s.")).await;
        let second = write(memory("m2", "The judge runs on decide-strict.")).await;
        db.hide(&[first]);
        app.refresh().await.unwrap();
        {
            let g = app.globe.read().unwrap();
            assert!(g.memory("m2").is_some() && g.memory("m1").is_none());
            assert_eq!(g.mem_rev, second);
        }
        db.reveal();
        app.refresh().await.unwrap();
        assert!(
            app.globe.read().unwrap().memory("m1").is_some(),
            "the row that arrived late comes in on the next read"
        );

        // An old version that arrives after the new one does not overwrite it.
        let old = write(memory("m3", "Qwen runs on 2 replicas.")).await;
        let mut closed = memory("m3", "Qwen runs on 2 replicas.");
        closed.valid_to = Some(5);
        write(closed).await;
        db.hide(&[old]);
        app.refresh().await.unwrap();
        db.reveal();
        app.refresh().await.unwrap();
        assert_eq!(
            app.globe.read().unwrap().memory("m3").unwrap().valid_to,
            Some(5)
        );
    }

    #[tokio::test]
    async fn the_judge_sees_the_closest_memories_by_vector_below_any_cosine_threshold() {
        use crate::app::{App, Cfg};
        use crate::model::Memory;
        use crate::storage::Db;
        use crate::storage::mem::Mem;
        use std::sync::Arc;

        let app = App::new(
            Arc::new(Mem::new()) as Arc<dyn Db>,
            None,
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg::default(),
        );
        let memory = |id: &str, what: &str, v: [f32; 4]| Memory {
            id: id.into(),
            scope: "user:ana".into(),
            what: what.into(),
            known_at: 1,
            valid_from: 1,
            embedding: Some(v.to_vec()),
            ..Default::default()
        };
        // Cosine with the candidate: 0.86 · 0.80 · 0.75 · 0.30 · 0.20. None shares words or
        // entities with it, so only the vector can bring them in.
        {
            let mut g = app.globe.write().unwrap();
            for (rev, m) in [
                memory("paraphrase", "Builds stall", [0.86, 0.51, 0.0, 0.0]),
                memory("second", "Deploys freeze", [0.80, 0.0, 0.60, 0.0]),
                memory("third", "Runners wedge", [0.75, 0.0, 0.0, 0.66]),
                memory("fourth", "Coffee broke", [0.30, 0.0, 0.0, -0.95]),
                memory("fifth", "Lot repaved", [0.20, -0.98, 0.0, 0.0]),
            ]
            .into_iter()
            .enumerate()
            {
                g.upsert_memory(m, false, rev as i64 + 1);
            }
        }
        let cand = memory("cand", "Pipeline hangs nightly", [1.0, 0.0, 0.0, 0.0]);
        let mut got: Vec<String> = app.neighbors_of(&cand).into_iter().map(|m| m.id).collect();
        got.sort();
        assert_eq!(got, ["paraphrase", "second", "third"]);
    }

    /// A state carries its planned end in `valid_to`; until then the judge must still see it.
    #[tokio::test]
    async fn the_judge_compares_a_candidate_with_a_state_that_has_not_ended() {
        use crate::app::{App, Cfg, now_ms};
        use crate::model::Memory;
        use crate::storage::Db;
        use crate::storage::mem::Mem;
        use std::sync::Arc;

        let app = App::new(
            Arc::new(Mem::new()) as Arc<dyn Db>,
            None,
            None,
            None,
            crate::judge::Judge::Heuristic,
            Cfg::default(),
        );
        let now = now_ms();
        let state = Memory {
            id: "state".into(),
            scope: "user:ana".into(),
            kind: "state".into(),
            what: "The deploy pipeline is frozen".into(),
            known_at: now - 1_000,
            valid_from: now - 1_000,
            valid_to: Some(now + 7 * 86_400_000),
            embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            ..Default::default()
        };
        app.globe.write().unwrap().upsert_memory(state, false, 1);
        let cand = Memory {
            id: "cand".into(),
            scope: "user:ana".into(),
            what: "The deploy pipeline is open again".into(),
            known_at: now,
            valid_from: now,
            embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            ..Default::default()
        };
        let got: Vec<String> = app.neighbors_of(&cand).into_iter().map(|m| m.id).collect();
        assert_eq!(got, ["state"]);
    }
}
