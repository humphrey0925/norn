//! Global link: per-file facts -> one graph. O(names log names + defs + refs),
//! plus the include closures, O(sum of closure sizes).

use crate::graph::Graph;
use crate::model::{flag, kind, pack_meta, refkind, root, EdgeHalf, FileFacts, FileRec, MemberHint, Sym, NONE};
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

/// How an `#include` operand is found among the indexed files, without the
/// build's `-I` list (see [`Options`]).
pub(crate) struct Includes<'a> {
    paths: Vec<&'a str>,
    by_path: FxHashMap<&'a str, u32>,
    by_base: FxHashMap<&'a str, Vec<u32>>,
}

impl<'a> Includes<'a> {
    pub(crate) fn new(paths: impl IntoIterator<Item = &'a str>) -> Includes<'a> {
        let paths: Vec<&str> = paths.into_iter().collect();
        let by_path = paths.iter().enumerate().map(|(i, p)| (*p, i as u32)).collect();
        let mut by_base: FxHashMap<&str, Vec<u32>> = FxHashMap::default();
        for (i, p) in paths.iter().enumerate() {
            by_base.entry(p.rsplit('/').next().unwrap_or(p)).or_default().push(i as u32);
        }
        Includes { paths, by_path, by_base }
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
        let shared = |id: u32| {
            let other = dir_of(paths[id as usize]);
            (above(id), include_folder(id), dir.split('/').zip(other.split('/')).take_while(|(a, b)| a == b).count())
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
fn include_edges(files: &[FileFacts], aliases: &[(String, String)], stats: &mut LinkStats) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
    let found = Includes::new(files.iter().map(|f| f.path.as_str()));
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
        // All it includes may not be compiled (`#ifdef CONFIG_PCI` extern
        // ... `#else` a stub): what is defined elsewhere for certain is the
        // other reading.
        // (A prototype beside the stub, in the other branch, is no
        // definition either way.)
        // (So is a macro defined only in a branch: `#ifdef
        // CONFIG_DEBUG_LOCK_ALLOC` `#define mutex_lock(l) ...` `#else` the
        // prototype.)
        let maybe = |c: &u32| {
            let s = &syms[*c as usize];
            matches!(s.kind, kind::FUNCTION | kind::MACRO) && s.flags & (flag::CONDITIONAL | flag::DECL) == flag::CONDITIONAL
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
        let f = self.files[fi];
        let name = self.remap[fi][hint.root as usize];
        let type_name = match hint.root_kind {
            root::TYPE => name,
            kind_of_root => {
                // The variable or function the name means here, then its type.
                let wanted = if kind_of_root == root::VAR { kind::GLOBAL } else { kind::FUNCTION };
                in_sight(g.syms_named(name), &g.syms, own, closure, |s| s.kind == wanted, scratch);
                *scratch.iter().find_map(|s| self.of_symbol.get(s))?
            }
        };
        let (mut rf, mut ri) = record_of(type_name, fi as u32, closure, self.records, self.aliases)?;
        let position = |rf: u32, ri: u32, wanted: u32| {
            let r = &self.files[rf as usize].records[ri as usize];
            (0..r.fields.len()).find(|&i| r.fields[i] != NONE && self.remap[rf as usize][r.fields[i] as usize] == wanted)
        };
        for &m in &f.hint_path[hint.path as usize..(hint.path + hint.len) as usize] {
            let at = position(rf, ri, self.remap[fi][m as usize])?;
            let t = self.files[rf as usize].records[ri as usize].types[at];
            if t == NONE {
                return None;
            }
            (rf, ri) = record_of(self.remap[rf as usize][t as usize], rf, closure, self.records, self.aliases)?;
        }
        let at = position(rf, ri, member)?;
        let def = self.files[rf as usize].records[ri as usize].defs[at];
        (def != NONE).then(|| g.files[rf as usize].sym_start + def)
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
    let (includes, sure_includes) = include_edges(files, &options.include_aliases, &mut stats);
    stats.include_ms = ms(t);
    let t = Instant::now();
    (stats.sites, stats.expanded_defs, stats.expanded_refs) = crate::expand::expand(files, &includes, &sure_includes, cache);
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
    crate::mem_mark("symbols");

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
    for (fi, f) in files.iter().enumerate() {
        for (i, r) in f.records.iter().enumerate() {
            records.entry(remap[fi][r.name as usize]).or_default().push((fi as u32, i as u32));
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

    let mut of_symbol: FxHashMap<u32, u32> = FxHashMap::default();
    for (fi, f) in files.iter().enumerate() {
        for &(def, t) in &f.typed {
            of_symbol.insert(g.files[fi].sym_start + def, remap[fi][t as usize]);
        }
    }
    let types = Types { files, remap: &remap, records: &records, aliases: &aliases, of_symbol: &of_symbol };

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
            let mut scratch: Vec<u32> = Vec::new();
            let mut hint_of = vec![NONE; f.refs.len()];
            for &(r, h) in &f.ref_hints {
                hint_of[r as usize] = h;
            }
            for (ri, r) in f.refs.iter().enumerate() {
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
                // With the type of what the member is taken from, the member
                // is one symbol and no guess.
                let typed = (member && hint_of[ri] != NONE)
                    .then(|| types.member(&f.hints[hint_of[ri] as usize], remap[fi][r.name as usize], fi, &g, base..end, closure, &mut scratch))
                    .flatten();
                match typed {
                    Some(sym) => {
                        tmp.clear();
                        tmp.push(sym);
                    }
                    None => in_sight(cands, syms, base..end, closure, want, &mut tmp),
                }
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
            for b in f.binds.iter().filter(|b| b.slot != NONE) {
                let typed = (!b.var && b.index == NONE && b.hint != NONE)
                    .then(|| types.member(&f.hints[b.hint as usize], remap[fi][b.slot as usize], fi, &g, base..end, closure, &mut scratch))
                    .flatten();
                if let Some(sym) = typed {
                    slots.clear();
                    slots.push(sym);
                } else if b.var {
                    in_sight(g.syms_named(remap[fi][b.slot as usize]), syms, base..end, closure, |s| s.kind == kind::GLOBAL, &mut slots);
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
                    in_sight(cands, syms, base..end, closure, holds, &mut slots);
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
