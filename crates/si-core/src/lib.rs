//! Code-intelligence core: per-file extraction, global link, pluggable stores.

pub mod builds;
pub mod cond;
pub mod expand;
pub mod extract;
pub mod graph;
pub mod link;
pub mod mask;
pub mod model;
pub mod names;
pub mod packed;
pub mod progress;
pub mod scope;
pub mod search;
pub mod session;
pub mod sqlite;
pub mod store;
pub mod units;

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
///
/// A file of another kind is one of them when a source includes it by
/// name (`#include "commands.def"`, `#include "table.inc"`): such files
/// are C all the same, often the lists that tables are generated from.
pub fn scan_scope(root: &Path, scope: &scope::Scope) -> Vec<PathBuf> {
    let is_c = |p: &Path| matches!(p.extension().and_then(|x| x.to_str()), Some("c" | "h"));
    let (mut files, others): (Vec<PathBuf>, Vec<PathBuf>) = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_type().is_dir() || scope.reaches(&rel_path(root, e.path())))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| scope.contains(&rel_path(root, p)))
        .partition(|p| is_c(p));
    // The other files by name, for the includes that mean them.
    let mut by_name: rustc_hash::FxHashMap<&std::ffi::OsStr, Vec<&PathBuf>> = Default::default();
    for p in &others {
        by_name.entry(p.file_name().unwrap_or_default()).or_default().push(p);
    }
    let mut taken: rustc_hash::FxHashSet<PathBuf> = Default::default();
    let mut look_in: Vec<PathBuf> = files.clone();
    while !look_in.is_empty() && !by_name.is_empty() {
        let found: Vec<PathBuf> = look_in
            .par_iter()
            .flat_map_iter(|p| {
                let src = std::fs::read(p).unwrap_or_default();
                let mut out = Vec::new();
                for line in src.split(|&b| b == b'\n') {
                    let Some(rest) = line.trim_ascii_start().strip_prefix(b"#") else { continue };
                    let Some(rest) = rest.trim_ascii_start().strip_prefix(b"include") else { continue };
                    let Some(rest) = rest.trim_ascii_start().strip_prefix(b"\"") else { continue };
                    let Some(end) = rest.iter().position(|&b| b == b'"') else { continue };
                    let Ok(name) = std::str::from_utf8(&rest[..end]) else { continue };
                    if name.ends_with(".c") || name.ends_with(".h") {
                        continue;
                    }
                    // Beside the file that includes it, or the only one of that name.
                    let beside = p.parent().map(|d| d.join(name));
                    let base = std::ffi::OsStr::new(name.rsplit(['/', '\\']).next().unwrap_or(name));
                    let Some(same) = by_name.get(base) else { continue };
                    match beside.filter(|b| same.iter().any(|s| *s == b)) {
                        Some(b) => out.push(b),
                        None if same.len() == 1 => out.push(same[0].clone()),
                        None => {}
                    }
                }
                out
            })
            .collect();
        look_in = found.into_iter().filter(|p| taken.insert(p.clone())).collect();
    }
    files.extend(taken);
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
    if !conditions.uses_source() {
        return read(conditions);
    }
    // What each condition comes to compilation by compilation, first:
    // which definitions are compiled depends on it.
    let scripts: Vec<mask::Script> = files.par_iter().map(|p| mask::script(&std::fs::read(p).unwrap_or_default())).collect();
    let verdicts = std::sync::Arc::new(units::verdicts(&paths, &scripts, conditions).unwrap_or_default());
    drop(scripts);
    if std::env::var_os("SI_COND_PROFILE").is_some() {
        eprintln!("decided   {} conditions, by reading each compilation", verdicts.iter().map(Vec::len).sum::<usize>());
        for (file, told) in files.iter().zip(verdicts.iter()).filter(|x| !x.1.is_empty()) {
            let src = std::fs::read(file).unwrap_or_default();
            for &(at, holds) in told {
                let line = src[at as usize..].split(|&b| b == b'\n').next().unwrap_or_default();
                eprintln!("told      {} {} {}", holds, rel_path(root, file), String::from_utf8_lossy(line).trim());
            }
        }
    }
    let mut first = cond::Source::new(&paths);
    first.set_verdicts(verdicts.clone());
    if !first.is_empty() {
        conditions.set_source(Some(std::sync::Arc::new(first)));
    }
    let mut defines = read(conditions);
    for _ in 0..4 {
        let mut source = mask::source_of(&defines, &paths);
        source.set_verdicts(verdicts.clone());
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
    let sets = MacroSets::from_defines(defines.into_iter().flatten()).with_conditions(conditions.clone());
    trim_memory();
    sets
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
    let mut facts: Vec<FileFacts> = files.par_iter().map_init(extract::Extractor::new, |ex, p| extract_file(ex, root, p, transparent)).collect();
    mem_mark("extracted");
    gather(&mut facts);
    trim_memory();
    mem_mark("extracted, gathered");
    if let Some(open) = mask::OPEN.lock().unwrap().take() {
        let mut defined: rustc_hash::FxHashMap<&str, u32> = Default::default();
        for f in &facts {
            for &n in &f.defined_names {
                *defined.entry(f.name(n as usize)).or_default() += 1;
            }
        }
        let mut rows: Vec<(&[u8], u64, bool)> = open.iter().map(|(n, k)| (&n[..], k.0, std::str::from_utf8(n).is_ok_and(|n| defined.contains_key(n)))).collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        let all: u64 = rows.iter().map(|r| r.1).sum();
        for sourced in [true, false] {
            let these: Vec<_> = rows.iter().filter(|r| r.2 == sourced).collect();
            let n: u64 = these.iter().map(|r| r.1).sum();
            let top: Vec<String> = these.iter().take(40).map(|r| format!("{} {}", String::from_utf8_lossy(r.0), r.1)).collect();
            eprintln!("open      {n} of {all} conditions on {} names {}: {}", these.len(), if sourced { "the sources define" } else { "nothing here defines" }, top.join(", "));
        }
    }
    facts
}

