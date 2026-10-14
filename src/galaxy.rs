// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The map: each root entity is a galaxy and each memory a star among the entities it links.
//! Positions are hashed from ids (stable); a tile is a contiguous range of a Z-order index.

use crate::globe::Globe;
use roaring::RoaringBitmap;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::f64::consts::TAU;

pub const KINDS: [&str; 10] = [
    "person", "team", "system", "env", "repo", "file", "channel", "project", "service", "concept",
];
pub const PALETTE: u8 = 10;
pub const MEMORY_KINDS: [&str; 6] = [
    "fact",
    "event",
    "decision",
    "procedure",
    "state",
    "preference",
];

fn color_idx(name: &str) -> u8 {
    let mut h: u32 = 0x811c_9dc5;
    for b in crate::text::norm(name.trim()).as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (h % PALETTE as u32) as u8
}

fn memory_kind_idx(kind: &str) -> u8 {
    MEMORY_KINDS.iter().position(|k| *k == kind).unwrap_or(0) as u8
}

pub const MAX_POINTS: usize = 3000;
pub const GRID: u32 = 64;
const MAX_Z: u32 = 16;

#[derive(Default)]
pub struct Map {
    ent_pos: Vec<(f32, f32)>,
    ent_kind: Vec<u8>,
    ent_galaxy_kind: Vec<u8>,
    mem_pos: Vec<(f32, f32)>,
    mem_kind: Vec<u8>,
    mem_root: Vec<u32>,
    /// (Z code, memory index), sorted.
    zindex: Vec<(u32, u32)>,
    ents_dirty: bool,
    dirty: Vec<u32>,
    /// Memories with a slot in `zindex`, to know which entry to drop when they move.
    placed: RoaringBitmap,
    /// Counted per scope: a route's weight would otherwise reveal memories the viewer cannot read.
    routes: HashMap<u64, Vec<(u16, u32)>>,
    /// Each live memory's (scope, entities), to subtract when it changes; empty = nothing.
    contrib: Vec<(u16, Box<[u32]>)>,
    /// Kept up to date per memory so tiles only read them: walking pairs or intersecting bitmaps
    /// on every tile takes ~300 ms at a million memories.
    ent_count: HashMap<u32, Vec<(u16, u32)>>,
    region_count: HashMap<u32, Vec<(u16, u32)>>,
    region_routes: HashMap<u64, Vec<(u16, u32)>>,
    scope_ids: HashMap<String, u16>,
    scope_names: Vec<String>,
    adj: HashMap<u32, Vec<u32>>,
    ent_root: Vec<u32>,
}

fn pair(a: u32, b: u32) -> u64 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    ((lo as u64) << 32) | hi as u64
}

fn unpair(k: u64) -> (u32, u32) {
    ((k >> 32) as u32, k as u32)
}

enum Change {
    Created,
    Updated,
    Removed,
}

fn bump<K: std::hash::Hash + Eq + Copy>(
    m: &mut HashMap<K, Vec<(u16, u32)>>,
    k: K,
    sid: u16,
    delta: i32,
) -> Change {
    let fresh = !m.contains_key(&k);
    let v = m.entry(k).or_default();
    match v.iter_mut().find(|(s, _)| *s == sid) {
        Some(e) => e.1 = (e.1 as i64 + delta as i64).max(0) as u32,
        None if delta > 0 => v.push((sid, delta as u32)),
        None => {}
    }
    v.retain(|(_, c)| *c > 0);
    if v.is_empty() {
        m.remove(&k);
        return if fresh {
            Change::Updated
        } else {
            Change::Removed
        };
    }
    if fresh {
        Change::Created
    } else {
        Change::Updated
    }
}

fn seen_sum(v: &[(u16, u32)], visible: &[u16]) -> u32 {
    v.iter()
        .filter(|(s, _)| visible.contains(s))
        .map(|(_, c)| *c)
        .sum()
}

impl Map {
    fn scope_id(&mut self, scope: &str) -> u16 {
        if let Some(&i) = self.scope_ids.get(scope) {
            return i;
        }
        let i = self.scope_names.len() as u16;
        self.scope_ids.insert(scope.to_string(), i);
        self.scope_names.push(scope.to_string());
        i
    }

