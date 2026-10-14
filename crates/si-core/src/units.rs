//! What each `#if` comes to, compilation by compilation.
//!
//! A condition is decided by what was defined when the compiler got to it,
//! and that by the order things were read in: the source file from its
//! first line, each `#include` where it stands. So each source file is
//! read here the way a preprocessor reads it — only its directives, and of
//! the macros only those some condition could be asked about — keeping
//! what is defined, undefined, or (after a line that may or may not be
//! compiled) not known.
//!
//! A header is read by many compilations, and its conditions need not come
//! out the same in each. A condition that holds in every reading holds;
//! one that fails in every reading fails; any other is left open.
//!
//! Two things are taken as said without being written:
//!
//! * A macro defined outright (`#define X`, on a line that is certainly
//!   compiled, the name not having been mentioned before it) was not
//!   defined before that line: C forbids defining it twice. So the build
//!   does not define it on the command line, and where no definition has
//!   been read it is not defined.
//! * A header named in quotes that is not in the tree (one the build
//!   generates) may define anything: from there on, a name nothing read so
//!   far has settled is not known.

use crate::cond::{Conditions, Local, Tri};
use crate::mask::{Op, Script};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};

/// Per file, the conditions that came out one way in every reading: where
/// the directive starts, and whether it holds.
pub type Verdicts = Vec<Vec<(u32, bool)>>;

/// More directive steps than this in all, and the tree is left alone:
/// the kernel, where an include of `<asm/...>` may mean any of twenty
/// architectures' and each compilation reads them all, takes three
/// thousand million, to decide three conditions in a thousand.
const MOST_STEPS: u64 = 200_000_000;
/// More than this in one compilation, and that one is: what it reads
/// then has no verdicts. (Not knowing which of several files an include
/// means, every one is read, and that can multiply without end.)
const MOST_IN_ONE: u64 = 3_000_000;
const DEEPEST: u32 = 200;

enum Step {
    /// `kind`: 0 an expression, 1 `defined`, 2 `!defined`.
    If { at: u32, kind: u8, expr: Box<[u8]> },
    Elif { at: u32, kind: u8, expr: Box<[u8]> },
    Else,
    Endif,
    Define { name: Box<[u8]>, value: Box<[u8]> },
    Undef { name: Box<[u8]> },
    Include { files: Box<[u32]> },
    /// An include, in quotes, of nothing in the tree.
    Unseen,
}

struct World<'a> {
    steps: Vec<Vec<Step>>,
    /// Files read once in a compilation, however often they are included.
    once: Vec<bool>,
    plain: &'a Conditions,
    /// The same, with the names the sources define outright not defined.
    told: Conditions,
    /// Per file and step, how its condition came out: 1 held, 2 failed, 4 open.
    seen: Vec<Vec<AtomicU8>>,
    spent: AtomicU64,
    stop: AtomicBool,
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for p in rel.split(['/', '\\']) {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(p),
        }
    }
    parts.join("/")
}

