//! Global link: per-file facts -> one graph. O(names log names + defs + refs).

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

pub fn link(files: &[FileFacts]) -> (Graph, LinkStats) {
    let mut stats = LinkStats::default();

    // 1. Intern every file-local name into one global table, then sort it so
    //    that a name id is its lexicographic rank.
    let t = Instant::now();
    let mut map: FxHashMap<&str, u32> = FxHashMap::default();
    let mut names: Vec<&str> = Vec::new();
    let mut remap: Vec<Vec<u32>> = Vec::with_capacity(files.len());
    for f in files {
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
    let t = Instant::now();
    let syms = &g.syms;
    let per_file: Vec<(Vec<[u32; 3]>, [u64; 4])> = files
        .par_iter()
        .enumerate()
        .map(|(fi, f)| {
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
                if tmp.is_empty() {
                    tmp.extend(cands.iter().filter(|&&c| {
                        let s = &syms[c as usize];
                        s.kind != kind::FILE && (s.flags & flag::STATIC == 0 || s.flags & flag::HEADER != 0)
                    }));
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
