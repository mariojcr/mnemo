// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Did the answer use what was injected? A `[m:<id>]` citation is a sure, free use; the rest goes
//! to the decisions service or a sampled LLM. Unjudged memories never count as useless.

use crate::app::{App, now_ms};
use crate::judge::Judge;
use crate::meter::{self, Eval, Injection, Tag, Turn};
use crate::storage::MeterRows;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

const DAY: i64 = 86_400_000;

pub const EVAL_PROMPT: &str = "You check whether an assistant's answer used background memories that were added to its context. \
For each numbered MEMORY, decide if the ANSWER relied on it: it applied, repeated or built on information that is only in that memory, \
so the answer would likely be different without it. A memory that is merely on the same topic does not count. \
Reply with JSON only: {\"used\": [numbers of the memories the answer relied on]}.";

/// Calibrated on 171 blind-labelled pairs (9 used): AUC 0.94, but real uses score 0.036 to 0.384,
/// so a 0.5 cut would mute every memory. 0.08 catches 5 of 9 with 7 false positives out of 161:
/// erring towards "used" only delays muting.
pub const USED_FROM: f32 = 0.08;

/// None of the 9 used pairs scored under 0.036. Between the two thresholds the verdict is doubtful
/// and does not count towards muting.
pub const UNUSED_BELOW: f32 = 0.03;

/// Re-reads p against the current threshold: evals stored under another one still count.
pub fn used(e: &Eval) -> bool {
    if e.method == "decisions" {
        e.p >= USED_FROM
    } else {
        e.used
    }
}

pub fn surely_unused(e: &Eval) -> bool {
    !used(e) && (e.method != "decisions" || e.p < UNUSED_BELOW)
}

pub fn cited(answer: &str, ids: &[String]) -> HashSet<String> {
    ids.iter()
        .filter(|id| answer.contains(&format!("m:{id}")))
        .cloned()
        .collect()
}

fn sampled(rate: f64) -> bool {
    rate >= 1.0 || (uuid::Uuid::new_v4().as_bytes()[0] as f64) < rate * 256.0
}

impl App {
    /// Close and judge at once: what the tests need. The handler splits it (`turn` in ops.rs).
    #[cfg(test)]
    pub async fn finish_turn(&self, t: Turn, answer: String) -> Result<usize, String> {
        let (t, injected) = self.close_turn(t).await?;
        self.judge_turn(&t, &injected, &answer).await
    }

    /// Closes the turn's window when its report arrives: records the turn and takes what was
    /// injected into the session since the previous one. Judging comes later, in the background;
    /// closing then would also take what the next turn already injected (a client that asks again
    /// at once, the next step of a workflow) and judge it against this answer.
    pub async fn close_turn(&self, mut t: Turn) -> Result<(Turn, Vec<Injection>), String> {
        let last = self.db.last_turn_at(&t.session).await?;
        // The edge is inclusive. An injection that lands in the same millisecond right after the
        // report falls in neither window: never judged, which is harmless (unjudged never counts as
        // useless), while moving the edge back would judge it against the wrong answer.
        t.at = now_ms().max(last + 1);
        let injected: Vec<Injection> = self
            .db
            .injections_after(&t.session, last)
            .await?
            .into_iter()
            .filter(|i| i.at <= t.at && !i.memory_ids.is_empty())
            .collect();
        self.db.record_turn(&t).await?;
        Ok((t, injected))
    }

    /// Judges what a closed turn had injected against its answer; returns how many injections.
    pub async fn judge_turn(
        &self,
        t: &Turn,
        injected: &[Injection],
        answer: &str,
    ) -> Result<usize, String> {
        let n = injected.len();
        if n > 0 && !answer.trim().is_empty() {
            let evals = self.evaluate(t, injected, answer).await;
            if !evals.is_empty() {
                self.db.record_evals(&evals).await?;
            }
        }
        Ok(n)
    }

