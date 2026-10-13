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
const FORMAT: u32 = 7;

/// Size and modification time of a source file when its facts were made.
pub type Stamp = (u64, u64);

pub fn stamp(path: &Path) -> Stamp {
    let Ok(m) = std::fs::metadata(path) else { return (0, 0) };
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
        root.join(".si").join("cache")
    }

    /// Make the directory, and if it is a `cache` folder under `.si`, tell
    /// git to leave it alone: the settings beside it are for sharing, the
    /// cache is not.
    fn prepare(&self) {
        let _ = std::fs::create_dir_all(&self.dir);
        let parent = self.dir.parent().filter(|p| p.file_name().is_some_and(|n| n == ".si"));
        if let (Some(parent), Some(name)) = (parent, self.dir.file_name().and_then(|n| n.to_str())) {
            let ignore = parent.join(".gitignore");
            if !ignore.exists() {
                let _ = std::fs::write(ignore, format!("# The index si keeps here is rebuilt from the sources.\n/{name}/\n"));
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

    pub fn save_manifest(&self, sets: u64, files: Vec<String>) {
        self.prepare();
        if let Ok(bytes) = bincode::serialize(&Manifest { format: FORMAT, sets, files }) {
            // Written aside and moved into place: a start never reads half of it.
            let tmp = self.dir.join("manifest.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("manifest"));
            }
        }
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
