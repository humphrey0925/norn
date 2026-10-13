//! Code-intelligence core: per-file extraction, global link, pluggable stores.

pub mod cond;
pub mod expand;
pub mod extract;
pub mod graph;
pub mod link;
pub mod mask;
pub mod model;
pub mod packed;
pub mod progress;
pub mod scope;
pub mod search;
pub mod session;
pub mod sqlite;
pub mod store;

use mask::MacroSets;
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

/// The C sources and headers under `root` that `scope` makes part of the
/// project, sorted. Folders that hold nothing of it are not entered.
pub fn scan_scope(root: &Path, scope: &scope::Scope) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_type().is_dir() || scope.reaches(&rel_path(root, e.path())))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| matches!(p.extension().and_then(|x| x.to_str()), Some("c" | "h")))
        .filter(|p| scope.contains(&rel_path(root, p)))
        .collect();
    files.sort_unstable();
    files
}

/// Path relative to `root`, with forward slashes.
pub fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Corpus-wide pre-pass: which macros can be dropped before parsing, and
/// which have to be expanded.
pub fn prescan(files: &[PathBuf]) -> MacroSets {
    prescan_under(Path::new(""), files, &mut cond::Conditions::without_source())
}

/// The compile conditions the tree's settings give, told (when they assume
/// anything) which names the files `#define` themselves.
pub fn conditions_of(root: &Path, files: &[PathBuf]) -> cond::Conditions {
    let mut conditions = cond::Conditions::load(root);
    if conditions.assumes() {
        let names: Vec<Vec<Vec<u8>>> = files
            .par_iter()
            .map(|p| mask::defined_names(&std::fs::read(p).unwrap_or_default()).into_iter().map(<[u8]>::to_vec).collect())
            .collect();
        conditions.set_in_source(names.into_iter().flatten().collect());
    }
    conditions
}

/// Every file's `#define`s under `conditions`, with what the sources settle
/// about macros put into `conditions` on the way: which definitions are
/// compiled depends on that, and that on which are compiled, so it is done
/// until nothing changes.
pub fn settle(root: &Path, files: &[PathBuf], conditions: &mut cond::Conditions) -> Vec<Vec<mask::Define>> {
    conditions.set_source(None);
    let paths: Vec<String> = files.iter().map(|p| rel_path(root, p)).collect();
    let read = |c: &cond::Conditions| -> Vec<Vec<mask::Define>> {
        files.par_iter().zip(&paths).map(|(p, rel)| mask::defines_of(&std::fs::read(p).unwrap_or_default(), c, Some(rel))).collect()
    };
    let mut defines = read(conditions);
    if !conditions.uses_source() {
        return defines;
    }
    for _ in 0..4 {
        let source = mask::source_of(&defines, &paths);
        if conditions.source().map_or(source.is_empty(), |s| **s == source) {
            break;
        }
        conditions.set_source(Some(std::sync::Arc::new(source)));
        defines = read(conditions);
    }
    defines
}

/// The same, with what the build is known to define: `#define`s in branches
/// it never compiles do not count, and extraction skips those branches.
/// `conditions` come back with what the sources settle added.
pub fn prescan_under(root: &Path, files: &[PathBuf], conditions: &mut cond::Conditions) -> MacroSets {
    let defines = settle(root, files, conditions);
    MacroSets::from_defines(defines.into_iter().flatten()).with_conditions(conditions.clone())
}

pub fn extract_file(ex: &mut extract::Extractor, root: &Path, path: &Path, transparent: &MacroSets) -> FileFacts {
    let src = std::fs::read(path).unwrap_or_default();
    let facts = ex.extract(&rel_path(root, path), &src, transparent);
    progress::tick();
    facts
}

/// Extract all files on the current rayon pool; output order matches `files`.
pub fn extract_all(root: &Path, files: &[PathBuf], transparent: &MacroSets) -> Vec<FileFacts> {
    progress::parsing(files.len());
    files.par_iter().map_init(extract::Extractor::new, |ex, p| extract_file(ex, root, p, transparent)).collect()
}

/// Route tree-sitter's C allocations through mimalloc. The platform heap
/// (notably on Windows) serializes on a lock under many parser threads.
pub fn use_mimalloc_for_parser() {
    use libmimalloc_sys::{mi_calloc, mi_free, mi_malloc, mi_realloc};
    unsafe { tree_sitter::set_allocator(Some(mi_malloc), Some(mi_calloc), Some(mi_realloc), Some(mi_free)) }
}

/// One caller of a function: directly, by using a macro whose expansion
/// makes the call, or by calling through a member or variable the function
/// is stored in.
#[derive(Clone, Copy, Debug)]
pub struct Caller {
    pub caller: u32,
    pub line: u32,
    pub ambiguous: bool,
    /// The macro used at `line`, or the member or variable called through,
    /// when the call is not written there by name.
    pub via: Option<u32>,
    /// The member or variable the call goes through, when it does: it then
    /// reaches this function only if that is what is stored there at the
    /// time.
    pub slot: Option<u32>,
    /// The call goes through a local pointer variable that may hold this.
    pub pointer: bool,
}

