//! The facts of every file, kept on disk between runs.
//!
//! Parsing is what indexing costs; the link over ready facts is quick. So
//! what is saved is each file's facts, one small file per source file, and a
//! start reads those back and parses only the sources that changed since.
//! The graph is linked afresh each time; what the link spends most of its
//! time on in a tree full of macros, expanding them, is kept as well (each
//! file's expansion beside its facts), so that a start does not do it over.
//!
//! A saved file is trusted when the source's size and modification time are
//! what they were; when they are not, the content hash decides. Facts also
//! depend on the corpus-wide macro sets; the `#define`s each file contributes
//! are saved alongside, so the sets of the last run can be rebuilt and
//! compared with today's (see `Session::open_cached`).

use crate::mask::Define;
use crate::model::FileFacts;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped whenever what extraction records, or how it is laid out, changes:
/// facts written by another version are not read.
const FORMAT: u32 = 47;

/// Size and modification time of a source file when its facts were made.
pub type Stamp = (u64, u64);

pub fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).map_or((0, 0), |m| stamp_of(&m))
}

pub fn stamp_of(m: &std::fs::Metadata) -> Stamp {
    let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    (m.len(), modified.map_or(0, |d| d.as_nanos() as u64))
}

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub stamp: Stamp,
    pub defines: Vec<Define>,
    pub facts: FileFacts,
}

#[derive(Serialize)]
struct SavedRef<'a> {
    stamp: Stamp,
    defines: &'a [Define],
    facts: &'a FileFacts,
}

/// What a complete, consistent save consists of.
#[derive(Serialize, Deserialize)]
pub struct Manifest {
    format: u32,
    /// Digest of the macro sets every saved file was extracted under.
    pub sets: u64,
    /// The files, by path relative to the root, in order.
    pub files: Vec<String>,
    /// What reading the compilations decided of each file's conditions
    /// (see `units`): part of what the files were extracted under.
    #[serde(default)]
    pub verdicts: crate::units::Verdicts,
    /// How each compilation reads the headers compiled more than one way.
    #[serde(default)]
    pub readings: crate::units::Readings,
    /// Each file's `FileFacts::lex_key`, in the order of `files`: what
    /// another root looks up before it reads any file's facts.
    #[serde(default)]
    pub keys: Vec<u64>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    /// Where the facts of the tree at `root` go when nothing else is said:
    /// `.si/cache` in the tree itself, beside the project's settings.
    pub fn default_dir(root: &Path) -> PathBuf {
        crate::scope::home(root).join("cache")
    }

    /// Make the directory, and if it is a `cache` folder under `.si`, tell
    /// git to leave it alone: the settings beside it are for sharing, the
    /// cache is not.
    fn prepare(&self) {
        let _ = std::fs::create_dir_all(&self.dir);
        let parent = self.dir.parent().filter(|p| p.file_name().is_some_and(|n| n == ".norn" || n == ".si"));
        if let (Some(parent), Some(name)) = (parent, self.dir.file_name().and_then(|n| n.to_str())) {
            let ignore = parent.join(".gitignore");
            if !ignore.exists() {
                let _ = std::fs::write(ignore, format!("# The index Norn keeps here is rebuilt from the sources.\n/{name}/\n"));
            }
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, rel: &str) -> PathBuf {
        let key = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(rel.as_bytes()));
        self.dir.join(&key[..2]).join(&key[2..])
    }

    pub fn manifest(&self) -> Option<Manifest> {
        let m: Manifest = bincode::deserialize(&std::fs::read(self.dir.join("manifest")).ok()?).ok()?;
        (m.format == FORMAT).then_some(m)
    }

    pub fn save_manifest(&self, sets: u64, files: Vec<String>, verdicts: crate::units::Verdicts, readings: crate::units::Readings, keys: Vec<u64>) {
        self.prepare();
        if let Ok(bytes) = bincode::serialize(&Manifest { format: FORMAT, sets, files, verdicts, readings, keys }) {
            // Written aside and moved into place: a start never reads half of it.
            let tmp = self.dir.join("manifest.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("manifest"));
            }
        }
    }

    /// The macro sets the saved files were extracted under, as they were
    /// made from their `#define`s: written before the manifest that has
    /// their digest, which vouches for them. Making them again from every
    /// file's defines is most of what a start of a large tree costs.
    pub fn save_sets(&self, sets: &crate::mask::MacroSets) {
        self.prepare();
        if let Ok(bytes) = bincode::serialize(&(FORMAT, sets.digest(), sets)) {
            let tmp = self.dir.join("sets.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("sets"));
            }
        }
    }

