// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Parsing a question without a model: the W it asks for, the dates it names and its terms.

use crate::model::W;
use crate::text::{norm, tokens};

pub const DAY_MS: i64 = 86_400_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parsed {
    pub w: Option<W>,
    /// Half-open range [from, until) in ms.
    pub from: Option<i64>,
    pub until: Option<i64>,
    pub terms: Vec<String>,
}

pub fn asked_w(q: &str) -> Option<W> {
    let s = norm(q);
    let s = s.trim_start_matches(|c: char| !c.is_alphanumeric());
    let words: Vec<&str> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has_pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);
    if has_pair("por", "que")
        || words.contains(&"porque")
        || words.contains(&"why")
        || has_pair("a", "que") && has_pair("se", "debe")
    {
        return Some(W::Why);
    }
    let first = words.first().copied().unwrap_or("");
    let second = words.get(1).copied().unwrap_or("");
    match first {
        "quien" | "quienes" | "who" | "whom" | "whose" => return Some(W::Who),
        "cuando" | "when" => return Some(W::When),
        "donde" | "where" => return Some(W::Where),
        "como" | "how" => return Some(W::How),
        "que" | "what" | "cual" | "which" => return Some(W::What),
        "en" if second == "que" || second == "donde" => return Some(W::Where),
        "de" | "a" | "con" | "para" if second == "quien" || second == "quienes" => {
            return Some(W::Who);
        }
        _ => {}
    }
    for w in &words {
        match *w {
            "quien" | "quienes" => return Some(W::Who),
            "cuando" => return Some(W::When),
            "donde" => return Some(W::Where),
            "como" if words.len() > 2 => return Some(W::How),
            _ => {}
        }
    }
    None
}

// Howard Hinnant's civil-date algorithms.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn ymd_ms(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d) * DAY_MS
}

pub fn fmt_day(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(DAY_MS));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Without a zone the time is taken as UTC.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let date = s.get(0..10)?;
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = ymd_ms(y, m, d);
    if let Some(time) = s.get(11..19) {
        let mut t = time.split(':');
        let h: i64 = t.next()?.parse().ok()?;
        let mi: i64 = t.next()?.parse().ok()?;
        let se: i64 = t.next()?.parse().ok()?;
        ms += ((h * 60 + mi) * 60 + se) * 1000;
        let rest = s.get(19..).unwrap_or("");
        let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
            let digits: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 2 {
                let oh: i64 = digits[0..2].parse().ok()?;
                let om: i64 = digits.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
                let off = (oh * 60 + om) * 60_000;
                ms -= if sign == '+' { off } else { -off };
            }
        }
    }
    Some(ms)
}

fn month(word: &str) -> Option<i64> {
    let w = word.trim_end_matches('.');
    let names: [(&[&str], i64); 12] = [
        (&["enero", "ene", "january", "jan"], 1),
        (&["febrero", "feb", "february"], 2),
        (&["marzo", "mar", "march"], 3),
        (&["abril", "abr", "april", "apr"], 4),
        (&["mayo", "may"], 5),
        (&["junio", "jun", "june"], 6),
        (&["julio", "jul", "july"], 7),
        (&["agosto", "ago", "august", "aug"], 8),
        (&["septiembre", "setiembre", "sep", "sept", "september"], 9),
        (&["octubre", "oct", "october"], 10),
        (&["noviembre", "nov", "november"], 11),
        (&["diciembre", "dic", "december", "dec"], 12),
    ];
    names
        .iter()
        .find(|(ns, _)| ns.contains(&w))
        .map(|(_, m)| *m)
}

pub fn date_range(q: &str, now: i64) -> Option<(i64, i64)> {
    let s = norm(q);
    let words: Vec<&str> = s
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let today = now.div_euclid(DAY_MS);
    let (cy, cm, _) = civil_from_days(today);
    let day = |d: i64| Some((d * DAY_MS, (d + 1) * DAY_MS));
    let has = |a: &str| words.contains(&a);
    let pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);
    // Monday = 0. Day 0 (the Unix epoch) was a Thursday.
    let weekday = |d: i64| (d + 3).rem_euclid(7);

    for w in &words {
        if w.len() == 10 && w.as_bytes()[4] == b'-' {
            if let Some(ms) = parse_iso(w) {
                return Some((ms, ms + DAY_MS));
            }
        }
    }
    if has("anteayer") {
        return day(today - 2);
    }
    if has("ayer") || has("yesterday") {
        return day(today - 1);
    }
    if has("hoy") || has("today") {
        return day(today);
    }
    let monday = today - weekday(today);
    if pair("semana", "pasada") || pair("last", "week") {
        return Some(((monday - 7) * DAY_MS, monday * DAY_MS));
    }
    if pair("esta", "semana") || pair("this", "week") {
        return Some((monday * DAY_MS, (monday + 7) * DAY_MS));
    }
    let month_range = |y: i64, m: i64| {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        Some((ymd_ms(y, m, 1), ymd_ms(ny, nm, 1)))
    };
    if pair("mes", "pasado") || pair("last", "month") {
        let (y, m) = if cm == 1 { (cy - 1, 12) } else { (cy, cm - 1) };
        return month_range(y, m);
    }
    if pair("este", "mes") || pair("this", "month") {
        return month_range(cy, cm);
    }
    // "N days/weeks/months ago": that point with one unit of slack on each side.
    for (i, w) in words.iter().enumerate() {
        let Ok(n) = w.parse::<i64>() else { continue };
        let unit = words.get(i + 1).copied().unwrap_or("");
        let ago = (i > 0 && words[i - 1] == "hace") || words.get(i + 2) == Some(&"ago");
        if !ago {
            continue;
        }
        let span = match unit {
            "dia" | "dias" | "day" | "days" => 1,
            "semana" | "semanas" | "week" | "weeks" => 7,
            "mes" | "meses" | "month" | "months" => 30,
            _ => continue,
        };
        let center = today - n * span;
        return Some(((center - span) * DAY_MS, (center + span + 1) * DAY_MS));
    }
    for (i, w) in words.iter().enumerate() {
        let Some(m) = month(w) else { continue };
        // "mar", "may"… are also ordinary words: without a nearby number they count only after a
        // preposition.
        let num_before = i
            .checked_sub(1)
            .and_then(|j| words[j].parse::<i64>().ok())
            .or_else(|| {
                if i >= 2 && words[i - 1] == "de" {
                    words[i - 2].parse::<i64>().ok()
                } else {
                    None
                }
            });
        let num_after = words.get(i + 1).and_then(|x| x.parse::<i64>().ok());
        let y = if m > cm { cy - 1 } else { cy };
        if let Some(d) = num_before.or(num_after).filter(|d| (1..=31).contains(d)) {
            return day(days_from_civil(y, m, d));
        }
        let ambiguous = matches!(*w, "mar" | "may" | "sep" | "dic" | "ago");
        let after_in = i > 0 && matches!(words[i - 1], "en" | "in" | "de" | "durante" | "during");
        if !ambiguous || after_in {
            return month_range(y, m);
        }
    }
    None
}

