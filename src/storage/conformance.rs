// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The storage contract, run against [`Mem`] and a real ClickHouse (`MNEMO_TEST_CH_URL`). Without
//! that variable the ClickHouse cases FAIL rather than skip, unless it is set to `skip`.

use super::ch::{ClickHouse, WRITER_LEASE_MS};
use super::mem::Mem;
use super::{ApplyError, Db, KeyRow, LoginRow, RawStore, SessionRow, Thread, WritePlan};
use crate::episode::Episode;
use crate::meter::{Eval, Injection, LlmCall, Turn};
use crate::model::{Entity, Link, Memory, Role};

trait Store: Db + RawStore {}
impl<T: Db + RawStore> Store for T {}

fn mem(id: &str, scope: &str, what: &str, links: &[(&str, Role)]) -> Memory {
    Memory {
        id: id.into(),
        scope: scope.into(),
        owner_id: "u1".into(),
        kind: "fact".into(),
        what: what.into(),
        known_at: 1,
        valid_from: 1,
        confidence: 0.9,
        source_episodes: vec!["e1".into()],
        links: links
            .iter()
            .map(|(e, r)| Link {
                entity: e.to_string(),
                role: *r,
                subrole: String::new(),
            })
            .collect(),
        judge: "t".into(),
        embedding: Some(vec![0.5, -0.25]),
        ..Default::default()
    }
}

async fn clickhouse(test: &str) -> Option<ClickHouse> {
    let url = std::env::var("MNEMO_TEST_CH_URL").expect(
        "MNEMO_TEST_CH_URL not set: `make test` starts ClickHouse (or set MNEMO_TEST_CH_URL=skip)",
    );
    if url == "skip" {
        return None;
    }
    let user = std::env::var("MNEMO_TEST_CH_USER").unwrap_or_else(|_| "mnemo".into());
    let pass = std::env::var("MNEMO_TEST_CH_PASSWORD").unwrap_or_else(|_| "mnemo".into());
    let db = format!("mnemo_test_{test}_{}", uuid::Uuid::new_v4().simple());
    let replicas = replicas();
    let ch = ClickHouse::new(&url, &db, &user, &pass)
        .unwrap()
        .require_replicated(!replicas.is_empty());
    ch.wait_ready(30).await.unwrap();
    for r in &replicas {
        ch.on(r).create_replicated_database().await.unwrap();
    }
    Db::migrate(&ch).await.unwrap();
    assert_eq!(ch.is_replicated(), !replicas.is_empty());
    Some(ch)
}

fn replicas() -> Vec<String> {
    std::env::var("MNEMO_TEST_CH_REPLICAS")
        .map(|v| v.split(',').map(String::from).collect())
        .unwrap_or_default()
}

async fn drop_db(ch: Option<ClickHouse>) {
    if let Some(ch) = ch {
        ch.drop_database().await.unwrap();
        for r in replicas() {
            ch.on(&r).drop_database().await.unwrap();
        }
    }
}

