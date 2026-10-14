//! In-memory store with the ClickHouse contract, for unit tests. ClickHouse specifics (SQL,
//! `FINAL`, the writer lease) are only covered by `conformance` against a real database.

use super::{
    ApplyError, ConnectionRow, Db, IdentityRow, KeyRow, LoginRow, MeterRows, RawStore, Res, Seen,
    SessionRow, Thread, WritePlan, seen,
};
use crate::episode::Episode;
use crate::meter::{Eval, Injection, LlmCall, Turn, Window};
use crate::model::{Entity, Memory};
use async_trait::async_trait;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

#[derive(Default)]
struct State {
    seq: i64,
    entities: Vec<(i64, Entity, bool)>,
    memories: Vec<(i64, Memory, bool)>,
    records: Vec<crate::records::Record>,
    key_usage: HashMap<String, i64>,
    sessions: BTreeMap<String, SessionRow>,
    identities: BTreeMap<String, IdentityRow>,
    connections: BTreeMap<(String, String), ConnectionRow>,
    blocks: BTreeMap<(String, String), i64>,
    logins: HashMap<String, (LoginRow, bool)>,
    meter: MeterRows,
    keys: BTreeMap<String, KeyRow>,
    cursors: HashMap<String, i64>,
    threads: HashMap<String, (Thread, i64)>,
    progress: HashMap<String, (i64, String)>,
    claims: HashMap<String, i64>,
    episodes: BTreeMap<String, Episode>,
    /// Mimics replication lag: log `seq`s not yet visible on the answering replica.
    hidden: std::collections::HashSet<i64>,
    lag_window_ms: i64,
}

impl State {
    fn memory(&self, id: &str) -> Option<Memory> {
        self.memories
            .iter()
            .rev()
            .find(|(_, m, _)| m.id == id)
            .filter(|(_, _, d)| !d)
            .map(|(_, m, _)| m.clone())
    }

    fn entity(&self, id: &str) -> Option<Entity> {
        self.entities
            .iter()
            .rev()
            .find(|(_, e, _)| e.id == id)
            .filter(|(_, _, d)| !d)
            .map(|(_, e, _)| e.clone())
    }

    fn push_memory(&mut self, m: Memory, deleted: bool) {
        self.seq += 1;
        self.memories.push((self.seq, m, deleted));
    }

    fn push_entity(&mut self, mut e: Entity) {
        if e.summary.is_empty() {
            if let Some(prev) = self.entity(&e.id) {
                e.summary = prev.summary;
            }
        }
        self.seq += 1;
        self.entities.push((self.seq, e, false));
    }

    fn thread_eps(&self, thread: &str, scope: &str) -> Vec<Episode> {
        let mut v: Vec<Episode> = self
            .episodes
            .values()
            .filter(|e| e.thread == thread && e.scope == scope)
            .cloned()
            .collect();
        v.sort_by(|a, b| (a.occurred_at, &a.episode_id).cmp(&(b.occurred_at, &b.episode_id)));
        v
    }
}

#[derive(Default)]
pub struct Mem(Mutex<State>);

impl Mem {
    pub fn new() -> Mem {
        Mem::default()
    }

    pub fn replicated(lag_window_ms: i64) -> Mem {
        let m = Mem::default();
        m.0.lock().unwrap().lag_window_ms = lag_window_ms;
        m
    }

    pub fn hide(&self, seqs: &[i64]) {
        self.0.lock().unwrap().hidden.extend(seqs);
    }

    pub fn reveal(&self) {
        self.0.lock().unwrap().hidden.clear();
    }
}

#[async_trait]
impl Db for Mem {
    async fn migrate(&self) -> Res<()> {
        Ok(())
    }

    async fn upsert_entities(&self, ents: &[Entity], _now: i64) -> Res<i64> {
        let mut s = self.0.lock().unwrap();
        for e in ents {
            s.push_entity(e.clone());
        }
        Ok(s.seq)
    }

