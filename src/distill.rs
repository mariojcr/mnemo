//! Distillation: from raw episodes to six-W memories. Everything the model returns is validated
//! here: a memory with no supporting episode is dropped, and so is a link to an unknown entity.

use crate::episode::Episode;
use crate::model::{Entity, KINDS, Link, Memory, Role};
use crate::query::{DAY_MS, parse_iso};
use crate::text::norm;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

pub const ENTITY_KINDS: [&str; 12] = [
    "person", "team", "system", "service", "env", "repo", "file", "symbol", "channel", "project",
    "org", "concept",
];

pub const EPISODE_CHARS: usize = crate::episode::MAX_CHUNK;

pub const SYSTEM_PROMPT: &str = r#"You turn transcripts of a team's work with AI assistants into durable memories. Each memory answers six questions: what happened or is true, who is involved, when, where (system, environment, repo, file, channel, project), why, and how.

Rules:
- Only extract what would help in a FUTURE, different conversation: facts about systems and people, decisions and their reasons, causes of problems and their fixes, procedures, preferences. Skip greetings, chit-chat, the assistant's own process and anything only relevant to the moment.
- Write `what`, `why` and `how` in English as short self-contained sentences that make sense without the transcript. Name people and systems; never write "the user" or "the assistant".
- `why` and `how` are null unless the transcript states them. Never invent a reason or a fix. For decisions, fixes and procedures the transcript usually does give them: fill them in, even briefly.
- `who` is only for people, teams or named bots that acted, decided or were affected; use the speaker names given. Systems, services and tools go in `where` or `about`, never in `who`.
- Each line says who speaks. `user <name>` is that person. `assistant` is the AI assistant and `tool` is automated output (a background task, a subagent report): neither is the person, even in the person's own session. What an assistant or tool line proposes, concludes, measures or reports is never the person's decision, preference or statement: store it as a fact or a state, and never write that a person decided, agreed or chose it. `decided_by` is only for a person whose own line makes or approves the choice. A question from a person is not a decision.
- A proposal, recommendation or option that someone (often the assistant) puts forward is not a decision and not something that happened. Store it as a decision only when a person accepts it or the transcript shows it was applied; otherwise skip it.
- When a person drops, parks or rejects a plan or an idea ("not worth it", "let's leave it"), store that as a decision that names the plan and the reason, so the earlier plan can be closed. Dropping something is as important to remember as starting it.
- Do not store a work log: that a commit was pushed, an image built or deployed, a file or a setting edited, a check run. Store a change only for what it leaves that later work needs: a new behaviour, a cause, a procedure, a constraint.
- Never store progress snapshots: counts ("52 memories"), "X is stable", "a review passed", "tests are green", what someone asked for or decided to look at. Keep only what stays useful: the decision, the cause, the fix, the procedure.
- When the transcript says a problem was fixed or something stopped being true, write it as its own event that names the same thing and what changed ("The Forwarded header spoofing in the gateway was fixed by stripping those headers"), so the old memory can be closed.
- For who/where/about use the KNOWN ENTITIES by id, also when the transcript names one in another language or another way ("juez" is the known "Judge"). Name a new entity in English, keep proper names as they are, and put the transcript's own term in `aliases` (name "judge", aliases ["juez"]). If something important is missing from the list, declare it in `entities` with a ref like "new:1" and use that ref. Never declare temporary files, run or request ids, numbers, error codes or generic words as entities.
- `kind`: event (something that happened), fact (true now), state (true now but transient: an outage, a failing deploy, a temporary workaround, a known problem not fixed yet), decision (a choice a person made or approved in their own line, with its reason), preference, procedure. A finding or a vulnerability is a state, not a decision.
- Dates are ISO `YYYY-MM-DD` or full ISO timestamps. Resolve relative dates ("yesterday") against the episode timestamps. `occurred_from` is when it happened; `valid_until` only for states with a known end.
- `episodes` lists the ids of the NEW episodes that support the memory. A memory without supporting episodes is discarded.
- Prefer a few precise memories over many vague ones. Do not repeat what the EARLIER CONTEXT already covered.

