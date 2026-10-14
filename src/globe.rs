// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The globe: the in-memory index search runs on, a projection of the ClickHouse log (the source
//! of truth) that each replica rebuilds at startup. The search path never calls a model.

use crate::model::{Entity, Memory, Role, W};
use crate::query::{self, DAY_MS, Parsed};
use crate::text::{Bm25, norm};
use aho_corasick::{AhoCorasick, MatchKind};
use roaring::RoaringBitmap;
use std::collections::{BTreeSet, HashMap, HashSet};

pub struct EntNode {
    pub e: Entity,
    pub alive: bool,
}

#[derive(Default)]
pub struct Globe {
    pub(crate) ents: Vec<EntNode>,
    pub(crate) ent_idx: HashMap<String, u32>,
    children_of: HashMap<String, Vec<u32>>,
    ac: Option<AhoCorasick>,
    ac_targets: Vec<Vec<u32>>,
    alias_map: HashMap<String, Vec<u32>>,
    ac_dirty: bool,
    /// `same_as` id → the id of the entity that counts.
    redirect: HashMap<String, String>,
    pub(crate) mems: Vec<Option<Memory>>,
    mem_idx: HashMap<String, u32>,
    /// Links pre-resolved to entity indexes: scoring's hot path, without the id HashMap.
    pub(crate) mem_links: Vec<Vec<(u32, Role)>>,
    times: Vec<i64>,
    pub(crate) by_ent: HashMap<u32, [RoaringBitmap; 3]>,
    pub(crate) by_scope: HashMap<String, RoaringBitmap>,
    pub(crate) live: RoaringBitmap,
    /// Live memories with a planned end, by end: nothing writes a row when a state expires, so
    /// `expire` takes them out of `live`.
    expiring: BTreeSet<(i64, u32)>,
    text: Bm25,
    vecs: HashMap<u32, Vec<i8>>,
    pub ent_rev: i64,
    pub mem_rev: i64,
    ent_seq: Vec<i64>,
    mem_seq: Vec<i64>,
    /// Memories linking an entity not yet seen: the only ones rechecked when entities arrive
    /// (rechecking all takes a third of a second at a million).
    unresolved: RoaringBitmap,
    pub(crate) map: crate::galaxy::Map,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub idx: u32,
    pub score: f32,
    pub reasons: Vec<&'static str>,
}

pub struct RecallOpts<'a> {
    pub query: &'a str,
    pub scopes: &'a [String],
    pub now: i64,
    pub as_of: Option<i64>,
    pub limit: usize,
    pub w: Option<W>,
    pub entities: &'a [String],
    pub query_vec: Option<&'a [f32]>,
    /// Hard filters on the anchor time; dates in the question only weigh.
    pub since: Option<i64>,
    pub until: Option<i64>,
}

pub struct RecallOut {
    pub parsed: Parsed,
    pub w: W,
    pub entities: Vec<u32>,
    pub hits: Vec<Hit>,
}

/// Beyond these, search stops taking milliseconds and extra candidates only add noise.
const MAX_ENTITY_CANDIDATES: usize = 3000;
const MAX_TEXT_CANDIDATES: usize = 500;

fn quantize(v: &[f32]) -> Vec<i8> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter()
        .map(|x| ((x / n) * 127.0).round().clamp(-127.0, 127.0) as i8)
        .collect()
}

fn cosine_q(a: &[i8], b: &[i8]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: i32 = a.iter().zip(b).map(|(x, y)| *x as i32 * *y as i32).sum();
    dot as f32 / (127.0 * 127.0)
}

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

impl Globe {
    fn canon<'a>(&'a self, id: &'a str) -> &'a str {
        self.redirect.get(id).map(|s| s.as_str()).unwrap_or(id)
    }

    /// Replicated ClickHouse delivers rows out of order and the reader rereads the lag window: an
    /// older or already-applied `seq` must not overwrite the current version.
    pub fn apply_entity(&mut self, e: Entity, deleted: bool, seq: i64) -> bool {
        if let Some(&i) = self.ent_idx.get(&e.id)
            && self.ent_seq[i as usize] >= seq
        {
            self.ent_rev = self.ent_rev.max(seq);
            return false;
        }
        let id = e.id.clone();
        self.upsert_entity(e, deleted, seq);
        if let Some(&i) = self.ent_idx.get(&id) {
            self.ent_seq[i as usize] = seq;
        }
        true
    }

    pub fn apply_memory(&mut self, m: Memory, deleted: bool, seq: i64) -> bool {
        if let Some(&i) = self.mem_idx.get(&m.id)
            && self.mem_seq[i as usize] >= seq
        {
            self.mem_rev = self.mem_rev.max(seq);
            return false;
        }
        let id = m.id.clone();
        self.upsert_memory(m, deleted, seq);
        if let Some(&i) = self.mem_idx.get(&id) {
            self.mem_seq[i as usize] = seq;
        }
        true
    }