    /// The saved macro sets, if they are the ones of digest `sets` once
    /// given `conditions` (which are not saved with them).
    pub fn load_sets(&self, sets: u64, conditions: &crate::cond::Conditions) -> Option<crate::mask::MacroSets> {
        let (format, digest, saved): (u32, u64, crate::mask::MacroSets) = bincode::deserialize(&std::fs::read(self.dir.join("sets")).ok()?).ok()?;
        let saved = saved.with_conditions(conditions.clone());
        (format == FORMAT && digest == sets && saved.digest() == sets).then_some(saved)
    }

    /// The graph, with what it is the graph of (a digest the session
    /// makes) and where functions are stored in members and variables,
    /// which the link finds and the graph does not hold.
    pub fn save_graph(&self, digest: u64, graph: &crate::graph::Graph, binds: &[[u32; 4]]) {
        self.prepare();
        let tmp = self.dir.join("graph.tmp");
        let write = || -> std::io::Result<()> {
            let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
            use std::io::Write;
            w.write_all(&(FORMAT as u64).to_le_bytes())?;
            w.write_all(&digest.to_le_bytes())?;
            crate::graph::write_vec(&mut w, binds)?;
            graph.write_tables(&mut w)?;
            crate::graph::write_vec(&mut w, &graph.out_off)?;
            crate::graph::write_vec(&mut w, &graph.out)?;
            crate::graph::write_vec(&mut w, &graph.in_off)?;
            crate::graph::write_vec(&mut w, &graph.inc)?;
            w.flush()
        };
        if write().is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join("graph"));
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// The digest of the saved graph, if there is one of this version.
    pub fn graph_digest(&self) -> Option<u64> {
        use std::io::Read;
        let mut head = [0u8; 16];
        std::fs::File::open(self.dir.join("graph")).ok()?.read_exact(&mut head).ok()?;
        (u64::from_le_bytes(head[..8].try_into().unwrap()) == FORMAT as u64).then(|| u64::from_le_bytes(head[8..].try_into().unwrap()))
    }

    pub fn load_graph(&self) -> Option<(u64, crate::graph::Graph, Vec<[u32; 4]>)> {
        use std::io::Read;
        let mut r = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(self.dir.join("graph")).ok()?);
        let mut head = [0u8; 16];
        r.read_exact(&mut head).ok()?;
        if u64::from_le_bytes(head[..8].try_into().unwrap()) != FORMAT as u64 {
            return None;
        }
        let binds = crate::graph::read_vec(&mut r).ok()?;
        let mut graph = crate::graph::Graph::read_tables(&mut r).ok()?;
        graph.out_off = crate::graph::read_vec(&mut r).ok()?;
        graph.out = crate::graph::read_vec(&mut r).ok()?;
        graph.in_off = crate::graph::read_vec(&mut r).ok()?;
        graph.inc = crate::graph::read_vec(&mut r).ok()?;
        Some((u64::from_le_bytes(head[8..].try_into().unwrap()), graph, binds))
    }

    /// No manifest: whatever is saved is no longer vouched for.
    pub fn invalidate(&self) {
        let _ = std::fs::remove_file(self.dir.join("manifest"));
    }

    pub fn load(&self, rel: &str) -> Option<Saved> {
        bincode::deserialize(&std::fs::read(self.file(rel)).ok()?).ok()
    }

    pub fn save(&self, rel: &str, stamp: Stamp, defines: &[Define], facts: &FileFacts) {
        let path = self.file(rel);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = bincode::serialize(&SavedRef { stamp, defines, facts }) {
            let _ = std::fs::write(path, bytes);
        }
    }

    pub fn remove(&self, rel: &str) {
        let _ = std::fs::remove_file(self.file(rel));
        let _ = std::fs::remove_file(self.expansion_file(rel));
    }

    fn expansion_file(&self, rel: &str) -> PathBuf {
        let mut path = self.file(rel).into_os_string();
        path.push(".x");
        path.into()
    }

    /// A file's macro expansion (see `expand::Cache`), or that it has none.
    pub fn save_expansion(&self, rel: &str, expanded: Option<&FileFacts>) {
        let path = self.expansion_file(rel);
        match expanded.and_then(|f| bincode::serialize(f).ok()) {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, bytes);
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    pub fn load_expansion(&self, rel: &str) -> Option<FileFacts> {
        bincode::deserialize(&std::fs::read(self.expansion_file(rel)).ok()?).ok()
    }

    /// The one-line-per-file part of the expansions, written after them:
    /// what it lists is on disk.
    pub fn save_expansions(&self, index: &crate::expand::CacheIndex) {
        self.prepare();
        if let Ok(bytes) = bincode::serialize(&(FORMAT, index)) {
            let tmp = self.dir.join("expansions.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("expansions"));
            }
        }
    }

    pub fn expansions(&self) -> Option<crate::expand::CacheIndex> {
        let (format, index): (u32, crate::expand::CacheIndex) = bincode::deserialize(&std::fs::read(self.dir.join("expansions")).ok()?).ok()?;
        (format == FORMAT).then_some(index)
    }

    /// Before expansions are written over: the list no longer vouches for them.
    pub fn invalidate_expansions(&self) {
        let _ = std::fs::remove_file(self.dir.join("expansions"));
    }
}