    async fn set_embeddings(&self, items: &[(String, Vec<f32>)], replace: bool) -> Res<usize> {
        let mut s = self.0.lock().unwrap();
        let mut n = 0;
        for (id, v) in items {
            if let Some(mut m) = s
                .memory(id)
                .filter(|m| replace || (m.is_live() && m.embedding.is_none()))
            {
                m.embedding = Some(v.clone());
                s.push_memory(m, false);
                n += 1;
            }
        }
        Ok(n)
    }

    async fn set_summaries(&self, summaries: &[(String, String)]) -> Res<()> {
        let mut s = self.0.lock().unwrap();
        for (id, summary) in summaries {
            if let Some(mut e) = s.entity(id).filter(|e| &e.summary != summary) {
                e.summary = summary.clone();
                s.push_entity(e);
            }
        }
        Ok(())
    }

    async fn entities_since(
        &self,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(Entity, bool, i64)>> {
        let s = self.0.lock().unwrap();
        Ok(s.entities
            .iter()
            .filter(|r| r.0 > rev && !seen(skip, r.0) && !s.hidden.contains(&r.0))
            .take(limit as usize)
            .map(|(q, e, d)| (e.clone(), *d, *q))
            .collect())
    }

    async fn log_head(&self) -> Option<u64> {
        let s = self.0.lock().unwrap();
        (s.lag_window_ms == 0).then_some(s.seq as u64)
    }

    fn lag_window_ms(&self) -> i64 {
        self.0.lock().unwrap().lag_window_ms
    }

    async fn memories_since(
        &self,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(Memory, bool, i64)>> {
        let s = self.0.lock().unwrap();
        Ok(s.memories
            .iter()
            .filter(|r| r.0 > rev && !seen(skip, r.0) && !s.hidden.contains(&r.0))
            .take(limit as usize)
            .map(|(q, m, d)| (m.clone(), *d, *q))
            .collect())
    }

    async fn apply(
        &self,
        plan: &WritePlan,
        _embedding_model: &str,
        _now: i64,
    ) -> Result<i64, ApplyError> {
        let mut s = self.0.lock().unwrap();
        if let Some((scope, ents, seen)) = &plan.stale_check {
            let stale = s.memories.iter().any(|(q, m, d)| {
                *q > *seen
                    && !d
                    && m.is_live()
                    && &m.scope == scope
                    && m.links.iter().any(|l| ents.contains(&l.entity))
            });
            if stale {
                return Err(ApplyError::Stale);
            }
        }
        for e in &plan.new_entities {
            s.push_entity(e.clone());
        }
        for (id, valid_to, by) in &plan.closes {
            if let Some(mut m) = s.memory(id).filter(|m| m.is_live()) {
                m.valid_to = Some(*valid_to);
                if by.is_some() {
                    m.superseded_by = by.clone();
                }
                s.push_memory(m, false);
            }
        }
        for (id, eps) in &plan.add_sources {
            if let Some(mut m) = s.memory(id) {
                for ep in eps {
                    if !m.source_episodes.contains(ep) {
                        m.source_episodes.push(ep.clone());
                    }
                }
                s.push_memory(m, false);
            }
        }
        for m in &plan.inserts {
            s.push_memory(m.clone(), false);
        }
        if let Some((thread, upto_ms, upto_id, _)) = &plan.thread_advance {
            s.progress
                .insert(thread.clone(), (*upto_ms, upto_id.clone()));
            s.claims.remove(thread);
        }
        Ok(s.seq)
    }

    async fn forget(&self, id: &str, scopes: &[String], _reason: &str, now: i64) -> Res<bool> {
        let mut s = self.0.lock().unwrap();
        let Some(mut m) = s
            .memory(id)
            .filter(|m| m.is_live() && scopes.contains(&m.scope))
        else {
            return Ok(false);
        };
        m.valid_to = Some(now);
        s.push_memory(m, false);
        Ok(true)
    }

    async fn purge_owner(&self, owner: &str) -> Res<(u64, u64)> {
        let mut s = self.0.lock().unwrap();
        let (scope, person) = (format!("user:{owner}"), format!("person:{owner}"));
        let mut mems: Vec<String> = s
            .memories
            .iter()
            .filter(|(_, m, _)| m.owner_id == owner || m.scope == scope)
            .map(|(_, m, _)| m.id.clone())
            .collect();
        let mut ents: Vec<String> = s
            .entities
            .iter()
            .filter(|(_, e, _)| e.scope == scope || e.id == person)
            .map(|(_, e, _)| e.id.clone())
            .collect();
        mems.sort();
        mems.dedup();
        ents.sort();
        ents.dedup();
        s.memories.retain(|(_, m, _)| !mems.contains(&m.id));
        s.meter.injections.retain(|x| x.subject != owner);
        s.meter.turns.retain(|x| x.subject != owner);
        s.meter.evals.retain(|x| x.subject != owner);
        s.meter.llm.retain(|x| x.subject != owner);
        s.sessions.retain(|_, x| x.subject != owner);
        s.identities.remove(owner);
        s.connections.retain(|(sub, _), _| sub != owner);
        s.blocks.retain(|(sub, _), _| sub != owner);
        s.entities.retain(|(_, e, _)| !ents.contains(&e.id));
        for id in &mems {
            s.push_memory(
                Memory {
                    id: id.clone(),
                    ..Default::default()
                },
                true,
            );
        }
        for id in &ents {
            s.seq += 1;
            let seq = s.seq;
            s.entities.push((
                seq,
                Entity {
                    id: id.clone(),
                    ..Default::default()
                },
                true,
            ));
        }
        s.threads
            .retain(|_, (t, _)| t.owner_id != owner && t.scope != scope);
        Ok((mems.len() as u64, ents.len() as u64))
    }

    async fn touch_thread(&self, t: &Thread, last_ms: i64, _n: i32) -> Res<()> {
        let mut s = self.0.lock().unwrap();
        let e = s
            .threads
            .entry(t.thread.clone())
            .or_insert((t.clone(), last_ms));
        e.1 = e.1.max(last_ms);
        Ok(())
    }

    async fn claim_threads(
        &self,
        now: i64,
        settle_ms: i64,
        limit: i64,
        lease_ms: i64,
    ) -> Res<Vec<Thread>> {
        let mut s = self.0.lock().unwrap();
        s.claims.retain(|_, until| *until > now);
        let mut ready: Vec<(i64, Thread)> = s
            .threads
            .values()
            .filter_map(|(t, last)| {
                let (upto_ms, upto_id) = s.progress.get(&t.thread).cloned().unwrap_or_default();
                (*last > upto_ms && *last < now - settle_ms && !s.claims.contains_key(&t.thread))
                    .then(|| {
                        (
                            *last,
                            Thread {
                                upto_ms,
                                upto_id,
                                ..t.clone()
                            },
                        )
                    })
            })
            .collect();
        ready.sort_by_key(|r| r.0);
        let out: Vec<Thread> = ready
            .into_iter()
            .take(limit as usize)
            .map(|r| r.1)
            .collect();
        for t in &out {
            s.claims.insert(t.thread.clone(), now + lease_ms);
        }
        Ok(out)
    }

    async fn cursor(&self, name: &str) -> Res<i64> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .cursors
            .get(name)
            .copied()
            .unwrap_or(0))
    }

