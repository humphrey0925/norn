//! Global link: per-file facts -> one graph. O(names log names + defs + refs),
//! plus the include closures, O(sum of closure sizes).

use crate::graph::Graph;
use crate::model::{flag, kind, pack_meta, refkind, root, EdgeHalf, FileFacts, FileRec, MemberHint, Sym, NONE};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::time::Instant;

/// Upper bound on targets emitted for one ambiguous reference.
/// Structs that may share a member name before a use of the name is left
/// unresolved.
const MAX_MEMBER_CANDIDATES: usize = 8;
const MAX_CANDIDATES: usize = 128;

#[derive(Clone, Debug, Default)]
pub struct LinkStats {
    pub refs: u64,
    pub resolved: u64,
    pub ambiguous: u64,
    pub unresolved: u64,
    pub capped: u64,
    pub edges: u64,
    /// `#include` operands that named a file in the index, and that did not
    /// (system headers, generated files).
    pub includes_found: u64,
    pub includes_missing: u64,
    pub include_ms: f64,
    /// Uses of expanding macros, and what expanding them added.
    pub sites: u64,
    pub expanded_defs: u64,
    pub expanded_refs: u64,
    pub expand_ms: f64,
    /// Where a function is stored in a member or variable: file and line of
    /// the store, the member or variable, the function.
    pub bind_sites: Vec<[u32; 4]>,
    pub intern_ms: f64,
    pub syms_ms: f64,
    pub resolve_ms: f64,
    pub csr_ms: f64,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// The adjacency lists the other way round: for each symbol, the edges
/// that end at it, in the order of where they start. `next` is scratch,
/// as long as `off`.
fn transposed(n: usize, off: &[u32], adj: &[EdgeHalf], mut next: Vec<u32>) -> (Vec<u32>, Vec<EdgeHalf>) {
    let mut back_off = vec![0u32; n + 1];
    for e in adj {
        back_off[e.other as usize + 1] += 1;
    }
    for i in 0..n {
        back_off[i + 1] += back_off[i];
    }
    next.copy_from_slice(&back_off);
    let mut back = vec![EdgeHalf { other: 0, meta: 0 }; adj.len()];
    for from in 0..n {
        for e in &adj[off[from] as usize..off[from + 1] as usize] {
            let slot = &mut next[e.other as usize];
            back[*slot as usize] = EdgeHalf { other: from as u32, meta: e.meta };
            *slot += 1;
        }
    }
    (back_off, back)
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// `dir` joined with `rel`, with `.` and `..` components folded away.
fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for p in rel.split('/') {
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

/// Some of the compilations: a few by number, or many as bits.
enum Units {
    Few(Vec<u32>),
    Many(Vec<u64>),
}

impl Units {
    fn of(n: usize, mut has: impl FnMut(u32) -> bool) -> Units {
        let few: Vec<u32> = (0..n as u32).filter(|&u| has(u)).collect();
        if few.len() < 64 {
            return Units::Few(few);
        }
        let mut bits = vec![0u64; n.div_ceil(64)];
        for u in few {
            bits[u as usize / 64] |= 1 << (u % 64);
        }
        Units::Many(bits)
    }

    /// These, numbered among `n`, sorted.
    fn from_list(n: usize, list: Vec<u32>) -> Units {
        if list.len() < 64 {
            return Units::Few(list);
        }
        let mut bits = vec![0u64; n.div_ceil(64)];
        for u in list {
            bits[u as usize / 64] |= 1 << (u % 64);
        }
        Units::Many(bits)
    }

    /// As bits, numbered among `n`.
    fn bits(&self, n: usize) -> Vec<u64> {
        match self {
            Units::Many(bits) => bits.clone(),
            Units::Few(few) => {
                let mut bits = vec![0u64; n.div_ceil(64)];
                for &u in few {
                    bits[u as usize / 64] |= 1 << (u % 64);
                }
                bits
            }
        }
    }

    /// Whether any of them is among `among` (as bits).
    fn meets(&self, among: &[u64]) -> bool {
        match self {
            Units::Few(few) => few.iter().any(|&u| among.get(u as usize / 64).is_some_and(|w| w & (1 << (u % 64)) != 0)),
            Units::Many(bits) => bits.iter().zip(among).any(|(a, b)| a & b != 0),
        }
    }
}

/// Which compilations read each file, and each way of reading a header
/// read more than one way: worked out when first asked, for the link.
struct Readers<'a> {
    readings: &'a crate::units::Readings,
    /// Per path: those reading it, and those reading it for certain.
    /// (Made at once, turning each compilation's list of what it reads
    /// over: asking every compilation, file by file, took minutes. Made
    /// before the files are resolved in parallel: made inside, on first
    /// asking, the threads waiting for it would have been those it needs.)
    paths: Vec<(Units, Units)>,
    /// Per header: per way, those reading it that way.
    ways: std::sync::Mutex<FxHashMap<u32, std::sync::Arc<Vec<Units>>>>,
}

impl<'a> Readers<'a> {
    fn new(readings: &'a crate::units::Readings) -> Readers<'a> {
        Readers { readings, paths: Readers::turned_over(readings), ways: Default::default() }
    }

    fn path(&self, p: u32) -> &(Units, Units) {
        &self.paths[p as usize]
    }

    fn turned_over(r: &crate::units::Readings) -> Vec<(Units, Units)> {
        {
            let (n, m) = (r.units.len(), r.paths.len());
            let mut reading: Vec<Vec<u32>> = vec![Vec::new(); m];
            for (u, bits) in r.reads.iter().enumerate() {
                for (i, &w) in bits.iter().enumerate() {
                    let mut w = w;
                    while w != 0 {
                        if let Some(list) = reading.get_mut(i * 64 + w.trailing_zeros() as usize) {
                            list.push(u as u32);
                        }
                        w &= w - 1;
                    }
                }
            }
            let mut maybe: Vec<Vec<u32>> = vec![Vec::new(); m];
            for (u, paths) in r.maybe.iter().enumerate() {
                for &p in paths.iter() {
                    if let Some(list) = maybe.get_mut(p as usize) {
                        list.push(u as u32);
                    }
                }
            }
            reading
                .into_par_iter()
                .zip(maybe)
                .map(|(all, maybe)| {
                    let sure = all.iter().copied().filter(|u| maybe.binary_search(u).is_err()).collect();
                    (Units::from_list(n, all), Units::from_list(n, sure))
                })
                .collect()
        }
    }

    fn ways(&self, h: u32) -> std::sync::Arc<Vec<Units>> {
        if let Some(r) = self.ways.lock().unwrap().get(&h) {
            return r.clone();
        }
        let n = self.readings.units.len();
        let way = |u: u32| self.readings.units[u as usize].1.binary_search_by_key(&h, |w| w.0).ok().map(|i| self.readings.units[u as usize].1[i].1);
        let r = std::sync::Arc::new((0..self.readings.ways[h as usize].len() as u32).map(|w| Units::of(n, |u| way(u) == Some(w))).collect::<Vec<Units>>());
        self.ways.lock().unwrap().insert(h, r.clone());
        r
    }

    /// What the compilations among `among` (as bits) that read `file`
    /// compile of it (`h`: its number as a header read more than one
    /// way, with the stretch of lines; else `p`, its path, and whether
    /// the definition is under an `#if`); `None` if none reads it. They
    /// say yes where some say yes and none no (one that does not know may
    /// just not read the line), no where all say no.
    fn compile(&self, among: &[u64], h: u32, stretch: u32, p: u32, conditional: bool) -> Option<crate::cond::Tri> {
        use crate::cond::Tri;
        if h != NONE {
            let ways = self.ways(h);
            let mut n = [false; 3];
            for (w, units) in ways.iter().enumerate() {
                if units.meets(among) {
                    n[match self.readings.ways[h as usize][w].get(stretch as usize) { Some(1) => 0, Some(0) => 1, _ => 2 }] = true;
                }
            }
            return Some(match n {
                [false, false, false] => return None,
                [true, false, _] => Tri::True,
                [false, true, false] => Tri::False,
                _ => Tri::Unknown,
            });
        }
        let r = self.path(p);
        if !r.0.meets(among) {
            return None;
        }
        Some(if !conditional && r.1.meets(among) { Tri::True } else { Tri::Unknown })
    }
}

/// Whether `path` is in one of `folders` (each `a/b/`).
pub(crate) fn outside(path: &str, folders: &[String]) -> bool {
    // A folder as `a/b/`, a file as it is. (Many, sorted, as the
    // conditions keep them: the file and each folder it is in looked up.)
    let one = |f: &String| if f.ends_with('/') { path.starts_with(f.as_str()) } else { path == f };
    if folders.len() <= 16 {
        return folders.iter().any(one);
    }
    let has = |p: &str| folders.binary_search_by(|f| f.as_str().cmp(p)).is_ok();
    has(path) || path.match_indices('/').any(|(i, _)| has(&path[..=i]))
}

/// For each file, the files it includes directly. Without the build's `-I`
/// list an operand is matched by path suffix: the includer's own directory
/// wins, then the candidates sharing the longest directory prefix with it.
/// What the build knows and the sources do not say.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `(from, to)`: an `#include` of a file named `from` reads `to`
    /// instead, for headers the build generates from a template
    /// (`bfd.h` from `bfd-in2.h`).
    pub include_aliases: Vec<(String, String)>,
    /// How each compilation reads the headers compiled more than one way
    /// (see `units`).
    pub readings: std::sync::Arc<crate::units::Readings>,
    /// Folders the build does not compile, as `a/b/` (`notBuilt`).
    pub not_built: Vec<String>,
}

/// How an `#include` operand is found among the indexed files, without the
/// build's `-I` list (see [`Options`]).
pub(crate) struct Includes<'a> {
    paths: Vec<&'a str>,
    by_path: FxHashMap<&'a str, u32>,
    by_base: FxHashMap<&'a str, Vec<u32>>,
    /// Per file, whether it is in a folder the build does not compile.
    outside: Vec<bool>,
}

impl<'a> Includes<'a> {
    pub(crate) fn new(paths: impl IntoIterator<Item = &'a str>) -> Includes<'a> {
        let paths: Vec<&str> = paths.into_iter().collect();
        let by_path = paths.iter().enumerate().map(|(i, p)| (*p, i as u32)).collect();
        let mut by_base: FxHashMap<&str, Vec<u32>> = FxHashMap::default();
        for (i, p) in paths.iter().enumerate() {
            by_base.entry(p.rsplit('/').next().unwrap_or(p)).or_default().push(i as u32);
        }
        let outside = vec![false; paths.len()];
        Includes { paths, by_path, by_base, outside }
    }

    /// The same, the files in `folders` (`a/b/`) not compiled by the build.
    pub(crate) fn not_built(mut self, folders: &[String]) -> Includes<'a> {
        self.outside = self.paths.iter().map(|p| outside(p, folders)).collect();
        self
    }

    /// The files `operand`, written in an `#include` of `includer`, may
    /// mean. Pushed onto `out`; false if there is none.
    pub(crate) fn resolve(&self, includer: &str, operand: &str, out: &mut Vec<u32>) -> bool {
        let dir = dir_of(includer);
        if let Some(&id) = self.by_path.get(join(dir, operand).as_str()) {
            out.push(id);
            return true;
        }
        let tail = join("", operand);
        let base = tail.rsplit('/').next().unwrap_or(&tail);
        let paths = &self.paths;
        let matches = |id: &&u32| {
            let p = paths[**id as usize];
            p == tail || p.ends_with(&tail) && p.as_bytes()[p.len() - tail.len() - 1] == b'/'
        };
        let cands: Vec<u32> = self.by_base.get(base).into_iter().flatten().filter(matches).copied().collect();
        // The folder the operand is looked up from (`include/` for
        // `include/linux/kernel.h`) is a `-I` of someone: of the
        // files in the folder above it. One whose folder above does
        // not hold the includer (`tools/virtio/` for a driver) is
        // another program's; of the rest the nearest wins, then the
        // one sharing the most of the includer's folder.
        let root = |id: u32| {
            let p = paths[id as usize];
            p[..p.len() - tail.len()].trim_end_matches('/')
        };
        let above = |id: u32| {
            let up = dir_of(root(id));
            let holds = up.is_empty() || dir == up || dir.starts_with(up) && dir.as_bytes().get(up.len()) == Some(&b'/');
            holds.then(|| up.split('/').filter(|s| !s.is_empty()).count())
        };
        // Where none is above the includer (`asm/barrier.h` from
        // `include/linux/`: each architecture's, and a test
        // program's), the folders made to be looked up from, by
        // their name (`arch/x86/include/`), before the others
        // (`tools/virtio/`).
        let include_folder = |id: u32| root(id).rsplit('/').next().is_some_and(|last| last == "include");
        // And of those the least deep: `arch/x86/include/` before
        // `tools/arch/x86/include/`, a copy kept for another program.
        let shallow = |id: u32| if above(id).is_none() { -(root(id).split('/').filter(|s| !s.is_empty()).count() as i64) } else { 0 };
        // A folder the build does not compile is another architecture's,
        // say: of those not above the includer, one the build compiles.
        let built = |id: u32| !self.outside[id as usize];
        let shared = |id: u32| {
            let other = dir_of(paths[id as usize]);
            (above(id), built(id), include_folder(id), shallow(id), dir.split('/').zip(other.split('/')).take_while(|(a, b)| a == b).count())
        };
        match cands.iter().map(|&c| shared(c)).max() {
            Some(best) => {
                out.extend(cands.iter().copied().filter(|&c| shared(c) == best));
                true
            }
            None => false,
        }
    }
}

/// Returns the edges twice: all of them, and those not under an `#if`.
fn include_edges(files: &[FileFacts], aliases: &[(String, String)], not_built: &[String], stats: &mut LinkStats) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
    let found = Includes::new(files.iter().map(|f| f.path.as_str())).not_built(not_built);
    let edges: Vec<(Vec<u32>, u64, Vec<u32>)> = files
        .par_iter()
        .map(|f| {
            let (mut out, mut sure, mut missing) = (Vec::new(), Vec::new(), 0);
            for (k, &inc) in f.includes.iter().enumerate() {
                let before = out.len();
                let always = !f.include_conditional.get(k).copied().unwrap_or(false);
                let written = f.name(inc as usize);
                let renamed = aliases.iter().find_map(|(from, to)| {
                    let dir = written.strip_suffix(from.as_str())?;
                    (dir.is_empty() || dir.ends_with('/')).then(|| format!("{dir}{to}"))
                });
                let operand = renamed.as_deref().unwrap_or(written);
                if !found.resolve(&f.path, operand, &mut out) {
                    missing += 1;
                }
                if always {
                    sure.extend_from_slice(&out[before..]);
                }
            }
            out.sort_unstable();
            out.dedup();
            sure.sort_unstable();
            sure.dedup();
            (out, missing, sure)
        })
        .collect();
    stats.includes_found = edges.iter().map(|e| e.0.len() as u64).sum();
    stats.includes_missing = edges.iter().map(|e| e.1).sum();
    edges.into_iter().map(|e| (e.0, e.2)).unzip()
}

/// Per-thread scratch for one file's include closure: membership in O(1),
/// reset in O(closure).
struct Closure {
    member: Vec<bool>,
    list: Vec<u32>,
    /// Whether the file is compiled on its own (not a header, nor a source
    /// included in another, which see what the includer defines first).
    unit: bool,
}

impl Closure {
    fn new(n: usize) -> Self {
        Closure { member: vec![false; n], list: Vec::new(), unit: false }
    }

    /// Everything `file` includes, directly or not (itself excluded).
    fn fill(&mut self, file: u32, edges: &[Vec<u32>]) {
        for &f in &self.list {
            self.member[f as usize] = false;
        }
        self.list.clear();
        self.member[file as usize] = true;
        let mut stack = vec![file];
        while let Some(f) = stack.pop() {
            for &to in &edges[f as usize] {
                if !std::mem::replace(&mut self.member[to as usize], true) {
                    self.list.push(to);
                    stack.push(to);
                }
            }
        }
        self.member[file as usize] = false;
    }
}