/// The indexes of other roots this one's facts may be taken from: an inner
/// folder's (opened on its own before) and an outer one's (a tree this one
/// is in). A file's facts are taken only where its `lex_key` is the one
/// made of it now (see `Extractor::extract_reusing`): the same text, read
/// under the same conditions.
pub struct Reuse {
    /// Each other index, how its paths are written, and the key it has of
    /// each file (from its manifest: no file's facts are read to find out).
    stores: Vec<(Store, Mapping, rustc_hash::FxHashMap<String, u64>)>,
    pub hits: std::sync::atomic::AtomicUsize,
}

/// How a path under this root is written under the other.
enum Mapping {
    /// The other root is the folder `prefix` (with a trailing `/`) in this one.
    Inner(String),
    /// This root is the folder `prefix` (with a trailing `/`) in the other.
    Outer(String),
}

impl Reuse {
    /// Folders this deep in the root are looked in for an index of their own.
    const DEPTH: usize = 6;

    /// The other indexes there are, if any (`own`: this root's, left out).
    pub fn find(root: &Path, own: &Path) -> Option<Reuse> {
        // (Read where they are, without moving a `.si` to `.norn` as
        // opening a root does: these roots are not being opened.)
        let caches = |d: &Path| [d.join(".norn").join("cache"), d.join(".si").join("cache")];
        let usable = |dir: PathBuf| -> Option<(Store, rustc_hash::FxHashMap<String, u64>)> {
            let s = Store::new(dir);
            if s.dir == own {
                return None;
            }
            let m = s.manifest().filter(|m| m.keys.len() == m.files.len())?;
            Some((s, m.files.into_iter().zip(m.keys).collect()))
        };
        let mut stores = Vec::new();
        let mut stack = vec![(root.to_path_buf(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || matches!(&*name, "node_modules" | "target") {
                    continue;
                }
                let path = e.path();
                if let Some((s, keys)) = caches(&path).into_iter().find_map(usable) {
                    stores.push((s, Mapping::Inner(format!("{}/", crate::rel_path(root, &path))), keys));
                }
                if depth + 1 < Self::DEPTH {
                    stack.push((path, depth + 1));
                }
            }
        }
        for outer in root.ancestors().skip(1).take(8) {
            if let Some((s, keys)) = caches(outer).into_iter().find_map(usable) {
                stores.push((s, Mapping::Outer(format!("{}/", crate::rel_path(outer, root))), keys));
            }
        }
        (!stores.is_empty()).then(|| Reuse { stores, hits: Default::default() })
    }

    /// The facts another index has of the file at `rel` (relative to this
    /// root) made with `key`, if one has.
    pub fn get(&self, rel: &str, key: u64) -> Option<FileFacts> {
        for (store, mapping, keys) in &self.stores {
            let there = match mapping {
                Mapping::Inner(prefix) => match rel.strip_prefix(prefix.as_str()) {
                    Some(r) => r.to_string(),
                    None => continue,
                },
                Mapping::Outer(prefix) => format!("{prefix}{rel}"),
            };
            if keys.get(&there) != Some(&key) {
                continue;
            }
            if let Some(saved) = store.load(&there).filter(|s| s.facts.lex_key == key) {
                return Some(saved.facts);
            }
        }
        None
    }
}
