// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Normalisation, tokens and an in-memory BM25 index. Tokens are cut to a five-letter stem:
//! crude, but it groups inflections in both languages without a per-language stemmer.

use roaring::RoaringBitmap;
use std::collections::HashMap;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

pub fn norm(s: &str) -> String {
    s.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

const STOP: &[&str] = &[
    "que",
    "por",
    "como",
    "cuando",
    "donde",
    "quien",
    "quienes",
    "cual",
    "cuales",
    "del",
    "las",
    "los",
    "una",
    "unos",
    "unas",
    "para",
    "con",
    "sin",
    "este",
    "esta",
    "esto",
    "estos",
    "estas",
    "ese",
    "esa",
    "eso",
    "paso",
    "pasa",
    "pasado",
    "sobre",
    "entre",
    "pero",
    "porque",
    "hay",
    "hace",
    "hizo",
    "fue",
    "era",
    "son",
    "ser",
    "estar",
    "esta",
    "estan",
    "nos",
    "les",
    "sus",
    "mas",
    "muy",
    "ya",
    "aun",
    "tambien",
    "todo",
    "toda",
    "todos",
    "algo",
    "alguien",
    "el",
    "la",
    "lo",
    "le",
    "se",
    "de",
    "en",
    "y",
    "o",
    "a",
    "al",
    "un",
    "es",
    "su",
    "mi",
    "tu",
    "me",
    "te",
    "si",
    "no",
    "ha",
    "he",
    "han",
    "habia",
    "sabes",
    "dime",
    "explica",
    "explicame",
    "cuentame",
    "queria",
    "quiero",
    "saber",
    "the",
    "and",
    "for",
    "with",
    "what",
    "who",
    "when",
    "where",
    "why",
    "how",
    "which",
    "this",
    "that",
    "these",
    "those",
    "was",
    "were",
    "are",
    "is",
    "be",
    "been",
    "did",
    "does",
    "do",
    "has",
    "have",
    "had",
    "about",
    "from",
    "into",
    "our",
    "your",
    "their",
    "its",
    "of",
    "to",
    "in",
    "on",
    "at",
    "by",
    "an",
    "it",
    "we",
    "you",
    "they",
    "there",
    "can",
    "could",
    "would",
    "should",
    "tell",
    "me",
    "know",
];

fn is_stop(t: &str) -> bool {
    STOP.contains(&t)
}

pub fn stem(t: &str) -> String {
    t.chars().take(5).collect()
}

pub fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in s.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let whole = norm(raw);
        push_token(&mut out, &whole);
        let parts = camel_parts(raw);
        if parts.len() > 1 {
            for p in parts {
                push_token(&mut out, &norm(&p));
            }
        }
    }
    out
}

fn push_token(out: &mut Vec<String>, t: &str) {
    if t.chars().count() < 2 || is_stop(t) {
        return;
    }
    out.push(stem(t));
}