    fn apply(&mut self, sid: u16, ents: &[u32], delta: i32) {
        for &e in ents {
            bump(&mut self.ent_count, e, sid, delta);
        }
        let root = |e: u32| self.ent_root.get(e as usize).copied().unwrap_or(e);
        let r0 = root(ents[0]);
        bump(&mut self.region_count, r0, sid, delta);
        for i in 0..ents.len() {
            for j in i + 1..ents.len() {
                let (a, b) = (ents[i], ents[j]);
                match bump(&mut self.routes, pair(a, b), sid, delta) {
                    Change::Created => {
                        self.adj.entry(a).or_default().push(b);
                        self.adj.entry(b).or_default().push(a);
                    }
                    Change::Removed => {
                        for (x, y) in [(a, b), (b, a)] {
                            if let Some(v) = self.adj.get_mut(&x) {
                                v.retain(|o| *o != y);
                            }
                        }
                    }
                    Change::Updated => {}
                }
                let (ra, rb) = (root(a), root(b));
                if ra != rb {
                    bump(&mut self.region_routes, pair(ra, rb), sid, delta);
                }
            }
        }
    }

    fn set_contrib(&mut self, idx: u32, now: Option<(&str, &[u32])>) {
        let i = idx as usize;
        if self.contrib.len() <= i {
            self.contrib.resize_with(i + 1, || (0, Box::default()));
        }
        let (sid, old) = std::mem::take(&mut self.contrib[i]);
        if !old.is_empty() {
            self.apply(sid, &old, -1);
        }
        if let Some((scope, ents)) = now {
            let mut es: Vec<u32> = ents.to_vec();
            es.sort_unstable();
            es.dedup();
            if !es.is_empty() {
                let sid = self.scope_id(scope);
                self.apply(sid, &es, 1);
                self.contrib[i] = (sid, es.into_boxed_slice());
            }
        }
    }

    fn rebuild_regions(&mut self) {
        self.region_count.clear();
        self.region_routes.clear();
        let contrib = std::mem::take(&mut self.contrib);
        for (sid, ents) in &contrib {
            if ents.is_empty() {
                continue;
            }
            let root = |e: u32| self.ent_root.get(e as usize).copied().unwrap_or(e);
            let r0 = root(ents[0]);
            bump(&mut self.region_count, r0, *sid, 1);
            for i in 0..ents.len() {
                for j in i + 1..ents.len() {
                    let (ra, rb) = (root(ents[i]), root(ents[j]));
                    if ra != rb {
                        bump(&mut self.region_routes, pair(ra, rb), *sid, 1);
                    }
                }
            }
        }
        self.contrib = contrib;
    }

    fn weight(&self, k: u64, visible: &[u16]) -> u32 {
        self.routes
            .get(&k)
            .map(|v| seen_sum(v, visible))
            .unwrap_or(0)
    }

    pub fn entities_changed(&mut self) {
        self.ents_dirty = true;
    }

    pub fn memory_changed(&mut self, idx: u32) {
        self.dirty.push(idx);
    }

    fn zindex_entry(&self, idx: u32) -> Option<(u32, u32)> {
        if !self.placed.contains(idx) {
            return None;
        }
        let (x, y) = *self.mem_pos.get(idx as usize)?;
        Some((morton(x, y), idx))
    }

    fn zindex_remove(&mut self, e: Option<(u32, u32)>) {
        if let Some(e) = e {
            if let Ok(at) = self.zindex.binary_search(&e) {
                self.zindex.remove(at);
            }
            self.placed.remove(e.1);
        }
    }

    #[cfg(test)]
    pub fn memory_pos(&self, idx: u32) -> Option<(f32, f32)> {
        self.mem_pos.get(idx as usize).copied()
    }
}

