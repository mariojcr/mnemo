// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The judge: decision service, else the extraction LLM, else a cautious heuristic. It decides
//! what gets PROMOTED, never what gets stored: discards stay in the raw store.

use crate::llm::Llm;
use crate::model::Memory;
use crate::query::fmt_day;
use crate::text::tokens;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::time::Duration;

pub const REL_MIN: f32 = 0.6;
/// First step to close a neighbour: the top answer is `updates` or `contradicts`, and together they
/// reach this. Only those neighbours are asked [`OUTDATED_QUESTION`].
///
/// Measured on 2026-11-25 against blind gold labels for 475 of 2,457 real candidate/neighbour pairs.
/// Only 1.5 % of neighbours are true updates, so even a good ranking (AUC 0.97) closes many that are
/// still true.
///
/// | Rule | Closings | Wrong | True updates caught |
/// |---|---|---|---|
/// | 0.3.0: `updates` + `contradicts` ≥ 0.5 | 75 | 52 | 22 of 38 |
/// | these two steps, no events, one per candidate | 21 | 8 | 10 of 38 |
pub const CLOSE_MIN: f32 = 0.4;
/// Second step: the probability that EVERYTHING in the neighbour is outdated. This yes/no is
/// calibrated low (true updates score 0.01 to 0.27), so the bar is low too.
pub const OUTDATED_MIN: f32 = 0.08;
/// An event records what happened; a later change does not make it untrue.
const NEVER_CLOSED: &str = "event";
pub const OUTDATED_QUESTION: &str = "Now that the CANDIDATE memory is known, is EVERYTHING the EXISTING memory says outdated or no longer true, so that showing it again would only mislead? Answer no if any part of it is still true or still useful, if they are about different things, if the existing memory gives a reason, a cause, a limit or a step that still holds, or if it records something that happened (an event stays true as history).";
pub const WORTH_MIN: f32 = 0.45;
/// Below this, the candidate is treated as a transient state.
pub const DURABLE_MIN: f32 = 0.5;
pub const WHY_MIN: f32 = 0.4;

#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub neighbor: String,
    /// same | updates | partly | contradicts | different
    pub kind: String,
    pub p: f32,
    /// Probability that the existing memory is outdated as a whole (`updates` + `contradicts`).
    pub close: f32,
    /// The answer to [`OUTDATED_QUESTION`]; 1.0 for judges that have no second step.
    pub outdated: f32,
}

#[derive(Clone, Debug)]
pub struct Verdict {
    pub relations: Vec<Relation>,
    pub durable: f32,
    pub worth: f32,
    pub why_ok: f32,
    pub judge: &'static str,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Insert,
    Duplicate(String),
    Supersede(Vec<String>),
    Drop,
}

/// Whether the relation passes both steps to close its neighbour.
pub fn closes(r: &Relation) -> bool {
    closing(&r.kind) && r.close >= CLOSE_MIN && r.outdated >= OUTDATED_MIN
}

/// `explicit`: asked for with `remember`, so its usefulness is not questioned.
pub fn outcome(v: &Verdict, explicit: bool) -> Outcome {
    let mut best: Option<&Relation> = None;
    for r in &v.relations {
        // Doubtful: merge nothing and close nothing; nobody reviews them by hand.
        if r.kind == "same" && r.p >= REL_MIN {
            return Outcome::Duplicate(r.neighbor.clone());
        }
        // `partly` never closes: what is still true in the existing memory would be lost. One
        // closing per candidate, the surest: a new memory that "closes" five is wrong about most.
        if closes(r) && best.is_none_or(|b| r.close > b.close) {
            best = Some(r);
        }
    }
    if !explicit && v.worth < WORTH_MIN {
        return Outcome::Drop;
    }
    match best {
        Some(r) => Outcome::Supersede(vec![r.neighbor.clone()]),
        None => Outcome::Insert,
    }
}

/// The second step, for the neighbours (by index) that passed the first: one request each, with
/// the candidate and that neighbour only.
pub fn outdated_requests(
    cand: &Memory,
    neighbors: &[Memory],
    which: &[usize],
) -> Vec<(Value, Value)> {
    which
        .iter()
        .filter_map(|&i| {
            let mut ex = mem_json(neighbors.get(i)?);
            ex["n"] = json!(i + 1);
            let st = json!({"candidate": mem_json(cand), "existing": [ex]});
            let q = json!({"outdated": {"type": "noul", "instructions": OUTDATED_QUESTION}});
            Some((st, q))
        })
        .collect()
}

/// Neighbours worth the second question: they pass the first step and are not events.
pub fn needs_outdated(v: &Verdict, neighbors: &[Memory]) -> Vec<usize> {
    v.relations
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            closing(&r.kind)
                && r.close >= CLOSE_MIN
                && neighbors.get(*i).is_some_and(|n| n.kind != NEVER_CLOSED)
        })
        .map(|(i, _)| i)
        .collect()
}