    pub fn upsert_entity(&mut self, e: Entity, deleted: bool, rev: i64) {
        self.ent_rev = self.ent_rev.max(rev);
        // `same_as`: the entity hides behind its target and its memories are reindexed there.
        let redirect_to = e.same_as.clone().filter(|t| t != &e.id && !deleted);
        let was = self.redirect.get(&e.id).cloned();
        let moved: Vec<u32> = if redirect_to.is_some() && was != redirect_to {
            self.ent_idx
                .get(&e.id)
                .and_then(|i| self.by_ent.get(i))
                .map(|b| b.iter().flat_map(|r| r.iter()).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        match &redirect_to {
            Some(t) => {
                self.redirect.insert(e.id.clone(), t.clone());
            }
            None => {
                self.redirect.remove(&e.id);
            }
        }
        let deleted = deleted || redirect_to.is_some();
        if let Some(&i) = self.ent_idx.get(&e.id) {
            let old_parent = self.ents[i as usize].e.parent.clone();
            if old_parent != e.parent {
                if let Some(p) = old_parent {
                    if let Some(v) = self.children_of.get_mut(&p) {
                        v.retain(|c| *c != i);
                    }
                }
                if let Some(p) = &e.parent {
                    self.children_of.entry(p.clone()).or_default().push(i);
                }
            }
            self.ents[i as usize] = EntNode { e, alive: !deleted };
            self.map.entities_changed();
        } else {
            let i = self.ents.len() as u32;
            if let Some(p) = &e.parent {
                self.children_of.entry(p.clone()).or_default().push(i);
            }
            self.ent_idx.insert(e.id.clone(), i);
            self.ents.push(EntNode { e, alive: !deleted });
            self.ent_seq.push(0);
            self.map.entities_changed();
        }
        self.ac_dirty = true;
        let mut moved = moved;
        moved.sort_unstable();
        moved.dedup();
        for i in moved {
            // The stored memory has no embedding (it lives in `vecs`): keep it across the reindex.
            if let Some(m) = self.mems[i as usize].clone() {
                let rev = self.mem_rev;
                let keep = self.vecs.remove(&i);
                self.upsert_memory(m, false, rev);
                if let Some(v) = keep {
                    self.vecs.insert(i, v);
                }
            }
        }
    }

    pub fn upsert_memory(&mut self, m: Memory, deleted: bool, rev: i64) {
        self.mem_rev = self.mem_rev.max(rev);
        let idx = match self.mem_idx.get(&m.id) {
            Some(&i) => {
                self.unindex_memory(i);
                i
            }
            None => {
                let i = self.mems.len() as u32;
                self.mems.push(None);
                self.mem_seq.push(0);
                self.mem_links.push(Vec::new());
                self.times.push(0);
                self.mem_idx.insert(m.id.clone(), i);
                i
            }
        };
        if deleted {
            self.mems[idx as usize] = None;
            // The map too, or a deleted memory would stay drawn in place.
            self.map.memory_changed(idx);
            return;
        }
        self.unresolved.remove(idx);
        for l in &m.links {
            let Some(&e) = self.ent_idx.get(self.canon(&l.entity)) else {
                self.unresolved.insert(idx);
                continue;
            };
            self.by_ent.entry(e).or_default()[l.role.index()].insert(idx);
            self.mem_links[idx as usize].push((e, l.role));
        }
        self.times[idx as usize] = m.anchor_time();
        self.by_scope
            .entry(m.scope.clone())
            .or_default()
            .insert(idx);
        if m.is_live() {
            self.live.insert(idx);
            if let Some(end) = m.valid_to {
                self.expiring.insert((end, idx));
            }
        }
        self.text.insert(idx, &m.index_text());
        if let Some(v) = &m.embedding {
            self.vecs.insert(idx, quantize(v));
        }
        let mut m = m;
        m.embedding = None;
        self.mems[idx as usize] = Some(m);
        self.map.memory_changed(idx);
    }

    fn unindex_memory(&mut self, idx: u32) {
        let Some(m) = self.mems[idx as usize].take() else {
            return;
        };
        for l in &m.links {
            if let Some(&e) = self.ent_idx.get(self.canon(&l.entity)) {
                if let Some(b) = self.by_ent.get_mut(&e) {
                    b[l.role.index()].remove(idx);
                }
            }
        }
        if let Some(b) = self.by_scope.get_mut(&m.scope) {
            b.remove(idx);
        }
        self.live.remove(idx);
        if let Some(end) = m.valid_to {
            self.expiring.remove(&(end, idx));
        }
        self.text.remove(idx);
        self.vecs.remove(&idx);
        self.mem_links[idx as usize].clear();
    }

    /// Takes out of `live` the memories whose end has passed; returns how many.
    pub fn expire(&mut self, now: i64) -> usize {
        let mut n = 0;
        while let Some(&(end, idx)) = self.expiring.first() {
            if end > now {
                break;
            }
            self.expiring.pop_first();
            self.live.remove(idx);
            self.map.memory_changed(idx);
            n += 1;
        }
        n
    }

    /// A memory can arrive before its entity: the two log cursors advance separately.
    pub fn finish_batch(&mut self) {
        if !self.ac_dirty {
            crate::galaxy::relayout(self);
            return;
        }
        let mut pending: Vec<(u32, String, Role)> = Vec::new();
        for i in self.unresolved.iter() {
            let Some(m) = &self.mems[i as usize] else {
                continue;
            };
            for l in &m.links {
                if let Some(&e) = self.ent_idx.get(self.canon(&l.entity)) {
                    let has = self
                        .by_ent
                        .get(&e)
                        .is_some_and(|b| b[l.role.index()].contains(i));
                    if !has {
                        pending.push((i, self.canon(&l.entity).to_string(), l.role));
                    }
                }
            }
        }
        for (i, eid, role) in pending {
            let e = self.ent_idx[&eid];
            self.by_ent.entry(e).or_default()[role.index()].insert(i);
            self.mem_links[i as usize].push((e, role));
            self.map.memory_changed(i);
        }
        let still: Vec<u32> = self
            .unresolved
            .iter()
            .filter(|&i| {
                self.mems[i as usize].as_ref().is_some_and(|m| {
                    m.links
                        .iter()
                        .any(|l| !self.ent_idx.contains_key(self.canon(&l.entity)))
                })
            })
            .collect();
        self.unresolved = still.into_iter().collect();
        self.rebuild_aliases();
        crate::galaxy::relayout(self);
    }

    fn rebuild_aliases(&mut self) {
        let mut by_alias: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, n) in self.ents.iter().enumerate() {
            if !n.alive {
                continue;
            }
            let mut names: Vec<&str> = n.e.aliases.iter().map(|s| s.as_str()).collect();
            names.push(&n.e.name);
            for a in names {
                let a = norm(a.trim());
                if a.chars().count() < 2 {
                    continue;
                }
                let v = by_alias.entry(a).or_default();
                if !v.contains(&(i as u32)) {
                    v.push(i as u32);
                }
            }
        }
        self.alias_map = by_alias.clone();
        let (pats, targets): (Vec<String>, Vec<Vec<u32>>) = by_alias.into_iter().unzip();
        self.ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&pats)
            .ok();
        self.ac_targets = targets;
        self.ac_dirty = false;
    }