async fn revisions_follow_writes_and_round_trip_everything(db: &dyn Store) {
    Db::migrate(db).await.unwrap();
    let r1 = db
        .upsert_entities(
            &[Entity {
                id: "system:gw".into(),
                scope: "global".into(),
                kind: "system".into(),
                name: "gateway".into(),
                aliases: vec!["gw".into()],
                summary: "The gateway".into(),
                ..Default::default()
            }],
            1,
        )
        .await
        .unwrap();
    // Without a summary: the existing one must survive.
    db.upsert_entities(
        &[Entity {
            id: "system:gw".into(),
            scope: "global".into(),
            kind: "system".into(),
            name: "gateway".into(),
            aliases: vec!["gw".into(), "edge-gw".into()],
            ..Default::default()
        }],
        1,
    )
    .await
    .unwrap();
    let plan = WritePlan {
        inserts: vec![mem(
            "m1",
            "global",
            "The gateway LB cut at 120 s",
            &[("system:gw", Role::Where)],
        )],
        ..Default::default()
    };
    let r2 = db.apply(&plan, "emb", 2).await.unwrap();
    assert!(r2 > r1, "each write gets a higher revision");
    let ents = db.entities_since(0, &[], 10).await.unwrap();
    let last = &ents.last().unwrap().0;
    assert_eq!(last.aliases, vec!["gw", "edge-gw"]);
    assert_eq!(last.summary, "The gateway");
    let mems = db.memories_since(r1, &[], 10).await.unwrap();
    assert_eq!(mems.len(), 1);
    assert_eq!(mems[0].0.links[0].entity, "system:gw");
    assert_eq!(mems[0].0.embedding, Some(vec![0.5, -0.25]));
    let plan = WritePlan {
        add_sources: vec![("m1".into(), vec!["e2".into(), "e1".into()])],
        closes: vec![("m1".into(), 5, Some("m2".into()))],
        ..Default::default()
    };
    db.apply(&plan, "", 3).await.unwrap();
    let m = db.memories_since(r2, &[], 10).await.unwrap();
    let m = &m.last().unwrap().0;
    assert_eq!(m.source_episodes, vec!["e1", "e2"]);
    assert_eq!(m.valid_to, Some(5));
    assert_eq!(m.superseded_by.as_deref(), Some("m2"));
    assert_eq!(
        m.embedding,
        Some(vec![0.5, -0.25]),
        "a new version keeps the embedding"
    );
    db.set_summaries(&[("system:gw".into(), "Front door".into())])
        .await
        .unwrap();
    assert_eq!(
        db.entities_since(0, &[], 10)
            .await
            .unwrap()
            .last()
            .unwrap()
            .0
            .summary,
        "Front door"
    );
    let st = db.stats().await.unwrap();
    assert_eq!(
        (
            st["memories"].as_i64(),
            st["live"].as_i64(),
            st["entities"].as_i64()
        ),
        (Some(1), Some(0), Some(1))
    );
}

async fn rows_already_read_are_skipped_by_range(db: &dyn Store) {
    Db::migrate(db).await.unwrap();
    let mut seqs = Vec::new();
    for id in ["m1", "m2", "m3", "m4"] {
        let plan = WritePlan {
            inserts: vec![mem(id, "user:ana", "x", &[])],
            ..Default::default()
        };
        seqs.push(db.apply(&plan, "", 1).await.unwrap());
    }
    let ids = |rows: Vec<(Memory, bool, i64)>| -> Vec<String> {
        rows.into_iter().map(|r| r.0.id).collect()
    };
    let got = db
        .memories_since(0, &[(seqs[0], seqs[1]), (seqs[3], seqs[3])], 10)
        .await
        .unwrap();
    assert_eq!(ids(got), vec!["m3"]);
    let got = db.memories_since(0, &[(0, i64::MAX)], 10).await.unwrap();
    assert!(got.is_empty());
    assert_eq!(
        ids(db.memories_since(seqs[1], &[], 10).await.unwrap()),
        vec!["m3", "m4"]
    );
}

async fn forget_and_stale_judgement(db: &dyn Store) {
    db.apply(
        &WritePlan {
            inserts: vec![mem("m1", "global", "x", &[("system:gw", Role::About)])],
            ..Default::default()
        },
        "",
        1,
    )
    .await
    .unwrap();
    let stale = WritePlan {
        stale_check: Some(("global".into(), vec!["system:gw".into()], 0)),
        inserts: vec![mem("m9", "global", "y", &[])],
        ..Default::default()
    };
    assert_eq!(db.apply(&stale, "", 2).await, Err(ApplyError::Stale));
    let seen = db
        .memories_since(0, &[], 10)
        .await
        .unwrap()
        .last()
        .unwrap()
        .2;
    let fresh = WritePlan {
        stale_check: Some(("global".into(), vec!["system:gw".into()], seen)),
        ..Default::default()
    };
    assert!(
        db.apply(&fresh, "", 2).await.is_ok(),
        "with the globe up to date the judgement holds"
    );
    assert!(
        !db.memories_since(0, &[], 10)
            .await
            .unwrap()
            .iter()
            .any(|m| m.0.id == "m9"),
        "a stale judgement writes nothing"
    );
    assert!(
        !db.forget("m1", &["user:x".into()], "", 3).await.unwrap(),
        "it is not closed outside its scopes"
    );
    assert!(db.forget("m1", &["global".into()], "", 3).await.unwrap());
    assert!(
        !db.forget("m1", &["global".into()], "", 4).await.unwrap(),
        "already closed"
    );
}

