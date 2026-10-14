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
}

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
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn is_source(path: &Path) -> bool {
    matches!(path.extension().and_then(|x| x.to_str()), Some("c" | "h"))
}

impl Session {
    pub fn open(root: &Path, options: Options) -> Session {
        let scope = Scope::load(root);
        let files = crate::scan_scope(root, &scope);
        let mut conditions = crate::conditions_of(root, &files);
        let defines: Vec<Vec<Define>> = crate::settle(root, &files, &mut conditions);
        let sets = MacroSets::from_defines(defines.iter().flatten().cloned()).with_conditions(conditions.clone());
        let defines = Packed::all(defines);
        let facts = crate::extract_all(root, &files, &sets);
        let mut cache = Cache::default();
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
        self.files.binary_search(&path).ok().map(|i| &self.facts[i])
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
        let paths = self.out_of_date();
        let mut up = self.update(&paths);
        up.extract_ms = ms(t) - up.link_ms;
        up
    }

    /// Files that are not as the index has them: new ones, ones that are
    /// gone or out of scope, and ones whose content changed.
    fn out_of_date(&mut self) -> Vec<PathBuf> {
        let now = crate::scan_scope(&self.root, &self.scope);
        let mut paths: Vec<PathBuf> = now.iter().filter(|p| self.files.binary_search(p).is_err()).cloned().collect();
        let on_disk: FxHashSet<&PathBuf> = now.iter().collect();
        let this = &*self;
        let stale: Vec<Option<bool>> = (0..this.files.len())
            .into_par_iter()
            .map(|i| {
                let path = &this.files[i];
                if !on_disk.contains(path) {
                    return Some(true); // gone
                }
                if stamp(path) == this.stamps[i] {
                    return None;
                }
                // Touched: the content says whether it changed.
                let same = std::fs::read(path).is_ok_and(|src| xxhash_rust::xxh3::xxh3_64(&src) == this.facts[i].hash);
                Some(!same)
            })
            .collect();
        for (i, s) in stale.into_iter().enumerate() {
            match s {
                Some(true) => paths.push(self.files[i].clone()),
                Some(false) => {
                    self.stamps[i] = stamp(&self.files[i]);
                    if let Some(store) = &self.store {
                        store.save(&crate::rel_path(&self.root, &self.files[i]), self.stamps[i], &self.defines[i].get(), &self.facts[i]);
                    }
                }
                None => {}
            }
        }
        paths
    }

    /// Like `open`, keeping every file's facts in `dir` between runs: what
    /// was saved there is read back, and only the sources that changed
    /// since (or that a change of macros touches) are parsed again.
    pub fn open_cached(root: &Path, options: Options, dir: PathBuf) -> (Session, Update) {
        let t = Instant::now();
        let store = Store::new(dir);
        if let Some(mut session) = Session::restore(root, &options, &store) {
            let loaded = session.files.len();
            session.store = Some(store);
            // What the tree looks like now, against what was saved.
            let paths = session.out_of_date();
            let mut up = session.update(&paths);
            up.loaded = loaded;
            up.extract_ms = ms(t) - up.link_ms;
            return (session, up);
        }
        // Nothing usable saved: index everything, and save it.
        let mut session = Session::open(root, options);
        store.invalidate();
        let s = &session;
        (0..s.files.len()).into_par_iter().for_each(|i| {
            store.save(&crate::rel_path(root, &s.files[i]), s.stamps[i], &s.defines[i].get(), &s.facts[i]);
        });
        session.store = Some(store);
        session.save_manifest();
        session.save_expansions();
        let up = Update { extracted: session.files.len(), extract_ms: ms(t), ..Update::default() };
        (session, up)
    }