Answer with JSON only, no prose and no code fences:
{"entities":[{"ref":"new:1","kind":"system|service|person|team|repo|file|channel|project|env|concept","name":"...","aliases":["..."],"parent":null}],
 "memories":[{"kind":"event","what":"...","why":null,"how":null,"who":[{"entity":"<id>","subrole":"actor|affected|owner|decided_by"}],"where":["<id>"],"about":["<id>"],"occurred_from":null,"occurred_to":null,"valid_until":null,"episodes":["<episode id>"]}]}
Return {"entities":[],"memories":[]} when nothing is worth remembering."#;

pub fn output_schema() -> serde_json::Value {
    let text = serde_json::json!({"type": ["string", "null"]});
    let ids = serde_json::json!({"type": "array", "items": {"type": "string"}});
    serde_json::json!({
        "type": "object",
        "properties": {
            "entities": {"type": "array", "items": {"type": "object", "properties": {
                "ref": {"type": "string"}, "kind": {"type": "string"}, "name": {"type": "string"},
                "aliases": ids, "parent": text,
            }, "required": ["ref", "kind", "name"]}},
            "memories": {"type": "array", "items": {"type": "object", "properties": {
                "kind": {"enum": KINDS},
                "what": {"type": "string"}, "why": text, "how": text,
                "who": {"type": "array", "items": {"type": "object", "properties": {
                    "entity": {"type": "string"}, "subrole": {"type": "string"},
                }, "required": ["entity"]}},
                "where": ids, "about": ids,
                "occurred_from": text, "occurred_to": text, "valid_until": text,
                "episodes": ids,
            }, "required": ["kind", "what", "episodes"]}},
        },
        "required": ["entities", "memories"],
    })
}

#[derive(Deserialize, Debug, Default)]
pub struct XOut {
    #[serde(default)]
    pub entities: Vec<XEnt>,
    #[serde(default)]
    pub memories: Vec<XMem>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct XEnt {
    #[serde(rename = "ref", default)]
    pub reference: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub parent: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum XRole {
    Id(String),
    Full {
        entity: String,
        #[serde(default)]
        subrole: String,
    },
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct XMem {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub what: String,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub how: Option<String>,
    #[serde(default)]
    pub who: Vec<XRole>,
    #[serde(default, rename = "where")]
    pub where_: Vec<XRole>,
    #[serde(default)]
    pub about: Vec<XRole>,
    #[serde(default)]
    pub occurred_from: Option<String>,
    #[serde(default)]
    pub occurred_to: Option<String>,
    #[serde(default)]
    pub valid_until: Option<String>,
    #[serde(default)]
    pub episodes: Vec<String>,
}

/// Models sometimes wrap the JSON in prose or in ``` fences.
pub fn parse_output(text: &str) -> Result<XOut, String> {
    let start = text.find('{').ok_or("the extraction returned no JSON")?;
    let end = text.rfind('}').ok_or("the extraction returned no JSON")?;
    if end < start {
        return Err("the extraction returned no JSON".into());
    }
    serde_json::from_str(&text[start..=end]).map_err(|e| format!("invalid extraction JSON: {e}"))
}

pub fn slug(s: &str) -> String {
    let n = norm(s);
    let mut out = String::new();
    let mut dash = false;
    for c in n.chars() {
        if c.is_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').chars().take(80).collect()
}

pub fn entity_id(kind: &str, name: &str, scope: &str) -> String {
    if scope == "global" {
        format!("{kind}:{}", slug(name))
    } else {
        format!("{kind}:{}@{scope}", slug(name))
    }
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty() && x != "null" && x.len() > 2)
}

pub struct Ctx<'a> {
    pub scope: &'a str,
    pub owner: &'a str,
    pub now: i64,
    /// The NEW episodes: the only ones that can support a memory.
    pub episodes: &'a [Episode],
    pub known: &'a HashMap<String, Entity>,
}

/// `resolve(name, kind)` finds an existing entity by exact name, so a "new" entity that already
/// exists is linked to it, not duplicated.
pub fn build(
    out: XOut,
    cx: &Ctx,
    resolve: &dyn Fn(&str, &str) -> Option<String>,
) -> (Vec<Entity>, Vec<Memory>) {
    let ep_by_id: HashMap<&str, &Episode> = cx
        .episodes
        .iter()
        .map(|e| (e.episode_id.as_str(), e))
        .collect();
    let mut refs: HashMap<String, String> = HashMap::new();
    let mut new_ents: HashMap<String, Entity> = HashMap::new();
    for x in &out.entities {
        let name = x.name.trim();
        if name.chars().count() < 2 || name.chars().count() > 80 || x.reference.is_empty() {
            continue;
        }
        if name
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == '.')
            || name.starts_with("/tmp")
        {
            continue;
        }
        let kind = if ENTITY_KINDS.contains(&x.kind.as_str()) {
            x.kind.clone()
        } else {
            "concept".to_string()
        };
        if let Some(id) =
            resolve(name, &kind).or_else(|| x.aliases.iter().find_map(|a| resolve(a, &kind)))
        {
            refs.insert(x.reference.clone(), id);
            continue;
        }
        let id = entity_id(&kind, name, cx.scope);
        let aliases: Vec<String> = x
            .aliases
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| a.chars().count() >= 2 && a.chars().count() <= 80)
            .take(8)
            .collect();
        refs.insert(x.reference.clone(), id.clone());
        new_ents.insert(
            id.clone(),
            Entity {
                id,
                scope: cx.scope.to_string(),
                kind,
                name: name.to_string(),
                aliases,
                parent: x.parent.clone(),
                summary: String::new(),
                source: "extracted".into(),
                same_as: None,
            },
        );
    }
    let ids: HashSet<String> = new_ents.keys().cloned().collect();
    for e in new_ents.values_mut() {
        e.parent = e
            .parent
            .take()
            .and_then(|p| refs.get(&p).cloned().or(Some(p)))
            .filter(|p| cx.known.contains_key(p) || ids.contains(p));
    }
    let resolve_ref = |r: &str| -> Option<String> {
        let r = r.trim();
        if cx.known.contains_key(r) {
            return Some(r.to_string());
        }
        refs.get(r).cloned()
    };

