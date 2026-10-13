//! Global link: per-file facts -> one graph. O(names log names + defs + refs),
//! plus the include closures, O(sum of closure sizes).

use crate::graph::Graph;
use crate::model::{flag, kind, pack_meta, refkind, EdgeHalf, FileFacts, FileRec, Sym, NONE};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::time::Instant;

/// Upper bound on targets emitted for one ambiguous reference.
/// Structs that may share a member name before a use of the name is left
/// unresolved.
const MAX_MEMBER_CANDIDATES: usize = 8;
const MAX_CANDIDATES: usize = 32;

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

/// Counting sort of `edges` by `key`, producing a CSR adjacency list.
fn csr(n: usize, edges: &[[u32; 3]], key: usize, other: usize) -> (Vec<u32>, Vec<EdgeHalf>) {
    let mut off = vec![0u32; n + 1];
    for e in edges {
        off[e[key] as usize + 1] += 1;
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    let mut next = off.clone();
    let mut adj = vec![EdgeHalf { other: 0, meta: 0 }; edges.len()];
    for e in edges {
        let slot = &mut next[e[key] as usize];
        adj[*slot as usize] = EdgeHalf { other: e[other], meta: e[2] };
        *slot += 1;
    }
    (off, adj)
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
}

/// Returns the edges twice: all of them, and those not under an `#if`.
fn include_edges(files: &[FileFacts], aliases: &[(String, String)], stats: &mut LinkStats) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
    let by_path: FxHashMap<&str, u32> = files.iter().enumerate().map(|(i, f)| (f.path.as_str(), i as u32)).collect();
    let mut by_base: FxHashMap<&str, Vec<u32>> = FxHashMap::default();
    for (i, f) in files.iter().enumerate() {
        by_base.entry(f.path.rsplit('/').next().unwrap_or(&f.path)).or_default().push(i as u32);
    }
    let edges: Vec<(Vec<u32>, u64, Vec<u32>)> = files
        .par_iter()
        .map(|f| {
            let dir = dir_of(&f.path);
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
                if let Some(&id) = by_path.get(join(dir, operand).as_str()) {
                    out.push(id);
                    if always {
                        sure.push(id);
                    }
                    continue;
                }
                let tail = join("", operand);
                let base = tail.rsplit('/').next().unwrap_or(&tail);
                let matches = |id: &&u32| {
                    let p = files[**id as usize].path.as_str();
                    p == tail || p.ends_with(&tail) && p.as_bytes()[p.len() - tail.len() - 1] == b'/'
                };
                let cands: Vec<u32> = by_base.get(base).into_iter().flatten().filter(matches).copied().collect();
                let shared = |id: u32| {
                    let other = dir_of(&files[id as usize].path);
                    dir.split('/').zip(other.split('/')).take_while(|(a, b)| a == b).count()
                };
                match cands.iter().map(|&c| shared(c)).max() {
                    Some(best) => out.extend(cands.iter().copied().filter(|&c| shared(c) == best)),
                    None => missing += 1,
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
}

impl Closure {
    fn new(n: usize) -> Self {
        Closure { member: vec![false; n], list: Vec::new() }
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
    want: impl Fn(&Sym) -> bool,
    out: &mut Vec<u32>,
) {
    out.clear();
    // A definition in the same file shadows everything else (covers `static`).
    out.extend(cands.iter().filter(|&&c| own.contains(&c) && want(&syms[c as usize])));
    let external = |c: &&u32| {
        let s = &syms[**c as usize];
        want(s) && (s.flags & flag::STATIC == 0 || s.flags & flag::HEADER != 0)
    };
    if out.is_empty() {
        // Included text is part of this translation unit, `static` or not.
        out.extend(cands.iter().filter(|&&c| want(&syms[c as usize]) && closure.member[syms[c as usize].file as usize]));
    }
    if !out.is_empty() && out.iter().all(|&c| syms[c as usize].flags & flag::DECL != 0) {
        let n = out.len();
        out.extend(cands.iter().filter(external).filter(|&&c| syms[c as usize].flags & flag::DECL == 0));
        if out.len() > n {
            out.drain(..n);
        }
    }
    if out.is_empty() {
        out.extend(cands.iter().filter(external));
    }
}

/// The members of the struct called `name` (a tag or a typedef) as seen
/// from `file`: which file lays it out, and the member names there.
fn layout<'a>(
    name: u32,
    file: u32,
    closure: &Closure,
    records: &FxHashMap<u32, Vec<(u32, u32)>>,
    aliases: &FxHashMap<u32, Vec<u32>>,
    files: &[&'a FileFacts],
) -> Option<(u32, &'a [u32])> {
    let direct = records.get(&name).into_iter().flatten();
    let through = aliases.get(&name).into_iter().flatten().filter_map(|tag| records.get(tag)).flatten();
    let mut all: Vec<(u32, u32)> = direct.chain(through).copied().collect();
    all.sort_unstable();
    all.dedup();
    let pick = all
        .iter()
        .find(|r| r.0 == file)
        .or_else(|| all.iter().find(|r| closure.member[r.0 as usize]))
        .or(if all.len() == 1 { all.first() } else { None })?;
    Some((pick.0, &files[pick.0 as usize].records[pick.1 as usize].fields))
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
    let mut stats = LinkStats::default();

    // 0. Which file includes which, then macro expansion: it needs the
    //    definitions other files give, and adds definitions and references.
    let t = Instant::now();
    let (includes, sure_includes) = include_edges(files, &options.include_aliases, &mut stats);
    stats.include_ms = ms(t);
    let t = Instant::now();
    (stats.sites, stats.expanded_defs, stats.expanded_refs) = crate::expand::expand(files, &includes, &sure_includes, cache);
    stats.expand_ms = ms(t);
    let files: Vec<&FileFacts> = files.iter().zip(&cache.expanded).map(|(f, e)| e.as_ref().unwrap_or(f)).collect();
    let files = &files[..];

    // 1. Intern every file-local name into one global table, then sort it so
    //    that a name id is its lexicographic rank.
    let t = Instant::now();
    let mut map: FxHashMap<&str, u32> = FxHashMap::default();
    let mut names: Vec<&str> = Vec::new();
    let mut remap: Vec<Vec<u32>> = Vec::with_capacity(files.len());
    for &f in files {
        let mut r = Vec::with_capacity(f.n_names());
        for i in 0..f.n_names() {
            let s = f.name(i);
            let id = *map.entry(s).or_insert_with(|| {
                names.push(s);
                (names.len() - 1) as u32
            });
            r.push(id);
        }
        remap.push(r);
    }
    drop(map);
    let mut order: Vec<u32> = (0..names.len() as u32).collect();
    order.par_sort_unstable_by(|&a, &b| names[a as usize].cmp(names[b as usize]));
    let mut rank = vec![0u32; names.len()];
    for (new, &old) in order.iter().enumerate() {
        rank[old as usize] = new as u32;
    }
    remap.par_iter_mut().for_each(|r| r.iter_mut().for_each(|x| *x = rank[*x as usize]));
    let mut g = Graph::default();
    g.str_off.reserve(names.len() + 1);
    g.str_off.push(0);
    for &old in &order {
        g.str_bytes.extend_from_slice(names[old as usize].as_bytes());
        g.str_off.push(g.str_bytes.len() as u32);
    }
    stats.intern_ms = ms(t);

    // 2. Symbol table (each file's symbols contiguous) and name -> symbols index.
    let t = Instant::now();
    let n_syms: usize = files.iter().map(|f| f.defs.len()).sum();
    g.syms.reserve(n_syms);
    g.by_name_off = vec![0u32; names.len() + 1];
    for (fi, f) in files.iter().enumerate() {
        let header = if f.path.ends_with(".h") { flag::HEADER } else { 0 };
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
    for (fi, f) in files.iter().enumerate() {
        for (i, r) in f.records.iter().enumerate() {
            records.entry(remap[fi][r.name as usize]).or_default().push((fi as u32, i as u32));
        }
        for &(alias, tag) in &f.aliases {
            aliases.entry(remap[fi][alias as usize]).or_default().push(remap[fi][tag as usize]);
        }
    }

    // Functions that call through a parameter: symbol -> (position, line).
    let mut through_param: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
    for (fi, f) in files.iter().enumerate() {
        for c in &f.param_calls {
            through_param.entry(g.files[fi].sym_start + c.scope).or_default().push((c.index, c.line));
        }
    }

    // 3. Resolve references. Each file is independent given the symbol table.
    //    A name means, in order: a definition in this file; one in a file it
    //    includes; anything with external visibility.
    let t = Instant::now();
    let syms = &g.syms;
    let per_file: Vec<(Vec<[u32; 3]>, [u64; 4], Vec<[u32; 4]>, Vec<[u32; 3]>)> = files
        .par_iter()
        .enumerate()
        .map_init(|| (Closure::new(files.len()), Closure::new(files.len())), |(closure, sure), (fi, f)| {
            closure.fill(fi as u32, &includes);
            // What the file reads in every configuration.
            sure.fill(fi as u32, &sure_includes);
            let base = g.files[fi].sym_start;
            let end = base + f.defs.len() as u32;
            let mut edges = Vec::with_capacity(f.refs.len());
            let mut counts = [0u64; 4]; // resolved, ambiguous, unresolved, capped
            let mut tmp: Vec<u32> = Vec::new();
            for r in &f.refs {
                let (kind, maybe) = (r.kind & !refkind::MAYBE, r.kind & refkind::MAYBE != 0);
                let cands = g.syms_named(remap[fi][r.name as usize]);
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
                in_sight(cands, syms, base..end, closure, want, &mut tmp);
                if tmp.is_empty() {
                    counts[2] += 1;
                    continue;
                }
                let callable = |c: &u32| matches!(syms[*c as usize].kind, kind::FUNCTION | kind::MACRO);
                if matches!(kind, refkind::CALL | refkind::PCALL) && tmp.iter().any(callable) {
                    tmp.retain(callable);
                }
                let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                if tmp.iter().any(is_def) {
                    tmp.retain(is_def);
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
                let optional = |c: &u32| {
                    let s = &syms[*c as usize];
                    s.kind == kind::MACRO && (s.flags & flag::CONDITIONAL != 0 || s.file != fi as u32 && !sure.member[s.file as usize])
                };
                let ambiguous = tmp.len() > 1 || maybe || tmp.iter().any(optional);
                counts[ambiguous as usize] += 1;
                let meta = pack_meta(r.line, ambiguous, kind);
                edges.extend(tmp.iter().map(|&to| [base + r.scope, to, meta]));
            }

            // Functions stored in members and variables: an edge from the
            // member or variable to the function.
            let mut bound: Vec<[u32; 3]> = Vec::new();
            let mut slots: Vec<u32> = Vec::new();
            for b in &f.binds {
                if b.var {
                    in_sight(g.syms_named(remap[fi][b.slot as usize]), syms, base..end, closure, |s| s.kind == kind::GLOBAL, &mut slots);
                } else {
                    // Positional: the member is the struct's `index`-th.
                    let (name, home) = if b.index == NONE {
                        (remap[fi][b.slot as usize], None)
                    } else {
                        let layout = layout(remap[fi][b.slot as usize], fi as u32, closure, &records, &aliases, files);
                        match layout.and_then(|(rf, fields)| Some((rf, *fields.get(b.index as usize)?))) {
                            Some((rf, field)) if field != NONE => (remap[rf as usize][field as usize], Some(rf)),
                            _ => continue,
                        }
                    };
                    let cands = g.syms_named(name);
                    let holds = |s: &Sym| s.kind == kind::FIELD && s.flags & flag::CALLABLE != 0;
                    in_sight(cands, syms, base..end, closure, holds, &mut slots);
                    if let Some(rf) = home {
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
                in_sight(named, syms, base..end, closure, |s| s.kind == kind::FUNCTION, &mut tmp);
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
                    in_sight(g.syms_named(remap[fi][a.callee as usize]), syms, base..end, closure, |s| s.kind == kind::FUNCTION, &mut slots);
                    if slots.iter().any(is_def) {
                        slots.retain(is_def);
                    }
                    for &callee in slots.iter().take(MAX_MEMBER_CANDIDATES) {
                        for &(_, line) in through_param.get(&callee).into_iter().flatten().filter(|p| p.0 == a.index) {
                            in_sight(g.syms_named(remap[fi][a.func as usize]), syms, base..end, closure, |s| s.kind == kind::FUNCTION, &mut tmp);
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
            bound.sort_unstable();
            bound.dedup_by_key(|e| (e[0], e[1]));
            let sites = bound.iter().map(|e| [fi as u32, e[2] >> 4, e[0], e[1]]).collect();
            edges.extend(bound);
            handed.sort_unstable();
            handed.dedup();
            (edges, counts, sites, handed)
        })
        .collect();
    let mut edges: Vec<[u32; 3]> = Vec::with_capacity(per_file.iter().map(|p| p.0.len()).sum());
    // The same callback is handed over in many places.
    let mut handed: Vec<[u32; 3]> = per_file.iter().flat_map(|p| p.3.iter().copied()).collect();
    handed.sort_unstable();
    handed.dedup();
    edges.extend(handed);
    for (e, c, sites, _) in &per_file {
        stats.bind_sites.extend_from_slice(sites);
        edges.extend_from_slice(e);
        stats.resolved += c[0];
        stats.ambiguous += c[1];
        stats.unresolved += c[2];
        stats.capped += c[3];
    }
    drop(per_file);
    stats.refs = stats.resolved + stats.ambiguous + stats.unresolved;
    stats.edges = edges.len() as u64;
    stats.resolve_ms = ms(t);

    // 4. Forward and reverse adjacency.
    let t = Instant::now();
    let ((out_off, out), (in_off, inc)) = rayon::join(|| csr(n_syms, &edges, 0, 1), || csr(n_syms, &edges, 1, 0));
    g.out_off = out_off;
    g.out = out;
    g.in_off = in_off;
    g.inc = inc;
    stats.csr_ms = ms(t);

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
        let sets = MacroSets::from_defines(files.iter().flat_map(|(_, src)| crate::mask::defines(src.as_bytes())));
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