    pub fn same_as_of(&self, id: &str) -> Option<String> {
        self.redirect.get(id).cloned()
    }

    /// Same-name non-person entities per scope: (the most linked, which stays; the rest).
    pub fn duplicate_groups(&self) -> Vec<(Entity, Vec<Entity>)> {
        let mut groups: HashMap<(String, String), Vec<u32>> = HashMap::new();
        for (i, n) in self.ents.iter().enumerate() {
            if n.alive && n.e.kind != "person" {
                groups
                    .entry((n.e.scope.clone(), norm(n.e.name.trim())))
                    .or_default()
                    .push(i as u32);
            }
        }
        let weight = |i: u32| -> u64 {
            self.by_ent
                .get(&i)
                .map_or(0, |b| b.iter().map(|r| r.len()).sum())
        };
        let mut out = Vec::new();
        for ((_, name), mut v) in groups {
            if v.len() < 2 || name.chars().count() < 2 {
                continue;
            }
            v.sort_by_key(|&i| (std::cmp::Reverse(weight(i)), i));
            let keep = self.ents[v[0] as usize].e.clone();
            let drop = v[1..]
                .iter()
                .map(|&i| self.ents[i as usize].e.clone())
                .collect();
            out.push((keep, drop));
        }
        out
    }

    pub fn mergeable_entities(&self) -> Vec<(Entity, u64)> {
        self.ents
            .iter()
            .enumerate()
            .filter(|(_, n)| n.alive && n.e.kind != "person" && n.e.same_as.is_none())
            .map(|(i, n)| {
                let w = self
                    .by_ent
                    .get(&(i as u32))
                    .map_or(0, |b| b.iter().map(|r| r.len()).sum());
                (n.e.clone(), w)
            })
            .collect()
    }

    pub fn entity(&self, id: &str) -> Option<&Entity> {
        self.ent_idx
            .get(self.canon(id))
            .map(|&i| &self.ents[i as usize])
            .filter(|n| n.alive)
            .map(|n| &n.e)
    }

    pub fn entity_at(&self, i: u32) -> &Entity {
        &self.ents[i as usize].e
    }

    pub fn entity_idx(&self, id: &str) -> Option<u32> {
        self.ent_idx
            .get(self.canon(id))
            .copied()
            .filter(|&i| self.ents[i as usize].alive)
    }

    pub fn memory(&self, id: &str) -> Option<&Memory> {
        self.mem_idx
            .get(id)
            .and_then(|&i| self.mems[i as usize].as_ref())
    }

    pub fn memory_at(&self, i: u32) -> &Memory {
        self.mems[i as usize].as_ref().expect("memory is indexed")
    }

    pub fn entity_count_in(&self, scope: &str) -> usize {
        self.ents
            .iter()
            .filter(|n| n.alive && n.e.scope == scope)
            .count()
    }

    /// Per scope: (live memories, total indexed).
    pub fn scope_counts(&self) -> Vec<(String, u64, u64)> {
        let mut v: Vec<(String, u64, u64)> = self
            .by_scope
            .iter()
            .map(|(s, b)| (s.clone(), (b & &self.live).len(), b.len()))
            .filter(|(_, _, total)| *total > 0)
            .collect();
        v.sort();
        v
    }

    /// (id, text to embed) of live memories without a vector.
    pub fn missing_vectors(&self, limit: usize) -> Vec<(String, String)> {
        self.live
            .iter()
            .filter(|i| !self.vecs.contains_key(i))
            .filter_map(|i| self.mems[i as usize].as_ref())
            .take(limit)
            .map(|m| (m.id.clone(), m.index_text()))
            .collect()
    }

    pub fn memories(&self) -> impl Iterator<Item = &Memory> {
        self.mems.iter().flatten()
    }

    pub fn counts(&self) -> (usize, usize) {
        (
            self.ents.iter().filter(|n| n.alive).count(),
            self.mems.iter().filter(|m| m.is_some()).count(),
        )
    }

    fn visible_entity(&self, i: u32, scopes: &HashSet<&str>) -> bool {
        let n = &self.ents[i as usize];
        n.alive && scopes.contains(n.e.scope.as_str())
    }

    pub fn link_entities(&self, text: &str, scopes: &[String]) -> Vec<u32> {
        let Some(ac) = &self.ac else {
            return Vec::new();
        };
        let scopes: HashSet<&str> = scopes.iter().map(|s| s.as_str()).collect();
        let t = norm(text);
        let mut out = Vec::new();
        for m in ac.find_iter(&t) {
            let before = t[..m.start()].chars().next_back();
            let after = t[m.end()..].chars().next();
            if is_word_char(before) || is_word_char(after) {
                continue;
            }
            for &e in &self.ac_targets[m.pattern().as_usize()] {
                if self.visible_entity(e, &scopes) && !out.contains(&e) {
                    out.push(e);
                }
            }
        }
        out
    }