fn camel_parts(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

type DocTerms = (u32, Vec<(u32, u16)>);

/// Postings are (doc, frequency, generation): removing or rewriting a doc bumps its generation
/// instead of walking the lists, and stale entries are compacted past 10,000 and a third.
#[derive(Default)]
pub struct Bm25 {
    term_ids: HashMap<String, u32>,
    postings: Vec<Vec<(u32, u16, u32)>>,
    df: Vec<u32>,
    /// Per document: (length, [(term, frequency)]).
    docs: Vec<Option<DocTerms>>,
    gens: Vec<u32>,
    n_live: u32,
    total_len: u64,
    entries: usize,
    dead: usize,
}

const K1: f32 = 1.2;
const B: f32 = 0.75;

impl Bm25 {
    fn ensure(&mut self, doc: u32) {
        let need = doc as usize + 1;
        if self.docs.len() < need {
            self.docs.resize_with(need, || None);
            self.gens.resize(need, 0);
        }
    }

    pub fn insert(&mut self, doc: u32, text: &str) {
        self.remove(doc);
        self.ensure(doc);
        let toks = tokens(text);
        let mut tf: HashMap<u32, u16> = HashMap::new();
        for t in &toks {
            let next = self.term_ids.len() as u32;
            let id = *self.term_ids.entry(t.clone()).or_insert(next);
            if id as usize == self.postings.len() {
                self.postings.push(Vec::new());
                self.df.push(0);
            }
            *tf.entry(id).or_default() += 1;
        }
        let generation = self.gens[doc as usize].wrapping_add(1);
        self.gens[doc as usize] = generation;
        let terms: Vec<(u32, u16)> = tf.into_iter().collect();
        for &(t, f) in &terms {
            self.postings[t as usize].push((doc, f, generation));
            self.df[t as usize] += 1;
        }
        self.entries += terms.len();
        self.total_len += toks.len() as u64;
        self.n_live += 1;
        self.docs[doc as usize] = Some((toks.len() as u32, terms));
    }

    pub fn remove(&mut self, doc: u32) {
        let Some(slot) = self.docs.get_mut(doc as usize) else {
            return;
        };
        let Some((len, terms)) = slot.take() else {
            return;
        };
        self.gens[doc as usize] = self.gens[doc as usize].wrapping_add(1);
        for (t, _) in &terms {
            self.df[*t as usize] -= 1;
        }
        self.dead += terms.len();
        self.total_len -= len as u64;
        self.n_live -= 1;
        if self.dead > 10_000 && self.dead * 3 > self.entries {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let gens = &self.gens;
        let docs = &self.docs;
        let mut entries = 0;
        for p in &mut self.postings {
            p.retain(|(d, _, g)| docs[*d as usize].is_some() && gens[*d as usize] == *g);
            entries += p.len();
        }
        self.entries = entries;
        self.dead = 0;
    }

    /// In a large index, a term in over a quarter of the documents is ignored if rarer ones are
    /// present: it distinguishes nothing and is the most expensive list to walk.
    pub fn score(&self, terms: &[String], allowed: &RoaringBitmap) -> Vec<(u32, f32)> {
        let n = self.n_live.max(1) as f32;
        let avgdl = (self.total_len as f32 / n).max(1.0);
        let mut ids: Vec<u32> = terms
            .iter()
            .filter_map(|t| self.term_ids.get(t).copied())
            .filter(|&t| self.df[t as usize] > 0)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if n >= 1000.0 && ids.iter().any(|&t| (self.df[t as usize] as f32) < 0.25 * n) {
            ids.retain(|&t| (self.df[t as usize] as f32) < 0.25 * n);
        }
        let mut acc: HashMap<u32, f32> = HashMap::new();
        for t in ids {
            let df = self.df[t as usize] as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(doc, f, g) in &self.postings[t as usize] {
                if self.gens[doc as usize] != g || !allowed.contains(doc) {
                    continue;
                }
                let Some((len, _)) = &self.docs[doc as usize] else {
                    continue;
                };
                let f = f as f32;
                *acc.entry(doc).or_default() +=
                    idf * (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * (*len as f32) / avgdl));
            }
        }
        acc.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_drops_accents_and_case() {
        assert_eq!(norm("Decisión ÚNICA"), "decision unica");
    }

    #[test]
    fn tokens_share_a_stem_across_inflections() {
        assert_eq!(tokens("cortaban")[0], tokens("cortar")[0]);
        assert_eq!(tokens("peticiones")[0], tokens("petición")[0]);
        assert!(
            tokens("¿Por qué el de la?").is_empty(),
            "only stopwords: {:?}",
            tokens("¿Por qué el de la?")
        );
    }

    #[test]
    fn compaction_keeps_live_postings() {
        let mut idx = Bm25::default();
        for i in 0..30_000u32 {
            idx.insert(
                i,
                if i % 2 == 0 {
                    "alpha beta"
                } else {
                    "gamma delta"
                },
            );
        }
        for i in 0..20_000u32 {
            idx.remove(i);
        }
        let all: RoaringBitmap = (0..30_000u32).collect();
        assert_eq!(idx.score(&tokens("alpha"), &all).len(), 5_000);
        assert!(idx.dead < 10_001, "compacted: {}", idx.dead);
    }

    #[test]
    fn camel_case_yields_its_parts() {
        let t = tokens("gatewayTimeout");
        assert!(
            t.contains(&"gatew".to_string()) && t.contains(&"timeo".to_string()),
            "{t:?}"
        );
    }

    #[test]
    fn bm25_ranks_the_rare_term_higher_and_forgets_removed_docs() {
        let mut idx = Bm25::default();
        idx.insert(1, "the gateway load balancer cut long requests");
        idx.insert(2, "the gateway serves models");
        idx.insert(3, "slack sent the same message twice");
        let all: RoaringBitmap = [1u32, 2, 3].into_iter().collect();
        let s: HashMap<u32, f32> = idx
            .score(&tokens("balancer cut"), &all)
            .into_iter()
            .collect();
        assert!(s[&1] > 0.0 && !s.contains_key(&2));
        idx.remove(1);
        assert!(idx.score(&tokens("balancer"), &all).is_empty());
        let only2: RoaringBitmap = [2u32].into_iter().collect();
        let s = idx.score(&tokens("gateway"), &only2);
        assert!(s.len() == 1 && s[0].0 == 2, "respects allowed");
        idx.insert(2, "kafka lag");
        assert!(idx.score(&tokens("gateway"), &all).is_empty());
        assert_eq!(idx.score(&tokens("kafka"), &all).len(), 1);
    }
}