    async fn evaluate(&self, t: &Turn, injected: &[Injection], answer: &str) -> Vec<Eval> {
        // A memory may arrive in two injections of the turn: judge it once, against the first.
        let mut first: Vec<(String, String)> = Vec::new();
        for i in injected {
            for m in &i.memory_ids {
                if !first.iter().any(|(id, _)| id == m) {
                    first.push((m.clone(), i.id.clone()));
                }
            }
        }
        let ids: Vec<String> = first.iter().map(|(m, _)| m.clone()).collect();
        let cites = cited(answer, &ids);
        let eval = |memory_id: &str, injection_id: &str, used: bool, method: &str, p: f32| Eval {
            at: now_ms(),
            injection_id: injection_id.to_string(),
            memory_id: memory_id.to_string(),
            session: t.session.clone(),
            subject: t.subject.clone(),
            used,
            method: method.into(),
            p,
        };
        let mut out: Vec<Eval> = first
            .iter()
            .filter(|(m, _)| cites.contains(m))
            .map(|(m, i)| eval(m, i, true, "cited", 1.0))
            .collect();
        let rest: Vec<&(String, String)> =
            first.iter().filter(|(m, _)| !cites.contains(m)).collect();
        if rest.is_empty() {
            return out;
        }
        let lines: Vec<String> = {
            let g = self.globe.read().unwrap();
            rest.iter()
                .map(|(m, _)| {
                    g.memory(m)
                        .map(|mem| crate::render::memory_line(&g, mem))
                        .unwrap_or_default()
                })
                .collect()
        };
        let question = injected.last().map(|i| i.query.clone()).unwrap_or_default();
        let answer: String = answer.chars().take(6000).collect();
        let tag = Tag {
            kind: "eval",
            subject: t.subject.clone(),
            thread: t.session.clone(),
        };
        let verdicts: Option<(Vec<f32>, &str)> = meter::tagged(tag, async {
            match &self.judge {
                Judge::Decisions(llm, model, timeout) => {
                    let state = json!({"user_question": question, "assistant_answer": answer,
                        "memories": lines.iter().enumerate().map(|(i, l)| json!({"n": i + 1, "memory": l})).collect::<Vec<_>>()});
                    let questions: serde_json::Map<String, Value> = (1..=lines.len())
                        .map(|n| (format!("used_{n}"), json!({"type": "noul", "instructions": format!("Did the ASSISTANT ANSWER rely on MEMORY number {n}: apply, repeat or build on information found only in it, so that the answer would likely be different without it? A memory that is merely on the same topic does not count.")})))
                        .collect();
                    match llm.decide(model, state, Value::Object(questions), *timeout).await {
                        Ok(a) => Some(((1..=lines.len()).map(|n| a.pointer(&format!("/used_{n}/noul")).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32).collect(), "decisions")),
                        Err(e) => {
                            log::warn!("evaluation with decisions: {e}");
                            None
                        }
                    }
                }
                Judge::Llm(llm, model, timeout) if sampled(self.cfg.eval_sample) => {
                    let user = format!(
                        "USER QUESTION:\n{question}\n\nANSWER:\n{answer}\n\nMEMORIES:\n{}",
                        lines.iter().enumerate().map(|(i, l)| format!("{}. {l}", i + 1)).collect::<Vec<_>>().join("\n")
                    );
                    match llm.chat(model, EVAL_PROMPT, &user, 200, *timeout, Some(("used", serde_json::json!({"type": "object", "properties": {"used": {"type": "array", "items": {"type": "integer"}}}, "required": ["used"]})))).await {
                        Ok(text) => parse_used(&text, lines.len()).map(|used| (used, "llm")),
                        Err(e) => {
                            log::warn!("evaluation with the LLM: {e}");
                            None
                        }
                    }
                }
                _ => None,
            }
        })
        .await;
        if let Some((ps, method)) = verdicts {
            for ((m, i), p) in rest.iter().zip(ps) {
                out.push(eval(m, i, p >= USED_FROM, method, p));
            }
        }
        out
    }