/// FNV-1a, stable across versions and machines (std's hash is not), mapped to [0, 1).
/// Each channel selects a layout dimension (angle, radius or jitter). These fixed selectors
/// keep positions stable; this hash is for drawing the map, not cryptography.
fn layout_fraction(id: &str, channel: u64) -> f64 {
    let mut x: u64 = 0xcbf29ce484222325 ^ channel.wrapping_mul(0x100000001b3);
    for b in id.bytes() {
        x ^= b as u64;
        x = x.wrapping_mul(0x100000001b3);
    }
    // Final mix: FNV alone leaves the low bits poorly spread for similar ids.
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

fn kind_idx(kind: &str) -> u8 {
    KINDS
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(KINDS.len() - 1) as u8
}

fn spiral(id: &str) -> (f64, f64) {
    let arms = 4.0;
    let arm = (layout_fraction(id, 21) * arms).floor();
    let t = layout_fraction(id, 22).powf(0.8);
    let r = 0.05 + 0.40 * t;
    let a = arm * TAU / arms + 3.4 * t + (layout_fraction(id, 23) - 0.5) * 0.45;
    let spread = 0.012 + 0.03 * t;
    let (jx, jy) = (
        (layout_fraction(id, 24) - 0.5) * spread,
        (layout_fraction(id, 25) - 0.5) * spread,
    );
    (0.5 + r * a.cos() + jx, 0.5 + r * a.sin() + jy)
}

fn arm(depth: u32) -> f64 {
    0.014 / 2.6f64.powi(depth.saturating_sub(1) as i32)
}

fn clamp01(v: f64) -> f32 {
    v.clamp(0.0, 0.999_999) as f32
}

/// Interleaves 16 bits of x (even bits) and 16 bits of y (odd bits).
pub fn morton(x: f32, y: f32) -> u32 {
    let spread = |v: u32| {
        let mut v = v & 0xffff;
        v = (v | (v << 8)) & 0x00ff00ff;
        v = (v | (v << 4)) & 0x0f0f0f0f;
        v = (v | (v << 2)) & 0x33333333;
        (v | (v << 1)) & 0x55555555
    };
    let q = |v: f32| ((v.clamp(0.0, 0.999_999) * 65536.0) as u32).min(0xffff);
    spread(q(x)) | (spread(q(y)) << 1)
}

/// All entities are re-placed if any changed (there are few); memories only when touched.
pub fn relayout(g: &mut Globe) {
    let ents_dirty = std::mem::take(&mut g.map.ents_dirty);
    let mut dirty = std::mem::take(&mut g.map.dirty);
    if !ents_dirty && dirty.is_empty() {
        return;
    }
    let mut roots_changed = false;
    if ents_dirty {
        // Only memories of entities that moved: re-placing all takes half a second at a million.
        let before = std::mem::take(&mut g.map.ent_pos);
        let roots_before = std::mem::take(&mut g.map.ent_root);
        layout_entities(g);
        roots_changed = roots_before
            .iter()
            .enumerate()
            .any(|(i, r)| g.map.ent_root.get(i) != Some(r));
        let mut moved = RoaringBitmap::new();
        for (i, p) in g.map.ent_pos.iter().enumerate() {
            if before.get(i) != Some(p) {
                if let Some(b) = g.by_ent.get(&(i as u32)) {
                    for r in b {
                        moved |= r;
                    }
                }
            }
        }
        dirty.extend(moved.iter());
    }
    dirty.sort_unstable();
    dirty.dedup();
    let n = g.mems.len();
    g.map.mem_pos.resize(n, (0.5, 0.5));
    g.map.mem_kind.resize(n, (KINDS.len() - 1) as u8);
    g.map.mem_root.resize(n, u32::MAX);
    let incremental = dirty.len() < 256 && !g.map.zindex.is_empty();
    for &idx in &dirty {
        let live = g
            .mems
            .get(idx as usize)
            .and_then(|m| m.as_ref())
            .filter(|m| m.is_live())
            .map(|m| m.scope.clone());
        let ents: Vec<u32> = g
            .mem_links
            .get(idx as usize)
            .map(|v| v.iter().map(|(e, _)| *e).collect())
            .unwrap_or_default();
        g.map
            .set_contrib(idx, live.as_deref().map(|s| (s, ents.as_slice())));
    }
    if roots_changed {
        g.map.rebuild_regions();
    }
    for &idx in &dirty {
        let i = idx as usize;
        let old = g.map.zindex_entry(idx);
        let Some(m) = g.mems.get(i).and_then(|m| m.as_ref()) else {
            if incremental {
                g.map.zindex_remove(old);
            }
            continue;
        };
        let links = &g.mem_links[i];
        let kind = memory_kind_idx(&m.kind);
        let pos = if links.is_empty() {
            let (a, r) = (
                layout_fraction(&m.id, 11) * TAU,
                0.48 * layout_fraction(&m.id, 12).sqrt(),
            );
            (0.5 + r * a.cos(), 0.5 + r * a.sin())
        } else {
            let (mut sx, mut sy) = (0.0, 0.0);
            for (e, _) in links {
                let p = g
                    .map
                    .ent_pos
                    .get(*e as usize)
                    .copied()
                    .unwrap_or((0.5, 0.5));
                sx += p.0 as f64;
                sy += p.1 as f64;
            }
            let k = links.len() as f64;
            let (a, r) = (
                layout_fraction(&m.id, 13) * TAU,
                0.0022 * (0.15 + layout_fraction(&m.id, 14)),
            );
            (sx / k + r * a.cos(), sy / k + r * a.sin())
        };
        g.map.mem_pos[i] = (clamp01(pos.0), clamp01(pos.1));
        g.map.mem_kind[i] = kind;
        g.map.mem_root[i] = links
            .first()
            .and_then(|(e, _)| g.map.ent_root.get(*e as usize).copied())
            .unwrap_or(u32::MAX);
        g.map.placed.insert(idx);
        if incremental {
            g.map.zindex_remove(old);
            let e = (morton(g.map.mem_pos[i].0, g.map.mem_pos[i].1), idx);
            let at = g.map.zindex.partition_point(|x| *x < e);
            g.map.zindex.insert(at, e);
        }
    }
    if !incremental {
        let mut z: Vec<(u32, u32)> = g
            .mems
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_some())
            .map(|(i, _)| {
                let (x, y) = g.map.mem_pos[i];
                (morton(x, y), i as u32)
            })
            .collect();
        z.sort_unstable();
        g.map.zindex = z;
    }
}