async fn threads_are_claimed_once_and_advanced(db: &dyn Store) {
    let t = Thread {
        thread: "s1|user:a".into(),
        scope: "user:a".into(),
        owner_id: "a".into(),
        reader_scopes: vec!["global".into()],
        upto_ms: 0,
        upto_id: String::new(),
    };
    db.touch_thread(&t, 100, 3).await.unwrap();
    assert!(
        db.claim_threads(150, 100, 4, 1000)
            .await
            .unwrap()
            .is_empty(),
        "not settled yet"
    );
    let got = db.claim_threads(300, 100, 4, 1000).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].reader_scopes, vec!["global"]);
    assert!(
        db.claim_threads(300, 100, 4, 1000)
            .await
            .unwrap()
            .is_empty(),
        "claimed only once"
    );
    assert_eq!(
        db.claim_threads(1400, 100, 4, 1000).await.unwrap().len(),
        1,
        "once the lease expires, it can be claimed again"
    );
    db.apply(
        &WritePlan {
            thread_advance: Some(("s1|user:a".into(), 100, "e3".into(), 3)),
            ..Default::default()
        },
        "",
        1,
    )
    .await
    .unwrap();
    assert_eq!(db.stats().await.unwrap()["pending_threads"], 0);
    assert!(
        db.claim_threads(5000, 100, 4, 1000)
            .await
            .unwrap()
            .is_empty(),
        "advanced, nothing left"
    );
    db.touch_thread(&t, 200, 1).await.unwrap();
    let again = db.claim_threads(5000, 100, 4, 1000).await.unwrap();
    assert_eq!(
        (again[0].upto_ms, again[0].upto_id.as_str()),
        (100, "e3"),
        "it resumes where it left off"
    );
}

