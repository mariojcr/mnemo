// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! [`Db`]: distilled data, distillation queue, cursors and keys. [`RawStore`]: immutable episodes.
//! [`ch::ClickHouse`] implements both; [`mem::Mem`] is the in-memory test double.

/// Inclusive `seq` ranges already read; few, since reads are nearly contiguous.
pub type Seen = [(i64, i64)];

pub fn seen(skip: &Seen, seq: i64) -> bool {
    skip.iter().any(|(a, b)| *a <= seq && seq <= *b)
}

pub mod ch;
#[cfg(test)]
mod conformance;
#[cfg(test)]
pub mod mem;
use crate::episode::Episode;
use crate::meter::{Eval, Injection, LlmCall, Turn, Window};
use crate::model::{Entity, Memory};
use async_trait::async_trait;

pub type Res<T> = Result<T, String>;

#[derive(Clone, Debug)]
pub struct Thread {
    pub thread: String,
    pub scope: String,
    pub owner_id: String,
    pub reader_scopes: Vec<String>,
    pub upto_ms: i64,
    pub upto_id: String,
}

#[derive(Default, Debug)]
pub struct WritePlan {
    pub new_entities: Vec<Entity>,
    pub inserts: Vec<Memory>,
    /// (id, valid_to, superseded_by)
    pub closes: Vec<(String, i64, Option<String>)>,
    pub add_sources: Vec<(String, Vec<String>)>,
    /// (scope, entities, rev): if the scope has a current memory about these entities newer than
    /// `rev`, the judgement is stale and nothing is written.
    pub stale_check: Option<(String, Vec<String>, i64)>,
    /// (thread, upto_ms, upto_id, episodes processed)
    pub thread_advance: Option<(String, i64, String, i32)>,
}

#[derive(Debug, PartialEq)]
pub enum ApplyError {
    Stale,
    Db(String),
}

/// Only the key's sha256 is stored, never the key itself.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyRow {
    pub id: String,
    pub hash: String,
    pub name: String,
    /// admin | service | user
    pub kind: String,
    /// Its personal scope is `user:<subject>`.
    pub subject: String,
    pub created_ms: i64,
    pub revoked: bool,
    /// 0 = never. UI-created keys expire: they are not reconciled with the identity provider.
    pub expires_ms: i64,
    pub created_by: String,
}

impl KeyRow {
    pub fn active(&self, now: i64) -> bool {
        !self.revoked && (self.expires_ms == 0 || now < self.expires_ms)
    }
}

/// Display only; `subject` is the identity provider's immutable `sub`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdentityRow {
    pub subject: String,
    pub email: String,
    pub name: String,
    pub last_ms: i64,
}

/// `blocked_ms` > 0 = disconnected from the web UI (the identity provider cannot revoke for us).
/// Blocks live in their own table so an activity write with a stale row cannot undo one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionRow {
    pub subject: String,
    pub client: String,
    pub first_ms: i64,
    pub last_ms: i64,
    pub agent: String,
    pub blocked_ms: i64,
}

/// Only the sha256 of the cookie id is stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub hash: String,
    pub subject: String,
    pub email: String,
    pub name: String,
    /// admin | user
    pub kind: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub revoked: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoginRow {
    pub state_hash: String,
    pub verifier: String,
    pub nonce: String,
    pub return_to: String,
    pub expires_ms: i64,
}

#[derive(Clone, Debug, Default)]
pub struct MeterRows {
    pub llm: Vec<LlmCall>,
    pub injections: Vec<Injection>,
    pub turns: Vec<Turn>,
    pub evals: Vec<Eval>,
}