    /// The session as it was last saved, not yet linked; `None` if the save
    /// is missing, from another version, incomplete or inconsistent.
    fn restore(root: &Path, options: &Options, store: &Store) -> Option<Session> {
        let manifest = store.manifest()?;
        let saved: Vec<Option<crate::store::Saved>> = manifest.files.par_iter().map(|rel| store.load(rel)).collect();
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
        // The facts were all extracted under these sets, or the save was
        // interrupted and cannot be trusted.
        // (The conditions are part of the sets: a save made under other
        // conditions does not match and is rebuilt.)
        let mut conditions = Session::conditions_now(root, &facts);
        if conditions.uses_source() {
            let paths: Vec<String> = files.iter().map(|p| crate::rel_path(root, p)).collect();
            let source = mask::source_of(&defines, &paths);
            if !source.is_empty() {
                conditions.set_source(Some(std::sync::Arc::new(source)));
            }
        }
        let sets = MacroSets::from_defines(defines.iter().flatten().cloned()).with_conditions(conditions.clone());
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
        Some(Session {
            root: root.to_path_buf(),
            files,
            defines: Packed::all(defines),
            facts,
            sets,
            cache,
            options: options.clone(),
            store: None,
            stamps,
            scope: Scope::load(root),
            conditions,
            all_stale: false,
            graph: Graph::default(),
            stats: LinkStats::default(),
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
            store.save_manifest(self.sets.digest(), files);
        }
    }

    /// Where facts are kept between runs, if they are.
    pub fn cache_dir(&self) -> Option<&Path> {
        self.store.as_ref().map(|s| s.dir())
    }

    pub fn files(&self) -> usize {
        self.files.len()
    }

    /// Bring the index up to date with what is on disk at `paths` (absolute,
    /// or relative to the root): files edited, created or deleted.
    pub fn update(&mut self, paths: &[PathBuf]) -> Update {
        let mut up = Update::default();
        let t = Instant::now();
        let mut touched: FxHashSet<PathBuf> = FxHashSet::default();
        // Most edits leave every `#define` as it was, and the macro sets with them.
        let mut defines_changed = false;
        for p in paths {
            let path = if p.is_absolute() { p.clone() } else { self.root.join(p) };
            // (A file of another kind is in the index when a source includes it.)
            if !(is_source(&path) || self.files.binary_search(&path).is_ok()) || !touched.insert(path.clone()) {
                continue;
            }
            // A file out of the project's scope is as good as not there.
            let there = path.is_file() && self.scope.contains(&crate::rel_path(&self.root, &path));
            match (self.files.binary_search(&path), there) {
                (Ok(i), true) => {
                    let defines = Packed::of(&mask::defines(&std::fs::read(&path).unwrap_or_default(), &self.conditions));
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
            let sets = MacroSets::from_defines(self.defines.iter().flat_map(Packed::get)).with_conditions(self.conditions.clone());
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
            self.save_manifest();
        }
        up.extract_ms = ms(t);

        let t = Instant::now();
        // The graph that was goes first: nobody can ask it anything while
        // this runs, and the link would make the new one beside it.
        self.graph = Graph::default();
        (self.graph, self.stats) = link_cached(&self.facts, &self.options, &mut self.cache);
        up.link_ms = ms(t);
        self.save_expansions();
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
            std::fs::write(self.0.join(name), text).unwrap();
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
        // Configured from a file: only the branch that is compiled.
        let settings = Settings { condition_files: vec!["dot-config".into()], ..Settings::default() };
        let up = s.set_conditions(&settings).unwrap();
        assert_eq!((up.extracted, s.conditions_known()), (1, 3));
        assert_eq!((functions(&s, "net_init"), functions(&s, "net_stub"), functions(&s, "helper")), (1, 0, 1));
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
        // Tested above the include that brings the switch in: not known yet, so kept.
        assert_eq!(functions(&s, "before"), 1);
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
        assert_eq!(s.update(&[b.clone()]).added, 1);
        assert_eq!((s.files(), functions(&s, "b")), (2, 1));
        let calls: Vec<&str> = s.graph.find("a").iter().flat_map(|&x| s.graph.refs_from(x)).map(|e| s.graph.sym_name(e.other)).collect();
        assert_eq!(calls, ["b"]);
        std::fs::remove_file(d.0.join("b.c")).unwrap();
        assert_eq!(s.update(&[b]).removed, 1);
        assert_eq!((s.files(), functions(&s, "b")), (1, 0));
    }
}