/// Move what is kept of each file into memory this thread allocates.
///
/// The facts were made on the worker threads, each of which allocates from
/// pages of its own, between the trees, tokens and lists it made on the way
/// and has let go since. What was let go leaves holes around what is kept:
/// the pages stay, a quarter empty (0.5 GB in the kernel, on 28 threads,
/// and more with every link, whose own short-lived lists land in the holes
/// and commit what was not yet). Copied by the one thread that asks, the
/// facts lie side by side in pages that hold nothing else, and the workers'
/// pages, left empty, go back to the system at the next [`trim_memory`].
pub fn gather<'a>(facts: impl IntoIterator<Item = &'a mut FileFacts>) {
    for f in facts {
        // A clone's vectors are each as long as what is in them.
        *f = f.clone();
    }
}

/// Route tree-sitter's C allocations through mimalloc. The platform heap
/// (notably on Windows) serializes on a lock under many parser threads.
/// Hand back to the system what was freed. A phase of indexing leaves
/// behind gigabytes it no longer needs (every file's definitions, the
/// macro table, the edges before they are packed), which the allocator
/// would keep at hand: the next phase would then be counted on top of it.
/// Only does anything when the process allocates with mimalloc, as `si`
/// does; with another allocator its heap is empty.
pub fn trim_memory() {
    // Each thread has a heap of its own, and it is the worker threads that
    // did the allocating.
    rayon::broadcast(|_| unsafe { libmimalloc_sys::mi_collect(true) });
    unsafe { libmimalloc_sys::mi_collect(true) }
}

/// With `SI_MEM_PROFILE` set, say what the process holds at this point.
pub fn mem_mark(what: &str) {
    if std::env::var_os("SI_MEM_PROFILE").is_none() {
        return;
    }
    let (mut rss, mut peak, mut commit, mut zero) = (0usize, 0usize, 0usize, 0usize);
    let z: *mut usize = &mut zero;
    unsafe { libmimalloc_sys::mi_process_info(z, z, z, &mut rss, &mut peak, &mut commit, z, z) };
    eprintln!("mem       {what}: resident {} MB, committed {} MB, peak {} MB", rss >> 20, commit >> 20, peak >> 20);
    // What the allocator's pages hold: a page is of blocks of one size,
    // and stays while one of them is in use. `used` is what was asked for,
    // rounded up to the block sizes there are; the rest of what is
    // committed is holes.
    unsafe extern "C" fn page(
        _: *const libmimalloc_sys::mi_heap_t,
        area: *const libmimalloc_sys::mi_heap_area_t,
        _: *mut std::ffi::c_void,
        _: usize,
        sums: *mut std::ffi::c_void,
    ) -> bool {
        let (sums, area) = (&mut *(sums as *mut [usize; 3]), &*area);
        *sums = [sums[0] + 1, sums[1] + area.committed, sums[2] + area.used * area.full_block_size];
        true
    }
    extern "C" {
        // Every thread allocates from this one heap (the binding does not name it).
        fn mi_heap_main() -> *mut libmimalloc_sys::mi_heap_t;
    }
    let mut sums = [0usize; 3];
    unsafe { libmimalloc_sys::mi_heap_visit_blocks(mi_heap_main(), false, Some(page), &mut sums as *mut _ as *mut _) };
    eprintln!("mem         in {} pages: committed {} MB, used {} MB", sums[0], sums[1] >> 20, sums[2] >> 20);
}

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
