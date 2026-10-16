//! A live index: every file's facts kept in memory and brought up to date
//! file by file, the way an editor needs it.
//!
//! An edit re-extracts the file and relinks. Two things can make more than
//! that necessary, and both are handled here rather than left stale:
//!
//! * the edit changes which macros are transparent or expanding (a
//!   `#define` appeared, went, or changed character): every file that
//!   mentions such a name was extracted under the old view and is extracted
//!   again;
//! * the edit changes a macro body or an `#include`: expansions elsewhere
//!   depend on it, and the expansion cache notices by itself.

use crate::cond::{Conditions, Settings};
use crate::expand::Cache;
use crate::extract::Extractor;
use crate::graph::Graph;
use crate::link::{link_cached, LinkStats, Options};
use crate::mask::{self, Define, MacroSets};
use crate::model::FileFacts;
use crate::scope::Scope;
use crate::store::{stamp, Stamp, Store};
use bincode::Options as _;
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Session {
    root: PathBuf,
    /// Sorted, like `scan` gives them; the other vectors run parallel.
    files: Vec<PathBuf>,
    defines: Vec<Packed>,
    facts: Vec<FileFacts>,
    sets: MacroSets,
    cache: Cache,
    options: Options,
    /// Where facts are kept between runs, and each file's size and time
    /// when its facts were made.
    store: Option<Store>,
    stamps: Vec<Stamp>,
    /// Which part of the tree is the project (`.si/config.json`).
    scope: Scope,
    /// What the build defines, from the same file: branches it does not
    /// compile are not indexed.
    conditions: Conditions,
    /// Set when the conditions changed: every file is then read again.
    all_stale: bool,
    pub graph: Graph,
    pub stats: LinkStats,
    /// Files whose facts are still to be saved, after an index from
    /// nothing: saving is done a few at a time when nothing else is
    /// asked (`save_some`), the manifest last.
    unsaved: Vec<PathBuf>,
    unsaved_manifest: bool,
    /// The graph is not the one saved.
    graph_unsaved: bool,
    /// What the saved graph was made of (`state_digest`), for a session
    /// that is only that graph (`early`); else 0.
    early: u64,
}

/// A graph larger than this is saved when the index is first made and
/// when the session ends, not after every change.
const SMALL_GRAPH: usize = 256 << 20;

/// A file's `#define`s as the session keeps them: in bytes, as they are
/// saved (but with every length and line in as few bytes as it needs).
///
/// They are only ever wanted a file at a time, all of them: to see whether
/// an edit changed any, to make the macro sets again when one did, to save.
/// As they are worked with (a list for the name, one for each name the
/// body refers to) the kernel's take 1.4 GB in millions of small blocks,
/// several times what they say.
#[derive(PartialEq)]
struct Packed(Box<[u8]>);

impl Packed {
    fn of(defines: &[Define]) -> Packed {
        Packed(bincode::options().serialize(defines).unwrap_or_default().into())
    }

    fn get(&self) -> Vec<Define> {
        bincode::options().deserialize(&self.0).unwrap_or_default()
    }

    /// Those of every file. Packed by all threads, then moved into memory
    /// of this one's (see `crate::gather`).
    fn all(defines: Vec<Vec<Define>>) -> Vec<Packed> {
        let packed: Vec<Packed> = defines.par_iter().map(|d| Packed::of(d)).collect();
        drop(defines);
        packed.into_iter().map(|p| Packed(p.0.clone())).collect()
    }
}

/// What one `update` did.
#[derive(Debug, Default)]
pub struct Update {
    /// Files whose facts were read from the last run's save (at opening).
    pub loaded: usize,
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    /// Files extracted again: the changed ones, and those that mention a
    /// macro whose kind changed.
    pub extracted: usize,
    /// Macros that became or stopped being transparent or expanding.
    pub macros_reclassified: usize,
    pub extract_ms: f64,
    pub link_ms: f64,
    /// At opening: the graph the caller holds is this session's (see
    /// `Session::open_deferred`).
    pub graph_kept: bool,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn is_source(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(crate::is_source)
}

impl Session {
    pub fn open(root: &Path, options: Options) -> Session {
        let scope = Scope::load(root);
        let files = crate::scan_scope(root, &scope);
        let mut conditions = crate::conditions_of(root, &files);
        let defines: Vec<Vec<Define>> = crate::settle(root, &files, &mut conditions);
        let paths: Vec<String> = files.iter().map(|p| crate::rel_path(root, p)).collect();
        let sets = MacroSets::of_files(&defines, &paths).with_conditions(conditions.clone());
        drop(paths);
        let defines = Packed::all(defines);
        let facts = crate::extract_all(root, &files, &sets);
        let mut cache = Cache::default();
        let options = Options { readings: crate::readings_of(&conditions), ..options };
        let (graph, stats) = link_cached(&facts, &options, &mut cache);
        let stamps = files.par_iter().map(|p| stamp(p)).collect();
        Session {
            root: root.to_path_buf(),
            files,
            defines,
            facts,
            sets,
            cache,
            options,
            store: None,
            stamps,
            scope,
            conditions,
            all_stale: false,
            graph,
            stats,
            unsaved: Vec::new(),
            unsaved_manifest: false,
            graph_unsaved: false,
            early: 0,
        }
    }

    /// The conditions the tree's settings give now, with the names the
    /// indexed files define (as the facts have them).
    fn conditions_now(root: &Path, facts: &[FileFacts]) -> Conditions {
        let mut conditions = Conditions::load(root);
        if conditions.assumes() {
            let names = facts.iter().flat_map(|f| f.defined_names.iter().map(|&n| f.name(n as usize).as_bytes().to_vec()));
            conditions.set_in_source(names.collect());
        }
        conditions
    }

    /// The compile conditions as written in the tree.
    pub fn condition_settings(&self) -> Settings {
        Settings::load(&self.root)
    }

    /// Replace every set of conditions the project has, and which is in
    /// force; index again if that changes anything.
    pub fn set_condition_project(&mut self, project: &crate::cond::Project) -> std::io::Result<Update> {
        project.save(&self.root)?;
        Ok(self.sync())
    }

    /// The macros whose value the sources settle, each with the number of
    /// files it is known in; empty when the settings turn that off.
    pub fn source_defines(&self) -> Vec<(String, String, usize)> {
        self.conditions.source().map(|s| s.summary()).unwrap_or_default()
    }

    /// The facts of the file indexed as `rel` (its own, before macro
    /// expansion): what is known of a file that is no part of the graph,
    /// such as its functions' parameters and local variables.
    pub fn facts_of(&self, rel: &str) -> Option<&FileFacts> {
        let path = self.root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        self.files.binary_search(&path).ok().and_then(|i| self.facts.get(i))
    }

    /// What the settings and the build say is defined.
    pub fn conditions(&self) -> &Conditions {
        &self.conditions
    }

    /// How many macros the conditions settle, one way or the other.
    pub fn conditions_known(&self) -> usize {
        self.conditions.len()
    }