fn layout_entities(g: &mut Globe) {
    let n = g.ents.len();
    let mut pos: Vec<Option<(f64, f64)>> = vec![None; n];
    let mut gkind = vec![(KINDS.len() - 1) as u8; n];
    // Depth capped so a parent cycle cannot hang the process.
    fn place(
        g: &Globe,
        i: usize,
        pos: &mut Vec<Option<(f64, f64)>>,
        gkind: &mut Vec<u8>,
        guard: u32,
    ) -> ((f64, f64), u32) {
        if let Some(p) = pos[i] {
            let mut d = 0;
            let mut cur = i;
            while let Some(pi) = g.ents[cur]
                .e
                .parent
                .as_ref()
                .and_then(|p| g.ent_idx.get(p))
                .map(|&x| x as usize)
            {
                d += 1;
                cur = pi;
                if d > 8 {
                    break;
                }
            }
            return (p, d);
        }
        let e = &g.ents[i].e;
        let parent = e
            .parent
            .as_ref()
            .and_then(|p| g.ent_idx.get(p))
            .map(|&x| x as usize)
            .filter(|&p| p != i);
        let (p, depth) = match parent {
            Some(pi) if guard < 8 => {
                let (pp, pd) = place(g, pi, pos, gkind, guard + 1);
                gkind[i] = gkind[pi];
                let depth = pd + 1;
                let r = arm(depth) * (0.3 + 0.7 * layout_fraction(&e.id, 3));
                let a = layout_fraction(&e.id, 4) * TAU + 1.6 * r / arm(depth);
                ((pp.0 + r * a.cos(), pp.1 + r * a.sin()), depth)
            }
            _ => {
                gkind[i] = kind_idx(&e.kind);
                ((spiral(&e.id).0, spiral(&e.id).1), 0)
            }
        };
        pos[i] = Some(p);
        (p, depth)
    }
    for i in 0..n {
        place(g, i, &mut pos, &mut gkind, 0);
    }
    g.map.ent_pos = pos
        .into_iter()
        .map(|p| {
            p.map(|(x, y)| (clamp01(x), clamp01(y)))
                .unwrap_or((0.5, 0.5))
        })
        .collect();
    g.map.ent_kind = g.ents.iter().map(|n| color_idx(&n.e.name)).collect();
    g.map.ent_root = (0..n)
        .map(|i| {
            let mut cur = i;
            for _ in 0..8 {
                match g.ents[cur].e.parent.as_ref().and_then(|p| g.ent_idx.get(p)) {
                    Some(&p) if p as usize != cur => cur = p as usize,
                    _ => break,
                }
            }
            cur as u32
        })
        .collect();
    g.map.ent_galaxy_kind = gkind;
}