    pub async fn refresh_usefulness(&self) -> Result<usize, String> {
        if self.cfg.mute_after == 0 {
            return Ok(0);
        }
        let evals = self.db.evals_since(now_ms() - 30 * DAY).await?;
        let mut per: HashMap<(&str, &str), (u64, u64)> = HashMap::new();
        for e in &evals {
            let c = per
                .entry((e.subject.as_str(), e.memory_id.as_str()))
                .or_default();
            c.0 += surely_unused(e) as u64;
            c.1 += used(e) as u64;
        }
        let muted: HashSet<(String, String)> = per
            .into_iter()
            .filter(|(_, (n, u))| *n >= self.cfg.mute_after && *u == 0)
            .map(|((who, id), _)| (who.to_string(), id.to_string()))
            .collect();
        let n = muted.len();
        *self.muted.write().unwrap() = muted;
        Ok(n)
    }

    pub async fn cost_report(&self, days: i64, subject: Option<&str>) -> Result<Value, String> {
        let from = now_ms() - days.clamp(1, 90) * DAY;
        let rows = self.db.meter_rows(from, subject).await?;
        let g = self.globe.read().unwrap();
        let muted = self.muted.read().unwrap();
        let is_muted = |id: &str| {
            muted
                .iter()
                .any(|(who, m)| m == id && subject.is_none_or(|s| s == who))
        };
        Ok(report(&rows, &self.cfg.prices, from, &|id| {
            g.memory(id).map(|m| (m.what.clone(), is_muted(id)))
        }))
    }
}

fn parse_used(text: &str, n: usize) -> Option<Vec<f32>> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let v: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let used: HashSet<u64> = v
        .get("used")?
        .as_array()?
        .iter()
        .filter_map(|x| x.as_u64())
        .collect();
    Some(
        (1..=n as u64)
            .map(|i| if used.contains(&i) { 1.0 } else { 0.0 })
            .collect(),
    )
}

fn dollars(
    prices: &HashMap<String, [f64; 4]>,
    model: &str,
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
) -> Option<f64> {
    let p = prices.get(model).or_else(|| {
        prices
            .iter()
            .find(|(k, _)| model.starts_with(k.as_str()))
            .map(|(_, v)| v)
    })?;
    Some(
        (input as f64 * p[0]
            + cached as f64 * p[1]
            + cache_write as f64 * p[2]
            + output as f64 * p[3])
            / 1e6,
    )
}

fn day_of(ms: i64) -> String {
    crate::query::fmt_day(ms)
}

