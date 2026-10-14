//! Hard facts, to veto merging two memories. An embedding says they talk about the same thing,
//! not that they say it: "the LB cuts at 120 s" vs "at 600 s" scores 0.947, above many paraphrases
//! (0.88-0.96). The vector proposes; this vetoes.

use std::collections::BTreeSet;

const NEGATIONS: [&str; 15] = [
    "not", "no", "never", "none", "without", "cannot", "nor", "neither", "nunca", "sin", "jamás",
    "ningún", "ninguna", "ninguno", "tampoco",
];

const POLES: [(&str, &str); 26] = [
    ("enabled", "disabled"),
    ("enable", "disable"),
    ("on", "off"),
    ("allowed", "denied"),
    ("allow", "deny"),
    ("allowed", "blocked"),
    ("true", "false"),
    ("up", "down"),
    ("success", "failure"),
    ("succeeds", "fails"),
    ("works", "broken"),
    ("cheap", "expensive"),
    ("increase", "decrease"),
    ("increased", "decreased"),
    ("add", "remove"),
    ("added", "removed"),
    ("before", "after"),
    ("min", "max"),
    ("minimum", "maximum"),
    ("open", "closed"),
    ("public", "private"),
    ("yes", "no"),
    ("keep", "drop"),
    ("include", "exclude"),
    ("required", "optional"),
    ("faster", "slower"),
];

#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    pub only_a: Vec<String>,
    pub only_b: Vec<String>,
    pub negation: bool,
    /// (a's pole, b's pole).
    pub poles: Vec<(String, String)>,
    pub joined_a: Vec<String>,
    pub joined_b: Vec<String>,
}

impl Diff {
    pub fn compatible(&self) -> bool {
        self.only_a.is_empty()
            && self.only_b.is_empty()
            && !self.negation
            && self.poles.is_empty()
            // Joined words separate them only when each side has its own: one replaces the other.
            && (self.joined_a.is_empty() || self.joined_b.is_empty())
    }
}

/// Only edge signs are trimmed: `0.30.0-2` and `user:ana` stay whole, and `can't` still negates.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '%')
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn is_fact(w: &str) -> bool {
    let has_digit = w.chars().any(|c| c.is_ascii_digit());
    if !has_digit && w.chars().all(|c| c.is_alphabetic() || c == '-' || c == '_') {
        return false;
    }
    let inner_sign = w
        .char_indices()
        .any(|(i, c)| i > 0 && i + c.len_utf8() < w.len() && "._/:@#-".contains(c));
    has_digit || inner_sign || w.starts_with("http")
}

/// Closed list: accepting any short word would turn "1 of 8" into the fact "1of".
const UNITS: [&str; 34] = [
    "s", "ms", "us", "ns", "sec", "secs", "second", "seconds", "min", "mins", "minute", "minutes",
    "h", "hr", "hrs", "hour", "hours", "d", "day", "days", "kb", "mb", "gb", "tb", "k", "m", "%",
    "rpm", "rps", "qps", "tokens", "req", "x", "€",
];

fn is_unit(w: &str) -> bool {
    UNITS.contains(&w)
}

/// Not hard facts: as facts they vetoed mere rewordings (on real duplicate pairs, 4 of 12 the
/// judge had right). Only one REPLACING another separates: "prod-eu" vs "prod-us".
fn is_joined_word(w: &str) -> bool {
    w.contains(['-', '_'])
        && w.chars().all(|c| c.is_alphabetic() || c == '-' || c == '_')
        && w.chars().any(char::is_alphabetic)
}

fn unit_canon(u: &str) -> &str {
    match u {
        "sec" | "secs" | "second" | "seconds" => "s",
        "mins" | "minute" | "minutes" => "min",
        "hr" | "hrs" | "hour" | "hours" => "h",
        "day" | "days" => "d",
        _ => u,
    }
}

fn number_with_unit(w: &str) -> Option<String> {
    let end = w
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(w.len());
    let (num, rest) = w.split_at(end);
    let unit = rest.strip_prefix('-').unwrap_or(rest);
    (num.starts_with(|c: char| c.is_ascii_digit()) && is_unit(unit))
        .then(|| format!("{num}{}", unit_canon(unit)))
}

pub fn hard_facts(text: &str) -> BTreeSet<String> {
    let ws = words(text);
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < ws.len() {
        let w = &ws[i];
        if is_fact(w) {
            let numeric = w
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == ',');
            match ws.get(i + 1) {
                Some(u) if numeric && is_unit(u) => {
                    out.insert(format!("{w}{}", unit_canon(u)));
                    i += 1;
                }
                _ => {
                    out.insert(number_with_unit(w).unwrap_or_else(|| w.clone()));
                }
            }
        }
        i += 1;
    }
    out
}

fn negates(ws: &[String]) -> bool {
    ws.iter()
        .any(|w| NEGATIONS.contains(&w.as_str()) || w.ends_with("n't"))
}

