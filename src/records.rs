// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Things with a stable key (an alert, a page, a job): events are never merged, snapshots are
//! stored only when the content changed. No model is involved.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const KINDS: [&str; 2] = ["event", "snapshot"];
pub const STATES: [&str; 3] = ["open", "close", "point"];
pub const MAX_TEXT: usize = 200_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Record {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub scope: String,
    /// Where it comes from: `slack:#oncall`, `web`, `github:org/repo`, `nomad:prod`.
    pub stream: String,
    pub key: String,
    /// event | snapshot
    pub kind: String,
    /// Events only: open | close | point.
    #[serde(default)]
    pub state: String,
    /// Free-form, from the source: firing, resolved, 200, merged…
    #[serde(default)]
    pub status: String,
    /// Milliseconds since the epoch.
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attrs: BTreeMap<String, String>,
    /// sha256 of the text: a snapshot equal to the last one is not a new version.
    #[serde(default)]
    pub hash: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Subject {
    /// Entity id.
    pub thing: String,
    pub scope: String,
    pub key: String,
    pub stream: String,
    pub title: String,
    pub events: u64,
    pub first_at: i64,
    pub last_at: i64,
    pub last_status: String,
    /// The first `open` after the last `close`, if still open.
    pub open_since: Option<i64>,
    pub snapshots: u64,
    pub last_snapshot_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub scopes: Vec<String>,
    pub key: String,
    pub kind: Option<String>,
    pub from: Option<i64>,
    pub until: Option<i64>,
    pub limit: u32,
}

fn sha(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    h.finalize()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The key is normalised so that `https://x.io/a/` and `x.io/a` are the same page.
pub fn thing_id(kind: &str, key: &str, scope: &str) -> String {
    crate::distill::entity_id(kind, &normalize_key(key), scope)
}

pub fn normalize_key(key: &str) -> String {
    let k = key.trim();
    let k = k
        .strip_prefix("https://")
        .or_else(|| k.strip_prefix("http://"))
        .unwrap_or(k);
    let k = k.strip_prefix("www.").unwrap_or(k);
    k.trim_end_matches('/').chars().take(200).collect()
}

/// `scope` must already be authorised. The id is deterministic: the same message read twice is the
/// same record.
pub fn prepare(mut r: Record, scope: &str, now: i64) -> Result<Record, String> {
    r.stream = r.stream.trim().to_string();
    r.key = r.key.trim().to_string();
    if r.stream.is_empty() || r.key.is_empty() {
        return Err("a record needs a stream and a key".into());
    }
    if !KINDS.contains(&r.kind.as_str()) {
        return Err(format!("invalid kind: {} (event or snapshot)", r.kind));
    }
    if r.kind == "event" {
        if r.state.is_empty() {
            r.state = "point".into();
        }
        if !STATES.contains(&r.state.as_str()) {
            return Err(format!("invalid state: {} (open, close or point)", r.state));
        }
    } else {
        r.state.clear();
        if r.text.trim().is_empty() {
            return Err("a snapshot without text stores nothing".into());
        }
    }
    r.scope = scope.to_string();
    if r.at <= 0 {
        r.at = now;
    }
    r.text = r.text.chars().take(MAX_TEXT).collect();
    r.attrs = std::mem::take(&mut r.attrs)
        .into_iter()
        .filter(|(k, _)| !k.is_empty() && k.len() <= 64)
        .take(32)
        .map(|(k, v)| (k, v.chars().take(1000).collect()))
        .collect();
    r.title = r.title.trim().chars().take(300).collect();
    r.hash = sha(&[&r.text]);
    let key = normalize_key(&r.key);
    if key != r.key {
        r.attrs.insert("source_key".into(), r.key.clone());
    }
    r.key = key.clone();
    r.id = match (r.id.trim(), r.kind.as_str()) {
        // A source id (a Slack message ts) wins: whenever it arrives, it is the same record.
        (ext, _) if !ext.is_empty() => sha(&[scope, &r.stream, &key, ext]),
        (_, "event") => sha(&[scope, &r.stream, &key, &r.at.to_string(), &r.text]),
        _ => sha(&[scope, &r.stream, &key, &r.hash]),
    };
    Ok(r)
}

/// Must give the same result as the ClickHouse query: the in-memory test store relies on it.
#[cfg(test)]
pub fn subjects_of(records: &[Record]) -> Vec<Subject> {
    let mut by: BTreeMap<(String, String), Vec<&Record>> = BTreeMap::new();
    for r in records {
        by.entry((r.scope.clone(), normalize_key(&r.key)))
            .or_default()
            .push(r);
    }
    by.into_iter()
        .map(|((scope, key), mut rs)| {
            rs.sort_by_key(|r| (r.at, r.id.clone()));
            let events: Vec<&&Record> = rs.iter().filter(|r| r.kind == "event").collect();
            let snaps: Vec<&&Record> = rs.iter().filter(|r| r.kind == "snapshot").collect();
            let last_close = events
                .iter()
                .filter(|r| r.state == "close")
                .map(|r| r.at)
                .max();
            let open_since = events
                .iter()
                .filter(|r| r.state == "open" && last_close.is_none_or(|c| r.at > c))
                .map(|r| r.at)
                .min();
            let last = rs.last().unwrap();
            Subject {
                thing: rs
                    .iter()
                    .rev()
                    .find_map(|r| r.attrs.get("thing").cloned())
                    .unwrap_or_default(),
                scope,
                key,
                stream: last.stream.clone(),
                title: rs
                    .iter()
                    .rev()
                    .find(|r| !r.title.is_empty())
                    .map(|r| r.title.clone())
                    .unwrap_or_default(),
                events: events.len() as u64,
                first_at: rs.first().unwrap().at,
                last_at: last.at,
                last_status: events.last().map(|r| r.status.clone()).unwrap_or_default(),
                open_since,
                snapshots: snaps.len() as u64,
                last_snapshot_at: snaps.last().map(|r| r.at).unwrap_or(0),
            }
        })
        .collect()
}

pub fn status_line(s: &Subject) -> String {
    let day = crate::episode::fmt_ts;
    let mut out = String::new();
    if s.events > 0 {
        out.push_str(&format!(
            "{} events from {} to {}",
            s.events,
            day(s.first_at),
            day(s.last_at)
        ));
        match s.open_since {
            Some(t) => out.push_str(&format!("; OPEN since {}", day(t))),
            None if !s.last_status.is_empty() => {
                out.push_str(&format!("; last status {}", s.last_status))
            }
            None => {}
        }
    }
    if s.snapshots > 0 {
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(&format!(
            "{} saved version(s), latest {}",
            s.snapshots,
            day(s.last_snapshot_at)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: &str, at: i64, state: &str, text: &str) -> Record {
        prepare(
            Record {
                stream: "slack:#oncall".into(),
                key: key.into(),
                kind: "event".into(),
                state: state.into(),
                status: if state == "close" {
                    "resolved"
                } else {
                    "firing"
                }
                .into(),
                at,
                text: text.into(),
                ..Default::default()
            },
            "global",
            1,
        )
        .unwrap()
    }

    #[test]
    fn the_same_message_is_the_same_record_and_a_repeat_is_another() {
        let a = ev("gw-latency", 1000, "open", "p99 > 2s");
        assert_eq!(
            a.id,
            ev("gw-latency", 1000, "open", "p99 > 2s").id,
            "read twice"
        );
        assert_ne!(
            a.id,
            ev("gw-latency", 2000, "open", "p99 > 2s").id,
            "fires again"
        );
        let mut ext = a.clone();
        ext.id = "1712.0001".into();
        let ext = prepare(ext, "global", 1).unwrap();
        let mut later = ext.clone();
        later.id = "1712.0001".into();
        later.at = 9999;
        assert_eq!(
            ext.id,
            prepare(later, "global", 1).unwrap().id,
            "the source id wins"
        );
    }

    #[test]
    fn a_web_page_is_one_thing_whatever_its_spelling() {
        assert_eq!(
            thing_id("web", "https://www.example.com/docs/", "global"),
            thing_id("web", "example.com/docs", "global")
        );
        let page = |text: &str| {
            prepare(
                Record {
                    stream: "web".into(),
                    key: "https://example.com/docs".into(),
                    kind: "snapshot".into(),
                    text: text.into(),
                    ..Default::default()
                },
                "global",
                1,
            )
            .unwrap()
        };
        assert_eq!(
            page("v1").id,
            page("v1").id,
            "rereading unchanged is not a new version"
        );
        assert_ne!(page("v1").id, page("v2").id);
        assert!(
            prepare(
                Record {
                    stream: "web".into(),
                    key: "x".into(),
                    kind: "snapshot".into(),
                    ..Default::default()
                },
                "global",
                1
            )
            .is_err()
        );
        assert!(
            prepare(
                Record {
                    stream: "s".into(),
                    key: "x".into(),
                    kind: "event".into(),
                    state: "maybe".into(),
                    ..Default::default()
                },
                "global",
                1
            )
            .is_err()
        );
    }

    #[test]
    fn free_attributes_are_bounded() {
        let r = Record {
            stream: "web".into(),
            key: "x".into(),
            kind: "event".into(),
            attrs: (0..100)
                .map(|i| (format!("k{i}"), "x".repeat(5000)))
                .collect(),
            ..Default::default()
        };
        let r = prepare(r, "global", 1).unwrap();
        assert!(r.attrs.len() <= 32 && r.attrs.values().all(|v| v.len() <= 1000));
    }
}
