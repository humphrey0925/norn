//! Global link: per-file facts -> one graph. O(names log names + defs + refs),
//! plus the include closures, O(sum of closure sizes).

use crate::graph::Graph;
use crate::model::{flag, kind, pack_meta, refkind, EdgeHalf, FileFacts, FileRec, Sym};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::time::Instant;

/// Upper bound on targets emitted for one ambiguous reference.
const MAX_CANDIDATES: usize = 32;

#[derive(Clone, Copy, Debug, Default)]
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
fn include_edges(files: &[FileFacts], stats: &mut LinkStats) -> Vec<Vec<u32>> {
    let by_path: FxHashMap<&str, u32> = files.iter().enumerate().map(|(i, f)| (f.path.as_str(), i as u32)).collect();
    let mut by_base: FxHashMap<&str, Vec<u32>> = FxHashMap::default();
    for (i, f) in files.iter().enumerate() {
        by_base.entry(f.path.rsplit('/').next().unwrap_or(&f.path)).or_default().push(i as u32);
    }
    let edges: Vec<(Vec<u32>, u64)> = files
        .par_iter()
        .map(|f| {
            let dir = dir_of(&f.path);
            let (mut out, mut missing) = (Vec::new(), 0);
            for &inc in &f.includes {
                let operand = f.name(inc as usize);
                if let Some(&id) = by_path.get(join(dir, operand).as_str()) {
                    out.push(id);
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
            }
            out.sort_unstable();
            out.dedup();
            (out, missing)
        })
        .collect();
    stats.includes_found = edges.iter().map(|e| e.0.len() as u64).sum();
    stats.includes_missing = edges.iter().map(|e| e.1).sum();
    edges.into_iter().map(|e| e.0).collect()
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

pub fn link(files: &[FileFacts]) -> (Graph, LinkStats) {
    let mut stats = LinkStats::default();

    // 0. Which file includes which, then macro expansion: it needs the
    //    definitions other files give, and adds definitions and references.
    let t = Instant::now();
    let includes = include_edges(files, &mut stats);
    stats.include_ms = ms(t);
    let t = Instant::now();
    let (expanded, counts) = crate::expand::expand(files, &includes);
    (stats.sites, stats.expanded_defs, stats.expanded_refs) = counts;
    stats.expand_ms = ms(t);
    let files: Vec<&FileFacts> = files.iter().zip(&expanded).map(|(f, e)| e.as_ref().unwrap_or(f)).collect();
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

    // 3. Resolve references. Each file is independent given the symbol table.
    //    A name means, in order: a definition in this file; one in a file it
    //    includes; anything with external visibility.
    let t = Instant::now();
    let syms = &g.syms;
    let per_file: Vec<(Vec<[u32; 3]>, [u64; 4])> = files
        .par_iter()
        .enumerate()
        .map_init(|| Closure::new(files.len()), |closure, (fi, f)| {
            closure.fill(fi as u32, &includes);
            let base = g.files[fi].sym_start;
            let end = base + f.defs.len() as u32;
            let mut edges = Vec::with_capacity(f.refs.len());
            let mut counts = [0u64; 4]; // resolved, ambiguous, unresolved, capped
            let mut tmp: Vec<u32> = Vec::new();
            for r in &f.refs {
                let cands = g.syms_named(remap[fi][r.name as usize]);
                tmp.clear();
                // A definition in the same file shadows everything else (covers `static`).
                tmp.extend(cands.iter().filter(|&&c| c >= base && c < end && syms[c as usize].kind != kind::FILE));
                let external = |c: &&u32| {
                    let s = &syms[**c as usize];
                    s.kind != kind::FILE && (s.flags & flag::STATIC == 0 || s.flags & flag::HEADER != 0)
                };
                if tmp.is_empty() {
                    // Included text is part of this translation unit, `static` or not.
                    tmp.extend(cands.iter().filter(|&&c| {
                        let s = &syms[c as usize];
                        s.kind != kind::FILE && closure.member[s.file as usize]
                    }));
                }
                // Only a prototype or `extern` in sight: the definition is in
                // another translation unit.
                if !tmp.is_empty() && tmp.iter().all(|&c| syms[c as usize].flags & flag::DECL != 0) {
                    let n = tmp.len();
                    tmp.extend(cands.iter().filter(external).filter(|&&c| syms[c as usize].flags & flag::DECL == 0));
                    if tmp.len() > n {
                        tmp.drain(..n);
                    }
                }
                if tmp.is_empty() {
                    tmp.extend(cands.iter().filter(external));
                }
                if tmp.is_empty() {
                    counts[2] += 1;
                    continue;
                }
                let callable = |c: &u32| matches!(syms[*c as usize].kind, kind::FUNCTION | kind::MACRO);
                if r.kind == refkind::CALL && tmp.iter().any(callable) {
                    tmp.retain(callable);
                }
                let is_def = |c: &u32| syms[*c as usize].flags & flag::DECL == 0;
                if tmp.iter().any(is_def) {
                    tmp.retain(is_def);
                }
                if tmp.len() > MAX_CANDIDATES {
                    tmp.truncate(MAX_CANDIDATES);
                    counts[3] += 1;
                }
                let ambiguous = tmp.len() > 1;
                counts[ambiguous as usize] += 1;
                let meta = pack_meta(r.line, ambiguous, r.kind);
                edges.extend(tmp.iter().map(|&to| [base + r.scope, to, meta]));
            }
            (edges, counts)
        })
        .collect();
    let mut edges: Vec<[u32; 3]> = Vec::with_capacity(per_file.iter().map(|p| p.0.len()).sum());
    for (e, c) in &per_file {
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