    /// Exact name or alias match; among several, the one in the narrowest scope.
    pub fn resolve_name(&self, name: &str, scopes: &[String], kind: Option<&str>) -> Option<u32> {
        let sc: HashSet<&str> = scopes.iter().map(|s| s.as_str()).collect();
        let cands = self.alias_map.get(&norm(name.trim()))?;
        let rank = |s: &str| {
            if s.starts_with("user:") {
                0
            } else if s.starts_with("team:") {
                1
            } else {
                2
            }
        };
        cands
            .iter()
            .copied()
            .filter(|&e| {
                self.visible_entity(e, &sc)
                    && kind.is_none_or(|k| self.ents[e as usize].e.kind == k)
            })
            .min_by_key(|&e| rank(&self.ents[e as usize].e.scope))
    }

    /// Finds rewordings that share no entities, so the judge does not store the same thing twice.
    pub fn similar_vec(&self, v: &[f32], scope: &str, k: usize) -> Vec<(u32, f32)> {
        let q = quantize(v);
        let allowed = self.by_scope.get(scope).cloned().unwrap_or_default() & &self.live;
        let mut best: Vec<(u32, f32)> = allowed
            .iter()
            .filter_map(|i| self.vecs.get(&i).map(|x| (i, cosine_q(&q, x))))
            .collect();
        best.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        best.truncate(k);
        best
    }

    pub fn similar_text(&self, text: &str, scope: &str, k: usize) -> Vec<u32> {
        let allowed = self.by_scope.get(scope).cloned().unwrap_or_default() & &self.live;
        let terms = crate::text::tokens(text);
        let mut v: Vec<(u32, f32)> = self
            .text
            .score(&terms, &allowed)
            .into_iter()
            .filter(|(_, s)| *s > 1.5)
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.into_iter().take(k).map(|x| x.0).collect()
    }

    pub fn ancestors(&self, i: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = self.ents[i as usize].e.parent.clone();
        while let Some(p) = cur {
            let Some(&pi) = self.ent_idx.get(&p) else {
                break;
            };
            if out.contains(&pi) || out.len() > 16 {
                break;
            }
            out.push(pi);
            cur = self.ents[pi as usize].e.parent.clone();
        }
        out
    }