fn mem_json(m: &Memory) -> Value {
    json!({
        "kind": m.kind,
        "what": m.what,
        "why": m.why,
        "how": m.how,
        "when": m.occurred_from.map(fmt_day).unwrap_or_else(|| format!("recorded {}", fmt_day(m.known_at))),
    })
}

const REL_CRITERIA: [(&str, &str); 5] = [
    (
        "same",
        "Both state the same fact, event, decision or preference; the candidate adds nothing that changes it",
    ),
    (
        "updates",
        "Both are about the same single thing and the candidate is newer or more precise, so EVERYTHING the existing memory says is outdated",
    ),
    (
        "partly",
        "The candidate changes, rejects or completes only PART of what the existing memory says; other facts in the existing memory are still true",
    ),
    (
        "contradicts",
        "The candidate says the existing memory as a whole is no longer true: the problem it describes was fixed, the plan was dropped, the value or setting changed, the thing was removed",
    ),
    (
        "different",
        "They are about different facts, even if they mention the same things",
    ),
];

pub fn questions(
    cand: &Memory,
    neighbors: &[Memory],
    source: &str,
    explicit: bool,
) -> (Value, Value) {
    let mut state = json!({
        "candidate": mem_json(cand),
        "existing": neighbors.iter().enumerate().map(|(i, m)| { let mut v = mem_json(m); v["n"] = json!(i + 1); v }).collect::<Vec<_>>(),
    });
    if !source.is_empty() {
        state["source"] = json!(source);
    }
    let crit: Map<String, Value> = REL_CRITERIA
        .iter()
        .map(|(k, d)| (k.to_string(), json!(d)))
        .collect();
    let mut q = Map::new();
    for (i, _) in neighbors.iter().enumerate() {
        q.insert(
            format!("rel_{}", i + 1),
            json!({"type": "choice", "instructions": format!("How does the CANDIDATE memory relate to EXISTING memory number {}?", i + 1), "criteria": crit}),
        );
    }
    q.insert(
        "durable".into(),
        json!({"type": "noul", "instructions": "Will the CANDIDATE memory most likely still be true one month after it was recorded? Answer no for transient states such as an outage in progress, a deploy currently failing or a temporary workaround."}),
    );
    if !explicit {
        q.insert(
            "worth".into(),
            json!({"type": "noul", "instructions": "Would the CANDIDATE memory help an assistant in a different, future conversation with these people: a durable fact, a decision and its reason, a cause, a preference or a procedure? Answer no for chit-chat, one-off details or things only relevant to the moment."}),
        );
    }
    if cand.why.is_some() && !source.is_empty() {
        q.insert("why_ok".into(), json!({"type": "noul", "instructions": "Does the SOURCE text support the reason given in the CANDIDATE's why field?"}));
    }
    (state, Value::Object(q))
}

/// Split by what each question needs: the service makes one call per question with the whole state
/// (twice), so each relation goes alone with its own neighbour, without the source, and durable,
/// worth and why_ok go without the neighbours. Measured on 5 real candidates with 8 neighbours each:
/// 48 % fewer input tokens than all neighbours in one state, the same top answer in 36 of 40; the
/// close rule was validated with one neighbour per question.
pub fn decision_requests(
    cand: &Memory,
    neighbors: &[Memory],
    source: &str,
    explicit: bool,
) -> Vec<(Value, Value)> {
    let (state, q) = questions(cand, neighbors, source, explicit);
    let Value::Object(q) = q else {
        return vec![(state, Value::Object(Map::new()))];
    };
    let (rel, rest): (Map<String, Value>, Map<String, Value>) =
        q.into_iter().partition(|(k, _)| k.starts_with("rel_"));
    let mut out = Vec::new();
    for (k, question) in rel {
        // `rel_n` is about neighbour n, whatever order the map keeps.
        let n: usize = k["rel_".len()..].parse().unwrap_or(0);
        let one = state["existing"]
            .get(n.wrapping_sub(1))
            .cloned()
            .unwrap_or(Value::Null);
        let st = json!({"candidate": state["candidate"], "existing": [one]});
        let mut q = Map::new();
        q.insert(k, question);
        out.push((st, Value::Object(q)));
    }
    if !rest.is_empty() {
        let mut st = state;
        if let Some(o) = st.as_object_mut() {
            o.remove("existing");
        }
        out.push((st, Value::Object(rest)));
    }
    out
}

/// Measured on production entities and test pairs: same names score 0.72-0.92, different ones
/// (a system and one of its parts) ≤ 0.38. Without the decision service nothing merges by name.
pub const SAME_NAME_QUESTION: &str = "Do NAME A and NAME B refer to the same thing, where one is a translation, an abbreviation or another spelling of the other? Answer no if one is a part, a feature or a kind of the other, or if they are related but different things.";