    /// Make these the project's compile conditions, save them in the tree,
    /// and index again under them.
    pub fn set_conditions(&mut self, settings: &Settings) -> std::io::Result<Update> {
        settings.save(&self.root)?;
        Ok(self.sync())
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Make `scope` the project's, save it in the tree, and bring the index
    /// in line with it.
    pub fn set_scope(&mut self, scope: Scope) -> std::io::Result<Update> {
        scope.save(&self.root)?;
        Ok(self.sync())
    }

    /// Bring the index in line with what is on disk now: the scope as
    /// saved in the tree, and every file in it that was added, removed or
    /// changed, however that happened.
    pub fn sync(&mut self) -> Update {
        let t = Instant::now();
        self.scope = Scope::load(&self.root);
        // Other conditions make other facts of every file.
        // (What the sources define is settled again in `update`.)
        let mut conditions = Session::conditions_now(&self.root, &self.facts);
        conditions.set_source(self.conditions.source().cloned());
        if conditions != self.conditions {
            self.conditions = conditions;
            self.all_stale = true;
        }
        let (paths, listed) = self.out_of_date();
        let mut up = self.update_listed(&paths, Some(&listed));
        up.extract_ms = ms(t) - up.link_ms;
        up
    }

    /// Files that are not as the index has them: new ones, ones that are
    /// gone or out of scope, and ones whose content changed; and of them,
    /// those the walk of the tree found to be in the project.
    fn out_of_date(&mut self) -> (Vec<PathBuf>, FxHashSet<PathBuf>) {
        let this = &*self;
        // What a file as it was when indexed includes is in its facts: it
        // is not read again to find out.
        let known = |path: &Path, at: Stamp| -> Option<Vec<String>> {
            let i = this.files.binary_search_by(|f| f.as_path().cmp(path)).ok()?;
            let facts = &this.facts[i];
            (this.stamps[i] == at).then(|| facts.includes.iter().map(|&n| facts.name(n as usize).to_string()).collect())
        };
        let now = crate::scan_scope_with(&this.root, &this.scope, &known);
        // Both in order: what the walk saw of each indexed file (nothing,
        // if it is gone), and the files that are new.
        let mut seen: Vec<Option<Stamp>> = vec![None; this.files.len()];
        let mut paths = Vec::new();
        let mut j = 0;
        for (path, at) in now {
            while j < this.files.len() && this.files[j] < path {
                j += 1;
            }
            if this.files.get(j) == Some(&path) {
                seen[j] = Some(at);
            } else {
                paths.push(path);
            }
        }
        let mut listed: FxHashSet<PathBuf> = paths.iter().cloned().collect();
        let stale: Vec<Option<bool>> = (0..this.files.len())
            .into_par_iter()
            .map(|i| match seen[i] {
                None => Some(true), // gone
                Some(at) if at == this.stamps[i] => None,
                // (The walk sees a link, the index what it leads to.)
                Some(_) if stamp(&this.files[i]) == this.stamps[i] => None,
                Some(_) => {
                    // Touched: the content says whether it changed.
                    let same = std::fs::read(&this.files[i]).is_ok_and(|src| xxhash_rust::xxh3::xxh3_64(&src) == this.facts[i].hash);
                    Some(!same)
                }
            })
            .collect();
        for (i, s) in stale.into_iter().enumerate() {
            match s {
                Some(true) => {
                    if seen[i].is_some() {
                        listed.insert(self.files[i].clone());
                    }
                    paths.push(self.files[i].clone());
                }
                Some(false) => {
                    self.stamps[i] = stamp(&self.files[i]);
                    if let Some(store) = &self.store {
                        store.save(&crate::rel_path(&self.root, &self.files[i]), self.stamps[i], &self.defines[i].get(), &self.facts[i]);
                    }
                }
                None => {}
            }
        }
        (paths, listed)
    }

    /// Like `open`, keeping every file's facts in `dir` between runs: what
    /// was saved there is read back, and only the sources that changed
    /// since (or that a change of macros touches) are parsed again.
    pub fn open_cached(root: &Path, options: Options, dir: PathBuf) -> (Session, Update) {
        let (mut session, up) = Session::open_deferred(root, options, dir, None);
        while session.save_some(usize::MAX) {}
        (session, up)
    }

    /// The same, with the saving left for later (`save_some`): the index
    /// is of use before it is saved. `have` is the digest of a saved
    /// graph the caller already holds (see `early`): if it is still the
    /// graph of the tree, none is made or read here, and the update says
    /// so (`graph_kept`) for the caller to hand its own over (`adopt`).
    pub fn open_deferred(root: &Path, options: Options, dir: PathBuf, have: Option<u64>) -> (Session, Update) {
        let t = Instant::now();
        let profile = |what: &str| {
            if std::env::var_os("SI_START_PROFILE").is_some() {
                eprintln!("start     {what}: {:.0} ms", ms(t));
            }
        };
        let store = Store::new(dir);
        if let Some(mut session) = Session::restore(root, &options, &store) {
            let loaded = session.files.len();
            // What the tree looks like now, against what was saved.
            session.store = Some(store);
            let (paths, listed) = session.out_of_date();
            profile("tree checked");
            // Nothing changed, and the graph that was saved is the graph
            // of these very facts: linking would only make it again.
            let digest = session.state_digest();
            if paths.is_empty() {
                let mut up = Update { loaded, extract_ms: ms(t), ..Update::default() };
                if have == Some(digest) {
                    up.graph_kept = true;
                    profile("graph kept");
                    return (session, up);
                }
                let saved = session.store.as_ref().and_then(|s| s.load_graph()).filter(|g| g.0 == digest);
                if let Some((_, graph, binds)) = saved {
                    session.graph = graph;
                    session.stats.bind_sites = binds;
                    profile("graph read");
                    return (session, up);
                }
            }
            let mut up = session.update_listed(&paths, Some(&listed));
            profile("linked");
            up.loaded = loaded;
            up.extract_ms = ms(t) - up.link_ms;
            return (session, up);
        }
        // Nothing usable saved: index everything; it is saved afterwards.
        let mut session = Session::open(root, options);
        store.invalidate();
        session.unsaved = session.files.clone();
        session.unsaved_manifest = true;
        session.graph_unsaved = true;
        session.store = Some(store);
        let up = Update { extracted: session.files.len(), extract_ms: ms(t), ..Update::default() };
        (session, up)
    }

    /// The graph the last session saved, and nothing else: enough to
    /// answer what is where and who calls whom while the index itself is
    /// read and checked against the tree (`open_deferred`), which takes
    /// far longer in a large one. It is the graph as it was saved: files
    /// changed since are as they were. `None` if there is none saved.
    pub fn early(root: &Path, dir: PathBuf) -> Option<Session> {
        let store = Store::new(dir);
        let manifest = store.manifest()?;
        let (digest, graph, binds) = store.load_graph()?;
        let mut session = Session::empty(root);
        session.files = manifest.files.iter().map(|rel| root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))).collect();
        session.graph = graph;
        session.stats.bind_sites = binds;
        session.early = digest;
        Some(session)
    }

    /// A session of nothing, to stand in until there is one.
    pub fn empty(root: &Path) -> Session {
        Session {
            root: root.to_path_buf(),
            files: Vec::new(),
            defines: Vec::new(),
            facts: Vec::new(),
            sets: MacroSets::default(),
            cache: Cache::default(),
            options: Options::default(),
            store: None,
            stamps: Vec::new(),
            scope: Scope::load(root),
            conditions: Conditions::default(),
            all_stale: false,
            graph: Graph::default(),
            stats: LinkStats::default(),
            unsaved: Vec::new(),
            unsaved_manifest: false,
            graph_unsaved: false,
            early: 1,
        }
    }

    /// Set for a session that is only a saved graph, or nothing at all:
    /// it answers, and is not to be changed.
    pub fn is_early(&self) -> bool {
        self.early != 0
    }

    /// The digest of the saved graph an early session holds.
    pub fn early_digest(&self) -> Option<u64> {
        (self.early > 1).then_some(self.early)
    }