/// The symbols a name can mean from one file, among `cands` that `want`
/// accepts: those in the file itself (`own`); else those in files it
/// includes; else anything with external visibility. A prototype or `extern`
/// alone stands for the definition elsewhere.
fn in_sight(
    cands: &[u32],
    syms: &[Sym],
    own: std::ops::Range<u32>,
    closure: &Closure,
    // What the file reads in every configuration (empty: not known).
    sure: &[bool],
    want: impl Fn(&Sym) -> bool,
    out: &mut Vec<u32>,
) {
    out.clear();
    // A definition in the same file shadows everything else (covers `static`).
    out.extend(cands.iter().filter(|&&c| own.contains(&c) && want(&syms[c as usize])));
    // (From a source file, a macro of another source file is seen only
    // where that one is included. A header sees what the files including
    // it define first.)
    let external = |c: &&u32| {
        let s = &syms[**c as usize];
        let unseen = closure.unit && s.kind == kind::MACRO && s.flags & flag::HEADER == 0 && !closure.member[s.file as usize];
        want(s) && (s.flags & flag::STATIC == 0 || s.flags & flag::HEADER != 0) && !unseen
    };
    // Unless all it is is a prototype and a stub that may not be compiled
    // (`#ifdef CONFIG_SMP` the prototype, `#else` the stub, both here): the
    // definitions elsewhere are the other reading.
    {
        let s = |c: &u32| &syms[*c as usize];
        let decl = |c: &u32| s(c).flags & flag::DECL != 0;
        let maybe = |c: &u32| matches!(s(c).kind, kind::FUNCTION | kind::MACRO) && s(c).flags & (flag::CONDITIONAL | flag::DECL) == flag::CONDITIONAL;
        if out.iter().any(decl) && out.iter().any(maybe) && out.iter().all(|c| decl(c) || maybe(c)) {
            let n = out.len();
            for &c in cands.iter().filter(external) {
                if !decl(&c) && !out[..n].contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    if out.is_empty() {
        // Included text is part of this translation unit, `static` or not.
        out.extend(cands.iter().filter(|&&c| want(&syms[c as usize]) && closure.member[syms[c as usize].file as usize]));
        // All it includes may not be compiled (`#ifdef CONFIG_PCI` extern
        // ... `#else` a stub): what is defined elsewhere for certain is the
        // other reading.
        // (A prototype beside the stub, in the other branch, is no
        // definition either way.)
        // (So is a macro defined only in a branch: `#ifdef
        // CONFIG_DEBUG_LOCK_ALLOC` `#define mutex_lock(l) ...` `#else` the
        // prototype.)
        // (Or in a header read only in some: `rcutiny.h` or `rcutree.h`.)
        let maybe = |c: &u32| {
            let s = &syms[*c as usize];
            let read_always = sure.is_empty() || sure[s.file as usize] || own.contains(c);
            matches!(s.kind, kind::FUNCTION | kind::MACRO) && s.flags & flag::DECL == 0 && (s.flags & flag::CONDITIONAL != 0 || !read_always)
        };
        let decl = |c: &u32| syms[*c as usize].flags & flag::DECL != 0;
        if out.iter().any(maybe) && out.iter().all(|c| maybe(c) || decl(c)) {
            let n = out.len();
            // (Under the same condition, often: `drivers/base/core.c` has
            // `_dev_info` in `#ifdef CONFIG_PRINTK` too.)
            for &c in cands.iter().filter(external) {
                let s = &syms[c as usize];
                if s.flags & flag::DECL == 0 && !out[..n].contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    if !out.is_empty() && out.iter().all(|&c| syms[c as usize].flags & flag::DECL != 0) {
        let n = out.len();
        // A `static` one in a header the file does not include is no one's
        // definition of this prototype, unless there is no other.
        let definition = |c: &&u32| syms[**c as usize].flags & flag::DECL == 0;
        out.extend(cands.iter().filter(external).filter(definition).filter(|&&c| syms[c as usize].flags & flag::STATIC == 0));
        if out.len() == n {
            out.extend(cands.iter().filter(external).filter(definition));
        }
        if out.len() > n {
            out.drain(..n);
        }
    }
    if out.is_empty() {
        out.extend(cands.iter().filter(external));
    }
}

/// The struct called `name` (a tag or a typedef) as seen from `file`: which
/// file lays it out, and which of that file's records it is.
fn record_of(
    name: u32,
    file: u32,
    closure: &Closure,
    records: &FxHashMap<u32, Vec<(u32, u32)>>,
    aliases: &FxHashMap<u32, Vec<u32>>,
) -> Option<(u32, u32)> {
    let direct = records.get(&name).into_iter().flatten();
    let through = aliases.get(&name).into_iter().flatten().filter_map(|tag| records.get(tag)).flatten();
    let mut all: Vec<(u32, u32)> = direct.chain(through).copied().collect();
    all.sort_unstable();
    all.dedup();
    all.iter()
        .find(|r| r.0 == file)
        .or_else(|| all.iter().find(|r| closure.member[r.0 as usize]))
        .or(if all.len() == 1 { all.first() } else { None })
        .copied()
}

/// C++: `c` is a method of a class that its header has only in one reading,
/// an alias in the other (abseil's `string_view`: the class, or
/// `std::string_view`): a call of it is one reading of the call.
fn aliased_class(g: &Graph, cpp: &Cpp, c: u32) -> bool {
    let s = &g.syms[c as usize];
    let class = cpp.class(c);
    s.kind == kind::FUNCTION
        && s.flags & flag::CONDITIONAL != 0
        && class != NONE
        && g.syms_named(class).iter().any(|&t| g.syms[t as usize].kind == kind::TYPEDEF && g.syms[t as usize].file == s.file)
}

/// C++: `c` is a method of a class that only source files other than `fi`
/// and those it includes write (declared in no header): from `fi` it is
/// out of sight.
fn unseen_class(g: &Graph, cpp: &Cpp, c: u32, fi: u32, closure: &Closure) -> bool {
    let s = &g.syms[c as usize];
    let class = cpp.class(c);
    let seen = |file: u32| file == fi || closure.member[file as usize];
    let is_class = |t: &u32| matches!(g.syms[*t as usize].kind, kind::CLASS | kind::STRUCT | kind::UNION);
    let classes = || g.syms_named(class).iter().filter(|t| is_class(t));
    class != NONE
        && s.flags & flag::HEADER == 0
        && !seen(s.file)
        && classes().next().is_some()
        && classes().all(|&t| g.syms[t as usize].flags & flag::HEADER == 0 && !seen(g.syms[t as usize].file))
}

/// The members of the struct called `name` as seen from `file`.
fn layout<'a>(
    name: u32,
    file: u32,
    closure: &Closure,
    records: &FxHashMap<u32, Vec<(u32, u32)>>,
    aliases: &FxHashMap<u32, Vec<u32>>,
    files: &[&'a FileFacts],
) -> Option<(u32, &'a [u32])> {
    let (rf, ri) = record_of(name, file, closure, records, aliases)?;
    Some((rf, &files[rf as usize].records[ri as usize].fields))
}

/// What the link knows about types: enough to say which struct a member
/// access is on.
struct Types<'a> {
    files: &'a [&'a FileFacts],
    remap: &'a [Vec<u32>],
    records: &'a FxHashMap<u32, Vec<(u32, u32)>>,
    aliases: &'a FxHashMap<u32, Vec<u32>>,
    /// Type name of a variable, or of what a function returns, by symbol.
    of_symbol: &'a FxHashMap<u32, u32>,
    /// C++: what a class derives from.
    cpp: &'a Cpp,
}

impl Types<'_> {
    /// The symbol of member `member` (a global name) of the struct that
    /// `hint`, written in file `fi`, leads to.
    #[allow(clippy::too_many_arguments)]
    fn member(
        &self,
        hint: &MemberHint,
        member: u32,
        fi: usize,
        g: &Graph,
        own: std::ops::Range<u32>,
        closure: &Closure,
        scratch: &mut Vec<u32>,
    ) -> Option<u32> {
        let t = self.type_of(hint, fi, g, own.clone(), closure, scratch);
        // `SI_HINT_WHY=<member>`: what each call of it is found to be made on.
        static WHY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if WHY.get_or_init(|| std::env::var("SI_HINT_WHY").ok()).as_deref().is_some_and(|w| g.name(member) == w) {
            let path: Vec<&str> = self.files[fi].hint_path[hint.path as usize..(hint.path + hint.len) as usize].iter().map(|&p| g.name(self.remap[fi][p as usize])).collect();
            eprintln!("hintwhy   {}: root {} {} path {:?} -> type {:?}, layout {:?}", self.files[fi].path, hint.root_kind, g.name(self.remap[fi][hint.root as usize]), path, t.map(|t| g.name(t)), t.and_then(|t| self.layout_of(t, fi, closure)).map(|(rf, _)| &self.files[rf as usize].path));
            if let Some((rf, ri)) = t.and_then(|t| self.layout_of(t, fi, closure)) {
                let r = &self.files[rf as usize].records[ri as usize];
                eprintln!("hintwhy     fields {:?}", r.fields.iter().map(|&x| if x == NONE { "-" } else { g.name(self.remap[rf as usize][x as usize]) }).collect::<Vec<_>>());
            }
        }
        let t = t?;
        let (rf, ri) = self.layout_of(t, fi, closure)?;
        let (rf, ri, at) = self.find(rf, ri, member, closure, 8)?;
        let def = self.files[rf as usize].records[ri as usize].defs[at];
        (def != NONE).then(|| g.files[rf as usize].sym_start + def)
    }

    /// The type (its name) of what `hint`, written in file `fi`, says a
    /// member access is made on.
    #[allow(clippy::too_many_arguments)]
    fn type_of(
        &self,
        hint: &MemberHint,
        fi: usize,
        g: &Graph,
        own: std::ops::Range<u32>,
        closure: &Closure,
        scratch: &mut Vec<u32>,
    ) -> Option<u32> {
        let f = self.files[fi];
        let path = &f.hint_path[hint.path as usize..(hint.path + hint.len) as usize];
        let name = self.remap[fi][hint.root as usize];
        let found = match hint.root_kind {
            root::TYPE => self.follow(name, path, fi, g, closure),
            kind_of_root => {
                // C++: `T ()`, a class made, is of that class; unless a
                // function of no class has the name, which hides the class
                // (bitcoin's `Params ()`, beside `Consensus::Params`).
                if kind_of_root == root::FUNC && self.cpp.types.contains(&name) {
                    in_sight(g.syms_named(name), &g.syms, own.clone(), closure, &[], |s| s.kind == kind::FUNCTION, scratch);
                    if !scratch.iter().any(|s| self.cpp.class(*s) == NONE && self.of_symbol.contains_key(s)) {
                        return self.follow(name, path, fi, g, closure);
                    }
                }
                // The variable or function the name means here, then its type.
                let wanted = if kind_of_root == root::VAR { kind::GLOBAL } else { kind::FUNCTION };
                in_sight(g.syms_named(name), &g.syms, own.clone(), closure, &[], |s| s.kind == wanted, scratch);
                let type_name = *scratch.iter().find_map(|s| self.of_symbol.get(s))?;
                self.follow(type_name, path, fi, g, closure)
            }
        };
        if found.is_some() || hint.root_kind != root::TYPE || path.is_empty() || !self.cpp.on() {
            return found;
        }
        // C++: in a method, a name not local was taken for a member of its
        // class (`m_chain.Tip ()`); if the class has none of that name, it is
        // a variable of the namespace or the file (`gArgs.GetDataDirNet ()`).
        let first = self.remap[fi][path[0] as usize];
        in_sight(g.syms_named(first), &g.syms, own, closure, &[], |s| s.kind == kind::GLOBAL, scratch);
        let type_name = *scratch.iter().find_map(|s| self.of_symbol.get(s))?;
        self.follow(type_name, &path[1..], fi, g, closure)
    }

    /// C++: the member `member` (a method, an operator) of the class
    /// `type_name` or of a class it derives from.
    fn class_member(&self, type_name: u32, member: u32, fi: usize, g: &Graph, closure: &Closure) -> Option<u32> {
        let (rf, ri) = self.layout_of(type_name, fi, closure)?;
        let (rf, ri, at) = self.find(rf, ri, member, closure, 8)?;
        let def = self.files[rf as usize].records[ri as usize].defs[at];
        (def != NONE).then(|| g.files[rf as usize].sym_start + def)
    }

    /// The layout of the struct or class `type_name` as seen from `fi`
    /// (C++: of a class with none of its own, a test case of a fixture, its
    /// base's).
    fn layout_of(&self, type_name: u32, fi: usize, closure: &Closure) -> Option<(u32, u32)> {
        record_of(type_name, fi as u32, closure, self.records, self.aliases).or_else(|| {
            self.cpp.bases.get(&type_name)?.iter().find_map(|&b| record_of(b, fi as u32, closure, self.records, self.aliases))
        })
    }

    /// The type (its name) that the members `path` lead to from `type_name`.
    fn follow(&self, type_name: u32, path: &[u32], fi: usize, g: &Graph, closure: &Closure) -> Option<u32> {
        // (C++: `[]` after a member marks an element of it, a container.)
        let element = g.name_id("[]");
        let mut t = type_name;
        let mut i = 0;
        // (C++: `#i` marks the `i`th of a structured binding.)
        let position = |m: u32| g.name(m).strip_prefix('#').and_then(|n| n.parse::<usize>().ok());
        while i < path.len() {
            let m = self.remap[fi][path[i] as usize];
            if Some(m) == element {
                // Of a class with an `operator[]`, what that returns; of
                // what is no member (`p[i]` of a pointer), of its type.
                // (One returning a `char` or a library type: not known. What
                // it returns is said where it is defined, too.)
                if let Some((name, op)) = g.name_id("operator[]").and_then(|name| Some((name, self.class_member(t, name, fi, g, closure)?))) {
                    let class = self.cpp.class(op);
                    let returns = |s: &u32| (*s == op || self.cpp.class(*s) == class).then(|| self.of_symbol.get(s)).flatten();
                    t = *g.syms_named(name).iter().find_map(returns)?;
                }
                i += 1;
                continue;
            }
            if let Some(n) = position(m) {
                // `auto [a, b] = x`: the `n`th data member of x.
                let (rf, ri) = self.layout_of(t, fi, closure)?;
                let r = &self.files[rf as usize].records[ri as usize];
                let defs = &self.files[rf as usize].defs;
                let at = (0..r.fields.len()).filter(|&a| r.defs[a] != NONE && defs[r.defs[a] as usize].kind == kind::FIELD).nth(n)?;
                let next = r.types[at];
                if next == NONE {
                    return None;
                }
                t = self.remap[rf as usize][next as usize];
                i += 1;
                continue;
            }
            let (rf, ri) = self.layout_of(t, fi, closure)?;
            let (mf, mi, at) = self.find(rf, ri, m, closure, 8)?;
            let r = &self.files[mf as usize].records[mi as usize];
            let of_element = path.get(i + 1).is_some_and(|&n| Some(self.remap[fi][n as usize]) == element);
            // `for (auto& [k, v] : m_map)`: of a map's element, the key or the value.
            let key = || self.files[mf as usize].cxx().keys.iter().find(|e| e.0 == mi && e.1 == at as u32).map(|e| e.2);
            if let Some(n) = of_element.then(|| path.get(i + 2).and_then(|&p| position(self.remap[fi][p as usize]))).flatten() {
                if let Some(k) = key() {
                    let element_type = self.files[mf as usize].cxx().elems.iter().find(|e| e.0 == mi && e.1 == at as u32).map(|e| e.2);
                    let next = if n == 0 { Some(k) } else { element_type };
                    t = self.remap[mf as usize][next? as usize];
                    i += 3;
                    continue;
                }
            }
            let element_type = || self.files[mf as usize].cxx().elems.iter().find(|e| e.0 == mi && e.1 == at as u32).map(|e| e.2);
            // (Of `StrictMock<M> m`, a class that derives from its argument:
            // `M`, whose members it has.)
            let wrapped = || {
                let t = r.types[at];
                (t != NONE && self.cpp.wrappers.contains(&self.remap[mf as usize][t as usize]))
                    .then(|| self.files[mf as usize].cxx().targs.iter().find(|e| e.0 == mi && e.1 == at as u32).map(|e| e.2))
                    .flatten()
            };
            let next = match of_element.then(element_type).flatten() {
                Some(e) => e,
                None => wrapped().unwrap_or(r.types[at]),
            };
            if next == NONE {
                return None;
            }
            t = self.remap[mf as usize][next as usize];
            i += 1 + of_element as usize;
        }
        Some(t)
    }

    /// C++: the type of member `member` of the class `type_name` (or of a
    /// class it derives from), by its own name.
    fn member_type(&self, type_name: u32, member: u32, fi: usize, closure: &Closure) -> Option<u32> {
        let (rf, ri) = record_of(type_name, fi as u32, closure, self.records, self.aliases)?;
        let (mf, mi, at) = self.find(rf, ri, member, closure, 8)?;
        let t = self.files[mf as usize].records[mi as usize].types[at];
        (t != NONE).then(|| self.remap[mf as usize][t as usize])
    }

    /// Member `wanted` of a struct or class, or (C++) of a class it
    /// derives from: (file, record, position).
    fn find(&self, rf: u32, ri: u32, wanted: u32, closure: &Closure, depth: u32) -> Option<(u32, u32, usize)> {
        let r = &self.files[rf as usize].records[ri as usize];
        let remap = &self.remap[rf as usize];
        if let Some(at) = (0..r.fields.len()).find(|&i| r.fields[i] != NONE && remap[r.fields[i] as usize] == wanted) {
            return Some((rf, ri, at));
        }
        if depth == 0 {
            return None;
        }
        let bases = self.cpp.bases.get(&remap[r.name as usize])?;
        bases.iter().find_map(|&b| {
            let (bf, bi) = record_of(b, rf, closure, self.records, self.aliases)?;
            self.find(bf, bi, wanted, closure, depth - 1)
        })
    }
}

/// What the link knows of C++ classes. Empty where no file is C++.
#[derive(Default)]
struct Cpp {
    /// Is any file C++?
    on: bool,
    /// Symbol -> the class it is a member of, by the class's own name
    /// (none: of none, or of a namespace).
    class_of: FxHashMap<u32, u32>,
    /// Class -> the classes it derives from, and the reverse.
    bases: FxHashMap<u32, Vec<u32>>,
    derived: FxHashMap<u32, Vec<u32>>,
    /// (class, name) of the methods declared virtual.
    virtuals: FxHashSet<(u32, u32)>,
    /// Names that are namespaces and no class.
    namespaces: FxHashSet<u32>,
    /// Symbol -> the innermost namespace it is in (none: the global one).
    ns_of: FxHashMap<u32, u32>,
    /// Names of classes (of any kind of type the corpus defines).
    types: FxHashSet<u32>,
    /// Another name for a namespace -> its own (`tfm` -> `tinyformat`).
    namespace_aliases: FxHashMap<u32, u32>,
    /// An operator of no class -> the types of its parameters.
    operator_params: FxHashMap<u32, (u32, u32)>,
    /// A function -> how many arguments it takes, fewest and most.
    arity: FxHashMap<u32, (u8, u8)>,
    /// (Function, position) -> the class of the parameter there.
    params: FxHashMap<(u32, u32), u32>,
    /// The constructors that convert nothing.
    explicit: FxHashSet<u32>,
    /// A macro -> what it passes its arguments on to: (the function or
    /// macro called, its qualifier, the parameter, the argument, `...`).
    forwards: FxHashMap<u32, Vec<(u32, u32, u32, u32, bool)>>,
    /// The class templates that derive from a parameter of theirs
    /// (`Cxx::param_bases`), by name.
    wrappers: FxHashSet<u32>,
}

impl Cpp {
    fn new(files: &[&FileFacts], remap: &[Vec<u32>], g: &mut Graph) -> Cpp {
        if !files.iter().any(|f| f.cpp()) {
            return Cpp::default();
        }
        let mut c = Cpp { on: true, ..Default::default() };
        // (A definition a macro makes is given its class after where it is:
        // the later says.)
        let mut owner: FxHashMap<u32, u32> = FxHashMap::default();
        c.types.extend(g.syms.iter().filter(|s| matches!(s.kind, kind::CLASS | kind::STRUCT | kind::UNION | kind::TYPEDEF | kind::ENUM)).map(|s| s.name));
        c.namespaces.extend(g.syms.iter().filter(|s| s.kind == kind::NAMESPACE).map(|s| s.name));
        for (fi, f) in files.iter().enumerate() {
            c.namespaces.extend(f.cxx().namespaces.iter().map(|&n| remap[fi][n as usize]));
            c.namespace_aliases.extend(f.cxx().namespace_aliases.iter().map(|&(a, t)| (remap[fi][a as usize], remap[fi][t as usize])));
        }
        // An alias is what a name means only where no namespace of that
        // name is opened (bitcoin's `fs`, and one test's `namespace fs =
        // std::filesystem;`).
        let mut opened: FxHashSet<u32> = g.syms.iter().filter(|s| s.kind == kind::NAMESPACE).map(|s| s.name).collect();
        for (fi, f) in files.iter().enumerate() {
            let aliases: FxHashSet<u32> = f.cxx().namespace_aliases.iter().map(|a| a.0).collect();
            opened.extend(f.cxx().namespaces.iter().filter(|n| !aliases.contains(n)).map(|&n| remap[fi][n as usize]));
        }
        c.namespace_aliases.retain(|a, _| !opened.contains(a));
        c.namespaces.retain(|name| !c.types.contains(name));
        for (fi, f) in files.iter().enumerate() {
            let base = g.files[fi].sym_start as usize;
            for &(def, chain, own) in &f.cxx().owners {
                let own = remap[fi][own as usize];
                let chain = remap[fi][chain as usize];
                let sym = (base + def as usize) as u32;
                owner.insert(sym, chain);
                if !c.namespaces.contains(&own) {
                    c.class_of.insert(sym, own);
                }
                // The namespaces lead the scope (`wallet::CWallet`): the last of them.
                let mut ns = NONE;
                for part in g.name(chain).split("::") {
                    match g.name_id(part).filter(|id| c.namespaces.contains(id)) {
                        Some(id) => ns = id,
                        None => break,
                    }
                }
                if ns != NONE {
                    c.ns_of.insert(sym, ns);
                }
            }
            for &(def, p0, p1) in &f.cxx().operator_params {
                let t = |p: u32| if p == NONE { NONE } else { remap[fi][p as usize] };
                c.operator_params.insert((base + def as usize) as u32, (t(p0), t(p1)));
            }
            c.arity.extend(f.cxx().arity.iter().map(|&(def, least, most)| ((base + def as usize) as u32, (least, most))));
            c.params.extend(f.cxx().params.iter().map(|&(def, at, t)| (((base + def as usize) as u32, at), remap[fi][t as usize])));
            c.explicit.extend(f.cxx().explicit.iter().map(|&def| (base + def as usize) as u32));
            for w in &f.cxx().forwards {
                let qual = if w.qual == NONE { NONE } else { remap[fi][w.qual as usize] };
                c.forwards.entry((base + w.def as usize) as u32).or_default().push((remap[fi][w.callee as usize], qual, w.param, w.arg, w.rest));
            }
            c.wrappers.extend(f.cxx().param_bases.iter().map(|&n| remap[fi][n as usize]));
            for &(class, b) in &f.cxx().bases {
                let (class, b) = (remap[fi][class as usize], remap[fi][b as usize]);
                if class != b {
                    c.bases.entry(class).or_default().push(b);
                    c.derived.entry(b).or_default().push(class);
                }
            }
        }
        for v in c.bases.values_mut().chain(c.derived.values_mut()) {
            v.sort_unstable();
            v.dedup();
        }
        g.owner = owner.into_iter().map(|(sym, chain)| [sym, chain]).collect();
        g.owner.sort_unstable_by_key(|o| o[0]);
        for (&sym, &class) in &c.class_of {
            let s = &g.syms[sym as usize];
            if s.flags & flag::VIRTUAL != 0 {
                c.virtuals.insert((class, s.name));
            }
        }
        // Default arguments are written where a function is declared, not
        // where it is defined apart: a definition takes as few as a
        // declaration of the name in its class with as many parameters.
        let mut fewest: FxHashMap<(u32, u32, u8), u8> = FxHashMap::default();
        for (&sym, &(least, most)) in &c.arity {
            let s = &g.syms[sym as usize];
            if s.flags & flag::DECL != 0 && least < most {
                let e = fewest.entry((c.class(sym), s.name, most)).or_insert(least);
                *e = (*e).min(least);
            }
        }
        if !fewest.is_empty() {
            let class_of = &c.class_of;
            for (sym, a) in c.arity.iter_mut() {
                let key = (class_of.get(sym).copied().unwrap_or(NONE), g.syms[*sym as usize].name, a.1);
                if let Some(&least) = fewest.get(&key) {
                    a.0 = a.0.min(least);
                }
            }
        }
        // So is `explicit`: a constructor defined apart is so where one of
        // its class declared with as many parameters, the first of the same
        // class, is.
        let key = |c: &Cpp, sym: u32| (c.class(sym), g.syms[sym as usize].name, c.arity.get(&sym).map_or(u8::MAX, |a| a.1), c.params.get(&(sym, 0)).copied().unwrap_or(NONE));
        let declared: FxHashSet<_> = c.explicit.iter().filter(|&&s| g.syms[s as usize].flags & flag::DECL != 0).map(|&s| key(&c, s)).collect();
        if !declared.is_empty() {
            let defs: Vec<u32> = c.arity.keys().copied().filter(|&s| g.syms[s as usize].flags & flag::DECL == 0 && declared.contains(&key(&c, s))).collect();
            c.explicit.extend(defs);
        }
        c
    }

    /// The constructors that make a call's arguments into the classes of
    /// the parameters they are passed as (`Put ("k")` of a `Put (const
    /// Slice& key)`: Slice's from a string), as (constructor, class).
    /// `given`: (position, what is known of the argument's type: a class,
    /// or `literals`, a string's, a number's or a `std::string`'s);
    /// `targets`: what the call reaches, functions or the one macro it
    /// uses, which passes them on.
    #[allow(clippy::too_many_arguments)]
    fn conversions(
        &self,
        g: &Graph,
        aliases: &FxHashMap<u32, Vec<u32>>,
        given: &[(u32, u32)],
        targets: &[u32],
        literals: [u32; 3],
        lineage: &mut Vec<u32>,
        out: &mut Vec<(u32, u32)>,
    ) {
        let syms = &g.syms;
        let is = |c: u32, k: u8| syms[c as usize].kind == k;
        // (what may be called, (position in the call, position there))
        let mut work: Vec<(Vec<u32>, Vec<(u32, u32)>)> = Vec::new();
        let start: Vec<(u32, u32)> = given.iter().map(|&(j, _)| (j, j)).collect();
        if !targets.is_empty() && targets.iter().all(|&t| is(t, kind::FUNCTION)) {
            work.push((targets.to_vec(), start));
        } else if let [m] = *targets {
            // Through the macros that pass the arguments on.
            let mut macros = vec![(m, start, 0)];
            while let Some((m, map, depth)) = macros.pop() {
                let Some(rows) = self.forwards.get(&m).filter(|_| is(m, kind::MACRO)) else { continue };
                let mut callees: Vec<(u32, u32)> = rows.iter().map(|r| (r.0, r.1)).collect();
                callees.sort_unstable();
                callees.dedup();
                for (callee, qual) in callees {
                    let mapped: Vec<(u32, u32)> = map
                        .iter()
                        .filter_map(|&(j, p)| {
                            rows.iter().filter(|r| r.0 == callee && r.1 == qual).find_map(|r| if r.4 { (p >= r.2).then(|| r.3 + (p - r.2)) } else { (p == r.2).then_some(r.3) }).map(|k| (j, k))
                        })
                        .collect();
                    if mapped.is_empty() {
                        continue;
                    }
                    let named = g.syms_named(callee);
                    let mut fns: Vec<u32> = named.iter().copied().filter(|&c| is(c, kind::FUNCTION)).collect();
                    if fns.is_empty() {
                        if let [next] = *named.iter().copied().filter(|&c| is(c, kind::MACRO)).collect::<Vec<_>>() {
                            if depth < 4 {
                                macros.push((next, mapped, depth + 1));
                            }
                        }
                        continue;
                    }
                    // `tfm::format`: of that namespace, or class; a name
                    // not qualified, a function of no class.
                    let qual = self.namespace_aliases.get(&qual).copied().unwrap_or(qual);
                    let keep = |c: &u32| {
                        if qual == NONE {
                            self.class(*c) == NONE
                        } else if self.namespaces.contains(&qual) {
                            self.ns(*c) == qual
                        } else {
                            self.class(*c) == qual
                        }
                    };
                    if fns.iter().any(keep) {
                        fns.retain(keep);
                    }
                    work.push((fns, mapped));
                }
            }
        }
        let literal = |t: u32| literals.contains(&t);
        let mut ctors: Vec<(u32, u32)> = Vec::new();
        for (fns, map) in work {
            for (j, k) in map {
                let Some(&(_, a)) = given.iter().find(|x| x.0 == j) else { continue };
                // A candidate taking it as it is is the one called.
                if fns.iter().any(|&c| self.params.get(&(c, k)) == Some(&a)) {
                    continue;
                }
                lineage.clear();
                if !literal(a) {
                    self.lineage(a, lineage);
                }
                // The classes the candidates take there, and of each the
                // constructors that would make one of the argument: taking
                // one argument, not `explicit`; of a string or a number one
                // whose parameter is no class of the corpus, of an object
                // one taking its class (or a base).
                let ctor_of = |c: u32, class: u32| is(c, kind::FUNCTION) && self.class(c) == class;
                let has_ctor = |class: u32| g.syms_named(class).iter().any(|&c| ctor_of(c, class));
                let from = |c: u32| match self.params.get(&(c, 0)) {
                    None => true,
                    Some(&p) if literal(p) => p == a,
                    Some(p) => !literal(a) && lineage.contains(p),
                };
                ctors.clear();
                let mut classes: Vec<u32> = fns.iter().filter_map(|&c| self.params.get(&(c, k)).copied()).filter(|&t| !literal(t)).collect();
                classes.sort_unstable();
                classes.dedup();
                for t in classes {
                    // (An object of a class derived from it is passed as is.)
                    if lineage.contains(&t) {
                        ctors.clear();
                        break;
                    }
                    let mut class = t;
                    if !has_ctor(class) {
                        match aliases.get(&class).into_iter().flatten().copied().find(|&x| has_ctor(x)) {
                            Some(tag) => class = tag,
                            None => continue,
                        }
                    }
                    for &c in g.syms_named(class) {
                        if ctor_of(c, class) && !self.explicit.contains(&c) && self.arity.get(&c).is_none_or(|&(least, most)| least <= 1 && 1 <= most) && from(c) {
                            ctors.push((c, class));
                        }
                    }
                }
                // Only where one class can take it.
                if ctors.iter().any(|x| x.1 != ctors[0].1) {
                    continue;
                }
                let is_def = |c: &(u32, u32)| syms[c.0 as usize].flags & flag::DECL == 0;
                if ctors.iter().any(is_def) {
                    ctors.retain(is_def);
                }
                out.extend_from_slice(&ctors);
            }
        }
    }

    /// Of overloads, keep those that take `n` arguments, if any does and
    /// not all do. (One whose parameters are not known takes any number.)
    /// Not for a declaration alone where definitions do not fit: that may
    /// be one the compiler writes (`T () = default;`), which the reader
    /// would rather see the class's for. Unless `all` (every symbol of the
    /// name) has definitions of it that fit, of its class and namespace,
    /// elsewhere: those (abseil's `StrCat (a, b)`, declared in `str_cat.h`
    /// beside inline overloads of other arities, defined in `str_cat.cc`).
    fn by_arity(&self, n: Option<u8>, cands: &mut Vec<u32>, all: &[u32], syms: &[Sym]) {
        let Some(n) = n else { return };
        let fits = |c: &u32| self.arity.get(c).is_none_or(|&(least, most)| least <= n && n <= most);
        let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
        let defs = cands.iter().any(is_def);
        if cands.iter().all(fits) {
            return;
        }
        if cands.iter().any(|c| fits(c) && (is_def(c) || !defs)) {
            cands.retain(fits);
            return;
        }
        let Some(&decl) = cands.iter().find(|c| fits(c)) else { return };
        let (class, ns) = (self.class(decl), self.ns(decl));
        let elsewhere = |c: &&u32| {
            let s = &syms[**c as usize];
            s.kind == kind::FUNCTION
                && is_def(c)
                && s.flags & flag::STATIC == 0
                && self.arity.contains_key(c)
                && fits(c)
                && self.class(**c) == class
                && self.ns(**c) == ns
                && !cands.contains(c)
        };
        let found: Vec<u32> = all.iter().filter(elsewhere).copied().collect();
        if !found.is_empty() {
            cands.retain(fits);
            cands.extend(found);
        }
    }

    fn on(&self) -> bool {
        self.on
    }

    fn class(&self, sym: u32) -> u32 {
        self.class_of.get(&sym).copied().unwrap_or(NONE)
    }

    fn ns(&self, sym: u32) -> u32 {
        self.ns_of.get(&sym).copied().unwrap_or(NONE)
    }

    /// The class and those it derives from, nearest first.
    fn lineage(&self, class: u32, out: &mut Vec<u32>) {
        out.clear();
        out.push(class);
        let mut i = 0;
        while i < out.len() && out.len() < 64 {
            for &b in self.bases.get(&out[i]).into_iter().flatten() {
                if !out.contains(&b) {
                    out.push(b);
                }
            }
            i += 1;
        }
    }

    /// The classes that derive from `class`, at any depth.
    fn descendants(&self, class: u32, out: &mut Vec<u32>) {
        out.clear();
        let mut i = 0;
        let mut todo = vec![class];
        while i < todo.len() && todo.len() < 512 {
            for &d in self.derived.get(&todo[i]).into_iter().flatten() {
                if !todo.contains(&d) {
                    todo.push(d);
                    out.push(d);
                }
            }
            i += 1;
        }
    }

    /// Is method `name` of `class` virtual, declared so there or in a base?
    fn is_virtual(&self, class: u32, name: u32, scratch: &mut Vec<u32>) -> bool {
        self.lineage(class, scratch);
        scratch.iter().any(|&c| self.virtuals.contains(&(c, name)))
    }

    /// Of `cands`, those of the class nearest in `class`'s lineage that has
    /// any (`class` `NONE`: those of no class).
    fn nearest(&self, cands: &[u32], class: u32, lineage: &mut Vec<u32>, out: &mut Vec<u32>) {
        out.clear();
        if class == NONE {
            out.extend(cands.iter().filter(|&&c| self.class(c) == NONE));
            return;
        }
        self.lineage(class, lineage);
        for &cls in lineage.iter() {
            out.extend(cands.iter().filter(|&&c| self.class(c) == cls));
            if !out.is_empty() {
                return;
            }
        }
    }
}

pub fn link(files: &[FileFacts]) -> (Graph, LinkStats) {
    link_with(files, &Options::default())
}

pub fn link_with(files: &[FileFacts], options: &Options) -> (Graph, LinkStats) {
    link_cached(files, options, &mut crate::expand::Cache::default())
}

/// Link, reusing the macro expansions `cache` holds from the last link of
/// these files where they are still good.
pub fn link_cached(files: &[FileFacts], options: &Options, cache: &mut crate::expand::Cache) -> (Graph, LinkStats) {
    let linked = link_all(files, options, cache);
    // Now that everything the link worked with is let go.
    crate::trim_memory();
    crate::mem_mark("linked, trimmed");
    linked
}

fn link_all(files: &[FileFacts], options: &Options, cache: &mut crate::expand::Cache) -> (Graph, LinkStats) {
    crate::progress::linking();
    let mut stats = LinkStats::default();

    // 0. Which file includes which, then macro expansion: it needs the
    //    definitions other files give, and adds definitions and references.
    let t = Instant::now();
    let (includes, sure_includes) = include_edges(files, &options.include_aliases, &options.not_built, &mut stats);
    stats.include_ms = ms(t);
    let t = Instant::now();
    (stats.sites, stats.expanded_defs, stats.expanded_refs) = crate::expand::expand(files, &includes, &sure_includes, &options.not_built, &options.readings, cache);
    stats.expand_ms = ms(t);
    crate::mem_mark("expanded");
    crate::trim_memory();
    crate::mem_mark("expanded, trimmed");
    if std::env::var_os("SI_MEM_PROFILE").is_some() {
        let mb = |n: usize| n >> 20;
        let part = |fs: &mut dyn Iterator<Item = &FileFacts>| {
            let (mut sums, mut n) = (Vec::new(), 0);
            for f in fs {
                n += 1;
                let parts = f.parts();
                sums.resize(parts.len(), ("", 0usize));
                for (sum, part) in sums.iter_mut().zip(parts) {
                    *sum = (part.0, sum.1 + part.1);
                }
            }
            let all: usize = sums.iter().map(|s| s.1).sum();
            format!("{n} files, {} MB: {}", mb(all), sums.iter().map(|s| format!("{} {}", s.0, mb(s.1))).collect::<Vec<_>>().join(", "))
        };
        // Of the names: how much is text kept for expanding (the arguments
        // of macro uses, initializers, macro bodies), and how much names proper.
        let (mut args, mut inits, mut bodies, mut all, mut count) = (0usize, 0usize, 0usize, 0usize, 0usize);
        for f in files.iter() {
            let len = |i: u32| if i == NONE { 0 } else { f.name(i as usize).len() };
            args += f.sites.iter().map(|s| len(s.args)).sum::<usize>();
            inits += f.inits.iter().map(|i| len(i.text)).sum::<usize>();
            bodies += f.macros.iter().map(|m| len(m.text) + len(m.guard)).sum::<usize>();
            all += (0..f.n_names()).map(|i| f.name(i).len()).sum::<usize>();
            count += f.n_names();
        }
        eprintln!("names     {count} in all, {} MB of text: use arguments {} MB, initializers {} MB, macro bodies {} MB, the rest {} MB", mb(all), mb(args), mb(inits), mb(bodies), mb(all.saturating_sub(args + inits + bodies)));
        eprintln!("facts     {}", part(&mut files.iter()));
        eprintln!("expanded  {}", part(&mut cache.expanded.iter().flatten()));
    }
    let crate::expand::Cache { expanded, names: name_cache, .. } = cache;
    let files: Vec<&FileFacts> = files.iter().zip(expanded.iter()).map(|(f, e)| e.as_ref().unwrap_or(f)).collect();
    let files = &files[..];

    // 1. Every file-local name under its number in the graph, which is
    //    its lexicographic rank among the names in use.
    let t = Instant::now();
    crate::mem_mark("before names");
    let (remap, names) = name_cache.table(files);
    crate::mem_mark("names numbered");
    let mut g = Graph::default();
    g.str_off.reserve_exact(names.len() + 1);
    g.str_off.push(0);
    g.str_bytes.reserve_exact(names.iter().map(|n| n.len()).sum());
    for name in &names {
        g.str_bytes.extend_from_slice(name.as_bytes());
        g.str_off.push(g.str_bytes.len() as u32);
    }
    stats.intern_ms = ms(t);
    crate::mem_mark("names");

    // 2. Symbol table (each file's symbols contiguous) and name -> symbols index.
    let t = Instant::now();
    let n_syms: usize = files.iter().map(|f| f.defs.len()).sum();
    g.syms.reserve(n_syms);
    g.by_name_off = vec![0u32; names.len() + 1];
    for (fi, f) in files.iter().enumerate() {
        let header = if crate::is_header(&f.path) { flag::HEADER } else { 0 };
        g.files.push(FileRec { hash: f.hash, path: remap[fi][f.defs[0].name as usize], sym_start: g.syms.len() as u32 });
        for d in &f.defs {
            let name = remap[fi][d.name as usize];
            g.by_name_off[name as usize + 1] += 1;
            g.syms.push(Sym {
                name,
                file: fi as u32,
                line: d.line,
                end_line: d.end_line,
                kind: d.kind,
                flags: d.flags | header,
                _pad: 0,
            });
        }
    }
    for i in 0..names.len() {
        g.by_name_off[i + 1] += g.by_name_off[i];
    }
    let mut next = g.by_name_off.clone();
    g.by_name = vec![0u32; n_syms];
    for (id, s) in g.syms.iter().enumerate() {
        let slot = &mut next[s.name as usize];
        g.by_name[*slot as usize] = id as u32;
        *slot += 1;
    }
    stats.syms_ms = ms(t);
    crate::mem_mark("symbols");
    let cpp = Cpp::new(files, &remap, &mut g);

    // Which members can hold a function: those declared as function
    // pointers, and those of a typedef type that is one (or that is not
    // defined anywhere here, so cannot be ruled out).
    let callable: Vec<u32> = files
        .iter()
        .enumerate()
        .flat_map(|(fi, f)| {
            let (g, remap) = (&g, &remap);
            f.field_types.iter().filter_map(move |&(def, type_name)| {
                let typedefs = || g.syms_named(remap[fi][type_name as usize]).iter().map(|&t| &g.syms[t as usize]).filter(|t| t.kind == kind::TYPEDEF);
                (typedefs().next().is_none() || typedefs().any(|t| t.flags & flag::CALLABLE != 0)).then_some(g.files[fi].sym_start + def)
            })
        })
        .collect();
    for id in callable {
        g.syms[id as usize].flags |= flag::CALLABLE;
    }

    // Struct layouts by name, and typedef names for struct tags.
    let mut records: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
    let mut aliases: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    let mut renames: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
    // (C++: a class in a class is what its short name means only where no
    // class outside any has that name.)
    let mut nested: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
    for (fi, f) in files.iter().enumerate() {
        for (i, r) in f.records.iter().enumerate() {
            let to = if f.cxx().nested.contains(&(i as u32)) { &mut nested } else { &mut records };
            to.entry(remap[fi][r.name as usize]).or_default().push((fi as u32, i as u32));
        }
        for &(alias, tag) in &f.aliases {
            aliases.entry(remap[fi][alias as usize]).or_default().push(remap[fi][tag as usize]);
        }
        // A macro that only names a struct (`#define ROW_STRUCT command`)
        // is another name for it: `struct ROW_STRUCT table[]`. Which
        // struct depends on who defines the macro, so the file is kept.
        for m in f.macros.iter().filter(|m| !m.function_like) {
            let body = f.name(m.text as usize).trim();
            let ident = !body.is_empty() && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !body.as_bytes()[0].is_ascii_digit();
            if let Some(tag) = g.name_id(body).filter(|_| ident) {
                renames.entry(remap[fi][m.name as usize]).or_default().push((fi as u32, tag));
            }
        }
    }

    for (name, list) in nested {
        records.entry(name).or_insert(list);
    }

    let mut of_symbol: FxHashMap<u32, u32> = FxHashMap::default();
    for (fi, f) in files.iter().enumerate() {
        for &(def, t) in &f.typed {
            of_symbol.insert(g.files[fi].sym_start + def, remap[fi][t as usize]);
        }
    }
    let types = Types { files, remap: &remap, records: &records, aliases: &aliases, of_symbol: &of_symbol, cpp: &cpp };

    // Functions that call through a parameter: symbol -> (position, line).
    let mut through_param: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
    for (fi, f) in files.iter().enumerate() {
        for c in &f.param_calls {
            through_param.entry(g.files[fi].sym_start + c.scope).or_default().push((c.index, c.line));
        }
    }

    // Per file, the compilation it starts and the header it is, as the
    // readings number them.
    let readings = &*options.readings;
    let numbered = |list: Vec<&str>| -> Vec<u32> {
        let at: FxHashMap<&str, u32> = list.into_iter().enumerate().map(|(i, p)| (p, i as u32)).collect();
        files.iter().map(|f| at.get(f.path.as_str()).copied().unwrap_or(NONE)).collect()
    };
    let edge_why = std::env::var("SI_EDGE_WHY").ok();
    // The files no compilation reads (a header only `#if`s the conditions
    // switch off include): what is defined there is no reading of a name
    // others define, from a file that is read.
    // (So are those in a folder the build does not compile.)
    let unread: Vec<bool> = {
        let set: FxHashSet<&str> = readings.unread.iter().map(String::as_str).collect();
        files.iter().map(|f| set.contains(f.path.as_str()) || outside(&f.path, &options.not_built)).collect()
    };
    // Of those, the files of another program (in a folder the build does not
    // compile): what they call is not what the build's files call. (A file of
    // this program the configuration leaves out calls what it would.)
    let apart: Vec<bool> = {
        let set: FxHashSet<&str> = readings.unread.iter().map(String::as_str).collect();
        let folders: Vec<String> = options.not_built.iter().filter(|f| f.ends_with('/')).cloned().collect();
        files.iter().map(|f| set.contains(f.path.as_str()) || outside(&f.path, &folders)).collect()
    };
    // Where none of a header's compilations reads the callee's file, what
    // every compilation that does compiles: per (file, stretch of lines).
    let everywhere: std::sync::Mutex<FxHashMap<(u32, u32), crate::cond::Tri>> = Default::default();
    // The macros whose body uses their own name: wrappers of the function
    // of that name (`#define class_mutex_constructor(_T)
    // WITH_LOCK_GUARD_1_ATTRS(...)`, which calls it).
    // So, likely, are those with parameters, there in every configuration,
    // whose body calls another macro: the name may come back expanded, as
    // it does there. (Not one whose body only does without the function,
    // `#define htab_size(h) ((h)->size)`.)
    let macro_names: FxHashSet<&str> = files.iter().flat_map(|f| f.defs.iter().filter(|d| d.kind == kind::MACRO).map(move |d| f.name(d.name as usize))).collect();
    let macro_names = &macro_names;
    let wrappers: FxHashSet<u32> = files
        .iter()
        .enumerate()
        .flat_map(|(fi, f)| {
            let base = g.files[fi].sym_start;
            f.refs
                .iter()
                .filter(move |r| {
                    f.defs.get(r.scope as usize).is_some_and(|d| {
                        d.kind == kind::MACRO
                            && (d.name == r.name
                                || d.flags & (flag::CONDITIONAL | flag::CALLABLE) == flag::CALLABLE
                                    && r.kind & !refkind::MAYBE == refkind::CALL
                                    && macro_names.contains(f.name(r.name as usize)))
                    })
                })
                .map(move |r| base + r.scope)
        })
        .collect();
    let who = Readers::new(readings);
    let all: Vec<u64> = vec![u64::MAX; readings.units.len().div_ceil(64)];
    let (unit_of, header_of, path_of) = if readings.is_empty() {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        (
            numbered(readings.units.iter().map(|u| u.0.as_str()).collect()),
            numbered(readings.files.iter().map(|f| f.0.as_str()).collect()),
            numbered(readings.paths.iter().map(|p| p.as_str()).collect()),
        )
    };

    // The files some file includes (a `.c` read as part of another).
    let mut included = vec![false; files.len()];
    for to in includes.iter().flatten() {
        included[*to as usize] = true;
    }

    // 3. Resolve references. Each file is independent given the symbol table.
    //    A name means, in order: a definition in this file; one in a file it
    //    includes; anything with external visibility.
    let t = Instant::now();
    let syms = &g.syms;
    // (What the facts call a string and a number passed.)
    let literals = ["\"\"", "0", "std::string"].map(|n| g.name_id(n).unwrap_or(NONE));
    let per_file: Vec<(Vec<[u32; 3]>, [u64; 4], Vec<[u32; 4]>, Vec<[u32; 3]>)> = files
        .par_iter()
        .enumerate()
        .map_init(|| (Closure::new(files.len()), Closure::new(files.len())), |(closure, sure), (fi, f)| {
            closure.fill(fi as u32, &includes);
            closure.unit = crate::is_unit(&f.path) && !included[fi];
            // What the file reads in every configuration.
            sure.fill(fi as u32, &sure_includes);
            let base = g.files[fi].sym_start;
            let end = base + f.defs.len() as u32;
            let mut edges = Vec::with_capacity(f.refs.len());
            let mut counts = [0u64; 4]; // resolved, ambiguous, unresolved, capped
            let mut tmp: Vec<u32> = Vec::new();
            let mut scratch: Vec<u32> = Vec::new();
            let mut hint_of = vec![NONE; f.refs.len()];
            for &(r, h) in &f.ref_hints {
                hint_of[r as usize] = h;
            }
            // C++: what each name was called with (`X::f`).
            let in_cpp = f.cpp() && cpp.on();
            let mut qual_of = Vec::new();
            if in_cpp {
                qual_of = vec![NONE; f.refs.len()];
                for &(r, q) in &f.cxx().ref_quals {
                    qual_of[r as usize] = remap[fi][q as usize];
                }
            }
            let (mut pick, mut lineage, mut overrides, mut descendants) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            // In a header: the compilations that read it (worked out when first needed).
            let readers: std::cell::OnceCell<Vec<u32>> = std::cell::OnceCell::new();
            let reader_bits: std::cell::OnceCell<Vec<u64>> = std::cell::OnceCell::new();
            // What they compile of a callee's file, per (file, stretch of
            // lines between its directives): asked once, not once a call.
            let known: std::cell::RefCell<FxHashMap<(u32, u32), crate::cond::Tri>> = Default::default();
            let constructs: FxHashSet<u32> = f.cxx().constructs.iter().copied().collect();
            let operands: FxHashMap<u32, (u32, u32, bool)> = f.cxx().operators.iter().map(|&(r, l, rr, binary)| (r, (l, rr, binary))).collect();
            let args_of: FxHashMap<u32, u8> = f.cxx().args.iter().copied().collect();
            let copies: FxHashSet<u32> = f.cxx().copies.iter().copied().collect();
            let mut arg_types: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
            for &(r, at, t) in &f.cxx().arg_types {
                arg_types.entry(r).or_default().push((at, remap[fi][t as usize]));
            }
            let mut made: Vec<(u32, u32)> = Vec::new();
            // The objects a call's arguments are made into (see `Cpp::conversions`).
            let mut convert = |ri: usize, r: &crate::model::Ref, targets: &[u32], ambiguous: bool, lineage: &mut Vec<u32>, edges: &mut Vec<[u32; 3]>| {
                let Some(given) = arg_types.get(&(ri as u32)) else { return };
                made.clear();
                cpp.conversions(&g, &aliases, given, targets, literals, lineage, &mut made);
                for &(c, class) in &made {
                    let several = made.iter().any(|&(o, k)| k == class && syms[o as usize].file != syms[c as usize].file);
                    edges.push([base + r.scope, c, pack_meta(r.line, ambiguous || several || aliased_class(&g, &cpp, c), refkind::CALL)]);
                }
            };
            for (ri, r) in f.refs.iter().enumerate() {
                if let Some(&(lh, rh, binary)) = operands.get(&(ri as u32)).filter(|_| in_cpp) {
                    // An operator used on objects: the left operand's
                    // class's (or a base's) member of that name, else a
                    // function of no class taking operands of their types.
                    // (C++20: `a < b` may be `a <=> b`, `a != b` `a == b`.)
                    let type_of = |h: u32, scratch: &mut Vec<u32>| {
                        (h != NONE).then(|| types.type_of(&f.hints[h as usize], fi, &g, base..end, closure, scratch)).flatten()
                    };
                    let (lt, rt) = (type_of(lh, &mut scratch), type_of(rh, &mut scratch));
                    let name = remap[fi][r.name as usize];
                    let assign = g.name(name) == "operator=";
                    let rewritten = match g.name(name) {
                        "operator<" | "operator>" | "operator<=" | "operator>=" => g.name_id("operator<=>"),
                        "operator!=" => g.name_id("operator=="),
                        _ => None,
                    };
                    // How well a parameter of type `p` takes an operand of
                    // type `t`: not (`None`); as a template's `T`, or an operand
                    // of no known type, does (1); as its type or a base (2).
                    // Of the overloads, those that fit best are the ones.
                    let fit = |p: u32, t: Option<u32>, lineage: &mut Vec<u32>| -> Option<u8> {
                        let Some(t) = t else { return Some(1) };
                        if p == NONE {
                            return None;
                        }
                        if !cpp.types.contains(&p) {
                            return Some(1);
                        }
                        cpp.lineage(t, lineage);
                        (lineage.contains(&p) || aliases.get(&p).is_some_and(|a| a.iter().any(|x| lineage.contains(x)))).then_some(2)
                    };
                    // `a = b` of one type: the copy (or move) its class has
                    // written, or else the compiler's, which is no call here.
                    let right = if assign && (rt.is_none() || rt == lt) { lt } else { rt };
                    tmp.clear();
                    let mut best = 0u8;
                    let mut keep = |c: u32, score: u8, tmp: &mut Vec<u32>| {
                        if score > best {
                            best = score;
                            tmp.clear();
                        }
                        if score == best {
                            tmp.push(c);
                        }
                    };
                    for name in std::iter::once(name).chain(rewritten) {
                        if let Some(sym) = lt.and_then(|l| types.class_member(l, name, fi, &g, closure)).filter(|&s| syms[s as usize].kind == kind::FUNCTION) {
                            // The class's: its overloads, of a binary one those
                            // taking the right operand best.
                            let class = cpp.class(sym);
                            for &c in g.syms_named(name) {
                                if syms[c as usize].kind != kind::FUNCTION || cpp.class(c) != class {
                                    continue;
                                }
                                let score = if !binary {
                                    Some(1)
                                } else {
                                    match cpp.operator_params.get(&c) {
                                        Some(_) if assign && right.is_none() => None,
                                        Some(&(p0, _)) => fit(p0, right, &mut lineage),
                                        None if assign => None,
                                        None => Some(1),
                                    }
                                };
                                if let Some(score) = score {
                                    keep(c, score, &mut tmp);
                                }
                            }
                        } else if binary && !assign && (lt.is_some() || rt.is_some()) {
                            // (A unary one on an object of no class with it is
                            // a built-in: `*p` of a pointer.)
                            for &c in g.syms_named(name) {
                                if syms[c as usize].kind != kind::FUNCTION || cpp.class(c) != NONE {
                                    continue;
                                }
                                let Some(&(p0, p1)) = cpp.operator_params.get(&c) else { continue };
                                if let (Some(a), Some(b)) = (fit(p0, lt, &mut lineage), fit(p1, rt, &mut lineage)) {
                                    keep(c, a + b, &mut tmp);
                                }
                            }
                        }
                        if !tmp.is_empty() {
                            break;
                        }
                    }
                    let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                    if tmp.iter().any(is_def) {
                        tmp.retain(is_def);
                    }
                    if tmp.is_empty() {
                        counts[2] += 1;
                        continue;
                    }
                    let first = (syms[tmp[0] as usize].file, cpp.class(tmp[0]));
                    let ambiguous = tmp.iter().any(|&c| (syms[c as usize].file, cpp.class(c)) != first);
                    counts[ambiguous as usize] += 1;
                    let meta = pack_meta(r.line, ambiguous, refkind::CALL);
                    edges.extend(tmp.iter().map(|&to| [base + r.scope, to, meta]));
                    continue;
                }
                // (`T (a)`, `fmt::string_view (p, n)`: a type called, of
                // which no function but its constructors has the name, is
                // one made.)
                let type_called = in_cpp
                    && r.kind == refkind::CALL
                    && cpp.types.contains(&remap[fi][r.name as usize])
                    && !g.syms_named(remap[fi][r.name as usize]).iter().any(|&c| syms[c as usize].kind == kind::FUNCTION && cpp.class(c) != syms[c as usize].name);
                if in_cpp && (type_called || constructs.contains(&(ri as u32))) {
                    // An object made: a constructor of the class named, or
                    // of the type of the member of the class so named
                    // (`: member_ (a)`), or of what a type's alias names.
                    let name = remap[fi][r.name as usize];
                    let mut class = if cpp.types.contains(&name) {
                        name
                    } else if hint_of[ri] != NONE {
                        let h = &f.hints[hint_of[ri] as usize];
                        types.member_type(remap[fi][h.root as usize], name, fi, closure).unwrap_or(NONE)
                    } else {
                        NONE
                    };
                    let ctor_of = |c: u32, class: u32| syms[c as usize].kind == kind::FUNCTION && cpp.class(c) == class;
                    let has_ctor = |class: u32| g.syms_named(class).iter().any(|&c| ctor_of(c, class));
                    if class != NONE && !has_ctor(class) {
                        if let Some(tag) = aliases.get(&class).into_iter().flatten().copied().find(|&t| has_ctor(t)) {
                            class = tag;
                        }
                    }
                    tmp.clear();
                    if class != NONE {
                        tmp.extend(g.syms_named(class).iter().copied().filter(|&c| ctor_of(c, class)));
                    }
                    cpp.by_arity(args_of.get(&(ri as u32)).copied(), &mut tmp, &[], syms);
                    if copies.contains(&(ri as u32)) {
                        // A copy: by the constructor taking one of its class,
                        // or by the compiler's, which is no call here.
                        tmp.retain(|c| cpp.params.get(&(*c, 0)) == Some(&class));
                    }
                    let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                    if tmp.iter().any(is_def) {
                        tmp.retain(is_def);
                    }
                    if tmp.is_empty() {
                        counts[2] += 1;
                        continue;
                    }
                    let first = syms[tmp[0] as usize].file;
                    let ambiguous = tmp.iter().any(|&c| syms[c as usize].file != first) || tmp.iter().all(|&c| aliased_class(&g, &cpp, c));
                    counts[ambiguous as usize] += 1;
                    let meta = pack_meta(r.line, ambiguous, refkind::CALL);
                    edges.extend(tmp.iter().map(|&to| [base + r.scope, to, meta]));
                    convert(ri, r, &tmp, ambiguous, &mut lineage, &mut edges);
                    continue;
                }
                let (kind, maybe) = (r.kind & !refkind::MAYBE, r.kind & refkind::MAYBE != 0);
                let cands = g.syms_named(remap[fi][r.name as usize]);
                // `_Pragma ("...")`, `__asm__ __volatile__ (...)`: no call,
                // whatever a parse in a macro's body took for a function.
                if !cands.is_empty() && crate::mask::is_keyword(g.name(remap[fi][r.name as usize]).as_bytes()) {
                    counts[2] += 1;
                    continue;
                }
                // A member name can only mean a member, and nothing else can.
                let member = matches!(kind, refkind::MEMBER | refkind::ICALL);
                let through = kind == refkind::ICALL;
                // `#define error(...) (error (__VA_ARGS__), flag ())`: a macro
                // is not expanded inside itself, so there the name is the
                // function.
                let from = &syms[(base + r.scope) as usize];
                let own_name = from.kind == kind::MACRO && from.name == remap[fi][r.name as usize];
                let want = |s: &Sym| {
                    if member {
                        s.kind == kind::FIELD && (!through || s.flags & flag::CALLABLE != 0)
                    } else {
                        s.kind != kind::FILE && s.kind != kind::FIELD && !(own_name && s.kind == kind::MACRO)
                    }
                };
                // With the type of what the member is taken from, the member
                // is one symbol and no guess.
                let typed = (member && hint_of[ri] != NONE)
                    .then(|| types.member(&f.hints[hint_of[ri] as usize], remap[fi][r.name as usize], fi, &g, base..end, closure, &mut scratch))
                    .flatten();
                // (`tfm::format`, with `namespace tfm = tinyformat;`: `tinyformat`.)
                let qual = if in_cpp { qual_of[ri] } else { NONE };
                let qual = cpp.namespace_aliases.get(&qual).copied().unwrap_or(qual);
                let mut edge_kind = kind;
                // A method chosen by its name alone, the object's type not
                // known (or not one of the corpus's: `name.c_str ()` of a
                // `std::string`), is a guess, however few there are.
                let mut guessed = false;
                match typed {
                    Some(sym) if in_cpp && syms[sym as usize].kind == kind::FUNCTION => {
                        // A method: wherever its class defines it.
                        let class = cpp.class(sym);
                        tmp.clear();
                        tmp.extend(cands.iter().filter(|&&c| syms[c as usize].kind == kind::FUNCTION && cpp.class(c) == class));
                        if tmp.is_empty() {
                            tmp.push(sym);
                        }
                    }
                    Some(sym) => {
                        tmp.clear();
                        tmp.push(sym);
                    }
                    None if in_cpp && through => {
                        // `x.f ()` of a type not known: any method of that
                        // name (of the class written, `p->Base::f ()`), and
                        // any member that holds a function.
                        pick.clear();
                        if qual != NONE && !g.name(qual).is_empty() {
                            cpp.nearest(cands, qual, &mut lineage, &mut pick);
                        }
                        if pick.is_empty() {
                            pick.extend(cands.iter().filter(|&&c| syms[c as usize].kind == kind::FUNCTION && cpp.class(c) != NONE));
                        }
                        pick.extend(cands.iter().filter(|&&c| want(&syms[c as usize])));
                        let seen = |c: &&u32| {
                            let s = &syms[**c as usize];
                            s.file == fi as u32 || closure.member[s.file as usize] || s.flags & flag::STATIC == 0 || s.flags & flag::HEADER != 0
                        };
                        tmp.clear();
                        tmp.extend(pick.iter().filter(seen));
                        guessed = true;
                    }
                    None if in_cpp && !member => {
                        // `f ()` in a method: its class's, or a base's, before
                        // anything else; `X::f ()`: X's (a namespace's, or
                        // `::f`: one of no class); elsewhere, one of no class.
                        let caller_class = cpp.class(base + r.scope);
                        let caller_ns = cpp.ns(base + r.scope);
                        if qual != NONE && g.name(qual).is_empty() {
                            // `::f`: one of the global namespace.
                            cpp.nearest(cands, NONE, &mut lineage, &mut pick);
                            pick.retain(|&c| cpp.ns(c) == NONE);
                        } else if qual != NONE && cpp.namespaces.contains(&qual) {
                            // `ns::f`: one of that namespace (or, where the
                            // namespace is opened by a macro the parser did
                            // not see through, one of no class).
                            cpp.nearest(cands, NONE, &mut lineage, &mut pick);
                            if pick.iter().any(|&c| cpp.ns(c) == qual) {
                                pick.retain(|&c| cpp.ns(c) == qual);
                            } else {
                                pick.retain(|&c| cpp.ns(c) == NONE);
                            }
                        } else if qual != NONE && cpp.types.contains(&qual) {
                            cpp.nearest(cands, qual, &mut lineage, &mut pick);
                            if pick.is_empty() {
                                cpp.nearest(cands, NONE, &mut lineage, &mut pick);
                            }
                        } else if qual != NONE && cands.iter().any(|&c| cpp.class(c) == qual) {
                            // A class known by its members alone, one a macro
                            // makes (Boost.Test's case): its.
                            cpp.nearest(cands, qual, &mut lineage, &mut pick);
                        } else if qual != NONE {
                            // A scope the corpus does not have (`std::max`):
                            // only a function of no class, and only a guess.
                            cpp.nearest(cands, NONE, &mut lineage, &mut pick);
                            guessed = true;
                        } else {
                            cpp.nearest(cands, caller_class, &mut lineage, &mut pick);
                            if pick.is_empty() && caller_class != NONE {
                                cpp.nearest(cands, NONE, &mut lineage, &mut pick);
                            }
                            // A function of the caller's own namespace hides
                            // those of others.
                            if caller_ns != NONE && pick.iter().any(|&c| cpp.class(c) == NONE && cpp.ns(c) == caller_ns) {
                                pick.retain(|&c| cpp.class(c) != NONE || cpp.ns(c) == caller_ns);
                            }
                        }
                        // (Qualified with a scope the corpus does not have,
                        // `std::max`: nothing of the corpus, not anything of
                        // that name.)
                        if pick.is_empty() && qual == NONE {
                            pick.extend_from_slice(cands);
                        }
                        in_sight(&pick, syms, base..end, closure, &sure.member, want, &mut tmp);
                        // `T (a)`, `T x (a)`: a class named is made, by its
                        // constructor if it has one written; else the class
                        // is only used.
                        let class_kind = |c: &u32| matches!(syms[*c as usize].kind, kind::CLASS | kind::STRUCT | kind::UNION);
                        if kind == refkind::CALL && !tmp.is_empty() && tmp.iter().all(class_kind) {
                            let name = remap[fi][r.name as usize];
                            let ctors = cands.iter().filter(|&&c| syms[c as usize].kind == kind::FUNCTION && cpp.class(c) == name);
                            pick.clear();
                            pick.extend(ctors);
                            if pick.is_empty() {
                                edge_kind = refkind::USE;
                            } else {
                                std::mem::swap(&mut tmp, &mut pick);
                            }
                        }
                    }
                    None => in_sight(cands, syms, base..end, closure, &sure.member, want, &mut tmp),
                }
                // C names no member of a C++ class (`write` is not
                // `RedisQtAdapter::write`).
                if !in_cpp && !cpp.class_of.is_empty() {
                    tmp.retain(|&c| cpp.class(c) == NONE);
                }
                // Nor, from a source file, only methods of classes other
                // source files have all to themselves (a test's fixture):
                // no name of them is in sight here. (`GetParam ()` in a test
                // is gtest's.) (Beside others, one may be an override of
                // them, through a base the file sees.)
                if in_cpp && closure.unit && !member && tmp.iter().all(|&c| unseen_class(&g, &cpp, c, fi as u32, closure)) {
                    tmp.clear();
                }
                if tmp.is_empty() {
                    counts[2] += 1;
                    continue;
                }
                let callable = |c: &u32| matches!(syms[*c as usize].kind, kind::FUNCTION | kind::MACRO);
                if matches!(kind, refkind::CALL | refkind::PCALL) && tmp.iter().any(callable) {
                    tmp.retain(callable);
                }
                if in_cpp {
                    cpp.by_arity(args_of.get(&(ri as u32)).copied(), &mut tmp, cands, syms);
                }
                let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                if tmp.iter().any(is_def) {
                    tmp.retain(is_def);
                }
                let built = |c: &u32| !unread.get(syms[*c as usize].file as usize).copied().unwrap_or(false);
                if !apart.get(fi).copied().unwrap_or(false) {
                    // (None built, as x86-64's `memcpy`, written in assembly:
                    // this program's before another's.)
                    let own = |c: &u32| !apart.get(syms[*c as usize].file as usize).copied().unwrap_or(false);
                    if tmp.iter().any(built) {
                        tmp.retain(built);
                    } else if tmp.iter().any(own) {
                        tmp.retain(own);
                    }
                }
                // A macro, or a function defined in a header, is there only
                // where its file is read: in a source file, not one its own
                // compilation does not read (`core_cm7.h`'s, beside the
                // `core_cm4.h` it does).
                let unit_here = unit_of.get(fi).copied().unwrap_or(NONE);
                if unit_here != NONE {
                    let local = |c: &u32| {
                        let s = &syms[*c as usize];
                        s.kind == kind::MACRO || s.kind == kind::FUNCTION && s.flags & flag::DECL == 0 && s.flags & flag::HEADER != 0
                    };
                    let read_here = |c: &u32| {
                        let s = &syms[*c as usize];
                        let p = path_of.get(s.file as usize).copied().unwrap_or(NONE);
                        p == NONE || s.file as usize == fi || readings.reads(unit_here, p)
                    };
                    // (But for a header nothing here includes, or no
                    // compilation reads, where the compilation includes one
                    // the build makes, which may bring it in.)
                    let generated = readings.generated.get(unit_here as usize).copied().unwrap_or(true);
                    let may = |c: &u32| {
                        let p = path_of.get(syms[*c as usize].file as usize).copied().unwrap_or(NONE);
                        let elsewhere = readings.orphans.binary_search(&p).is_ok() || unread.get(syms[*c as usize].file as usize).copied().unwrap_or(false);
                        !local(c) || read_here(c) || generated && elsewhere
                    };
                    // Where the compilation reads one of those, or a
                    // prototype of the name: it means that, not a macro of
                    // a header it does not read.
                    let declared = cands.iter().any(|c| syms[*c as usize].flags & flag::DECL != 0 && read_here(c));
                    if (declared || tmp.iter().any(|c| local(c) && read_here(c)))
                        && tmp.iter().any(may) && !tmp.iter().all(may) {
                            tmp.retain(may);
                        }
                }
                // In a source file: what its own compilation compiles of a
                // header read one way here and another elsewhere.
                // In a header: what every compilation that reads it compiles.
                let unit = unit_of.get(fi).copied().unwrap_or(NONE);
                let compiled = |c: u32| {
                    use crate::cond::Tri;
                    let s = &syms[c as usize];
                    let h = header_of.get(s.file as usize).copied().unwrap_or(NONE);
                    let p = path_of.get(s.file as usize).copied().unwrap_or(NONE);
                    // In a file every compilation reads one way: compiled
                    // where it is read for certain, unless under an `#if`
                    // not known. (None: a compilation that does not read
                    // it has no say.)
                    let in_unit = |u: u32| -> Option<Tri> {
                        if h != NONE {
                            return readings.units[u as usize].1.binary_search_by_key(&h, |w| w.0).is_ok().then(|| readings.compiled(u, h, s.line));
                        }
                        if p == NONE || !readings.reads(u, p) {
                            return None;
                        }
                        // (Its own compilation: read wherever the call is,
                        // the call under the branches it is included in.)
                        let along = || {
                            u == unit
                                && readings.read_under(u, p).is_some_and(|ways| {
                                    ways.iter().any(|lines| lines.iter().all(|&l| files[fi].undecided.iter().any(|b| b.0 <= r.line && r.line <= b.1 && b.2 == l)))
                                })
                        };
                        Some(if s.flags & flag::CONDITIONAL == 0 && (readings.reads_surely(u, p) || along()) { Tri::True } else { Tri::Unknown })
                    };
                    if h == NONE && p == NONE {
                        return Tri::Unknown;
                    }
                    if unit != NONE {
                        return in_unit(unit).unwrap_or(Tri::Unknown);
                    }
                    // (Here every compilation reading the callee's file reads
                    // it one way per stretch: one answer for all of it.)
                    let stretch = if h != NONE { readings.files[h as usize].1.partition_point(|&l| l < s.line) as u32 } else { u32::MAX - (s.flags & flag::CONDITIONAL != 0) as u32 };
                    if let Some(&t) = known.borrow().get(&(s.file, stretch)) {
                        return t;
                    }
                    let at = path_of.get(fi).copied().unwrap_or(NONE);
                    let among = reader_bits.get_or_init(|| {
                        if at == NONE { vec![0u64; readings.units.len().div_ceil(64)] } else { who.path(at).0.bits(readings.units.len()) }
                    });
                    // (A compilation that does not read the callee's file
                    // has no say: what it calls is not that.)
                    // (What every compilation compiles of it is the same
                    // from any file: worked out once for them all.)
                    let conditional = s.flags & flag::CONDITIONAL != 0;
                    let t = who.compile(among, h, stretch, p, conditional).unwrap_or_else(|| {
                        let key = (s.file, stretch);
                        if let Some(&t) = everywhere.lock().unwrap().get(&key) {
                            return t;
                        }
                        let t = who.compile(&all, h, stretch, p, conditional).unwrap_or(Tri::Unknown);
                        everywhere.lock().unwrap().insert(key, t);
                        t
                    });
                    known.borrow_mut().insert((s.file, stretch), t);
                    t
                };
                if !header_of.is_empty() && tmp.iter().any(|&c| compiled(c) == crate::cond::Tri::False) && tmp.iter().any(|&c| compiled(c) != crate::cond::Tri::False) {
                    tmp.retain(|&c| compiled(c) != crate::cond::Tri::False);
                }
                // Without types, a member name shared by many structs says
                // nothing about which one is meant.
                if kind == refkind::MEMBER && tmp.len() > MAX_MEMBER_CANDIDATES {
                    counts[2] += 1;
                    continue;
                }
                if tmp.len() > MAX_CANDIDATES {
                    tmp.truncate(MAX_CANDIDATES);
                    counts[3] += 1;
                }
                // A macro that is not there in every configuration is one
                // reading of the name; so is what a call through it reaches.
                // (Not where the call is under the same branches as the
                // macro: compiled, it finds the macro there.)
                let optional = |c: &u32| {
                    let s = &syms[*c as usize];
                    let branches = |file: usize, line: u32| files[file].undecided.iter().filter(move |u| u.0 <= line && line <= u.1).map(|u| u.3);
                    let along = || branches(s.file as usize, s.line).all(|k| branches(fi, r.line).any(|h| h == k));
                    s.kind == kind::MACRO
                        && (s.flags & flag::CONDITIONAL != 0 && !along() || s.file != fi as u32 && !sure.member[s.file as usize])
                        && compiled(*c) != crate::cond::Tri::True
                };
                // C++: a name called that only a macro the corpus defines
                // once can mean is that macro, defined or not where it is
                // tested (`#if !GTEST_DONT_DEFINE_ASSERT_EQ`): without it
                // the call would not compile.
                let sole_macro = in_cpp
                    && kind == refkind::CALL
                    && tmp.len() == 1
                    && syms[tmp[0] as usize].kind == kind::MACRO
                    && cands.iter().all(|&c| c == tmp[0] || !matches!(syms[c as usize].kind, kind::FUNCTION | kind::MACRO));
                let optional = |c: &u32| !sole_macro && optional(c);
                // (C++: overloads are one function, of one class in one file.)
                let several = if in_cpp {
                    let first = (syms[tmp[0] as usize].file, cpp.class(tmp[0]));
                    tmp.iter().any(|&c| (syms[c as usize].file, cpp.class(c)) != first)
                } else {
                    // (A macro that wraps the function of its name is the
                    // way to it, not another reading of the call.)
                    let function = |c: &u32| syms[*c as usize].kind == kind::FUNCTION;
                    let wraps = tmp.iter().any(function);
                    tmp.iter().filter(|c| !(wraps && wrappers.contains(c))).count() > 1
                };
                let twin = in_cpp && tmp.iter().all(|&c| aliased_class(&g, &cpp, c));
                let ambiguous = several || maybe || guessed || twin || tmp.iter().any(optional);
                counts[ambiguous as usize] += 1;
                // `SI_EDGE_WHY=<name>[,<name>...]`: why each call of those names that is
                // not certain is not.
                if ambiguous && edge_why.as_deref().is_some_and(|w| w.split(',').any(|w| g.name(remap[fi][r.name as usize]) == w)) {
                    let at = path_of.get(fi).and_then(|&p| readings.paths.get(p as usize)).map_or("?", |p| p.as_str());
                    let c = tmp[0];
                    // (Here worked out only to say: the link asks the turned-over table.)
                    let readers = readers.get_or_init(|| {
                        let p = path_of.get(fi).copied().unwrap_or(NONE);
                        if p == NONE { Vec::new() } else { (0..readings.units.len() as u32).filter(|&u| readings.reads(u, p)).collect() }
                    });
                    let n_readers = readers.len();
                    eprintln!(
                        "edgewhy   {at}:{}: {} {} target(s) {:?}, maybe {maybe}, guessed {guessed}, optional {}, conditional {}, compiled {:?}, in its unit {}, readers {n_readers}",
                        r.line,
                        g.name(remap[fi][r.name as usize]),
                        tmp.len(),
                        tmp.iter().map(|&t| format!("{}{}", files[syms[t as usize].file as usize].path, if optional(&t) { " (optional)" } else { "" })).collect::<Vec<_>>(),
                        tmp.iter().any(optional),
                        syms[c as usize].flags & flag::CONDITIONAL != 0,
                        compiled(c),
                        unit != NONE,
                    );
                    // How its own compilation reads the macro's file.
                    let s = &syms[c as usize];
                    {
                        // In a header: what each compilation reading it says
                        // (yes, no, not known, does not read the macro's file).
                        use crate::cond::Tri;
                        let h = header_of.get(s.file as usize).copied().unwrap_or(NONE);
                        let p = path_of.get(s.file as usize).copied().unwrap_or(NONE);
                        let say = |u: u32| -> Option<Tri> {
                            if h != NONE {
                                return readings.units[u as usize].1.binary_search_by_key(&h, |w| w.0).is_ok().then(|| readings.compiled(u, h, s.line));
                            }
                            if p == NONE || !readings.reads(u, p) {
                                return None;
                            }
                            Some(if s.flags & flag::CONDITIONAL == 0 && readings.reads_surely(u, p) { Tri::True } else { Tri::Unknown })
                        };
                        let mut n = [0u32; 4];
                        let mut odd: Vec<&str> = Vec::new();
                        for &u in readers {
                            let k = match say(u) { Some(Tri::True) => 0, Some(Tri::False) => 1, Some(Tri::Unknown) => 2, None => 3 };
                            n[k] += 1;
                            if (k == 1 || k == 2) && odd.len() < 4 {
                                odd.push(readings.units[u as usize].0.as_str());
                            }
                        }
                        eprintln!("edgewhy     readers say: yes {}, no {}, not known {}, do not read it {}; e.g. {}", n[0], n[1], n[2], n[3], odd.join(" "));
                    }
                    let h = header_of.get(s.file as usize).copied().unwrap_or(NONE);
                    let p = path_of.get(s.file as usize).copied().unwrap_or(NONE);
                    let file = if p.ne(&NONE) { readings.paths[p as usize].as_str() } else { "?" };
                    if h == NONE {
                        eprintln!("edgewhy     {file}:{}: read one way by all; read {}, for sure {}", s.line, unit != NONE && p != NONE && readings.reads(unit, p), unit != NONE && p != NONE && readings.reads_surely(unit, p));
                        let call: Vec<u32> = files[fi].undecided.iter().filter(|b| b.0 <= r.line && r.line <= b.1).map(|b| b.2).collect();
                        let under = (unit != NONE && p != NONE).then(|| readings.read_under(unit, p)).flatten();
                        eprintln!("edgewhy     included under branches (lines): {under:?}; the call is under {call:?}");
                    } else {
                        let lines = &readings.files[h as usize].1;
                        let stretch = lines.partition_point(|&l| l < s.line);
                        let way = (unit != NONE).then(|| readings.units[unit as usize].1.binary_search_by_key(&h, |w| w.0).ok()).flatten();
                        let way = way.map(|i| &readings.ways[h as usize][readings.units[unit as usize].1[i].1 as usize]);
                        eprintln!(
                            "edgewhy     {file}:{}: read {} ways, {} directives (around: {:?}), stretch {stretch}, its way {:?}",
                            s.line,
                            readings.ways[h as usize].len(),
                            lines.len(),
                            &lines[stretch.saturating_sub(2)..(stretch + 2).min(lines.len())],
                            way.map(|w| (w.len(), w.get(stretch.saturating_sub(2)..(stretch + 2).min(w.len())).map(|x| x.to_vec()))),
                        );
                    }
                }
                if in_cpp && through {
                    // A method reached through an object is called, not a
                    // function stored in a member.
                    let meta = |to: u32, amb: bool| {
                        let k = if syms[to as usize].kind == kind::FUNCTION { refkind::CALL } else { kind };
                        pack_meta(r.line, amb, k)
                    };
                    edges.extend(tmp.iter().map(|&to| [base + r.scope, to, meta(to, ambiguous)]));
                } else {
                    let meta = pack_meta(r.line, ambiguous, edge_kind);
                    edges.extend(tmp.iter().map(|&to| [base + r.scope, to, meta]));
                }
                // (Not of a function no header read here declares: gtest's
                // `printf ("...")` is the C library's, not fmt's.)
                let hidden = tmp.iter().any(|&c| {
                    let seen = |d: &u32| syms[*d as usize].file == fi as u32 || closure.member[syms[*d as usize].file as usize];
                    syms[c as usize].kind == kind::FUNCTION && !g.syms_named(syms[c as usize].name).iter().any(|d| seen(d) && cpp.class(*d) == cpp.class(c))
                });
                if in_cpp && edge_kind != refkind::USE && !hidden {
                    convert(ri, r, &tmp, ambiguous, &mut lineage, &mut edges);
                }
                // A virtual method called on an object may run an override:
                // those of the classes derived from its class.
                if in_cpp && (through || (!member && qual == NONE)) && edge_kind != refkind::USE {
                    overrides.clear();
                    for &t in tmp.iter() {
                        let (s, class) = (&syms[t as usize], cpp.class(t));
                        if s.kind != kind::FUNCTION || class == NONE || !cpp.is_virtual(class, s.name, &mut lineage) {
                            continue;
                        }
                        cpp.descendants(class, &mut descendants);
                        for &c in g.syms_named(s.name) {
                            let o = &syms[c as usize];
                            if o.kind == kind::FUNCTION && o.flags & flag::DECL == 0 && descendants.contains(&cpp.class(c)) && !tmp.contains(&c) && !overrides.contains(&c) {
                                overrides.push(c);
                            }
                        }
                    }
                    overrides.truncate(MAX_CANDIDATES);
                    let meta = pack_meta(r.line, true, refkind::CALL);
                    edges.extend(overrides.iter().map(|&to| [base + r.scope, to, meta]));
                }
            }

            // Functions stored in members and variables: an edge from the
            // member or variable to the function.
            let mut bound: Vec<[u32; 3]> = Vec::new();
            let mut slots: Vec<u32> = Vec::new();
            for b in f.binds.iter().filter(|b| b.slot != NONE) {
                let typed = (!b.var && b.index == NONE && b.hint != NONE)
                    .then(|| types.member(&f.hints[b.hint as usize], remap[fi][b.slot as usize], fi, &g, base..end, closure, &mut scratch))
                    .flatten();
                if let Some(sym) = typed {
                    slots.clear();
                    slots.push(sym);
                } else if b.var {
                    in_sight(g.syms_named(remap[fi][b.slot as usize]), syms, base..end, closure, &sure.member, |s| s.kind == kind::GLOBAL, &mut slots);
                } else {
                    // Positional: the member is the struct's `index`-th.
                    // The struct may be named through a macro, and then it
                    // is the one the macro names where this is read: in the
                    // translation unit itself first, else here, else any
                    // that has a member at that place.
                    let mut ty = remap[fi][b.slot as usize];
                    if b.index != NONE && !records.contains_key(&ty) && !aliases.contains_key(&ty) {
                        if let Some(named) = renames.get(&ty) {
                            let has = |tag: u32| {
                                layout(tag, fi as u32, closure, &records, &aliases, files).is_some_and(|(_, fields)| fields.get(b.index as usize).is_some_and(|f| *f != NONE))
                            };
                            let pick = named
                                .iter()
                                .find(|(file, tag)| *file == b.unit && has(*tag))
                                .or_else(|| named.iter().find(|(file, tag)| *file == fi as u32 && has(*tag)))
                                .or_else(|| named.iter().find(|(_, tag)| b.unit == NONE && has(*tag)));
                            match pick {
                                Some(&(_, tag)) => ty = tag,
                                None => continue,
                            }
                        }
                    }
                    let (name, home) = if b.index == NONE {
                        (remap[fi][b.slot as usize], None)
                    } else {
                        let layout = layout(ty, fi as u32, closure, &records, &aliases, files);
                        match layout.and_then(|(rf, fields)| Some((rf, *fields.get(b.index as usize)?))) {
                            Some((rf, field)) if field != NONE => (remap[rf as usize][field as usize], Some(rf)),
                            _ => continue,
                        }
                    };
                    let cands = g.syms_named(name);
                    let holds = |s: &Sym| s.kind == kind::FIELD && s.flags & flag::CALLABLE != 0;
                    in_sight(cands, syms, base..end, closure, &sure.member, holds, &mut slots);
                    // The very member at that position, when the struct's
                    // layout says which definition it is.
                    let exact = (b.index != NONE)
                        .then(|| record_of(ty, fi as u32, closure, &records, &aliases))
                        .flatten()
                        .and_then(|(rf, ri)| {
                            let def = *files[rf as usize].records[ri as usize].defs.get(b.index as usize)?;
                            (def != NONE).then(|| g.files[rf as usize].sym_start + def)
                        });
                    if let Some(sym) = exact {
                        slots.clear();
                        slots.push(sym);
                    } else if let Some(rf) = home {
                        let there = |c: &&u32| holds(&syms[**c as usize]) && syms[**c as usize].file == rf;
                        if cands.iter().any(|c| there(&c)) {
                            slots.clear();
                            slots.extend(cands.iter().filter(there));
                        }
                    }
                }
                if slots.is_empty() || slots.len() > MAX_MEMBER_CANDIDATES {
                    continue;
                }
                let named = g.syms_named(remap[fi][b.func as usize]);
                in_sight(named, syms, base..end, closure, &sure.member, |s| s.kind == kind::FUNCTION, &mut tmp);
                // Read in another translation unit, the name is first of all
                // that unit's own function.
                let own = |c: &&u32| syms[**c as usize].kind == kind::FUNCTION && syms[**c as usize].file == b.unit;
                if b.unit != NONE && named.iter().any(|c| own(&c)) {
                    tmp.clear();
                    tmp.extend(named.iter().filter(own));
                }
                let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                if tmp.iter().any(is_def) {
                    tmp.retain(is_def);
                }
                tmp.truncate(MAX_CANDIDATES);
                let meta = pack_meta(b.line, slots.len() > 1 || tmp.len() > 1, refkind::BIND);
                for &slot in &slots {
                    bound.extend(tmp.iter().map(|&to| [slot, to, meta]));
                }
            }
            // Callbacks: a function passed where the callee calls its
            // parameter is called by the callee.
            let mut handed: Vec<[u32; 3]> = Vec::new();
            if !through_param.is_empty() {
                let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                for a in &f.arg_passes {
                    in_sight(g.syms_named(remap[fi][a.callee as usize]), syms, base..end, closure, &sure.member, |s| s.kind == kind::FUNCTION, &mut slots);
                    if slots.iter().any(is_def) {
                        slots.retain(is_def);
                    }
                    for &callee in slots.iter().take(MAX_MEMBER_CANDIDATES) {
                        for &(_, line) in through_param.get(&callee).into_iter().flatten().filter(|p| p.0 == a.index) {
                            in_sight(g.syms_named(remap[fi][a.func as usize]), syms, base..end, closure, &sure.member, |s| s.kind == kind::FUNCTION, &mut tmp);
                            if tmp.iter().any(is_def) {
                                tmp.retain(is_def);
                            }
                            tmp.truncate(MAX_CANDIDATES);
                            let meta = pack_meta(line, slots.len() > 1 || tmp.len() > 1, refkind::PCALL);
                            handed.extend(tmp.iter().map(|&to| [callee, to, meta]));
                        }
                    }
                }
            }
            // Every place a function is stored is kept (for "who assigns
            // this member"); the graph needs each pair only once.
            bound.sort_unstable();
            bound.dedup();
            let sites = bound.iter().map(|e| [fi as u32, e[2] >> 4, e[0], e[1]]).collect();
            bound.dedup_by_key(|e| (e[0], e[1]));
            edges.extend(bound);
            handed.sort_unstable();
            handed.dedup();
            (edges, counts, sites, handed)
        })
        .collect();
    // The same callback is handed over in many places.
    let mut handed: Vec<[u32; 3]> = per_file.iter().flat_map(|p| p.3.iter().copied()).collect();
    handed.sort_unstable();
    handed.dedup();
    for (_, c, sites, _) in &per_file {
        stats.bind_sites.extend_from_slice(sites);
        stats.resolved += c[0];
        stats.ambiguous += c[1];
        stats.unresolved += c[2];
        stats.capped += c[3];
    }
    stats.refs = stats.resolved + stats.ambiguous + stats.unresolved;
    stats.edges = (handed.len() + per_file.iter().map(|p| p.0.len()).sum::<usize>()) as u64;
    stats.resolve_ms = ms(t);
    drop((remap, names));
    crate::mem_mark("resolved");

    // 4. Forward and reverse adjacency. The edges are never all in one
    //    list: each file's are put in their place and let go, and the
    //    reverse is made from the forward one.
    let t = Instant::now();
    let mut out_off = vec![0u32; n_syms + 1];
    for e in std::iter::once(&handed).chain(per_file.iter().map(|p| &p.0)).flatten() {
        out_off[e[0] as usize + 1] += 1;
    }
    for i in 0..n_syms {
        out_off[i + 1] += out_off[i];
    }
    let mut next = out_off.clone();
    let mut out = vec![EdgeHalf { other: 0, meta: 0 }; stats.edges as usize];
    for list in std::iter::once(handed).chain(per_file.into_iter().map(|p| p.0)) {
        for e in &list {
            let slot = &mut next[e[0] as usize];
            out[*slot as usize] = EdgeHalf { other: e[1], meta: e[2] };
            *slot += 1;
        }
    }
    let (in_off, inc) = transposed(n_syms, &out_off, &out, next);
    g.out_off = out_off;
    g.out = out;
    g.in_off = in_off;
    g.inc = inc;
    stats.csr_ms = ms(t);
    crate::mem_mark("packed");

    crate::progress::idle();
    (g, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::Extractor;
    use crate::mask::MacroSets;

    fn graph(files: &[(&str, &str)]) -> Graph {
        let mut ex = Extractor::new();
        let t = MacroSets::default();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &t)).collect();
        link(&facts).0
    }

    /// Files defining the symbols that `caller` references under `name`.
    fn targets(g: &Graph, caller: &str, name: &str) -> Vec<String> {
        let mut out: Vec<String> = g
            .find(caller)
            .iter()
            .flat_map(|&s| g.refs_from(s))
            .filter(|e| g.sym_name(e.other) == name)
            .map(|e| g.file_path(g.syms[e.other as usize].file).to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Like `graph`, with the corpus-wide pre-pass, so macros are expanded.
    fn expanded(files: &[(&str, &str)]) -> Graph {
        let sets = MacroSets::from_defines(files.iter().flat_map(|(_, src)| crate::mask::defines(src.as_bytes(), &crate::cond::Conditions::default())));
        let mut ex = Extractor::new();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &sets)).collect();
        link(&facts).0
    }

    fn kinds(g: &Graph, name: &str) -> Vec<u8> {
        g.find(name).iter().map(|&s| g.syms[s as usize].kind).collect()
    }

    /// (callee, member or variable it was reached through) for all `caller` calls.
    fn callees(g: &Graph, caller: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = g
            .find(caller)
            .iter()
            .flat_map(|&s| crate::callees_through_slots(g, s))
            .map(|c| (g.sym_name(c.caller).to_string(), c.slot.map_or(String::new(), |m| g.sym_name(m).to_string())))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn a_pasted_name_is_a_definition_in_each_unit_that_makes_it() {
        let g = expanded(&[
            ("cat.h", "#define CAT(a,b) a##b\n"),
            ("tmpl.h", "#include \"cat.h\"\nint NAME (helper) (void) { return 0; }\nint NAME (entry) (void) { return NAME (helper) (); }\n"),
            ("x32.c", "#define NAME(x) CAT (x, _32)\n#include \"tmpl.h\"\n"),
            ("x64.c", "#define NAME(x) CAT (x, _64)\n#include \"tmpl.h\"\n"),
            ("user.c", "int entry_32 (void);\nint f (void) { return entry_32 (); }\n"),
        ]);
        assert_eq!(targets(&g, "f", "entry_32"), ["tmpl.h"]);
        // Each variant calls its own helper, not the other unit's.
        assert_eq!(targets(&g, "entry_32", "helper_32"), ["tmpl.h"]);
        assert!(targets(&g, "entry_32", "helper_64").is_empty());
        assert_eq!(targets(&g, "entry_64", "helper_64"), ["tmpl.h"]);
        assert!(g.find("NAME").iter().all(|&s| g.syms[s as usize].kind == kind::MACRO));
    }

    #[test]
    fn a_macro_that_declares_its_argument_declares_it() {
        let g = expanded(&[
            ("r.h", "#define START(n) enum n {\n#define NUM(n, v) n = v,\n#define END(n) n };\n"),
            ("avr.h", "#include \"r.h\"\nSTART (kinds)\n  NUM (K_NONE, 0)\n  NUM (K_ONE, 1)\nEND (K_max)\n"),
        ]);
        assert_eq!(kinds(&g, "kinds"), [kind::ENUM]);
        assert_eq!(kinds(&g, "K_ONE"), [kind::ENUM_CONST]);
    }

    #[test]
    fn a_renaming_macro_names_the_definition_only_where_it_is_defined() {
        let g = expanded(&[
            ("impl.c", "#ifdef WIDE\n#define build wide_build\n#endif\nint build (int x) { return x; }\n"),
            ("wide.c", "#define WIDE\n#include \"impl.c\"\n"),
        ]);
        assert_eq!(kinds(&g, "wide_build"), [kind::FUNCTION]);
        assert!(kinds(&g, "build").contains(&kind::FUNCTION));
    }

    #[test]
    fn calls_through_a_member_reach_what_is_stored_there() {
        let g = expanded(&[
            ("ops.h", "struct ops { int (*open) (int); int (*close) (int); int count; };\n#define CALL(o, m, a) ((*((o)->m)) (a))\n"),
            (
                "a.c",
                "#include \"ops.h\"\nstatic int a_open (int x) { return x; }\nstatic int a_close (int x) { return x; }\n\
                 #define PAIR(p) p##_open, p##_close\nconst struct ops a_ops = { PAIR (a), 0 };\n",
            ),
            (
                "b.c",
                "#include \"ops.h\"\nstatic int b_open (int x) { return x; }\nint b_close (int x) { return x; }\n\
                 const struct ops b_ops = {\n#ifdef FAST\n  b_open,\n#else\n  0,\n#endif\n  b_close };\n",
            ),
            ("use.c", "#include \"ops.h\"\nint run (struct ops *o) { return o->open (1) + CALL (o, close, 2) + o->count; }\n"),
        ]);
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            callees(&g, "run"),
            [pair("CALL", ""), pair("a_close", "close"), pair("a_open", "open"), pair("b_close", "close"), pair("b_open", "open")]
        );
        let callers: Vec<&str> =
            crate::callers_through_macros(&g, g.find("a_close")[0]).iter().filter(|c| c.indirect()).map(|c| g.sym_name(c.caller)).collect();
        assert_eq!(callers, ["run"]);
        // Members are symbols; a plain use of one is not a call through it.
        assert_eq!(kinds(&g, "count"), [kind::FIELD]);
        assert_eq!(targets(&g, "run", "count"), ["ops.h"]);
    }

    #[test]
    fn calls_through_a_variable_reach_what_is_assigned_to_it() {
        let g = graph(&[(
            "h.c",
            "static int (*hook) (int);\nstatic int impl (int x) { return x; }\nstatic int other (int x) { return x; }\n\
             void set (int fast) { hook = fast ? impl : &other; }\nint go (void) { return hook (3); }\n",
        )]);
        assert_eq!(callees(&g, "go"), [("hook".to_string(), String::new()), ("impl".to_string(), "hook".to_string()), ("other".to_string(), "hook".to_string())]);
    }

    #[test]
    fn calls_through_a_local_go_where_its_values_come_from() {
        let g = graph(&[(
            "t.c",
            "struct ops { int (*run) (int); };
int a (int x) { return x; }
int b (int x) { return x; }
int c (int x) { return x; }
             void set (struct ops *o) { o->run = c; }
             int go (struct ops *o, int k)
{
  int (*f) (int) = k ? a : b;
  int (*h) (int);
  int r = f (1) + (*h) (2);
  h = o->run;
  return r;
}
",
        )]);
        // `f` holds a or b; `h` holds whatever is in the member, assigned later or not.
        assert_eq!(callees(&g, "go"), [("a".to_string(), String::new()), ("b".to_string(), String::new()), ("c".to_string(), "run".to_string())]);
        let through_pointer: Vec<&str> =
            crate::callers_through_macros(&g, g.find("a")[0]).iter().filter(|c| c.pointer).map(|c| g.sym_name(c.caller)).collect();
        assert_eq!(through_pointer, ["go"]);
    }

    #[test]
    fn a_table_whose_rows_and_type_come_through_macros_is_followed() {
        // The shape of a generated command table: a list that two files
        // include, each with its own idea of what a row is and of what
        // struct it is a row of.
        let g = expanded(&[
            ("cmd.h", "struct command { const char *name; int arity; void (*proc) (void); int flags; };
struct doc { const char *name; const char *summary; };
void get_cmd (void);
void set_cmd (void);
"),
            ("table.h", "struct ROW_STRUCT table[] = {
{ROW (\"get\", \"reads, one\", get_cmd, 2)},
{ROW (\"set\", \"writes\", set_cmd, 3), .flags = 1},
};
"),
            ("server.c", "#include \"cmd.h\"
void get_cmd (void) {}
void set_cmd (void) {}
#define ROW(name, summary, function, arity) name, arity, function
#define ROW_STRUCT command
#include \"table.h\"
void run (struct command *c) { c->proc (); }
"),
            ("help.c", "#include \"cmd.h\"
#define ROW(name, summary, function, arity) name, summary
#define ROW_STRUCT doc
#include \"table.h\"
"),
        ]);
        let proc = |f: &str| (f.to_string(), "proc".to_string());
        assert_eq!(callees(&g, "run"), [proc("get_cmd"), proc("set_cmd")]);
    }

    #[test]
    fn a_row_macro_that_fills_a_struct_inside_the_row_is_followed() {
        // The shape of a configuration table: every row a macro, the
        // functions in a struct that is a member of the row, by name.
        let g = expanded(&[(
            "config.c",
            "struct iface { void (*init) (void); int (*set) (int); int (*get) (void); };
struct data { int *addr; int (*check) (int); };
struct config { const char *name; struct iface interface; struct data data; };
void bool_init (void) {}
int bool_set (int v) { return v; }
int bool_get (void) { return 0; }
int port_ok (int v) { return v; }
int port;
#define COMMON(n) .name = (n),
#define IFACE(i, s, g) .interface = { .init = (i), .set = (s), .get = (g) },
#define BOOL(n, where, valid) { COMMON (n) IFACE (bool_init, bool_set, bool_get) .data = { .addr = &(where), .check = (valid) } }
struct config table[] = {
  BOOL (\"port\", port, port_ok),
  BOOL (\"other\", port, 0),
};
int apply (struct config *c, int v) { c->interface.init (); return c->interface.set (v) + c->data.check (v); }
",
        )]);
        let via = |f: &str, m: &str| (f.to_string(), m.to_string());
        assert_eq!(callees(&g, "apply"), [via("bool_init", "init"), via("bool_set", "set"), via("port_ok", "check")]);
    }

    #[test]
    fn rows_under_a_condition_in_a_long_table_are_rows_all_the_same() {
        // Long enough to be read a row at a time, with an `#if` across rows
        // and one inside a row.
        let mut table = String::from("struct command { const char *name; int arity; void (*proc) (void); };\n");
        let mut names = Vec::new();
        for i in 0..400 {
            table += &format!("void cmd_{i} (void) {{}}\n");
        }
        table += "#define ROW(n, f, a) n, a, f\nstruct command table[] = {\n";
        for i in 0..400 {
            if i == 100 {
                table += "#ifdef WITH_EXTRA\n";
            }
            if i == 110 {
                table += "#endif\n";
            }
            if i == 300 {
                table += "{ROW (\"split\",\n#if BIG\n cmd_300,\n#else\n cmd_301,\n#endif\n 1)},\n";
            } else if i != 301 {
                table += &format!("{{ROW (\"command number {i} of the table\", cmd_{i}, {i})}},\n");
            }
            names.push((format!("cmd_{i}"), "proc".to_string()));
        }
        table += "};\nvoid run (struct command *c) { c->proc (); }\n";
        let g = expanded(&[("t.c", &table)]);
        names.sort();
        assert_eq!(callees(&g, "run"), names);
    }

    #[test]
    fn members_of_the_same_name_in_different_structs_are_told_apart() {
        let g = graph(&[
            ("t.h", "struct reader { int (*run) (int); };
struct writer { int (*run) (int); };
typedef struct writer writer_t;
struct box { struct reader *rd; writer_t *wr; };
struct box *the_box (void);
"),
            (
                "u.c",
                "#include \"t.h\"
static int do_read (int x) { return x; }
static int do_write (int x) { return x; }
                 struct reader r = { do_read };
struct writer w = { .run = do_write };
                 int by_param (struct reader *p) { return p->run (1); }
                 int by_global (void) { return w.run (2); }
                 int by_cast (void *p) { return ((struct writer *) p)->run (3); }
                 int by_member (struct box *b) { return b->rd->run (4) + b->wr->run (5); }
                 int by_call (void) { return the_box ()->wr->run (6); }
                 int by_local (void) { writer_t *q = &w; int (*f) (int) = q->run; return f (7); }
                 void set (struct reader *p) { p->run = do_read; }
",
            ),
        ]);
        let run = |f: &str| (f.to_string(), "run".to_string());
        assert_eq!(callees(&g, "by_param"), [run("do_read")]);
        assert_eq!(callees(&g, "by_global"), [run("do_write")]);
        assert_eq!(callees(&g, "by_cast"), [run("do_write")]);
        assert_eq!(callees(&g, "by_member"), [run("do_read"), run("do_write")]);
        assert_eq!(callees(&g, "by_call"), [run("do_write"), ("the_box".to_string(), String::new())]);
        assert_eq!(callees(&g, "by_local"), [run("do_write")]);
        // Each function is stored in one of the two members, not in "run".
        for (f, line) in [("do_read", 1), ("do_write", 2)] {
            let slots: Vec<u32> = g.find(f).iter().flat_map(|&s| g.refs_to(s)).filter(|e| e.kind() == refkind::BIND).map(|e| g.syms[e.other as usize].line).collect();
            assert_eq!(slots, [line], "{f}");
        }
    }

    #[test]
    fn a_member_that_cannot_hold_a_function_is_no_slot() {
        let g = graph(&[
            ("t.h", "typedef int (*handler) (int);\ntypedef unsigned long word;\nstruct a { word f; };\nstruct b { handler f; };\n"),
            ("u.c", "#include \"t.h\"\nint h (int x) { return x; }\nvoid set (struct b *p) { p->f = h; }\nint go (struct b *p) { return p->f (1); }\n"),
        ]);
        assert_eq!(callees(&g, "go"), [("h".to_string(), "f".to_string())]);
        let slots: Vec<u32> = g.find("h").iter().flat_map(|&s| g.refs_to(s)).filter(|e| e.kind() == refkind::BIND).map(|e| g.syms[e.other as usize].line).collect();
        assert_eq!(slots, [4], "only the member of struct b");
    }

    #[test]
    fn a_static_function_is_visible_to_a_file_that_includes_it() {
        let g = graph(&[
            ("gas/a.c", "#include \"impl.c\"\nvoid f (void) { helper (); }\n"),
            ("gas/impl.c", "static void helper (void) { }\n"),
            ("ld/other.c", "void g (void) { helper (); }\n"),
        ]);
        assert_eq!(targets(&g, "f", "helper"), ["gas/impl.c"]);
        assert!(targets(&g, "g", "helper").is_empty());
    }

    #[test]
    fn a_call_in_a_function_a_macro_may_make_is_there_whenever_the_function_is() {
        let files = [
            ("trace.h", "#ifdef TRACING\n#define DECLARE(n) static inline void trace_##n (void) { probe (); }\n#else\n#define DECLARE(n)\n#endif\nDECLARE(open)\n"),
            ("probe.c", "void probe (void) { }\n"),
            ("a.c", "#include \"trace.h\"\nvoid f (void) { trace_open (); }\n"),
        ];
        let paths: Vec<std::path::PathBuf> = files.iter().map(|(p, _)| p.into()).collect();
        let dir = std::env::temp_dir().join(format!("si-link-made-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (p, src) in &files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), src).unwrap();
        }
        let full: Vec<std::path::PathBuf> = paths.iter().map(|p| dir.join(p)).collect();
        let sets = crate::prescan(&full);
        let facts = crate::extract_all(&dir, &full, &sets);
        let g = link(&facts).0;
        let calls: Vec<bool> = g.find("trace_open").iter().flat_map(|&s| g.refs_from(s)).filter(|e| g.sym_name(e.other) == "probe").map(|e| !e.ambiguous()).collect();
        assert_eq!(calls, [true]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The graph of these files as `norn index` makes it: prescanned, so
    /// that macros are expanded.
    fn indexed(files: &[(&str, &str)]) -> Graph {
        let dir = std::env::temp_dir().join(format!("si-link-indexed-{}-{}", std::process::id(), files[0].0.replace(['/', '.'], "_")));
        let _ = std::fs::remove_dir_all(&dir);
        for (p, src) in files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), src).unwrap();
        }
        let full: Vec<std::path::PathBuf> = files.iter().map(|(p, _)| dir.join(p)).collect();
        let sets = crate::prescan(&full);
        let g = link(&crate::extract_all(&dir, &full, &sets)).0;
        let _ = std::fs::remove_dir_all(&dir);
        g
    }

    #[test]
    fn a_macro_defined_further_down_is_not_there_yet() {
        // `mutex.h` makes `class_mutex_constructor`, then a macro of that
        // name that wraps it (the kernel's guards).
        let g = indexed(&[
            ("cleanup.h", "#define __GUARD(n, t) static inline int class_##n##_constructor (t *l) { return lock_it (l); }
#define GUARD(n, t) __GUARD(n, t)
"),
            ("mutex.h", "#include \"cleanup.h\"
int lock_it (int *l);
GUARD(mutex, int)
#define WITH(n, l) class_##n##_constructor (l), check ()
#define class_mutex_constructor(l) WITH(mutex, l)
"),
            ("a.c", "#include \"mutex.h\"
int check (void) { return 0; }
void f (int *m) { class_mutex_constructor (m); }
"),
        ]);
        let defs: Vec<bool> = g.find("class_mutex_constructor").iter().filter(|&&s| g.syms[s as usize].kind == kind::FUNCTION).map(|&s| g.syms[s as usize].flags & flag::DECL == 0).collect();
        assert_eq!(defs, [true]);
        // And the call through the wrapper reaches it, for certain.
        let calls: Vec<(String, bool)> = g
            .find("f")
            .iter()
            .flat_map(|&s| g.refs_from(s))
            .filter(|e| g.sym_name(e.other) == "class_mutex_constructor")
            .map(|e| (kind::name(g.syms[e.other as usize].kind).to_string(), !e.ambiguous()))
            .collect();
        assert!(calls.contains(&("function".to_string(), true)), "{calls:?}");
        assert!(calls.iter().all(|c| c.1), "{calls:?}");
    }

    #[test]
    fn a_folder_the_build_does_not_compile_renames_nothing_it_does() {
        // m68k's `#define clear_bit_unlock clear_bit`, seen by a header that
        // may read anything (it includes a header the build makes).
        let dir = std::env::temp_dir().join(format!("si-link-apart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = [
            ("arch/m68k/include/asm/bitops.h", "#define clear_bit_unlock clear_bit
"),
            ("include/lock.h", "#include \"generated/made.h\"
static inline void clear_bit_unlock (long n) { }
"),
            ("a.c", "#include \"include/lock.h\"
"),
        ];
        for (p, src) in &files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), src).unwrap();
        }
        let full: Vec<std::path::PathBuf> = files.iter().map(|(p, _)| dir.join(p)).collect();
        let defined = |not_built: Vec<String>| {
            let settings = crate::cond::Settings { not_built, ..Default::default() };
            let mut conditions = crate::cond::Conditions::from_settings(&settings, &dir);
            let sets = crate::prescan_under(&dir, &full, &mut conditions);
            let facts = crate::extract_all(&dir, &full, &sets);
            let g = link(&facts).0;
            ["clear_bit_unlock", "clear_bit"].map(|n| g.find(n).iter().any(|&s| g.syms[s as usize].kind == kind::FUNCTION))
        };
        // (Not told: a `clear_bit` is made of it beside.)
        assert!(defined(vec![])[1]);
        assert_eq!(defined(vec!["arch/m68k".into()]), [true, false]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_a_file_switches_off_before_including_it_renames_nothing_there() {
        // FreeRTOS's `tasks.c` defines `MPU_WRAPPERS_INCLUDED_FROM_API_FILE`
        // before including `mpu_wrappers.h` (and undefines it after): its
        // `xTaskCreate` is `xTaskCreate`, not `MPU_xTaskCreate`.
        let dir = std::env::temp_dir().join(format!("si-link-api-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = [
            ("w.h", "#ifndef W_H
#define W_H
#ifndef FROM_API
#define make_task MPU_make_task
#endif
int make_task (int);
#endif
"),
            ("tasks.c", "#define FROM_API
#include \"w.h\"
#undef FROM_API
int make_task (int x) { return x; }
"),
            ("app.c", "#include \"w.h\"
int main (void) { return make_task (1); }
"),
        ];
        for (p, src) in &files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), src).unwrap();
        }
        let full: Vec<std::path::PathBuf> = files.iter().map(|(p, _)| dir.join(p)).collect();
        let mut conditions = crate::conditions_of(&dir, &full);
        let sets = crate::prescan_under(&dir, &full, &mut conditions);
        let facts = crate::extract_all(&dir, &full, &sets);
        let g = link_with(&facts, &Options { readings: crate::readings_of(&conditions), ..Default::default() }).0;
        let _ = std::fs::remove_dir_all(&dir);
        let defined = |n: &str| g.find(n).iter().any(|&s| g.syms[s as usize].kind == kind::FUNCTION && g.syms[s as usize].flags & flag::DECL == 0);
        assert_eq!([defined("make_task"), defined("MPU_make_task")], [true, false]);
    }

    #[test]
    fn a_pragma_is_no_call() {
        // A parse in a macro's body took `_Pragma ("...")` for a function
        // (the kernel's HID BPF programs); its uses are no calls of it.
        let g = indexed(&[
            ("p.c", "int _Pragma (const char *s) { return 0; }
"),
            ("a.c", "void f (void) { _Pragma (\"GCC diagnostic push\"); }
"),
        ]);
        let called: Vec<&str> = g.find("f").iter().flat_map(|&s| g.refs_from(s)).map(|e| g.sym_name(e.other)).collect();
        assert!(!called.contains(&"_Pragma"), "{called:?}");
    }

    #[test]
    fn every_port_s_definition_is_a_reading() {
        // FreeRTOS: `vPortEnterCritical` in each of 86 ports; the one the
        // build compiles is among them, whichever it is.
        let ports: Vec<(String, String)> = (0..40).map(|i| (format!("port{i:02}/port.c"), "void enter (void) { }
".to_string())).collect();
        let mut files: Vec<(&str, &str)> = ports.iter().map(|(p, s)| (p.as_str(), s.as_str())).collect();
        files.insert(0, ("tasks.c", "void enter (void);
void f (void) { enter (); }
"));
        let g = indexed(&files);
        let called = g.find("f").iter().flat_map(|&s| g.refs_from(s)).filter(|e| g.sym_name(e.other) == "enter").count();
        assert_eq!(called, 40);
    }

    #[test]
    fn a_macro_that_does_without_the_function_of_its_name_is_another_reading() {
        // libiberty's `htab_size`: the macro reads the member; the function,
        // defined as `(htab_size)`, is for those who take its address.
        let g = indexed(&[(
            "hashtab.c",
            "struct t { int size; };
#define htab_size(h) ((h)->size)
int (htab_size) (struct t *h) { return htab_size (h); }
int f (struct t *h) { return htab_size (h); }
",
        )]);
        let sure: Vec<bool> = g.find("f").iter().flat_map(|&s| g.refs_from(s)).filter(|e| g.sym_name(e.other) == "htab_size").map(|e| !e.ambiguous()).collect();
        assert!(!sure.is_empty() && sure.iter().all(|s| !s), "{sure:?}");
    }

    #[test]
    fn a_macro_of_another_source_file_is_not_seen() {
        let g = graph(&[
            ("include/slab.h", "void kfree (void *p);\n"),
            ("mm/slub.c", "#include \"../include/slab.h\"\nvoid kfree (void *p) { }\n"),
            ("lib/unxz.c", "#ifdef STATIC\n#define kfree(p) free (p)\n#endif\nvoid h (void *p) { kfree (p); }\n"),
            ("kernel/a.c", "#include \"../include/slab.h\"\nvoid f (void *p) { kfree (p); }\n"),
        ]);
        assert_eq!(targets(&g, "f", "kfree"), ["mm/slub.c"]);
    }

    #[test]
    fn a_folder_the_build_does_not_compile_gives_way() {
        let files = [
            ("arch/x86/include/asm/bitops.h", "static inline int set_bit (int n) { return n; }\n"),
            ("arch/arm64/include/asm/bitops.h", "static inline int set_bit (int n) { return n; }\n"),
            ("tools/lib/slab.c", "void kfree (void *p) { }\n"),
            ("mm/slab.c", "void kfree (void *p) { }\n"),
            ("kernel/a.c", "#include <asm/bitops.h>\nvoid f (void *p) { set_bit (1); kfree (p); }\n"),
            ("arch/arm64/kernel/b.c", "#include <asm/bitops.h>\nvoid g (void) { set_bit (2); }\n"),
        ];
        let mut ex = Extractor::new();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &MacroSets::default())).collect();
        let sure = |g: &Graph, caller: &str, name: &str| -> Vec<(String, bool)> {
            g.find(caller)
                .iter()
                .flat_map(|&s| g.refs_from(s))
                .filter(|e| g.sym_name(e.other) == name)
                .map(|e| (g.file_path(g.syms[e.other as usize].file).to_string(), !e.ambiguous()))
                .collect()
        };
        // Not told: either may be meant.
        let g = link(&facts).0;
        assert_eq!(sure(&g, "f", "kfree").len(), 2);
        // Told: what the build compiles, for certain; another
        // architecture's own code still reads its own header.
        let options = Options { not_built: vec!["arch/arm64/".into(), "tools/".into()], ..Options::default() };
        let g = link_with(&facts, &options).0;
        assert_eq!(sure(&g, "f", "kfree"), [("mm/slab.c".to_string(), true)]);
        assert_eq!(sure(&g, "f", "set_bit"), [("arch/x86/include/asm/bitops.h".to_string(), true)]);
        assert_eq!(sure(&g, "g", "set_bit"), [("arch/arm64/include/asm/bitops.h".to_string(), true)]);
    }

    #[test]
    fn an_included_header_decides_between_same_named_macros() {
        let g = graph(&[
            ("a/conf.h", "#define SIZE 4\n"),
            ("b/conf.h", "#define SIZE 8\n"),
            ("a/x.c", "#include \"conf.h\"\nint f (void) { return SIZE; }\n"),
            ("c/y.c", "int g (void) { return SIZE; }\n"),
        ]);
        assert_eq!(targets(&g, "f", "SIZE"), ["a/conf.h"]);
        assert_eq!(targets(&g, "g", "SIZE"), ["a/conf.h", "b/conf.h"]);
    }

    #[test]
    fn a_prototype_in_a_header_leads_to_the_definition() {
        let g = graph(&[
            ("inc/api.h", "extern int api (int);\n"),
            ("lib/api.c", "#include \"api.h\"\nint api (int x) { return x; }\n"),
            ("app/main.c", "#include \"../inc/api.h\"\nint main (void) { return api (1); }\n"),
        ]);
        assert_eq!(targets(&g, "main", "api"), ["lib/api.c"]);
    }

    #[test]
    fn macro_bodies_and_macro_blocks_are_indexed() {
        let g = graph(&[(
            "m.c",
            "#define CHECK(x) validate (x)\n#define EACH(i) for (i = 0; i < 3; i++)\n\
             int validate (int);\nint last (int);\n\
             int f (int a)\n{\n  int i;\n  EACH (i)\n    {\n      CHECK (a);\n    }\n  return last (a);\n}\n",
        )]);
        assert_eq!(targets(&g, "CHECK", "validate"), ["m.c"]);
        // `EACH (i) { ... }` must not end `f` early or become a function itself.
        assert_eq!(targets(&g, "f", "last"), ["m.c"]);
        assert_eq!(targets(&g, "f", "EACH"), ["m.c"]);
        assert!(g.find("EACH").iter().all(|&s| g.syms[s as usize].kind == kind::MACRO));
        let through: Vec<&str> =
            crate::callers_through_macros(&g, g.find("validate")[0]).iter().map(|c| g.sym_name(c.caller)).collect();
        assert!(through.contains(&"CHECK") && through.contains(&"f"), "{through:?}");
    }
}

#[cfg(test)]
mod cpp_tests {
    use super::*;
    use crate::extract::Extractor;
    use crate::mask::MacroSets;

    fn graph(files: &[(&str, &str)]) -> Graph {
        let mut ex = Extractor::new();
        let t = MacroSets::default();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &t)).collect();
        link(&facts).0
    }
    #[test]
    fn a_member_call_in_a_macro_body_of_an_object_it_declares_is_of_its_class() {
        let g = graph(&[
            ("g.h", r#"namespace testing {
class AssertionResult { public: const char* failure_message() const { return ""; } };
class Other { public: const char* failure_message() const { return ""; } };
}
#define GTEST_ASSERT_(expression, on_failure)   if (const ::testing::AssertionResult gtest_ar = (expression))     ;   else     on_failure(gtest_ar.failure_message())
"#),
        ]);
        let called: Vec<(String, bool)> = g
            .find("GTEST_ASSERT_")
            .iter()
            .flat_map(|&m| g.refs_from(m))
            .filter(|e| e.kind() == refkind::CALL)
            .map(|e| (crate::graph::Qualifier::new(&g).name(&g, e.other).into_owned(), e.ambiguous()))
            .collect();
        assert_eq!(called, [("AssertionResult::failure_message".to_string(), false)]);
    }

    /// (file of the callee, certain) of what `caller` calls under `name`.
    fn calls(g: &Graph, caller: &str, name: &str) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = g
            .find(caller)
            .iter()
            .flat_map(|&s| g.refs_from(s))
            .filter(|e| g.sym_name(e.other) == name && e.kind() == refkind::CALL)
            .map(|e| (g.file_path(g.syms[e.other as usize].file).to_string(), !e.ambiguous()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn a_call_qualified_with_a_namespace_is_one_of_that_namespace() {
        // The namespaces closed in column 0, as is usual (a brace there
        // resynchronizes C, and must not end the outer namespace here).
        let g = graph(&[
            ("crc.h", "namespace leveldb {\nnamespace crc32c {\ninline unsigned Value(int n) {\n  return n;\n}\n}  // namespace crc32c\n}  // namespace leveldb\n"),
            ("other.h", "namespace other {\ninline unsigned Value(int n) { return n; }\n}\n"),
            ("use.cc", "#include \"crc.h\"\n#include \"other.h\"\nnamespace leveldb {\nunsigned Fix() { return crc32c::Value(1); }\n}\n"),
        ]);
        assert_eq!(calls(&g, "Fix", "Value"), [("crc.h".to_string(), true)]);
    }

    #[test]
    fn a_scope_the_corpus_does_not_have_is_no_class_of_it() {
        let g = graph(&[
            ("r.h", "class Rand {\n public:\n  int max() { return 1; }\n};\n"),
            ("u.cc", "#include \"r.h\"\nint f(int a, int b) { return std::max(a, b); }\n"),
        ]);
        assert_eq!(calls(&g, "f", "max"), []);
    }

    #[test]
    fn a_method_is_found_by_the_type_of_the_object_and_its_overrides_too() {
        let g = graph(&[
            ("it.h", "class Iterator {\n public:\n  virtual void Next() = 0;\n};\nclass Fast : public Iterator {\n public:\n  void Next() override;\n};\nclass Other {\n public:\n  void Next() {}\n};\n"),
            ("it.cc", "#include \"it.h\"\nvoid Fast::Next() {}\n"),
            ("db.cc", "#include \"it.h\"\nclass DB {\n  Iterator* iter_;\n  void Step();\n};\nvoid DB::Step() { iter_->Next(); }\n"),
        ]);
        // `Iterator::Next` has no body: the override is what may run, and
        // `Other::Next`, of another class, is not.
        assert_eq!(calls(&g, "Step", "Next"), [("it.cc".to_string(), false), ("it.h".to_string(), true)]);
    }

    #[test]
    fn a_call_in_a_method_is_of_its_class_before_a_function_of_the_same_name() {
        let g = graph(&[
            ("a.h", "int Size();\nclass Box {\n public:\n  int Size() const { return 1; }\n  int Twice() const { return 2 * Size(); }\n};\n"),
            ("a.cc", "#include \"a.h\"\nint Size() { return 0; }\nint Free() { return Size(); }\n"),
        ]);
        assert_eq!(calls(&g, "Twice", "Size"), [("a.h".to_string(), true)]);
        assert_eq!(calls(&g, "Free", "Size"), [("a.cc".to_string(), true)]);
    }

    #[test]
    fn a_namespace_alias_is_its_namespace_where_no_namespace_has_its_name() {
        let g = graph(&[
            ("tf.h", "namespace tinyformat {
inline int format(int x) { return x; }
}
namespace tfm = tinyformat;
"),
            ("fs.h", "namespace fs {
inline int format(int x) { return x; }
}
"),
            ("t.cc", "#include \"tf.h\"
#include \"fs.h\"
namespace fs = std::filesystem;
int a() { return tfm::format(1); }
int b() { return fs::format(2); }
"),
        ]);
        assert_eq!(calls(&g, "a", "format"), [("tf.h".to_string(), true)]);
        // `fs` is opened as a namespace: the alias in one file does not
        // make every `fs::` another.
        assert_eq!(calls(&g, "b", "format"), [("fs.h".to_string(), true)]);
    }

    #[test]
    fn a_call_qualified_in_a_macro_body_is_resolved_as_written() {
        let g = graph(&[
            ("h.h", "namespace testing {
struct EqHelper { static bool Compare(int a, int b) { return a == b; } };
struct Other { static bool Compare(int a, int b) { return a < b; } };
}
#define CHECK_EQ(a, b) ::testing::EqHelper::Compare(a, b)
"),
            ("t.cc", "#include \"h.h\"
bool t() { return CHECK_EQ(1, 2); }
"),
        ]);
        let compare: Vec<String> = g.find("CHECK_EQ").iter().flat_map(|&m| g.refs_from(m)).filter(|e| e.kind() == refkind::CALL && !e.ambiguous()).map(|e| g.sym_name(e.other).to_string()).collect();
        assert_eq!(compare, ["Compare"]);
    }

    #[test]
    fn an_auto_variable_is_of_what_it_is_made_from() {
        let g = graph(&[
            ("k.h", "class Key {
 public:
  int Size() const { return 1; }
};
class Box {
 public:
  int Size() const { return 2; }
  Key Get() const { return Key(); }
};
"),
            ("k.cc", "#include \"k.h\"
int a() { auto k = std::make_unique<Key>(); return k->Size(); }
int b(const Box& box) { auto key = box.Get(); return key.Size(); }
int c() { auto* p = new Box(); return p->Size(); }
"),
        ]);
        assert_eq!(calls(&g, "a", "Size"), [("k.h".to_string(), true)]);
        let class_of = |caller: &str| -> Vec<String> {
            let q = crate::graph::Qualifier::new(&g);
            g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| g.sym_name(e.other) == "Size" && !e.ambiguous()).map(|e| q.name(&g, e.other).into_owned()).collect()
        };
        assert_eq!(class_of("a"), ["Key::Size"]);
        assert_eq!(class_of("b"), ["Key::Size"]);
        assert_eq!(class_of("c"), ["Box::Size"]);
    }

    #[test]
    fn an_element_of_a_container_is_of_its_elements_type() {
        let g = graph(&[
            ("b.h", "#include <vector>
class Tx {
 public:
  int Hash() const { return 1; }
};
class Peer {
 public:
  int Hash() const { return 2; }
};
typedef std::shared_ptr<const Tx> TxRef;
class Block {
 public:
  std::vector<TxRef> vtx;
};
"),
            ("b.cc", "#include \"b.h\"
int a(const Block& b) { return b.vtx[0]->Hash(); }
int c(const Block& b) { return b.vtx.back()->Hash(); }
int d(const Block& b) { int n = 0; for (const auto& tx : b.vtx) n += tx->Hash(); return n; }
int e(const std::vector<Peer>& peers) { int n = 0; for (const auto& p : peers) n += p.Hash(); return n; }
"),
        ]);
        let q = crate::graph::Qualifier::new(&g);
        let certain = |caller: &str| -> Vec<String> {
            g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| g.sym_name(e.other) == "Hash" && !e.ambiguous()).map(|e| q.name(&g, e.other).into_owned()).collect()
        };
        for f in ["a", "c", "d"] {
            assert_eq!(certain(f), ["Tx::Hash"], "{f}");
        }
        assert_eq!(certain("e"), ["Peer::Hash"]);
    }

    #[test]
    fn an_object_made_is_a_call_of_a_constructor_of_its_class() {
        let g = graph(&[
            ("c.h", "class Bar {
 public:
  Bar(int x) {}
};
class Base {
 public:
  Base(int x) {}
};
class Foo : public Base {
 public:
  Foo(int x);
 private:
  Bar m_bar;
};
using Alias = Bar;
struct Plain { int x; };
#define HOLD(v) Bar held_##v(v)
"),
            ("c.cc", "#include \"c.h\"
Foo::Foo(int x) : Base(x), m_bar(x) {}
void a() { Foo f(1); }
void b() { Foo* p = new Foo(2); }
void c() { Alias(3); }
void d() { Plain p; Foo* q; Foo& r = *q; }
void e() { HOLD(4); }
"),
        ]);
        let q = crate::graph::Qualifier::new(&g);
        let made = |caller: &str| -> Vec<String> {
            let mut v: Vec<String> = g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| e.kind() == refkind::CALL).map(|e| q.name(&g, e.other).into_owned()).collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(made("a"), ["Foo::Foo"]);
        assert_eq!(made("b"), ["Foo::Foo"]);
        assert_eq!(made("c"), ["Bar::Bar"]);
        // A struct with no constructor, a pointer and a reference make none.
        assert!(made("d").is_empty(), "{:?}", made("d"));
        // The constructor's initializers: its base, and its member's type.
        let init: Vec<String> = g.find("Foo").iter().filter(|&&s| g.syms[s as usize].kind == kind::FUNCTION && g.syms[s as usize].flags & flag::DECL == 0)
            .flat_map(|&s| g.refs_from(s)).filter(|e| e.kind() == refkind::CALL).map(|e| q.name(&g, e.other).into_owned()).collect();
        let mut init = init;
        init.sort();
        assert_eq!(init, ["Bar::Bar", "Base::Base"]);
        // A macro whose text declares an object of the class.
        let held: Vec<String> = g.find("HOLD").iter().flat_map(|&m| g.refs_from(m)).filter(|e| e.kind() == refkind::CALL).map(|e| q.name(&g, e.other).into_owned()).collect();
        assert_eq!(held, ["Bar::Bar"]);
    }

    #[test]
    fn a_macro_body_makes_objects_and_assigns_to_them() {
        let g = graph(&[
            ("t.h", "namespace testing {
class Message {
 public:
  Message();
};
class AssertHelper {
 public:
  AssertHelper(int type, const char* message);
  void operator=(const Message& message) const;
};
class AssertionResult {
 public:
  AssertionResult(bool ok) {}
  template <typename T> AssertionResult& operator<<(const T& value) { return *this; }
};
}
struct SourceLocation {
  SourceLocation(const char* func) {}
};
inline void Log(SourceLocation&& loc, int x) {}
#define FAIL_AT(t) ::testing::AssertHelper(t, \"m\") \\
    = ::testing::Message()
#define LOG(x) Log(SourceLocation{__func__}, x)
#define CHECK_MSG(x) if (::testing::AssertionResult ar = ::testing::AssertionResult(x)) ; else ar << \"failed\"
"),
            ("t.cc", "#include \"t.h\"
namespace testing {
Message::Message() {}
AssertHelper::AssertHelper(int type, const char* message) {}
void AssertHelper::operator=(const Message& message) const {}
}
void f() { FAIL_AT(1); }
void g() { LOG(2); }
void h() { CHECK_MSG(true); }
"),
        ]);
        let q = crate::graph::Qualifier::new(&g);
        let callers = |callee: &str| -> Vec<String> {
            let mut v: Vec<String> = g.syms.iter().enumerate()
                .filter(|(i, s)| s.kind == kind::FUNCTION && s.flags & flag::DECL == 0 && q.name(&g, *i as u32) == callee)
                .flat_map(|(i, _)| crate::callers_through_macros(&g, i as u32))
                .filter(|c| g.syms[c.caller as usize].kind == kind::FUNCTION && !c.ambiguous)
                .map(|c| g.sym_name(c.caller).to_string())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(callers("AssertHelper::AssertHelper"), ["f"]);
        assert_eq!(callers("AssertHelper::operator="), ["f"]);
        assert_eq!(callers("Message::Message"), ["f"]);
        assert_eq!(callers("SourceLocation::SourceLocation"), ["g"]);
        // `ar << ...` of an `ar` the body declares: its class's.
        assert_eq!(callers("AssertionResult::operator<<"), ["h"]);
    }

    #[test]
    fn of_overloads_in_several_files_the_call_is_of_those_taking_its_arguments() {
        let g = graph(&[
            ("m.h", "class Message {
 public:
  Message();
  Message(const Message& m) {}
  void Put(int a, int b = 0);
  void Put(const char* s, int a, int b) {}
};
#define MAKE() Message()
inline void Use(int a) {}
"),
            ("m.cc", "#include \"m.h\"
Message::Message() {}
void Message::Put(int a, int b) {}
void f() { Message m; Message n(m); Message* p = new Message(); MAKE(); }
void g(Message& m) { m.Put(1); m.Put(\"x\", 1, 2); }
"),
        ]);
        let q = crate::graph::Qualifier::new(&g);
        let certain = |caller: &str, callee: &str| -> Vec<String> {
            let mut v: Vec<String> = g.find(caller).iter().chain(g.find("MAKE").iter()).flat_map(|&s| g.refs_from(s))
                .filter(|e| e.kind() == refkind::CALL && !e.ambiguous() && q.name(&g, e.other) == callee)
                .map(|e| format!("{}:{}", g.file_path(g.syms[e.other as usize].file), g.syms[e.other as usize].line)).collect();
            v.sort();
            v.dedup();
            v
        };
        // `Message m;`, `new Message ()` and the macro's `Message ()` take
        // none; `Message n (m)` one.
        assert_eq!(certain("f", "Message::Message"), ["m.cc:2", "m.h:4"]);
        // The default argument is in the declaration, not the definition.
        assert_eq!(certain("g", "Message::Put"), ["m.cc:3", "m.h:6"]);
    }

    #[test]
    fn an_object_returned_by_value_is_copied_by_the_constructor_its_class_has() {
        let g = graph(&[(
            "s.cc",
            "class Status {
 public:
  Status() {}
  Status(const Status& s) {}
  explicit Status(int code) {}
};
class Plain {
 public:
  Plain(int x) {}
};
Status f() { Status s; return s; }
Plain g(Plain p) { return p; }
Status* h(Status* p) { return p; }
",
        )]);
        let made = |caller: &str| -> Vec<u32> {
            let mut v: Vec<u32> = g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| e.kind() == refkind::CALL).map(|e| g.syms[e.other as usize].line).collect();
            v.sort();
            v
        };
        // `Status s;` by the one taking none, `return s;` by the copy.
        assert_eq!(made("f"), [3, 4]);
        // A class with no copy written is copied by the compiler's; a
        // pointer is no object.
        assert!(made("g").is_empty(), "{:?}", made("g"));
        assert!(made("h").is_empty(), "{:?}", made("h"));
    }

    #[test]
    fn an_argument_passed_as_another_class_is_made_into_one() {
        let g = graph(&[
            ("s.h", "#include <string>
class Slice {
 public:
  Slice(const char* s) {}
  Slice(const std::string& s) {}
};
class Key {
 public:
  explicit Key(const char* s) {}
};
class Status {
 public:
  Status(const Slice& msg) {}
};
void Put(const Slice& key, int n) {}
void Name(const char* s) {}
void Name(const Slice& s) {}
void Lookup(const Key& k) {}
namespace tinyformat {
template <unsigned n>
struct FormatStringCheck {
  FormatStringCheck(const char* str) {}
};
template <typename... Args>
void format(FormatStringCheck<sizeof...(Args)> fmt, const Args&... args) {}
}
namespace tfm = tinyformat;
#define strprintf tfm::format
inline void LogPrint(int level, tinyformat::FormatStringCheck<1> fmt, int x) {}
#define LogLevel_(level, ...) LogPrint(level, __VA_ARGS__)
#define LogInfo(...) LogLevel_(1, __VA_ARGS__)
"),
            ("o.h", "class Other {
 public:
  Other(const char* s) {}
};
void Elsewhere(const Other& o) {}
"),
            ("s.cc", "#include \"s.h\"
void a() { Put(\"k\", 1); }
void a2() { Slice s(\"x\"); Status t(s); }
void b() { Name(\"n\"); Lookup(Key(\"k\")); }
void c(int x) { strprintf(\"%d\", x); }
void c2(int x) { LogInfo(\"%d\", x); }
void d() { Elsewhere(\"e\"); }
void e(const std::string& k) { std::string s; Put(k, 1); Put(s, 2); }
"),
        ]);
        let q = crate::graph::Qualifier::new(&g);
        let made = |caller: &str| -> Vec<String> {
            let mut v: Vec<String> = g.find(caller).iter().flat_map(|&s| g.refs_from(s))
                .filter(|e| e.kind() == refkind::CALL && !e.ambiguous() && g.syms[e.other as usize].line < 25 && q.name(&g, e.other).contains("::"))
                .map(|e| q.name(&g, e.other).into_owned()).collect();
            v.sort();
            v.dedup();
            v
        };
        // `Put ("k", 1)` makes a Slice of the string; `Status t (s)` takes
        // the Slice it is given, made by the constructor of Status.
        assert_eq!(made("a"), ["Slice::Slice"]);
        assert_eq!(made("a2"), ["Slice::Slice", "Status::Status"]);
        // A string is what `Name (const char*)` takes; an explicit
        // constructor converts nothing (`Key ("k")` is made, not converted).
        assert_eq!(made("b"), ["Key::Key"]);
        // Through a name for a function, and through macros passing on
        // what they are given.
        for f in ["c", "c2"] {
            assert_eq!(made(f), ["FormatStringCheck::FormatStringCheck"], "{f}");
        }
        // Of a function no header read here declares, nothing.
        assert!(made("d").is_empty(), "{:?}", made("d"));
        // A `std::string` passed as a Slice is made into one too.
        assert_eq!(made("e"), ["Slice::Slice"]);
    }

    #[test]
    fn what_operator_subscript_or_a_function_hiding_a_class_returns_is_typed() {
        let g = graph(&[(
            "u.cc",
            "class UniValue {
 public:
  const UniValue& operator[](unsigned i) const;
  const char* get_str() const { return \"\"; }
};
class Slice {
 public:
  char operator[](unsigned i) const { return 0; }
  int size() const { return 0; }
};
struct Other { const char* get_str() const { return \"\"; } int size() const { return 1; } };
namespace Consensus { struct Params { int Height() const { return 0; } }; }
class CChainParams { public: int Height() const { return 1; } };
const CChainParams& Params();
const UniValue& UniValue::operator[](unsigned i) const { return *this; }
void f(const Slice& s) { UniValue val; val[0].get_str(); s[0].size(); }
int g() { return Params().Height(); }
",
        )]);
        let q = crate::graph::Qualifier::new(&g);
        let certain = |caller: &str| -> Vec<String> {
            let mut v: Vec<String> = g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| e.kind() == refkind::CALL && !e.ambiguous()).map(|e| q.name(&g, e.other).into_owned()).collect();
            v.sort();
            v
        };
        // `val[0]` is what UniValue's `operator[]` returns; `s[0]`, a char,
        // is of no class.
        assert_eq!(certain("f"), ["Slice::operator[]", "UniValue::get_str", "UniValue::operator[]"]);
        // `Params ()` calls the function, not a class of that name.
        assert_eq!(certain("g"), ["CChainParams::Height", "Params"]);
    }

    #[test]
    fn an_object_the_parser_takes_for_a_function_is_made_and_typed() {
        let g = graph(&[(
            "u.cc",
            "class UniValue {
 public:
  enum VType { VNULL, VOBJ };
  UniValue(VType t) {}
  void pushKV(int k) {}
};
struct Other { void pushKV(int k) {} };
void f() { UniValue result(UniValue::VOBJ); result.pushKV(1); }
void g(UniValue::VType n) { UniValue r(n); r.pushKV(2); }
void k(const UniValue::VType* types) { UniValue e(types[0]); e.pushKV(3); }
void h() { UniValue proto(); UniValue other(int x); }
",
        )]);
        let q = crate::graph::Qualifier::new(&g);
        let certain = |caller: &str| -> Vec<String> {
            let mut v: Vec<String> = g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| e.kind() == refkind::CALL && !e.ambiguous()).map(|e| q.name(&g, e.other).into_owned()).collect();
            v.sort();
            v
        };
        for f in ["f", "g", "k"] {
            assert_eq!(certain(f), ["UniValue::UniValue", "UniValue::pushKV"], "{f}");
        }
        // With no arguments, or a parameter named, it declares a function.
        assert!(certain("h").is_empty(), "{:?}", certain("h"));
    }

    #[test]
    fn a_macro_only_some_configurations_define_is_what_cpp_calls_by_its_name() {
        let h = "#if !DONT_DEFINE_CHECK
#define CHECK_IT(x) Check(x)
#endif
#ifdef USE_TWO
#define TWO(x) Check(x)
#else
#define TWO(x) Other(x)
#endif
inline void Check(int x) {}
inline void Other(int x) {}
";
        let src = "#include \"t.h\"\nvoid f() { CHECK_IT(1); }\nvoid g() { TWO(2); }\n";
        // (What C calls by a name it has no declaration of may be another
        // function: there the macro stays one reading.)
        for (ext, sure) in [("cc", true), ("c", false)] {
            let path = format!("t.{ext}");
            let g = graph(&[("t.h", h), (&path, src)]);
            let callers = |callee: &str| -> Vec<(String, bool)> {
                crate::callers_through_macros(&g, g.find(callee)[0]).iter().filter(|c| g.syms[c.caller as usize].kind == kind::FUNCTION).map(|c| (g.sym_name(c.caller).to_string(), c.ambiguous)).collect()
            };
            assert!(callers("Check").contains(&("f".to_string(), !sure)), "{ext}: {:?}", callers("Check"));
            // Two definitions of the name are two readings.
            assert!(callers("Other").contains(&("g".to_string(), true)), "{ext}: {:?}", callers("Other"));
        }
    }

    #[test]
    fn an_operator_on_objects_is_a_call_of_the_one_their_types_take() {
        let h = r#"class Writer {
 public:
  Writer& operator<<(int v);
};
class Key {
 public:
  Key& operator=(const Key& k);
  int x;
};
class Plain { public: int y; };
class Other { public: int z; };
bool operator==(const Key& a, const Key& b);
bool operator==(const Other& a, const Other& b);
int operator""_k(unsigned long long v);
"#;
        let cc = r#"#include "o.h"
Writer& Writer::operator<<(int v) { return *this; }
Key& Key::operator=(const Key& k) { return *this; }
bool operator==(const Key& a, const Key& b) { return a.x == b.x; }
bool operator==(const Other& a, const Other& b) { return true; }
int operator""_k(unsigned long long v) { return 1; }
void w(Writer& out) { out << 1; }
void eq(const Key& a, const Key& b) { if (a == b) {} }
void assign(Key& a, const Key& b) { a = b; }
void plain(Plain& a, const Plain& b) { a = b; }
void deref(Key* p) { Key k = *p; }
int lit() { return 3_k; }
"#;
        let g = graph(&[("o.h", h), ("o.cc", cc)]);
        let q = crate::graph::Qualifier::new(&g);
        let calls_of = |caller: &str| -> Vec<(String, bool, u32)> {
            let mut v: Vec<(String, bool, u32)> = g
                .find(caller)
                .iter()
                .flat_map(|&s| g.refs_from(s))
                .filter(|e| e.kind() == refkind::CALL && g.sym_name(e.other).starts_with("operator"))
                .map(|e| (q.name(&g, e.other).into_owned(), !e.ambiguous(), g.syms[e.other as usize].line))
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(calls_of("w"), [("Writer::operator<<".to_string(), true, 2)]);
        // Of two `operator==`, the one taking Keys.
        assert_eq!(calls_of("eq"), [("operator==".to_string(), true, 4)]);
        // The copy written is called; a class's the compiler makes is none.
        assert_eq!(calls_of("assign"), [("Key::operator=".to_string(), true, 3)]);
        assert_eq!(calls_of("plain"), []);
        // `*p` of a pointer is no call.
        assert_eq!(calls_of("deref"), []);
        assert_eq!(calls_of("lit"), [(r#"operator""_k"#.to_string(), true, 6)]);
    }

    #[test]
    fn a_structured_binding_is_of_its_part_of_what_it_binds() {
        let h = r#"#include <map>
#include <vector>
class Net { public: int Id() const { return 1; } };
class Count { public: int Value() const { return 2; } };
struct Entry { Net net; Count count; };
class Book {
 public:
  std::map<Net, Count> m_counts;
  std::vector<Entry> m_entries;
};
Entry Make();
"#;
        let cc = r#"#include "b.h"
int a(const Book& b) { int n = 0; for (const auto& [net, count] : b.m_counts) n += net.Id() + count.Value(); return n; }
int c(const std::map<Net, Count>& m) { int n = 0; for (const auto& [k, v] : m) n += k.Id() + v.Value(); return n; }
int d(const Book& b) { int n = 0; for (const auto& [x, y] : b.m_entries) n += x.Id() + y.Value(); return n; }
int e() { auto [p, q] = Make(); return p.Id() + q.Value(); }
"#;
        let g = graph(&[("b.h", h), ("b.cc", cc)]);
        let q = crate::graph::Qualifier::new(&g);
        let certain = |caller: &str| -> Vec<String> {
            let mut v: Vec<String> = g
                .find(caller)
                .iter()
                .flat_map(|&s| g.refs_from(s))
                .filter(|e| e.kind() == refkind::CALL && !e.ambiguous() && matches!(g.sym_name(e.other), "Id" | "Value"))
                .map(|e| q.name(&g, e.other).into_owned())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        for f in ["a", "c", "d", "e"] {
            assert_eq!(certain(f), ["Count::Value", "Net::Id"], "{f}");
        }
    }

    #[test]
    fn a_c_file_is_linked_as_before() {
        // No class, no namespace: what C has is unchanged.
        let g = graph(&[("a.h", "struct ops { int (*run) (int); };\nint helper (int x);\n"), ("a.c", "#include \"a.h\"\nint helper (int x) { return x; }\nint go (struct ops *o) { return helper (o->run (1)); }\n")]);
        assert!(g.owner.is_empty());
        assert_eq!(calls(&g, "go", "helper"), [("a.c".to_string(), true)]);
    }
}

#[cfg(test)]
mod test_framework_tests {
    use super::*;
    use crate::extract::Extractor;
    use crate::mask::MacroSets;

    fn named(files: &[(&str, &str)]) -> Vec<String> {
        let mut ex = Extractor::new();
        let t = MacroSets::default();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &t)).collect();
        let g = link(&facts).0;
        let q = crate::graph::Qualifier::new(&g);
        let mut out: Vec<String> = (0..g.syms.len() as u32).filter(|&s| g.syms[s as usize].kind == kind::FUNCTION).map(|s| q.name(&g, s).into_owned()).collect();
        out.sort();
        out
    }

    #[test]
    fn a_boost_test_is_its_case_s_test_method_of_its_suite_s_fixture() {
        let src = "struct Setup { int Count() { return 1; } };\n\
                   BOOST_FIXTURE_TEST_SUITE(addrman_tests, Setup)\n\
                   BOOST_AUTO_TEST_CASE(simple)\n{\n  Count();\n}\n\
                   BOOST_AUTO_TEST_SUITE_END()\n\
                   BOOST_FIXTURE_TEST_CASE(other, Setup)\n{\n  Count();\n}\n\
                   BOOST_AUTO_TEST_CASE(plain)\n{\n}\n";
        assert_eq!(
            named(&[("t.cpp", src)]),
            ["Setup::Count", "other::test_method", "other_invoker", "plain::test_method", "plain_invoker", "simple::test_method", "simple_invoker"]
        );
        // What the fixture has, the test calls.
        let mut ex = Extractor::new();
        let g = link(&[ex.extract("t.cpp", src.as_bytes(), &MacroSets::default())]).0;
        let count = g.find("Count")[0];
        assert_eq!(g.refs_to(count).iter().filter(|e| e.kind() == refkind::CALL).count(), 2);
        // The function the macro makes runs the case's test method.
        let q = crate::graph::Qualifier::new(&g);
        for case in ["simple", "other", "plain"] {
            let invoker = g.find(&format!("{case}_invoker"))[0];
            let runs: Vec<String> = g.refs_from(invoker).iter().filter(|e| e.kind() == refkind::CALL && !e.ambiguous()).map(|e| q.name(&g, e.other).into_owned()).collect();
            assert_eq!(runs, [format!("{case}::test_method")]);
        }
    }

    #[test]
    fn a_googletest_test_is_its_test_body() {
        let src = "class DBTest { public: void Put(); };\nTEST(Coding, Fixed32) {\n}\nTEST_F(DBTest, Empty) {\n  Put();\n}\n";
        assert_eq!(named(&[("t.cc", src)]), ["Coding_Fixed32_Test::TestBody", "DBTest::Put", "DBTest_Empty_Test::TestBody"]);
    }

    #[test]
    fn a_macro_defining_a_function_in_a_namespace_is_as_one_at_file_scope() {
        // leveldb's first `TEST (Coding, Fixed32)` after `namespace leveldb {`
        // was a function named TEST.
        let files = [
            ("t.h", "#define TEST(a, b) void a##_##b##_Test()\n"),
            ("t.cc", "#include \"t.h\"\nnamespace leveldb {\nTEST(Coding, Fixed32) {\n}\nTEST(Coding, Fixed64) {\n}\n}\n"),
        ];
        let sets = MacroSets::from_defines(files.iter().flat_map(|(_, src)| crate::mask::defines(src.as_bytes(), &crate::cond::Conditions::default())));
        let mut ex = Extractor::new();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &sets)).collect();
        let g = link(&facts).0;
        assert!(g.find("TEST").iter().all(|&s| g.syms[s as usize].kind == kind::MACRO));
        assert_eq!(g.find("Coding_Fixed32_Test").len(), 1);
        assert_eq!(g.find("Coding_Fixed64_Test").len(), 1);
    }

    #[test]
    fn a_class_a_macro_defines_derives_from_what_it_says() {
        // gtest's TEST_F, of a corpus that has gtest: the test's class
        // derives from the fixture, whose methods the body calls.
        let files = [
            ("t.h", "#define TEST_F(f, n) class f##_##n##_Test : public ::testing::f { void TestBody(); }; void f##_##n##_Test::TestBody()\n"),
            ("t.cc", "#include \"t.h\"\nnamespace testing { class LogTest { public: void Write(int x) {} }; }\nclass Other { public: void Write(int x) {} };\nTEST_F(LogTest, BadLength) {\n  Write(1);\n}\n"),
        ];
        let sets = MacroSets::from_defines(files.iter().flat_map(|(_, src)| crate::mask::defines(src.as_bytes(), &crate::cond::Conditions::default())));
        let mut ex = Extractor::new();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &sets)).collect();
        let g = link(&facts).0;
        let q = crate::graph::Qualifier::new(&g);
        let body = g.find("TestBody").iter().copied().find(|&s| g.syms[s as usize].flags & flag::DECL == 0).expect("TestBody");
        let calls: Vec<String> = g.refs_from(body).iter().filter(|e| e.kind() == refkind::CALL && !e.ambiguous()).map(|e| q.name(&g, e.other).into_owned()).collect();
        assert_eq!(calls, ["LogTest::Write"]);
    }

    #[test]
    fn a_call_of_what_a_call_returns_is_its_operator_call() {
        // gtest's ASSERT_THAT: `pred_format (#v1, v1)` of a `pred_format`
        // that is `MakePredicateFormatterFromMatcher (matcher)`.
        let files = [
            ("t.h", "class Formatter {\n public:\n  bool operator()(const char* t, int x) const { return true; }\n};\n\
                     class Other {\n public:\n  bool operator()(const char* t, int x) const { return false; }\n};\n\
                     Formatter MakeFormatter(int m) { return Formatter(); }\n\
                     #define PRED1(pred, v) if (pred(#v, v)) ;\n#define CHECK_THAT(v, m) PRED1(MakeFormatter(m), v)\n"),
            ("t.cc", "#include \"t.h\"\nvoid f() { CHECK_THAT(1, 2); }\nvoid g() { MakeFormatter(3)(\"x\", 4); }\n"),
        ];
        let sets = MacroSets::from_defines(files.iter().flat_map(|(_, src)| crate::mask::defines(src.as_bytes(), &crate::cond::Conditions::default())));
        let mut ex = Extractor::new();
        let facts: Vec<FileFacts> = files.iter().map(|(path, src)| ex.extract(path, src.as_bytes(), &sets)).collect();
        let g = link(&facts).0;
        let q = crate::graph::Qualifier::new(&g);
        for caller in ["f", "g"] {
            let calls: Vec<String> = g.find(caller).iter().flat_map(|&s| g.refs_from(s)).filter(|e| e.kind() == refkind::CALL && !e.ambiguous() && g.sym_name(e.other) == "operator()").map(|e| q.name(&g, e.other).into_owned()).collect();
            assert_eq!(calls, ["Formatter::operator()"], "{caller}");
        }
    }

    #[test]
    fn a_test_macro_the_corpus_defines_is_expanded_not_guessed() {
        // A macro of that name of the corpus's own: what it makes is read
        // from its expansion, and nothing is made of the name.
        let src = "#define TEST(a, b) void a##_##b(void)\nTEST(x, y) {\n}\n";
        let sets = MacroSets::from_defines(crate::mask::defines(src.as_bytes(), &crate::cond::Conditions::default()));
        let mut ex = Extractor::new();
        let g = link(&[ex.extract("t.cc", src.as_bytes(), &sets)]).0;
        assert!(g.find("TestBody").is_empty());
    }
}