fn names_in<'a>(text: &'a [u8], mut each: impl FnMut(&'a [u8])) {
    let mut i = 0;
    while i < text.len() {
        if text[i].is_ascii_alphabetic() || text[i] == b'_' {
            let a = i;
            while i < text.len() && (text[i].is_ascii_alphanumeric() || text[i] == b'_') {
                i += 1;
            }
            each(&text[a..i]);
        } else if text[i].is_ascii_digit() {
            while i < text.len() && (text[i].is_ascii_alphanumeric() || text[i] == b'_' || text[i] == b'.') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

/// One compilation being read.
struct Unit<'a> {
    w: &'a World<'a>,
    local: Local,
    /// A generated header has been read: see the module's comment.
    unseen: bool,
    /// Read-once files read so far, and of those the ones certainly read.
    read: FxHashMap<u32, bool>,
    /// First pass: names defined outright.
    outright: Option<FxHashSet<Vec<u8>>>,
    /// Files being read, with how many times over (a file may include
    /// itself, having defined something to read itself differently).
    reading: FxHashMap<u32, u8>,
    spent: u64,
}

impl Unit<'_> {
    fn conditions(&self) -> &Conditions {
        if self.unseen { self.w.plain } else { &self.w.told }
    }

    fn holds(&self, kind: u8, expr: &[u8]) -> Tri {
        match kind {
            0 => self.conditions().eval_in(&self.local, None, expr),
            1 => self.conditions().defined_in(&self.local, None, expr),
            _ => self.conditions().defined_in(&self.local, None, expr).not(),
        }
    }

    /// Read `file`; `maybe`: it is not known to be read at all.
    fn read(&mut self, file: u32, maybe: bool, depth: u32) {
        let w = self.w;
        let steps = &w.steps[file as usize];
        if w.spent.fetch_add(steps.len() as u64, Relaxed) > MOST_STEPS {
            w.stop.store(true, Relaxed);
        }
        self.spent += steps.len() as u64 + 1;
        if depth > DEEPEST || self.spent > MOST_IN_ONE || w.stop.load(Relaxed) {
            self.spent = u64::MAX / 2;
            return;
        }
        // (A file that includes itself without end is read no deeper.)
        let times = self.reading.entry(file).or_default();
        if *times >= 2 {
            return;
        }
        *times += 1;
        // The open groups: whether a branch so far was the one compiled,
        // and whether the one being read is. `live` is whether this line
        // is compiled, the file being read at all.
        let mut open: Vec<(Tri, Tri, Tri)> = Vec::new();
        let mut live = Tri::True;
        for (i, step) in steps.iter().enumerate() {
            match step {
                Step::If { kind, expr, .. } => {
                    let holds = if live == Tri::False { Tri::False } else { self.holds(*kind, expr) };
                    if live != Tri::False {
                        w.seen[file as usize][i].fetch_or(1 << holds as u8, Relaxed);
                    }
                    open.push((holds, holds, live));
                    live = live.and(holds);
                }
                Step::Elif { kind, expr, .. } => {
                    let Some(group) = open.last_mut() else { continue };
                    let outer = group.2;
                    let own = if outer == Tri::False || group.0 == Tri::True {
                        Tri::False
                    } else {
                        let own = match kind {
                            0 => if self.unseen { w.plain } else { &w.told }.eval_in(&self.local, None, expr),
                            1 => if self.unseen { w.plain } else { &w.told }.defined_in(&self.local, None, expr),
                            _ => if self.unseen { w.plain } else { &w.told }.defined_in(&self.local, None, expr).not(),
                        };
                        w.seen[file as usize][i].fetch_or(1 << own as u8, Relaxed);
                        own
                    };
                    group.1 = group.0.not().and(own);
                    group.0 = group.0.or(own);
                    live = outer.and(group.1);
                }
                Step::Else => {
                    let Some(group) = open.last_mut() else { continue };
                    group.1 = group.0.not();
                    group.0 = Tri::True;
                    live = group.2.and(group.1);
                }
                Step::Endif => {
                    if let Some(group) = open.pop() {
                        live = group.2;
                    }
                }
                _ if live == Tri::False => {}
                Step::Define { name, value } => {
                    if live == Tri::True && !maybe {
                        if let Some(outright) = &mut self.outright {
                            if !self.local.contains_key(&name[..]) && !w.plain.settles(name) {
                                outright.insert(name.to_vec());
                            }
                        }
                        self.local.insert(name.to_vec(), Some(Some(value.to_vec())));
                    } else if self.local.get(&name[..]).is_none_or(|v| v.as_ref().and_then(|v| v.as_deref()) != Some(&value[..])) {
                        self.local.insert(name.to_vec(), None);
                    }
                }
                Step::Undef { name } => {
                    if live == Tri::True && !maybe {
                        self.local.insert(name.to_vec(), Some(None));
                    } else if self.local.get(&name[..]) != Some(&Some(None)) {
                        self.local.insert(name.to_vec(), None);
                    }
                }
                Step::Unseen => self.unseen = true,
                Step::Include { files } => {
                    let maybe = maybe || live != Tri::True || files.len() > 1;
                    for &f in files.iter() {
                        if w.once[f as usize] {
                            match self.read.get(&f) {
                                Some(true) => continue,
                                Some(false) if maybe => continue,
                                _ => drop(self.read.insert(f, !maybe)),
                            }
                        }
                        self.read(f, maybe, depth + 1);
                    }
                }
            }
        }
        if let Some(times) = self.reading.get_mut(&file) {
            *times -= 1;
        }
    }
}

/// Read every compilation of the tree: `paths` as the files are indexed,
/// `scripts` their directives. `None` when the tree is too much to read.
pub fn verdicts(paths: &[String], scripts: &[Script], conditions: &Conditions) -> Option<Verdicts> {
    let n = paths.len();
    // The names a condition can be asked about: those the conditions
    // name, and those the values of such names name.
    let mut asked: FxHashSet<&[u8]> = FxHashSet::default();
    for op in scripts.iter().flat_map(|s| &s.ops) {
        if let Op::If { expr, .. } | Op::Elif { expr, .. } = op {
            names_in(expr, |name| _ = asked.insert(name));
        }
    }
    loop {
        let before = asked.len();
        for op in scripts.iter().flat_map(|s| &s.ops) {
            if let Op::Define { name, value } = op {
                if asked.contains(&name[..]) {
                    names_in(value, |name| _ = asked.insert(name));
                }
            }
        }
        if asked.len() == before {
            break;
        }
    }
    // Which file an include reads: the one beside the includer, else the
    // ones of that name nearest to it.
    let by_path: FxHashMap<&str, u32> = paths.iter().enumerate().map(|(i, p)| (p.as_str(), i as u32)).collect();
    let mut by_base: FxHashMap<&str, Vec<u32>> = FxHashMap::default();
    for (i, p) in paths.iter().enumerate() {
        by_base.entry(p.rsplit('/').next().unwrap_or(p)).or_default().push(i as u32);
    }
    let files_of = |from: &str, name: &[u8]| -> Vec<u32> {
        let name = String::from_utf8_lossy(name);
        let dir = dir_of(from);
        if let Some(&id) = by_path.get(join(dir, &name).as_str()) {
            return vec![id];
        }
        let tail = join("", &name);
        let named = by_base.get(tail.rsplit('/').next().unwrap_or(&tail)).map_or(&[][..], |v| &v[..]);
        let fits = |id: &&u32| {
            let p = paths[**id as usize].as_str();
            p == tail || p.ends_with(&tail) && p.as_bytes()[p.len() - tail.len() - 1] == b'/'
        };
        let near = |id: u32| dir.split('/').zip(dir_of(&paths[id as usize]).split('/')).take_while(|(a, b)| a == b).count();
        let fitting: Vec<u32> = named.iter().filter(fits).copied().collect();
        let nearest = fitting.iter().map(|&c| near(c)).max();
        fitting.into_iter().filter(|&c| Some(near(c)) == nearest).collect()
    };
    let mut included = vec![false; n];
    let steps: Vec<Vec<Step>> = scripts
        .par_iter()
        .zip(paths)
        .map(|(script, path)| {
            let cut = |b: &[u8]| -> Box<[u8]> { b.into() };
            script
                .ops
                .iter()
                .filter_map(|op| {
                    Some(match op {
                        Op::If { at, kind, expr } => Step::If { at: *at, kind: *kind, expr: cut(expr) },
                        Op::Elif { at, kind, expr } => Step::Elif { at: *at, kind: *kind, expr: cut(expr) },
                        Op::Else => Step::Else,
                        Op::Endif => Step::Endif,
                        // (What the settings say of a name has the last word.)
                        Op::Define { name, value } if asked.contains(&name[..]) && !conditions.settles(name) => Step::Define { name: cut(name), value: cut(value) },
                        Op::Undef { name } if asked.contains(&name[..]) && !conditions.settles(name) => Step::Undef { name: cut(name) },
                        Op::Define { .. } | Op::Undef { .. } => return None,
                        Op::Include { name, quoted } => match files_of(path, name) {
                            files if !files.is_empty() => Step::Include { files: files.into() },
                            _ if *quoted => Step::Unseen,
                            _ => return None,
                        },
                    })
                })
                .collect()
        })
        .collect();
    for step in steps.iter().flatten() {
        if let Step::Include { files } = step {
            files.iter().for_each(|&f| included[f as usize] = true);
        }
    }
    // What a compilation starts from: a file that is not a header, or a
    // header nothing here includes (it is somebody's, out of sight).
    let roots: Vec<u32> = (0..n as u32).filter(|&i| !paths[i as usize].ends_with(".h") || !included[i as usize]).collect();
    let sizes: Vec<usize> = steps.iter().map(Vec::len).collect();
    let seen = || sizes.iter().map(|&n| (0..n).map(|_| AtomicU8::new(0)).collect()).collect();
    let mut w = World {
        once: scripts.iter().map(|s| s.once).collect(),
        plain: conditions,
        told: conditions.clone(),
        seen: seen(),
        steps,
        spent: AtomicU64::new(0),
        stop: AtomicBool::new(false),
    };
    // Returns the names defined outright (on the first pass), and the
    // compilations that were too much to read.
    let read_all = |w: &World, first: bool| -> (FxHashSet<Vec<u8>>, Vec<u32>) {
        roots
            .par_iter()
            .fold(Default::default, |mut all: (FxHashSet<Vec<u8>>, Vec<u32>), &root| {
                let mut unit = Unit {
                    w,
                    local: Local::default(),
                    // A header nothing here includes is included by
                    // something out of sight, after who knows what.
                    unseen: paths[root as usize].ends_with(".h"),
                    read: FxHashMap::default(),
                    outright: first.then(FxHashSet::default),
                    reading: FxHashMap::default(),
                    spent: 0,
                };
                unit.read.insert(root, true);
                unit.read(root, false, 0);
                if unit.spent > MOST_IN_ONE {
                    all.1.push(root);
                } else {
                    all.0.extend(unit.outright.into_iter().flatten());
                }
                all
            })
            .reduce(Default::default, |mut a, b| {
                a.0.extend(b.0);
                a.1.extend(b.1);
                a
            })
    };
    // Once to learn which names the sources define outright, and then
    // knowing that.
    let (outright, mut left) = read_all(&w, true);
    if std::env::var_os("SI_COND_PROFILE").is_some() {
        eprintln!("units     {} roots of {} files, {} steps, {} names defined outright, {} compilations left{}", roots.len(), n, w.spent.load(Relaxed), outright.len(), left.len(), if w.stop.load(Relaxed) { ", given up" } else { "" });
    }
    if w.stop.load(Relaxed) {
        return None;
    }
    if !outright.is_empty() {
        w.told = conditions.with_undefined(outright);
        w.seen = seen();
        w.spent = AtomicU64::new(0);
        left = read_all(&w, false).1;
        if w.stop.load(Relaxed) {
            return None;
        }
    }
    // What a compilation left unread may read in a way nobody saw.
    let mut unread = vec![false; n];
    while let Some(f) = left.pop() {
        if !std::mem::replace(&mut unread[f as usize], true) {
            for step in &w.steps[f as usize] {
                if let Step::Include { files } = step {
                    left.extend(files.iter().copied().filter(|&t| !unread[t as usize]));
                }
            }
        }
    }
    let verdicts = w
        .steps
        .iter()
        .zip(&w.seen)
        .zip(&unread)
        .map(|((steps, seen), unread)| {
            if *unread {
                return Vec::new();
            }
            let one = |(step, seen): (&Step, &AtomicU8)| {
                let (Step::If { at, .. } | Step::Elif { at, .. }) = step else { return None };
                match seen.load(Relaxed) {
                    s if s == 1 << Tri::True as u8 => Some((*at, true)),
                    s if s == 1 << Tri::False as u8 => Some((*at, false)),
                    _ => None,
                }
            };
            steps.iter().zip(seen).filter_map(one).collect()
        })
        .collect();
    Some(verdicts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn told(files: &[(&str, &str)]) -> Vec<Vec<(String, bool)>> {
        let paths: Vec<String> = files.iter().map(|f| f.0.to_string()).collect();
        let scripts: Vec<Script> = files.iter().map(|f| crate::mask::script(f.1.as_bytes())).collect();
        let verdicts = verdicts(&paths, &scripts, &Conditions::default()).unwrap();
        let line = |f: usize, at: u32| files[f].1[at as usize..].lines().next().unwrap().to_string();
        verdicts.iter().enumerate().map(|(f, v)| v.iter().map(|&(at, holds)| (line(f, at), holds)).collect()).collect()
    }

    fn one(line: &str, holds: bool) -> (String, bool) {
        (line.to_string(), holds)
    }

    #[test]
    fn a_condition_is_decided_by_what_was_read_before_it() {
        let got = told(&[
            ("a.c", "#include \"early.h\"\n#define LATE 1\n#include \"late.h\"\n"),
            ("early.h", "#ifndef EARLY_H\n#define EARLY_H\n#ifdef LATE\nint early;\n#endif\n#endif\n"),
            ("late.h", "#pragma once\n#if LATE == 1\nint late;\n#elif LATE == 2\nint later;\n#endif\n#ifdef BUILD_SAYS\n#endif\n"),
        ]);
        // LATE is defined outright, so it is not defined before that line;
        // what nothing here defines stays open.
        assert_eq!(got[1], [one("#ifdef LATE", false)]);
        assert_eq!(got[2], [one("#if LATE == 1", true)]);
    }

    #[test]
    fn a_header_read_two_ways_is_left_open_where_they_differ() {
        let got = told(&[
            ("with.c", "#define WITH_TABLE\n#include \"rows.def\"\n"),
            ("without.c", "#include \"rows.def\"\n"),
            ("both.c", "#define SHORT\n#include \"rows.def\"\n#undef SHORT\n#include \"rows.def\"\n"),
            ("rows.def", "#ifdef WITH_TABLE\nrow\n#endif\n#ifdef NEVER\n#elif defined(SHORT)\n#else\n#endif\n#ifndef WITH_TABLE\n#define FALLBACK 1\n#endif\n#if FALLBACK\n#endif\n"),
        ]);
        assert_eq!(got[3], []);
    }

    #[test]
    fn a_generated_header_leaves_what_is_not_settled_open() {
        let files = [
            ("a.c", "#include \"switch.h\"\n#ifdef SWITCH\n#endif\n#include <stdio.h>\n#ifdef OTHER\n#endif\n#include \"generated.h\"\n#ifdef OTHER\n#endif\n#ifdef SWITCH\n#endif\n"),
            ("b.c", "#define OTHER\n"),
            ("switch.h", "#define SWITCH 1\n"),
        ];
        let got = told(&files);
        assert_eq!(got[0], [one("#ifdef SWITCH", true), one("#ifdef OTHER", false), one("#ifdef SWITCH", true)]);
    }

    #[test]
    fn a_line_that_may_not_be_compiled_leaves_its_name_open() {
        let got = told(&[
            ("a.c", "#define A 1\n#ifdef BUILD_SAYS\n#undef A\n#define B\n#endif\n#if A\n#endif\n#ifdef B\n#endif\n#ifdef BUILD_SAYS\n#include \"h.h\"\n#endif\n#ifdef H\n#endif\n"),
            ("c.c", "#define B\n#define H\n"),
            ("h.h", "#define H\n"),
        ]);
        assert_eq!(got[0], []);
    }
}
