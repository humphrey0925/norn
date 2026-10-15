//! Symbol search the way one types it: fragments, in any order, anywhere in
//! the name, or just initials.
//!
//! `cache init` finds `bfd_cache_init` and `init_cache`; `bci` finds
//! `bfd_cache_init` by the first letters of its parts; `Sect` finds
//! `bfd_map_over_sections`. Every word of the query has to match the name,
//! case-insensitively, as a substring, as initials of consecutive parts, or
//! failing both as letters in order. Names are ranked by how well they
//! match: whole name over prefix over start of a part over the middle, and
//! shorter names first.
//!
//! One pass over the name table, in parallel: a table of 100,000 names is
//! searched in about a millisecond, so nothing is indexed for it.

use crate::graph::Graph;
use crate::model::{flag, kind};
use rayon::prelude::*;

/// Does a part of the name start at `i`? After `_`, at a capital after a
/// small letter, at a digit after a letter (`elf32` has two parts).
fn part_start(name: &[u8], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let (p, c) = (name[i - 1], name[i]);
    !p.is_ascii_alphanumeric() || p.is_ascii_lowercase() && c.is_ascii_uppercase() || p.is_ascii_alphabetic() && c.is_ascii_digit()
}

/// How well one word (lowercase) matches the name, or `None`.
fn word_score(name: &[u8], lower: &[u8], word: &[u8]) -> Option<i32> {
    if word.is_empty() {
        return Some(0);
    }
    if word.len() > lower.len() {
        return None;
    }
    // As a substring: the best of its occurrences.
    let mut best: Option<i32> = None;
    for i in 0..=lower.len() - word.len() {
        if lower[i] == word[0] && &lower[i..i + word.len()] == word {
            // In the middle of a part it is the weakest way to match:
            // initials (`bci` for `bfd_cache_init`) say more than `bbci`.
            let mut s = if i == 0 {
                140
            } else if part_start(name, i) {
                125
            } else {
                55
            };
            // Ends where a part ends: a whole part, not a piece of one.
            if i + word.len() == name.len() || part_start(name, i + word.len()) {
                s += 10;
            }
            best = best.max(Some(s));
            if i == 0 {
                break;
            }
        }
    }
    if best.is_some_and(|s| s >= 100) || word.len() < 2 {
        return best;
    }
    // As initials: each letter starts a part, in order (`bci`).
    let mut k = 0;
    let mut skipped = 0;
    for i in (0..lower.len()).filter(|&i| part_start(name, i)) {
        if k < word.len() && lower[i] == word[k] {
            k += 1;
        } else if k > 0 && k < word.len() {
            skipped += 1;
        }
    }
    if k == word.len() {
        return best.max(Some(70 - 5 * skipped));
    }
    // As letters in order, not too far apart.
    let (mut k, mut first, mut last) = (0, 0, 0);
    for (i, &b) in lower.iter().enumerate() {
        if k < word.len() && b == word[k] {
            if k == 0 {
                first = i;
            }
            last = i;
            k += 1;
        }
    }
    let span = last + 1 - first;
    best.max((k == word.len() && span <= word.len() * 3).then(|| 30 - (span - word.len()) as i32))
}

/// How well the query's words match the name, or `None` if one does not.
pub fn score(name: &str, words: &[Vec<u8>]) -> Option<i32> {
    let name = name.as_bytes();
    let lower: Vec<u8> = name.iter().map(u8::to_ascii_lowercase).collect();
    let mut total = 0;
    for w in words {
        total += word_score(name, &lower, w)?;
    }
    // The whole name typed out comes first; then shorter names.
    if words.len() == 1 && words[0] == lower {
        total += 200;
    }
    Some(total - (name.len() / 4) as i32)
}

/// The symbols whose names match `query`, best first, each with its
/// score: of one name, definitions before prototypes and declarations.
pub fn search(g: &Graph, query: &str, limit: usize) -> Vec<(i32, u32)> {
    let words: Vec<Vec<u8>> = query.split_whitespace().map(|w| w.to_ascii_lowercase().into_bytes()).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut names: Vec<(i32, u32)> =
        (0..g.n_names() as u32).into_par_iter().filter_map(|n| Some((score(g.name(n), &words)?, n))).collect();
    // Best score first; names in order among equals, so results are stable.
    names.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut out = Vec::new();
    for (s, n) in names {
        let syms = g.syms_named(n);
        let wanted = |&&x: &&u32| g.syms[x as usize].kind != kind::FILE;
        let declared = |x: &&u32| g.syms[**x as usize].flags & flag::DECL != 0;
        out.extend(syms.iter().filter(wanted).filter(|x| !declared(x)).map(|&x| (s, x)));
        out.extend(syms.iter().filter(wanted).filter(declared).map(|&x| (s, x)));
        if out.len() >= limit {
            out.truncate(limit);
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(query: &str, names: &[&str]) -> Vec<String> {
        let words: Vec<Vec<u8>> = query.split_whitespace().map(|w| w.to_ascii_lowercase().into_bytes()).collect();
        let mut hits: Vec<(i32, &str)> = names.iter().filter_map(|n| Some((score(n, &words)?, *n))).collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        hits.into_iter().map(|h| h.1.to_string()).collect()
    }

    const NAMES: [&str; 9] = [
        "bfd_cache_init",
        "init_cache",
        "bfd_cache_close",
        "cache",
        "bfd_check_init",
        "BfdCacheInit",
        "bfd_map_over_sections",
        "elf32_arm_final_link",
        "precache_initial",
    ];

    #[test]
    fn fragments_match_in_any_order_and_whole_parts_first() {
        assert_eq!(rank("cache init", &NAMES), ["init_cache", "BfdCacheInit", "bfd_cache_init", "precache_initial"]);
        assert_eq!(rank("init cache", &NAMES)[..3], ["init_cache", "BfdCacheInit", "bfd_cache_init"]);
        assert_eq!(rank("cache", &NAMES)[0], "cache");
    }

    #[test]
    fn initials_and_letters_in_order_match_too() {
        assert_eq!(rank("bci", &NAMES), ["BfdCacheInit", "bfd_cache_init", "bfd_check_init"]);
        assert_eq!(rank("eafl", &NAMES), ["elf32_arm_final_link"]);
        assert_eq!(rank("Sect", &NAMES), ["bfd_map_over_sections"]);
        // Letters in order, close together, are enough.
        assert_eq!(rank("mapsec", &NAMES), ["bfd_map_over_sections"]);
        assert!(rank("bfdlink", &NAMES).is_empty(), "too far apart");
        assert_eq!(rank("map sec", &NAMES), ["bfd_map_over_sections"]);
    }

    #[test]
    fn what_does_not_match_is_left_out() {
        assert!(rank("cache zzz", &NAMES).is_empty());
        assert!(rank("x", &NAMES).is_empty());
    }
}