    async fn set_cursor(&self, name: &str, value: i64) -> Res<()> {
        let mut s = self.0.lock().unwrap();
        let c = s.cursors.entry(name.into()).or_insert(value);
        *c = (*c).max(value);
        Ok(())
    }

    async fn stats(&self) -> Res<serde_json::Value> {
        let s = self.0.lock().unwrap();
        let mut ids: Vec<&str> = s.memories.iter().map(|(_, m, _)| m.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        let current: Vec<Memory> = ids.iter().filter_map(|id| s.memory(id)).collect();
        let pending = s
            .threads
            .values()
            .filter(|(t, last)| *last > s.progress.get(&t.thread).map(|p| p.0).unwrap_or(0))
            .count();
        Ok(json!({
            "memories": current.len(),
            "live": current.iter().filter(|m| m.is_live()).count(),
            "entities": s.entities.iter().map(|(_, e, _)| e.id.as_str()).collect::<std::collections::HashSet<_>>().len(),
            "pending_threads": pending,
            "episodes": s.episodes.len(),
        }))
    }

    async fn create_key(&self, k: &KeyRow) -> Res<()> {
        self.0.lock().unwrap().keys.insert(k.id.clone(), k.clone());
        Ok(())
    }

    async fn key_by_hash(&self, hash: &str) -> Res<Option<KeyRow>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .keys
            .values()
            .find(|k| k.hash == hash)
            .cloned())
    }

    async fn list_keys(&self) -> Res<Vec<KeyRow>> {
        Ok(self.0.lock().unwrap().keys.values().cloned().collect())
    }

    async fn revoke_key(&self, id: &str) -> Res<bool> {
        let mut s = self.0.lock().unwrap();
        match s.keys.get_mut(id).filter(|k| !k.revoked) {
            Some(k) => {
                k.revoked = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn touch_key(&self, id: &str, at: i64) -> Res<()> {
        let mut s = self.0.lock().unwrap();
        let e = s.key_usage.entry(id.into()).or_insert(at);
        *e = (*e).max(at);
        Ok(())
    }

    async fn keys_last_used(&self) -> Res<HashMap<String, i64>> {
        Ok(self.0.lock().unwrap().key_usage.clone())
    }

    async fn touch_identity(&self, i: &IdentityRow) -> Res<()> {
        self.0
            .lock()
            .unwrap()
            .identities
            .insert(i.subject.clone(), i.clone());
        Ok(())
    }

    async fn identity(&self, subject: &str) -> Res<Option<IdentityRow>> {
        Ok(self.0.lock().unwrap().identities.get(subject).cloned())
    }

    async fn put_connection(&self, c: &ConnectionRow) -> Res<()> {
        let row = ConnectionRow {
            blocked_ms: 0,
            ..c.clone()
        };
        self.0
            .lock()
            .unwrap()
            .connections
            .insert((c.subject.clone(), c.client.clone()), row);
        Ok(())
    }

    async fn put_block(&self, subject: &str, client: &str, blocked_ms: i64) -> Res<()> {
        self.0
            .lock()
            .unwrap()
            .blocks
            .insert((subject.to_string(), client.to_string()), blocked_ms);
        Ok(())
    }

    async fn blocks(&self) -> Res<Vec<(String, String, i64)>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .blocks
            .iter()
            .filter(|(_, b)| **b > 0)
            .map(|((s, c), b)| (s.clone(), c.clone(), *b))
            .collect())
    }

    async fn connections(&self) -> Res<Vec<ConnectionRow>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .connections
            .values()
            .cloned()
            .collect())
    }

    async fn identities(&self) -> Res<Vec<IdentityRow>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .identities
            .values()
            .cloned()
            .collect())
    }

    async fn create_session(&self, x: &SessionRow) -> Res<()> {
        self.0
            .lock()
            .unwrap()
            .sessions
            .insert(x.id.clone(), x.clone());
        Ok(())
    }

    async fn session_by_hash(&self, hash: &str) -> Res<Option<SessionRow>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .sessions
            .values()
            .find(|x| x.hash == hash)
            .cloned())
    }

    async fn list_sessions(&self, now: i64) -> Res<Vec<SessionRow>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|x| !x.revoked && x.expires_ms > now)
            .cloned()
            .collect())
    }

    async fn revoke_session(&self, id: &str) -> Res<bool> {
        let mut s = self.0.lock().unwrap();
        match s.sessions.get_mut(id).filter(|x| !x.revoked) {
            Some(x) => {
                x.revoked = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn put_login(&self, l: &LoginRow) -> Res<()> {
        self.0
            .lock()
            .unwrap()
            .logins
            .insert(l.state_hash.clone(), (l.clone(), false));
        Ok(())
    }

    async fn take_login(&self, state_hash: &str, now: i64) -> Res<Option<LoginRow>> {
        let mut s = self.0.lock().unwrap();
        match s
            .logins
            .get_mut(state_hash)
            .filter(|(l, used)| !used && l.expires_ms > now)
        {
            Some((l, used)) => {
                *used = true;
                Ok(Some(l.clone()))
            }
            None => Ok(None),
        }
    }

    async fn record_llm(&self, calls: &[LlmCall]) -> Res<()> {
        self.0.lock().unwrap().meter.llm.extend_from_slice(calls);
        Ok(())
    }

    async fn record_injection(&self, i: &Injection) -> Res<()> {
        self.0.lock().unwrap().meter.injections.push(i.clone());
        Ok(())
    }

    async fn record_turn(&self, t: &Turn) -> Res<()> {
        self.0.lock().unwrap().meter.turns.push(t.clone());
        Ok(())
    }

    async fn record_evals(&self, evals: &[Eval]) -> Res<()> {
        self.0.lock().unwrap().meter.evals.extend_from_slice(evals);
        Ok(())
    }

    async fn session_window(&self, session: &str) -> Res<Window> {
        let s = self.0.lock().unwrap();
        let mut rows: Vec<Injection> = s
            .meter
            .injections
            .iter()
            .filter(|i| i.session == session)
            .cloned()
            .collect();
        rows.sort_by_key(|i| i.at);
        Ok(crate::meter::window_of(&rows))
    }

    async fn last_turn_at(&self, session: &str) -> Res<i64> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .meter
            .turns
            .iter()
            .filter(|t| t.session == session)
            .map(|t| t.at)
            .max()
            .unwrap_or(0))
    }

    async fn injections_after(&self, session: &str, after_ms: i64) -> Res<Vec<Injection>> {
        let s = self.0.lock().unwrap();
        let mut v: Vec<Injection> = s
            .meter
            .injections
            .iter()
            .filter(|i| i.session == session && i.at > after_ms)
            .cloned()
            .collect();
        v.sort_by_key(|i| i.at);
        Ok(v)
    }

    async fn meter_rows(&self, from_ms: i64, subject: Option<&str>) -> Res<MeterRows> {
        let s = self.0.lock().unwrap();
        let ok = |at: i64, who: &str| at >= from_ms && subject.is_none_or(|u| u == who);
        Ok(MeterRows {
            llm: s
                .meter
                .llm
                .iter()
                .filter(|x| ok(x.at, &x.subject))
                .cloned()
                .collect(),
            injections: s
                .meter
                .injections
                .iter()
                .filter(|x| ok(x.at, &x.subject))
                .cloned()
                .collect(),
            turns: s
                .meter
                .turns
                .iter()
                .filter(|x| ok(x.at, &x.subject))
                .cloned()
                .collect(),
            evals: s
                .meter
                .evals
                .iter()
                .filter(|x| ok(x.at, &x.subject))
                .cloned()
                .collect(),
        })
    }

    async fn evals_since(&self, from_ms: i64) -> Res<Vec<Eval>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .meter
            .evals
            .iter()
            .filter(|e| e.at >= from_ms)
            .cloned()
            .collect())
    }

    async fn writer_status(&self) -> Res<Option<(String, i64)>> {
        Ok(None)
    }
}