    let mut mems = Vec::new();
    let mut used_new: HashSet<String> = HashSet::new();
    for x in out.memories {
        let what = x.what.trim().to_string();
        if what.chars().count() < 8 {
            continue;
        }
        let cited: Vec<&Episode> = x
            .episodes
            .iter()
            .filter_map(|id| ep_by_id.get(id.trim()).copied())
            .collect();
        if cited.is_empty() {
            continue;
        }
        let mut links: Vec<Link> = Vec::new();
        let push = |entity: String, role: Role, subrole: String, links: &mut Vec<Link>| {
            if !links.iter().any(|l| l.entity == entity && l.role == role) {
                links.push(Link {
                    entity,
                    role,
                    subrole,
                });
            }
        };
        for (list, role) in [
            (&x.who, Role::Who),
            (&x.where_, Role::Where),
            (&x.about, Role::About),
        ] {
            for r in list {
                let (id, sub) = match r {
                    XRole::Id(id) => (id.clone(), String::new()),
                    XRole::Full { entity, subrole } => (entity.clone(), subrole.clone()),
                };
                if let Some(e) = resolve_ref(&id) {
                    push(e, role, sub, &mut links);
                }
            }
        }
        for ep in &cited {
            for e in &ep.entities {
                if cx.known.contains_key(e)
                    && !links.iter().any(|l| &l.entity == e)
                    && !e.starts_with("person:")
                {
                    push(e.clone(), Role::Where, "context".into(), &mut links);
                }
            }
        }
        // No person's own line among the sources: a conclusion, credited to nobody. The prompt
        // rule alone is not enough: models still write "X decided" about what the assistant said.
        let by_person = cited.iter().any(|e| {
            (e.role.is_empty() || e.role == "user") && !crate::ops::is_internal_prompt(&e.text)
        });
        let mut kind = x.kind.clone();
        if !by_person {
            links.retain(|l| {
                !(l.role == Role::Who
                    && l.subrole == "decided_by"
                    && (l.entity.starts_with("person:")
                        || cx.known.get(&l.entity).is_some_and(|e| e.kind == "person")))
            });
            if kind == "decision" {
                kind = "fact".into();
            }
        }
        for l in &links {
            if new_ents.contains_key(&l.entity) {
                used_new.insert(l.entity.clone());
            }
        }
        let first = cited.iter().map(|e| e.occurred_at).min().unwrap_or(cx.now);
        let last = cited.iter().map(|e| e.occurred_at).max().unwrap_or(cx.now);
        let kind = if KINDS.contains(&kind.as_str()) {
            kind
        } else {
            "fact".to_string()
        };
        let cited_at: Vec<i64> = cited.iter().map(|e| e.occurred_at).collect();
        let mut occurred_from = x
            .occurred_from
            .as_deref()
            .and_then(|raw| parse_iso(raw).map(|t| at_episode_time(raw, t, &cited_at, false)))
            .filter(|t| *t <= cx.now + DAY_MS);
        if occurred_from.is_none() && matches!(kind.as_str(), "event" | "decision") {
            occurred_from = Some(first);
        }
        let occurred_to = x
            .occurred_to
            .as_deref()
            .and_then(|raw| parse_iso(raw).map(|t| at_episode_time(raw, t, &cited_at, true)))
            .filter(|t| occurred_from.is_none_or(|f| *t >= f));
        let valid_from = occurred_from.unwrap_or(last).min(last);
        let valid_to = x
            .valid_until
            .as_deref()
            .and_then(parse_iso)
            .filter(|t| *t > valid_from);
        let mut episodes: Vec<String> = cited.iter().map(|e| e.episode_id.clone()).collect();
        episodes.dedup();
        mems.push(Memory {
            id: uuid::Uuid::new_v4().to_string(),
            scope: cx.scope.to_string(),
            owner_id: cx.owner.to_string(),
            kind,
            what,
            why: clean(x.why),
            how: clean(x.how),
            occurred_from,
            occurred_to,
            known_at: last,
            valid_from,
            valid_to,
            superseded_by: None,
            confidence: 0.0,
            source_episodes: episodes,
            links,
            judge: String::new(),
            embedding: None,
        });
    }
    let ents = new_ents
        .into_values()
        .filter(|e| used_new.contains(&e.id))
        .collect();
    (ents, mems)
}

