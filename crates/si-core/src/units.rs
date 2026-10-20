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
use std::ops::Not;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};

/// Per file, the conditions that came out one way in every reading: where
/// the directive starts, and whether it holds.
pub type Verdicts = Vec<Vec<(u32, bool)>>;

/// How each compilation reads the headers that some compilations read
/// one way and others another (`#if (IS_PLAT_A_EN)` true where the
/// platform header came first, false where it did not): what a call in a
/// source file reaches is what its own compilation defines.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Readings {
    /// Those headers, by path, each with the lines its `#if`s, `#elif`s,
    /// `#else`s and `#endif`s are on.
    pub files: Vec<(String, Vec<u32>)>,
    /// Per header, the ways it is read: per stretch of lines (before the
    /// first directive, then after each), whether it is compiled, as a
    /// [`Tri`] (0 no, 1 yes, 2 not known).
    pub ways: Vec<Vec<Box<[u8]>>>,
    /// Per compilation (the path of the file it starts from), the way it
    /// reads each of those headers it reads: (header, way).
    pub units: Vec<(String, Vec<(u32, u32)>)>,
    /// Every file, by path, and per compilation (as `units`) the files it
    /// reads, as bits in that order: what a call in a header reaches is
    /// what the compilations that read the header compile.
    pub paths: Vec<String>,
    pub reads: Vec<Box<[u64]>>,
    /// Per compilation, of the files it reads, those it may not read.
    pub maybe: Vec<Box<[u32]>>,
    /// The files no compilation reads (where it may not, either): headers
    /// included only under `#if`s these conditions switch off, by path.
    #[serde(default)]
    pub unread: Vec<String>,
    /// Per compilation (as `units`), whether it includes a header not in
    /// the tree (one the build makes), which may bring in any.
    #[serde(default)]
    pub generated: Vec<bool>,
    /// The headers no file here includes (as `paths` numbers them): one
    /// the build makes may.
    #[serde(default)]
    pub orphans: Vec<u32>,
    /// For each of those (in that order), when it is read only under the
    /// compilation's own file's `#if`s not known: for each way it is
    /// included, the lines of the branches it is included in there.
    pub under: Vec<Box<[Option<Box<[Box<[u32]>]>>]>>,
}

impl Readings {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.units.is_empty()
    }

    /// What the ways come to, for knowing when expansions made with them
    /// are stale.
    pub fn digest(&self) -> u64 {
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        for (path, lines) in &self.files {
            h.update(path.as_bytes());
            lines.iter().for_each(|l| h.update(&l.to_le_bytes()));
        }
        for ways in &self.ways {
            h.update(&(ways.len() as u32).to_le_bytes());
            ways.iter().for_each(|w| h.update(w));
        }
        for (path, ways) in &self.units {
            h.update(path.as_bytes());
            ways.iter().for_each(|w| h.update(&[w.0.to_le_bytes(), w.1.to_le_bytes()].concat()));
        }
        h.digest()
    }

    /// Whether compilation `unit` reads the file `path` (numbered as in `paths`).
    pub fn reads(&self, unit: u32, path: u32) -> bool {
        self.reads.get(unit as usize).and_then(|b| b.get(path as usize / 64)).is_some_and(|w| w & (1 << (path % 64)) != 0)
    }

    /// Whether it reads it for certain.
    /// Where it may not read it, only for its own file's `#if`s: the lines
    /// of the branches it is included in.
    pub fn read_under(&self, unit: u32, path: u32) -> Option<&[Box<[u32]>]> {
        let i = self.maybe.get(unit as usize)?.binary_search(&path).ok()?;
        self.under.get(unit as usize)?.get(i)?.as_deref()
    }

    pub fn reads_surely(&self, unit: u32, path: u32) -> bool {
        self.reads(unit, path) && self.maybe.get(unit as usize).is_none_or(|m| m.binary_search(&path).is_err())
    }

    /// Whether the compilation `unit` compiles `line` of `file` (both
    /// numbered as here); `Unknown` where it does not read the file.
    pub fn compiled(&self, unit: u32, file: u32, line: u32) -> Tri {
        let ways = &self.units[unit as usize].1;
        let Ok(i) = ways.binary_search_by_key(&file, |w| w.0) else { return Tri::Unknown };
        let way = &self.ways[file as usize][ways[i].1 as usize];
        // (A definition is on a line after the directive that opens its branch.)
        let stretch = self.files[file as usize].1.partition_point(|&l| l < line);
        match way.get(stretch) {
            Some(0) => Tri::False,
            Some(1) => Tri::True,
            _ => Tri::Unknown,
        }
    }
}

/// The ways files are read, numbered per file as they are first seen.
#[derive(Default)]
struct Ways {
    by_file: FxHashMap<u32, FxHashMap<Box<[u8]>, u32>>,
}

impl Ways {
    fn number(&mut self, file: u32, way: Vec<u8>) -> u32 {
        let of = self.by_file.entry(file).or_default();
        let n = of.len() as u32;
        *of.entry(way.into()).or_insert(n)
    }
}

/// More directive steps than this in all, and the tree is left alone:
/// the kernel, where an include of `<asm/...>` may mean any of twenty
/// architectures' and each compilation reads them all, takes three
/// thousand million. Told its architecture (Linux's configuration says,
/// the others are `notBuilt`), it takes under five hundred million, in
/// about fifteen seconds.
const MOST_STEPS: u64 = 600_000_000;
/// `MOST_STEPS`, or `SI_MOST_STEPS` (for measuring what more would cost).
fn most_steps() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("SI_MOST_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(MOST_STEPS))
}
/// More than this in one compilation, and that one is: what it reads
/// then has no verdicts. (Not knowing which of several files an include
/// means, every one is read, and that can multiply without end.)
const MOST_IN_ONE: u64 = 3_000_000;
const DEEPEST: u32 = 200;
/// Ways one condition is kept worked out, at most, on each thread.
const KEPT_WAYS: usize = 8;

/// A condition worked out: the names it depends on (of those some step
/// defines or undefines: how the others are does not change), each with
/// how it was (see [`defined_at`]), and what it came to.
struct Kept {
    names: Box<[(u32, u64)]>,
    unseen: bool,
    holds: Tri,
}

thread_local! {
    /// Per file and step, the conditions this thread worked out. Every
    /// compilation reads the same headers, mostly with the same macros
    /// defined: their conditions come out the same, and working each out
    /// (its macros expanded, their values read, again and again) was most
    /// of the time.
    /// (With the reading it was for: see [`World::pass`].)
    static KEPT: std::cell::RefCell<(u64, Vec<Vec<Kept>>)> = Default::default();
}

/// Readings numbered as they start, all of them in this process.
static PASSES: AtomicU64 = AtomicU64::new(1);

/// How a name is in a compilation, as one number: not mentioned (0), not
/// known (1), undefined (2), or defined by the `#define` at this file and
/// step (the same line in every compilation, so the same number).
fn defined_at(file: u32, step: usize) -> u64 {
    (file as u64 + 1) << 32 | step as u64
}

enum Step {
    /// `kind`: 0 an expression, 1 `defined`, 2 `!defined`.
    /// `slot`: the condition's number among all the tree's expressions
    /// (`kind` 0), where it is kept worked out.
    If { at: u32, kind: u8, expr: Box<[u8]>, slot: u32 },
    Elif { at: u32, kind: u8, expr: Box<[u8]>, slot: u32 },
    Else,
    Endif,
    /// `id`: the name's number in [`World::ids`].
    Define { id: u32, name: Box<[u8]>, value: Box<[u8]> },
    Undef { id: u32, name: Box<[u8]> },
    Include { files: Box<[u32]> },
    /// An include, in quotes, of nothing in the tree.
    Unseen,
}

