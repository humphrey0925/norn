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

use crate::expand::Cache;
use crate::extract::Extractor;
use crate::graph::Graph;
use crate::link::{link_cached, LinkStats, Options};
use crate::mask::{self, Define, MacroSets};
use crate::model::FileFacts;
use crate::scope::Scope;
use crate::store::{stamp, Stamp, Store};
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Session {
    root: PathBuf,
    /// Sorted, like `scan` gives them; the other vectors run parallel.
    files: Vec<PathBuf>,
    defines: Vec<Vec<Define>>,
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
    pub graph: Graph,
    pub stats: LinkStats,
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
        let defines: Vec<Vec<Define>> =
            files.par_iter().map(|p| mask::defines(&std::fs::read(p).unwrap_or_default())).collect();
        let sets = MacroSets::from_defines(defines.iter().flatten().cloned());
        let facts = crate::extract_all(root, &files, &sets);
        let mut cache = Cache::default();
        let (graph, stats) = link_cached(&facts, &options, &mut cache);
        let stamps = files.par_iter().map(|p| stamp(p)).collect();
        Session { root: root.to_path_buf(), files, defines, facts, sets, cache, options, store: None, stamps, scope, graph, stats }
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
                        store.save(&crate::rel_path(&self.root, &self.files[i]), self.stamps[i], &self.defines[i], &self.facts[i]);
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
            store.save(&crate::rel_path(root, &s.files[i]), s.stamps[i], &s.defines[i], &s.facts[i]);
        });
        session.store = Some(store);
        session.save_manifest();
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
        // The facts were all extracted under these sets, or the save was
        // interrupted and cannot be trusted.
        let sets = MacroSets::from_defines(defines.iter().flatten().cloned());
        if sets.digest() != manifest.sets {
            return None;
        }
        Some(Session {
            root: root.to_path_buf(),
            files,
            defines,
            facts,
            sets,
            cache: Cache::default(),
            options: options.clone(),
            store: None,
            stamps,
            scope: Scope::load(root),
            graph: Graph::default(),
            stats: LinkStats::default(),
        })
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
            if !is_source(&path) || !touched.insert(path.clone()) {
                continue;
            }
            // A file out of the project's scope is as good as not there.
            let there = path.is_file() && self.scope.contains(&crate::rel_path(&self.root, &path));
            match (self.files.binary_search(&path), there) {
                (Ok(i), true) => {
                    let defines = mask::defines(&std::fs::read(&path).unwrap_or_default());
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
                    self.defines.insert(i, mask::defines(&std::fs::read(&path).unwrap_or_default()));
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

        // Did any macro change character? Then whoever mentions it is stale.
        let mut reclassified = FxHashSet::default();
        if defines_changed {
            let sets = MacroSets::from_defines(self.defines.iter().flatten().cloned());
            reclassified = self.sets.reclassified(&sets);
            self.sets = sets;
        }
        up.macros_reclassified = reclassified.len();
        let stale: Vec<usize> = (0..self.files.len())
            .into_par_iter()
            .filter(|&i| {
                let f = &self.facts[i];
                touched.contains(&self.files[i])
                    || !reclassified.is_empty() && (0..f.n_names()).any(|n| reclassified.contains(f.name(n).as_bytes()))
            })
            .collect();
        let (root, files, sets) = (&self.root, &self.files, &self.sets);
        let fresh: Vec<FileFacts> =
            stale.par_iter().map_init(Extractor::new, |ex, &i| crate::extract_file(ex, root, &files[i], sets)).collect();
        up.extracted = fresh.len();
        for (&i, f) in stale.iter().zip(fresh) {
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
                store.save(&crate::rel_path(&s.root, &s.files[i]), s.stamps[i], &s.defines[i], &s.facts[i]);
            });
            self.save_manifest();
        }
        up.extract_ms = ms(t);

        let t = Instant::now();
        (self.graph, self.stats) = link_cached(&self.facts, &self.options, &mut self.cache);
        up.link_ms = ms(t);
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