/// Models usually give a bare date, which parses to midnight and loses the order within the day:
/// use the time of a cited episode on that day (the first for a start, the last for an end).
fn at_episode_time(raw: &str, t: i64, cited: &[i64], end: bool) -> i64 {
    if raw.trim().len() > 10 {
        return t;
    }
    let same_day = cited.iter().copied().filter(|c| *c >= t && *c < t + DAY_MS);
    let picked = if end { same_day.max() } else { same_day.min() };
    picked.unwrap_or(t)
}

/// The judge only compares against what is already stored, so same-pass duplicates merge here.
pub fn dedupe_within(mems: Vec<Memory>) -> Vec<Memory> {
    let mut out: Vec<Memory> = Vec::new();
    for m in mems {
        let key: HashSet<String> = crate::text::tokens(&m.what).into_iter().collect();
        let dup = out.iter_mut().find(|o| {
            let k: HashSet<String> = crate::text::tokens(&o.what).into_iter().collect();
            !key.is_empty()
                && key.intersection(&k).count() as f32 / key.union(&k).count() as f32 >= 0.7
        });
        match dup {
            Some(o) => {
                for e in m.source_episodes {
                    if !o.source_episodes.contains(&e) {
                        o.source_episodes.push(e);
                    }
                }
                if o.why.is_none() {
                    o.why = m.why;
                }
                if o.how.is_none() {
                    o.how = m.how;
                }
            }
            None => out.push(m),
        }
    }
    out
}