#[async_trait]
impl RawStore for Mem {
    async fn put_records(&self, rs: &[crate::records::Record]) -> Res<usize> {
        let mut s = self.0.lock().unwrap();
        let mut n = 0;
        for r in rs {
            if s.records.iter().any(|x| x.id == r.id) {
                continue;
            }
            if r.kind == "snapshot" {
                let last = s
                    .records
                    .iter()
                    .filter(|x| x.kind == "snapshot" && x.scope == r.scope && x.key == r.key)
                    .max_by_key(|x| x.at);
                if last.is_some_and(|x| x.hash == r.hash) {
                    continue;
                }
            }
            s.records.push(r.clone());
            n += 1;
        }
        Ok(n)
    }

    async fn records(&self, q: &crate::records::Query) -> Res<Vec<crate::records::Record>> {
        let s = self.0.lock().unwrap();
        let mut out: Vec<crate::records::Record> = s
            .records
            .iter()
            .filter(|r| q.scopes.contains(&r.scope) && r.key == q.key)
            .filter(|r| q.kind.as_ref().is_none_or(|k| &r.kind == k))
            .filter(|r| q.from.is_none_or(|f| r.at >= f) && q.until.is_none_or(|u| r.at <= u))
            .cloned()
            .collect();
        out.sort_by(|a, b| b.at.cmp(&a.at).then(b.id.cmp(&a.id)));
        out.truncate(q.limit.max(1) as usize);
        Ok(out)
    }

