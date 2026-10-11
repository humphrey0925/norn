//! Code-intelligence core: per-file extraction, global link, pluggable stores.

pub mod extract;
pub mod graph;
pub mod link;
pub mod mask;
pub mod model;
pub mod packed;
pub mod sqlite;

use mask::Transparent;
use model::FileFacts;
use rayon::prelude::*;
use std::path::{Path, PathBuf};

/// C sources and headers under `root`, sorted for determinism.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| matches!(p.extension().and_then(|x| x.to_str()), Some("c" | "h")))
        .collect();
    files.sort_unstable();
    files
}

/// Path relative to `root`, with forward slashes.
pub fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Corpus-wide pre-pass: which macros can be dropped before parsing.
pub fn prescan(files: &[PathBuf]) -> Transparent {
    let defines: Vec<Vec<mask::Define>> =
        files.par_iter().map(|p| mask::defines(&std::fs::read(p).unwrap_or_default())).collect();
    Transparent::from_defines(defines.into_iter().flatten())
}

pub fn extract_file(ex: &mut extract::Extractor, root: &Path, path: &Path, transparent: &Transparent) -> FileFacts {
    let src = std::fs::read(path).unwrap_or_default();
    ex.extract(&rel_path(root, path), &src, transparent)
}

/// Extract all files on the current rayon pool; output order matches `files`.
pub fn extract_all(root: &Path, files: &[PathBuf], transparent: &Transparent) -> Vec<FileFacts> {
    files.par_iter().map_init(extract::Extractor::new, |ex, p| extract_file(ex, root, p, transparent)).collect()
}

/// Route tree-sitter's C allocations through mimalloc. The platform heap
/// (notably on Windows) serializes on a lock under many parser threads.
pub fn use_mimalloc_for_parser() {
    use libmimalloc_sys::{mi_calloc, mi_free, mi_malloc, mi_realloc};
    unsafe { tree_sitter::set_allocator(Some(mi_malloc), Some(mi_calloc), Some(mi_realloc), Some(mi_free)) }
}

/// One caller of a function: directly, or by using a macro whose expansion
/// makes the call.
#[derive(Clone, Copy, Debug)]
pub struct Caller {
    pub caller: u32,
    pub line: u32,
    pub ambiguous: bool,
    /// The macro used at `line`, when the call is not written there itself.
    pub via: Option<u32>,
}

/// Callers of `sym`, looking through macros: if macro `M` calls `sym`, every
/// user of `M` calls it too (and so on through macros that use `M`). The
/// macros themselves are reported as well, since the call is written there.
/// A macro that only names the function (`#define emit emit_le`) calls it
/// where the macro itself is called.
pub fn callers_through_macros(g: &graph::Graph, sym: u32) -> Vec<Caller> {
    use model::{kind, refkind};
    let mut out = Vec::new();
    let mut seen_macros = rustc_hash::FxHashSet::default();
    // (target, macro it was reached through, ambiguous so far, whether the
    // call is still to be written by whoever uses the target)
    let mut work = vec![(sym, None, false, true)];
    while let Some((target, via, via_ambiguous, call_pending)) = work.pop() {
        for e in g.refs_to(target) {
            let from_macro = g.syms[e.other as usize].kind == kind::MACRO;
            let called = e.kind() == refkind::CALL;
            if call_pending && !called && !from_macro {
                continue;
            }
            let ambiguous = via_ambiguous || e.ambiguous();
            let pending = call_pending && !called;
            if !pending {
                out.push(Caller { caller: e.other, line: e.line(), ambiguous, via });
            }
            if from_macro && seen_macros.insert((e.other, pending)) {
                work.push((e.other, Some(e.other), ambiguous, pending));
            }
        }
    }
    out
}

/// Bounded BFS over references, the shape of a call-tree / reference-tree view.
/// Returns the number of distinct symbols reached.
pub fn bfs(start: u32, depth: u32, mut neighbors: impl FnMut(u32, &mut Vec<u32>)) -> usize {
    let mut seen = rustc_hash::FxHashSet::default();
    seen.insert(start);
    let mut frontier = vec![start];
    let mut next = Vec::new();
    let mut buf = Vec::new();
    for _ in 0..depth {
        for &s in &frontier {
            buf.clear();
            neighbors(s, &mut buf);
            for &n in &buf {
                if seen.insert(n) {
                    next.push(n);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        std::mem::swap(&mut frontier, &mut next);
        next.clear();
    }
    seen.len()
}