fn visible(g: &Globe, scopes: &[String], include_old: bool) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for s in scopes {
        if let Some(x) = g.by_scope.get(s) {
            b |= x;
        }
    }
    if !include_old {
        b &= &g.live;
    }
    b
}

pub fn tile(g: &Globe, z: u32, x: u32, y: u32, scopes: &[String], include_old: bool) -> Value {
    let z = z.min(22);
    let span = 1.0 / (1u64 << z) as f64;
    let (x0, y0) = (x as f64 * span, y as f64 * span);
    let inside = |p: (f32, f32)| {
        (p.0 as f64) >= x0
            && (p.0 as f64) < x0 + span
            && (p.1 as f64) >= y0
            && (p.1 as f64) < y0 + span
    };
    // Below level 16 (65536 per side) the Z range is the ancestor's, filtered by coordinates.
    let zr = z.min(MAX_Z);
    let (ax, ay) = (x >> (z - zr), y >> (z - zr));
    let shift = 32 - 2 * zr;
    let prefix = if zr == 0 {
        0
    } else {
        morton_prefix(ax, ay, zr)
    };
    let lo = (prefix as u64) << shift;
    let hi = ((prefix as u64) + 1) << shift;
    let start = g.map.zindex.partition_point(|(c, _)| (*c as u64) < lo);
    let end = g.map.zindex.partition_point(|(c, _)| (*c as u64) < hi);
    let vis = visible(g, scopes, include_old);
    let candidates: Vec<u32> = g.map.zindex[start..end]
        .iter()
        .map(|(_, i)| *i)
        .filter(|i| vis.contains(*i) && (zr == z || inside(g.map.mem_pos[*i as usize])))
        .collect();
    let mut out = json!({"z": z, "x": x, "y": y, "total": candidates.len()});
    let visible_scopes: Vec<u16> = scopes
        .iter()
        .filter_map(|s| g.map.scope_ids.get(s).copied())
        .collect();
    if candidates.len() <= MAX_POINTS {
        out["points"] = json!(candidates
            .iter()
            .map(|&i| {
                let (px, py) = g.map.mem_pos[i as usize];
                let m = g.mems[i as usize].as_ref();
                json!({"id": m.map(|m| m.id.as_str()).unwrap_or(""), "x": px, "y": py, "k": g.map.mem_kind[i as usize], "live": m.is_some_and(|m| m.is_live())})
            })
            .collect::<Vec<_>>());
    } else {
        let mut cells: HashMap<(u32, u32), (u32, [u32; KINDS.len()])> = HashMap::new();
        for &i in &candidates {
            let (px, py) = g.map.mem_pos[i as usize];
            let cx = (((px as f64 - x0) / span) * GRID as f64).clamp(0.0, (GRID - 1) as f64) as u32;
            let cy = (((py as f64 - y0) / span) * GRID as f64).clamp(0.0, (GRID - 1) as f64) as u32;
            let c = cells.entry((cx, cy)).or_insert((0, [0; KINDS.len()]));
            c.0 += 1;
            c.1[g.map.mem_kind[i as usize] as usize] += 1;
        }
        let cell = span / GRID as f64;
        out["cells"] = json!(cells
            .iter()
            .map(|((cx, cy), (n, kinds))| {
                let k = kinds.iter().enumerate().max_by_key(|(_, c)| **c).map(|(k, _)| k).unwrap_or(0);
                json!({"x": x0 + (*cx as f64 + 0.5) * cell, "y": y0 + (*cy as f64 + 0.5) * cell, "n": n, "k": k})
            })
            .collect::<Vec<_>>());
        out["cell"] = json!(cell);
    }
    // From afar only the most populated entities, so labels stay readable. Sizes count only what
    // the viewer can see: counting everything would reveal memories they cannot read.
    let scope_set: std::collections::HashSet<&str> = scopes.iter().map(|s| s.as_str()).collect();
    let seen = |i: usize| {
        g.map
            .ent_count
            .get(&(i as u32))
            .map(|v| seen_sum(v, &visible_scopes) as u64)
            .unwrap_or(0)
    };
    let ent_visible = |i: usize| g.ents[i].alive && scope_set.contains(g.ents[i].e.scope.as_str());
    let mut ents: Vec<(u64, usize)> = (0..g.ents.len())
        .filter(|&i| ent_visible(i) && g.map.ent_pos.get(i).is_some_and(|p| inside(*p)))
        .map(|i| (seen(i), i))
        .collect();
    // Regions always go in: they name the map even when their memories hang from child systems.
    ents.sort_by_key(|e| (g.ents[e.1].e.parent.is_some(), std::cmp::Reverse(e.0)));
    let roots = ents
        .iter()
        .filter(|e| g.ents[e.1].e.parent.is_none())
        .count();
    let limit = roots
        + if z < 4 {
            80
        } else if z < 8 {
            400
        } else {
            3000
        };
    let pos_of = |i: usize| g.map.ent_pos[i];
    out["entities"] = json!(ents
        .iter()
        .take(limit)
        .map(|(count, i)| {
            let n = &g.ents[*i].e;
            let (px, py) = pos_of(*i);
            let parent = n.parent.as_ref().and_then(|p| g.ent_idx.get(p)).map(|&p| p as usize).filter(|&p| ent_visible(p));
            json!({"id": n.id, "name": n.name, "x": px, "y": py, "k": g.map.ent_kind[*i], "n": count, "root": n.parent.is_none(),
                "region_n": g.map.region_count.get(&(*i as u32)).map(|v| seen_sum(v, &visible_scopes)).unwrap_or(0),
                "p": parent.map(|p| { let (qx, qy) = pos_of(p); json!([qx, qy]) })})
        })
        .collect::<Vec<_>>());
    let visible_scopes: Vec<u16> = scopes
        .iter()
        .filter_map(|s| g.map.scope_ids.get(s).copied())
        .collect();
    let mut routes: Vec<(u32, u64, usize, usize)> = Vec::new();
    if z < 3 {
        for (&k, v) in &g.map.region_routes {
            let (a, b) = unpair(k);
            let w = seen_sum(v, &visible_scopes);
            if w > 0 && ent_visible(a as usize) && ent_visible(b as usize) {
                routes.push((w, k, a as usize, b as usize));
            }
        }
    } else {
        let mut done = std::collections::HashSet::new();
        for (_, i) in &ents {
            for &o in g
                .map
                .adj
                .get(&(*i as u32))
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let k = pair(*i as u32, o);
                if !done.insert(k) || !ent_visible(o as usize) {
                    continue;
                }
                let w = g.map.weight(k, &visible_scopes);
                if w > 0 {
                    routes.push((w, k, *i, o as usize));
                }
            }
        }
    }
    routes.sort_by_key(|r| std::cmp::Reverse(r.0));
    out["routes"] = json!(routes
        .iter()
        .take(if z < 3 { 400 } else { 800 })
        .map(|(w, k, a, b)| {
            let (pa, pb) = (pos_of(*a), pos_of(*b));
            json!({"id": k.to_string(), "a": [pa.0, pa.1], "b": [pb.0, pb.1], "w": w, "ka": g.map.ent_kind[*a], "kb": g.map.ent_kind[*b],
                "ea": g.ents[*a].e.id, "eb": g.ents[*b].e.id})
        })
        .collect::<Vec<_>>());
    out
}