pub fn diff(a: &str, b: &str) -> Diff {
    let (fa, fb) = (hard_facts(a), hard_facts(b));
    let (wa, wb) = (words(a), words(b));
    let set_a: BTreeSet<&str> = wa.iter().map(|s| s.as_str()).collect();
    let set_b: BTreeSet<&str> = wb.iter().map(|s| s.as_str()).collect();
    let mut poles = Vec::new();
    for (x, y) in POLES {
        for (p, q) in [(x, y), (y, x)] {
            if set_a.contains(p) && !set_a.contains(q) && set_b.contains(q) && !set_b.contains(p) {
                poles.push((p.to_string(), q.to_string()));
            }
        }
    }
    let joined = |ws: &[String]| -> BTreeSet<String> {
        ws.iter().filter(|w| is_joined_word(w)).cloned().collect()
    };
    let (ja, jb) = (joined(&wa), joined(&wb));
    Diff {
        only_a: fa.difference(&fb).cloned().collect(),
        only_b: fb.difference(&fa).cloned().collect(),
        negation: negates(&wa) != negates(&wb),
        poles,
        joined_a: ja.difference(&jb).cloned().collect(),
        joined_b: jb.difference(&ja).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_number_or_unit_is_another_fact() {
        let d = diff(
            "The gateway LB times out streams at 120 seconds",
            "The gateway LB times out streams at 600 seconds",
        );
        assert!(!d.compatible());
        assert_eq!(d.only_a, vec!["120s"]);
        assert!(!diff("The cut is at 120 s", "The cut is at 120 ms").compatible());
        assert!(!diff("Pool capped at 20", "Pool capped at 50").compatible());
    }

    #[test]
    fn negation_and_opposite_poles_split_facts() {
        assert!(
            !diff(
                "The kill switch is checked before every step",
                "The kill switch is not checked before every step"
            )
            .compatible()
        );
        assert!(!diff("Approvals are checked", "Approvals aren't checked").compatible());
        let d = diff(
            "The nightly backup job for billing is enabled in production",
            "The nightly backup job for billing is disabled in production",
        );
        assert_eq!(
            d.poles,
            vec![("enabled".to_string(), "disabled".to_string())]
        );
        assert!(!diff("Tencent memory is cheap", "Tencent memory is expensive").compatible());
    }

    #[test]
    fn a_paraphrase_with_the_same_facts_is_compatible() {
        let d = diff(
            "The gateway load balancer times out streams at 120 seconds",
            "Streams through the gateway LB are cut after 120 seconds",
        );
        assert!(d.compatible(), "{d:?}");
        assert!(
            diff(
                "Ana prefers Chrome over Brave for login flows",
                "For logins Ana wants URLs opened in Chrome, not Brave"
            )
            .negation,
            "«not Brave» negates: better not to merge than to merge wrongly"
        );
        assert_eq!(
            hard_facts("released v2 yesterday, 3 builds, round 1 of 8, 20 s")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["1", "20s", "3", "8", "v2"]
        );
    }

    #[test]
    fn hyphenated_words_are_words_not_identifiers() {
        let d = diff(
            "The decision-making for follow-ups stays with the on-call owner",
            "The on call owner keeps deciding the follow up work",
        );
        assert!(d.compatible(), "{d:?}");
        assert!(
            diff(
                "The known_bad list is applied first",
                "The list of known bad hosts is applied first"
            )
            .compatible()
        );
        assert!(
            !diff(
                "Deploy goes to prod-eu first",
                "Deploy goes to prod-us first"
            )
            .compatible()
        );
        assert!(!diff("Traffic moved to us-east", "Traffic moved to eu-west").compatible());
        assert!(!diff("deploy gateway-v2 to prod", "deploy gateway-v3 to prod").compatible());
        assert!(!diff("run by svc_backup_01", "run by svc_backup_02").compatible());
        assert!(!diff("visible to user:ana", "visible to user:bob").compatible());
    }

    #[test]
    fn a_unit_is_compared_by_value_not_by_spelling() {
        assert!(
            diff(
                "The LB cuts streams at 30 seconds",
                "The LB cuts streams at 30s"
            )
            .compatible()
        );
        assert!(diff("Logs are kept 400-day", "Logs are kept 400 days").compatible());
        assert!(diff("p50 is 2 hrs", "p50 is 2h").compatible());
        assert!(diff("p50 is 13.8ms", "p50 is 13.8 ms").compatible());
        assert!(!diff("The LB cuts at 30 seconds", "The LB cuts at 60s").compatible());
        assert!(!diff("The cut is at 120s", "The cut is at 120ms").compatible());
        assert!(!diff("Logs are kept 400-day", "Logs are kept 30 days").compatible());
        assert_eq!(
            hard_facts("3-4 replicas at 1:1")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["1:1", "3-4"]
        );
    }
}