/// For `SI_COND_WHY=<path end>:<line>`: how the compilations read that
/// one condition, and the names it depends on where they left it open.
#[derive(Default)]
struct Why {
    /// Readings: held, failed, open, in a branch not compiled; and of
    /// all, those of a file maybe read, and those after an include of
    /// something not in the tree.
    counts: [u64; 6],
    /// Per name, in the readings left open: defined, undefined, not
    /// known, not mentioned, given by the settings.
    names: std::collections::BTreeMap<String, [u64; 5]>,
    /// Per name, the definitions read: (file, value) -> how often.
    defined: std::collections::BTreeMap<String, std::collections::BTreeMap<(String, String), u64>>,
    /// Per name, why it was not known: what made it so -> how often.
    unknown: std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>,
    /// The compilations (their first file) that left it open.
    open_in: Vec<String>,
}

struct World<'a> {
    steps: Vec<Vec<Step>>,
    paths: &'a [String],
    /// The steps `SI_COND_WHY` asks about, per file, and what was found.
    why: FxHashMap<u32, FxHashSet<usize>>,
    why_found: std::sync::Mutex<Why>,
    /// `SI_COND_WHY_UNIT=<path end>`: only the compilations of that file.
    why_unit: Option<String>,
    /// Per file, the line of each step that is an `#if`, `#elif` or
    /// `#else` (0 for the others).
    why_lines: Vec<Vec<u32>>,
    /// The names some step defines or undefines, numbered.
    ids: FxHashMap<Box<[u8]>, u32>,
    /// How many expressions there are (see `Step::If`).
    slots: usize,
    /// Files read once in a compilation, however often they are included.
    once: Vec<bool>,
    /// Which reading this is, of all in this process: what a thread kept
    /// of another is not this one's.
    pass: AtomicU64,
    /// Per file, its directory, numbered.
    dir: Vec<u32>,
    plain: &'a Conditions,
    /// The same, with the names the sources define outright not defined.
    told: Conditions,
    /// After a header not in the tree: the plain ones, with the files'
    /// include guards still not defined (no header defines another's).
    unseen: Conditions,
    /// Per file and step, how its condition came out: 1 held, 2 failed, 4 open.
    seen: Vec<Vec<AtomicU8>>,
    /// Per file, whether some compilation reads it (where it may not, too).
    reached: Vec<AtomicBool>,
    /// The files whose reading each compilation keeps (see [`Readings`]).
    traced: Vec<bool>,
    spent: AtomicU64,
    stop: AtomicBool,
}