    /// Take over the graph of `early`, which is this session's too
    /// (`Update::graph_kept`).
    pub fn adopt(&mut self, early: &mut Session) {
        self.graph = std::mem::take(&mut early.graph);
        self.stats.bind_sites = std::mem::take(&mut early.stats.bind_sites);
    }

    /// What the graph is made of: the sets every file was read under,
    /// and each file's path and content.
    fn state_digest(&self) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed as mix;
        let mut h = self.sets.digest() ^ self.files.len() as u64;
        for (from, to) in &self.options.include_aliases {
            h = mix(to.as_bytes(), mix(from.as_bytes(), h));
        }
        for (path, facts) in self.files.iter().zip(&self.facts) {
            h = mix(facts.path.as_bytes(), h ^ facts.hash);
            debug_assert!(path.ends_with(std::path::Path::new(&facts.path.replace('/', std::path::MAIN_SEPARATOR_STR))));
        }
        // (0 and 1 say other things in `early`.)
        h.max(2)
    }

    /// Whether anything is still to be saved.
    pub fn unsaved(&self) -> bool {
        self.store.is_some() && (!self.unsaved.is_empty() || self.unsaved_manifest || self.graph_unsaved && self.graph_due())
    }

    fn graph_due(&self) -> bool {
        // A large graph is not written again after every change.
        self.graph.heap_bytes() <= SMALL_GRAPH || self.store.as_ref().is_some_and(|s| s.graph_digest().is_none())
    }

    /// Save some of what is not saved yet: the facts of `at_most` files,
    /// or, those done, what vouches for them, or the graph. Returns
    /// whether more is left.
    pub fn save_some(&mut self, at_most: usize) -> bool {
        if self.store.is_none() {
            return false;
        }
        if !self.unsaved.is_empty() {
            let batch = self.unsaved.split_off(self.unsaved.len().saturating_sub(at_most));
            let (s, store) = (&*self, self.store.as_ref().unwrap());
            batch.par_iter().for_each(|path| {
                if let Ok(i) = s.files.binary_search(path) {
                    store.save(&crate::rel_path(&s.root, path), s.stamps[i], &s.defines[i].get(), &s.facts[i]);
                }
            });
        } else if self.unsaved_manifest {
            self.unsaved_manifest = false;
            self.save_manifest();
            self.save_expansions();
        } else if self.graph_unsaved && self.graph_due() {
            self.save_graph();
        }
        self.unsaved()
    }

    /// Save the graph, if it is not the one saved and everything it is
    /// the graph of is: at the end of a session, whatever its size.
    pub fn save_graph(&mut self) {
        if self.graph_unsaved && self.unsaved.is_empty() && !self.unsaved_manifest {
            if let Some(store) = &self.store {
                store.save_graph(self.state_digest(), &self.graph, &self.stats.bind_sites);
                self.graph_unsaved = false;
            }
        }
    }

    /// The session as it was last saved, not yet linked; `None` if the save
    /// is missing, from another version, incomplete or inconsistent.
    fn restore(root: &Path, options: &Options, store: &Store) -> Option<Session> {
        let t = Instant::now();
        let mark = |what: &str| {
            if std::env::var_os("SI_START_PROFILE").is_some() {
                eprintln!("start     {what}: {:.0} ms", ms(t));
            }
        };
        let manifest = store.manifest()?;
        mark("manifest");
        let saved: Vec<Option<crate::store::Saved>> = manifest.files.par_iter().map(|rel| store.load(rel)).collect();
        mark("facts read");
        let mut files = Vec::with_capacity(saved.len());
        let (mut defines, mut facts, mut stamps) = (Vec::new(), Vec::new(), Vec::new());
        for (rel, s) in manifest.files.iter().zip(saved) {
            let s = s?;
            files.push(root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)));
            defines.push(s.defines);
            facts.push(s.facts);
            stamps.push(s.stamp);
        }
        if !files.windows(2).all(|w| w[0] < w[1]) {
            return None;
        }
        // Read by all threads, each between the files' bytes it has let go.
        crate::gather(&mut facts);
        mark("gathered");
        // The facts were all extracted under these sets, or the save was
        // interrupted and cannot be trusted.
        // (The conditions are part of the sets: a save made under other
        // conditions does not match and is rebuilt.)
        let mut conditions = Session::conditions_now(root, &facts);
        mark("conditions");
        if conditions.uses_source() {
            let paths: Vec<String> = files.iter().map(|p| crate::rel_path(root, p)).collect();
            let mut source = mask::source_of(&defines, &paths);
            source.set_verdicts(std::sync::Arc::new(manifest.verdicts.clone()));
            source.set_readings(std::sync::Arc::new(manifest.readings.clone()));
            if !source.is_empty() {
                conditions.set_source(Some(std::sync::Arc::new(source)));
            }
            mark("source");
        }
        // Saved with the manifest; made again if not (a save from before
        // they were).
        let sets = store.load_sets(manifest.sets, &conditions).unwrap_or_else(|| {
            let sets = MacroSets::of_files(&defines, &manifest.files).with_conditions(conditions.clone());
            if sets.digest() == manifest.sets {
                store.save_sets(&sets);
            }
            sets
        });
        mark("sets");
        if sets.digest() != manifest.sets {
            return None;
        }
        // The macro expansions of the last link, where they were saved for
        // these very files: the link then only expands what changed.
        let cache = match store.expansions().filter(|x| x.hashes.len() == files.len()) {
            Some(index) => {
                let expanded = manifest
                    .files
                    .par_iter()
                    .zip(&index.has)
                    .map(|(rel, &has)| if has { store.load_expansion(rel).map(Some).ok_or(()) } else { Ok(None) })
                    .collect();
                Cache::restored(index, expanded)
            }
            None => Cache::default(),
        };
        mark("expansions read");
        Some(Session {
            root: root.to_path_buf(),
            files,
            defines: Packed::all(defines),
            facts,
            sets,
            cache,
            options: Options { readings: crate::readings_of(&conditions), ..options.clone() },
            store: None,
            stamps,
            scope: Scope::load(root),
            conditions,
            all_stale: false,
            graph: Graph::default(),
            stats: LinkStats::default(),
            unsaved: Vec::new(),
            unsaved_manifest: false,
            graph_unsaved: false,
            early: 0,
        })
    }

    /// Keep the expansions the last link made anew.
    fn save_expansions(&mut self) {
        let Some(store) = &self.store else { return };
        let changed = self.cache.take_changed();
        if changed.is_empty() {
            return;
        }
        store.invalidate_expansions();
        let (root, files, cache) = (&self.root, &self.files, &self.cache);
        changed.par_iter().for_each(|&i| {
            if let Some(path) = files.get(i) {
                store.save_expansion(&crate::rel_path(root, path), cache.expansion(i));
            }
        });
        store.save_expansions(&cache.index());
    }

    fn save_manifest(&self) {
        if let Some(store) = &self.store {
            let files = self.files.iter().map(|p| crate::rel_path(&self.root, p)).collect();
            let verdicts = self.conditions.source().map(|s| s.verdicts().as_ref().clone()).unwrap_or_default();
            store.save_sets(&self.sets);
            store.save_manifest(self.sets.digest(), files, verdicts, crate::readings_of(&self.conditions).as_ref().clone());
        }
    }

    /// Where facts are kept between runs, if they are.
    pub fn cache_dir(&self) -> Option<&Path> {
        self.store.as_ref().map(|s| s.dir())
    }

    pub fn files(&self) -> usize {
        self.files.len()
    }

    /// The folders that hold indexed headers, as absolute paths: where an
    /// `#include` can find something, for whoever has no build to ask.
    pub fn header_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for file in self.files.iter().filter(|f| f.extension().is_some_and(|x| x == "h")) {
            if let Some(dir) = file.parent().filter(|d| dirs.last().map(PathBuf::as_path) != Some(*d)) {
                dirs.push(dir.to_path_buf());
            }
        }
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Bring the index up to date with what is on disk at `paths` (absolute,
    /// or relative to the root): files edited, created or deleted.
    pub fn update(&mut self, paths: &[PathBuf]) -> Update {
        self.update_listed(paths, None)
    }

    /// The same, a walk of the tree having found which of `paths` are in
    /// the project (`listed`): a file of another kind comes and goes with
    /// the sources that include it.
    fn update_listed(&mut self, paths: &[PathBuf], listed: Option<&FxHashSet<PathBuf>>) -> Update {
        let mut up = Update::default();
        let t = Instant::now();
        let mut touched: FxHashSet<PathBuf> = FxHashSet::default();
        // Most edits leave every `#define` as it was, and the macro sets with them.
        let mut defines_changed = false;
        for p in paths {
            let path = if p.is_absolute() { p.clone() } else { self.root.join(p) };
            // (A file of another kind is in the index when a source includes it.)
            let in_list = listed.map(|l| l.contains(&path));
            if !(in_list == Some(true) || is_source(&path) || self.files.binary_search(&path).is_ok()) || !touched.insert(path.clone()) {
                continue;
            }
            // A file out of the project's scope is as good as not there.
            let there = in_list != Some(false) && path.is_file() && self.scope.contains(&crate::rel_path(&self.root, &path));
            match (self.files.binary_search(&path), there) {
                (Ok(i), true) => {
                    let src = std::fs::read(&path).unwrap_or_default();
                    // Said to have changed and the same as indexed: touched
                    // only (Windows tells of a file read as one changed, when
                    // the time it was last read is updated).
                    if xxhash_rust::xxh3::xxh3_64(&src) == self.facts[i].hash && !self.facts[i].path.is_empty() {
                        touched.remove(&path);
                        if std::mem::replace(&mut self.stamps[i], stamp(&path)) != self.stamps[i] && self.store.is_some() {
                            self.unsaved.push(path);
                        }
                        continue;
                    }
                    let defines = Packed::of(&mask::defines(&src, &self.conditions));
                    defines_changed |= defines != self.defines[i];
                    self.defines[i] = defines;
                    up.changed += 1;
                }
                (Ok(i), false) => {
                    if let Some(store) = &self.store {
                        store.remove(&crate::rel_path(&self.root, &path));
                    }
                    self.files.remove(i);
                    self.defines.remove(i);
                    self.facts.remove(i);
                    self.stamps.remove(i);
                    touched.remove(&path);
                    defines_changed = true;
                    up.removed += 1;
                }
                (Err(i), true) => {
                    self.files.insert(i, path.clone());
                    self.defines.insert(i, Packed::of(&mask::defines(&std::fs::read(&path).unwrap_or_default(), &self.conditions)));
                    self.facts.insert(i, FileFacts::new(String::new()));
                    self.stamps.insert(i, (0, 0));
                    defines_changed = true;
                    up.added += 1;
                }
                (Err(_), false) => {
                    touched.remove(&path);
                }
            }
        }

        // New conditions: which `#define`s count has to be seen again, and
        // every file read under them.
        let all_stale = std::mem::take(&mut self.all_stale);
        // What the sources settle about macros may have changed with the
        // definitions (an edit to a configuration header reaches every
        // file that includes it): the files told something else now are
        // read again.
        let mut retold: FxHashSet<PathBuf> = FxHashSet::default();
        if all_stale || defines_changed && self.conditions.uses_source() {
            let before = self.conditions.source().cloned();
            self.defines = Packed::all(crate::settle(&self.root, &self.files, &mut self.conditions));
            defines_changed = true;
            let now = self.conditions.source();
            if before.as_ref() != now {
                let digest = |s: Option<&std::sync::Arc<crate::cond::Source>>, rel: &str| s.map_or(0, |s| s.file_digest(rel));
                let root = &self.root;
                retold = self
                    .files
                    .par_iter()
                    .filter(|p| {
                        let rel = crate::rel_path(root, p);
                        digest(before.as_ref(), &rel) != digest(now, &rel)
                    })
                    .cloned()
                    .collect();
            }
        }
        // Did any macro change character? Then whoever mentions it is stale.
        let mut reclassified = FxHashSet::default();
        if defines_changed {
            let per_file: Vec<Vec<Define>> = self.defines.iter().map(Packed::get).collect();
            let paths: Vec<String> = self.files.iter().map(|p| crate::rel_path(&self.root, p)).collect();
            let sets = MacroSets::of_files(&per_file, &paths).with_conditions(self.conditions.clone());
            reclassified = self.sets.reclassified(&sets);
            self.sets = sets;
        }
        up.macros_reclassified = reclassified.len();
        let stale: Vec<usize> = (0..self.files.len())
            .into_par_iter()
            .filter(|&i| {
                let f = &self.facts[i];
                all_stale
                    || touched.contains(&self.files[i])
                    || retold.contains(&self.files[i])
                    || !reclassified.is_empty() && (0..f.n_names()).any(|n| reclassified.contains(f.name(n).as_bytes()))
            })
            .collect();
        let (root, files, sets) = (&self.root, &self.files, &self.sets);
        crate::progress::parsing(stale.len());
        let fresh: Vec<FileFacts> =
            stale.par_iter().map_init(Extractor::new, |ex, &i| crate::extract_file(ex, root, &files[i], sets)).collect();
        up.extracted = fresh.len();
        for (&i, mut f) in stale.iter().zip(fresh) {
            // Read again, perhaps under other conditions with the very
            // same text: what the link kept of the file no longer holds.
            self.cache.forget(i);
            crate::gather([&mut f]);
            self.facts[i] = f;
            self.stamps[i] = stamp(&self.files[i]);
        }
        // Keep the save in step. Until the manifest is rewritten it does
        // not vouch for anything, so an interruption costs a full index
        // and never a wrong one.
        if let Some(store) = self.store.as_ref().filter(|_| !stale.is_empty() || up.removed > 0) {
            store.invalidate();
            let s = &*self;
            stale.par_iter().for_each(|&i| {
                store.save(&crate::rel_path(&s.root, &s.files[i]), s.stamps[i], &s.defines[i].get(), &s.facts[i]);
            });
            // (While the first save of everything is still on its way,
            // the manifest waits for it.)
            if self.unsaved.is_empty() && !self.unsaved_manifest {
                self.save_manifest();
            }
        }
        up.extract_ms = ms(t);

        let t = Instant::now();
        // The graph that was goes first: nobody can ask it anything while
        // this runs, and the link would make the new one beside it.
        self.graph = Graph::default();
        self.options.readings = crate::readings_of(&self.conditions);
        (self.graph, self.stats) = link_cached(&self.facts, &self.options, &mut self.cache);
        up.link_ms = ms(t);
        self.graph_unsaved = true;
        if !self.unsaved_manifest {
            self.save_expansions();
        }
        up
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::kind;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("si-session-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            PathBuf::from(name)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn functions(s: &Session, name: &str) -> usize {
        s.graph.find(name).iter().filter(|&&x| s.graph.syms[x as usize].kind == kind::FUNCTION).count()
    }

    #[test]
    fn an_edited_macro_body_changes_the_names_made_from_it_elsewhere() {
        let d = Dir::new("body");
        d.write("a.h", "#define NAME(x) x##_v1\n");
        d.write("b.c", "#include \"a.h\"\nint NAME (run) (void) { return 0; }\n");
        let mut s = Session::open(&d.0, Options::default());
        assert_eq!((functions(&s, "run_v1"), functions(&s, "run_v2")), (1, 0));
        let a = d.write("a.h", "#define NAME(x) x##_v2\n");
        let up = s.update(&[a]);
        assert_eq!((up.changed, up.extracted), (1, 1), "only a.h is read again; b.c is re-expanded");
        assert_eq!((functions(&s, "run_v1"), functions(&s, "run_v2")), (0, 1));
    }

    #[test]
    fn a_macro_that_changes_character_has_its_users_extracted_again() {
        let d = Dir::new("kind");
        d.write("a.h", "#define TAG(x) x\n");
        d.write("b.c", "#include \"a.h\"\nint TAG (run) (void) { return 0; }\nint other (void) { return 1; }\n");
        d.write("c.c", "int unrelated (void) { return 2; }\n");
        let mut s = Session::open(&d.0, Options::default());
        assert_eq!(functions(&s, "run_x"), 0);
        // TAG now pastes: it has to be expanded where it is used.
        let a = d.write("a.h", "#define TAG(x) x##_x\n");
        let up = s.update(&[a]);
        assert!(up.macros_reclassified >= 1);
        assert_eq!(up.extracted, 2, "a.h and b.c, not c.c");
        assert_eq!((functions(&s, "run_x"), functions(&s, "other"), functions(&s, "unrelated")), (1, 1, 1));
    }

    #[test]
    fn a_saved_index_is_read_back_and_only_changes_are_parsed() {
        let d = Dir::new("saved");
        let cache = d.0.join(".cache");
        d.write("a.h", "#define TAG(x) x\n");
        d.write("b.c", "#include \"a.h\"\nint TAG (run) (void) { return other (); }\nint other (void) { return 1; }\n");
        d.write("c.c", "int unrelated (void) { return 2; }\n");
        let open = |d: &Dir| Session::open_cached(&d.0, Options::default(), cache.clone());

        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted), (0, 3), "nothing saved yet");
        let edges = s.graph.out.len();
        drop(s);

        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted), (3, 0), "all read back");
        assert_eq!((s.graph.out.len(), functions(&s, "other"), functions(&s, "unrelated")), (edges, 1, 1));
        drop(s);

        // An edit while nothing was running.
        d.write("c.c", "int unrelated (void) { return 3; }\nint added (void) { return unrelated (); }\n");
        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.changed, up.extracted), (3, 1, 1));
        assert_eq!(functions(&s, "added"), 1);
        drop(s);

        // A macro changed character while nothing was running: its users
        // are parsed again though their text is the same.
        d.write("a.h", "#define TAG(x) x##_x\n");
        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted), (3, 2), "a.h and b.c, not c.c");
        assert_eq!((functions(&s, "run_x"), functions(&s, "added")), (1, 1));
        drop(s);

        // A file deleted, another created.
        std::fs::remove_file(d.0.join("c.c")).unwrap();
        d.write("e.c", "int fresh (void) { return 0; }\n");
        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.removed, up.added, s.files()), (3, 1, 1, 3));
        assert_eq!((functions(&s, "fresh"), functions(&s, "unrelated")), (1, 0));
        drop(s);
        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted, functions(&s, "fresh")), (3, 0, 1));
    }

    #[test]
    fn a_loop_macro_does_not_lose_the_functions_after_it() {
        let d = Dir::new("loops");
        d.write(
            "loops.h",
            "#define for_each_bit(b, n) for ((b) = 0; (b) < (n); (b)++)\n#define for_each_cpu(c) for_each_bit(c, 64)\n#define skip_spaces(p) while (*(p) == ' ') ++(p)\n",
        );
        d.write(
            "a.c",
            "#include \"loops.h\"\nint g (int x) { return x; }\nint f (void)\n{\n\tint i, sum = 0;\n\tfor_each_cpu(i)\n\t\tsum += g (i);\n\treturn sum;\n}\nint h (char *p) { skip_spaces(p); return g (1); }\nint k (void) { return h (0); }\n",
        );
        let s = Session::open(&d.0, Options::default());
        assert_eq!((functions(&s, "f"), functions(&s, "h"), functions(&s, "k")), (1, 1, 1));
        let g = &s.graph;
        let gid = (0..g.syms.len() as u32).find(|&x| g.sym_name(x) == "g" && g.syms[x as usize].kind == kind::FUNCTION).unwrap();
        let mut callers: Vec<&str> = crate::callers_through_macros(g, gid).iter().map(|c| g.sym_name(c.caller)).collect();
        callers.sort_unstable();
        assert_eq!(callers, ["f", "h"]);
    }

    #[test]
    fn a_body_after_a_macro_with_no_type_before_it_is_the_function_it_makes() {
        let d = Dir::new("syscall");
        d.write(
            "sys.h",
            "#define SYSCALL_DEFINE1(name, t, a) \\\n\tstatic long __do_sys_##name(t a); \\\n\tlong __x64_sys_##name(t a) { return __do_sys_##name(a); } \\\n\tstatic long __do_sys_##name(t a)\n",
        );
        d.write("open.c", "#include \"sys.h\"\nstatic int helper (int a) { return a; }\n/* opens */\nSYSCALL_DEFINE1(open, int, flags)\n{\n\treturn helper (flags);\n}\n");
        let s = Session::open(&d.0, Options::default());
        let g = &s.graph;
        let calls = |from: &str| -> Vec<&str> {
            let f = (0..g.syms.len() as u32).find(|&x| g.sym_name(x) == from && g.syms[x as usize].flags & crate::model::flag::DECL == 0).unwrap();
            crate::callees_through_slots(g, f).iter().map(|c| g.sym_name(c.caller)).collect()
        };
        assert_eq!((calls("__x64_sys_open"), calls("__do_sys_open")), (vec!["__do_sys_open"], vec!["helper"]));
    }

    #[test]
    fn a_macro_is_only_seen_where_its_definition_can_be_read() {
        let d = Dir::new("scoped");
        for dir in ["include/linux", "tools/include/linux", "tools/prog", "drivers", "arch/parisc"] {
            std::fs::create_dir_all(d.0.join(dir)).unwrap();
        }
        d.write("include/linux/spinlock.h", "static inline void unlock (int *l, unsigned long f)\n{\n\thelper (f);\n}\n");
        d.write("drivers/a.c", "#include <linux/spinlock.h>\nvoid helper (unsigned long f) { }\nvoid user (int *l) { unlock (l, 0); }\n");
        // Another program's macro of the name, and a file's own.
        d.write("tools/include/linux/spinlock.h", "#define unlock(x, f) (void)f, pthread_mutex_unlock(x)\n");
        d.write("tools/prog/t.c", "#include <linux/spinlock.h>\nvoid t (int *m) { unlock (m, 0); }\n");
        d.write("arch/parisc/fw.c", "# define unlock(a, b)\nvoid fw (void) { unlock (0, 0); }\n");
        let s = Session::open(&d.0, Options::default());
        assert_eq!(functions(&s, "unlock"), 1, "the function in include/ is not taken for either macro");
        let g = &s.graph;
        let unlock = (0..g.syms.len() as u32).find(|&x| g.sym_name(x) == "unlock" && g.syms[x as usize].kind == kind::FUNCTION).unwrap();
        let callers: Vec<&str> = crate::callers_through_macros(g, unlock).iter().map(|c| g.sym_name(c.caller)).collect();
        assert_eq!(callers, ["user"]);
    }

    #[test]
    fn a_file_said_to_change_that_did_not_is_not_parsed_again() {
        let d = Dir::new("touched");
        let a = d.write("a.c", "int a (void) { return 0; }\n");
        let mut s = Session::open(&d.0, Options::default());
        // What Windows reports of a file that was only read.
        let up = s.update(std::slice::from_ref(&a));
        assert_eq!((up.changed, up.extracted), (0, 0));
        d.write("a.c", "int a (void) { return 1; }\nint b (void) { return a (); }\n");
        let up = s.update(&[a]);
        assert_eq!((up.changed, up.extracted, functions(&s, "b")), (1, 1, 1));
    }

    #[test]
    fn a_file_included_by_name_is_found_again_without_reading_the_sources() {
        let d = Dir::new("included");
        let cache = d.0.join(".cache");
        d.write("t.c", "#include \"rows.def\"\nint t (void) { return 0; }\n");
        d.write("rows.def", "int row (void) { return 1; }\n");
        d.write("other.def", "int not_included (void) { return 1; }\n");
        let open = |d: &Dir| Session::open_cached(&d.0, Options::default(), cache.clone());
        let (s, _) = open(&d);
        assert_eq!((s.files(), functions(&s, "row"), functions(&s, "not_included")), (2, 1, 0));
        drop(s);
        // As it was: the includes come from what was saved.
        let (s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted, s.files(), functions(&s, "row")), (2, 0, 2, 1));
        drop(s);
        // The source changed while nothing ran: read, and it includes another.
        d.write("t.c", "#include \"other.def\"\nint t (void) { return 2; }\n");
        let (s, up) = open(&d);
        assert_eq!((up.removed, up.added, up.changed), (1, 1, 1), "rows.def, other.def, t.c");
        assert_eq!((s.files(), functions(&s, "row"), functions(&s, "not_included")), (2, 0, 1));
    }

    #[test]
    fn the_scope_kept_in_the_tree_says_what_is_indexed() {
        let d = Dir::new("scope");
        std::fs::create_dir_all(d.0.join("lib")).unwrap();
        std::fs::create_dir_all(d.0.join("tests")).unwrap();
        d.write("lib/a.c", "int a (void) { return 0; }\n");
        d.write("tests/t.c", "int t (void) { return a (); }\n");
        d.write("main.c", "int main (void) { return a (); }\n");
        let cache = d.0.join("cache-outside-scan");
        let open = |d: &Dir| Session::open_cached(&d.0, Options::default(), cache.clone()).0;

        let mut s = open(&d);
        assert_eq!((s.files(), functions(&s, "t")), (3, 1));
        // Leave the tests out; the setting goes into the tree.
        let up = s.set_scope(Scope { include: vec![], exclude: vec!["tests".into()] }).unwrap();
        assert_eq!((up.removed, s.files(), functions(&s, "t"), functions(&s, "a")), (1, 2, 0, 1));
        assert!(Scope::config_path(&d.0).is_file());
        drop(s);

        // The next start finds the setting by itself.
        let mut s = open(&d);
        assert_eq!((s.files(), functions(&s, "t"), s.scope().exclude.len()), (2, 0, 1));
        // Only `lib`: a file outside it is out even when told it changed.
        s.set_scope(Scope { include: vec!["lib".into()], exclude: vec![] }).unwrap();
        assert_eq!((s.files(), functions(&s, "main")), (1, 0));
        assert_eq!(s.update(&[PathBuf::from("main.c")]).added, 0);
        // Back to everything; and a sync picks up what changed meanwhile.
        s.set_scope(Scope::default()).unwrap();
        assert_eq!(s.files(), 3);
        d.write("lib/b.c", "int b (void) { return 1; }\n");
        std::fs::remove_file(d.0.join("main.c")).unwrap();
        let up = s.sync();
        assert_eq!((up.added, up.removed, functions(&s, "b"), functions(&s, "main")), (1, 1, 1, 0));
    }

    #[test]
    fn compile_conditions_decide_which_branches_are_indexed() {
        let d = Dir::new("conditions");
        d.write("a.c", "#ifdef CONFIG_NET\nint net_init (void) { return helper (); }\n#else\nint net_stub (void) { return 0; }\n#endif\nint helper (void) { return 1; }\n");
        d.write("dot-config", "CONFIG_NET=y\n# CONFIG_USB is not set\n");
        let cache = d.0.join("cache-outside-scan");
        let open = |d: &Dir| Session::open_cached(&d.0, Options::default(), cache.clone());

        // Nothing configured: both branches, as before.
        let (mut s, _) = open(&d);
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub")), (1, 1));
        // Each only as sure as the directive that opens it.
        let facts = s.facts_of("a.c").unwrap();
        let undecided: Vec<(u32, u32, u32)> = facts.undecided.iter().map(|u| (u.0, u.1, u.2)).collect();
        assert_eq!((undecided, facts.inactive.clone()), (vec![(2, 2, 1), (4, 4, 3)], vec![]));
        // Configured from a file: only the branch that is compiled.
        let settings = Settings { condition_files: vec!["dot-config".into()], ..Settings::default() };
        let up = s.set_conditions(&settings).unwrap();
        assert_eq!((up.extracted, s.conditions_known()), (1, 3));
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub"), functions(&s, "helper")), (1, 0, 1));
        // The lines switched off are known, to find nothing from them.
        assert_eq!(s.facts_of("a.c").map(|f| (f.inactive.clone(), f.undecided.clone())), Some((vec![(4, 4)], vec![])));
        // The scope, written to the same file, leaves the conditions alone.
        s.set_scope(Scope::default()).unwrap();
        assert_eq!(s.condition_settings(), settings);
        drop(s);

        // The next start is under the same conditions, from the save.
        let (mut s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted, functions(&s, "net_stub")), (1, 0, 0));
        // Turned the other way by hand in the settings.
        let mut other = settings.clone();
        other.undefines.push("CONFIG_NET".into());
        s.set_conditions(&other).unwrap();
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub")), (0, 1));
        assert_eq!(s.facts_of("a.c").map(|f| f.inactive.clone()), Some(vec![(2, 2)]));
        drop(s);
        // Conditions changed while nothing was running: the save is rebuilt.
        Settings::default().save(&d.0).unwrap();
        let (s, up) = open(&d);
        assert_eq!((up.loaded, functions(&s, "net_init"), functions(&s, "net_stub")), (0, 1, 1));
    }

    #[test]
    fn a_switch_in_a_configuration_header_decides_branches_everywhere() {
        let d = Dir::new("source-defines");
        d.write("config.h", "#ifndef CONFIG_H\n#define CONFIG_H\n#define USE_NET 1\n#define USE_USB 0\n#endif\n");
        d.write("a.c", "#if USE_NET\nint before (void) { return 1; }\n#endif\n#include \"config.h\"\n#if USE_NET\nint net_init (void) { return 1; }\n#else\nint net_stub (void) { return 0; }\n#endif\n#if USE_USB\nint usb_init (void) { return 1; }\n#endif\n");
        let cache = d.0.join("cache-outside-scan");
        let open = |d: &Dir| Session::open_cached(&d.0, Options::default(), cache.clone());
        let (mut s, _) = open(&d);
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub"), functions(&s, "usb_init")), (1, 0, 0));
        // Tested above the include that brings the switch in: the header
        // defines it outright, so it was not defined before.
        assert_eq!(functions(&s, "before"), 0);
        // The switch is thrown in the header: the file that tests it is read again.
        d.write("config.h", "#ifndef CONFIG_H\n#define CONFIG_H\n#define USE_NET 0\n#define USE_USB 1\n#endif\n");
        let up = s.update(&[d.0.join("config.h")]);
        assert_eq!((up.extracted, functions(&s, "net_init"), functions(&s, "net_stub"), functions(&s, "usb_init")), (2, 0, 1, 1));
        drop(s);
        // From the save, the same.
        let (mut s, up) = open(&d);
        assert_eq!((up.loaded, up.extracted, functions(&s, "net_stub"), functions(&s, "usb_init")), (2, 0, 1, 1));
        // The settings overrule the sources, and can turn this off.
        let mut settings = Settings::default();
        settings.defines.insert("USE_NET".into(), "1".into());
        s.set_conditions(&settings).unwrap();
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub")), (1, 0));
        s.set_conditions(&Settings { source_defines: Some(false), ..Settings::default() }).unwrap();
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub"), functions(&s, "usb_init")), (1, 1, 1));
    }

    #[test]
    fn what_is_written_where_a_macro_does_not_use_it_is_not_called() {
        // nrfx's glue: an assertion compiled out; another kept where some
        // definition uses its argument.
        let d = Dir::new("discarded-arguments");
        d.write("glue.h", "#define NRFX_ASSERT(expression)\n#define IRQ_SET(irq, prio)\n#ifdef DEBUG\n#define CHECK(e) do { if (!(e)) halt(); } while (0)\n#else\n#define CHECK(e) ((void)0)\n#endif\n");
        d.write("a.c", "#include \"glue.h\"\nint valid (int a) { return a > 0; }\nint irq (void) { return 3; }\nvoid halt (void) {}\nvoid f (int a)\n{\n    NRFX_ASSERT(valid (a));\n    IRQ_SET(irq (), 1);\n    CHECK(valid (a));\n}\n");
        let (s, _) = Session::open_cached(&d.0, Options::default(), d.0.join("cache-outside-scan"));
        let g = &s.graph;
        let called = |name: &str| -> Vec<u32> {
            let f = g.find(name).iter().copied().find(|&x| g.syms[x as usize].kind == kind::FUNCTION).unwrap();
            let mut lines: Vec<u32> = crate::callers_through_macros(g, f).iter().filter(|c| g.sym_name(c.caller) == "f").map(|c| c.line).collect();
            lines.sort();
            lines
        };
        assert_eq!((called("valid"), called("irq")), (vec![9], vec![]));
    }

    #[test]
    fn a_header_only_a_branch_switched_off_includes_is_read_by_no_compilation() {
        let d = Dir::new("unread-headers");
        d.write("chip.h", "#if SOC == 1\n#include \"a1.h\"\n#elif SOC == 2\n#include \"a2.h\"\n#endif\n");
        d.write("a1.h", "int f1 (void);\n");
        d.write("a2.h", "int f2 (void);\n#include \"a2_more.h\"\n");
        d.write("a2_more.h", "int f2_more (void);\n");
        // Nothing includes it: somebody's, out of sight; read.
        d.write("lone.h", "int lone (void);\n");
        d.write("a.c", "#define SOC 1\n#include \"chip.h\"\n");
        let (s, _) = Session::open_cached(&d.0, Options::default(), d.0.join("cache-outside-scan"));
        let mut unread = crate::readings_of(&s.conditions).unread.clone();
        unread.sort();
        assert_eq!(unread, ["a2.h", "a2_more.h"]);
    }

    #[test]
    fn a_header_of_one_branch_does_not_see_the_macros_of_another_s() {
        // CMSIS: one header per compiler, chosen by an `#if` not known;
        // IAR's `#define __QSUB8 __iar_builtin_QSUB8` is not GCC's.
        let d = Dir::new("rival-headers");
        d.write("compiler.h", "#ifndef COMPILER_H\n#define COMPILER_H\n#if defined(__CC_ARM)\n#include \"armcc.h\"\n#elif defined(__GNUC__)\n#include \"gcc.h\"\n#elif defined(__ICCARM__)\n#include \"iccarm.h\"\n#endif\n#endif\n");
        d.write("armcc.h", "#define __QSUB8 __qsub8\n#define __INLINE __inline\n");
        d.write("iccarm.h", "#define __QSUB8 __iar_builtin_QSUB8\n#define __INLINE inline\n");
        d.write("gcc.h", "static inline unsigned __QSUB8(unsigned a, unsigned b) { return a - b; }\n");
        for c in ["a.c", "b.c", "c.c", "d.c", "e.c", "f.c", "g.c", "h.c", "i.c", "j.c", "k.c", "l.c", "m.c", "n.c", "o.c", "p.c", "q.c", "r.c", "s.c", "t.c"] {
            d.write(c, "#include \"compiler.h\"\nunsigned use (void) { return __QSUB8 (2, 1); }\n");
        }
        let (s, _) = Session::open_cached(&d.0, Options::default(), d.0.join("cache-outside-scan"));
        assert_eq!((functions(&s, "__QSUB8"), functions(&s, "__iar_builtin_QSUB8"), functions(&s, "__qsub8")), (1, 0, 0));
    }

    #[test]
    fn of_one_chip_s_definitions_the_one_read_with_the_file_is_the_value() {
        let d = Dir::new("chip-values");
        d.write("plat.h", "#if FOO\nint p1;\n#else\nint p2;\n#endif\n");
        d.write("reg_5037.h", "#define NOBLK_SQ (2)\n");
        d.write("reg_2321.h", "#define NOBLK_SQ (4)\n");
        d.write("api.h", "#include \"reg_5037.h\"\n#define Q3 (3 * NOBLK_SQ)\n");
        d.write("a.c", "#define FOO 1\n#include \"plat.h\"\n#include \"api.h\"\n");
        d.write("lpm.c", "#include \"plat.h\"\n#include \"reg_2321.h\"\n");
        let (s, _) = Session::open_cached(&d.0, Options::default(), d.0.join("cache-outside-scan"));
        let g = &s.graph;
        let root = d.0.clone();
        let mut read = |p: &str| std::fs::read(root.join(p)).ok().map(std::sync::Arc::new);
        let readings = crate::readings_of(&s.conditions);
        let q3 = g.find("Q3")[0];
        let mut values = crate::value::Values::new(g, &s.conditions, &mut read).asked_at("api.h", 2).read_with(&readings, "api.h");
        assert_eq!(values.of_macro(q3).map(|v| v.number), Some(6));
    }

    #[test]
    fn a_macro_s_value_is_worked_out_as_an_enumerator_s_is() {
        let d = Dir::new("macro-values");
        d.write("reg.h", "typedef unsigned int U32;\n#define BUF_X_BASE (((0x00400000)) + ((32) << (0x05)) + ((32) << (0x05)))\n#define BUF_X_MASK ((4) - 1)\n#define BUF_X_DATA_SIZE_LOG ((0x04))\n#define M_BUF_PTR(BUFID, OFFSET) (void *)((BUFID ## _BASE) + ((U32)(OFFSET & (BUFID ## _MASK)) << (BUFID ## _DATA_SIZE_LOG)))\n#define BUF_BASE (0x1000u)\n#define BUF_STRIDE ((U32)4)\n#define BUF_Q(n) (BUF_BASE + (n) * BUF_STRIDE)\n#define BUF_Q3 BUF_Q(3)\nenum q { Q_A, Q_B = 5, Q_C,\n  Q_D = Q_C * 2 };\n#define WITH_ENUM (Q_D + 1)\n#ifdef BIG\n#define WIDTH 64\n#else\n#define WIDTH 32\n#endif\n#define NAMED_ELSEWHERE (UNKNOWN_THING + 1)\n#define TWICE_W (WIDTH * 2)\n");
        d.write("a.c", "#include \"reg.h\"\n");
        // One per chip, each header its own: the one beside what names it.
        d.write("chip_a/regs.h", "#define REG_BASE 0x100\n#define REG_X (REG_BASE + 4)\n");
        d.write("chip_b/regs.h", "#define REG_BASE 0x200\n");
        // A type that is a macro, cast to.
        d.write("types.h", "#define U32 unsigned long\n#define u32 U32\n#define Q_SIZE ((u32)0x10 << 2)\n#define Q_END ((U32)Q_SIZE + (U32)(REG_SIZE * 2))\n#define REG_SIZE (8)\n");
        let (s, _) = Session::open_cached(&d.0, Options::default(), d.0.join("cache-outside-scan"));
        let g = &s.graph;
        let root = d.0.clone();
        let mut read = |p: &str| std::fs::read(root.join(p)).ok().map(std::sync::Arc::new);
        let conditions = Conditions::default();
        let mut values = crate::value::Values::new(g, &conditions, &mut read);
        let mut of = |name: &str| {
            let sym = g.find(name).iter().copied().find(|&x| matches!(g.syms[x as usize].kind, kind::MACRO | kind::ENUM_CONST))?;
            if g.syms[sym as usize].kind == kind::ENUM_CONST { values.enumerator(sym) } else { values.of_macro(sym).map(|v| v.number) }
        };
        assert_eq!(of("BUF_BASE"), Some(0x1000));
        assert_eq!(of("BUF_Q3"), Some(0x1000 + 12));
        assert_eq!((of("Q_A"), of("Q_C"), of("Q_D")), (Some(0), Some(6), Some(12)));
        assert_eq!(of("WITH_ENUM"), Some(13));
        assert_eq!(of("NAMED_ELSEWHERE"), None);
        // Naming a macro defined two ways: no one value.
        assert_eq!(of("TWICE_W"), None);
        assert_eq!(of("REG_X"), Some(0x104));
        assert_eq!((of("Q_SIZE"), of("Q_END")), (Some(64), Some(80)));
        // What of it is numbers, worked out; the rest as written.
        let at = g.syms[g.find("BUF_Q")[0] as usize].file;
        assert_eq!(values.folded("M_BUF_PTR(BUF_X, gRing.Used[(2)])", at).as_deref(), Some("(void *)(0x400800 + ((U32)(gRing.Used[2] & 3) << 4))"));
        assert_eq!(values.folded("BUF_X_BASE", at).as_deref(), Some("0x400800"));
        // Defined two ways, the build deciding: each its own.
        let mut widths: Vec<Option<i64>> = g.find("WIDTH").iter().map(|&w| values.of_macro(w).map(|v| v.number)).collect();
        widths.sort();
        assert_eq!(widths, [Some(32), Some(64)]);
    }

    #[test]
    fn a_call_under_the_if_a_header_is_included_under_reaches_it_for_sure() {
        // A whole file inside `#if FEATURE_EN`, its includes too: where the
        // call is compiled, the macro's header was read.
        let d = Dir::new("include-under-if");
        d.write("plat.h", "#if FOO\nint p1;\n#else\nint p2;\n#endif\n");
        d.write("ring.h", "#define M_GET(x) ((x) + 2)\n");
        // (Read first under one more `#if`, through another header; then,
        // already read, through one under `FEATURE_EN` alone.)
        d.write("ext.h", "#include \"ring.h\"\n");
        d.write("mod.h", "#include \"ring.h\"\n");
        d.write("c2.c", "#ifdef R\n#define OPT_X 1\n#endif\n");
        // (And before all that, through a header's own `#if` not known.)
        d.write("pre.h", "#if OPT_X\n#include \"ring.h\"\n#endif\n");
        d.write("a.c", "#define FOO 1\n#include \"plat.h\"\n#include \"pre.h\"\n#if FEATURE_EN\n#if OPT_X\n#include \"ext.h\"\n#endif\n#include \"mod.h\"\nint f (void) { return M_GET (1); }\n#endif\nint g (void) { return M_GET (2); }\n");
        d.write("b.c", "#include \"plat.h\"\n");
        d.write("c.c", "#ifdef R\n#define FEATURE_EN 1\n#endif\n");
        let (s, _) = Session::open_cached(&d.0, Options::default(), d.0.join("cache-outside-scan"));
        let g = &s.graph;
        let m = g.find("M_GET")[0];
        let mut callers: Vec<(&str, bool)> = crate::callers_through_macros(g, m).iter().map(|c| (g.sym_name(c.caller), !c.ambiguous)).collect();
        callers.sort();
        assert_eq!(callers, [("f", true), ("g", false)]);
    }

    #[test]
    fn parameters_and_local_variables_are_kept_with_their_uses() {
        let d = Dir::new("locals");
        d.write("a.c", "int total;\nint sum (int *values, int count)\n{\n  int i, acc = 0;\n  for (i = 0; i < count; i++)\n    acc += values[i];\n  total = acc;\n  return acc;\n}\nint other (int count) { return count; }\n");
        let s = Session::open(&d.0, Options::default());
        let f = s.facts_of("a.c").unwrap();
        let of = |name: &str| -> Vec<(u32, bool, Vec<u32>)> {
            f.locals
                .iter()
                .enumerate()
                .filter(|(_, l)| f.name(l.name as usize) == name)
                .map(|(i, l)| (l.line, l.param, f.local_uses.iter().filter(|u| u.0 == i as u32).map(|u| u.1).collect()))
                .collect()
        };
        assert_eq!(of("values"), [(2, true, vec![6])]);
        // The same name in two functions is two locals.
        assert_eq!(of("count"), [(2, true, vec![5]), (10, true, vec![10])]);
        assert_eq!(of("i"), [(4, false, vec![5, 5, 5, 6])]);
        assert_eq!(of("acc"), [(4, false, vec![6, 7, 8])]);
        // A global used in the function is the graph's, not a local.
        assert!(of("total").is_empty());
        assert_eq!(f.name(f.defs[f.locals[0].scope as usize].name as usize), "sum");
    }

    #[test]
    fn files_come_and_go() {
        let d = Dir::new("files");
        d.write("a.c", "int a (void) { return b (); }\n");
        let mut s = Session::open(&d.0, Options::default());
        assert_eq!((s.files(), functions(&s, "b")), (1, 0));
        let b = d.write("b.c", "int b (void) { return 0; }\n");
        assert_eq!(s.update(std::slice::from_ref(&b)).added, 1);
        assert_eq!((s.files(), functions(&s, "b")), (2, 1));
        let calls: Vec<&str> = s.graph.find("a").iter().flat_map(|&x| s.graph.refs_from(x)).map(|e| s.graph.sym_name(e.other)).collect();
        assert_eq!(calls, ["b"]);
        std::fs::remove_file(d.0.join("b.c")).unwrap();
        assert_eq!(s.update(&[b]).removed, 1);
        assert_eq!((s.files(), functions(&s, "b")), (1, 0));
    }
}