fn morton_prefix(x: u32, y: u32, z: u32) -> u32 {
    // The z bits of x and y, interleaved as in `morton`, are the prefix of the tile's range.
    let mut code = 0u32;
    for b in 0..z {
        code |= ((x >> b) & 1) << (2 * b);
        code |= ((y >> b) & 1) << (2 * b + 1);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globe::tests::{ent, mem};
    use crate::model::{Memory, Role};

    fn gm(id: &str, scope: &str, links: &[(&str, Role)]) -> Memory {
        let mut m = mem(id, (2026, 1, 1), "x", None, links);
        m.scope = scope.into();
        m
    }

    fn mem_idx(g: &Globe, id: &str) -> u32 {
        g.mems
            .iter()
            .position(|m| m.as_ref().is_some_and(|m| m.id == id))
            .unwrap() as u32
    }

    fn sample() -> Globe {
        let mut g = Globe::default();
        g.upsert_entity(ent("repo:a", "repo", "alpha", &[], None), false, 1);
        g.upsert_entity(
            ent("file:a1", "file", "a1.rs", &[], Some("repo:a")),
            false,
            2,
        );
        g.upsert_entity(ent("repo:b", "repo", "beta", &[], None), false, 3);
        for i in 0..50 {
            g.upsert_memory(
                gm(&format!("m{i}"), "global", &[("file:a1", Role::Where)]),
                false,
                10 + i,
            );
        }
        g.upsert_memory(
            gm(
                "bridge",
                "global",
                &[("repo:a", Role::Where), ("repo:b", Role::Where)],
            ),
            false,
            99,
        );
        g.upsert_memory(
            gm("secret", "user:ana", &[("repo:a", Role::Where)]),
            false,
            100,
        );
        g.finish_batch();
        g
    }

    #[test]
    fn positions_are_stable_and_children_orbit_their_parent() {
        let g = sample();
        let pos = |id: &str| g.map.ent_pos[g.entity_idx(id).unwrap() as usize];
        let (a, f) = (pos("repo:a"), pos("file:a1"));
        let d = ((a.0 - f.0).powi(2) + (a.1 - f.1).powi(2)).sqrt();
        assert!(d < 0.02 && d > 0.0, "the child near its root: {d}");
        let again = sample();
        assert_eq!(again.map.ent_pos, g.map.ent_pos, "same id, same position");
        let bridge = g.map.memory_pos(mem_idx(&g, "bridge")).unwrap();
        let b = pos("repo:b");
        let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        assert!(
            ((bridge.0 - mid.0).powi(2) + (bridge.1 - mid.1).powi(2)).sqrt() < 0.01,
            "the bridge, between the two galaxies"
        );
    }

    #[test]
    fn the_whole_sky_is_one_tile_and_scopes_are_respected() {
        let g = sample();
        let all = tile(&g, 0, 0, 0, &["global".into()], false);
        assert_eq!(all["total"], 51, "ana's memory is not visible from global");
        let with_ana = tile(&g, 0, 0, 0, &["global".into(), "user:ana".into()], false);
        assert_eq!(with_ana["total"], 52);
        let names: Vec<&str> = all["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["alpha", "beta", "a1.rs"],
            "regions first (they name the map), then systems from most to least populated"
        );
        let sum: u64 = (0..2)
            .flat_map(|x| (0..2).map(move |y| (x, y)))
            .map(|(x, y)| {
                tile(&g, 1, x, y, &["global".into()], false)["total"]
                    .as_u64()
                    .unwrap()
            })
            .sum();
        assert_eq!(sum, 51);
        // At level 20 (below the index) filtering is by coordinates.
        let (px, py) = g.map.memory_pos(mem_idx(&g, "bridge")).unwrap();
        let (tx, ty) = (
            (px as f64 * (1u64 << 20) as f64) as u32,
            (py as f64 * (1u64 << 20) as f64) as u32,
        );
        let deep = tile(&g, 20, tx, ty, &["global".into()], false);
        assert!(
            deep["points"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == "bridge")
        );
    }

    #[test]
    fn small_changes_patch_the_index_like_a_full_rebuild() {
        let mut g = sample();
        g.upsert_memory(gm("late", "global", &[("repo:b", Role::Where)]), false, 200);
        g.upsert_memory(gm("m3", "global", &[]), true, 201);
        g.upsert_entity(ent("repo:c", "repo", "gamma", &[], None), false, 202);
        g.finish_batch();
        let patched = g.map.zindex.clone();
        assert!(patched.windows(2).all(|w| w[0] <= w[1]), "still sorted");
        g.map.zindex.clear();
        g.map.dirty = (0..g.mems.len() as u32).collect();
        relayout(&mut g);
        assert_eq!(patched, g.map.zindex, "patching equals a full rebuild");
        assert_eq!(
            tile(&g, 0, 0, 0, &["global".into()], false)["total"],
            51,
            "51 + late - m3"
        );
    }

    #[test]
    fn routes_follow_shared_memories_and_only_count_what_you_see() {
        let g = sample();
        // "secret" (user:ana) only touches repo:a, so it adds no route.
        let far = tile(&g, 0, 0, 0, &["global".into()], false);
        let routes = far["routes"].as_array().unwrap();
        assert_eq!(
            routes.len(),
            1,
            "one route between the two regions: {routes:?}"
        );
        assert_eq!(routes[0]["w"], 1);
        let mut g2 = sample();
        g2.upsert_memory(
            gm(
                "ana-bridge",
                "user:ana",
                &[("repo:a", Role::Where), ("repo:b", Role::Where)],
            ),
            false,
            300,
        );
        g2.finish_batch();
        assert_eq!(
            tile(&g2, 0, 0, 0, &["global".into()], false)["routes"][0]["w"],
            1,
            "ana's memory does not widen the route for others"
        );
        assert_eq!(
            tile(&g2, 0, 0, 0, &["global".into(), "user:ana".into()], false)["routes"][0]["w"],
            2
        );
        let n_of = |t: &serde_json::Value, id: &str| {
            t["entities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == id)
                .map(|e| e["n"].as_u64().unwrap())
                .unwrap_or(0)
        };
        assert_eq!(
            n_of(&tile(&g2, 0, 0, 0, &["global".into()], false), "repo:a"),
            1
        );
        assert_eq!(
            n_of(
                &tile(&g2, 0, 0, 0, &["global".into(), "user:ana".into()], false),
                "repo:a"
            ),
            3
        );
        g2.upsert_memory(gm("bridge", "global", &[]), false, 301);
        g2.finish_batch();
        assert!(
            tile(&g2, 0, 0, 0, &["global".into()], false)["routes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn regions_sit_on_spiral_arms() {
        let rs: Vec<f64> = (0..400)
            .map(|i| {
                let (x, y) = spiral(&format!("r{i}"));
                ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt()
            })
            .collect();
        assert!(rs.iter().all(|r| *r > 0.03 && *r < 0.49));
        let inner = rs.iter().filter(|r| **r < 0.2).count();
        assert!(
            inner > 50 && inner < 350,
            "neither all at the center nor all at the edge: {inner}"
        );
    }

    #[test]
    fn crowded_tiles_turn_into_clouds() {
        let mut g = Globe::default();
        g.upsert_entity(ent("repo:big", "repo", "big", &[], None), false, 1);
        for i in 0..(MAX_POINTS + 500) {
            g.upsert_memory(
                gm(&format!("m{i}"), "global", &[("repo:big", Role::About)]),
                false,
                10 + i as i64,
            );
        }
        g.finish_batch();
        let t = tile(&g, 0, 0, 0, &["global".into()], false);
        assert!(t.get("points").is_none());
        let n: u64 = t["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["n"].as_u64().unwrap())
            .sum();
        assert_eq!(n as usize, MAX_POINTS + 500, "the clouds count every point");
    }

    #[test]
    fn morton_tiles_are_contiguous_ranges() {
        for (x, y) in [(0.1f32, 0.7f32), (0.9, 0.2), (0.5, 0.5)] {
            let code = morton(x, y);
            for z in 1..=16u32 {
                let (tx, ty) = (
                    (x as f64 * (1u64 << z) as f64) as u32,
                    (y as f64 * (1u64 << z) as f64) as u32,
                );
                let prefix = morton_prefix(tx, ty, z);
                assert_eq!(code >> (32 - 2 * z), prefix, "z={z}");
            }
        }
    }
}
