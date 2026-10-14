//! Entities and six-W memories. Memory text is in English because a model reads it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Who,
    Where,
    About,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Who, Role::Where, Role::About];

    pub fn index(self) -> usize {
        self as usize
    }
}

/// `When` is not an entity role: it is the memory's timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum W {
    What,
    Who,
    When,
    Where,
    Why,
    How,
}

impl W {
    pub fn as_str(self) -> &'static str {
        match self {
            W::What => "what",
            W::Who => "who",
            W::When => "when",
            W::Where => "where",
            W::Why => "why",
            W::How => "how",
        }
    }

    pub fn parse(s: &str) -> Option<W> {
        match s {
            "what" => Some(W::What),
            "who" => Some(W::Who),
            "when" => Some(W::When),
            "where" => Some(W::Where),
            "why" => Some(W::Why),
            "how" => Some(W::How),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub id: String,
    /// Empty means the creator's default scope.
    #[serde(default)]
    pub scope: String,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub summary: String,
    /// Where it came from: `directory`, `repo`, `slack`, `infra`, `extracted`, `api`.
    #[serde(default)]
    pub source: String,
    /// Same entity under another id; this one still resolves to it. Identity is the name, not the
    /// kind: splitting "mnemo" the repo from "mnemo" the system would split their memories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub entity: String,
    pub role: Role,
    #[serde(default)]
    pub subrole: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub scope: String,
    pub owner_id: String,
    /// One of `KINDS`.
    pub kind: String,
    pub what: String,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub how: Option<String>,
    #[serde(default)]
    pub occurred_from: Option<i64>,
    #[serde(default)]
    pub occurred_to: Option<i64>,
    pub known_at: i64,
    pub valid_from: i64,
    #[serde(default)]
    pub valid_to: Option<i64>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    pub confidence: f32,
    pub source_episodes: Vec<String>,
    #[serde(default)]
    pub links: Vec<Link>,
    /// Who promoted it: `decisions`, `heuristic`, `llm`, `explicit`, or `keep` for a saved copy.
    #[serde(default)]
    pub judge: String,
    #[serde(default, skip_serializing)]
    pub embedding: Option<Vec<f32>>,
}

pub const KINDS: [&str; 6] = [
    "event",
    "fact",
    "state",
    "decision",
    "preference",
    "procedure",
];

impl Memory {
    /// Stored with `keep`: its text was written by a third party, not the person or distillation.
    pub fn is_saved_copy(&self) -> bool {
        self.judge == "keep"
    }

    pub fn is_live_at(&self, t: i64) -> bool {
        self.valid_from <= t && self.valid_to.is_none_or(|end| t < end)
    }

    /// Not ended yet. A closed memory has `valid_to` in the past; a state carries its planned end
    /// there and is live until then.
    pub fn is_live(&self) -> bool {
        self.valid_to.is_none_or(|end| crate::app::now_ms() < end)
    }

    /// Bits in the order of `W`.
    pub fn completeness(&self) -> u8 {
        let mut bits = 0u8;
        let has = |r: Role| self.links.iter().any(|l| l.role == r);
        if !self.what.trim().is_empty() {
            bits |= 1 << 0;
        }
        if has(Role::Who) {
            bits |= 1 << 1;
        }
        if self.occurred_from.is_some() {
            bits |= 1 << 2;
        }
        if has(Role::Where) || has(Role::About) {
            bits |= 1 << 3;
        }
        if self.why.as_deref().is_some_and(|s| !s.trim().is_empty()) {
            bits |= 1 << 4;
        }
        if self.how.as_deref().is_some_and(|s| !s.trim().is_empty()) {
            bits |= 1 << 5;
        }
        bits
    }

    pub fn answers(&self, w: W) -> bool {
        let bit = match w {
            W::What => 0,
            W::Who => 1,
            W::When => 2,
            W::Where => 3,
            W::Why => 4,
            W::How => 5,
        };
        self.completeness() & (1 << bit) != 0
    }

    pub fn anchor_time(&self) -> i64 {
        self.occurred_from.unwrap_or(self.known_at)
    }

    pub fn index_text(&self) -> String {
        let mut s = self.what.clone();
        for part in [&self.why, &self.how].into_iter().flatten() {
            s.push_str(". ");
            s.push_str(part);
        }
        s
    }
}

pub fn valid_scope(s: &str) -> bool {
    s.strip_prefix("user:")
        .is_some_and(|id| !id.is_empty() && !id.contains([':', ',']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completeness_marks_each_w() {
        let mut m = Memory {
            what: "x".into(),
            known_at: 1,
            valid_from: 1,
            ..Default::default()
        };
        assert!(m.answers(W::What));
        assert!(!m.answers(W::Why));
        m.why = Some("because".into());
        m.links.push(Link {
            entity: "e".into(),
            role: Role::Where,
            subrole: String::new(),
        });
        assert!(m.answers(W::Why));
        assert!(m.answers(W::Where));
        assert!(!m.answers(W::Who));
        m.why = Some("  ".into());
        assert!(!m.answers(W::Why), "a blank why does not count");
    }

    #[test]
    fn liveness_respects_the_validity_window() {
        let m = Memory {
            valid_from: 10,
            valid_to: Some(20),
            ..Default::default()
        };
        assert!(!m.is_live_at(9));
        assert!(m.is_live_at(10));
        assert!(!m.is_live_at(20));
        assert!(!m.is_live());
    }

    #[test]
    fn a_memory_with_a_future_end_is_live_until_then() {
        let now = crate::app::now_ms();
        let state = |end| Memory {
            kind: "state".into(),
            valid_from: now - 1_000,
            valid_to: end,
            ..Default::default()
        };
        assert!(state(Some(now + 60_000)).is_live());
        assert!(!state(Some(now - 1)).is_live());
        assert!(state(None).is_live());
    }

    #[test]
    fn entity_scope_is_optional_on_the_wire() {
        let e: Entity =
            serde_json::from_str(r#"{"id":"repo:x","kind":"repo","name":"x"}"#).unwrap();
        assert_eq!(e.scope, "");
    }
}
