//! ClickHouse store: immutable `episodes`, an append-only `log` (last row per id is current) and
//! `ReplacingMergeTree` state read with `FINAL`. No conditional writes: a single leased writer.

use super::{
    ApplyError, ConnectionRow, Db, IdentityRow, KeyRow, LoginRow, MeterRows, RawStore, Seen,
    SessionRow, Thread, WritePlan, seen,
};
use crate::episode::{Episode, fmt_ts};
use crate::meter::{Eval, Injection, LlmCall, Turn, Window};
use crate::model::{Entity, Memory};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

pub type Res<T> = Result<T, String>;

pub struct ClickHouse {
    url: String,
    db: String,
    user: String,
    password: String,
    http: reqwest::Client,
    write: tokio::sync::Mutex<u64>,
    claims: std::sync::Mutex<HashMap<String, i64>>,
    replicated: std::sync::atomic::AtomicBool,
    /// Fail on a missing or plain database instead of creating one that lives on a single replica.
    require_replicated: bool,
    /// Log rows written within the last window: the answering replica may not have them yet.
    own: std::sync::Mutex<std::collections::VecDeque<OwnRow>>,
}

/// How long a row may take to reach every replica. Rows only a downed replica had reappear when it
/// returns; if that takes longer than this, a running reader misses them until restart.
pub const LAG_WINDOW_MS: i64 = 15 * 60_000;

/// The lease is not taken from a replica with parts of these tables still to fetch.
const WRITER_TABLES: &str = "['log', 'writer']";

/// Client strings only enter queries as typed parameters (`{x:String}`), never spliced into SQL.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn arr(v: &[String]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|s| format!("'{}'", esc(s).replace('\'', "\\'")))
        .collect();
    format!("[{}]", items.join(","))
}

#[derive(Deserialize)]
struct Row {
    episode_id: String,
    thread: String,
    source: String,
    source_ref: String,
    scope: String,
    owner_id: String,
    actor_id: String,
    project_id: String,
    agent: String,
    role: String,
    occurred_ms: i64,
    text: String,
    entities: Vec<String>,
}

impl From<Row> for Episode {
    fn from(r: Row) -> Episode {
        Episode {
            episode_id: r.episode_id,
            thread: r.thread,
            source: r.source,
            source_ref: r.source_ref,
            scope: r.scope,
            owner_id: r.owner_id,
            actor_id: r.actor_id,
            project_id: r.project_id,
            agent: r.agent,
            role: r.role,
            occurred_at: r.occurred_ms,
            text: r.text,
            entities: r.entities,
        }
    }
}

const COLS: &str = "episode_id, thread, source, source_ref, scope, owner_id, actor_id, project_id, agent, role, toUnixTimestamp64Milli(occurred_at) AS occurred_ms, text, entities";

const SCHEMA: &[&str] = &[
    // text_norm: lowercased and unaccented at insert, so stem search ignores case and accents.
    "CREATE TABLE IF NOT EXISTS episodes (\
       episode_id String, thread String, source LowCardinality(String), source_ref String, \
       scope LowCardinality(String), owner_id String, actor_id String, project_id String, \
       agent LowCardinality(String), role LowCardinality(String), occurred_at DateTime64(3, 'UTC'), \
       text String CODEC(ZSTD(3)), \
       text_norm String MATERIALIZED replaceRegexpAll(normalizeUTF8NFD(lowerUTF8(text)), '\\\\p{Mn}', '') CODEC(ZSTD(3)), \
       entities Array(String), revision UInt64, deleted UInt8 DEFAULT 0 \
     ) ENGINE = ReplacingMergeTree(revision) PARTITION BY toYYYYMM(occurred_at) \
     ORDER BY (scope, thread, occurred_at, episode_id)",
    "CREATE TABLE IF NOT EXISTS log (\
       seq UInt64, kind LowCardinality(String), id String, scope LowCardinality(String), owner_id String, \
       entity_ids Array(String), live UInt8, deleted UInt8, body String CODEC(ZSTD(3)), \
       embedding Array(Float32) CODEC(ZSTD(1)), at DateTime64(3, 'UTC') DEFAULT now64(3) \
     ) ENGINE = MergeTree ORDER BY seq",
    "CREATE TABLE IF NOT EXISTS api_keys (\
       id String, hash String, name String, kind String, subject String, \
       created_ms Int64, revoked UInt8, version UInt64, expires_ms Int64 DEFAULT 0, created_by String DEFAULT '' \
     ) ENGINE = ReplacingMergeTree(version) ORDER BY id",
    "CREATE TABLE IF NOT EXISTS key_usage (id String, last_ms Int64) ENGINE = ReplacingMergeTree(last_ms) ORDER BY id",
    "CREATE TABLE IF NOT EXISTS identities (subject String, email String, name String, last_ms Int64) ENGINE = ReplacingMergeTree(last_ms) ORDER BY subject",
    "CREATE TABLE IF NOT EXISTS connections (subject String, client String, first_ms Int64, last_ms Int64, agent String) ENGINE = ReplacingMergeTree(last_ms) ORDER BY (subject, client)",
    "CREATE TABLE IF NOT EXISTS connection_blocks (subject String, client String, blocked_ms Int64, version Int64) ENGINE = ReplacingMergeTree(version) ORDER BY (subject, client)",
    "CREATE TABLE IF NOT EXISTS sessions (\
       id String, hash String, subject String, email String, name String, kind String, \
       created_ms Int64, expires_ms Int64, revoked UInt8, version UInt64 \
     ) ENGINE = ReplacingMergeTree(version) ORDER BY id",
    "CREATE TABLE IF NOT EXISTS logins (\
       state_hash String, verifier String, nonce String, return_to String, expires_ms Int64, used UInt8, version UInt64 \
     ) ENGINE = ReplacingMergeTree(version) ORDER BY state_hash \
     TTL toDateTime(intDiv(expires_ms, 1000)) + INTERVAL 1 DAY",
    "CREATE TABLE IF NOT EXISTS records (\
       id String, scope LowCardinality(String), stream LowCardinality(String), key String, \
       kind LowCardinality(String), state LowCardinality(String), status String, at DateTime64(3, 'UTC'), \
       title String, text String CODEC(ZSTD(3)), attrs Map(String, String), hash String\
     ) ENGINE = ReplacingMergeTree ORDER BY (scope, key, at, id)",
    // `at` is in milliseconds, like every other timestamp in mnemo.
    "CREATE TABLE IF NOT EXISTS llm_calls (\
       at Int64, kind LowCardinality(String), model LowCardinality(String), subject String, thread String, \
       prompt_tokens UInt64, cached_tokens UInt64, cache_creation_tokens UInt64, completion_tokens UInt64, ms UInt64, ok Bool \
     ) ENGINE = MergeTree ORDER BY at",
    "CREATE TABLE IF NOT EXISTS injections (\
       id String, at Int64, surface LowCardinality(String), subject String, session String, query String CODEC(ZSTD(3)), \
       memory_ids Array(String), entity_ids Array(String), chars UInt64, tokens UInt64, repeated UInt64, muted UInt64, \
       reset Bool, skipped LowCardinality(String) \
     ) ENGINE = MergeTree ORDER BY (session, at)",
    "CREATE TABLE IF NOT EXISTS turns (\
       at Int64, subject String, session String, source LowCardinality(String), model LowCardinality(String), \
       input_tokens UInt64, cache_read_tokens UInt64, cache_creation_tokens UInt64, output_tokens UInt64, calls UInt64 \
     ) ENGINE = MergeTree ORDER BY (session, at)",
    "CREATE TABLE IF NOT EXISTS evals (\
       at Int64, injection_id String, memory_id String, session String, subject String, used Bool, \
       method LowCardinality(String), p Float32 \
     ) ENGINE = MergeTree ORDER BY (memory_id, at)",
    // Versioned by its value: a cursor only moves forward, even if an old batch arrives late.
    "CREATE TABLE IF NOT EXISTS cursors (name String, value Int64) ENGINE = ReplacingMergeTree(value) ORDER BY name",
    "CREATE TABLE IF NOT EXISTS threads (\
       thread String, scope String, owner_id String, reader_scopes Array(String), last_ms Int64 \
     ) ENGINE = ReplacingMergeTree(last_ms) ORDER BY thread",
    "CREATE TABLE IF NOT EXISTS progress (\
       thread String, upto_ms Int64, upto_id String, version UInt64 \
     ) ENGINE = ReplacingMergeTree(version) ORDER BY thread",
    "CREATE TABLE IF NOT EXISTS writer (\
       name String, instance String, until_ms Int64, version UInt64 \
     ) ENGINE = ReplacingMergeTree(version) ORDER BY name",
];

/// No engine arguments: the Keeper path and replica name come from `default_replica_path`/`_name`.
fn replicated_ddl(ddl: &str) -> String {
    if ddl.starts_with("CREATE TABLE") {
        ddl.replacen("ENGINE = ", "ENGINE = Replicated", 1)
    } else {
        ddl.to_string()
    }
}

fn memory_row(seq: u64, m: &Memory, deleted: bool) -> Value {
    let ents: Vec<&str> = m.links.iter().map(|l| l.entity.as_str()).collect();
    json!({
        "seq": seq, "kind": "memory", "id": m.id, "scope": m.scope, "owner_id": m.owner_id,
        "entity_ids": ents, "live": (m.is_live() && !deleted) as u8, "deleted": deleted as u8,
        "body": serde_json::to_string(m).unwrap_or_default(),
        "embedding": m.embedding.clone().unwrap_or_default(),
    })
}