    async fn subjects(&self) -> Res<Vec<crate::records::Subject>> {
        Ok(crate::records::subjects_of(&self.0.lock().unwrap().records))
    }

    async fn insert(&self, eps: &[Episode], _revision: i64) -> Res<()> {
        let mut s = self.0.lock().unwrap();
        for e in eps {
            s.episodes.insert(e.episode_id.clone(), e.clone());
        }
        Ok(())
    }

    async fn thread_after(
        &self,
        thread: &str,
        scope: &str,
        upto_ms: i64,
        upto_id: &str,
        limit: u32,
    ) -> Res<Vec<Episode>> {
        let s = self.0.lock().unwrap();
        Ok(s.thread_eps(thread, scope)
            .into_iter()
            .filter(|e| (e.occurred_at, e.episode_id.as_str()) > (upto_ms, upto_id))
            .take(limit as usize)
            .collect())
    }

    async fn thread_before(
        &self,
        thread: &str,
        scope: &str,
        upto_ms: i64,
        upto_id: &str,
        limit: u32,
    ) -> Res<Vec<Episode>> {
        let s = self.0.lock().unwrap();
        let v: Vec<Episode> = s
            .thread_eps(thread, scope)
            .into_iter()
            .filter(|e| (e.occurred_at, e.episode_id.as_str()) <= (upto_ms, upto_id))
            .collect();
        Ok(v[v.len().saturating_sub(limit as usize)..].to_vec())
    }