pub async fn same_name(j: &Judge, kind: &str, a: &str, b: &str) -> Result<Option<f32>, String> {
    let Judge::Decisions(llm, model, timeout) = j else {
        return Ok(None);
    };
    let state = json!({"kind": kind, "NAME A": a, "NAME B": b});
    let q = json!({"same": {"type": "noul", "instructions": SAME_NAME_QUESTION}});
    let a = llm.decide(model, state, q, *timeout).await?;
    Ok(Some(noul(&a, "same", 0.0)))
}

pub const SOURCE_CHARS: usize = 1500;
const SEGMENT_CHARS: usize = 400;

pub fn segments(text: &str) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut rest: Vec<char> = line.chars().collect();
        while rest.len() > SEGMENT_CHARS {
            let window = &rest[..SEGMENT_CHARS];
            let cut = (SEGMENT_CHARS / 2..SEGMENT_CHARS)
                .rev()
                .find(|&i| matches!(window[i - 1], '.' | '!' | '?' | ';') && window[i] == ' ')
                .or_else(|| {
                    (SEGMENT_CHARS / 2..SEGMENT_CHARS)
                        .rev()
                        .find(|&i| window[i] == ' ')
                })
                .unwrap_or(SEGMENT_CHARS);
            pieces.push(rest[..cut].iter().collect::<String>().trim().to_string());
            rest = rest[cut..].to_vec();
        }
        let tail: String = rest.into_iter().collect::<String>().trim().to_string();
        if !tail.is_empty() {
            pieces.push(tail);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for p in pieces {
        match out.last_mut() {
            Some(last) if last.chars().count() + 1 + p.chars().count() <= SEGMENT_CHARS => {
                last.push('\n');
                last.push_str(&p);
            }
            _ => out.push(p),
        }
    }
    out
}

/// Share of the candidate's words found in the fragment: only meaningful in the same language,
/// so with an embedder cosine similarity is used instead.
pub fn lexical_score(cand: &Memory, seg: &str) -> f32 {
    let want: HashSet<String> = tokens(&cand.index_text()).into_iter().collect();
    if want.is_empty() {
        return 0.0;
    }
    let have: HashSet<String> = tokens(seg).into_iter().collect();
    want.intersection(&have).count() as f32 / want.len() as f32
}

pub fn pick(segs: &[String], scores: &[f32], limit: usize) -> String {
    let mut order: Vec<usize> = (0..segs.len()).collect();
    order.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let mut chosen = Vec::new();
    let mut used = 0;
    for i in order {
        let n = segs[i].chars().count() + if chosen.is_empty() { 0 } else { 3 };
        if used + n <= limit {
            used += n;
            chosen.push(i);
        }
    }
    chosen.sort_unstable();
    chosen
        .into_iter()
        .map(|i| segs[i].as_str())
        .collect::<Vec<_>>()
        .join(" … ")
}

fn noul(a: &Value, id: &str, default: f32) -> f32 {
    a.get(id)
        .and_then(|x| x.get("noul"))
        .and_then(|x| x.as_f64())
        .map(|x| x as f32)
        .unwrap_or(default)
}