/// Returns the message and how many new episodes fit; the rest wait for the next pass. At least
/// one always goes in (truncated if needed), or a huge episode would stall the thread forever.
pub fn prompt(
    known: &[&Entity],
    before: &[Episode],
    new: &[Episode],
    names: &HashMap<String, String>,
    budget_chars: usize,
    max_chars: usize,
) -> (String, usize) {
    let mut s = String::from("KNOWN ENTITIES (id | kind | name | aliases):\n");
    for e in known {
        s.push_str(&format!(
            "{} | {} | {} | {}\n",
            e.id,
            e.kind,
            e.name,
            e.aliases.join(", ")
        ));
    }
    let line = |ep: &Episode, max: usize| {
        // Only the person's own lines carry their name: clients stamp assistant replies and task
        // notifications with the person too, and the extractor would credit them with those.
        let role = if ep.role == "user" && crate::ops::is_internal_prompt(&ep.text) {
            "tool"
        } else {
            ep.role.as_str()
        };
        let who = if role.is_empty() || role == "user" {
            format!(
                " {}",
                names
                    .get(&ep.actor_id)
                    .cloned()
                    .unwrap_or_else(|| ep.actor_id.clone())
            )
        } else {
            String::new()
        };
        let text: String = ep.text.chars().take(max).collect();
        format!(
            "[{}] {} {}{}{}: {}\n",
            ep.episode_id,
            crate::episode::fmt_ts(ep.occurred_at),
            role,
            who,
            if ep.agent.is_empty() {
                String::new()
            } else {
                format!(" ({})", ep.agent)
            },
            text
        )
    };
    if !before.is_empty() {
        s.push_str("\nEARLIER CONTEXT (already processed; do not extract from it):\n");
        for ep in before {
            s.push_str(&line(ep, 800));
        }
    }
    s.push_str("\nNEW EPISODES:\n");
    let mut budget = budget_chars.saturating_sub(s.len()).max(2_000);
    let mut included = 0;
    for ep in new {
        let mut l = line(ep, max_chars);
        if l.len() > budget {
            if included > 0 {
                break;
            }
            l = line(ep, budget.saturating_sub(300).max(300));
        }
        budget = budget.saturating_sub(l.len());
        s.push_str(&l);
        included += 1;
    }
    (s, included)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_ingested_chunk_reaches_the_extractor() {
        let text = format!("{}END-OF-CHUNK", "x".repeat(crate::episode::MAX_CHUNK - 12));
        let ep = Episode {
            episode_id: "e1".into(),
            text,
            ..Default::default()
        };
        let (s, n) = prompt(&[], &[], &[ep], &HashMap::new(), 18_000, EPISODE_CHARS);
        assert_eq!(n, 1);
        assert!(
            s.contains("END-OF-CHUNK"),
            "the end of the chunk does not reach the model"
        );
    }

    #[test]
    fn the_prompt_fits_the_budget_and_says_how_many_episodes_went_in() {
        let ep = |id: &str, text: &str| Episode {
            episode_id: id.into(),
            text: text.into(),
            ..Default::default()
        };
        let big = "x".repeat(5_000);
        let new = [ep("e1", &big), ep("e2", &big), ep("e3", &"y".repeat(200))];
        let names = HashMap::new();
        let (s, n) = prompt(&[], &[], &new, &names, 8_300, 4_000);
        assert_eq!(n, 2);
        assert!(s.contains("[e2]") && !s.contains("[e3]"));
        let (s, n) = prompt(&[], &[], &new, &names, 100, 4_000);
        assert_eq!(n, 1);
        assert!(s.len() < 3_000, "{}", s.len());
    }

    #[test]
    fn the_person_is_only_named_on_their_own_lines() {
        let ep = |id: &str, role: &str, text: &str| Episode {
            episode_id: id.into(),
            role: role.into(),
            actor_id: "u1".into(),
            agent: "claude-code".into(),
            text: text.into(),
            ..Default::default()
        };
        let names = HashMap::from([("u1".to_string(), "Ana López".to_string())]);
        let new = [
            ep("e1", "user", "let's park it"),
            ep("e2", "assistant", "I recommend a LoRA"),
            ep(
                "e3",
                "user",
                "<task-notification> report </task-notification>",
            ),
            ep("e4", "tool", "subagent report"),
        ];
        let (s, _) = prompt(&[], &[], &new, &names, 18_000, EPISODE_CHARS);
        assert!(
            s.contains("user Ana López (claude-code): let's park it"),
            "{s}"
        );
        assert!(
            s.contains("assistant (claude-code): I recommend a LoRA"),
            "{s}"
        );
        assert!(s.contains("tool (claude-code): <task-notification>"), "{s}");
        assert!(s.contains("tool (claude-code): subagent report"), "{s}");
        assert_eq!(s.matches("Ana López").count(), 1, "{s}");
    }

    #[test]
    fn a_date_without_time_takes_the_time_of_its_episode() {
        let day = crate::query::ymd_ms(2026, 10, 10);
        let cited = [
            day + 11 * 3_600_000 + 31 * 60_000,
            day + 14 * 3_600_000,
            day - 3_600_000,
        ];
        assert_eq!(
            at_episode_time("2026-10-10", day, &cited, false),
            cited[0],
            "the first one that day"
        );
        assert_eq!(
            at_episode_time("2026-10-10", day, &cited, true),
            cited[1],
            "the last one, for the end"
        );
        assert_eq!(
            at_episode_time("2026-10-09", day - crate::query::DAY_MS, &[cited[0]], false),
            day - crate::query::DAY_MS,
            "another day: the date as is"
        );
        let explicit = day + 9 * 3_600_000;
        assert_eq!(
            at_episode_time("2026-10-10T09:00:00Z", explicit, &cited, false),
            explicit,
            "an explicit time is kept"
        );
    }
    use crate::query::ymd_ms;

    fn ep(id: &str, t: i64, text: &str, ents: &[&str]) -> Episode {
        Episode {
            episode_id: id.into(),
            thread: "s1".into(),
            scope: "user:u1".into(),
            occurred_at: t,
            text: text.into(),
            entities: ents.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn known() -> HashMap<String, Entity> {
        ["system:gateway", "person:u1", "project:p1"]
            .iter()
            .map(|id| {
                (
                    id.to_string(),
                    Entity {
                        id: id.to_string(),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn only_a_persons_own_line_makes_a_decision_theirs() {
        let t = ymd_ms(2026, 10, 12);
        let line = |id: &str, role: &str, text: &str| Episode {
            role: role.into(),
            ..ep(id, t, text, &[])
        };
        let eps = vec![
            line(
                "u1",
                "user",
                "let's leave the reflex for mnemo, not worth it",
            ),
            line(
                "a1",
                "assistant",
                "the reflex only answers 'different' for mnemo",
            ),
            line(
                "n1",
                "user",
                "<task-notification>replay finished</task-notification>",
            ),
        ];
        let k = known();
        let out = parse_output(
            r#"{"entities":[],"memories":[
                  {"kind":"decision","what":"The reflex was parked for mnemo.","who":[{"entity":"person:u1","subrole":"decided_by"}],"episodes":["u1","a1"]},
                  {"kind":"decision","what":"The reflex is not adopted for mnemo.","who":[{"entity":"person:u1","subrole":"decided_by"}],"episodes":["a1"]},
                  {"kind":"decision","what":"The replay ends the reflex work.","who":[{"entity":"person:u1","subrole":"decided_by"},{"entity":"system:gateway","subrole":"decided_by"}],"episodes":["n1"]}
                ]}"#,
        )
        .unwrap();
        let cx = Ctx {
            scope: "user:u1",
            owner: "u1",
            now: t + DAY_MS,
            episodes: &eps,
            known: &k,
        };
        let (_, mems) = build(out, &cx, &|_: &str, _: &str| None);
        fn decided(m: &Memory) -> Vec<&str> {
            m.links
                .iter()
                .filter(|l| l.subrole == "decided_by")
                .map(|l| l.entity.as_str())
                .collect()
        }
        assert_eq!(
            (mems[0].kind.as_str(), decided(&mems[0])),
            ("decision", vec!["person:u1"]),
            "she said it: it is hers"
        );
        assert_eq!(
            (mems[1].kind.as_str(), decided(&mems[1])),
            ("fact", vec![]),
            "only the assistant: a conclusion"
        );
        assert_eq!(
            (mems[2].kind.as_str(), decided(&mems[2])),
            ("fact", vec!["system:gateway"]),
            "a task notification is not the person either; what is not a person stays"
        );
    }

    #[test]
    fn output_is_found_inside_prose_and_fences() {
        let o = parse_output("Sure!\n```json\n{\"memories\": [{\"what\": \"x\"}]}\n```").unwrap();
        assert_eq!(o.memories.len(), 1);
        assert!(parse_output("no json here").is_err());
    }

    #[test]
    fn build_validates_citations_links_and_dates() {
        let t = ymd_ms(2026, 9, 22) + 3_600_000;
        let eps = vec![
            ep(
                "chat:1",
                t,
                "the LB cut at 120s",
                &["project:p1", "person:u1"],
            ),
            ep("chat:2", t + 60_000, "raised to 600", &[]),
        ];
        let k = known();
        let out = parse_output(
            r#"{"entities":[{"ref":"new:1","kind":"system","name":"LB del gateway","aliases":["lb"],"parent":"system:gateway"},
                            {"ref":"new:2","kind":"concept","name":"12345"},
                            {"ref":"new:3","kind":"system","name":"Gateway"}],
                "memories":[
                  {"kind":"event","what":"Long gateway requests were cut at 120 seconds.","why":"timeout_sec=120 on the LB","who":[{"entity":"person:u1","subrole":"actor"}],"where":["new:1","system:unknown"],"about":["new:3"],"episodes":["chat:1"]},
                  {"kind":"fact","what":"Something without support","episodes":["chat:99"]},
                  {"kind":"weird","what":"The LB timeout is now 600 seconds.","why":"null","where":["new:1"],"occurred_from":"2026-09-22","episodes":["chat:2","chat:1"]}
                ]}"#,
        )
        .unwrap();
        let resolve = |name: &str, _k: &str| {
            if norm(name) == "gateway" {
                Some("system:gateway".to_string())
            } else {
                None
            }
        };
        let cx = Ctx {
            scope: "user:u1",
            owner: "u1",
            now: t + DAY_MS,
            episodes: &eps,
            known: &k,
        };
        let (ents, mems) = build(out, &cx, &resolve);
        assert_eq!(
            ents.len(),
            1,
            "the number is not an entity and 'Gateway' already existed: {ents:?}"
        );
        assert_eq!(ents[0].id, "system:lb-del-gateway@user:u1");
        assert_eq!(ents[0].parent.as_deref(), Some("system:gateway"));
        assert_eq!(
            mems.len(),
            2,
            "the memory without a valid episode is dropped"
        );
        let m = &mems[0];
        assert!(
            m.links
                .iter()
                .any(|l| l.entity == "system:gateway" && l.role == Role::About),
            "new:3 resolved to the existing entity"
        );
        assert!(
            !m.links.iter().any(|l| l.entity == "system:unknown"),
            "an unknown id is not linked"
        );
        assert!(
            m.links
                .iter()
                .any(|l| l.entity == "project:p1" && l.subrole == "context"),
            "the episode's project comes in without the model"
        );
        assert!(
            !m.links
                .iter()
                .any(|l| l.entity == "person:u1" && l.role == Role::Where),
            "the speaker is not a where"
        );
        assert_eq!(
            m.occurred_from,
            Some(t),
            "an event without a date takes the episode's"
        );
        let m2 = &mems[1];
        assert_eq!(m2.kind, "fact", "an unknown kind falls back to fact");
        assert_eq!(m2.why, None, "'null' as text is not a why");
        assert_eq!(
            m2.occurred_from,
            Some(t),
            "'2026-09-22' without a time: that of the first cited episode of that day, not 00:00"
        );
        assert_eq!(m2.known_at, t + 60_000);
        assert_eq!(m2.source_episodes, vec!["chat:2", "chat:1"]);
    }

    #[test]
    fn same_pass_duplicates_merge_their_sources() {
        let base = Memory {
            what: "The LB timeout is 600 seconds".into(),
            source_episodes: vec!["a".into()],
            ..Default::default()
        };
        let mut other = base.clone();
        other.source_episodes = vec!["b".into()];
        other.why = Some("so it never cuts first".into());
        let out = dedupe_within(vec![
            base,
            other,
            Memory {
                what: "Nomad has three servers".into(),
                source_episodes: vec!["c".into()],
                ..Default::default()
            },
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].source_episodes, vec!["a", "b"]);
        assert!(out[0].why.is_some());
    }

    #[test]
    fn ids_are_slugs_scoped_when_private() {
        assert_eq!(
            entity_id("system", "Qdrant Cluster!", "global"),
            "system:qdrant-cluster"
        );
        assert_eq!(entity_id("file", "lb.tf", "team:t1"), "file:lb.tf@team:t1");
    }
}