    async fn thread_episode_ids(
        &self,
        thread: &str,
        scopes: &[String],
        since_ms: i64,
    ) -> Res<Vec<String>> {
        let s = self.0.lock().unwrap();
        Ok(s.episodes
            .values()
            .filter(|e| {
                e.thread == thread && scopes.contains(&e.scope) && e.occurred_at >= since_ms
            })
            .map(|e| e.episode_id.clone())
            .collect())
    }

    async fn episode_with_context(
        &self,
        id: &str,
        scopes: &[String],
        around: u32,
    ) -> Res<Vec<Episode>> {
        let Some(ep) = self
            .0
            .lock()
            .unwrap()
            .episodes
            .get(id)
            .filter(|e| scopes.contains(&e.scope))
            .cloned()
        else {
            return Ok(Vec::new());
        };
        let mut v = self
            .thread_before(
                &ep.thread,
                &ep.scope,
                ep.occurred_at,
                &ep.episode_id,
                around + 1,
            )
            .await?;
        v.extend(
            self.thread_after(
                &ep.thread,
                &ep.scope,
                ep.occurred_at,
                &ep.episode_id,
                around,
            )
            .await?,
        );
        Ok(v)
    }

    async fn search(
        &self,
        terms: &[String],
        entities: &[String],
        scopes: &[String],
        from: Option<i64>,
        until: Option<i64>,
        limit: u32,
    ) -> Res<Vec<Episode>> {
        if terms.is_empty() && entities.is_empty() {
            return Ok(Vec::new());
        }
        let s = self.0.lock().unwrap();
        let mut hits: Vec<(usize, Episode)> = s
            .episodes
            .values()
            .filter(|e| {
                scopes.contains(&e.scope)
                    && from.is_none_or(|f| e.occurred_at >= f)
                    && until.is_none_or(|u| e.occurred_at < u)
            })
            .filter_map(|e| {
                let text = crate::text::norm(&e.text);
                let n = terms.iter().filter(|t| text.contains(t.as_str())).count();
                (n > 0 || e.entities.iter().any(|x| entities.contains(x))).then(|| (n, e.clone()))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.occurred_at.cmp(&a.1.occurred_at)));
        Ok(hits.into_iter().take(limit as usize).map(|h| h.1).collect())
    }

    async fn purge_owner(&self, owner: &str) -> Res<()> {
        let scope = format!("user:{owner}");
        self.0
            .lock()
            .unwrap()
            .episodes
            .retain(|_, e| e.owner_id != owner && e.actor_id != owner && e.scope != scope);
        Ok(())
    }
}