/// A missing relation answer counts as doubtful, never as a yes; unasked questions default to 1.0.
pub fn parse_answers(answers: &Value, neighbors: &[Memory]) -> Verdict {
    let mut relations = Vec::new();
    for (i, n) in neighbors.iter().enumerate() {
        let a = answers.get(format!("rel_{}", i + 1));
        let probs = a
            .and_then(|a| a.get("probabilities"))
            .and_then(|p| p.as_object());
        let best = probs.and_then(|p| {
            p.iter()
                .filter_map(|(k, v)| v.as_f64().map(|v| (k.clone(), v as f32)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
        });
        let (kind, p) = match (
            best,
            a.and_then(|a| a.get("choice")).and_then(|c| c.as_str()),
        ) {
            (Some((k, p)), _) => (k, p),
            (None, Some(c)) => (
                c.to_string(),
                a.and_then(|a| a.get("confidence"))
                    .and_then(|c| c.as_f64())
                    .unwrap_or(0.0) as f32,
            ),
            (None, None) => ("different".to_string(), 0.0),
        };
        let close = match probs {
            Some(pr) => ["updates", "contradicts"]
                .iter()
                .filter_map(|k| pr.get(*k).and_then(|v| v.as_f64()))
                .sum::<f64>() as f32,
            None if closing(&kind) => p,
            None => 0.0,
        };
        relations.push(Relation {
            neighbor: n.id.clone(),
            kind,
            p,
            close,
            // asked afterwards, and only of those that pass the first step
            outdated: 0.0,
        });
    }
    Verdict {
        relations,
        durable: noul(answers, "durable", 1.0),
        worth: noul(answers, "worth", 1.0),
        why_ok: noul(answers, "why_ok", 1.0),
        judge: "decisions",
    }
}

fn closing(kind: &str) -> bool {
    matches!(kind, "updates" | "contradicts")
}

fn jaccard(a: &str, b: &str) -> f32 {
    let x: HashSet<String> = tokens(a).into_iter().collect();
    let y: HashSet<String> = tokens(b).into_iter().collect();
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    x.intersection(&y).count() as f32 / x.union(&y).count() as f32
}

/// The same fact told in two consecutive messages; farther apart, two occurrences.
pub const EVENT_SAME_MS: i64 = 2 * 60_000;

/// Below [`REL_MIN`]: the candidate is stored on its own instead of merging or closing anything.
const UNSURE: f32 = 0.5;

/// A duplicate only if the candidate adds NOTHING and no hard fact separates them (at 70% overlap
/// "cuts at 600 s" would merge with "at 120 s"). Never closes anything without a judge.
pub fn heuristic(cand: &Memory, neighbors: &[Memory]) -> Verdict {
    let relations = neighbors
        .iter()
        .map(|n| {
            let x: HashSet<String> = tokens(&cand.what).into_iter().collect();
            let y: HashSet<String> = tokens(&n.what).into_iter().collect();
            let adds_nothing = !x.is_empty() && x.is_subset(&y);
            let close = adds_nothing || jaccard(&cand.what, &n.what) >= 0.7;
            let compatible = crate::facts::diff(&cand.what, &n.what).compatible();
            let (kind, p) = match (close, compatible) {
                (true, false) => ("updates", UNSURE),
                (true, true) if adds_nothing => ("same", 0.9),
                _ => ("different", 0.9),
            };
            Relation {
                neighbor: n.id.clone(),
                kind: kind.into(),
                p,
                close: 0.0,
                outdated: 1.0,
            }
        })
        .collect();
    Verdict {
        relations,
        durable: if cand.kind == "state" { 0.0 } else { 1.0 },
        worth: 1.0,
        why_ok: 1.0,
        judge: "heuristic",
    }
}

/// Veto over ANY judge: memories whose hard facts differ are not the same. Merging erases one of
/// the two values for good; when in doubt, keep both.
pub fn guard(mut v: Verdict, cand: &Memory, neighbors: &[Memory]) -> Verdict {
    for r in &mut v.relations {
        let Some(n) = neighbors.iter().find(|n| n.id == r.neighbor) else {
            continue;
        };
        if n.kind == NEVER_CLOSED {
            r.close = 0.0;
        }
        if r.kind != "same" {
            continue;
        }
        // The same alert at 10:00 and 10:15 fired again: merging them would lose the recurrence.
        if cand.kind == "event"
            && n.kind == "event"
            && (cand.anchor_time() - n.anchor_time()).abs() > EVENT_SAME_MS
        {
            r.kind = "different".into();
            r.close = 0.0;
            continue;
        }
        if !crate::facts::diff(&cand.what, &n.what).compatible() {
            r.kind = "updates".into();
            r.p = r.p.min(UNSURE);
            r.close = 0.0;
        }
    }
    v
}

pub enum Judge {
    Decisions(Llm, String, Duration),
    Llm(Llm, String, Duration),
    Heuristic,
}

pub const LLM_JUDGE_PROMPT: &str = r#"You judge whether a CANDIDATE memory should be stored, given EXISTING memories about the same things and the SOURCE text it came from.
Answer with JSON only, no prose:
{"relations":[{"n":1,"relation":"same|updates|partly|contradicts|different","confidence":0.0}],"durable":0.0,"worth":0.0,"why_supported":0.0}
- relations: one entry per EXISTING memory, by its number n. same = states the same thing and adds nothing; updates = same single subject and the candidate is newer or more precise, so everything the existing one says is outdated; partly = the candidate changes or rejects only part of the existing one, the rest still holds; contradicts = the candidate says the existing one as a whole is no longer true; different = about something else even if it mentions the same things.
- durable: probability the candidate is still true one month after it was recorded (low for outages in progress, failing deploys, temporary workarounds).
- worth: probability it helps in a different future conversation (a durable fact, a decision and its reason, a cause, a preference, a procedure) rather than chit-chat or a one-off detail.
- why_supported: probability the SOURCE supports the candidate's why field (1.0 if there is no why).
Confidences are numbers between 0 and 1."#;

pub fn judge_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "relations": {"type": "array", "items": {"type": "object", "properties": {
                "n": {"type": "integer"},
                "relation": {"enum": ["same", "updates", "partly", "contradicts", "different"]},
                "confidence": {"type": "number"},
            }, "required": ["n", "relation", "confidence"]}},
            "durable": {"type": "number"},
            "worth": {"type": "number"},
            "why_supported": {"type": "number"},
        },
        "required": ["relations", "durable", "worth", "why_supported"],
    })
}

