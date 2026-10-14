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
pub const WORTH_MIN: f32 = 0.45;
/// Below this, the candidate is treated as a transient state.
pub const DURABLE_MIN: f32 = 0.5;
pub const WHY_MIN: f32 = 0.4;

#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub neighbor: String,
    /// same | updates | contradicts | different
    pub kind: String,
    pub p: f32,
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

/// `explicit`: asked for with `remember`, so its usefulness is not questioned.
pub fn outcome(v: &Verdict, explicit: bool) -> Outcome {
    let mut supersede = Vec::new();
    for r in &v.relations {
        if r.p < REL_MIN {
            // Doubtful: merge nothing and close nothing; nobody reviews them by hand.
            continue;
        }
        match r.kind.as_str() {
            "same" => return Outcome::Duplicate(r.neighbor.clone()),
            "updates" | "contradicts" => supersede.push(r.neighbor.clone()),
            _ => {}
        }
    }
    if !explicit && v.worth < WORTH_MIN {
        return Outcome::Drop;
    }
    if supersede.is_empty() {
        Outcome::Insert
    } else {
        Outcome::Supersede(supersede)
    }
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

const REL_CRITERIA: [(&str, &str); 4] = [
    (
        "same",
        "Both state the same fact, event, decision or preference; the candidate adds nothing that changes it",
    ),
    (
        "updates",
        "Both are about the same thing and the candidate is newer or more precise, so the existing memory is outdated",
    ),
    (
        "contradicts",
        "The candidate says the existing memory is no longer true: the problem it describes was fixed, the plan was dropped, the value or setting changed, the thing was removed",
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

/// Split by what each question needs: the service resends the whole state per question, and
/// relations do not need the source while durable, worth and why_ok do not need the neighbours.
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
    if !rel.is_empty() {
        let mut st = state.clone();
        if let Some(o) = st.as_object_mut() {
            o.remove("source");
        }
        out.push((st, Value::Object(rel)));
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
        relations.push(Relation {
            neighbor: n.id.clone(),
            kind,
            p,
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
        if r.kind != "same" {
            continue;
        }
        let Some(n) = neighbors.iter().find(|n| n.id == r.neighbor) else {
            continue;
        };
        // The same alert at 10:00 and 10:15 fired again: merging them would lose the recurrence.
        if cand.kind == "event"
            && n.kind == "event"
            && (cand.anchor_time() - n.anchor_time()).abs() > EVENT_SAME_MS
        {
            r.kind = "different".into();
            continue;
        }
        if !crate::facts::diff(&cand.what, &n.what).compatible() {
            r.kind = "updates".into();
            r.p = r.p.min(UNSURE);
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
{"relations":[{"n":1,"relation":"same|updates|contradicts|different","confidence":0.0}],"durable":0.0,"worth":0.0,"why_supported":0.0}
- relations: one entry per EXISTING memory, by its number n. same = states the same thing and adds nothing; updates = same subject but the candidate is newer or more precise, so the existing one is outdated; contradicts = the candidate says the existing one is no longer true; different = about something else even if it mentions the same things.
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
                "relation": {"enum": ["same", "updates", "contradicts", "different"]},
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
            Relation {
                neighbor: n.id.clone(),
                kind: kind.to_string(),
                p,
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
                None => return (parse_answers(&Value::Object(answers), neighbors), None),
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
            outcome(&v(&[("a", "contradicts", 0.5)], 0.9), false),
            Outcome::Insert,
            "a doubtful relation closes nothing"
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
        assert_eq!(reqs.len(), 2);
        let (rel_state, rel_q) = &reqs[0];
        let (rest_state, rest_q) = &reqs[1];
        assert!(
            rel_q
                .as_object()
                .unwrap()
                .keys()
                .all(|k| k.starts_with("rel_"))
        );
        assert_eq!(rel_q.as_object().unwrap().len(), 2);
        assert!(rel_state.get("source").is_none() && rel_state["existing"].is_array());
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
                p: 0.8
            }
        );
        assert_eq!(v.relations[1].p, 0.0, "no answer means no certainty");
        assert_eq!(
            outcome(&v, false),
            Outcome::Supersede(vec!["a".into()]),
            "the certain one applies; the doubt about b neither blocks anything nor touches b"
        );
        assert!((v.durable - 0.2).abs() < 1e-6);
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
                p: 0.9
            },
            "an invented relation does not count"
        );
        assert_eq!(
            v.relations[1],
            Relation {
                neighbor: "b".into(),
                kind: "updates".into(),
                p: 0.9
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
