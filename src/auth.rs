// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Who is calling and what they can see. Every memory belongs to one person (`user:<subject>`);
//! only a `service` (a system that already authorizes its users) may act for another person, and
//! admins read only their own memory.

use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    Admin,
    Service,
    #[default]
    User,
}

/// CSRF protection and self-service key management depend on it.
#[derive(Clone, Debug, PartialEq)]
pub enum Via {
    AdminKey,
    Key(String),
    Session(String),
    /// With the token's `sub`.
    OAuth(String),
    /// A `service` key acting as a person (`X-Mnemo-Act-As`), with the key's id: only that
    /// person's memory, no account management.
    Delegated(String),
}

impl Default for Via {
    fn default() -> Via {
        Via::Key(String::new())
    }
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "admin" => Some(Kind::Admin),
            "service" => Some(Kind::Service),
            "user" => Some(Kind::User),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Admin => "admin",
            Kind::Service => "service",
            Kind::User => "user",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Principal {
    pub kind: Kind,
    pub subject: String,
    pub via: Via,
}

impl Principal {
    /// May act on behalf of another person.
    pub fn trusted(&self) -> bool {
        self.kind == Kind::Service
    }

    pub fn own_scope(&self) -> String {
        format!("user:{}", self.subject)
    }

    pub fn own_scopes(&self) -> Vec<String> {
        vec![self.own_scope()]
    }

    pub fn read_scopes(&self, requested: &[String]) -> Vec<String> {
        if requested.is_empty() {
            return self.own_scopes();
        }
        let own = self.own_scope();
        requested
            .iter()
            .filter(|s| crate::model::valid_scope(s) && (self.trusted() || **s == own))
            .cloned()
            .collect()
    }

    pub fn can_write(&self, scope: &str) -> bool {
        crate::model::valid_scope(scope) && (self.trusted() || scope == self.own_scope())
    }

    pub fn default_scope(&self) -> String {
        self.own_scope()
    }

    pub fn writable_of(&self, requested: &[String]) -> Vec<String> {
        let base = if requested.is_empty() {
            self.own_scopes()
        } else {
            requested.to_vec()
        };
        base.into_iter().filter(|s| self.can_write(s)).collect()
    }

    pub fn owner(&self, requested: &str) -> String {
        if self.trusted() && !requested.is_empty() {
            requested.to_string()
        } else {
            self.subject.clone()
        }
    }

    /// Per-user namespace, so two people don't overwrite each other's mark for the same source.
    pub fn cursor_name(&self, name: &str) -> String {
        if self.trusted() {
            name.to_string()
        } else {
            format!("{}/{}", self.subject, name)
        }
    }
}

pub fn hash(key: &str) -> String {
    let d = Sha256::digest(key.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_key() -> String {
    format!(
        "mnemo_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Constant-time: a key must not leak through how long a mismatch takes.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: Kind, subject: &str) -> Principal {
        Principal {
            kind,
            subject: subject.into(),
            ..Default::default()
        }
    }

    #[test]
    fn everyone_sees_and_writes_only_their_own_admins_too() {
        for k in [Kind::User, Kind::Admin] {
            let u = p(k, "ana@x");
            assert_eq!(u.read_scopes(&[]), vec!["user:ana@x"]);
            assert_eq!(
                u.read_scopes(&["user:ana@x".into(), "user:bea@x".into(), "global".into()]),
                vec!["user:ana@x"],
                "someone else's scope is ignored, and global no longer exists"
            );
            assert!(u.can_write("user:ana@x"));
            assert!(
                !u.can_write("user:bea@x") && !u.can_write("global") && !u.can_write("team:plat")
            );
            assert_eq!(u.default_scope(), "user:ana@x");
            assert_eq!(u.owner("someone-else"), "ana@x");
            assert!(!u.trusted(), "not even an admin acts for someone else");
        }
    }

    #[test]
    fn a_service_acts_for_a_person_but_only_in_personal_scopes() {
        let s = p(Kind::Service, "plane");
        assert_eq!(
            s.read_scopes(&[
                "user:ana".into(),
                "global".into(),
                "team:x".into(),
                "bogus".into()
            ]),
            vec!["user:ana"]
        );
        assert!(s.can_write("user:ana") && !s.can_write("global"));
        assert_eq!(s.owner("ana"), "ana");
    }

    #[test]
    fn keys_hash_stably_and_compare_in_constant_time() {
        let k = new_key();
        assert!(k.starts_with("mnemo_") && k.len() == 70);
        assert_eq!(hash(&k), hash(&k));
        assert_ne!(hash(&k), hash(&new_key()));
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"ab", b"abc"));
    }
}