/// A missing relation counts as doubtful, an unknown one as "different"; missing scores are 1.0.
pub fn parse_llm(text: &str, neighbors: &[Memory]) -> Option<Verdict> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let v: Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    let num = |k: &str, d: f32| {
        v.get(k)
            .and_then(|x| x.as_f64())
            .map(|x| (x as f32).clamp(0.0, 1.0))
            .unwrap_or(d)
    };
    let rels = v
        .get("relations")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let relations = neighbors
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let r = rels
                .iter()
                .find(|r| r.get("n").and_then(|x| x.as_u64()) == Some(i as u64 + 1));
            let kind = r
                .and_then(|r| r.get("relation"))
                .and_then(|x| x.as_str())
                .filter(|k| REL_CRITERIA.iter().any(|(c, _)| c == k))
                .unwrap_or("different");
            let p = r
                .and_then(|r| r.get("confidence"))
                .and_then(|x| x.as_f64())
                .map(|x| x as f32)
                .unwrap_or(0.0);
            // A model's stated confidence is not a probability: it keeps the old bar to close.
            let close = if closing(kind) && p >= REL_MIN {
                p
            } else {
                0.0
            };
            Relation {
                neighbor: n.id.clone(),
                kind: kind.to_string(),
                p,
                close,
                // no second step: a stated confidence already needs 0.6
                outdated: 1.0,
            }
        })
        .collect();
    Some(Verdict {
        relations,
        durable: num("durable", 1.0),
        worth: num("worth", 1.0),
        why_ok: num("why_supported", 1.0),
        judge: "llm",
    })
}

impl Judge {
    pub fn backend(&self) -> Option<&Llm> {
        match self {
            Judge::Decisions(l, ..) | Judge::Llm(l, ..) => Some(l),
            Judge::Heuristic => None,
        }
    }
}

/// Falls back to the heuristic when the service does not answer: someone is waiting (`remember`).
pub async fn judge(
    j: &Judge,
    cand: &Memory,
    neighbors: &[Memory],
    source: &str,
    explicit: bool,
) -> Verdict {
    let (v, _) = judge_unguarded(j, cand, neighbors, source, explicit).await;
    guard(v, cand, neighbors)
}

/// Errors when the service does not answer: in the background a heuristic verdict would be a
/// silent downgrade, and the pass can simply wait.
pub async fn judge_strict(
    j: &Judge,
    cand: &Memory,
    neighbors: &[Memory],
    source: &str,
) -> Result<Verdict, String> {
    match judge_unguarded(j, cand, neighbors, source, false).await {
        (_, Some(e)) => Err(e),
        (v, None) => Ok(guard(v, cand, neighbors)),
    }
}