fn entity_row(seq: u64, e: &Entity, deleted: bool) -> Value {
    json!({
        "seq": seq, "kind": "entity", "id": e.id, "scope": e.scope, "owner_id": "",
        "entity_ids": [e.id.as_str()], "live": (!deleted) as u8, "deleted": deleted as u8,
        "body": serde_json::to_string(e).unwrap_or_default(), "embedding": Vec::<f32>::new(),
    })
}

fn tombstone(seq: u64, kind: &str, id: &str) -> Value {
    json!({"seq": seq, "kind": kind, "id": id, "scope": "", "owner_id": "", "entity_ids": [], "live": 0, "deleted": 1,
        "body": json!({"id": id}).to_string(), "embedding": Vec::<f32>::new()})
}

/// `serve` beats every 10 s, so the lease survives three missed beats, and a process that dies
/// holding it delays the next writer by at most this.
pub const WRITER_LEASE_MS: i64 = 40_000;

const LOG_COLS: &str = "seq, kind, id, scope, owner_id, entity_ids, live, deleted, body, embedding";

#[derive(Clone, Debug)]
struct OwnRow {
    at: i64,
    seq: i64,
    kind: String,
    id: String,
    scope: String,
    owner_id: String,
    entity_ids: Vec<String>,
    live: bool,
    deleted: bool,
    body: String,
    embedding: Vec<f32>,
}

impl OwnRow {
    fn of(at: i64, r: &Value) -> OwnRow {
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        OwnRow {
            at,
            seq: i64_of(&r["seq"]),
            kind: s("kind"),
            id: s("id"),
            scope: s("scope"),
            owner_id: s("owner_id"),
            entity_ids: strings(&r["entity_ids"]),
            live: i64_of(&r["live"]) != 0,
            deleted: i64_of(&r["deleted"]) != 0,
            body: s("body"),
            embedding: floats(&r["embedding"]),
        }
    }
}

/// A tombstone's body carries only the id.
fn parse_entity(body: &str) -> Entity {
    serde_json::from_str(body).unwrap_or_else(|_| Entity {
        id: serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v["id"].as_str().map(String::from))
            .unwrap_or_default(),
        ..Default::default()
    })
}

fn parse_memory(body: &str, embedding: Vec<f32>) -> Memory {
    let mut m: Memory = serde_json::from_str(body).unwrap_or_else(|_| Memory {
        id: serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v["id"].as_str().map(String::from))
            .unwrap_or_default(),
        ..Default::default()
    });
    m.embedding = (!embedding.is_empty()).then_some(embedding);
    m
}

fn i64_of(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn floats(v: &Value) -> Vec<f32> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64())
                .map(|x| x as f32)
                .collect()
        })
        .unwrap_or_default()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

impl ClickHouse {
    pub fn new(url: &str, db: &str, user: &str, password: &str) -> Res<ClickHouse> {
        if !db.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || db.is_empty() {
            return Err(format!("invalid ClickHouse database name: {db:?}"));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|x| x.to_string())?;
        Ok(ClickHouse {
            url: url.trim_end_matches('/').to_string(),
            db: db.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            http,
            write: tokio::sync::Mutex::new(0),
            claims: std::sync::Mutex::new(HashMap::new()),
            replicated: std::sync::atomic::AtomicBool::new(false),
            require_replicated: false,
            own: std::sync::Mutex::new(std::collections::VecDeque::new()),
        })
    }

    pub fn require_replicated(mut self, on: bool) -> ClickHouse {
        self.require_replicated = on;
        self
    }

    /// Known only after `migrate`.
    pub fn is_replicated(&self) -> bool {
        self.replicated.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn run(
        &self,
        sql: &str,
        params: &[(&str, String)],
        body: Option<String>,
        use_db: bool,
    ) -> Res<String> {
        self.run_with(sql, params, body, use_db, &[]).await
    }

    async fn run_with(
        &self,
        sql: &str,
        params: &[(&str, String)],
        body: Option<String>,
        use_db: bool,
        settings: &[(&str, &str)],
    ) -> Res<String> {
        // Without data the query goes in the body, so the POST carries the Content-Length it needs.
        let mut q: Vec<(String, String)> =
            vec![("output_format_json_quote_64bit_integers".into(), "0".into())];
        for (k, v) in settings {
            q.push((k.to_string(), v.to_string()));
        }
        let payload = match body {
            Some(data) => {
                q.push(("query".into(), sql.to_string()));
                data
            }
            None => sql.to_string(),
        };
        if use_db {
            q.push(("database".into(), self.db.clone()));
        }
        for (k, v) in params {
            q.push((format!("param_{k}"), v.clone()));
        }
        let req = self
            .http
            .post(&self.url)
            .query(&q)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(payload);
        let resp = req.send().await.map_err(|x| format!("clickhouse: {x}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|x| x.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "clickhouse {}: {}",
                status.as_u16(),
                text.chars().take(500).collect::<String>()
            ));
        }
        Ok(text)
    }