/// A header of the C standard library, as `#include <...>` names it.
fn standard_header(name: &[u8]) -> bool {
    matches!(
        name,
        b"assert.h" | b"complex.h" | b"ctype.h" | b"errno.h" | b"fenv.h" | b"float.h" | b"inttypes.h" | b"iso646.h" | b"limits.h" | b"locale.h" | b"math.h"
            | b"setjmp.h" | b"signal.h" | b"stdalign.h" | b"stdarg.h" | b"stdatomic.h" | b"stdbool.h" | b"stddef.h" | b"stdint.h" | b"stdio.h" | b"stdlib.h"
            | b"stdnoreturn.h" | b"string.h" | b"tgmath.h" | b"threads.h" | b"time.h" | b"uchar.h" | b"wchar.h" | b"wctype.h"
    )
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

pub(crate) fn names_in<'a>(text: &'a [u8], mut each: impl FnMut(&'a [u8])) {
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
    /// How each name of [`World::ids`] is, as [`defined_at`] says.
    states: Vec<u64>,
    /// For `SI_COND_PROFILE`: conditions found worked out, worked out
    /// anew, and worked out with no room left to keep them.
    kept: std::cell::Cell<[u64; 3]>,
    spent: u64,
    /// For `SI_COND_PROFILE`: conditions worked out and the ns they took,
    /// definitions kept and their ns, includes followed.
    prof: Option<[u64; 5]>,
    /// How this compilation reads the files of [`World::traced`]: per
    /// file, whether each stretch is compiled, as a `Tri`.
    traces: FxHashMap<u32, Vec<u8>>,
    /// While a file that may not be read is read: each name it changes,
    /// with how the name was before (see `Step::Include`).
    journal: Option<Vec<(u32, Box<[u8]>, Option<Option<Option<Vec<u8>>>>, u64)>>,
    /// Per file, the names a reading of it that may not have been made not
    /// known, with how they were before: read for sure, it is read from
    /// there (it would do the same again).
    before: FxHashMap<u32, Vec<(u32, Box<[u8]>, Option<Option<Option<Vec<u8>>>>, u64)>>,
    /// While reading what its own file includes under `#if`s not known:
    /// the lines of those branches; and per file it may not read, the
    /// ways it is included so (each one read wherever those hold).
    under_here: Option<Box<[u32]>>,
    under: FxHashMap<u32, Option<Vec<Box<[u32]>>>>,
    /// The directories of the files it certainly reads.
    dirs: FxHashSet<u32>,
    /// The file the compilation starts from.
    root: u32,
    /// The files it reads, when that is kept (see [`Readings::reads`]).
    files: Option<FxHashMap<u32, bool>>,
    /// For `SI_COND_WHY`: per name not known, the file that made it so,
    /// and how (1: a definition on a line that may not be compiled; 2:
    /// a file that may not be read changed it), with, for 2, the file
    /// that includes it and how many files the include may mean (0:
    /// one, under an `#if` not known).
    unknown_why: FxHashMap<u32, (u32, u8, u32, u32, u32)>,
}

impl Unit<'_> {
    fn conditions(&self) -> &Conditions {
        if self.unseen { &self.w.unseen } else { &self.w.told }
    }

    fn holds(&self, slot: u32, kind: u8, expr: &[u8]) -> Tri {
        match kind {
            0 => self.worked_out(slot, expr),
            1 => self.conditions().defined_in(&self.local, None, expr),
            _ => self.conditions().defined_in(&self.local, None, expr).not(),
        }
    }

    /// An `#if` expression, as worked out before where the names it
    /// depends on are as they were then.
    fn worked_out(&self, slot: u32, expr: &[u8]) -> Tri {
        let found = KEPT.with(|k| {
            let mut k = k.borrow_mut();
            // (A thread may have worked for another reading before.)
            let pass = self.w.pass.load(Relaxed);
            if k.0 != pass {
                *k = (pass, Vec::new());
            }
            let k = &mut k.1;
            if k.len() < self.w.slots {
                k.resize_with(self.w.slots, Vec::new);
            }
            k[slot as usize].iter().find(|w| w.unseen == self.unseen && w.names.iter().all(|&(n, s)| self.states[n as usize] == s)).map(|w| w.holds)
        });
        let mut kept = self.kept.get();
        if let Some(holds) = found {
            kept[0] += 1;
            self.kept.set(kept);
            return holds;
        }
        kept[1] += 1;
        let noted = std::cell::RefCell::new(Vec::new());
        let holds = self.conditions().eval_noting(&self.local, None, expr, Some(&noted));
        let mut names: Vec<u32> = noted.into_inner().iter().filter_map(|n| self.w.ids.get(&n[..]).copied()).collect();
        names.sort_unstable();
        names.dedup();
        let names = names.into_iter().map(|n| (n, self.states[n as usize])).collect();
        KEPT.with(|k| {
            let mut k = k.borrow_mut();
            let ways = &mut k.1[slot as usize];
            if ways.len() < KEPT_WAYS {
                ways.push(Kept { names, unseen: self.unseen, holds });
            } else {
                kept[2] += 1;
            }
        });
        self.kept.set(kept);
        holds
    }

    /// Note, for `SI_COND_WHY`, how step `i` of `file` came out here.
    fn note_why(&self, file: u32, i: usize, live: Tri, holds: Tri, maybe: bool, expr: &[u8]) {
        let w = self.w;
        if !w.why.get(&file).is_some_and(|steps| steps.contains(&i)) {
            return;
        }
        if w.why_unit.as_ref().is_some_and(|u| !w.paths[self.root as usize].ends_with(u.as_str())) {
            return;
        }
        let mut why = w.why_found.lock().unwrap();
        why.counts[match (live, holds) {
            (Tri::False, _) => 3,
            (_, Tri::True) => 0,
            (_, Tri::False) => 1,
            _ => 2,
        }] += 1;
        why.counts[4] += maybe as u64;
        why.counts[5] += self.unseen as u64;
        if live == Tri::False || holds != Tri::Unknown {
            return;
        }
        if why.open_in.len() < 8 {
            why.open_in.push(w.paths[self.root as usize].clone());
        }
        // The names it names, and those their values name, as they are here.
        let mut names: Vec<Vec<u8>> = Vec::new();
        names_in(expr, |n| names.push(n.to_vec()));
        let mut k = 0;
        while k < names.len() && names.len() < 40 {
            if let Some(Some(Some(v))) = self.local.get(&names[k]) {
                let mut more = Vec::new();
                names_in(v, |n| more.push(n.to_vec()));
                for n in more {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
            k += 1;
        }
        for n in names {
            let name = String::from_utf8_lossy(&n).into_owned();
            let state = match self.local.get(&n) {
                Some(Some(Some(v))) => {
                    let at = w.ids.get(&n[..]).map(|&id| self.states[id as usize]).unwrap_or(0);
                    let from = if at >> 32 > 0 { w.paths[(at >> 32) as usize - 1].clone() } else { "?".to_string() };
                    *why.defined.entry(name.clone()).or_default().entry((from, String::from_utf8_lossy(v).into_owned())).or_default() += 1;
                    0
                }
                Some(Some(None)) => 1,
                Some(None) => {
                    let how = w.ids.get(&n[..]).and_then(|id| self.unknown_why.get(id)).map_or("?".to_string(), |&(f, how, from, several, at)| {
                        let path = &w.paths[f as usize];
                        let from = &w.paths[from as usize];
                        match (how, several) {
                            (1, _) => format!("a #define in {path} on a line that may not be compiled"),
                            (_, 0) => format!("changed by {path}, included by {from} under an #if not known (line {at})"),
                            _ => format!("changed by {path}, included by {from} with an include that may mean {several} files"),
                        }
                    });
                    *why.unknown.entry(name.clone()).or_default().entry(how).or_default() += 1;
                    2
                }
                None if self.conditions().settles(&n) => 4,
                None => 3,
            };
            why.names.entry(name).or_default()[state] += 1;
        }
    }

    /// Note that `file`, which may not be read, is included here, under
    /// [`Unit::under_here`].
    fn note_under(&mut self, file: u32) {
        if self.files.is_none() {
            return;
        }
        // (Included here otherwise, as through a header's own `#if` not
        // known: no way to have it, but the others still are.)
        let Some(here) = self.under_here.clone() else { return };
        let ways = self.under.entry(file).or_insert_with(|| Some(Vec::new())).get_or_insert_with(Vec::new);
        if !ways.contains(&here) {
            ways.push(here);
        }
    }

    fn set(&mut self, id: u32, name: &[u8], value: Option<Option<Vec<u8>>>, state: u64) {
        if let Some(j) = &mut self.journal {
            j.push((id, name.into(), self.local.get(name).cloned(), self.states[id as usize]));
        }
        self.local.insert(name.to_vec(), value);
        self.states[id as usize] = state;
    }

    /// Read `file`; `maybe`: it is not known to be read at all.
    fn read(&mut self, file: u32, maybe: bool, depth: u32) {
        let w = self.w;
        let steps = &w.steps[file as usize];
        if w.spent.fetch_add(steps.len() as u64, Relaxed) > most_steps() {
            w.stop.store(true, Relaxed);
        }
        self.spent += steps.len() as u64 + 1;
        if depth > DEEPEST || self.spent > MOST_IN_ONE || w.stop.load(Relaxed) {
            self.spent = u64::MAX / 2;
            return;
        }
        if !w.reached[file as usize].load(Relaxed) {
            w.reached[file as usize].store(true, Relaxed);
        }
        if let Some(files) = &mut self.files {
            *files.entry(file).or_default() |= !maybe;
            // (Read where it may not be for another reason too: no lines.)
            if maybe {
                self.note_under(file);
            }
        }
        // (A file that includes itself without end is read no deeper.)
        let times = self.reading.entry(file).or_default();
        if *times >= 2 {
            return;
        }
        *times += 1;
        if !maybe && self.journal.is_none() {
            self.dirs.insert(w.dir[file as usize]);
            for (id, name, was, state) in self.before.remove(&file).into_iter().flatten() {
                if self.local.get(&name[..]) == Some(&None) {
                    match was {
                        Some(v) => drop(self.local.insert(name.to_vec(), v)),
                        None => drop(self.local.remove(&name[..])),
                    }
                    self.states[id as usize] = state;
                }
            }
        }
        // The open groups: whether a branch so far was the one compiled,
        // and whether the one being read is. `live` is whether this line
        // is compiled, the file being read at all.
        let mut open: Vec<(Tri, Tri, Tri)> = Vec::new();
        // (For `SI_COND_WHY`: the step of each open group's branch.)
        let mut branch: Vec<usize> = Vec::new();
        let mut live = Tri::True;
        // How this compilation reads the file, if that is kept: after a
        // directive, whether what follows is compiled, the file being read.
        // (A definition a call reaches is in a file that was.)
        let mut trace = w.traced[file as usize].then(|| vec![Tri::True as u8]);
        let mut after = false;
        for (i, step) in steps.iter().enumerate() {
            if let Some(t) = trace.as_mut().filter(|_| std::mem::take(&mut after)) {
                t.push(live as u8);
            }
            after = trace.is_some() && matches!(step, Step::If { .. } | Step::Elif { .. } | Step::Else | Step::Endif);
            match step {
                Step::If { kind, expr, slot, .. } => {
                    let t = self.prof.map(|_| std::time::Instant::now());
                    let holds = if live == Tri::False { Tri::False } else { self.holds(*slot, *kind, expr) };
                    self.note_why(file, i, live, holds, maybe, expr);
                    if let (Some(p), Some(t)) = (&mut self.prof, t) {
                        p[0] += 1;
                        p[1] += t.elapsed().as_nanos() as u64;
                    }
                    if live != Tri::False {
                        w.seen[file as usize][i].fetch_or(1 << holds as u8, Relaxed);
                    }
                    open.push((holds, holds, live));
                    branch.push(i);
                    live = live.and(holds);
                }
                Step::Elif { kind, expr, slot, .. } => {
                    let Some(group) = open.last_mut() else { continue };
                    let outer = group.2;
                    let t = self.prof.map(|_| std::time::Instant::now());
                    let own = if outer == Tri::False || group.0 == Tri::True {
                        Tri::False
                    } else {
                        let own = self.holds(*slot, *kind, expr);
                        w.seen[file as usize][i].fetch_or(1 << own as u8, Relaxed);
                        own
                    };
                    self.note_why(file, i, if outer == Tri::False || group.0 == Tri::True { Tri::False } else { outer }, own, maybe, expr);
                    if let (Some(p), Some(t)) = (&mut self.prof, t) {
                        p[0] += 1;
                        p[1] += t.elapsed().as_nanos() as u64;
                    }
                    group.1 = group.0.not().and(own);
                    group.0 = group.0.or(own);
                    live = outer.and(group.1);
                    if let Some(b) = branch.last_mut() {
                        *b = i;
                    }
                }
                Step::Else => {
                    let Some(group) = open.last_mut() else { continue };
                    group.1 = group.0.not();
                    group.0 = Tri::True;
                    live = group.2.and(group.1);
                    if let Some(b) = branch.last_mut() {
                        *b = i;
                    }
                }
                Step::Endif => {
                    if let Some(group) = open.pop() {
                        live = group.2;
                    }
                    branch.pop();
                }
                _ if live == Tri::False => {}
                Step::Define { id, name, value } => {
                    let t = self.prof.map(|_| std::time::Instant::now());
                    if let Some(p) = &mut self.prof {
                        p[2] += 1;
                    }
                    // (In a file that may not be read, what follows in it is
                    // read only if it is: see `Step::Include`.)
                    if live == Tri::True {
                        if let Some(outright) = self.outright.as_mut().filter(|_| !maybe) {
                            if !self.local.contains_key(&name[..]) && !w.plain.settles(name) {
                                outright.insert(name.to_vec());
                            }
                        }
                        self.set(*id, name, Some(Some(value.to_vec())), defined_at(file, i));
                    } else if self.local.get(&name[..]).is_none_or(|v| v.as_ref().and_then(|v| v.as_deref()) != Some(&value[..])) {
                        self.set(*id, name, None, 1);
                        if !w.why.is_empty() {
                            self.unknown_why.insert(*id, (file, 1, 0, 0, 0));
                        }
                    }
                    if let (Some(p), Some(t)) = (&mut self.prof, t) {
                        p[3] += t.elapsed().as_nanos() as u64;
                    }
                }
                Step::Undef { id, name } => {
                    if live == Tri::True {
                        self.set(*id, name, Some(None), 2);
                    } else if self.local.get(&name[..]) != Some(&Some(None)) {
                        self.set(*id, name, None, 1);
                        if !w.why.is_empty() {
                            self.unknown_why.insert(*id, (file, 1, 0, 0, 0));
                        }
                    }
                }
                // (Where it may not be included, what follows is read as if
                // it were not: what else could it be read as.)
                Step::Unseen if live == Tri::True => self.unseen = true,
                Step::Unseen => {}
                Step::Include { files } => {
                    if let Some(p) = &mut self.prof {
                        p[4] += files.len() as u64;
                    }
                    // Of several files an include may mean, those in a
                    // directory the compilation certainly reads from (as
                    // its include path would have it), if not all.
                    let near: Vec<u32>;
                    let files: &[u32] = if files.len() > 1 {
                        near = files.iter().copied().filter(|&f| self.dirs.contains(&w.dir[f as usize])).collect();
                        if !near.is_empty() && near.len() < files.len() { &near } else { files }
                    } else {
                        files
                    };
                    // Included under the compilation's own file's `#if`s
                    // not known, and only so: the branches.
                    let own = file == self.root && !maybe && live != Tri::True && files.len() == 1;
                    let was_under = self.under_here.clone();
                    if !own && (live != Tri::True || files.len() > 1) {
                        self.under_here = None;
                    }
                    if own {
                        let lines = open.iter().zip(&branch).filter(|(g, _)| g.1 != Tri::True).map(|(_, &s)| w.why_lines[file as usize].get(s).copied().unwrap_or(0));
                        self.under_here = Some(lines.collect());
                    }
                    let maybe = maybe || live != Tri::True || files.len() > 1;
                    // A file that may not be read is read as if it were:
                    // its own conditions come after its own lines. Here,
                    // where it may not have been, a name it changed is not
                    // known. (Each of several files an include may mean is
                    // read from how things are here.)
                    let unsure = maybe && self.journal.is_none();
                    let mut changed: Vec<(u32, Box<[u8]>, u32)> = Vec::new();
                    let mut before: Vec<(u32, (u32, Box<[u8]>, Option<Option<Option<Vec<u8>>>>, u64))> = Vec::new();
                    for &f in files.iter() {
                        if w.once[f as usize] {
                            match self.read.get(&f) {
                                Some(true) => continue,
                                Some(false) if maybe => {
                                    // (Included here too, read before.)
                                    self.note_under(f);
                                    continue;
                                }
                                _ => drop(self.read.insert(f, !maybe)),
                            }
                        }
                        if unsure {
                            self.journal = Some(Vec::new());
                        }
                        self.read(f, maybe, depth + 1);
                        // (Within, the journal goes on for the outermost to take.)
                        if let Some(journal) = unsure.then(|| self.journal.take()).flatten() {
                            // (A name's first entry has how it was before.)
                            let mut named: FxHashSet<&[u8]> = FxHashSet::default();
                            for (id, name, was, state) in &journal {
                                if named.insert(&name[..]) && was.as_ref() != self.local.get(&name[..]) {
                                    changed.push((*id, name.clone(), f));
                                    // (The file that defined it last.)
                                    if let Some(by) = (self.states[*id as usize] >> 32).checked_sub(1) {
                                        before.push((by as u32, (*id, name.clone(), was.clone(), *state)));
                                    }
                                }
                            }
                            for (by, entry) in before.drain(..) {
                                self.before.entry(by).or_default().push(entry);
                            }
                            for (id, name, was, state) in journal.into_iter().rev() {
                                match was {
                                    Some(v) => drop(self.local.insert(name.to_vec(), v)),
                                    None => drop(self.local.remove(&name[..])),
                                }
                                self.states[id as usize] = state;
                            }
                        }
                    }
                    self.under_here = was_under;
                    for (id, name, f) in changed {
                        if self.local.get(&name[..]) != Some(&None) {
                            self.set(id, &name, None, 1);
                            if !w.why.is_empty() {
                                // (The innermost branch not surely compiled.)
                                let at = open.iter().zip(&branch).rev().find(|(g, _)| g.1 != Tri::True).map_or(0, |(_, &s)| w.why_lines[file as usize].get(s).copied().unwrap_or(0));
                                self.unknown_why.insert(id, (f, 2, file, if files.len() > 1 { files.len() as u32 } else { 0 }, at));
                            }
                        }
                    }
                }
            }
        }
        if let Some(mut t) = trace {
            if after {
                t.push(live as u8);
            }
            // Read twice over: compiled where either reading compiles it.
            match self.traces.get_mut(&file) {
                Some(was) if was.len() == t.len() => was.iter_mut().zip(&t).for_each(|(a, &b)| *a = tri(*a).or(tri(b)) as u8),
                Some(_) => {}
                None => drop(self.traces.insert(file, t)),
            }
        }
        if let Some(times) = self.reading.get_mut(&file) {
            *times -= 1;
        }
    }
}

fn tri(b: u8) -> Tri {
    match b {
        0 => Tri::False,
        1 => Tri::True,
        _ => Tri::Unknown,
    }
}

/// Read every compilation of the tree: `paths` as the files are indexed,
/// `scripts` their directives. `None` when the tree is too much to read.
pub fn verdicts(paths: &[String], scripts: &[Script], conditions: &Conditions) -> Option<Verdicts> {
    read_tree(paths, scripts, conditions).map(|r| r.0)
}

/// The same, and how each compilation reads the headers compiled more
/// than one way.
pub fn read_tree(paths: &[String], scripts: &[Script], conditions: &Conditions) -> Option<(Verdicts, Readings)> {
    let n = paths.len();
    // The names a condition can be asked about: those the conditions
    // name, and those the values of such names name (in the sources or
    // in the settings: `-DPLAT=PLAT_B` asks about `PLAT_B`).
    let mut asked: FxHashSet<&[u8]> = FxHashSet::default();
    for op in scripts.iter().flat_map(|s| &s.ops) {
        if let Op::If { expr, .. } | Op::Elif { expr, .. } = op {
            names_in(expr, |name| _ = asked.insert(name));
        }
    }
    loop {
        let before = asked.len();
        let set: Vec<&[u8]> = asked.iter().filter_map(|n| conditions.setting(n)).collect();
        for value in set {
            names_in(value, |name| _ = asked.insert(name));
        }
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
        let mut fitting: Vec<u32> = named.iter().filter(fits).copied().collect();
        // (Of a folder the build compiles, where one is: not another
        // architecture's.)
        let built = |c: &u32| !crate::link::outside(&paths[*c as usize], conditions.not_built());
        if fitting.iter().any(built) {
            fitting.retain(built);
        }
        let nearest = fitting.iter().map(|&c| near(c)).max();
        fitting.into_iter().filter(|&c| Some(near(c)) == nearest).collect()
    };
    let mut ids: FxHashMap<Box<[u8]>, u32> = FxHashMap::default();
    for op in scripts.iter().flat_map(|s| &s.ops) {
        if let Op::Define { name, .. } | Op::Undef { name } = op {
            if asked.contains(&name[..]) && !conditions.settles(name) && !ids.contains_key(&name[..]) {
                ids.insert(name[..].into(), ids.len() as u32);
            }
        }
    }
    let mut included = vec![false; n];
    let mut steps: Vec<Vec<Step>> = scripts
        .par_iter()
        .zip(paths)
        .map(|(script, path)| {
            let cut = |b: &[u8]| -> Box<[u8]> { b.into() };
            script
                .ops
                .iter()
                .filter_map(|op| {
                    Some(match op {
                        Op::If { at, kind, expr } => Step::If { at: *at, kind: *kind, expr: cut(expr), slot: u32::MAX },
                        Op::Elif { at, kind, expr } => Step::Elif { at: *at, kind: *kind, expr: cut(expr), slot: u32::MAX },
                        Op::Else => Step::Else,
                        Op::Endif => Step::Endif,
                        // (What the settings say of a name has the last word.)
                        Op::Define { name, value } if asked.contains(&name[..]) && !conditions.settles(name) => Step::Define { id: ids[&name[..]], name: cut(name), value: cut(value) },
                        Op::Undef { name } if asked.contains(&name[..]) && !conditions.settles(name) => Step::Undef { id: ids[&name[..]], name: cut(name) },
                        Op::Define { .. } | Op::Undef { .. } => return None,
                        // (`<assert.h>` is the C library's, not a header of
                        // that name somewhere in the tree.)
                        Op::Include { name, quoted: false } if standard_header(name) => return None,
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
    // The directories of the files, numbered.
    let dir: Vec<u32> = {
        let mut dirs: FxHashMap<&str, u32> = FxHashMap::default();
        paths.iter().map(|p| { let n = dirs.len() as u32; *dirs.entry(dir_of(p)).or_insert(n) }).collect()
    };
    // Of several files an include may mean, those in the directory the
    // sources most include from elsewhere (as an include path would have
    // it), when that is not all of them.
    let mut from_elsewhere = vec![0u32; n];
    for (f, step) in steps.iter().enumerate().flat_map(|(f, s)| s.iter().map(move |s| (f, s))) {
        if let Step::Include { files } = step {
            if let [one] = files[..] {
                if dir[one as usize] != dir[f] {
                    from_elsewhere[dir[one as usize] as usize] += 1;
                }
            }
        }
    }
    for step in steps.iter_mut().flatten() {
        if let Step::Include { files } = step {
            if files.len() > 1 {
                let most = files.iter().map(|&f| from_elsewhere[dir[f as usize] as usize]).max().unwrap_or(0);
                let near: Box<[u32]> = files.iter().copied().filter(|&f| from_elsewhere[dir[f as usize] as usize] == most).collect();
                if most > 0 && near.len() < files.len() {
                    *files = near;
                }
            }
        }
    }
    // What a compilation starts from: a file that is not a header, or a
    // header nothing here includes (it is somebody's, out of sight).
    // (Not those in folders the build does not compile. A file of this
    // program the configuration leaves out is read as it would be.)
    let folders: Vec<String> = conditions.not_built().iter().filter(|f| f.ends_with('/')).cloned().collect();
    let roots: Vec<u32> = (0..n as u32)
        .filter(|&i| !crate::is_header(&paths[i as usize]) || !included[i as usize])
        .filter(|&i| !crate::link::outside(&paths[i as usize], &folders))
        .collect();
    // What every compilation reads before its own file (the build's
    // `-include`).
    let read_first: Vec<u32> = conditions.include_first().iter().filter_map(|f| paths.iter().position(|p| p == f)).map(|i| i as u32).collect();
    let sizes: Vec<usize> = steps.iter().map(Vec::len).collect();
    let seen = || sizes.iter().map(|&n| (0..n).map(|_| AtomicU8::new(0)).collect()).collect();
    let mut slots = 0;
    for step in steps.iter_mut().flatten() {
        if let Step::If { kind: 0, slot, .. } | Step::Elif { kind: 0, slot, .. } = step {
            *slot = slots;
            slots += 1;
        }
    }
    // `SI_COND_WHY=ring_api.h:201`: the `#if`s and `#elif`s on that line of
    // the files whose path ends so.
    let mut why: FxHashMap<u32, FxHashSet<usize>> = FxHashMap::default();
    if let Some((end, line)) = std::env::var("SI_COND_WHY").ok().and_then(|v| v.rsplit_once(':').and_then(|(e, l)| Some((e.replace('\\', "/"), l.parse::<u32>().ok()?)))) {
        for (f, p) in paths.iter().enumerate().filter(|(_, p)| *p == &end || p.ends_with(&format!("/{end}"))) {
            let mut d = 0;
            for (i, step) in steps[f].iter().enumerate() {
                if matches!(step, Step::If { .. } | Step::Elif { .. } | Step::Else | Step::Endif) {
                    if matches!(step, Step::If { .. } | Step::Elif { .. }) && scripts[f].lines.get(d) == Some(&line) {
                        why.entry(f as u32).or_default().insert(i);
                    }
                    d += 1;
                }
            }
            eprintln!("why       {p}:{line}: {} directive(s)", why.get(&(f as u32)).map_or(0, |s| s.len()));
        }
    }
    if std::env::var_os("SI_COND_PROFILE").is_some() {
        // Includes that may mean more than one file, and quoted ones of
        // nothing here, by the name written.
        let mut several: FxHashMap<String, (u64, usize)> = FxHashMap::default();
        let mut missing: FxHashMap<String, u64> = FxHashMap::default();
        let mut all = 0u64;
        for (script, path) in scripts.iter().zip(paths) {
            for op in &script.ops {
                if let Op::Include { name, quoted } = op {
                    all += 1;
                    let files = files_of(path, name);
                    let name = String::from_utf8_lossy(name).into_owned();
                    if files.len() > 1 {
                        let e = several.entry(name).or_default();
                        e.0 += 1;
                        e.1 = e.1.max(files.len());
                    } else if files.is_empty() && *quoted {
                        *missing.entry(name).or_default() += 1;
                    }
                }
            }
        }
        let mut top: Vec<(String, (u64, usize))> = several.into_iter().collect();
        top.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
        let ambiguous: u64 = top.iter().map(|t| t.1.0).sum();
        eprintln!("includes  {ambiguous} of {all} include lines may mean more than one file: {}", top.iter().take(12).map(|t| format!("{} ({} files) {}", t.0, t.1.1, t.1.0)).collect::<Vec<_>>().join(", "));
        let mut top: Vec<(String, u64)> = missing.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let unseen: u64 = top.iter().map(|t| t.1).sum();
        eprintln!("includes  {unseen} quoted include lines name nothing here: {}", top.iter().take(12).map(|t| format!("{} {}", t.0, t.1)).collect::<Vec<_>>().join(", "));
    }
    let mut w = World {
        paths,
        why_lines: {
            steps
                .iter()
                .zip(scripts)
                .map(|(steps, script)| {
                    let mut d = 0;
                    steps.iter().map(|step| match step {
                        Step::If { .. } | Step::Elif { .. } | Step::Else | Step::Endif => {
                            d += 1;
                            script.lines.get(d - 1).copied().unwrap_or(0)
                        }
                        _ => 0,
                    }).collect()
                })
                .collect()
        },
        why,
        why_found: Default::default(),
        why_unit: std::env::var("SI_COND_WHY_UNIT").ok().map(|u| u.replace('\\', "/")),
        ids,
        slots: slots as usize,
        once: scripts.iter().map(|s| s.once).collect(),
        pass: AtomicU64::new(0),
        dir,
        plain: conditions,
        told: conditions.clone(),
        unseen: conditions.clone(),
        seen: seen(),
        reached: (0..n).map(|_| AtomicBool::new(false)).collect(),
        traced: vec![false; n],
        steps,
        spent: AtomicU64::new(0),
        stop: AtomicBool::new(false),
    };
    // (Compilation, the ways it reads the traced files it reads.)
    let ways: std::sync::Mutex<Ways> = Default::default();
    let read_as: std::sync::Mutex<Vec<(u32, Vec<(u32, u32)>, Vec<(u32, bool)>, FxHashMap<u32, Option<Vec<Box<[u32]>>>>, bool)>> = Default::default();
    // Returns the names defined outright (on the first pass), and the
    // compilations that were too much to read.
    let profile = std::env::var_os("SI_COND_PROFILE").is_some();
    // (Per compilation: ms, steps, local names, and the counts of `prof`.)
    let profiled: std::sync::Mutex<Vec<(f64, u64, usize, u32, [u64; 5])>> = Default::default();
    let kept_counts: std::sync::Mutex<[u64; 3]> = Default::default();
    let read_all = |w: &World, first: bool| -> (FxHashSet<Vec<u8>>, Vec<u32>) {
        // (What was worked out under other conditions does not hold now.)
        w.pass.store(PASSES.fetch_add(1, Relaxed), Relaxed);
        roots
            .par_iter()
            .fold(Default::default, |mut all: (FxHashSet<Vec<u8>>, Vec<u32>), &root| {
                let t = std::time::Instant::now();
                let mut unit = Unit {
                    w,
                    local: Local::default(),
                    // A header nothing here includes is included by
                    // something out of sight, after who knows what.
                    unseen: crate::is_header(&paths[root as usize]),
                    read: FxHashMap::default(),
                    outright: first.then(FxHashSet::default),
                    reading: FxHashMap::default(),
                    states: vec![0; w.ids.len()],
                    kept: Default::default(),
                    spent: 0,
                    prof: profile.then_some([0; 5]),
                    traces: FxHashMap::default(),
                    journal: None,
                    before: FxHashMap::default(),
                    dirs: FxHashSet::default(),
                    under_here: None,
                    under: FxHashMap::default(),
                    root,
                    files: Some(FxHashMap::default()),
                    unknown_why: FxHashMap::default(),
                };
                for &f in read_first.iter().filter(|&&f| f != root) {
                    unit.read.insert(f, true);
                    unit.read(f, false, 0);
                }
                unit.read.insert(root, true);
                unit.read(root, false, 0);
                if let Some(p) = unit.prof {
                    let mut k = kept_counts.lock().unwrap();
                    (0..3).for_each(|i| k[i] += unit.kept.get()[i]);
                    drop(k);
                    profiled.lock().unwrap().push((t.elapsed().as_secs_f64() * 1000.0, unit.spent, unit.local.len(), root, p));
                }
                if unit.spent > MOST_IN_ONE {
                    all.1.push(root);
                } else {
                    all.0.extend(unit.outright.into_iter().flatten());
                    if let Some(files) = unit.files {
                        let mut read: Vec<(u32, Vec<u8>)> = unit.traces.into_iter().collect();
                        read.sort_unstable_by_key(|r| r.0);
                        let mut ways = ways.lock().unwrap();
                        let read = read.into_iter().map(|(f, t)| (f, ways.number(f, t))).collect();
                        drop(ways);
                        read_as.lock().unwrap().push((root, read, files.into_iter().collect(), unit.under, unit.unseen && !crate::is_header(&paths[root as usize])));
                    }
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
    let (mut outright, mut left) = read_all(&w, true);
    // So is a file's include guard, though more follow its `#endif` or come
    // before its `#ifndef`.
    let mut guards: Vec<Vec<u8>> = scripts
        .iter()
        .zip(paths)
        .filter(|(_, p)| crate::is_header(p))
        .filter_map(|(s, _)| crate::mask::guard_name(&s.ops))
        .filter(|g| !conditions.settles(g))
        .collect();
    // And a guard within a file (`#ifndef __CORE_CM4_H_DEPENDANT`, a bare
    // `#define` of it next): a name every definition of which is such, and
    // no other file tests (`support_p` another tests is a switch).
    {
        let mut inner: FxHashMap<Vec<u8>, (bool, FxHashSet<usize>)> = FxHashMap::default();
        for (f, s) in scripts.iter().enumerate() {
            for (k, op) in s.ops.iter().enumerate() {
                if let Op::Define { name, value } = op {
                    let guarded = k > 0 && matches!(&s.ops[k - 1], Op::If { kind: 2, expr, .. } if expr == name) && value.is_empty();
                    let e = inner.entry(name.clone()).or_insert((true, FxHashSet::default()));
                    e.0 &= guarded;
                    e.1.insert(f);
                }
            }
        }
        inner.retain(|n, e| e.0 && !conditions.settles(n));
        for (f, s) in scripts.iter().enumerate() {
            for op in &s.ops {
                if let Op::If { expr, .. } | Op::Elif { expr, .. } = op {
                    names_in(expr, |n| {
                        if let Some(e) = inner.get_mut(n) {
                            if !e.1.contains(&f) {
                                e.0 = false;
                            }
                        }
                    });
                }
            }
        }
        guards.extend(inner.into_iter().filter(|(_, e)| e.0).map(|(n, _)| n));
    }
    w.unseen = conditions.with_undefined(guards.iter().cloned());
    outright.extend(guards);
    // So is a name defined only in files no compilation reads (another
    // chip's header): no source a compilation reads brings it in.
    {
        let mut defining: FxHashMap<&[u8], (bool, bool)> = FxHashMap::default();
        for (f, steps) in w.steps.iter().enumerate() {
            let read = w.reached[f].load(Relaxed);
            for step in steps {
                if let Step::Define { name, .. } = step {
                    let e = defining.entry(&name[..]).or_insert((false, false));
                    *e = (e.0 || read, true);
                }
            }
        }
        let elsewhere: Vec<Vec<u8>> = defining.into_iter().filter(|(n, (read, _))| !read && !conditions.settles(n) && !crate::cond::builtin(n)).map(|(n, _)| n.to_vec()).collect();
        // (Nor does a header the build makes.)
        w.unseen = w.unseen.with_undefined(elsewhere.iter().cloned());
        outright.extend(elsewhere);
    }
    if profile {
        let mut rows = std::mem::take(&mut *profiled.lock().unwrap());
        let mut sum = [0u64; 5];
        rows.iter().for_each(|r| (0..5).for_each(|i| sum[i] += r.4[i]));
        let (values, bytes) = rayon::broadcast(|_| crate::cond::COUNTS.with(|c| c.take())).into_iter().chain([crate::cond::COUNTS.with(|c| c.take())]).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let ms: f64 = rows.iter().map(|r| r.0).sum();
        eprintln!("unitsprof {ms:.0} ms in all units; {} conditions {:.0} ms; {} defines {:.0} ms; {} includes; {values} macro values read; {bytes} bytes tokenized", sum[0], sum[1] as f64 / 1e6, sum[2], sum[3] as f64 / 1e6, sum[4]);
        let k = kept_counts.lock().unwrap();
        eprintln!("unitskept {} found worked out, {} worked out anew, {} of them with no room to keep", k[0], k[1], k[2]);
        drop(k);
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        for r in rows.iter().take(5) {
            eprintln!("unitslow  {:.0} ms, {} steps, {} names, {} conditions {:.0} ms, {} defines {:.0} ms, {} includes: {}", r.0, r.1, r.2, r.4[0], r.4[1] as f64 / 1e6, r.4[2], r.4[3] as f64 / 1e6, r.4[4], paths[r.3 as usize]);
        }
        eprintln!("units     {} roots of {} files, {} steps, {} names defined outright, {} compilations left{}", roots.len(), n, w.spent.load(Relaxed), outright.len(), left.len(), if w.stop.load(Relaxed) { ", given up" } else { "" });
    }
    if w.stop.load(Relaxed) {
        return None;
    }
    // A file some reading left open or decided otherwise than another is
    // one each compilation's reading of is kept (knowing the names
    // defined outright decides more, never otherwise).
    let undecided = |w: &World, f: usize| {
        w.steps[f].iter().zip(&w.seen[f]).any(|(step, seen)| matches!(step, Step::If { .. } | Step::Elif { .. }) && !matches!(seen.load(Relaxed), 0..=2))
    };
    w.traced = (0..n).map(|f| undecided(&w, f)).collect();
    if !outright.is_empty() || w.traced.iter().any(|t| *t) {
        if !outright.is_empty() {
            w.told = conditions.with_undefined(outright);
        }
        w.seen = seen();
        w.spent = AtomicU64::new(0);
        w.why_found = Default::default();
        // (What the first reading kept, this one keeps anew.)
        read_as.lock().unwrap().clear();
        left = read_all(&w, false).1;
        if w.stop.load(Relaxed) {
            return None;
        }
    }
    rayon::broadcast(|_| KEPT.with(|k| std::mem::take(&mut *k.borrow_mut())));
    KEPT.with(|k| std::mem::take(&mut *k.borrow_mut()));
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
    // Of the files kept, those decided one way in some compilations and
    // another in others (open in all, a compilation's reading says no
    // more).
    let split: Vec<bool> = (0..n)
        .map(|f| {
            w.traced[f]
                && !unread[f]
                && w.steps[f].iter().zip(&w.seen[f]).any(|(step, seen)| {
                    let s = seen.load(Relaxed);
                    matches!(step, Step::If { .. } | Step::Elif { .. }) && s & 3 != 0 && !matches!(s, 1 | 2)
                })
        })
        .collect();
    if !w.why.is_empty() {
        let why = w.why_found.lock().unwrap();
        let c = why.counts;
        eprintln!("why       read {} times: held {}, failed {}, open {}, in a branch not compiled {}; of a file maybe read {}, after an include of nothing here {}", c[0] + c[1] + c[2] + c[3], c[0], c[1], c[2], c[3], c[4], c[5]);
        for (name, n) in &why.names {
            eprintln!("why       {name}: defined {}, undefined {}, not known {}, not mentioned {}, by the settings {}", n[0], n[1], n[2], n[3], n[4]);
            for ((from, value), k) in why.defined.get(name).into_iter().flatten().take(4) {
                eprintln!("why         {k}x {from} = {value}");
            }
            for (how, k) in why.unknown.get(name).into_iter().flatten().take(4) {
                eprintln!("why         {k}x not known: {how}");
            }
        }
        if !why.open_in.is_empty() {
            eprintln!("why       left open in: {}", why.open_in.join(", "));
        }
        // What each of those includes that may still mean several files.
        let mut said = FxHashSet::default();
        for root in &why.open_in {
            let Some(f) = paths.iter().position(|p| p == root) else { continue };
            for step in &w.steps[f] {
                if let Step::Include { files } = step {
                    if files.len() > 1 && said.insert((f, files.clone())) {
                        eprintln!("why       {root} includes one of {}:", files.len());
                        for &c in files.iter() {
                            eprintln!("why         {}", paths[c as usize]);
                        }
                    }
                }
            }
        }
    }
    // (What a compilation left unread reaches may be read.)
    let mut readings = Readings {
        unread: (0..n).filter(|&f| !w.reached[f].load(Relaxed) && !unread[f]).map(|f| paths[f].clone()).collect(),
        ..Default::default()
    };
    let mut number = vec![u32::MAX; n];
    let ways = ways.into_inner().unwrap();
    // A header's ways are numbered as the compilations met them, which the
    // threads decide: in order of what they are instead, the same each time
    // (what was expanded with them is kept while they are).
    let mut renumber: FxHashMap<u32, FxHashMap<u32, u32>> = FxHashMap::default();
    for f in (0..n).filter(|&f| split[f]) {
        number[f] = readings.files.len() as u32;
        readings.files.push((paths[f].clone(), scripts[f].lines.clone()));
        let mut of: Vec<(&Box<[u8]>, &u32)> = ways.by_file.get(&(f as u32)).into_iter().flatten().collect();
        of.sort_unstable_by(|a, b| a.0.cmp(b.0));
        renumber.insert(f as u32, of.iter().enumerate().map(|(i, w)| (*w.1, i as u32)).collect());
        readings.ways.push(of.into_iter().map(|w| w.0.clone()).collect());
    }
    let mut read_as = read_as.into_inner().unwrap();
    read_as.sort_unstable_by_key(|r| r.0);
    for (root, read, files, under, generated) in read_as {
        if unread[root as usize] {
            continue;
        }
        let read: Vec<(u32, u32)> = read.iter().filter(|r| split[r.0 as usize]).map(|&(f, way)| (number[f as usize], renumber[&f].get(&way).copied().unwrap_or(way))).collect();
        readings.units.push((paths[root as usize].clone(), read));
        readings.generated.push(generated);
        let mut bits = vec![0u64; n.div_ceil(64)];
        files.iter().for_each(|&(f, _)| bits[f as usize / 64] |= 1 << (f % 64));
        readings.reads.push(bits.into());
        let mut maybe: Vec<u32> = files.iter().filter(|f| !f.1).map(|f| f.0).collect();
        maybe.sort_unstable();
        readings.under.push(maybe.iter().map(|f| under.get(f).cloned().flatten().map(Vec::into_boxed_slice)).collect());
        readings.maybe.push(maybe.into());
    }
    if !readings.units.is_empty() {
        readings.paths = paths.to_vec();
        readings.orphans = (0..n as u32).filter(|&f| crate::is_header(&paths[f as usize]) && !included[f as usize]).collect();
    }
    // (A way that does not fit the lines is no reading: see `Readings::compiled`.)
    for (f, ways) in readings.ways.iter_mut().enumerate() {
        let stretches = readings.files[f].1.len() + 1;
        for way in ways.iter_mut().filter(|w| w.len() != stretches) {
            *way = vec![Tri::Unknown as u8; stretches].into();
        }
    }
    Some((verdicts, readings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn told(files: &[(&str, &str)]) -> Vec<Vec<(String, bool)>> {
        told_under(files, &Conditions::default())
    }

    fn told_under(files: &[(&str, &str)], conditions: &Conditions) -> Vec<Vec<(String, bool)>> {
        let paths: Vec<String> = files.iter().map(|f| f.0.to_string()).collect();
        let scripts: Vec<Script> = files.iter().map(|f| crate::mask::script(f.1.as_bytes())).collect();
        let verdicts = verdicts(&paths, &scripts, conditions).unwrap();
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

    #[test]
    fn each_compilation_is_known_to_read_a_header_its_own_way() {
        let files = [
            ("good.c", "#include \"env.h\"
#include \"ring.h\"
"),
            ("spoil.c", "#include \"ring.h\"
"),
            ("env.h", "#define PLAT_A_EN 1
"),
            ("ring.h", "#if PLAT_A_EN
int a;
#else
int b;
#endif
int both;
"),
        ];
        let paths: Vec<String> = files.iter().map(|f| f.0.to_string()).collect();
        let scripts: Vec<Script> = files.iter().map(|f| crate::mask::script(f.1.as_bytes())).collect();
        let (verdicts, readings) = read_tree(&paths, &scripts, &Conditions::default()).unwrap();
        assert_eq!(verdicts[3], []);
        assert_eq!(readings.files, [("ring.h".to_string(), vec![1, 3, 5])]);
        let unit = |p: &str| readings.units.iter().position(|u| u.0 == p).unwrap() as u32;
        let at = |p: &str, line: u32| readings.compiled(unit(p), 0, line);
        assert_eq!([at("good.c", 2), at("good.c", 4), at("good.c", 6)], [Tri::True, Tri::False, Tri::True]);
        assert_eq!([at("spoil.c", 2), at("spoil.c", 4), at("spoil.c", 6)], [Tri::False, Tri::True, Tri::True]);
    }

    #[test]
    fn a_header_the_build_includes_first_is_read_before_the_file() {
        // Kbuild's `-include include/linux/kconfig.h`: no file includes it,
        // every compilation has read it before its first line.
        let files = [
            ("include/linux/kconfig.h", "#define ON 1
"),
            ("a.c", "#if ON
int a;
#endif
"),
        ];
        let settings = crate::cond::Settings { include_first: vec!["include/linux/kconfig.h".into()], ..Default::default() };
        let first = crate::cond::Conditions::from_settings(&settings, std::path::Path::new("."));
        assert_eq!(told(&files)[1], [one("#if ON", false)]);
        assert_eq!(told_under(&files, &first)[1], [one("#if ON", true)]);
    }

    #[test]
    fn a_header_is_read_as_things_stood_where_it_was_included() {
        // FreeRTOS's `tasks.c`: `mpu_wrappers.h` renames nothing there,
        // whatever is undefined after.
        let files = [
            ("tasks.c", "#define FROM_API
#include \"w.h\"
#undef FROM_API
"),
            ("app.c", "#include \"w.h\"
"),
            ("w.h", "#ifndef W_H
#define W_H
#ifndef FROM_API
#define make_task MPU_make_task
#endif
#endif
"),
        ];
        let paths: Vec<String> = files.iter().map(|f| f.0.to_string()).collect();
        let scripts: Vec<Script> = files.iter().map(|f| crate::mask::script(f.1.as_bytes())).collect();
        let (_, readings) = read_tree(&paths, &scripts, &Conditions::default()).unwrap();
        let unit = |p: &str| readings.units.iter().position(|u| u.0 == p).unwrap() as u32;
        let h = readings.files.iter().position(|f| f.0 == "w.h").unwrap() as u32;
        assert_eq!([readings.compiled(unit("tasks.c"), h, 4), readings.compiled(unit("app.c"), h, 4)], [Tri::False, Tri::True]);
    }

    #[test]
    fn a_file_that_may_not_be_read_reads_its_own_lines_as_written() {
        let got = told(&[
            ("a.c", "#ifdef BUILD_SAYS\n#include \"h.h\"\n#endif\n#ifdef X\n#endif\n#include \"env.h\"\n#if PLAT == 1\n#endif\n"),
            ("h.h", "#define X 1\n#if X\n#endif\n"),
            ("one/env.h", "#define PLAT 1\n#if PLAT == 1\n#endif\n"),
            ("two/env.h", "#define PLAT 2\n#if PLAT == 2\n#endif\n"),
        ]);
        // Read at all, h.h has defined X by its `#if`; a.c may not have read it.
        assert_eq!(got[1], [one("#if X", true)]);
        assert_eq!(got[0], []);
        // Each of the two `env.h`s an include may mean, on its own.
        assert_eq!(got[2], [one("#if PLAT == 1", true)]);
        assert_eq!(got[3], [one("#if PLAT == 2", true)]);
    }

    #[test]
    fn what_a_file_that_may_not_be_read_changes_after_an_include_is_not_known() {
        let got = told(&[
            ("a.c", "#ifdef BUILD_SAYS\n#include \"h.h\"\n#endif\n#ifdef X\n#endif\n"),
            ("h.h", "#include \"inner.h\"\n#define X 1\n"),
            ("inner.h", ""),
            ("b.c", "#ifdef Q\n#define BUILD_SAYS\n#endif\n"),
        ]);
        assert_eq!(got[0], []);
    }

    #[test]
    fn a_header_not_in_the_tree_does_not_define_another_s_guard() {
        let got = told(&[
            ("a.c", "#include \"generated.h\"\n#include \"g.h\"\n#if SOC == 1\n#endif\n"),
            ("g.h", "#ifndef G_H\n#define G_H\n#define SOC 1\n#endif\n#ifdef MORE\n#endif\n"),
            ("b.c", "#ifdef Q\n#define MORE\n#endif\n"),
        ]);
        assert_eq!(got[0], [one("#if SOC == 1", true)]);
    }

    #[test]
    fn a_name_only_another_chip_s_header_defines_is_not_defined() {
        let got = told(&[
            ("a.c", "#define SOC 1\n#include \"chip.h\"\n#ifdef BANK1\n#endif\n#include \"core.h\"\n#if V == 1\n#endif\n"),
            ("chip.h", "#if SOC == 1\n#include \"c1.h\"\n#elif SOC == 2\n#include \"c2.h\"\n#endif\n"),
            ("c1.h", ""),
            ("c2.h", "#define BANK1 1\n"),
            // A guard within the file: defined nowhere else, tested nowhere else.
            ("core.h", "#include \"c1.h\"\n#ifndef CORE_DEP\n#define CORE_DEP\n#define V 1\n#endif\n#define AFTER 1\n"),
        ]);
        assert_eq!(got[0], [one("#ifdef BANK1", false), one("#if V == 1", true)]);
    }

    #[test]
    fn a_default_is_no_include_guard() {
        // binutils: `sframe-opt.c` gives `support_sframe_p` a default; a
        // target's header the build picks may define it.
        for default in ["#include \"as.h\"\n#ifndef support_p\n#define support_p\n#endif\n", "#include \"as.h\"\n#ifndef support_p\n# define support_p() 0\n#endif\n"] {
            for at in ["opt.h", "opt.c"] {
                let got = told(&[("gen.c", "#include \"targ-cpu.h\"\n#ifdef support_p\n#endif\n"), (at, default)]);
                assert_eq!(got[0], [], "{at}: {default}");
            }
        }
    }

    #[test]
    fn an_include_guard_after_an_include_is_one_all_the_same() {
        let got = told(&[
            ("a.c", "#include \"g.h\"\n#if SOC == 1\n#endif\n"),
            ("g.h", "#include \"x.h\"\n#ifndef G_H\n#define G_H\n#define SOC 1\n#endif\n"),
            ("x.h", ""),
        ]);
        assert_eq!(got[0], [one("#if SOC == 1", true)]);
    }

    #[test]
    fn of_two_headers_an_include_may_mean_the_one_beside_those_read_is_read() {
        let got = told(&[
            ("src/app/a.c", "#include \"symbol.h\"\n#include \"typedef.h\"\n#if SOC == 1\n#endif\n"),
            ("src/common/symbol.h", ""),
            ("src/common/typedef.h", "#define SOC 1\n"),
            ("src/drv/x/typedef.h", "#define SOC 2\n"),
        ]);
        assert_eq!(got[0], [one("#if SOC == 1", true)]);
    }

    #[test]
    fn of_two_headers_an_include_may_mean_the_one_where_the_sources_include_from_is_read() {
        let got = told(&[
            ("src/app/a.c", "#include \"typedef.h\"\n#if SOC == 1\n#endif\n"),
            ("src/hal/b.c", "#include \"symbol.h\"\n"),
            ("src/common/symbol.h", ""),
            ("src/common/typedef.h", "#define SOC 1\n"),
            ("src/drv/x/typedef.h", "#define SOC 2\n"),
        ]);
        assert_eq!(got[0], [one("#if SOC == 1", true)]);
    }

    #[test]
    fn a_header_read_for_sure_after_it_may_have_been_is_read_from_before() {
        // A guard not seen as one: read first where it may not be, its
        // name is then not known; read for sure, the header is read all the same.
        for guard in ["#if !defined(SETUP_H)
#define SETUP_H
", "#ifndef SETUP_H
#define SETUP_H
#include \"env.h\"
"] {
            let setup = format!("{guard}#include \"env.h\"
#endif
#include \"more.h\"
");
            let got = told(&[
                ("a.c", "#ifdef BUILD_SAYS
#include \"x.h\"
#endif
#include \"setup.h\"
#if SOC == 1
#endif
"),
                ("x.h", "#include \"setup.h\"
"),
                ("setup.h", &setup),
                ("env.h", "#define SOC 1
"),
                ("more.h", ""),
                ("b.c", "#ifdef Q\n#define BUILD_SAYS\n#endif\n"),
            ]);
            assert_eq!(got[0], [one("#if SOC == 1", true)], "{guard}");
        }
    }

    #[test]
    fn a_value_the_settings_give_is_followed_into_the_sources() {
        // `PLAT_B` is only ever defined: the setting is what names it.
        let files = [
            ("main.c", "#include \"cfg.h\"
#define SEL (PLAT)
#define IS_A (SEL == PLAT_A)
#if (IS_A)
#endif
"),
            ("cfg.h", "#define PLAT_A (17)
#define PLAT_B (23)
"),
        ];
        for (plat, holds) in [("PLAT_B", false), ("PLAT_A", true)] {
            let mut c = Conditions::default();
            c.define("PLAT", plat);
            assert_eq!(told_under(&files, &c)[0], [one("#if (IS_A)", holds)]);
        }
    }
}