pub fn parse(q: &str, now: i64) -> Parsed {
    let range = date_range(q, now);
    Parsed {
        w: asked_w(q),
        from: range.map(|r| r.0),
        until: range.map(|r| r.1),
        terms: tokens(q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Friday, at noon.
    fn now() -> i64 {
        ymd_ms(2026, 10, 2) + 12 * 3_600_000
    }

    #[test]
    fn a_timestamp_with_an_offset_is_moved_to_utc() {
        let utc = parse_iso("2026-10-10T09:00:00Z").unwrap();
        assert_eq!(parse_iso("2026-10-10T11:00:00+02:00"), Some(utc));
        assert_eq!(parse_iso("2026-10-10T04:00:00.250-0500"), Some(utc));
        assert_eq!(
            parse_iso("2026-10-10T09:00:00"),
            Some(utc),
            "no offset means UTC"
        );
        assert_eq!(parse_iso("2026-10-10"), Some(ymd_ms(2026, 10, 10)));
    }

    #[test]
    fn asked_w_reads_the_interrogative() {
        assert_eq!(
            asked_w("¿Por qué se cortaban las peticiones?"),
            Some(W::Why)
        );
        assert_eq!(asked_w("why did the LB cut requests"), Some(W::Why));
        assert_eq!(asked_w("¿Quién decidió sacar la inferencia?"), Some(W::Who));
        assert_eq!(asked_w("¿Cuándo se migró Nomad?"), Some(W::When));
        assert_eq!(asked_w("¿Dónde corre Qwen?"), Some(W::Where));
        assert_eq!(asked_w("¿En qué entorno corre Qwen?"), Some(W::Where));
        assert_eq!(asked_w("¿Cómo se arregló el doble turno?"), Some(W::How));
        assert_eq!(asked_w("dime cómo funciona projectCodeRoot"), Some(W::How));
        assert_eq!(asked_w("¿Qué le pasó a Nomad?"), Some(W::What));
        assert_eq!(asked_w("gateway timeouts"), None);
    }

    #[test]
    fn civil_dates_round_trip() {
        for (y, m, d) in [(1970, 1, 1), (2026, 9, 22), (2024, 2, 29), (2000, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        assert_eq!(
            fmt_day(parse_iso("2026-09-22T10:00:00Z").unwrap()),
            "2026-09-22"
        );
    }

    #[test]
    fn relative_dates_resolve_against_now() {
        let n = now();
        assert_eq!(
            date_range("¿qué pasó ayer?", n),
            Some((ymd_ms(2026, 10, 1), ymd_ms(2026, 10, 2)))
        );
        assert_eq!(
            date_range("la semana pasada", n),
            Some((ymd_ms(2026, 9, 21), ymd_ms(2026, 9, 28)))
        );
        assert_eq!(
            date_range("el mes pasado", n),
            Some((ymd_ms(2026, 9, 1), ymd_ms(2026, 10, 1)))
        );
        assert_eq!(
            date_range("¿qué le pasó al gateway en septiembre?", n),
            Some((ymd_ms(2026, 9, 1), ymd_ms(2026, 10, 1)))
        );
        assert_eq!(
            date_range("el 22 de septiembre", n),
            Some((ymd_ms(2026, 9, 22), ymd_ms(2026, 9, 23)))
        );
        assert_eq!(
            date_range("on sep 22", n),
            Some((ymd_ms(2026, 9, 22), ymd_ms(2026, 9, 23)))
        );
        assert_eq!(
            date_range("en diciembre", n),
            Some((ymd_ms(2025, 12, 1), ymd_ms(2026, 1, 1)))
        );
        assert!(date_range("hace 3 días", n).is_some());
        assert_eq!(
            date_range("2026-09-10", n),
            Some((ymd_ms(2026, 9, 10), ymd_ms(2026, 9, 11)))
        );
    }

    #[test]
    fn ambiguous_month_words_need_context() {
        let n = now();
        assert_eq!(date_range("el mar está en calma", n), None);
        assert_eq!(date_range("may I ask", n), None);
        assert!(date_range("en mayo", n).is_some());
    }
}