    async fn rows(&self, sql: &str, params: &[(&str, String)]) -> Res<Vec<Episode>> {
        Ok(self
            .json(sql, params)
            .await?
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Row>(v).ok().map(Episode::from))
            .collect())
    }

    async fn json(&self, sql: &str, params: &[(&str, String)]) -> Res<Vec<Value>> {
        let text = self
            .run(&format!("{sql} FORMAT JSONEachRow"), params, None, true)
            .await?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|x| x.to_string()))
            .collect()
    }

    /// The metering tables must use the same column names as their structs.
    async fn typed<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        params: &[(&str, String)],
    ) -> Res<Vec<T>> {
        self.json(sql, params)
            .await?
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
            .collect()
    }

    async fn insert_meter<T: serde::Serialize>(&self, table: &str, rows: &[T]) -> Res<()> {
        let rows: Vec<Value> = rows
            .iter()
            .filter_map(|r| serde_json::to_value(r).ok())
            .collect();
        let cols: Vec<String> = rows
            .first()
            .and_then(|r| r.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        self.insert_rows(table, &cols.join(", "), &rows).await
    }

    async fn exec(&self, sql: &str, params: &[(&str, String)]) -> Res<()> {
        self.run(sql, params, None, true).await.map(|_| ())
    }

    async fn insert_rows(&self, table: &str, cols: &str, rows: &[Value]) -> Res<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let body: String = rows.iter().map(|r| r.to_string() + "\n").collect();
        self.run(
            &format!("INSERT INTO {table} ({cols}) FORMAT JSONEachRow"),
            &[],
            Some(body),
            true,
        )
        .await
        .map(|_| ())
    }

    async fn ping(&self) -> bool {
        self.http
            .get(format!("{}/ping", self.url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    pub async fn wait_ready(&self, secs: u64) -> Res<()> {
        for _ in 0..secs * 4 {
            if self.ping().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(format!("ClickHouse is not answering at {}", self.url))
    }

    /// (body, deleted, embedding) of each id's latest version. Own rows win when newer: the replica
    /// may lack them, and building on the previous version would undo a close or source just added.
    async fn latest(
        &self,
        kind: &str,
        ids: &[String],
    ) -> Res<HashMap<String, (String, bool, Vec<f32>)>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = self
            .json(
                "SELECT id, max(seq) AS last_seq, argMax(body, seq) AS body, argMax(deleted, seq) AS deleted, argMax(embedding, seq) AS embedding \
                 FROM log WHERE kind = {k:String} AND id IN {ids:Array(String)} GROUP BY id",
                &[("k", esc(kind)), ("ids", arr(ids))],
            )
            .await?;
        let mut out: HashMap<String, (i64, String, bool, Vec<f32>)> = rows
            .into_iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or("").to_string(),
                    (
                        i64_of(&r["last_seq"]),
                        r["body"].as_str().unwrap_or("").to_string(),
                        i64_of(&r["deleted"]) != 0,
                        floats(&r["embedding"]),
                    ),
                )
            })
            .collect();
        for o in self.own_rows(|o| o.kind == kind && ids.contains(&o.id)) {
            if out.get(&o.id).is_none_or(|(seq, ..)| o.seq > *seq) {
                out.insert(o.id, (o.seq, o.body, o.deleted, o.embedding));
            }
        }
        Ok(out
            .into_iter()
            .map(|(id, (_, body, deleted, emb))| (id, (body, deleted, emb)))
            .collect())
    }

    fn own_any(&self, pred: impl Fn(&OwnRow) -> bool) -> bool {
        self.own.lock().unwrap().iter().any(pred)
    }

    /// Waits only for this replica: waiting for all would abort the delete halfway (no tombstones)
    /// when one is down; the others get the mutation through their queue.
    async fn delete(&self, sql: &str, params: &[(&str, String)]) -> Res<()> {
        let settings: &[(&str, &str)] = if self.is_replicated() {
            &[("lightweight_deletes_sync", "1")]
        } else {
            &[]
        };
        self.run_with(sql, params, None, true, settings)
            .await
            .map(|_| ())
    }

    fn own_rows(&self, keep: impl Fn(&OwnRow) -> bool) -> Vec<OwnRow> {
        self.own
            .lock()
            .unwrap()
            .iter()
            .filter(|o| keep(o))
            .cloned()
            .collect()
    }

    async fn append(&self, rows: &[Value]) -> Res<()> {
        self.insert_rows("log", LOG_COLS, rows).await?;
        if self.is_replicated() && !rows.is_empty() {
            let now = crate::app::now_ms();
            let mut own = self.own.lock().unwrap();
            while own.front().is_some_and(|o| o.at < now - LAG_WINDOW_MS) {
                own.pop_front();
            }
            own.extend(rows.iter().map(|r| OwnRow::of(now, r)));
        }
        Ok(())
    }

    async fn entity_rows(&self, ents: &[Entity], next: &mut u64) -> Res<Vec<Value>> {
        let prev = self
            .latest(
                "entity",
                &ents.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            )
            .await?;
        let mut rows = Vec::new();
        for e in ents {
            let mut e = e.clone();
            if e.summary.is_empty() {
                if let Some((body, false, _)) = prev.get(&e.id) {
                    e.summary = parse_entity(body).summary;
                }
            }
            rows.push(entity_row(*next, &e, false));
            *next += 1;
        }
        Ok(rows)
    }

    /// (seq, deleted, body, embedding), including own rows the answering replica lacks.
    async fn log_since(
        &self,
        kind: &str,
        embedding: &str,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(i64, bool, String, Vec<f32>)>> {
        let mut params = vec![
            ("k", esc(kind)),
            ("r", rev.max(0).to_string()),
            ("n", limit.to_string()),
        ];
        let skip_sql = if skip.is_empty() {
            ""
        } else {
            let list: Vec<String> = skip.iter().map(|(a, b)| format!("({a},{b})")).collect();
            params.push(("skip", format!("[{}]", list.join(","))));
            " AND NOT arrayExists(r -> seq BETWEEN r.1 AND r.2, {skip:Array(Tuple(UInt64, UInt64))})"
        };
        let rows = self
            .json(
                &format!(
                    "SELECT seq, deleted, body{embedding} FROM log WHERE kind = {{k:String}} AND seq > {{r:UInt64}}{skip_sql} \
                     ORDER BY seq LIMIT {{n:UInt64}}"
                ),
                &params,
            )
            .await?;
        let mut out: Vec<(i64, bool, String, Vec<f32>)> = rows
            .iter()
            .map(|r| {
                (
                    i64_of(&r["seq"]),
                    i64_of(&r["deleted"]) != 0,
                    r["body"].as_str().unwrap_or("").to_string(),
                    floats(&r["embedding"]),
                )
            })
            .collect();
        let have: std::collections::HashSet<i64> = out.iter().map(|r| r.0).collect();
        let mine = self.own_rows(|o| {
            o.kind == kind && o.seq > rev && !seen(skip, o.seq) && !have.contains(&o.seq)
        });
        if !mine.is_empty() {
            out.extend(
                mine.into_iter()
                    .map(|o| (o.seq, o.deleted, o.body, o.embedding)),
            );
            out.sort_by_key(|r| r.0);
            out.truncate(limit.max(0) as usize);
        }
        Ok(out)
    }

    /// Retries when replicated: a just-created table may not have reached this replica yet.
    async fn max_seq(&self, replicated: bool) -> Res<u64> {
        let mut tries = if replicated { 30 } else { 1 };
        loop {
            match self.json("SELECT max(seq) AS m FROM log", &[]).await {
                Ok(rows) => return Ok(rows.first().map(|r| i64_of(&r["m"]) as u64).unwrap_or(0)),
                Err(e) if tries <= 1 => return Err(e),
                Err(_) => {
                    tries -= 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Two writers would hand out the same sequence and clobber each other's judgements.
    pub async fn claim_writer(&self, instance: &str, now: i64) -> Res<()> {
        let rows = if self.is_replicated() {
            // One query, so one replica answers both the lease and whether it lags (parts still to
            // fetch may hide the live writer's last beat; read-only means it lost Keeper).
            // `inserts_in_queue` is unusable: it reads 1 briefly after the replica's own insert.
            let rows = self
                .json(
                    &format!(
                        "SELECT w.instance AS instance, w.until_ms AS until_ms, r.lagging AS lagging FROM \
                         (SELECT 1 AS k, \
                           (SELECT count() FROM system.replicas WHERE database = currentDatabase() AND has({WRITER_TABLES}, table) AND is_readonly) \
                           + (SELECT count() FROM system.replication_queue WHERE database = currentDatabase() AND has({WRITER_TABLES}, table) \
                              AND type = 'GET_PART' AND source_replica != replica_name) AS lagging) AS r \
                         LEFT JOIN (SELECT 1 AS k, instance, until_ms FROM writer FINAL WHERE name = 'writer') AS w USING k"
                    ),
                    &[],
                )
                .await?;
            if rows.first().is_some_and(|r| i64_of(&r["lagging"]) > 0) {
                return Err(
                    "the ClickHouse replica answering is lagging on the log or the writer lease; not claiming the lease through it".into(),
                );
            }
            rows.into_iter()
                .filter(|r| !r["instance"].as_str().unwrap_or("").is_empty())
                .collect()
        } else {
            self.json(
                "SELECT instance, until_ms FROM writer FINAL WHERE name = 'writer'",
                &[],
            )
            .await?
        };
        if let Some(r) = rows.first() {
            let (other, until) = (r["instance"].as_str().unwrap_or(""), i64_of(&r["until_ms"]));
            if other != instance && until > now {
                return Err(format!(
                    "another writer ({other}) is already active on this database; for more replicas use `mnemo api` with MNEMO_WRITER_URL"
                ));
            }
        }
        self.beat(instance, now).await?;
        // Two processes starting together both insert and the higher version wins: read back and,
        // if the lease is not ours, yield.
        match self.writer_status().await? {
            Some((other, until)) if other != instance && until > now => Err(format!(
                "another writer ({other}) took the lease at the same time; for more replicas use `mnemo api` with MNEMO_WRITER_URL"
            )),
            _ => Ok(()),
        }
    }

    /// `Ok(false)` if another writer took over (this process stalled past the lease): stop writing.
    pub async fn heartbeat(&self, instance: &str, now: i64) -> Res<bool> {
        if let Some((other, until)) = self.writer_status().await?
            && other != instance
            && until > now
        {
            return Ok(false);
        }
        self.beat(instance, now).await?;
        Ok(true)
    }

    async fn beat(&self, instance: &str, now: i64) -> Res<()> {
        self.insert_rows("writer", "name, instance, until_ms, version", &[json!({"name": "writer", "instance": instance, "until_ms": now + WRITER_LEASE_MS, "version": now})]).await
    }

    #[cfg(test)]
    pub async fn drop_database(&self) -> Res<()> {
        self.run(
            &format!("DROP DATABASE IF EXISTS {} SYNC", self.db),
            &[],
            None,
            false,
        )
        .await
        .map(|_| ())
    }

    #[cfg(test)]
    pub fn on(&self, url: &str) -> ClickHouse {
        ClickHouse::new(url, &self.db, &self.user, &self.password).unwrap()
    }

    #[cfg(test)]
    pub async fn create_replicated_database(&self) -> Res<()> {
        let sql = format!(
            "CREATE DATABASE IF NOT EXISTS {0} ENGINE = Replicated('/clickhouse/databases/{0}', '{{shard}}', '{{replica}}')",
            self.db
        );
        let mut last = String::new();
        for _ in 0..30 {
            match self.run(&sql, &[], None, false).await {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(last)
    }

    pub async fn release_writer(&self, instance: &str, now: i64) -> Res<()> {
        self.insert_rows(
            "writer",
            "name, instance, until_ms, version",
            &[json!({"name": "writer", "instance": instance, "until_ms": 0, "version": now + 1})],
        )
        .await
    }
}

#[async_trait]
impl Db for ClickHouse {
    async fn migrate(&self) -> Res<()> {
        let found = self
            .run(
                "SELECT engine FROM system.databases WHERE name = {d:String} FORMAT JSONEachRow",
                &[("d", esc(&self.db))],
                None,
                false,
            )
            .await?;
        let engine = found
            .lines()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|v| v["engine"].as_str().map(String::from));
        let replicated = match engine.as_deref() {
            Some("Replicated") => true,
            Some(other) if self.require_replicated => {
                return Err(format!(
                    "MNEMO_CH_REPLICATED: database {} is {other}, not Replicated",
                    self.db
                ));
            }
            None if self.require_replicated => {
                return Err(format!(
                    "MNEMO_CH_REPLICATED: database {} does not exist; create it with ENGINE = Replicated on EVERY replica (docs/storage.md)",
                    self.db
                ));
            }
            Some(_) => false,
            None => {
                self.run(
                    &format!("CREATE DATABASE IF NOT EXISTS {}", self.db),
                    &[],
                    None,
                    false,
                )
                .await?;
                false
            }
        };
        self.replicated
            .store(replicated, std::sync::atomic::Ordering::Relaxed);
        if replicated {
            // Replicated databases don't convert engines: a hand-made `MergeTree` is unreplicated.
            let plain = self
                .json(
                    "SELECT name, engine FROM system.tables WHERE database = currentDatabase() \
                     AND endsWith(engine, 'MergeTree') AND NOT startsWith(engine, 'Replicated')",
                    &[],
                )
                .await?;
            if !plain.is_empty() {
                let names: Vec<String> = plain
                    .iter()
                    .map(|r| {
                        format!(
                            "{} ({})",
                            r["name"].as_str().unwrap_or(""),
                            r["engine"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                return Err(format!(
                    "database {} is Replicated but these tables are not: {}. Their data lives on one replica only",
                    self.db,
                    names.join(", ")
                ));
            }
        }
        for ddl in SCHEMA.iter() {
            if replicated {
                // Don't wait for other replicas: with one down, even a no-op DDL blocks minutes.
                self.run_with(
                    &replicated_ddl(ddl),
                    &[],
                    None,
                    true,
                    &[("distributed_ddl_task_timeout", "0")],
                )
                .await?;
            } else {
                self.exec(ddl, &[]).await?;
            }
        }
        let max = self.max_seq(replicated).await?;
        let mut next = max + 1;
        if replicated {
            // The answering replica may lack the previous writer's latest rows, whose `seq` must
            // not be reused. Starting above the clock in µs is safe: no writer writes a row per µs.
            next = next.max(crate::app::now_ms() as u64 * 1000);
        }
        *self.write.lock().await = next;
        Ok(())
    }

    async fn upsert_entities(&self, ents: &[Entity], _now: i64) -> Res<i64> {
        let mut next = self.write.lock().await;
        let mut seq = *next;
        let rows = self.entity_rows(ents, &mut seq).await?;
        // Advance even if the insert fails: it may have landed with the response cut, and reusing
        // those `seq` would leave two rows sharing one.
        *next = seq;
        self.append(&rows).await?;
        Ok(seq as i64 - 1)
    }

    async fn set_embeddings(&self, items: &[(String, Vec<f32>)], replace: bool) -> Res<usize> {
        let mut next = self.write.lock().await;
        let ids: Vec<String> = items.iter().map(|i| i.0.clone()).collect();
        let prev = self.latest("memory", &ids).await?;
        let mut seq = *next;
        let mut rows = Vec::new();
        for (id, v) in items {
            let Some((body, false, emb)) = prev.get(id) else {
                continue;
            };
            if !replace && !emb.is_empty() {
                continue;
            }
            let mut m = parse_memory(body, Vec::new());
            if !replace && !m.is_live() {
                continue;
            }
            m.embedding = Some(v.clone());
            rows.push(memory_row(seq, &m, false));
            seq += 1;
        }
        *next = seq;
        self.append(&rows).await?;
        Ok(rows.len())
    }

    async fn set_summaries(&self, summaries: &[(String, String)]) -> Res<()> {
        let mut next = self.write.lock().await;
        let prev = self
            .latest(
                "entity",
                &summaries.iter().map(|s| s.0.clone()).collect::<Vec<_>>(),
            )
            .await?;
        let mut seq = *next;
        let mut rows = Vec::new();
        for (id, summary) in summaries {
            let Some((body, false, _)) = prev.get(id) else {
                continue;
            };
            let mut e = parse_entity(body);
            if &e.summary != summary {
                e.summary = summary.clone();
                rows.push(entity_row(seq, &e, false));
                seq += 1;
            }
        }
        *next = seq;
        self.append(&rows).await?;
        Ok(())
    }

    async fn entities_since(
        &self,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(Entity, bool, i64)>> {
        let rows = self.log_since("entity", "", rev, skip, limit).await?;
        Ok(rows
            .into_iter()
            .map(|(seq, deleted, body, _)| (parse_entity(&body), deleted, seq))
            .collect())
    }

    /// The mutex waits out an in-flight write (`wait_for_async_insert` makes it readable by then).
    /// Replicated: the write may not have reached the answering replica, so always read.
    async fn log_head(&self) -> Option<u64> {
        if self.is_replicated() {
            return None;
        }
        Some(*self.write.lock().await)
    }

    async fn memories_since(
        &self,
        rev: i64,
        skip: &Seen,
        limit: i64,
    ) -> Res<Vec<(Memory, bool, i64)>> {
        let rows = self
            .log_since("memory", ", embedding", rev, skip, limit)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(seq, deleted, body, emb)| (parse_memory(&body, emb), deleted, seq))
            .collect())
    }

    fn lag_window_ms(&self) -> i64 {
        if self.is_replicated() {
            LAG_WINDOW_MS
        } else {
            0
        }
    }

    async fn log_floor(&self, before_ms: i64) -> Res<i64> {
        let rows = self
            .json(
                "SELECT max(seq) AS m FROM log WHERE at < fromUnixTimestamp64Milli({t:Int64})",
                &[("t", before_ms.to_string())],
            )
            .await?;
        Ok(rows.first().map(|r| i64_of(&r["m"])).unwrap_or(0))
    }

    async fn apply(
        &self,
        plan: &WritePlan,
        _embedding_model: &str,
        _now: i64,
    ) -> Result<i64, ApplyError> {
        let db = ApplyError::Db;
        let mut next = self.write.lock().await;
        // Stands in for a transaction: under the lease and mutex nobody writes until the insert.
        if let Some((scope, ents, seen)) = &plan.stale_check {
            if !ents.is_empty() {
                let n = self
                    .json(
                        "SELECT count() AS n FROM log WHERE kind = 'memory' AND seq > {seen:UInt64} AND scope = {s:String} AND live = 1 AND hasAny(entity_ids, {ents:Array(String)})",
                        &[("seen", seen.max(&0).to_string()), ("s", esc(scope)), ("ents", arr(ents))],
                    )
                    .await
                    .map_err(db)?;
                if n.first().map(|r| i64_of(&r["n"])).unwrap_or(0) > 0 {
                    return Err(ApplyError::Stale);
                }
                // Own writes count even if the answering replica lacks them.
                if self.own_any(|o| {
                    o.kind == "memory"
                        && o.seq > *seen
                        && &o.scope == scope
                        && o.live
                        && o.entity_ids.iter().any(|e| ents.contains(e))
                }) {
                    return Err(ApplyError::Stale);
                }
            }
        }
        let mut seq = *next;
        let mut rows = self
            .entity_rows(&plan.new_entities, &mut seq)
            .await
            .map_err(db)?;
        let touched: Vec<String> = plan
            .closes
            .iter()
            .map(|c| c.0.clone())
            .chain(plan.add_sources.iter().map(|a| a.0.clone()))
            .collect();
        let prev = self.latest("memory", &touched).await.map_err(db)?;
        let mut current: HashMap<String, Memory> = prev
            .into_iter()
            .filter(|(_, (_, deleted, _))| !deleted)
            .map(|(id, (body, _, emb))| (id, parse_memory(&body, emb)))
            .collect();
        for (id, valid_to, by) in &plan.closes {
            if let Some(m) = current.get_mut(id).filter(|m| m.is_live()) {
                m.valid_to = Some(*valid_to);
                if by.is_some() {
                    m.superseded_by = by.clone();
                }
                rows.push(memory_row(seq, m, false));
                seq += 1;
            }
        }
        for (id, eps) in &plan.add_sources {
            if let Some(m) = current.get_mut(id) {
                for ep in eps {
                    if !m.source_episodes.contains(ep) {
                        m.source_episodes.push(ep.clone());
                    }
                }
                rows.push(memory_row(seq, m, false));
                seq += 1;
            }
        }
        for m in &plan.inserts {
            rows.push(memory_row(seq, m, false));
            seq += 1;
        }
        *next = seq;
        self.append(&rows).await.map_err(db)?;
        if let Some((thread, upto_ms, upto_id, _)) = &plan.thread_advance {
            self.insert_rows("progress", "thread, upto_ms, upto_id, version", &[json!({"thread": thread, "upto_ms": upto_ms, "upto_id": upto_id, "version": seq})]).await.map_err(db)?;
            self.claims.lock().unwrap().remove(thread);
        }
        Ok(seq as i64 - 1)
    }

    async fn forget(&self, id: &str, scopes: &[String], _reason: &str, now: i64) -> Res<bool> {
        let mut next = self.write.lock().await;
        let prev = self.latest("memory", &[id.to_string()]).await?;
        let Some((body, false, emb)) = prev.get(id) else {
            return Ok(false);
        };
        let mut m = parse_memory(body, emb.clone());
        if !m.is_live() || !scopes.contains(&m.scope) {
            return Ok(false);
        }
        m.valid_to = Some(now);
        let seq = *next;
        *next += 1;
        self.append(&[memory_row(seq, &m, false)]).await?;
        Ok(true)
    }

    async fn purge_owner(&self, owner: &str) -> Res<(u64, u64)> {
        let mut next = self.write.lock().await;
        let scope = format!("user:{owner}");
        let person = format!("person:{owner}");
        let params = [("o", esc(owner)), ("s", esc(&scope)), ("p", esc(&person))];
        let filter = "(owner_id = {o:String} OR scope = {s:String} OR (kind = 'entity' AND id = {p:String}))";
        let mut ids: std::collections::BTreeSet<(String, String)> = self
            .json(
                &format!("SELECT DISTINCT kind, id FROM log WHERE {filter}"),
                &params,
            )
            .await?
            .iter()
            .map(|r| {
                (
                    r["kind"].as_str().unwrap_or("memory").to_string(),
                    r["id"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        // Own rows the replica lacks are the owner's too; drop them from the buffer, or `latest`
        // would return them and a write would revive them.
        let is_theirs = |o: &OwnRow| {
            o.owner_id == owner || o.scope == scope || (o.kind == "entity" && o.id == person)
        };
        ids.extend(self.own_rows(is_theirs).into_iter().map(|o| (o.kind, o.id)));
        // The WHOLE history (every version has the text); tombstones make replicas drop the ids.
        self.delete(&format!("DELETE FROM log WHERE {filter}"), &params)
            .await?;
        self.own.lock().unwrap().retain(|o| !is_theirs(o));
        let mut seq = *next;
        let (mut mems, mut ents) = (0u64, 0u64);
        let rows: Vec<Value> = ids
            .iter()
            .map(|(kind, id)| {
                if kind == "entity" {
                    ents += 1
                } else {
                    mems += 1
                }
                let row = tombstone(seq, kind, id);
                seq += 1;
                row
            })
            .collect();
        *next = seq;
        self.append(&rows).await?;
        self.delete(
            "DELETE FROM threads WHERE owner_id = {o:String} OR scope = {s:String}",
            &params[..2],
        )
        .await?;
        // Metering holds fragments of their queries; identities and connections are theirs too.
        for table in [
            "injections",
            "turns",
            "evals",
            "llm_calls",
            "sessions",
            "identities",
            "connections",
            "connection_blocks",
        ] {
            self.delete(
                &format!("DELETE FROM {table} WHERE subject = {{o:String}}"),
                &params[..1],
            )
            .await?;
        }
        Ok((mems, ents))
    }

    async fn touch_thread(&self, t: &Thread, last_ms: i64, _n: i32) -> Res<()> {
        self.insert_rows("threads", "thread, scope, owner_id, reader_scopes, last_ms", &[json!({"thread": t.thread, "scope": t.scope, "owner_id": t.owner_id, "reader_scopes": t.reader_scopes, "last_ms": last_ms})]).await
    }

    async fn claim_threads(
        &self,
        now: i64,
        settle_ms: i64,
        limit: i64,
        lease_ms: i64,
    ) -> Res<Vec<Thread>> {
        let rows = self
            .json(
                "SELECT thread, scope, owner_id, reader_scopes, upto_ms, upto_id FROM ( \
                   SELECT t.thread AS thread, t.scope AS scope, t.owner_id AS owner_id, t.reader_scopes AS reader_scopes, p.upto_ms AS upto_ms, p.upto_id AS upto_id, t.last_ms AS last_ms, \
                          row_number() OVER (PARTITION BY t.owner_id ORDER BY t.last_ms) AS rn \
                   FROM (SELECT * FROM threads FINAL) AS t LEFT JOIN (SELECT * FROM progress FINAL) AS p ON p.thread = t.thread \
                   WHERE t.last_ms > p.upto_ms AND t.last_ms < {cut:Int64} \
                 ) WHERE rn <= 2 ORDER BY rn, last_ms LIMIT {n:UInt64}",
                &[("cut", (now - settle_ms).to_string()), ("n", (limit * 4).to_string())],
            )
            .await?;
        let mut claims = self.claims.lock().unwrap();
        claims.retain(|_, until| *until > now);
        let mut out = Vec::new();
        for r in rows {
            let thread = r["thread"].as_str().unwrap_or("").to_string();
            if claims.contains_key(&thread) || out.len() as i64 >= limit {
                continue;
            }
            claims.insert(thread.clone(), now + lease_ms);
            out.push(Thread {
                thread,
                scope: r["scope"].as_str().unwrap_or("").into(),
                owner_id: r["owner_id"].as_str().unwrap_or("").into(),
                reader_scopes: strings(&r["reader_scopes"]),
                upto_ms: i64_of(&r["upto_ms"]),
                upto_id: r["upto_id"].as_str().unwrap_or("").into(),
            });
        }
        Ok(out)
    }

    async fn cursor(&self, name: &str) -> Res<i64> {
        let rows = self
            .json(
                "SELECT value FROM cursors FINAL WHERE name = {n:String}",
                &[("n", esc(name))],
            )
            .await?;
        Ok(rows.first().map(|r| i64_of(&r["value"])).unwrap_or(0))
    }

    async fn set_cursor(&self, name: &str, value: i64) -> Res<()> {
        self.insert_rows(
            "cursors",
            "name, value",
            &[json!({"name": name, "value": value})],
        )
        .await
    }

    async fn stats(&self) -> Res<Value> {
        let q = |sql: &'static str| async move {
            self.json(sql, &[])
                .await
                .map(|r| r.first().map(|x| i64_of(&x["n"])).unwrap_or(0))
        };
        let folded = self
            .json(
                "SELECT countIf(kind = 'memory' AND d = 0) AS memories, countIf(kind = 'memory' AND d = 0 AND l = 1) AS live, countIf(kind = 'entity' AND d = 0) AS entities \
                 FROM (SELECT kind, id, argMax(deleted, seq) AS d, argMax(live, seq) AS l FROM log GROUP BY kind, id)",
                &[],
            )
            .await?;
        let f = folded.first().cloned().unwrap_or_default();
        Ok(json!({
            "memories": i64_of(&f["memories"]),
            "live": i64_of(&f["live"]),
            "entities": i64_of(&f["entities"]),
            "pending_threads": q("SELECT count() AS n FROM (SELECT * FROM threads FINAL) AS t LEFT JOIN (SELECT * FROM progress FINAL) AS p ON p.thread = t.thread WHERE t.last_ms > p.upto_ms").await?,
            "episodes": q("SELECT count() AS n FROM episodes FINAL WHERE deleted = 0").await?,
        }))
    }

    async fn create_key(&self, k: &KeyRow) -> Res<()> {
        let _w = self.write.lock().await;
        self.insert_rows("api_keys", KEY_INSERT, &[key_row(k, k.created_ms as u64)])
            .await
    }

    async fn key_by_hash(&self, hash: &str) -> Res<Option<KeyRow>> {
        let rows = self.json("SELECT id, hash, name, kind, subject, created_ms, revoked, expires_ms, created_by FROM api_keys FINAL WHERE hash = {h:String}", &[("h", esc(hash))]).await?;
        Ok(rows.first().map(key_from))
    }

    async fn list_keys(&self) -> Res<Vec<KeyRow>> {
        let rows = self.json("SELECT id, hash, name, kind, subject, created_ms, revoked, expires_ms, created_by FROM api_keys FINAL ORDER BY created_ms", &[]).await?;
        Ok(rows.iter().map(key_from).collect())
    }

    async fn revoke_key(&self, id: &str) -> Res<bool> {
        let _w = self.write.lock().await;
        let rows = self.json("SELECT id, hash, name, kind, subject, created_ms, revoked, expires_ms, created_by FROM api_keys FINAL WHERE id = {i:String}", &[("i", esc(id))]).await?;
        let Some(mut k) = rows.first().map(key_from).filter(|k| !k.revoked) else {
            return Ok(false);
        };
        k.revoked = true;
        let version = crate::app::now_ms().max(k.created_ms + 1);
        self.insert_rows("api_keys", KEY_INSERT, &[key_row(&k, version as u64)])
            .await?;
        Ok(true)
    }

    async fn touch_key(&self, id: &str, at: i64) -> Res<()> {
        self.insert_rows(
            "key_usage",
            "id, last_ms",
            &[json!({"id": id, "last_ms": at})],
        )
        .await
    }

    async fn keys_last_used(&self) -> Res<HashMap<String, i64>> {
        let rows = self
            .json(
                "SELECT id, max(last_ms) AS l FROM key_usage GROUP BY id",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r["id"].as_str().unwrap_or("").to_string(), i64_of(&r["l"])))
            .collect())
    }

    async fn touch_identity(&self, i: &IdentityRow) -> Res<()> {
        self.insert_rows(
            "identities",
            "subject, email, name, last_ms",
            &[json!({"subject": i.subject, "email": i.email, "name": i.name, "last_ms": i.last_ms})],
        )
        .await
    }

    async fn identity(&self, subject: &str) -> Res<Option<IdentityRow>> {
        let rows = self
            .json(
                "SELECT subject, email, name, last_ms FROM identities FINAL WHERE subject = {s:String}",
                &[("s", esc(subject))],
            )
            .await?;
        Ok(rows.first().map(identity_from))
    }

    async fn put_connection(&self, c: &ConnectionRow) -> Res<()> {
        self.insert_rows(
            "connections",
            "subject, client, first_ms, last_ms, agent",
            &[json!({"subject": c.subject, "client": c.client, "first_ms": c.first_ms, "last_ms": c.last_ms, "agent": c.agent})],
        )
        .await
    }

    async fn put_block(&self, subject: &str, client: &str, blocked_ms: i64) -> Res<()> {
        // The version is the write time: between disconnecting and re-allowing, the latest wins.
        self.insert_rows(
            "connection_blocks",
            "subject, client, blocked_ms, version",
            &[json!({"subject": subject, "client": client, "blocked_ms": blocked_ms, "version": crate::app::now_ms()})],
        )
        .await
    }

    async fn blocks(&self) -> Res<Vec<(String, String, i64)>> {
        let rows = self
            .json(
                "SELECT subject, client, blocked_ms FROM connection_blocks FINAL WHERE blocked_ms > 0",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r["subject"].as_str().unwrap_or("").to_string(),
                    r["client"].as_str().unwrap_or("").to_string(),
                    i64_of(&r["blocked_ms"]),
                )
            })
            .collect())
    }

    async fn connections(&self) -> Res<Vec<ConnectionRow>> {
        let rows = self
            .json(
                "SELECT subject, client, first_ms, last_ms, agent FROM connections FINAL ORDER BY subject, client",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ConnectionRow {
                subject: r["subject"].as_str().unwrap_or("").to_string(),
                client: r["client"].as_str().unwrap_or("").to_string(),
                first_ms: i64_of(&r["first_ms"]),
                last_ms: i64_of(&r["last_ms"]),
                agent: r["agent"].as_str().unwrap_or("").to_string(),
                blocked_ms: 0,
            })
            .collect())
    }

    async fn identities(&self) -> Res<Vec<IdentityRow>> {
        let rows = self
            .json(
                "SELECT subject, email, name, last_ms FROM identities FINAL ORDER BY subject",
                &[],
            )
            .await?;
        Ok(rows.iter().map(identity_from).collect())
    }

    async fn create_session(&self, x: &SessionRow) -> Res<()> {
        self.insert_rows(
            SESSION_TABLE,
            SESSION_INSERT,
            &[session_row(x, x.created_ms as u64)],
        )
        .await
    }

    async fn session_by_hash(&self, hash: &str) -> Res<Option<SessionRow>> {
        let rows = self
            .json(
                &format!("SELECT {SESSION_COLS} FROM sessions FINAL WHERE hash = {{h:String}}"),
                &[("h", esc(hash))],
            )
            .await?;
        Ok(rows.first().map(session_from))
    }

    async fn list_sessions(&self, now: i64) -> Res<Vec<SessionRow>> {
        let rows = self.json(&format!("SELECT {SESSION_COLS} FROM sessions FINAL WHERE revoked = 0 AND expires_ms > {{n:Int64}} ORDER BY created_ms DESC"), &[("n", now.to_string())]).await?;
        Ok(rows.iter().map(session_from).collect())
    }

    async fn revoke_session(&self, id: &str) -> Res<bool> {
        let rows = self
            .json(
                &format!("SELECT {SESSION_COLS} FROM sessions FINAL WHERE id = {{i:String}}"),
                &[("i", esc(id))],
            )
            .await?;
        let Some(mut x) = rows.first().map(session_from).filter(|x| !x.revoked) else {
            return Ok(false);
        };
        x.revoked = true;
        let version = crate::app::now_ms().max(x.created_ms + 1) as u64;
        self.insert_rows(SESSION_TABLE, SESSION_INSERT, &[session_row(&x, version)])
            .await?;
        Ok(true)
    }

    async fn put_login(&self, l: &LoginRow) -> Res<()> {
        self.insert_rows("logins", "state_hash, verifier, nonce, return_to, expires_ms, used, version", &[json!({"state_hash": l.state_hash, "verifier": l.verifier, "nonce": l.nonce, "return_to": l.return_to, "expires_ms": l.expires_ms, "used": 0, "version": 1})]).await
    }

    async fn take_login(&self, state_hash: &str, now: i64) -> Res<Option<LoginRow>> {
        let rows = self.json("SELECT state_hash, verifier, nonce, return_to, expires_ms, used, version FROM logins FINAL WHERE state_hash = {h:String}", &[("h", esc(state_hash))]).await?;
        let Some(r) = rows
            .first()
            .filter(|r| i64_of(&r["used"]) == 0 && i64_of(&r["expires_ms"]) > now)
        else {
            return Ok(None);
        };
        let l = LoginRow {
            state_hash: state_hash.to_string(),
            verifier: r["verifier"].as_str().unwrap_or("").into(),
            nonce: r["nonce"].as_str().unwrap_or("").into(),
            return_to: r["return_to"].as_str().unwrap_or("").into(),
            expires_ms: i64_of(&r["expires_ms"]),
        };
        self.insert_rows("logins", "state_hash, verifier, nonce, return_to, expires_ms, used, version", &[json!({"state_hash": l.state_hash, "verifier": "", "nonce": "", "return_to": "", "expires_ms": l.expires_ms, "used": 1, "version": i64_of(&r["version"]) + 1})]).await?;
        Ok(Some(l))
    }

    async fn record_llm(&self, calls: &[LlmCall]) -> Res<()> {
        self.insert_meter("llm_calls", calls).await
    }

    async fn record_injection(&self, i: &Injection) -> Res<()> {
        self.insert_meter("injections", std::slice::from_ref(i))
            .await
    }

    async fn record_turn(&self, t: &Turn) -> Res<()> {
        self.insert_meter("turns", std::slice::from_ref(t)).await
    }

    async fn record_evals(&self, evals: &[Eval]) -> Res<()> {
        self.insert_meter("evals", evals).await
    }

    async fn session_window(&self, session: &str) -> Res<Window> {
        let rows: Vec<Injection> = self
            .typed(
                "SELECT * FROM injections WHERE session = {s:String} \
                 AND at >= (SELECT max(at) FROM injections WHERE session = {s:String} AND reset) ORDER BY at",
                &[("s", esc(session))],
            )
            .await?;
        Ok(crate::meter::window_of(&rows))
    }

    async fn last_turn_at(&self, session: &str) -> Res<i64> {
        let rows = self
            .json(
                "SELECT max(at) AS m FROM turns WHERE session = {s:String}",
                &[("s", esc(session))],
            )
            .await?;
        Ok(rows.first().map(|r| i64_of(&r["m"])).unwrap_or(0))
    }

    async fn injections_after(&self, session: &str, after_ms: i64) -> Res<Vec<Injection>> {
        self.typed(
            "SELECT * FROM injections WHERE session = {s:String} AND at > {a:Int64} ORDER BY at",
            &[("s", esc(session)), ("a", after_ms.to_string())],
        )
        .await
    }

    async fn meter_rows(&self, from_ms: i64, subject: Option<&str>) -> Res<MeterRows> {
        let filter = if subject.is_some() {
            " AND subject = {u:String}"
        } else {
            ""
        };
        let params = [
            ("f", from_ms.to_string()),
            ("u", esc(subject.unwrap_or(""))),
        ];
        let q = |table: &str| {
            format!("SELECT * FROM {table} WHERE at >= {{f:Int64}}{filter} ORDER BY at")
        };
        Ok(MeterRows {
            llm: self.typed(&q("llm_calls"), &params).await?,
            injections: self.typed(&q("injections"), &params).await?,
            turns: self.typed(&q("turns"), &params).await?,
            evals: self.typed(&q("evals"), &params).await?,
        })
    }

    async fn evals_since(&self, from_ms: i64) -> Res<Vec<Eval>> {
        self.typed(
            "SELECT * FROM evals WHERE at >= {f:Int64}",
            &[("f", from_ms.to_string())],
        )
        .await
    }

    async fn writer_status(&self) -> Res<Option<(String, i64)>> {
        let rows = self
            .json(
                "SELECT instance, until_ms FROM writer FINAL WHERE name = 'writer'",
                &[],
            )
            .await?;
        Ok(rows.first().map(|r| {
            (
                r["instance"].as_str().unwrap_or("").to_string(),
                i64_of(&r["until_ms"]),
            )
        }))
    }
}

const KEY_INSERT: &str =
    "id, hash, name, kind, subject, created_ms, revoked, expires_ms, created_by, version";

fn key_row(k: &KeyRow, version: u64) -> Value {
    json!({"id": k.id, "hash": k.hash, "name": k.name, "kind": k.kind, "subject": k.subject,
        "created_ms": k.created_ms, "revoked": k.revoked as u8, "expires_ms": k.expires_ms, "created_by": k.created_by, "version": version})
}

fn identity_from(r: &Value) -> IdentityRow {
    let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
    IdentityRow {
        subject: s("subject"),
        email: s("email"),
        name: s("name"),
        last_ms: i64_of(&r["last_ms"]),
    }
}

const SESSION_TABLE: &str = "sessions";
const SESSION_COLS: &str = "id, hash, subject, email, name, kind, created_ms, expires_ms, revoked";
const SESSION_INSERT: &str =
    "id, hash, subject, email, name, kind, created_ms, expires_ms, revoked, version";

fn session_row(x: &SessionRow, version: u64) -> Value {
    json!({"id": x.id, "hash": x.hash, "subject": x.subject, "email": x.email, "name": x.name, "kind": x.kind,
        "created_ms": x.created_ms, "expires_ms": x.expires_ms, "revoked": x.revoked as u8, "version": version})
}

fn session_from(r: &Value) -> SessionRow {
    let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
    SessionRow {
        id: s("id"),
        hash: s("hash"),
        subject: s("subject"),
        email: s("email"),
        name: s("name"),
        kind: s("kind"),
        created_ms: i64_of(&r["created_ms"]),
        expires_ms: i64_of(&r["expires_ms"]),
        revoked: i64_of(&r["revoked"]) != 0,
    }
}

fn key_from(r: &Value) -> KeyRow {
    KeyRow {
        id: r["id"].as_str().unwrap_or("").into(),
        hash: r["hash"].as_str().unwrap_or("").into(),
        name: r["name"].as_str().unwrap_or("").into(),
        kind: r["kind"].as_str().unwrap_or("").into(),
        subject: r["subject"].as_str().unwrap_or("").into(),
        created_ms: i64_of(&r["created_ms"]),
        revoked: i64_of(&r["revoked"]) != 0,
        expires_ms: i64_of(&r["expires_ms"]),
        created_by: r["created_by"].as_str().unwrap_or("").into(),
    }
}

#[async_trait]
impl RawStore for ClickHouse {
    async fn insert(&self, eps: &[Episode], revision: i64) -> Res<()> {
        if eps.is_empty() {
            return Ok(());
        }
        let mut body = String::new();
        for ep in eps {
            let row = serde_json::json!({
                "episode_id": ep.episode_id, "thread": ep.thread, "source": ep.source, "source_ref": ep.source_ref,
                "scope": ep.scope, "owner_id": ep.owner_id, "actor_id": ep.actor_id, "project_id": ep.project_id,
                "agent": ep.agent, "role": ep.role, "occurred_at": fmt_ts(ep.occurred_at), "text": ep.text,
                "entities": ep.entities, "revision": revision, "deleted": 0,
            });
            body.push_str(&row.to_string());
            body.push('\n');
        }
        self.run(
            "INSERT INTO episodes (episode_id, thread, source, source_ref, scope, owner_id, actor_id, project_id, agent, role, occurred_at, text, entities, revision, deleted) FORMAT JSONEachRow",
            &[],
            Some(body),
            true,
        )
        .await
        .map(|_| ())
    }

    async fn put_records(&self, rs: &[crate::records::Record]) -> Res<usize> {
        if rs.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = rs.iter().map(|r| r.id.clone()).collect();
        let have: std::collections::HashSet<String> = self
            .json(
                "SELECT DISTINCT id FROM records WHERE id IN {ids:Array(String)}",
                &[("ids", arr(&ids))],
            )
            .await?
            .iter()
            .filter_map(|r| r["id"].as_str().map(String::from))
            .collect();
        let keys: Vec<String> = rs
            .iter()
            .filter(|r| r.kind == "snapshot")
            .map(|r| r.key.clone())
            .collect();
        let mut last_hash: HashMap<(String, String), String> = HashMap::new();
        if !keys.is_empty() {
            for r in self
                .json(
                    "SELECT scope, key, argMax(hash, at) AS h FROM records FINAL WHERE kind = 'snapshot' AND key IN {k:Array(String)} GROUP BY scope, key",
                    &[("k", arr(&keys))],
                )
                .await?
            {
                let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
                last_hash.insert((s("scope"), s("key")), s("h"));
            }
        }
        let mut seen = std::collections::HashSet::new();
        let rows: Vec<Value> = rs
            .iter()
            .filter(|r| !have.contains(&r.id) && seen.insert(r.id.clone()))
            .filter(|r| {
                r.kind != "snapshot"
                    || last_hash.get(&(r.scope.clone(), r.key.clone())) != Some(&r.hash)
            })
            .map(|r| json!({"id": r.id, "scope": r.scope, "stream": r.stream, "key": r.key, "kind": r.kind, "state": r.state, "status": r.status, "at": fmt_ts(r.at), "title": r.title, "text": r.text, "attrs": r.attrs, "hash": r.hash}))
            .collect();
        let n = rows.len();
        self.insert_rows(
            "records",
            "id, scope, stream, key, kind, state, status, at, title, text, attrs, hash",
            &rows,
        )
        .await?;
        Ok(n)
    }

    async fn records(&self, q: &crate::records::Query) -> Res<Vec<crate::records::Record>> {
        let mut sql = String::from(
            "SELECT id, scope, stream, key, kind, state, status, toUnixTimestamp64Milli(at) AS at_ms, title, text, attrs, hash FROM records FINAL WHERE scope IN {s:Array(String)} AND key = {k:String}",
        );
        let mut params = vec![("s", arr(&q.scopes)), ("k", esc(&q.key))];
        if let Some(kind) = &q.kind {
            sql.push_str(" AND kind = {kind:String}");
            params.push(("kind", esc(kind)));
        }
        if let Some(f) = q.from {
            sql.push_str(" AND at >= fromUnixTimestamp64Milli({f:Int64})");
            params.push(("f", f.to_string()));
        }
        if let Some(u) = q.until {
            sql.push_str(" AND at <= fromUnixTimestamp64Milli({u:Int64})");
            params.push(("u", u.to_string()));
        }
        sql.push_str(" ORDER BY at DESC, id DESC LIMIT {n:UInt64}");
        params.push(("n", q.limit.max(1).to_string()));
        let rows = self.json(&sql, &params).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
                crate::records::Record {
                    id: s("id"),
                    scope: s("scope"),
                    stream: s("stream"),
                    key: s("key"),
                    kind: s("kind"),
                    state: s("state"),
                    status: s("status"),
                    at: i64_of(&r["at_ms"]),
                    title: s("title"),
                    text: s("text"),
                    attrs: serde_json::from_value(r["attrs"].clone()).unwrap_or_default(),
                    hash: s("hash"),
                }
            })
            .collect())
    }

    async fn subjects(&self) -> Res<Vec<crate::records::Subject>> {
        // Open = an `open` after the last `close` (`lc`, put on every row by the window function).
        let rows = self
            .json(
                "SELECT scope, key, argMax(attrs['thing'], at) AS thing, argMax(stream, at) AS stream, argMaxIf(title, at, title != '') AS title, \
                 countIf(kind = 'event') AS events, toUnixTimestamp64Milli(min(at)) AS first_ms, toUnixTimestamp64Milli(max(at)) AS last_ms, \
                 argMaxIf(status, at, kind = 'event') AS last_status, \
                 countIf(kind = 'event' AND state = 'open' AND at > lc) AS opens, \
                 toUnixTimestamp64Milli(minIf(at, kind = 'event' AND state = 'open' AND at > lc)) AS open_ms, \
                 countIf(kind = 'snapshot') AS snapshots, toUnixTimestamp64Milli(maxIf(at, kind = 'snapshot')) AS snap_ms \
                 FROM (SELECT *, maxIf(at, kind = 'event' AND state = 'close') OVER (PARTITION BY scope, key) AS lc FROM records FINAL) \
                 GROUP BY scope, key",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
                crate::records::Subject {
                    thing: s("thing"),
                    scope: s("scope"),
                    key: s("key"),
                    stream: s("stream"),
                    title: s("title"),
                    events: i64_of(&r["events"]) as u64,
                    first_at: i64_of(&r["first_ms"]),
                    last_at: i64_of(&r["last_ms"]),
                    last_status: s("last_status"),
                    open_since: (i64_of(&r["opens"]) > 0).then(|| i64_of(&r["open_ms"])),
                    snapshots: i64_of(&r["snapshots"]) as u64,
                    last_snapshot_at: if i64_of(&r["snapshots"]) > 0 {
                        i64_of(&r["snap_ms"])
                    } else {
                        0
                    },
                }
            })
            .collect())
    }

    async fn thread_after(
        &self,
        thread: &str,
        scope: &str,
        upto_ms: i64,
        upto_id: &str,
        limit: u32,
    ) -> Res<Vec<Episode>> {
        self.rows(
            &format!(
                "SELECT {COLS} FROM episodes FINAL WHERE thread = {{t:String}} AND scope = {{s:String}} AND deleted = 0 \
                 AND (toUnixTimestamp64Milli(occurred_at), episode_id) > ({{um:Int64}}, {{uid:String}}) \
                 ORDER BY occurred_at, episode_id LIMIT {{n:UInt32}}"
            ),
            &[("t", esc(thread)), ("s", esc(scope)), ("um", upto_ms.to_string()), ("uid", esc(upto_id)), ("n", limit.to_string())],
        )
        .await
    }

    async fn thread_before(
        &self,
        thread: &str,
        scope: &str,
        upto_ms: i64,
        upto_id: &str,
        limit: u32,
    ) -> Res<Vec<Episode>> {
        let mut v = self
            .rows(
                &format!(
                    "SELECT {COLS} FROM episodes FINAL WHERE thread = {{t:String}} AND scope = {{s:String}} AND deleted = 0 \
                     AND (toUnixTimestamp64Milli(occurred_at), episode_id) <= ({{um:Int64}}, {{uid:String}}) \
                     ORDER BY occurred_at DESC, episode_id DESC LIMIT {{n:UInt32}}"
                ),
                &[("t", esc(thread)), ("s", esc(scope)), ("um", upto_ms.to_string()), ("uid", esc(upto_id)), ("n", limit.to_string())],
            )
            .await?;
        v.reverse();
        Ok(v)
    }

    async fn thread_episode_ids(
        &self,
        thread: &str,
        scopes: &[String],
        since_ms: i64,
    ) -> Res<Vec<String>> {
        let rows = self
            .json(
                "SELECT DISTINCT episode_id FROM episodes FINAL WHERE thread = {t:String} AND scope IN {sc:Array(String)} \
                 AND deleted = 0 AND toUnixTimestamp64Milli(occurred_at) >= {f:Int64} LIMIT 20000",
                &[("t", esc(thread)), ("sc", arr(scopes)), ("f", since_ms.to_string())],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r["episode_id"].as_str().map(String::from))
            .collect())
    }

    async fn episode_with_context(
        &self,
        id: &str,
        scopes: &[String],
        around: u32,
    ) -> Res<Vec<Episode>> {
        let base = self
            .rows(
                &format!("SELECT {COLS} FROM episodes FINAL WHERE episode_id = {{id:String}} AND scope IN {{sc:Array(String)}} AND deleted = 0 LIMIT 1"),
                &[("id", esc(id)), ("sc", arr(scopes))],
            )
            .await?;
        let Some(ep) = base.into_iter().next() else {
            return Ok(Vec::new());
        };
        let mut before = self
            .thread_before(
                &ep.thread,
                &ep.scope,
                ep.occurred_at,
                &ep.episode_id,
                around + 1,
            )
            .await?;
        let after = self
            .thread_after(
                &ep.thread,
                &ep.scope,
                ep.occurred_at,
                &ep.episode_id,
                around,
            )
            .await?;
        before.extend(after);
        Ok(before)
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
        let terms: Vec<String> = terms.iter().take(64).cloned().collect();
        let mut cond = vec![
            "deleted = 0".to_string(),
            "scope IN {sc:Array(String)}".to_string(),
        ];
        let mut params: Vec<(&str, String)> = vec![("sc", arr(scopes)), ("n", limit.to_string())];
        let mut any = Vec::new();
        if !terms.is_empty() {
            any.push("multiSearchAny(text_norm, {terms:Array(String)})".to_string());
            params.push(("terms", arr(&terms)));
        }
        if !entities.is_empty() {
            any.push("hasAny(entities, {ents:Array(String)})".to_string());
            params.push(("ents", arr(entities)));
        }
        cond.push(format!("({})", any.join(" OR ")));
        if let Some(f) = from {
            cond.push("occurred_at >= fromUnixTimestamp64Milli({f:Int64})".into());
            params.push(("f", f.to_string()));
        }
        if let Some(u) = until {
            cond.push("occurred_at < fromUnixTimestamp64Milli({u:Int64})".into());
            params.push(("u", u.to_string()));
        }
        // Without terms there is no score: ClickHouse reads `ORDER BY 0` as a positional column.
        let order = if terms.is_empty() {
            "occurred_at DESC".to_string()
        } else {
            "(length(multiSearchAllPositions(text_norm, {terms:Array(String)})) - countEqual(multiSearchAllPositions(text_norm, {terms:Array(String)}), 0)) DESC, occurred_at DESC".to_string()
        };
        let sql = format!(
            "SELECT episode_id, thread, source, source_ref, scope, owner_id, actor_id, project_id, agent, role, \
             toUnixTimestamp64Milli(occurred_at) AS occurred_ms, substring(text, 1, 600) AS text, entities \
             FROM episodes FINAL WHERE {} ORDER BY {order} LIMIT {{n:UInt32}}",
            cond.join(" AND ")
        );
        self.rows(&sql, &params).await
    }

    async fn purge_owner(&self, owner: &str) -> Res<()> {
        let scope = format!("user:{owner}");
        self.delete(
            "DELETE FROM episodes WHERE owner_id = {o:String} OR actor_id = {o:String} OR scope = {s:String}",
            &[("o", esc(owner)), ("s", esc(&scope))],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_is_created_replicated_in_a_replicated_database() {
        let creates: Vec<&str> = SCHEMA
            .iter()
            .copied()
            .filter(|d| d.starts_with("CREATE TABLE"))
            .collect();
        assert!(creates.len() >= 18);
        for ddl in creates {
            let r = replicated_ddl(ddl);
            let engine = r.split("ENGINE = ").nth(1).unwrap();
            assert!(
                engine.starts_with("ReplicatedMergeTree")
                    || engine.starts_with("ReplicatedReplacingMergeTree"),
                "{r}"
            );
            assert_eq!(r.matches("ENGINE = ").count(), 1);
        }
        assert_eq!(
            replicated_ddl("ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS x Int64"),
            "ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS x Int64"
        );
    }

    async fn replicated_pair(test: &str) -> (ClickHouse, ClickHouse) {
        let urls = std::env::var("MNEMO_TEST_CH_REPLICAS")
            .expect("MNEMO_TEST_CH_REPLICAS not set: `make test-replicated` starts the replicas");
        let urls: Vec<&str> = urls.split(',').collect();
        assert_eq!(urls.len(), 2, "two replicas");
        let user = std::env::var("MNEMO_TEST_CH_USER").unwrap_or_else(|_| "mnemo".into());
        let pass = std::env::var("MNEMO_TEST_CH_PASSWORD").unwrap_or_else(|_| "mnemo".into());
        let db = format!("mnemo_rtest_{test}_{}", uuid::Uuid::new_v4().simple());
        let a = ClickHouse::new(urls[0], &db, &user, &pass)
            .unwrap()
            .require_replicated(true);
        let b = ClickHouse::new(urls[1], &db, &user, &pass)
            .unwrap()
            .require_replicated(true);
        for c in [&a, &b] {
            c.wait_ready(30).await.unwrap();
            c.create_replicated_database().await.unwrap();
        }
        Db::migrate(&a).await.unwrap();
        Db::migrate(&b).await.unwrap();
        assert!(a.is_replicated() && b.is_replicated());
        (a, b)
    }

    async fn drop_pair(a: ClickHouse, b: ClickHouse) {
        a.drop_database().await.unwrap();
        b.drop_database().await.unwrap();
    }

    impl ClickHouse {
        async fn system(&self, cmd: &str, table: &str) {
            self.exec(&format!("SYSTEM {cmd} {}.{table}", self.db), &[])
                .await
                .unwrap();
        }

        async fn count(&self, sql: &str) -> i64 {
            let r = self.json(sql, &[]).await.unwrap();
            i64_of(&r[0]["n"])
        }

        async fn wait_count(&self, sql: &str, n: i64) {
            for _ in 0..100 {
                if self.count(sql).await == n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("the replica never reached {n} with {sql}");
        }
    }

    fn insert(m: Memory) -> WritePlan {
        WritePlan {
            inserts: vec![m],
            ..Default::default()
        }
    }

    fn memory(id: &str) -> Memory {
        Memory {
            id: id.into(),
            scope: "user:ana".into(),
            what: format!("memory {id}"),
            known_at: 1,
            valid_from: 1,
            source_episodes: vec!["e1".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    #[ignore = "needs two replicas: make test-replicated"]
    async fn replicated_a_row_seen_late_on_this_replica_is_still_read() {
        let (a, b) = replicated_pair("late").await;
        b.system("STOP FETCHES", "log").await;
        let first = a.apply(&insert(memory("m1")), "", 1).await.unwrap();
        // The load balancer sends the next write to replica 2: same sequence, different path.
        let via_b = b.on(&b.url);
        *via_b.write.lock().await = first as u64 + 1;
        via_b.apply(&insert(memory("m2")), "", 1).await.unwrap();

        let reader = std::sync::Arc::new(b.on(&b.url));
        Db::migrate(&*reader).await.unwrap();
        let app = crate::app::App::new(
            reader.clone(),
            None,
            None,
            None,
            crate::judge::Judge::Heuristic,
            crate::app::Cfg::default(),
        );
        app.refresh().await.unwrap();
        {
            let g = app.globe.read().unwrap();
            assert!(g.memory("m2").is_some());
            assert!(g.memory("m1").is_none(), "replica 2 does not have m1 yet");
        }
        b.system("START FETCHES", "log").await;
        b.wait_count("SELECT count() AS n FROM log", 2).await;
        app.refresh().await.unwrap();
        assert!(
            app.globe.read().unwrap().memory("m1").is_some(),
            "m1 arrives late, but it arrives"
        );
        drop_pair(a, b).await;
    }

    #[tokio::test]
    #[ignore = "needs two replicas: make test-replicated"]
    async fn replicated_the_writer_builds_on_its_own_last_version() {
        let (mut a, b) = replicated_pair("own").await;
        a.apply(&insert(memory("m1")), "", 1).await.unwrap();
        b.wait_count("SELECT count() AS n FROM log", 1).await;
        b.system("STOP FETCHES", "log").await;
        let close = WritePlan {
            closes: vec![("m1".into(), 5, None)],
            ..Default::default()
        };
        a.apply(&close, "", 2).await.unwrap();
        // The writer's next request is answered by replica 2, which does not have the close.
        let a_url = std::mem::replace(&mut a.url, b.url.clone());
        let sources = WritePlan {
            add_sources: vec![("m1".into(), vec!["e2".into()])],
            ..Default::default()
        };
        a.apply(&sources, "", 3).await.unwrap();
        b.system("START FETCHES", "log").await;
        b.wait_count("SELECT count() AS n FROM log", 3).await;
        let last = b.latest("memory", &["m1".into()]).await.unwrap();
        let m = parse_memory(&last["m1"].0, vec![]);
        assert_eq!(m.valid_to, Some(5), "the close is still in place");
        assert_eq!(m.source_episodes, vec!["e1", "e2"]);
        a.url = a_url;
        drop_pair(a, b).await;
    }

    #[tokio::test]
    #[ignore = "needs two replicas: make test-replicated"]
    async fn replicated_a_new_writer_does_not_reuse_sequence_numbers() {
        let (a, b) = replicated_pair("seq").await;
        b.system("STOP FETCHES", "log").await;
        let mut last = 0;
        for i in 0..3 {
            last = a
                .apply(&insert(memory(&format!("m{i}"))), "", 1)
                .await
                .unwrap();
        }
        let next = b.on(&b.url);
        Db::migrate(&next).await.unwrap();
        assert!(
            *next.write.lock().await as i64 > last,
            "replica 2 does not see replica 1's rows, yet the sequence numbers do not repeat"
        );
        b.system("START FETCHES", "log").await;
        drop_pair(a, b).await;
    }

    #[tokio::test]
    #[ignore = "needs two replicas: make test-replicated"]
    async fn replicated_the_lease_is_not_claimed_from_a_lagging_replica() {
        let (a, b) = replicated_pair("lease").await;
        b.system("STOP FETCHES", "writer").await;
        let now = crate::app::now_ms();
        a.claim_writer("a", now).await.unwrap();
        let pending = "SELECT count() AS n FROM system.replication_queue WHERE database = currentDatabase() \
                       AND table = 'writer' AND type = 'GET_PART'";
        b.wait_count(pending, 1).await;
        assert!(
            b.claim_writer("b", now + 1_000).await.is_err(),
            "replica 2 does not see a's lease, but knows it is missing something"
        );
        b.system("START FETCHES", "writer").await;
        b.wait_count(pending, 0).await;
        assert!(
            b.claim_writer("b", now + 1_000).await.is_err(),
            "once caught up, it sees a's live lease"
        );
        drop_pair(a, b).await;
    }

    #[test]
    fn params_are_escaped() {
        assert_eq!(arr(&["a'b".into(), "c\\d".into()]), r"['a\'b','c\\d']");
        assert_eq!(esc("a\tb\nc"), r"a\tb\nc");
    }

    #[test]
    fn log_rows_round_trip_memories_entities_and_tombstones() {
        let m = Memory {
            id: "m1".into(),
            scope: "user:a".into(),
            what: "x".into(),
            known_at: 1,
            valid_from: 1,
            source_episodes: vec!["e".into()],
            embedding: Some(vec![0.5]),
            links: vec![crate::model::Link {
                entity: "repo:x".into(),
                role: crate::model::Role::Where,
                subrole: String::new(),
            }],
            ..Default::default()
        };
        let row = memory_row(7, &m, false);
        assert_eq!(row["entity_ids"], json!(["repo:x"]));
        assert_eq!(row["live"], 1);
        let back = parse_memory(row["body"].as_str().unwrap(), floats(&row["embedding"]));
        assert_eq!(back.id, "m1");
        assert_eq!(back.links.len(), 1);
        assert_eq!(
            back.embedding,
            Some(vec![0.5]),
            "the embedding travels in its own column, not in the body"
        );
        let t = tombstone(8, "memory", "m1");
        assert_eq!(parse_memory(t["body"].as_str().unwrap(), vec![]).id, "m1");
        assert_eq!(
            parse_entity(&json!({"id": "repo:x"}).to_string()).id,
            "repo:x"
        );
        assert_eq!(i64_of(&json!("42")), 42, "a 64-bit integer may come quoted");
    }
}