impl Caller {
    pub fn indirect(&self) -> bool {
        self.slot.is_some() || self.pointer
    }
}

/// Callers of `sym`, looking through macros and function pointers:
///
/// * if macro `M` calls `sym`, every user of `M` calls it too (and so on
///   through macros that use `M`); the macros themselves are reported as
///   well, since the call is written there;
/// * a macro that only names the function (`#define emit emit_le`) calls it
///   where the macro itself is called;
/// * if `sym` is stored in a struct member or a variable, every call through
///   that member or variable may call it.
pub fn callers_through_macros(g: &graph::Graph, sym: u32) -> Vec<Caller> {
    use model::{kind, refkind};
    let mut out = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    // (target, what it was reached through, ambiguous so far, whether the
    // call is still to be written by whoever uses the target, indirect)
    let mut work = vec![(sym, None, false, true, None)];
    while let Some((target, via, via_ambiguous, call_pending, slot)) = work.pop() {
        for e in g.refs_to(target) {
            if e.kind() == refkind::BIND {
                // `target` is stored in `e.other`.
                if via.is_none() && seen.insert((e.other, true)) {
                    work.push((e.other, Some(e.other), e.ambiguous(), true, Some(e.other)));
                }
                continue;
            }
            let from_macro = g.syms[e.other as usize].kind == kind::MACRO;
            let called = matches!(e.kind(), refkind::CALL | refkind::ICALL | refkind::PCALL);
            if call_pending && !called && !from_macro {
                continue;
            }
            // A member is called only where it is called: a macro that
            // merely names it (`.callback = (f)`) calls nothing.
            if slot == Some(target) && !called {
                continue;
            }
            let ambiguous = via_ambiguous || e.ambiguous();
            let pending = call_pending && !called;
            if !pending {
                out.push(Caller { caller: e.other, line: e.line(), ambiguous, via, slot, pointer: e.kind() == refkind::PCALL });
            }
            if from_macro && seen.insert((e.other, pending)) {
                work.push((e.other, Some(e.other), ambiguous, pending, slot));
            }
        }
    }
    out
}

/// Who mentions `sym`, whatever it is: the "called by" of a variable, a
/// type, a member, a constant or a macro. A function's callers are these
/// only in part (see [`callers_through_macros`]).
pub fn users(g: &graph::Graph, sym: u32) -> Vec<Caller> {
    use model::refkind;
    let of = |e: &model::EdgeHalf| Caller {
        caller: e.other,
        line: e.line(),
        ambiguous: e.ambiguous(),
        via: None,
        slot: None,
        pointer: e.kind() == refkind::PCALL,
    };
    g.refs_to(sym).iter().filter(|e| e.kind() != refkind::BIND).map(of).collect()
}

/// The functions stored in the member or variable `slot`: what a call
/// through it may reach.
pub fn stored_in(g: &graph::Graph, slot: u32) -> Vec<Caller> {
    let of = |e: &model::EdgeHalf| Caller { caller: e.other, line: 0, ambiguous: e.ambiguous(), via: None, slot: None, pointer: false };
    g.refs_from(slot).iter().filter(|e| e.kind() == model::refkind::BIND).map(of).collect()
}

/// What `sym` calls: by name, and through members and variables (`via`),
/// each function stored there.
pub fn callees_through_slots(g: &graph::Graph, sym: u32) -> Vec<Caller> {
    use model::refkind;
    let mut out = Vec::new();
    for e in g.refs_from(sym).iter().filter(|e| matches!(e.kind(), refkind::CALL | refkind::ICALL | refkind::PCALL)) {
        let stored: Vec<_> = g.refs_from(e.other).iter().filter(|b| b.kind() == refkind::BIND).collect();
        if e.kind() != refkind::ICALL {
            let pointer = e.kind() == refkind::PCALL;
            out.push(Caller { caller: e.other, line: e.line(), ambiguous: e.ambiguous(), via: None, slot: None, pointer });
        }
        for b in stored {
            let ambiguous = e.ambiguous() || b.ambiguous();
            out.push(Caller { caller: b.other, line: e.line(), ambiguous, via: Some(e.other), slot: Some(e.other), pointer: false });
        }
    }
    out
}

/// The symbol the name `name` on `line` of `file` (a path as indexed) refers
/// to: what "go to definition" wants. Falls back on every symbol of that
/// name when the line has no such reference (a declaration, say).
pub fn definition_at(g: &graph::Graph, file: &str, line: u32, name: &str) -> Vec<u32> {
    let here = (0..g.files.len() as u32).find(|&f| g.file_path(f) == file);
    let mut out: Vec<u32> = here
        .map(|f| g.symbol_at(f, line).unwrap_or(g.files[f as usize].sym_start))
        .into_iter()
        .flat_map(|scope| g.refs_from(scope))
        .filter(|e| e.line() == line && e.kind() != model::refkind::BIND && g.sym_name(e.other) == name)
        .map(|e| e.other)
        .collect();
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        out.extend_from_slice(g.find(name));
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
