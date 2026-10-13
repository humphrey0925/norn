//! Which part of a tree is indexed: the project's own setting, kept in the
//! tree at `.si/config.json` so that it is there the next time, whoever
//! opens it (an editor, the command line, another tool).
//!
//! ```json
//! { "include": ["bfd", "include"], "exclude": ["bfd/doc"] }
//! ```
//!
//! Both are folders relative to the root, `.` being the root itself. For a
//! file, the rule on its longest enclosing folder decides; with no rule
//! above it, it is in when nothing is listed under `include` (the whole tree
//! is the project) and out when something is (only those folders are).
//! `"exclude": ["."]` therefore leaves everything out but what `include`
//! brings back: the way to start from nothing.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Folders that are never part of a project: ours, and version control's.
const NEVER: [&str; 4] = [".si", ".git", ".svn", ".hg"];

fn clean(folder: &str) -> String {
    let f = folder.replace('\\', "/");
    let f = f.trim_matches('/');
    if f == "." { String::new() } else { f.to_string() }
}

/// Is `path` the folder `rule` or inside it?
fn under(path: &str, rule: &str) -> bool {
    rule.is_empty() || path == rule || path.len() > rule.len() && path.starts_with(rule) && path.as_bytes()[rule.len()] == b'/'
}

impl Scope {
    pub fn config_path(root: &Path) -> PathBuf {
        root.join(".si").join("config.json")
    }

    /// The scope saved in the tree, or the whole tree if there is none (or
    /// it cannot be read: a broken setting must not hide the project).
    pub fn load(root: &Path) -> Scope {
        let saved = std::fs::read(Scope::config_path(root)).ok().and_then(|b| serde_json::from_slice::<Scope>(&b).ok());
        saved.unwrap_or_default().normalized()
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = Scope::config_path(root);
        std::fs::create_dir_all(path.parent().unwrap())?;
        // The root is written as `.`, which reads better than an empty name.
        let mut written = self.clone().normalized();
        for list in [&mut written.include, &mut written.exclude] {
            list.iter_mut().filter(|f| f.is_empty()).for_each(|f| *f = ".".into());
        }
        std::fs::write(path, serde_json::to_string_pretty(&written).unwrap() + "\n")
    }

    pub fn normalized(mut self) -> Scope {
        for list in [&mut self.include, &mut self.exclude] {
            *list = list.iter().map(|f| clean(f)).collect();
            list.sort();
            list.dedup();
        }
        self
    }

    /// Is the file or folder at `rel` (relative to the root, `/` between
    /// parts) part of the project?
    pub fn contains(&self, rel: &str) -> bool {
        if rel.split('/').any(|part| NEVER.contains(&part)) {
            return false;
        }
        let longest = |rules: &[String]| rules.iter().filter(|r| under(rel, r)).map(|r| r.len() as i64).max().unwrap_or(-1);
        let (inc, exc) = (longest(&self.include), longest(&self.exclude));
        if inc < 0 && exc < 0 {
            return self.include.is_empty();
        }
        inc > exc
    }

    /// Can anything under the folder `rel` be part of the project? (It may
    /// be out itself and still hold a folder that is in.)
    pub fn reaches(&self, rel: &str) -> bool {
        if rel.split('/').any(|part| NEVER.contains(&part)) {
            return false;
        }
        self.contains(rel) || self.include.iter().any(|r| under(r, rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(include: &[&str], exclude: &[&str]) -> Scope {
        Scope { include: include.iter().map(|s| s.to_string()).collect(), exclude: exclude.iter().map(|s| s.to_string()).collect() }.normalized()
    }

    #[test]
    fn the_longest_enclosing_rule_decides() {
        let all = scope(&[], &[]);
        assert!(all.contains("bfd/elf.c") && all.contains("x.c") && !all.contains(".git/x.c") && !all.contains("a/.si/b.c"));

        let some = scope(&["bfd", "include/elf"], &["bfd/doc", "ld"]);
        assert!(some.contains("bfd/elf.c") && some.contains("include/elf/arm.h"));
        assert!(!some.contains("bfd/doc/chew.c") && !some.contains("ld/ld.c") && !some.contains("gas/as.c") && !some.contains("include/ansidecl.h"));
        assert!(!some.contains("bfdx/a.c"), "a name that merely starts the same");

        let carve = scope(&["bfd/doc/keep"], &["bfd"]);
        assert!(carve.contains("bfd/doc/keep/a.c") && !carve.contains("bfd/elf.c") && !carve.contains("gas/as.c"));
        // The walk has to go through excluded `bfd` to reach `bfd/doc/keep`.
        assert!(carve.reaches("bfd") && carve.reaches("bfd/doc") && !carve.reaches("gas"));

        // Start from nothing, then bring folders back.
        let none = scope(&[], &["."]);
        assert!(!none.contains("x.c") && !none.contains("bfd/elf.c") && !none.reaches("bfd"));
        let one = scope(&["bfd"], &["."]);
        assert!(one.contains("bfd/elf.c") && !one.contains("x.c") && !one.contains("gas/as.c") && one.reaches("bfd"));

        let without = scope(&[], &["testsuite"]);
        assert!(without.contains("gas/as.c") && !without.contains("testsuite/a.c") && without.reaches("gas"));
    }

    #[test]
    fn it_is_kept_in_the_tree() {
        let dir = std::env::temp_dir().join(format!("si-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Scope::load(&dir), Scope::default());
        scope(&["b\\", "/a/"], &["a/x"]).save(&dir).unwrap();
        assert_eq!(Scope::load(&dir), scope(&["a", "b"], &["a/x"]));
        std::fs::write(Scope::config_path(&dir), "not json").unwrap();
        assert_eq!(Scope::load(&dir), Scope::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