    pub fn descendants(&self, i: u32) -> Vec<u32> {
        let mut out = vec![i];
        let mut k = 0;
        while k < out.len() && out.len() < 4096 {
            let id = &self.ents[out[k] as usize].e.id;
            if let Some(ch) = self.children_of.get(id) {
                for &c in ch {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
            k += 1;
        }
        out
    }

    pub fn postings_any(&self, e: u32) -> RoaringBitmap {
        self.by_ent
            .get(&e)
            .map(|b| &b[0] | &b[1] | &b[2])
            .unwrap_or_default()
    }

    pub fn memories_of(&self, e: u32, scopes: &[String]) -> Vec<u32> {
        let allowed = self.allowed(scopes);
        let mut set = RoaringBitmap::new();
        for d in self.descendants(e) {
            set |= self.postings_any(d);
        }
        let mut v: Vec<u32> = (&set & &allowed).iter().collect();
        v.sort_by_key(|&i| std::cmp::Reverse(self.memory_at(i).anchor_time()));
        v
    }

    fn allowed(&self, scopes: &[String]) -> RoaringBitmap {
        let mut a = RoaringBitmap::new();
        for s in scopes {
            if let Some(b) = self.by_scope.get(s) {
                a |= b;
            }
        }
        a
    }

    pub fn neighbors(&self, entity_ids: &[String], scope: &str, limit: usize) -> Vec<u32> {
        let allowed = self.by_scope.get(scope).cloned().unwrap_or_default() & &self.live;
        let mut count: HashMap<u32, u32> = HashMap::new();
        for id in entity_ids {
            let Some(e) = self.entity_idx(id) else {
                continue;
            };
            for m in (&self.postings_any(e) & &allowed).iter() {
                *count.entry(m).or_default() += 1;
            }
        }
        let mut v: Vec<(u32, u32)> = count.into_iter().collect();
        v.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| {
                self.memory_at(b.0)
                    .anchor_time()
                    .cmp(&self.memory_at(a.0).anchor_time())
            })
        });
        v.into_iter().take(limit).map(|x| x.0).collect()
    }

    pub fn recall(&self, o: &RecallOpts) -> RecallOut {
        let parsed = query::parse(o.query, o.now);
        let w = o.w.or(parsed.w).unwrap_or(W::What);
        let allowed = self.allowed(o.scopes);

        let mut qents = self.link_entities(o.query, o.scopes);
        // Client-supplied ids pass the same scope filter as the text, and are capped: a huge list
        // would be a way to guess ids.
        let sc: HashSet<&str> = o.scopes.iter().map(|s| s.as_str()).collect();
        for id in o.entities.iter().take(32) {
            if let Some(i) = self.entity_idx(id) {
                if self.visible_entity(i, &sc) && !qents.contains(&i) {
                    qents.push(i);
                }
            }
        }
        let ent_sets: Vec<(u32, HashSet<u32>, HashSet<u32>)> = qents
            .iter()
            .map(|&q| {
                let desc: HashSet<u32> = self
                    .descendants(q)
                    .into_iter()
                    .filter(|&d| d != q)
                    .collect();
                let anc: HashSet<u32> = self.ancestors(q).into_iter().collect();
                (q, desc, anc)
            })
            .collect();
        // Same-name entities ("Nomad" the system and the repo) are one mention and count once:
        // summing them would let a memory linked to both beat the one that answers.
        let mut group_of: HashMap<String, usize> = HashMap::new();
        let groups: Vec<usize> = qents
            .iter()
            .map(|&q| {
                let n = group_of.len();
                *group_of
                    .entry(norm(&self.ents[q as usize].e.name))
                    .or_insert(n)
            })
            .collect();

        let mut by_ents = RoaringBitmap::new();
        for (q, desc, _) in &ent_sets {
            by_ents |= self.postings_any(*q);
            for d in desc {
                by_ents |= self.postings_any(*d);
            }
        }
        by_ents &= &allowed;
        let mut cand: Vec<u32> = by_ents.iter().collect();
        if cand.len() > MAX_ENTITY_CANDIDATES {
            cand.select_nth_unstable_by_key(MAX_ENTITY_CANDIDATES, |&i| {
                std::cmp::Reverse(self.times[i as usize])
            });
            cand.truncate(MAX_ENTITY_CANDIDATES);
        }
        let mut bm_all = self.text.score(&parsed.terms, &allowed);
        if bm_all.len() > MAX_TEXT_CANDIDATES {
            bm_all.select_nth_unstable_by(MAX_TEXT_CANDIDATES, |a, b| b.1.total_cmp(&a.1));
            bm_all.truncate(MAX_TEXT_CANDIDATES);
        }
        let bm: HashMap<u32, f32> = bm_all.into_iter().collect();
        let mut cand: RoaringBitmap = cand.into_iter().collect();
        for &i in bm.keys() {
            cand.insert(i);
        }
        let mut sem: HashMap<u32, f32> = HashMap::new();
        if let Some(qv) = o.query_vec {
            let q = quantize(qv);
            let mut best: Vec<(u32, f32)> = self
                .vecs
                .iter()
                .filter(|(i, _)| allowed.contains(**i))
                .map(|(i, v)| (*i, cosine_q(&q, v)))
                .filter(|(_, c)| *c > 0.35)
                .collect();
            best.sort_by(|a, b| b.1.total_cmp(&a.1));
            for (i, c) in best.into_iter().take(40) {
                sem.insert(i, c);
                cand.insert(i);
            }
        }

        let history = w == W::Why || parsed.from.is_some();
        let mut hits = Vec::new();
        for i in cand.iter() {
            let m = self.memory_at(i);
            if let Some(t) = o.as_of {
                if !m.is_live_at(t) {
                    continue;
                }
            }
            let at = m.anchor_time();
            if o.since.is_some_and(|s| at < s) || o.until.is_some_and(|u| at >= u) {
                continue;
            }
            let mut s = 0.0f32;
            let mut reasons = Vec::new();
            let link_ents = &self.mem_links[i as usize];
            let mut by_group = vec![0.0f32; group_of.len()];
            for (k, (q, desc, anc)) in ent_sets.iter().enumerate() {
                let mut best: f32 = 0.0;
                for (e, role) in link_ents {
                    let mut v = if e == q {
                        3.0
                    } else if desc.contains(e) {
                        2.0
                    } else if anc.contains(e) {
                        1.0
                    } else {
                        0.0
                    };
                    if v > 0.0
                        && matches!(
                            (w, role),
                            (W::Who, Role::Who) | (W::Where, Role::Where) | (W::Where, Role::About)
                        )
                    {
                        v += 0.5;
                    }
                    best = best.max(v);
                }
                by_group[groups[k]] = by_group[groups[k]].max(best);
            }
            let ent_score: f32 = by_group.iter().sum();
            if ent_score > 0.0 {
                reasons.push("entity");
            }
            s += ent_score;
            let b = bm.get(&i).copied().unwrap_or(0.0);
            if b > 0.0 {
                s += 3.0 * b / (b + 2.0);
                reasons.push("text");
            }
            if let Some(c) = sem.get(&i) {
                s += 3.0 * c;
                reasons.push("semantic");
            }
            if ent_score == 0.0 && b == 0.0 && !sem.contains_key(&i) {
                continue;
            }
            if let (Some(f), Some(u)) = (parsed.from, parsed.until) {
                let start = m.occurred_from.unwrap_or(m.known_at);
                let end = m.occurred_to.unwrap_or(start + 1);
                if start < u && end > f {
                    s += 2.5;
                    reasons.push("date");
                } else if m.occurred_from.is_some() {
                    s -= 2.0;
                }
            }
            if m.answers(w) {
                s += 1.2;
                reasons.push("answers");
            }
            // Closed memories must rank behind anything live that answers: with a mild -1, one
            // sharing an entity (+3) would still win and the card would state something false.
            if m.is_live() {
                s += 0.6;
            } else if !history {
                s -= 6.0;
            }
            let age_days = ((o.now - at).max(0) / DAY_MS) as f32;
            s += 0.5 * (-age_days / 30.0).exp();
            s += 0.4 * m.confidence;
            hits.push(Hit {
                idx: i,
                score: s,
                reasons,
            });
        }
        hits.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| {
                self.memory_at(b.idx)
                    .anchor_time()
                    .cmp(&self.memory_at(a.idx).anchor_time())
            })
        });
        hits.truncate(o.limit);
        RecallOut {
            parsed,
            w,
            entities: qents,
            hits,
        }
    }

    pub fn snapshot(
        &self,
        scopes: &[String],
        as_of: Option<i64>,
        max: usize,
    ) -> (Vec<&Entity>, Vec<&Memory>) {
        let allowed = self.allowed(scopes);
        let sc: HashSet<&str> = scopes.iter().map(|s| s.as_str()).collect();
        let mut mems: Vec<&Memory> = allowed
            .iter()
            .map(|i| self.memory_at(i))
            .filter(|m| as_of.is_none_or(|t| m.valid_from <= t))
            .collect();
        mems.sort_by_key(|m| std::cmp::Reverse(m.anchor_time()));
        mems.truncate(max);
        let ents = self
            .ents
            .iter()
            .filter(|n| n.alive && sc.contains(n.e.scope.as_str()))
            .map(|n| &n.e)
            .collect();
        (ents, mems)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::Link;
    use crate::query::ymd_ms;

    pub fn ent(id: &str, kind: &str, name: &str, aliases: &[&str], parent: Option<&str>) -> Entity {
        Entity {
            id: id.into(),
            scope: "global".into(),
            kind: kind.into(),
            name: name.into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            parent: parent.map(|s| s.into()),
            ..Default::default()
        }
    }

    pub fn mem(
        id: &str,
        day: (i64, i64, i64),
        what: &str,
        why: Option<&str>,
        links: &[(&str, Role)],
    ) -> Memory {
        let t = ymd_ms(day.0, day.1, day.2);
        Memory {
            id: id.into(),
            scope: "global".into(),
            owner_id: "u1".into(),
            kind: "event".into(),
            what: what.into(),
            why: why.map(|s| s.into()),
            how: None,
            occurred_from: Some(t),
            occurred_to: None,
            known_at: t,
            valid_from: t,
            valid_to: None,
            superseded_by: None,
            confidence: 0.9,
            source_episodes: vec!["chat:1".into()],
            links: links
                .iter()
                .map(|(e, r)| Link {
                    entity: e.to_string(),
                    role: *r,
                    subrole: String::new(),
                })
                .collect(),
            judge: "test".into(),
            embedding: None,
        }
    }

    #[test]
    fn a_state_is_live_until_it_expires() {
        let now = crate::app::now_ms();
        let mut g = Globe::default();
        let mut m = mem("state", (2026, 1, 1), "pipeline frozen", None, &[]);
        m.kind = "state".into();
        m.valid_from = now - 1_000;
        m.valid_to = Some(now + 60_000);
        m.embedding = Some(vec![1.0, 0.0]);
        g.upsert_memory(m, false, 1);
        assert_eq!(g.live.len(), 1);
        assert_eq!(g.similar_vec(&[1.0, 0.0], "global", 3).len(), 1);
        assert_eq!(g.expire(now), 0);
        assert_eq!(g.expire(now + 60_000), 1);
        assert!(g.live.is_empty());
        assert!(g.similar_vec(&[1.0, 0.0], "global", 3).is_empty());
    }

    pub fn sample() -> Globe {
        let mut g = Globe::default();
        for e in [
            ent("person:ana", "person", "Ana", &["ana"], None),
            ent(
                "system:gateway",
                "system",
                "acme-gateway",
                &["gateway", "agw"],
                None,
            ),
            ent(
                "system:lb",
                "system",
                "LB del gateway",
                &["lb", "balanceador"],
                Some("system:gateway"),
            ),
            ent("file:lb.tf", "code", "lb.tf", &["lb.tf"], Some("system:lb")),
            ent("system:nomad", "system", "Nomad", &["nomad"], None),
            ent(
                "system:slack",
                "system",
                "puente de Slack",
                &["slack"],
                None,
            ),
        ] {
            g.upsert_entity(e, false, 1);
        }
        let mut old = mem(
            "m2",
            (2026, 9, 8),
            "The gateway LB cuts connections at 120 s.",
            None,
            &[("system:lb", Role::Where)],
        );
        old.valid_to = Some(ymd_ms(2026, 9, 22));
        old.superseded_by = Some("m3".into());
        old.kind = "state".into();
        for m in [
            mem(
                "m1",
                (2026, 9, 22),
                "Long gateway requests were cut at 120 seconds.",
                Some("The load balancer had timeout_sec=120; the gateway binary did not cut."),
                &[
                    ("person:ana", Role::Who),
                    ("system:lb", Role::Where),
                    ("file:lb.tf", Role::Where),
                    ("system:gateway", Role::About),
                ],
            ),
            old,
            mem(
                "m3",
                (2026, 9, 22),
                "The gateway LB timeout is raised to 600 s.",
                Some("So the LB never cuts before the gateway."),
                &[("system:lb", Role::Where), ("person:ana", Role::Who)],
            ),
            mem(
                "m4",
                (2026, 9, 25),
                "The production Nomad cluster is up with 3 servers and ACLs.",
                None,
                &[("system:nomad", Role::About), ("person:ana", Role::Who)],
            ),
            mem(
                "m5",
                (2026, 9, 10),
                "One Slack message started two turns.",
                Some("Slack resent the message with a new event_id."),
                &[("system:slack", Role::Where)],
            ),
        ] {
            g.upsert_memory(m, false, 1);
        }
        g.finish_batch();
        g
    }

    fn global() -> Vec<String> {
        vec!["global".into()]
    }

    fn recall(g: &Globe, q: &str) -> RecallOut {
        let scopes = global();
        g.recall(&RecallOpts {
            query: q,
            scopes: &scopes,
            now: ymd_ms(2026, 10, 2),
            as_of: None,
            limit: 5,
            w: None,
            entities: &[],
            query_vec: None,
            since: None,
            until: None,
        })
    }

    #[test]
    fn an_entity_that_is_another_one_hands_over_its_memories() {
        let mut g = Globe::default();
        g.upsert_entity(ent("system:nomad", "system", "Nomad", &[], None), false, 1);
        g.upsert_entity(ent("repo:nomad", "repo", "Nomad", &[], None), false, 2);
        g.upsert_memory(
            mem(
                "m1",
                (2026, 10, 1),
                "The bootstrap token is in the bashrc",
                None,
                &[("repo:nomad", Role::About)],
            ),
            false,
            3,
        );
        g.finish_batch();
        let sys = g.entity_idx("system:nomad").unwrap();
        assert!(g.memories_of(sys, &global()).is_empty());
        let mut same = ent("repo:nomad", "repo", "Nomad", &[], None);
        same.same_as = Some("system:nomad".into());
        g.upsert_entity(same, false, 4);
        g.finish_batch();
        assert_eq!(
            g.entity_idx("repo:nomad"),
            Some(sys),
            "the old id leads to the right one"
        );
        assert_eq!(g.entity("repo:nomad").unwrap().id, "system:nomad");
        assert_eq!(
            g.memories_of(sys, &global()).len(),
            1,
            "and its memory moves to the right one"
        );
        assert_eq!(g.duplicate_groups().len(), 0, "no longer a duplicate");
    }

    #[test]
    fn a_closed_memory_ranks_behind_a_current_one_that_answers() {
        let mut g = Globe::default();
        g.upsert_entity(
            ent("system:gatehouse", "system", "Gatehouse", &[], None),
            false,
            1,
        );
        let mut closed = mem(
            "m-closed",
            (2026, 10, 1),
            "Gatehouse lets clients spoof the Forwarded header",
            None,
            &[("system:gatehouse", Role::About)],
        );
        closed.valid_to = Some(closed.valid_from + 1);
        let live = mem(
            "m-live",
            (2026, 10, 1),
            "Forwarded and True-Client-IP headers are now stripped at the edge",
            None,
            &[],
        );
        g.upsert_memory(closed, false, 2);
        g.upsert_memory(live, false, 3);
        g.finish_batch();
        let out = g.recall(&RecallOpts {
            query: "Does gatehouse strip the Forwarded header?",
            scopes: &global(),
            now: ymd_ms(2026, 10, 2),
            as_of: None,
            limit: 5,
            w: None,
            entities: &[],
            query_vec: None,
            since: None,
            until: None,
        });
        assert_eq!(g.memory_at(out.hits[0].idx).id, "m-live");
    }

    #[test]
    fn same_named_entities_in_the_question_count_once() {
        let mut g = Globe::default();
        g.upsert_entity(ent("system:nomad", "system", "Nomad", &[], None), false, 1);
        g.upsert_entity(ent("repo:nomad", "repo", "Nomad", &[], None), false, 2);
        g.upsert_entity(
            ent("system:gatehouse", "system", "Gatehouse", &[], None),
            false,
            3,
        );
        let both = mem(
            "m-both",
            (2026, 10, 1),
            "Gatehouse's token can read Nomad job definitions with registry passwords",
            None,
            &[
                ("system:gatehouse", Role::About),
                ("system:nomad", Role::About),
                ("repo:nomad", Role::Where),
            ],
        );
        let right = mem(
            "m-right",
            (2026, 10, 1),
            "The bootstrap token is exported in the bashrc",
            None,
            &[("system:nomad", Role::About)],
        );
        g.upsert_memory(both, false, 4);
        g.upsert_memory(right, false, 5);
        g.finish_batch();
        let out = g.recall(&RecallOpts {
            query: "Is the Nomad bootstrap token exposed?",
            scopes: &global(),
            now: ymd_ms(2026, 10, 2),
            as_of: None,
            limit: 5,
            w: None,
            entities: &[],
            query_vec: None,
            since: None,
            until: None,
        });
        assert_eq!(g.memory_at(out.hits[0].idx).id, "m-right");
    }

    #[test]
    fn links_entities_by_alias_with_word_boundaries() {
        let g = sample();
        let s = global();
        let found: Vec<&str> = g
            .link_entities("¿Por qué el LB cortaba antes que el Gateway?", &s)
            .into_iter()
            .map(|i| g.entity_at(i).id.as_str())
            .collect();
        assert!(
            found.contains(&"system:lb") && found.contains(&"system:gateway"),
            "{found:?}"
        );
        assert!(
            g.link_entities("ablbx gatewayish", &s).is_empty(),
            "no word boundary, no entity"
        );
        // LeftmostLongest: the LB's full name contains "gateway" but names only the LB.
        let long: Vec<&str> = g
            .link_entities("el LB del gateway", &s)
            .into_iter()
            .map(|i| g.entity_at(i).id.as_str())
            .collect();
        assert_eq!(long, vec!["system:lb"]);
        assert!(
            g.link_entities("gateway", &["team:other".to_string()])
                .is_empty(),
            "an entity from another scope is not visible"
        );
    }

    #[test]
    fn why_question_prefers_the_memory_that_has_a_why() {
        let g = sample();
        let out = recall(
            &g,
            "¿Por qué se cortaban las peticiones largas del gateway?",
        );
        assert_eq!(out.w, W::Why);
        assert_eq!(
            g.memory_at(out.hits[0].idx).id,
            "m1",
            "{:?}",
            out.hits
                .iter()
                .map(|h| (&g.memory_at(h.idx).id, h.score))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_system_includes_what_it_contains() {
        let g = sample();
        // m3 only links the LB, which is INSIDE the gateway.
        let out = recall(&g, "¿qué ha cambiado en el gateway?");
        let ids: Vec<&str> = out
            .hits
            .iter()
            .map(|h| g.memory_at(h.idx).id.as_str())
            .collect();
        assert!(ids.contains(&"m3"), "{ids:?}");
    }

    #[test]
    fn superseded_memories_sink_unless_asking_for_history() {
        let g = sample();
        let out = recall(&g, "timeout del lb");
        let pos = |id: &str| out.hits.iter().position(|h| g.memory_at(h.idx).id == id);
        assert!(
            pos("m3") < pos("m2"),
            "the current memory ranks before the superseded one"
        );
        let scopes = global();
        let past = g.recall(&RecallOpts {
            query: "timeout del lb",
            scopes: &scopes,
            now: ymd_ms(2026, 10, 2),
            as_of: Some(ymd_ms(2026, 9, 15)),
            limit: 5,
            w: None,
            entities: &[],
            query_vec: None,
            since: None,
            until: None,
        });
        let ids: Vec<&str> = past
            .hits
            .iter()
            .map(|h| g.memory_at(h.idx).id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["m2"],
            "on 15-sep only the 120 s cut was true at the time"
        );
    }

    #[test]
    fn dates_in_the_question_filter_softly() {
        let g = sample();
        let out = recall(&g, "¿qué pasó con slack el 10 de septiembre?");
        assert_eq!(g.memory_at(out.hits[0].idx).id, "m5");
        assert!(out.hits[0].reasons.contains(&"date"));
    }

    #[test]
    fn scopes_hide_other_peoples_memories() {
        let mut g = sample();
        let mut private = mem(
            "p1",
            (2026, 9, 30),
            "Gateway secret plan.",
            None,
            &[("system:gateway", Role::About)],
        );
        private.scope = "user:other".into();
        g.upsert_memory(private, false, 2);
        let out = recall(&g, "gateway secret plan");
        assert!(
            out.hits
                .iter()
                .all(|h| g.memory_at(h.idx).scope == "global")
        );
    }

    #[test]
    fn updating_and_deleting_a_memory_reindexes_it() {
        let mut g = sample();
        let mut m = g.memory("m5").unwrap().clone();
        m.what = "Kafka consumer lag on billing.".into();
        m.why = None;
        m.links.clear();
        g.upsert_memory(m.clone(), false, 3);
        assert!(
            recall(&g, "slack message turns")
                .hits
                .iter()
                .all(|h| g.memory_at(h.idx).id != "m5")
        );
        assert!(
            recall(&g, "kafka lag")
                .hits
                .iter()
                .any(|h| g.memory_at(h.idx).id == "m5")
        );
        g.upsert_memory(m, true, 4);
        assert!(g.memory("m5").is_none());
        assert!(recall(&g, "kafka lag").hits.is_empty());
        assert_eq!(g.mem_rev, 4);
    }

    #[test]
    fn an_old_version_that_arrives_late_does_not_overwrite_the_new_one() {
        let mut g = Globe::default();
        let v1 = mem("m1", (2026, 9, 1), "The gateway cut at 120 s.", None, &[]);
        let mut v2 = v1.clone();
        v2.what = "The gateway cut at 300 s.".into();
        assert!(g.apply_memory(v2.clone(), false, 20));
        assert!(
            !g.apply_memory(v1, false, 10),
            "a replica brings version 10 after version 20"
        );
        assert_eq!(g.memory("m1").unwrap().what, "The gateway cut at 300 s.");
        assert!(
            !g.apply_memory(v2.clone(), false, 20),
            "the same row again is not reapplied"
        );
        assert!(g.apply_memory(v2, true, 30), "the later delete is applied");
        assert!(g.memory("m1").is_none());
        assert_eq!(g.mem_rev, 30);

        let gw = ent("system:gw", "system", "gateway", &[], None);
        let mut old = gw.clone();
        old.name = "old gateway".into();
        assert!(g.apply_entity(gw, false, 40));
        assert!(!g.apply_entity(old, false, 35));
        assert_eq!(g.ents[g.ent_idx["system:gw"] as usize].e.name, "gateway");
        assert_eq!(g.ent_rev, 40);
    }

    #[test]
    fn a_memory_that_arrives_before_its_entity_is_linked_after_the_batch() {
        let mut g = sample();
        g.upsert_memory(
            mem(
                "m9",
                (2026, 9, 29),
                "Vector store migrated.",
                None,
                &[("system:qdrant", Role::About)],
            ),
            false,
            5,
        );
        g.upsert_entity(
            ent("system:qdrant", "system", "Qdrant", &["qdrant"], None),
            false,
            5,
        );
        g.finish_batch();
        let out = recall(&g, "¿qué pasó con qdrant?");
        assert_eq!(
            out.hits.first().map(|h| g.memory_at(h.idx).id.as_str()),
            Some("m9")
        );
    }

    #[test]
    fn semantic_vectors_find_paraphrases() {
        let mut g = sample();
        let mut m = mem("v1", (2026, 9, 1), "Unrelated wording entirely.", None, &[]);
        m.embedding = Some(vec![1.0, 0.0, 0.0]);
        g.upsert_memory(m, false, 6);
        let scopes = global();
        let qv = [0.9f32, 0.1, 0.0];
        let out = g.recall(&RecallOpts {
            query: "zzz",
            scopes: &scopes,
            now: ymd_ms(2026, 10, 2),
            as_of: None,
            limit: 3,
            w: None,
            entities: &[],
            query_vec: Some(&qv),
            since: None,
            until: None,
        });
        assert_eq!(
            out.hits.first().map(|h| g.memory_at(h.idx).id.as_str()),
            Some("v1")
        );
    }

    #[test]
    fn entity_ids_of_another_person_are_ignored_in_recall() {
        let mut g = sample();
        let mut secret = ent(
            "project:secret@user:b",
            "project",
            "Secret Project",
            &[],
            None,
        );
        secret.scope = "user:b".into();
        secret.summary = "what only b knows".into();
        g.upsert_entity(secret, false, 2);
        let mut m = mem(
            "mb",
            (2026, 9, 1),
            "b knows this",
            None,
            &[("project:secret@user:b", Role::About)],
        );
        m.scope = "user:b".into();
        g.upsert_memory(m, false, 7);
        let scopes = global();
        let ids = ["project:secret@user:b".to_string()];
        let out = g.recall(&RecallOpts {
            query: "zzz",
            scopes: &scopes,
            now: ymd_ms(2026, 10, 2),
            as_of: None,
            limit: 5,
            w: None,
            entities: &ids,
            query_vec: None,
            since: None,
            until: None,
        });
        assert!(out.entities.is_empty(), "{:?}", out.entities);
        assert!(out.hits.is_empty());
    }
}