async fn judge_unguarded(
    j: &Judge,
    cand: &Memory,
    neighbors: &[Memory],
    source: &str,
    explicit: bool,
) -> (Verdict, Option<String>) {
    let failed = match j {
        Judge::Decisions(llm, model, timeout) => {
            let calls = decision_requests(cand, neighbors, source, explicit)
                .into_iter()
                .map(|(state, q)| llm.decide(model, state, q, *timeout));
            let mut answers = Map::new();
            let mut failed = None;
            for r in futures::future::join_all(calls).await {
                match r {
                    Ok(Value::Object(a)) => answers.extend(a),
                    Ok(_) => failed = Some("decisions: malformed response".to_string()),
                    Err(err) => failed = Some(format!("decisions did not answer ({err})")),
                }
            }
            match failed {
                None => {
                    let mut v = parse_answers(&Value::Object(answers), neighbors);
                    let which = needs_outdated(&v, neighbors);
                    let asks = outdated_requests(cand, neighbors, &which)
                        .into_iter()
                        .map(|(state, q)| llm.decide(model, state, q, *timeout));
                    let mut failed = None;
                    for (i, r) in which.iter().zip(futures::future::join_all(asks).await) {
                        match r {
                            Ok(a) => v.relations[*i].outdated = noul(&a, "outdated", 0.0),
                            Err(err) => failed = Some(format!("decisions did not answer ({err})")),
                        }
                    }
                    match failed {
                        None => return (v, None),
                        Some(e) => e,
                    }
                }
                Some(e) => e,
            }
        }
        Judge::Llm(llm, model, timeout) => {
            let (state, _) = questions(cand, neighbors, source, explicit);
            match llm
                .chat(
                    model,
                    LLM_JUDGE_PROMPT,
                    &format!("Judge this:\n{state}"),
                    600,
                    *timeout,
                    Some(("judgement", judge_schema())),
                )
                .await
            {
                Ok(text) => match parse_llm(&text, neighbors) {
                    Some(mut v) => {
                        if explicit {
                            v.worth = 1.0;
                        }
                        return (v, None);
                    }
                    // Answered badly: the model is at fault, not the service; waiting won't fix it.
                    None => {
                        log::warn!("the LLM judge returned no JSON; using the heuristic");
                        return (heuristic(cand, neighbors), None);
                    }
                },
                Err(err) => format!("the LLM judge did not answer ({err})"),
            }
        }
        Judge::Heuristic => return (heuristic(cand, neighbors), None),
    };
    log::warn!("{failed}; using the heuristic");
    (heuristic(cand, neighbors), Some(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, what: &str) -> Memory {
        Memory {
            id: id.into(),
            what: what.into(),
            kind: "fact".into(),
            known_at: 1,
            valid_from: 1,
            source_episodes: vec!["e".into()],
            ..Default::default()
        }
    }

    fn v(rel: &[(&str, &str, f32)], worth: f32) -> Verdict {
        Verdict {
            relations: rel
                .iter()
                .map(|(n, k, p)| Relation {
                    neighbor: n.to_string(),
                    kind: k.to_string(),
                    p: *p,
                    close: if closing(k) { *p } else { 0.0 },
                    outdated: 1.0,
                })
                .collect(),
            durable: 1.0,
            worth,
            why_ok: 1.0,
            judge: "t",
        }
    }

    #[test]
    fn outcomes_follow_the_verdict() {
        assert_eq!(outcome(&v(&[], 0.9), false), Outcome::Insert);
        assert_eq!(
            outcome(&v(&[("a", "same", 0.8)], 0.9), false),
            Outcome::Duplicate("a".into())
        );
        assert_eq!(
            outcome(
                &v(&[("a", "different", 0.9), ("b", "updates", 0.7)], 0.9),
                false
            ),
            Outcome::Supersede(vec!["b".into()])
        );
        assert_eq!(
            outcome(&v(&[("a", "contradicts", 0.35)], 0.9), false),
            Outcome::Insert,
            "a doubtful relation closes nothing"
        );
        assert_eq!(
            outcome(
                &v(
                    &[
                        ("a", "updates", 0.5),
                        ("b", "contradicts", 0.8),
                        ("c", "updates", 0.7)
                    ],
                    0.9
                ),
                false
            ),
            Outcome::Supersede(vec!["b".into()]),
            "one closing per candidate, the surest"
        );
        let mut doubt = v(&[("a", "updates", 0.9)], 0.9);
        doubt.relations[0].outdated = OUTDATED_MIN - 0.01;
        assert_eq!(
            outcome(&doubt, false),
            Outcome::Insert,
            "the second step says part of it still holds"
        );
        assert_eq!(
            outcome(&v(&[("a", "partly", 0.9)], 0.9), false),
            Outcome::Insert,
            "a partial change keeps the existing memory"
        );
        assert_eq!(outcome(&v(&[], 0.2), false), Outcome::Drop);
        assert_eq!(
            outcome(&v(&[], 0.2), true),
            Outcome::Insert,
            "an explicit request is not dropped for low worth"
        );
    }

    #[test]
    fn questions_cover_each_neighbor_and_skip_worth_when_explicit() {
        let mut c = m("c", "The LB timeout is 600 s.");
        c.why = Some("So it never cuts first.".into());
        let (state, q) = questions(&c, &[m("a", "x"), m("b", "y")], "source text", true);
        let q = q.as_object().unwrap();
        assert!(
            q.contains_key("rel_1")
                && q.contains_key("rel_2")
                && q.contains_key("durable")
                && q.contains_key("why_ok")
        );
        assert!(!q.contains_key("worth"));
        assert_eq!(state["existing"][1]["n"], 2);
        assert!(
            q["rel_1"]["criteria"].is_object(),
            "criteria as an object: every engine accepts it"
        );
    }

    #[test]
    fn relations_go_without_the_source_and_the_rest_without_the_neighbors() {
        let mut c = m("c", "The LB timeout is 600 s.");
        c.why = Some("So it never cuts first.".into());
        let reqs = decision_requests(&c, &[m("a", "x"), m("b", "y")], "source text", false);
        assert_eq!(reqs.len(), 3, "one per neighbour, then the rest");
        for (i, (rel_state, rel_q)) in reqs[..2].iter().enumerate() {
            let keys: Vec<&String> = rel_q.as_object().unwrap().keys().collect();
            assert_eq!(keys, vec![&format!("rel_{}", i + 1)]);
            assert!(rel_state.get("source").is_none());
            let existing = rel_state["existing"].as_array().unwrap();
            assert_eq!(existing.len(), 1, "only its own neighbour");
            assert_eq!(existing[0]["n"], i + 1);
            assert_eq!(existing[0]["what"], ["x", "y"][i]);
        }
        let (rest_state, rest_q) = &reqs[2];
        let rest_keys: Vec<&String> = rest_q.as_object().unwrap().keys().collect();
        assert!(rest_keys.iter().all(|k| !k.starts_with("rel_")) && rest_keys.len() == 3);
        assert!(rest_state.get("existing").is_none() && rest_state["source"] == "source text");
        assert_eq!(decision_requests(&c, &[], "source text", false).len(), 1);
    }

    #[test]
    fn the_source_shown_to_the_judge_is_the_passage_that_matters_not_the_start() {
        let filler = "The team talked about the weather and lunch plans for a while. ".repeat(60);
        let text = format!(
            "{filler}\nThe gateway timeout was raised to 600 s because the load balancer cut long streams at 120 s.\n{filler}"
        );
        let segs = segments(&text);
        assert!(segs.iter().all(|s| s.chars().count() <= SEGMENT_CHARS));
        let mut c = m("c", "The gateway timeout was raised to 600 s.");
        c.why = Some("The load balancer cut long streams at 120 s.".into());
        let scores: Vec<f32> = segs.iter().map(|s| lexical_score(&c, s)).collect();
        let shown = pick(&segs, &scores, SOURCE_CHARS);
        assert!(shown.chars().count() <= SOURCE_CHARS);
        assert!(shown.contains("cut long streams at 120 s"), "{shown}");
        assert!(
            !text
                .chars()
                .take(2000)
                .collect::<String>()
                .contains("120 s")
        );
    }

    #[test]
    fn picked_fragments_keep_their_original_order() {
        let segs: Vec<String> = ["a1", "b2", "c3", "d4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick(&segs, &[0.9, 0.1, 0.8, 0.2], 8), "a1 … c3");
    }

    #[test]
    fn answers_are_read_and_missing_ones_are_doubtful() {
        let ans = json!({
            "rel_1": {"type": "choice", "choice": "updates", "probabilities": {"same": 0.1, "updates": 0.8, "contradicts": 0.05, "different": 0.05}},
            "durable": {"type": "noul", "noul": 0.2},
            "worth": {"type": "noul", "noul": 0.9}
        });
        let v = parse_answers(&ans, &[m("a", "x"), m("b", "y")]);
        assert_eq!(
            v.relations[0],
            Relation {
                neighbor: "a".into(),
                kind: "updates".into(),
                p: 0.8,
                close: 0.85,
                outdated: 0.0
            }
        );
        assert_eq!(v.relations[1].p, 0.0, "no answer means no certainty");
        assert_eq!(
            outcome(&v, false),
            Outcome::Insert,
            "nothing closes before the second step"
        );
        let mut v = v;
        v.relations[0].outdated = 0.2;
        assert_eq!(
            outcome(&v, false),
            Outcome::Supersede(vec!["a".into()]),
            "the certain one applies; the doubt about b neither blocks anything nor touches b"
        );
        assert!((v.durable - 0.2).abs() < 1e-6);
    }

    #[test]
    fn only_a_top_answer_of_updates_or_contradicts_goes_to_the_second_step() {
        let ns = [m("a", "x")];
        let rel = |p: Value| {
            parse_answers(
                &json!({"rel_1": {"type": "choice", "probabilities": p}}),
                &ns,
            )
        };
        let mut upd = rel(
            json!({"same": 0.03, "updates": 0.05, "partly": 0.38, "contradicts": 0.51, "different": 0.03}),
        );
        assert_eq!(needs_outdated(&upd, &ns), vec![0]);
        upd.relations[0].outdated = 0.1;
        assert_eq!(outcome(&upd, false), Outcome::Supersede(vec!["a".into()]));
        // The sum alone closed this in 0.3.0: on real pairs, a top answer of `partly` was mostly a
        // memory that is still true.
        let both = rel(
            json!({"same": 0.04, "updates": 0.42, "partly": 0.5, "contradicts": 0.02, "different": 0.01}),
        );
        assert!(needs_outdated(&both, &ns).is_empty());
        assert_eq!(outcome(&both, false), Outcome::Insert);
        let same = rel(
            json!({"same": 0.7, "updates": 0.2, "partly": 0.05, "contradicts": 0.0, "different": 0.05}),
        );
        assert_eq!(outcome(&same, false), Outcome::Duplicate("a".into()));
    }

    #[test]
    fn events_are_never_closed_and_the_second_step_asks_one_neighbour_at_a_time() {
        let mut ev = m("e", "vLLM was upgraded to 0.34 on every node.");
        ev.kind = "event".into();
        let ns = [m("a", "x"), ev, m("b", "y")];
        let cand = m("c", "vLLM was rolled back to 0.33.");
        let mut verdict = v(
            &[
                ("a", "updates", 0.6),
                ("e", "contradicts", 0.9),
                ("b", "different", 0.9),
            ],
            0.9,
        );
        assert_eq!(
            needs_outdated(&verdict, &ns),
            vec![0],
            "not the event, not `different`"
        );
        let reqs = outdated_requests(&cand, &ns, &[0]);
        assert_eq!(reqs.len(), 1);
        let (state, q) = &reqs[0];
        assert_eq!(state["existing"].as_array().unwrap().len(), 1);
        assert_eq!(state["existing"][0]["n"], 1);
        assert_eq!(q["outdated"]["type"], "noul");
        verdict = guard(verdict, &cand, &ns);
        assert_eq!(verdict.relations[1].close, 0.0);
        assert_eq!(
            outcome(&verdict, false),
            Outcome::Supersede(vec!["a".into()]),
            "the event stays, even with a higher score"
        );
    }

    #[test]
    fn llm_verdicts_are_read_and_unknowns_are_doubtful() {
        let ns = [m("a", "x"), m("b", "y")];
        let v = parse_llm("Sure:\n{\"relations\":[{\"n\":2,\"relation\":\"updates\",\"confidence\":0.9},{\"n\":1,\"relation\":\"bogus\",\"confidence\":0.9}],\"durable\":0.3,\"worth\":1.7}", &ns).unwrap();
        assert_eq!(
            v.relations[0],
            Relation {
                neighbor: "a".into(),
                kind: "different".into(),
                p: 0.9,
                close: 0.0,
                outdated: 1.0
            },
            "an invented relation does not count"
        );
        assert_eq!(
            v.relations[1],
            Relation {
                neighbor: "b".into(),
                kind: "updates".into(),
                p: 0.9,
                close: 0.9,
                outdated: 1.0
            }
        );
        assert!((v.durable - 0.3).abs() < 1e-6 && v.worth == 1.0 && v.why_ok == 1.0);
        assert_eq!(v.judge, "llm");
        assert!(parse_llm("no json", &ns).is_none());
        let empty = parse_llm("{}", &ns).unwrap();
        assert_eq!(
            outcome(&empty, false),
            Outcome::Insert,
            "without clear relations, it goes to candidates"
        );
    }

    #[test]
    fn heuristic_only_merges_near_identical_text() {
        let c = m("c", "The gateway LB timeout is 600 seconds.");
        let v = heuristic(
            &c,
            &[
                m("a", "The gateway LB timeout is 600 seconds"),
                m("b", "Nomad runs three servers."),
            ],
        );
        assert_eq!(v.relations[0].kind, "same");
        assert_eq!(v.relations[1].kind, "different");
    }

    #[test]
    fn a_changed_number_is_not_swallowed_as_a_duplicate() {
        // 75% shared words must not make this "same": the new value would be lost.
        let c = m(
            "c",
            "The gateway load balancer times out streams at 600 seconds",
        );
        let old = m(
            "a",
            "The gateway load balancer times out streams at 120 seconds",
        );
        let v = heuristic(&c, std::slice::from_ref(&old));
        assert_eq!(v.relations[0].kind, "updates");
        assert_eq!(
            outcome(&v, true),
            Outcome::Insert,
            "goes to review, without closing the old one"
        );
        let on = m(
            "on",
            "The nightly backup job for billing is enabled in production",
        );
        let off = m(
            "off",
            "The nightly backup job for billing is disabled in production",
        );
        assert_eq!(
            outcome(&heuristic(&off, std::slice::from_ref(&on)), true),
            Outcome::Insert
        );
    }

    #[test]
    fn a_candidate_that_adds_words_is_not_a_duplicate_without_a_judge() {
        let c = m(
            "c",
            "The gateway LB timeout is 600 seconds for streaming only",
        );
        let v = heuristic(&c, &[m("a", "The gateway LB timeout is 600 seconds")]);
        assert_eq!(v.relations[0].kind, "different", "adds words: not merged");
    }

    #[test]
    fn the_same_alert_at_another_time_is_another_event() {
        let at = |id: &str, ms: i64| {
            let mut x = m(id, "HighLatency firing on the gateway");
            x.kind = "event".into();
            x.occurred_from = Some(ms);
            x
        };
        let first = at("a", 10 * 3_600_000);
        let again = at("c", 10 * 3_600_000 + 15 * 60_000);
        let v = guard(
            heuristic(&again, std::slice::from_ref(&first)),
            &again,
            std::slice::from_ref(&first),
        );
        assert_eq!(v.relations[0].kind, "different");
        assert_eq!(
            outcome(&v, false),
            Outcome::Insert,
            "stored as a new occurrence"
        );
        let echo = at("e", 10 * 3_600_000 + 30_000);
        let v = guard(
            heuristic(&echo, std::slice::from_ref(&first)),
            &echo,
            std::slice::from_ref(&first),
        );
        assert_eq!(outcome(&v, false), Outcome::Duplicate("a".into()));
    }

    #[test]
    fn the_guard_overrules_a_model_that_calls_different_facts_the_same() {
        let c = m("c", "The PostgreSQL pool is capped at 50 connections");
        let n = m("a", "The PostgreSQL pool is capped at 20 connections");
        let para = m("p", "The PostgreSQL pool has a cap of 50 connections");
        let model = v(&[("a", "same", 0.97), ("p", "same", 0.9)], 1.0);
        let g = guard(model, &c, &[n, para]);
        assert_eq!(g.relations[0].kind, "updates");
        assert!(g.relations[0].p < REL_MIN);
        assert_eq!(
            g.relations[1].kind, "same",
            "with no facts telling them apart, the judge decides"
        );
        assert_eq!(
            outcome(&g, false),
            Outcome::Duplicate("p".into()),
            "the vetoed neighbor is left alone; the compatible one is recognized"
        );
    }
}