async fn raw_episodes_are_searchable_by_stem_and_scoped(db: &dyn Store) {
    let ep = |id: &str, scope: &str, t: i64, text: &str| Episode {
        episode_id: id.into(),
        thread: "s1".into(),
        source: "chat".into(),
        scope: scope.into(),
        owner_id: scope.trim_start_matches("user:").into(),
        occurred_at: t,
        text: text.into(),
        entities: vec!["repo:mnemo".into()],
        ..Default::default()
    };
    let e1 = ep(
        "e1",
        "user:a",
        1,
        "Las peticiones se cortaban a los 120 segundos",
    );
    db.insert(
        &[
            e1.clone(),
            ep("e2", "user:a", 2, "otra cosa"),
            ep("e3", "user:b", 3, "peticiones de b"),
        ],
        0,
    )
    .await
    .unwrap();
    // Re-inserting must not duplicate.
    db.insert(&[e1], 0).await.unwrap();
    let ids = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(
        ids(db
            .thread_episode_ids("s1", &["user:a".into()], 0)
            .await
            .unwrap()),
        vec!["e1", "e2"]
    );
    assert_eq!(
        db.thread_episode_ids("s1", &["user:a".into()], 2)
            .await
            .unwrap(),
        vec!["e2"],
        "from the instant, inclusive"
    );
    assert_eq!(
        db.thread_episode_ids("s1", &["user:b".into()], 0)
            .await
            .unwrap(),
        vec!["e3"],
        "a thread in another scope is not visible"
    );
    assert!(
        db.thread_episode_ids("s2", &["user:a".into()], 0)
            .await
            .unwrap()
            .is_empty()
    );
    let found = db
        .search(
            &crate::text::tokens("¿por qué se cortaban las peticiónes?"),
            &[],
            &["user:a".into()],
            None,
            None,
            5,
        )
        .await
        .unwrap();
    assert_eq!(
        found
            .iter()
            .map(|e| e.episode_id.as_str())
            .collect::<Vec<_>>(),
        vec!["e1"]
    );
    let by_ent = db
        .search(
            &[],
            &["repo:mnemo".into()],
            &["user:a".into()],
            None,
            None,
            5,
        )
        .await
        .unwrap();
    assert_eq!(by_ent.len(), 2);
    let dated = db
        .search(
            &[],
            &["repo:mnemo".into()],
            &["user:a".into()],
            Some(2),
            None,
            5,
        )
        .await
        .unwrap();
    assert_eq!(
        dated
            .iter()
            .map(|e| e.episode_id.as_str())
            .collect::<Vec<_>>(),
        vec!["e2"]
    );
    let ctx = db
        .episode_with_context("e2", &["user:a".into()], 1)
        .await
        .unwrap();
    assert_eq!(
        ctx.iter()
            .map(|e| e.episode_id.as_str())
            .collect::<Vec<_>>(),
        vec!["e1", "e2"]
    );
    assert_eq!(
        db.thread_after("s1", "user:a", 1, "e1", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        db.episode_with_context("e3", &["user:a".into()], 1)
            .await
            .unwrap()
            .is_empty(),
        "what belongs to another scope is not visible"
    );
    RawStore::purge_owner(db, "b").await.unwrap();
    assert!(
        db.search(
            &crate::text::tokens("peticiones"),
            &[],
            &["user:b".into()],
            None,
            None,
            5
        )
        .await
        .unwrap()
        .is_empty()
    );
}

async fn purge_removes_history_and_leaves_tombstones(db: &dyn Store) {
    db.upsert_entities(
        &[Entity {
            id: "person:b".into(),
            scope: "global".into(),
            kind: "person".into(),
            name: "B".into(),
            ..Default::default()
        }],
        1,
    )
    .await
    .unwrap();
    let mut mine = mem("mb", "user:b", "B likes tea", &[("person:b", Role::Who)]);
    mine.owner_id = "b".into();
    db.apply(
        &WritePlan {
            inserts: vec![mine, mem("mg", "global", "Shared", &[])],
            ..Default::default()
        },
        "",
        1,
    )
    .await
    .unwrap();
    db.forget("mb", &["user:b".into()], "", 2).await.unwrap();
    let (m, e) = Db::purge_owner(db, "b").await.unwrap();
    assert_eq!((m, e), (1, 1));
    let after = db.memories_since(0, &[], 100).await.unwrap();
    assert!(
        after
            .iter()
            .all(|(m, deleted, _)| m.id != "mb" || (*deleted && m.what.is_empty())),
        "no version keeps the text"
    );
    assert!(
        after.iter().any(|(m, deleted, _)| m.id == "mb" && *deleted),
        "replicas see the tombstone"
    );
    assert!(
        after.iter().any(|(m, _, _)| m.id == "mg"),
        "what belongs to others is untouched"
    );
    assert!(
        db.entities_since(0, &[], 100)
            .await
            .unwrap()
            .iter()
            .any(|(e, d, _)| e.id == "person:b" && *d)
    );
}

async fn keys_and_cursors(db: &dyn Store) {
    let k = KeyRow {
        id: "k1".into(),
        hash: "h".into(),
        name: "laptop".into(),
        kind: "user".into(),
        subject: "ana".into(),
        created_ms: 1,
        revoked: false,
        expires_ms: 500,
        created_by: "admin".into(),
    };
    db.create_key(&k).await.unwrap();
    assert_eq!(db.key_by_hash("h").await.unwrap(), Some(k.clone()));
    assert!(k.active(499) && !k.active(500), "it expires at its instant");
    assert!(db.revoke_key("k1").await.unwrap());
    assert!(!db.revoke_key("k1").await.unwrap(), "already revoked");
    assert!(db.key_by_hash("h").await.unwrap().unwrap().revoked);
    assert_eq!(db.list_keys().await.unwrap().len(), 1);
    db.set_cursor("c", 7).await.unwrap();
    db.set_cursor("c", 3).await.unwrap();
    assert_eq!(
        db.cursor("c").await.unwrap(),
        7,
        "a cursor only moves forward"
    );
    assert_eq!(db.cursor("other").await.unwrap(), 0);
}

async fn metering_round_trips_and_windows_reset(db: &dyn Store) {
    let inj = |id: &str, at: i64, mems: &[&str], tokens: u64, reset: bool| Injection {
        id: id.into(),
        at,
        surface: "prompt".into(),
        subject: "ana".into(),
        session: "ana/s1".into(),
        memory_ids: mems.iter().map(|m| m.to_string()).collect(),
        entity_ids: vec!["repo:x".into()],
        tokens,
        reset,
        ..Default::default()
    };
    db.record_injection(&inj("i1", 10, &["m1", "m2"], 100, true))
        .await
        .unwrap();
    db.record_injection(&inj("i2", 20, &["m3"], 50, false))
        .await
        .unwrap();
    let w = db.session_window("ana/s1").await.unwrap();
    assert_eq!(
        (w.memory_ids.clone(), w.tokens),
        (vec!["m1".to_string(), "m2".into(), "m3".into()], 150)
    );
    assert_eq!(w.entity_ids, vec!["repo:x"]);
    assert_eq!(w.since, 10, "the live context starts at the reset");
    db.record_injection(&inj("i3", 30, &["m9"], 10, true))
        .await
        .unwrap();
    let w = db.session_window("ana/s1").await.unwrap();
    assert_eq!(
        (w.memory_ids, w.tokens, w.since),
        (vec!["m9".to_string()], 10, 30),
        "a reset empties the window"
    );
    assert_eq!(db.session_window("ana/other").await.unwrap().tokens, 0);
    assert_eq!(db.last_turn_at("ana/s1").await.unwrap(), 0);
    db.record_turn(&Turn {
        at: 25,
        subject: "ana".into(),
        session: "ana/s1".into(),
        model: "m".into(),
        input_tokens: 9,
        calls: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(db.last_turn_at("ana/s1").await.unwrap(), 25);
    let after: Vec<String> = db
        .injections_after("ana/s1", 25)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(after, vec!["i3"]);
    db.record_llm(&[LlmCall {
        at: 40,
        kind: "extract".into(),
        model: "m".into(),
        subject: "ana".into(),
        prompt_tokens: 1000,
        completion_tokens: 10,
        ok: true,
        ..Default::default()
    }])
    .await
    .unwrap();
    db.record_evals(&[Eval {
        at: 50,
        injection_id: "i1".into(),
        memory_id: "m1".into(),
        session: "ana/s1".into(),
        subject: "ana".into(),
        used: true,
        method: "cited".into(),
        p: 1.0,
    }])
    .await
    .unwrap();
    let rows = db.meter_rows(0, Some("ana")).await.unwrap();
    assert_eq!(
        (
            rows.injections.len(),
            rows.turns.len(),
            rows.llm.len(),
            rows.evals.len()
        ),
        (3, 1, 1, 1)
    );
    assert_eq!(
        rows.injections[0],
        inj("i1", 10, &["m1", "m2"], 100, true),
        "an injection comes back whole"
    );
    assert!(rows.evals[0].used);
    assert_eq!(
        db.meter_rows(0, Some("bob"))
            .await
            .unwrap()
            .injections
            .len(),
        0,
        "each subject sees only their own"
    );
    assert_eq!(db.meter_rows(45, None).await.unwrap().evals.len(), 1);
    assert_eq!(db.evals_since(60).await.unwrap().len(), 0);
    Db::purge_owner(db, "ana").await.unwrap();
    assert_eq!(
        db.meter_rows(0, None).await.unwrap().injections.len(),
        0,
        "metering keeps its queries: it is purged with everything else"
    );
}

async fn sessions_and_logins(db: &dyn Store) {
    let x = SessionRow {
        id: "s1".into(),
        hash: "h1".into(),
        subject: "ana@x.com".into(),
        email: "ana@x.com".into(),
        name: "Ana".into(),
        kind: "user".into(),
        created_ms: 1,
        expires_ms: 1000,
        revoked: false,
    };
    db.create_session(&x).await.unwrap();
    assert_eq!(db.session_by_hash("h1").await.unwrap(), Some(x.clone()));
    assert_eq!(db.list_sessions(500).await.unwrap().len(), 1);
    assert_eq!(
        db.list_sessions(2000).await.unwrap().len(),
        0,
        "an expired session is not listed"
    );
    assert!(db.revoke_session("s1").await.unwrap());
    assert!(!db.revoke_session("s1").await.unwrap());
    assert!(db.session_by_hash("h1").await.unwrap().unwrap().revoked);
    // Real timestamps: the logins table has a TTL and ClickHouse drops expired rows on the fly.
    let now = crate::app::now_ms();
    let l = LoginRow {
        state_hash: "st".into(),
        verifier: "v".into(),
        nonce: "n".into(),
        return_to: "/".into(),
        expires_ms: now + 600_000,
    };
    db.put_login(&l).await.unwrap();
    assert_eq!(
        db.take_login("st", now + 700_000).await.unwrap(),
        None,
        "expired"
    );
    db.put_login(&LoginRow {
        state_hash: "st2".into(),
        ..l.clone()
    })
    .await
    .unwrap();
    assert_eq!(
        db.take_login("st2", now).await.unwrap().map(|l| l.verifier),
        Some("v".into())
    );
    assert_eq!(
        db.take_login("st2", now).await.unwrap(),
        None,
        "a state works only once"
    );
    db.touch_key("k1", 10).await.unwrap();
    db.touch_key("k1", 20).await.unwrap();
    assert_eq!(db.keys_last_used().await.unwrap().get("k1"), Some(&20));
}

async fn records_of_things_keep_events_and_versions(db: &dyn Store) {
    use crate::records::{Query, Record, prepare};
    let ev = |at: i64, state: &str, status: &str| {
        prepare(
            Record {
                stream: "slack:#oncall".into(),
                key: "gw-latency".into(),
                kind: "event".into(),
                state: state.into(),
                status: status.into(),
                at,
                title: "Gateway p99".into(),
                text: format!("p99 at {at}"),
                ..Default::default()
            },
            "global",
            1,
        )
        .unwrap()
    };
    let page = |at: i64, text: &str| {
        prepare(
            Record {
                stream: "web".into(),
                key: "https://example.com/docs/".into(),
                kind: "snapshot".into(),
                at,
                title: "Docs".into(),
                text: text.into(),
                ..Default::default()
            },
            "global",
            1,
        )
        .unwrap()
    };
    let batch = vec![
        ev(10_000, "open", "firing"),
        ev(20_000, "open", "firing"),
        ev(30_000, "close", "resolved"),
        ev(40_000, "open", "firing"),
    ];
    assert_eq!(db.put_records(&batch).await.unwrap(), 4);
    assert_eq!(
        db.put_records(&batch).await.unwrap(),
        0,
        "the same message twice does not duplicate"
    );
    assert_eq!(db.put_records(&[page(1_000, "v1")]).await.unwrap(), 1);
    assert_eq!(
        db.put_records(&[page(2_000, "v1")]).await.unwrap(),
        0,
        "re-read without changes"
    );
    assert_eq!(
        db.put_records(&[page(3_000, "v2")]).await.unwrap(),
        1,
        "changed: new version"
    );
    let closed = |at: i64, state: &str, status: &str| {
        prepare(
            Record {
                stream: "slack:#oncall".into(),
                key: "queue-depth".into(),
                kind: "event".into(),
                state: state.into(),
                status: status.into(),
                at,
                title: "Queue depth".into(),
                ..Default::default()
            },
            "global",
            1,
        )
        .unwrap()
    };
    let resolved = [
        closed(5_000, "open", "firing"),
        closed(50_000, "close", "resolved"),
    ];
    assert_eq!(db.put_records(&resolved).await.unwrap(), 2);
    let mut subs = db.subjects().await.unwrap();
    subs.sort_by(|a, b| a.key.cmp(&b.key));
    let web = &subs[0];
    assert_eq!(
        (
            web.key.as_str(),
            web.snapshots,
            web.last_snapshot_at,
            web.events
        ),
        ("example.com/docs", 2, 3_000, 0)
    );
    let gw = &subs[1];
    assert_eq!(
        (
            gw.events,
            gw.open_since,
            gw.last_status.as_str(),
            gw.first_at,
            gw.last_at
        ),
        (4, Some(40_000), "firing", 10_000, 40_000)
    );
    assert_eq!(gw.title, "Gateway p99");
    let queue = &subs[2];
    assert_eq!(
        (
            queue.key.as_str(),
            queue.open_since,
            queue.last_status.as_str()
        ),
        ("queue-depth", None, "resolved"),
        "closed means no longer open"
    );
    let q = |kind: Option<&str>, from: Option<i64>| Query {
        scopes: vec!["global".into()],
        key: "gw-latency".into(),
        kind: kind.map(String::from),
        from,
        until: None,
        limit: 10,
    };
    let all = db.records(&q(None, None)).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.at).collect::<Vec<_>>(),
        vec![40_000, 30_000, 20_000, 10_000],
        "most recent first"
    );
    assert_eq!(
        db.records(&q(Some("event"), Some(25_000)))
            .await
            .unwrap()
            .len(),
        2
    );
    let mut other = q(None, None);
    other.scopes = vec!["user:ana".into()];
    assert!(
        db.records(&other).await.unwrap().is_empty(),
        "another scope does not see it"
    );
    let docs = db
        .records(&Query {
            scopes: vec!["global".into()],
            key: "example.com/docs".into(),
            kind: Some("snapshot".into()),
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(docs[0].text, "v2");
    assert_eq!(
        docs[0].attrs.get("source_key").map(String::as_str),
        Some("https://example.com/docs/")
    );
}

macro_rules! suite {
    ($($name:ident),*) => {
        mod in_memory {
            $(#[tokio::test] async fn $name() { super::$name(&super::Mem::new()).await })*
        }
        mod clickhouse {
            $(#[tokio::test] async fn $name() {
                let ch = super::clickhouse(stringify!($name)).await;
                if let Some(db) = &ch { super::$name(db).await }
                super::drop_db(ch).await;
            })*
        }
    };
}

suite!(
    revisions_follow_writes_and_round_trip_everything,
    rows_already_read_are_skipped_by_range,
    forget_and_stale_judgement,
    threads_are_claimed_once_and_advanced,
    raw_episodes_are_searchable_by_stem_and_scoped,
    purge_removes_history_and_leaves_tombstones,
    keys_and_cursors,
    metering_round_trips_and_windows_reset,
    sessions_and_logins,
    records_of_things_keep_events_and_versions
);

#[tokio::test]
async fn only_one_writer_at_a_time() {
    let Some(ch) = clickhouse("writer").await else {
        return;
    };
    ch.claim_writer("a", 1000).await.unwrap();
    assert!(
        ch.claim_writer("b", 2000).await.is_err(),
        "another live writer blocks"
    );
    ch.claim_writer("a", 2000).await.unwrap();
    assert!(
        ch.claim_writer("b", 2000 + WRITER_LEASE_MS - 1)
            .await
            .is_err(),
        "it lasts the whole lease"
    );
    let t = 2000 + WRITER_LEASE_MS + 1;
    let r = ch.claim_writer("b", t).await;
    assert!(r.is_ok(), "a lease without a heartbeat expires: {r:?}");
    ch.release_writer("b", t + 1000).await.unwrap();
    assert!(
        ch.claim_writer("c", t + 1001).await.is_ok(),
        "once released, another one gets in"
    );
    drop_db(Some(ch)).await;
}
