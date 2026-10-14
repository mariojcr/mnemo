//! The episode: an immutable unit of raw input with its provenance.

use crate::query::fmt_day;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Episode {
    pub episode_id: String,
    /// The unit that is distilled together.
    pub thread: String,
    /// chat | tool_result | run_step | slack
    pub source: String,
    #[serde(default)]
    pub source_ref: String,
    /// Empty means the sender's default scope (their own).
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub owner_id: String,
    #[serde(default)]
    pub actor_id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub role: String,
    pub occurred_at: i64,
    pub text: String,
    #[serde(default)]
    pub entities: Vec<String>,
}

/// Longer text is split into chunks; each chunk's id gets a `#n` suffix.
pub const MAX_CHUNK: usize = 6000;

pub fn chunk(ep: &Episode) -> Vec<Episode> {
    let chars: Vec<char> = ep.text.chars().collect();
    if chars.len() <= MAX_CHUNK {
        return vec![ep.clone()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut n = 0;
    while start < chars.len() {
        let mut end = (start + MAX_CHUNK).min(chars.len());
        if end < chars.len() {
            if let Some(nl) = (start + MAX_CHUNK / 2..end)
                .rev()
                .find(|&i| chars[i] == '\n')
            {
                end = nl + 1;
            }
        }
        let mut part = ep.clone();
        part.episode_id = format!("{}#{}", ep.episode_id, n);
        part.text = chars[start..end].iter().collect();
        out.push(part);
        start = end;
        n += 1;
    }
    out
}

/// `YYYY-MM-DD HH:MM:SS.mmm` in UTC, the format a DateTime64(3) column parses.
pub fn fmt_ts(ms: i64) -> String {
    let day = fmt_day(ms);
    let rem = ms.rem_euclid(86_400_000);
    format!(
        "{} {:02}:{:02}:{:02}.{:03}",
        day,
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_chunked_on_line_breaks_and_short_text_is_not() {
        let short = Episode {
            episode_id: "chat:1".into(),
            text: "hola".into(),
            ..Default::default()
        };
        assert_eq!(chunk(&short), vec![short.clone()]);
        let line = "x".repeat(99) + "\n";
        let long = Episode {
            episode_id: "chat:2".into(),
            text: line.repeat(150),
            ..Default::default()
        };
        let parts = chunk(&long);
        assert!(parts.len() >= 3);
        assert_eq!(parts[0].episode_id, "chat:2#0");
        assert!(parts.iter().all(|p| p.text.chars().count() <= MAX_CHUNK));
        assert!(parts[0].text.ends_with('\n'), "cuts at a line break");
        assert_eq!(
            parts.iter().map(|p| p.text.clone()).collect::<String>(),
            long.text,
            "nothing is lost"
        );
    }

    #[test]
    fn scope_is_optional_on_the_wire() {
        let ep: Episode = serde_json::from_str(
            r#"{"episode_id":"cc:1","thread":"cc:s","source":"chat","occurred_at":1,"text":"x"}"#,
        )
        .unwrap();
        assert_eq!(ep.scope, "");
    }

    #[test]
    fn timestamps_are_utc_with_millis() {
        assert_eq!(
            fmt_ts(crate::query::ymd_ms(2026, 9, 22) + 3_723_004),
            "2026-09-22 01:02:03.004"
        );
    }
}