pub fn report(
    rows: &MeterRows,
    prices: &HashMap<String, [f64; 4]>,
    from: i64,
    memory: &dyn Fn(&str) -> Option<(String, bool)>,
) -> Value {
    let with_memory: Vec<&Injection> = rows
        .injections
        .iter()
        .filter(|i| !i.memory_ids.is_empty() || !i.entity_ids.is_empty())
        .collect();
    let injected_tokens: u64 = with_memory.iter().map(|i| i.tokens).sum();
    let mut by_surface: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for i in &with_memory {
        let e = by_surface.entry(i.surface.as_str()).or_default();
        e.0 += 1;
        e.1 += i.tokens;
    }
    let lines: u64 = with_memory.iter().map(|i| i.memory_ids.len() as u64).sum();
    let per_line = if lines > 0 {
        injected_tokens as f64 / lines as f64
    } else {
        0.0
    };
    let repeated: u64 = rows.injections.iter().map(|i| i.repeated).sum();
    let muted: u64 = rows.injections.iter().map(|i| i.muted).sum();
    let budget_skips = rows
        .injections
        .iter()
        .filter(|i| i.skipped == "budget")
        .count();

    let sessions: HashSet<&str> = with_memory.iter().map(|i| i.session.as_str()).collect();
    let mut sess = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut sess_cost = 0.0;
    let mut memory_cost = 0.0;
    let mut model_of: HashMap<&str, &str> = HashMap::new();
    for t in &rows.turns {
        sess.0 += 1;
        sess.1 += t.input_tokens;
        sess.2 += t.cache_read_tokens;
        sess.3 += t.cache_creation_tokens;
        sess.4 += t.output_tokens;
        sess_cost += dollars(
            prices,
            &t.model,
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_creation_tokens,
            t.output_tokens,
        )
        .unwrap_or(0.0);
        model_of.insert(t.session.as_str(), t.model.as_str());
    }
    for i in &with_memory {
        // Injected tokens are paid at least once as fresh input to that session's model.
        if let Some(m) = model_of.get(i.session.as_str()) {
            memory_cost += dollars(prices, m, i.tokens, 0, 0, 0).unwrap_or(0.0);
        }
    }
    let session_input = sess.1 + sess.2 + sess.3;
    let measured_sessions = rows
        .turns
        .iter()
        .filter(|t| sessions.contains(t.session.as_str()))
        .count();

    let mut bg: BTreeMap<String, Value> = BTreeMap::new();
    let mut bg_cost = 0.0;
    let mut bg_tokens = 0u64;
    for c in &rows.llm {
        let e = bg.entry(c.kind.clone()).or_insert_with(|| json!({"calls": 0, "failed": 0, "prompt": 0, "cached": 0, "cache_creation": 0, "completion": 0, "ms": 0, "usd": 0.0, "models": []}));
        let add = |e: &mut Value, k: &str, n: u64| e[k] = json!(e[k].as_u64().unwrap_or(0) + n);
        add(e, "calls", 1);
        add(e, "failed", (!c.ok) as u64);
        add(e, "prompt", c.prompt_tokens);
        add(e, "cached", c.cached_tokens);
        add(e, "cache_creation", c.cache_creation_tokens);
        add(e, "completion", c.completion_tokens);
        add(e, "ms", c.ms);
        if let Some(d) = dollars(
            prices,
            &c.model,
            c.prompt_tokens.saturating_sub(c.cached_tokens),
            c.cached_tokens,
            c.cache_creation_tokens,
            c.completion_tokens,
        ) {
            e["usd"] = json!(e["usd"].as_f64().unwrap_or(0.0) + d);
            bg_cost += d;
        }
        if !e["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == &json!(c.model))
        {
            e["models"].as_array_mut().unwrap().push(json!(c.model));
        }
        bg_tokens += c.prompt_tokens + c.completion_tokens;
    }

    let mut evaluated: HashMap<&str, bool> = HashMap::new();
    let mut methods: BTreeMap<&str, u64> = BTreeMap::new();
    let mut per_mem: HashMap<&str, (u64, u64, u64)> = HashMap::new();
    for e in &rows.evals {
        let u = evaluated.entry(e.injection_id.as_str()).or_insert(false);
        *u |= used(e);
        *methods.entry(e.method.as_str()).or_default() += 1;
        let m = per_mem.entry(e.memory_id.as_str()).or_default();
        m.1 += 1;
        m.2 += used(e) as u64;
    }
    for i in &with_memory {
        for id in &i.memory_ids {
            per_mem.entry(id.as_str()).or_default().0 += 1;
        }
    }
    let useful = evaluated.values().filter(|u| **u).count() as u64;
    let judged = evaluated.len() as u64;

    let mut daily: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    let mut d = from;
    while d <= now_ms() {
        daily.entry(day_of(d)).or_default();
        d += DAY;
    }
    for i in &with_memory {
        daily.entry(day_of(i.at)).or_default().0 += i.tokens;
    }
    for c in &rows.llm {
        daily.entry(day_of(c.at)).or_default().1 += c.prompt_tokens + c.completion_tokens;
    }
    let inj_day: HashMap<&str, String> = rows
        .injections
        .iter()
        .map(|i| (i.id.as_str(), day_of(i.at)))
        .collect();
    for (id, used) in &evaluated {
        if let Some(day) = inj_day.get(id) {
            let e = daily.entry(day.clone()).or_default();
            e.2 += 1;
            e.3 += *used as u64;
        }
    }

    let mut top: Vec<(&str, (u64, u64, u64))> = per_mem.into_iter().collect();
    top.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    let ratio = |a: u64, b: u64| {
        if b == 0 {
            Value::Null
        } else {
            json!(a as f64 / b as f64)
        }
    };
    json!({
        "from": from,
        "injected": {
            "blocks": with_memory.len(),
            "tokens": injected_tokens,
            "by_surface": by_surface.iter().map(|(k, (n, t))| (k.to_string(), json!({"blocks": n, "tokens": t}))).collect::<serde_json::Map<_, _>>(),
            "repeated_skipped": repeated,
            "repeated_tokens_saved": (repeated as f64 * per_line).round() as u64,
            "muted_skipped": muted,
            "budget_skips": budget_skips,
            "usd": memory_cost,
        },
        "sessions": {
            "turns": sess.0,
            "turns_with_memory": measured_sessions,
            "input": sess.1, "cache_read": sess.2, "cache_creation": sess.3, "output": sess.4,
            "usd": sess_cost,
            "memory_share": ratio(injected_tokens, session_input),
        },
        "background": {"by_kind": bg, "tokens": bg_tokens, "usd": bg_cost},
        "value": {
            "judged_blocks": judged,
            "useful_blocks": useful,
            "useful_rate": ratio(useful, judged),
            "unjudged_blocks": (with_memory.iter().filter(|i| !i.memory_ids.is_empty()).count() as u64).saturating_sub(judged),
            "by_method": methods,
            "tokens_per_useful_block": injected_tokens.checked_div(useful).map(|v| json!(v)).unwrap_or(Value::Null),
        },
        "daily": daily.iter().map(|(day, (inj, bgt, j, u))| json!({"day": day, "injected_tokens": inj, "background_tokens": bgt, "judged": j, "useful": u})).collect::<Vec<_>>(),
        "memories": top.iter().take(25).map(|(id, (inj, j, u))| {
            let (what, is_muted) = memory(id).unwrap_or_default();
            json!({"id": id, "what": what, "injected": inj, "judged": j, "used": u, "muted": is_muted})
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meter::LlmCall;

    #[test]
    fn citations_are_found() {
        let ids = vec!["abc".to_string(), "def".to_string()];
        assert_eq!(
            cited("as noted in [m:abc], yes", &ids),
            HashSet::from(["abc".to_string()])
        );
        assert!(cited("nothing here", &ids).is_empty());
    }

    #[tokio::test]
    async fn only_sure_verdicts_mute_a_memory() {
        use crate::app::Cfg;
        use crate::storage::mem::Mem;
        use crate::storage::{Db, RawStore};
        use std::sync::Arc;
        let db = Arc::new(Mem::new());
        let app = App::new(
            db.clone() as Arc<dyn Db>,
            Some(db.clone() as Arc<dyn RawStore>),
            None,
            None,
            Judge::Heuristic,
            Cfg {
                mute_after: 3,
                ..Default::default()
            },
        );
        let ev = |m: &str, p: f32| Eval {
            at: now_ms(),
            memory_id: m.into(),
            subject: "ana".into(),
            session: "ana/s".into(),
            // The stored flag says unused: the verdict must come from re-reading `p`.
            used: false,
            method: "decisions".into(),
            p,
            ..Default::default()
        };
        db.record_evals(&[
            ev("noise", 0.01),
            ev("noise", 0.02),
            ev("noise", 0.005),
            ev("doubt", 0.05),
            ev("doubt", 0.04),
            ev("doubt", 0.01),
            ev("doubt", 0.02),
            ev("useful", 0.01),
            ev("useful", 0.01),
            ev("useful", 0.01),
            ev("useful", 0.12),
        ])
        .await
        .unwrap();
        app.refresh_usefulness().await.unwrap();
        let muted = app.muted.read().unwrap().clone();
        let is = |m: &str| muted.contains(&("ana".to_string(), m.to_string()));
        assert!(is("noise"), "three confident 'unused' verdicts");
        assert!(
            !is("doubt"),
            "unsure verdicts do not count: only two are confident"
        );
        assert!(
            !is("useful"),
            "a p of 0.12 stored under the old cutoff counts as a use"
        );
    }

    #[test]
    fn llm_verdicts_parse_and_default_to_unused() {
        assert_eq!(
            parse_used("sure: {\"used\": [2]}", 3),
            Some(vec![0.0, 1.0, 0.0])
        );
        assert_eq!(parse_used("no json", 2), None);
    }

    #[test]
    fn report_adds_up_cost_and_value() {
        let now = now_ms();
        let inj = |id: &str, session: &str, mems: &[&str], tokens: u64| Injection {
            id: id.into(),
            at: now,
            surface: "prompt".into(),
            subject: "ana".into(),
            session: session.into(),
            memory_ids: mems.iter().map(|m| m.to_string()).collect(),
            tokens,
            ..Default::default()
        };
        let rows = MeterRows {
            injections: vec![
                inj("i1", "s1", &["m1", "m2"], 200),
                inj("i2", "s1", &["m3"], 100),
                Injection {
                    repeated: 2,
                    ..inj("i3", "s1", &[], 0)
                },
            ],
            turns: vec![Turn {
                at: now,
                session: "s1".into(),
                model: "claude-x".into(),
                input_tokens: 1000,
                cache_read_tokens: 2000,
                output_tokens: 300,
                calls: 2,
                ..Default::default()
            }],
            llm: vec![LlmCall {
                at: now,
                kind: "extract".into(),
                model: "claude-x".into(),
                prompt_tokens: 5000,
                completion_tokens: 500,
                ok: true,
                ..Default::default()
            }],
            evals: vec![
                Eval {
                    injection_id: "i1".into(),
                    memory_id: "m1".into(),
                    used: true,
                    method: "cited".into(),
                    ..Default::default()
                },
                Eval {
                    injection_id: "i1".into(),
                    memory_id: "m2".into(),
                    used: false,
                    method: "llm".into(),
                    ..Default::default()
                },
                Eval {
                    injection_id: "i2".into(),
                    memory_id: "m3".into(),
                    used: false,
                    method: "llm".into(),
                    ..Default::default()
                },
            ],
        };
        let prices = HashMap::from([("claude-x".to_string(), [3.0, 0.3, 3.75, 15.0])]);
        let r = report(&rows, &prices, now - DAY, &|_| None);
        assert_eq!(r["injected"]["tokens"], 300);
        assert_eq!(r["injected"]["repeated_skipped"], 2);
        assert_eq!(
            r["injected"]["repeated_tokens_saved"], 200,
            "2 repeated lines × 100 tokens on average"
        );
        assert_eq!(r["value"]["judged_blocks"], 2);
        assert_eq!(r["value"]["useful_blocks"], 1);
        assert_eq!(r["value"]["useful_rate"], 0.5);
        assert_eq!(r["value"]["tokens_per_useful_block"], 300);
        assert_eq!(r["sessions"]["memory_share"], 0.1, "300 of 3,000 input");
        assert_eq!(r["background"]["by_kind"]["extract"]["calls"], 1);
        let usd = r["background"]["usd"].as_f64().unwrap();
        assert!((usd - (5000.0 * 3.0 + 500.0 * 15.0) / 1e6).abs() < 1e-9);
        assert_eq!(r["memories"][0]["id"], "m1");
    }
}
