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
    pub graph: Graph,
    pub stats: LinkStats,
}

/// What one `update` did.
#[derive(Debug, Default)]
pub struct Update {
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
        let files = crate::scan(root);
        let defines: Vec<Vec<Define>> =
            files.par_iter().map(|p| mask::defines(&std::fs::read(p).unwrap_or_default())).collect();
        let sets = MacroSets::from_defines(defines.iter().flatten().cloned());
        let facts = crate::extract_all(root, &files, &sets);
        let mut cache = Cache::default();
        let (graph, stats) = link_cached(&facts, &options, &mut cache);
        Session { root: root.to_path_buf(), files, defines, facts, sets, cache, options, graph, stats }
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
            match (self.files.binary_search(&path), path.is_file()) {
                (Ok(i), true) => {
                    let defines = mask::defines(&std::fs::read(&path).unwrap_or_default());
                    defines_changed |= defines != self.defines[i];
                    self.defines[i] = defines;
                    up.changed += 1;
                }
                (Ok(i), false) => {
                    self.files.remove(i);
                    self.defines.remove(i);
                    self.facts.remove(i);
                    touched.remove(&path);
                    defines_changed = true;
                    up.removed += 1;
                }
                (Err(i), true) => {
                    self.files.insert(i, path.clone());
                    self.defines.insert(i, mask::defines(&std::fs::read(&path).unwrap_or_default()));
                    self.facts.insert(i, FileFacts::new(String::new()));
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
        for (i, f) in stale.into_iter().zip(fresh) {
            self.facts[i] = f;
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
