//! Code-intelligence core: per-file extraction, global link, pluggable stores.

pub mod extract;
pub mod graph;
pub mod link;
pub mod model;
pub mod packed;
pub mod sqlite;

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

pub fn extract_file(ex: &mut extract::Extractor, root: &Path, path: &Path) -> FileFacts {
    let src = std::fs::read(path).unwrap_or_default();
    ex.extract(&rel_path(root, path), &src)
}

/// Extract all files on the current rayon pool; output order matches `files`.
pub fn extract_all(root: &Path, files: &[PathBuf]) -> Vec<FileFacts> {
    files.par_iter().map_init(extract::Extractor::new, |ex, p| extract_file(ex, root, p)).collect()
}

/// Route tree-sitter's C allocations through mimalloc. The platform heap
/// (notably on Windows) serializes on a lock under many parser threads.
pub fn use_mimalloc_for_parser() {
    use libmimalloc_sys::{mi_calloc, mi_free, mi_malloc, mi_realloc};
    unsafe { tree_sitter::set_allocator(Some(mi_malloc), Some(mi_calloc), Some(mi_realloc), Some(mi_free)) }
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