#[async_trait]
pub trait Db: Send + Sync {
    async fn migrate(&self) -> Res<()>;
    async fn upsert_entities(&self, ents: &[Entity], now: i64) -> Res<i64>;
    async fn set_summaries(&self, summaries: &[(String, String)]) -> Res<()>;
    /// Writes a new version with the vector. Without `replace`, only current memories lacking one;
    /// with `replace`, all of them, current or not.
    async fn set_embeddings(&self, items: &[(String, Vec<f32>)], replace: bool) -> Res<usize>;
    /// Rows with `rev` above the given one, in `rev` order, excluding `skip`.
    async fn entities_since(
        &self,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(Entity, bool, i64)>>;
    async fn memories_since(
        &self,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(Memory, bool, i64)>>;
    /// How long a log row may take to become visible: 0 on a single node, where rows appear in
    /// `rev` order. Replicas may show row 11 before row 10, so readers re-read this window.
    fn lag_window_ms(&self) -> i64 {
        0
    }
    /// Highest `seq` written before that instant: all at or below it is visible on every replica.
    async fn log_floor(&self, _before_ms: i64) -> Res<i64> {
        Ok(0)
    }
    /// Next log `seq`, if known. All writes go through the writer: unchanged means nothing new.
    async fn log_head(&self) -> Option<u64> {
        None
    }
    async fn apply(
        &self,
        plan: &WritePlan,
        embedding_model: &str,
        now: i64,
    ) -> Result<i64, ApplyError>;
    async fn forget(&self, id: &str, scopes: &[String], reason: &str, now: i64) -> Res<bool>;
    async fn purge_owner(&self, owner: &str) -> Res<(u64, u64)>;
    async fn touch_thread(&self, t: &Thread, last_ms: i64, n: i32) -> Res<()>;
    async fn claim_threads(
        &self,
        now: i64,
        settle_ms: i64,
        limit: i64,
        lease_ms: i64,
    ) -> Res<Vec<Thread>>;
    async fn cursor(&self, name: &str) -> Res<i64>;
    async fn set_cursor(&self, name: &str, value: i64) -> Res<()>;
    async fn stats(&self) -> Res<serde_json::Value>;
    async fn create_key(&self, k: &KeyRow) -> Res<()>;
    async fn key_by_hash(&self, hash: &str) -> Res<Option<KeyRow>>;
    async fn list_keys(&self) -> Res<Vec<KeyRow>>;
    async fn revoke_key(&self, id: &str) -> Res<bool>;
    async fn touch_key(&self, id: &str, at: i64) -> Res<()>;
    async fn keys_last_used(&self) -> Res<std::collections::HashMap<String, i64>>;

    async fn touch_identity(&self, i: &IdentityRow) -> Res<()>;
    async fn identity(&self, subject: &str) -> Res<Option<IdentityRow>>;
    async fn identities(&self) -> Res<Vec<IdentityRow>>;
    /// Activity only; leaves the block untouched.
    async fn put_connection(&self, c: &ConnectionRow) -> Res<()>;
    async fn connections(&self) -> Res<Vec<ConnectionRow>>;
    /// 0 = allowed.
    async fn put_block(&self, subject: &str, client: &str, blocked_ms: i64) -> Res<()>;
    async fn blocks(&self) -> Res<Vec<(String, String, i64)>>;

    async fn create_session(&self, s: &SessionRow) -> Res<()>;
    async fn session_by_hash(&self, hash: &str) -> Res<Option<SessionRow>>;
    async fn list_sessions(&self, now: i64) -> Res<Vec<SessionRow>>;
    async fn revoke_session(&self, id: &str) -> Res<bool>;
    async fn put_login(&self, l: &LoginRow) -> Res<()>;
    /// Consumes the login: a `state` is single-use.
    async fn take_login(&self, state_hash: &str, now: i64) -> Res<Option<LoginRow>>;

    async fn record_llm(&self, calls: &[LlmCall]) -> Res<()>;
    async fn record_injection(&self, i: &Injection) -> Res<()>;
    async fn record_turn(&self, t: &Turn) -> Res<()>;
    async fn record_evals(&self, evals: &[Eval]) -> Res<()>;
    /// Since the session's last context reset.
    async fn session_window(&self, session: &str) -> Res<Window>;
    async fn last_turn_at(&self, session: &str) -> Res<i64>;
    async fn injections_after(&self, session: &str, after_ms: i64) -> Res<Vec<Injection>>;
    async fn meter_rows(&self, from_ms: i64, subject: Option<&str>) -> Res<MeterRows>;
    async fn evals_since(&self, from_ms: i64) -> Res<Vec<Eval>>;
    /// (instance, lease expiry)
    async fn writer_status(&self) -> Res<Option<(String, i64)>>;
}

#[async_trait]
pub trait RawStore: Send + Sync {
    async fn insert(&self, eps: &[Episode], revision: i64) -> Res<()>;
    /// Strictly after the (time, id) cursor, in order.
    async fn thread_after(
        &self,
        thread: &str,
        scope: &str,
        upto_ms: i64,
        upto_id: &str,
        limit: u32,
    ) -> Res<Vec<Episode>>;
    /// The last `limit` episodes up to and including the cursor, in order.
    async fn thread_before(
        &self,
        thread: &str,
        scope: &str,
        upto_ms: i64,
        upto_id: &str,
        limit: u32,
    ) -> Res<Vec<Episode>>;
    async fn thread_episode_ids(
        &self,
        thread: &str,
        scopes: &[String],
        since_ms: i64,
    ) -> Res<Vec<String>>;
    async fn episode_with_context(
        &self,
        id: &str,
        scopes: &[String],
        around: u32,
    ) -> Res<Vec<Episode>>;
    async fn search(
        &self,
        terms: &[String],
        entities: &[String],
        scopes: &[String],
        from: Option<i64>,
        until: Option<i64>,
        limit: u32,
    ) -> Res<Vec<Episode>>;
    async fn purge_owner(&self, owner: &str) -> Res<()>;
    /// Skips known ids and snapshots equal to their thing's latest. Returns how many are new.
    async fn put_records(&self, rs: &[crate::records::Record]) -> Res<usize>;
    /// Newest first.
    async fn records(&self, q: &crate::records::Query) -> Res<Vec<crate::records::Record>>;
    async fn subjects(&self) -> Res<Vec<crate::records::Subject>>;
}
