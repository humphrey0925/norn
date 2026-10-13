//! Compile conditions: which macros the build defines, so that `#if`
//! branches the build never compiles are left out of the index.
//!
//! Without them every branch is indexed, and what only some configuration
//! has is marked uncertain. With them, a branch whose condition is known to
//! be false is skipped as the compiler would skip it; one whose condition is
//! not decided by what is known is still read, both ways.
//!
//! They are part of the project's settings in `.si/config.json`:
//!
//! ```json
//! { "defines": { "CONFIG_SMP": "1", "DEBUG": "" },
//!   "undefines": ["__MINGW32__"],
//!   "assumeUndefined": ["CONFIG_"],
//!   "conditionFiles": [".config", "include/generated/autoconf.h"] }
//! ```
//!
//! `defines` gives names a value (empty for a bare `#define`); `undefines`
//! says a name is not defined; `assumeUndefined` says the same of every
//! name with one of these prefixes that is not defined here (a kernel
//! configuration lists what is set, and the rest is not), `*` standing for
//! every name: what the build does not define is then not defined. A name
//! the sources themselves `#define` somewhere is never assumed undefined;
//! whether it is defined where it is tested depends on what was included,
//! which is not known here. `conditionFiles`
//! are read for more: lines of `#define NAME value`, `#undef NAME`,
//! `NAME=value` and `# NAME is not set` (with `=y` as 1 and `=m` defining
//! `NAME_MODULE`, the way Kconfig means them).
//!
//! What the sources themselves define counts too, where it can be told
//! without running the preprocessor. Within a file, a `#define` or `#undef`
//! that is certainly compiled settles the name for the lines after it.
//! Across files, a name tested in a file is known there when every source
//! file that the file is compiled as part of certainly includes a
//! certain definition of it, all to the same value, and none of them can
//! reach anything that undefines it or may define it otherwise: a switch
//! in a configuration header everything includes is known everywhere, a
//! macro one `.c` file sets before including a shared header is not known
//! in that header. `"sourceDefines": false` turns the second off.
//!
//! A project may have several such sets, as a build has configurations,
//! one of them in force:
//!
//! ```json
//! { "activeConfiguration": "Debug",
//!   "configurations": { "Debug": { "defines": { "DEBUG": "1" } },
//!                       "Release": { "conditionFiles": [".cproject#Release"] } } }
//! ```
//!
//! and a condition file may be the build's own description, with the
//! configuration meant after a `#`: an Eclipse CDT `.cproject` (DS-5, Arm
//! Development Studio), or the `c_cpp_properties.json` of VS Code's C/C++
//! extension. The set then follows what is written there.

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The settings as written.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub defines: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub undefines: Vec<String>,
    #[serde(default, rename = "assumeUndefined")]
    pub assume_undefined: Vec<String>,
    #[serde(default, rename = "conditionFiles")]
    pub condition_files: Vec<String>,
    /// `false`: macros the sources define to one value are not taken as
    /// known outside the file that defines them.
    #[serde(default, rename = "sourceDefines", skip_serializing_if = "Option::is_none")]
    pub source_defines: Option<bool>,
}

impl Settings {
    /// The configuration in force.
    pub fn load(root: &Path) -> Settings {
        Project::load(root).current()
    }

    pub fn is_empty(&self) -> bool {
        *self == Settings::default()
    }

    /// As the configuration in force, into `.si/config.json`.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let mut project = Project::load(root);
        let name = project.active.clone().unwrap_or_else(|| DEFAULT.to_string());
        project.configurations.insert(name.clone(), self.clone());
        project.active = Some(name);
        project.save(root)
    }
}

/// What a set is called when nobody named it.
pub const DEFAULT: &str = "Default";

/// Every set of conditions the project has, and which is in force.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub configurations: std::collections::BTreeMap<String, Settings>,
    #[serde(default, rename = "activeConfiguration")]
    pub active: Option<String>,
}

impl Project {
    pub fn load(root: &Path) -> Project {
        let path = crate::scope::Scope::config_path(root);
        let Some(bytes) = std::fs::read(path).ok() else { return Project::default() };
        let mut project: Project = serde_json::from_slice(&bytes).unwrap_or_default();
        // Written before there were configurations: one set, at the top.
        if project.configurations.is_empty() {
            let one: Settings = serde_json::from_slice(&bytes).unwrap_or_default();
            if !one.is_empty() {
                project.configurations.insert(DEFAULT.to_string(), one);
                project.active = Some(DEFAULT.to_string());
            }
        }
        if project.active.as_ref().is_some_and(|a| !project.configurations.contains_key(a)) {
            project.active = None;
        }
        project
    }

    pub fn current(&self) -> Settings {
        self.active.as_ref().and_then(|a| self.configurations.get(a)).cloned().unwrap_or_default()
    }

    /// Into `.si/config.json`, beside the scope.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let mut written = serde_json::to_value(self).unwrap();
        for old in ["defines", "undefines", "assumeUndefined", "conditionFiles"] {
            written[old] = serde_json::Value::Null;
        }
        crate::scope::write_settings(root, written)
    }
}

/// A macro as the configuration in force has it, and what says so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Effective {
    pub name: String,
    /// `None`: not defined.
    pub value: Option<String>,
    /// A condition file as written in the settings, or empty for the
    /// settings themselves.
    pub source: String,
}

/// What `settings` come to, name by name, with where each is from.
pub fn effective(settings: &Settings, root: &Path) -> Vec<Effective> {
    let mut all: std::collections::BTreeMap<String, (Option<String>, String)> = Default::default();
    let mut add = |c: &Conditions, source: &str| {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        for (name, value) in &c.defined {
            all.insert(text(name), (Some(text(value)), source.to_string()));
        }
        for name in &c.undefined {
            all.insert(text(name), (None, source.to_string()));
        }
    };
    for file in &settings.condition_files {
        let mut c = Conditions::default();
        c.read_source(root, file);
        add(&c, file);
    }
    let own = Settings { defines: settings.defines.clone(), undefines: settings.undefines.clone(), ..Settings::default() };
    add(&Conditions::from_settings(&own, root), "");
    all.into_iter().map(|(name, (value, source))| Effective { name, value, source }).collect()
}

/// A build's own configurations found in the tree, as condition-file
/// entries: `(name, entry)`.
pub fn discover(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for file in [".cproject", ".vscode/c_cpp_properties.json"] {
        let Ok(text) = std::fs::read(root.join(file)) else { continue };
        let text = String::from_utf8_lossy(&text);
        let names = if file.ends_with(".cproject") { cproject(&text) } else { cpp_properties(&text) };
        // (The configuration written for the C/C++ extension is our own
        // conditions on their way out, not a build's to follow.)
        let theirs = names.into_iter().filter(|c| c.name != WRITTEN);
        out.extend(theirs.map(|c| (c.name.clone(), format!("{file}#{}", c.name))));
    }
    out
}

/// The name of the `c_cpp_properties.json` configuration an editor
/// extension keeps in step with the conditions in force.
pub const WRITTEN: &str = "SI";

/// Where the build looks for headers, for the build descriptions among the
/// condition files of `settings`: paths under `root` relative to it, with
/// `/`, others as they are. Paths that depend on what only the build tool
/// knows are left out.
pub fn include_paths(settings: &Settings, root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in &settings.condition_files {
        let (path, wanted) = split_entry(entry);
        if path.rsplit(['/', '\\']).next() != Some(".cproject") {
            continue;
        }
        let Ok(text) = std::fs::read(root.join(path)) else { continue };
        let builds = cproject(&String::from_utf8_lossy(&text));
        let Some(b) = builds.iter().find(|b| wanted.is_none_or(|w| b.name == w)) else { continue };
        // The project is the folder of its `.cproject`; a build runs in a
        // folder named for the configuration, which relative paths start from.
        let project = path.rsplit_once(['/', '\\']).map_or("", |p| p.0).replace('\\', "/");
        for raw in &b.includes {
            let raw = raw.trim().trim_matches('"').replace('\\', "/");
            let under = |rest: &str| normal(&format!("{project}/{}", rest.trim_start_matches('/')));
            let found = if let Some(rest) = raw.strip_prefix("${workspace_loc:/").and_then(|r| r.strip_suffix('}')) {
                // `${workspace_loc:/Project/rest}`: taken to be this project.
                rest.split_once('/').map_or(Some(under("")), |(_, rest)| Some(under(rest)))
            } else if let Some(rest) = raw.strip_prefix("${ProjDirPath}").or(raw.strip_prefix("${project_loc}")) {
                Some(under(rest))
            } else if raw.contains("${") || raw.is_empty() {
                None
            } else if raw.starts_with('/') || raw.as_bytes().get(1) == Some(&b':') {
                Some(raw.clone())
            } else {
                Some(normal(&format!("{project}/{}/{raw}", b.name)))
            };
            if let Some(f) = found.filter(|f| !out.contains(f)) {
                out.push(f);
            }
        }
    }
    out
}

/// `a/./b/../c` as `a/c`; the tree's own root as `.`.
fn normal(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() { ".".to_string() } else { parts.join("/") }
}

/// A condition file as the settings name it: the path, and for a build
/// description the configuration meant after `#`.
fn split_entry(entry: &str) -> (&str, Option<&str>) {
    match entry.rsplit_once('#') {
        Some((path, name)) if !name.contains(['/', '\\']) => (path, Some(name)),
        _ => (entry, None),
    }
}

/// One configuration of a build description.
#[derive(Debug, Default, PartialEq, Eq)]
struct Build {
    name: String,
    defines: Vec<String>,
    undefines: Vec<String>,
    includes: Vec<String>,
}

/// `-DNAME=value` and `-UNAME` in a line of compiler flags.
fn flags(line: &str, into: &mut Build) {
    for word in line.split_whitespace() {
        if let Some(d) = word.strip_prefix("-D").filter(|d| !d.is_empty()) {
            into.defines.push(d.trim_matches('"').to_string());
        } else if let Some(u) = word.strip_prefix("-U").filter(|u| !u.is_empty()) {
            into.undefines.push(u.to_string());
        }
    }
}

/// The configurations of an Eclipse CDT `.cproject`: under each
/// `<configuration name=...>`, the options that list defined and undefined
/// symbols, whatever the toolchain calls them, and `-D` in free-form flags.
/// Settings for single files are left out.
fn cproject(text: &str) -> Vec<Build> {
    let unescape = |v: &str| v.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    let mut out: Vec<Build> = Vec::new();
    // Elements open: (name, the list it collects into, in a configuration, for one file)
    let mut open: Vec<(&str, u8, bool, bool)> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        if rest.starts_with(['?', '!']) {
            rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
            continue;
        }
        if let Some(closing) = rest.strip_prefix('/') {
            let name = closing.split(['>', ' ']).next().unwrap_or("");
            if let Some(k) = open.iter().rposition(|o| o.0 == name) {
                open.truncate(k);
            }
            continue;
        }
        let end = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(rest.len());
        let name = &rest[..end];
        rest = &rest[end..];
        let mut attrs: Vec<(&str, String)> = Vec::new();
        let self_closing = loop {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix("/>") {
                rest = r;
                break true;
            }
            if let Some(r) = rest.strip_prefix('>') {
                rest = r;
                break false;
            }
            let Some(eq) = rest.find('=') else { break true };
            let key = rest[..eq].trim();
            let after = rest[eq + 1..].trim_start();
            let Some(quote) = after.chars().next().filter(|c| matches!(c, '"' | '\'')) else { break true };
            let Some(close) = after[1..].find(quote) else { break true };
            attrs.push((key, unescape(&after[1..1 + close])));
            rest = &after[close + 2..];
        };
        let attr = |k: &str| attrs.iter().find(|a| a.0 == k).map(|a| a.1.as_str());
        let (mut list, mut inside, mut one_file) = open.last().map_or((0, false, false), |o| (o.1, o.2, o.3));
        match name {
            "configuration" if attr("name").is_some() && !inside => {
                out.push(Build { name: attr("name").unwrap().to_string(), ..Build::default() });
                inside = true;
            }
            "fileInfo" => one_file = true,
            "option" if inside && !one_file => {
                list = match attr("valueType") {
                    Some("definedSymbols") => 1,
                    Some("undefDefinedSymbols") => 2,
                    Some("includePath") => 3,
                    _ => 0,
                };
                if let (0, Some(value), Some(b)) = (list, attr("value"), out.last_mut()) {
                    flags(value, b);
                }
            }
            "listOptionValue" if list != 0 => {
                if let (Some(value), Some(b)) = (attr("value"), out.last_mut()) {
                    let value = value.trim().to_string();
                    match list {
                        1 => b.defines.push(value),
                        2 => b.undefines.push(value),
                        _ => b.includes.push(value),
                    }
                }
            }
            _ => {}
        }
        if !self_closing {
            open.push((name, if name == "option" { list } else { 0 }, inside, one_file));
        }
    }
    out
}

/// The configurations of a `c_cpp_properties.json` (VS Code's C/C++
/// extension): `defines`, and `-D` in `compilerArgs`. The file may have
/// comments and trailing commas.
fn cpp_properties(text: &str) -> Vec<Build> {
    // Comments out, then commas that end a list.
    let (mut plain, mut chars, mut in_string) = (String::with_capacity(text.len()), text.chars().peekable(), false);
    while let Some(c) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '\\' if in_string => {
                plain.push(c);
                plain.extend(chars.next());
                continue;
            }
            '/' if !in_string && chars.peek() == Some(&'/') => {
                while chars.next_if(|&c| c != '\n').is_some() {}
                continue;
            }
            '/' if !in_string && chars.peek() == Some(&'*') => {
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                continue;
            }
            '}' | ']' if !in_string => {
                let kept = plain.trim_end().len();
                if plain[..kept].ends_with(',') {
                    plain.truncate(kept - 1);
                }
            }
            _ => {}
        }
        plain.push(c);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&plain) else { return Vec::new() };
    let strings = |v: &serde_json::Value| v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(str::to_string).collect::<Vec<_>>();
    let mut out = Vec::new();
    for c in v["configurations"].as_array().into_iter().flatten() {
        let Some(name) = c["name"].as_str() else { continue };
        let mut b = Build { name: name.to_string(), defines: strings(&c["defines"]), ..Build::default() };
        // `${default}` and the like stand for settings kept elsewhere.
        b.defines.retain(|d| !d.contains("${"));
        flags(&strings(&c["compilerArgs"]).join(" "), &mut b);
        out.push(b);
    }
    out
}

/// What is known about macros when conditions are evaluated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conditions {
    defined: FxHashMap<Vec<u8>, Vec<u8>>,
    undefined: FxHashSet<Vec<u8>>,
    assume_undefined: Vec<Vec<u8>>,
    /// `*`: every name not defined here is assumed undefined.
    assume_all: bool,
    /// Names the sources `#define` somewhere: not for assuming about.
    in_source: FxHashSet<Vec<u8>>,
    /// What the sources define, file by file (see [`Source`]). What the
    /// settings say of a name comes first.
    source: Option<std::sync::Arc<Source>>,
    /// Set when the settings say not to use those.
    no_source: bool,
}

/// The macros whose value the sources settle, and in which files each is
/// known (see the module's comment). Files are numbered as they were
/// listed when this was made, and found by the path they are indexed by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Source {
    files: FxHashMap<String, u32>,
    /// name -> (value, the files it is known in as a bit set), one entry per value.
    names: FxHashMap<Vec<u8>, Vec<(Vec<u8>, Vec<u64>)>>,
}

impl Source {
    pub fn new(paths: &[String]) -> Source {
        Source { files: paths.iter().enumerate().map(|(i, p)| (p.clone(), i as u32)).collect(), names: FxHashMap::default() }
    }

    /// `name` has `value` in the files of `known` (one flag per file).
    pub fn add(&mut self, name: &[u8], value: &[u8], known: &[bool]) {
        let mut bits = vec![0u64; known.len().div_ceil(64)];
        for (i, _) in known.iter().enumerate().filter(|(_, k)| **k) {
            bits[i / 64] |= 1 << (i % 64);
        }
        self.names.entry(name.to_vec()).or_default().push((value.to_vec(), bits));
    }

    pub fn file(&self, path: &str) -> Option<u32> {
        self.files.get(path).copied()
    }

    fn get(&self, file: u32, name: &[u8]) -> Option<&[u8]> {
        let (word, bit) = (file as usize / 64, 1u64 << (file % 64));
        self.names.get(name)?.iter().find(|(_, bits)| bits.get(word).is_some_and(|w| w & bit != 0)).map(|(v, _)| &v[..])
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn digest(&self) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed as h;
        let each = |(k, values): (&Vec<u8>, &Vec<(Vec<u8>, Vec<u64>)>)| {
            values.iter().fold(0, |acc, (v, bits)| acc ^ h(bytemuck::cast_slice(bits), h(v, h(k, 15))))
        };
        self.names.iter().map(each).fold(self.files.len() as u64, |a, b| a ^ b)
    }

    /// What `file` is told: other facts come of it when this changes.
    pub fn file_digest(&self, path: &str) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed as h;
        let Some(file) = self.file(path) else { return 0 };
        let (word, bit) = (file as usize / 64, 1u64 << (file % 64));
        let mut d = 0;
        for (k, values) in &self.names {
            for (v, bits) in values {
                if bits.get(word).is_some_and(|w| w & bit != 0) {
                    d ^= h(v, h(k, 16));
                }
            }
        }
        d
    }

    /// Each macro and value, with the number of files it is known in.
    pub fn summary(&self) -> Vec<(String, String, usize)> {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let mut out: Vec<_> = self
            .names
            .iter()
            .flat_map(|(k, values)| values.iter().map(move |(v, bits)| (text(k), text(v), bits.iter().map(|w| w.count_ones() as usize).sum())))
            .collect();
        out.sort();
        out
    }
}

/// What a file has said of macros so far, on the way down it: defined to
/// a value, undefined, or (`None`) one or the other depending on a branch
/// that may or may not be compiled.
pub type Local = FxHashMap<Vec<u8>, Option<Option<Vec<u8>>>>;

/// The conditions as seen from a place in a file.
struct View<'a> {
    c: &'a Conditions,
    local: &'a Local,
    /// The file, as [`Source`] numbers it.
    file: Option<u32>,
}

impl View<'_> {
    /// From the sources, where the settings say nothing of the name.
    fn sourced(&self, name: &[u8]) -> Option<&[u8]> {
        self.c.source.as_ref()?.get(self.file?, name)
    }

    fn defined(&self, name: &[u8]) -> Tri {
        match self.local.get(name) {
            Some(Some(Some(_))) => Tri::True,
            Some(Some(None)) => Tri::False,
            Some(None) => Tri::Unknown,
            None if self.c.defined.contains_key(name) => Tri::True,
            None if self.c.undefined.contains(name) => Tri::False,
            None if self.sourced(name).is_some() => Tri::True,
            None => self.c.defined(name),
        }
    }

    fn text(&self, name: &[u8]) -> Option<Option<&[u8]>> {
        match self.local.get(name) {
            Some(Some(v)) => Some(v.as_deref()),
            Some(None) => None,
            None => match self.c.defined.get(name).map(|v| &v[..]).or(if self.c.undefined.contains(name) { None } else { self.sourced(name) }) {
                Some(v) => Some(Some(v)),
                None => (self.c.defined(name) == Tri::False).then_some(None),
            },
        }
    }

    /// The value of a name in an `#if`: its definition if that is a number
    /// (or a name that is), 0 if it is known not to be defined.
    fn value(&self, name: &[u8], depth: u8) -> Option<i64> {
        match self.text(name)? {
            None => Some(0),
            Some(v) => number(v).or(if v.is_empty() || v == name || depth > 8 { None } else { self.value(v, depth + 1) }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    False,
    True,
    Unknown,
}

impl Tri {
    fn of(b: bool) -> Tri {
        if b { Tri::True } else { Tri::False }
    }

    pub fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }

    pub fn or(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::True, _) | (_, Tri::True) => Tri::True,
            (Tri::False, Tri::False) => Tri::False,
            _ => Tri::Unknown,
        }
    }

    pub fn and(self, other: Tri) -> Tri {
        self.not().or(other.not()).not()
    }
}

impl Conditions {
    /// The settings and everything the files they name add.
    pub fn load(root: &Path) -> Conditions {
        Conditions::from_settings(&Settings::load(root), root)
    }

    pub fn from_settings(settings: &Settings, root: &Path) -> Conditions {
        let mut c = Conditions::default();
        for file in &settings.condition_files {
            c.read_source(root, file);
        }
        // What is written in the settings themselves has the last word.
        for (name, value) in &settings.defines {
            c.define(name, value);
        }
        for name in &settings.undefines {
            c.undefine(name);
        }
        let prefixes = || settings.assume_undefined.iter().map(|p| p.trim_end_matches('*')).filter(|p| !p.is_empty());
        c.assume_undefined = prefixes().map(|p| p.as_bytes().to_vec()).collect();
        c.assume_all = settings.assume_undefined.iter().any(|p| p == "*");
        c.no_source = settings.source_defines == Some(false);
        c
    }

    pub fn define(&mut self, name: &str, value: &str) {
        self.undefined.remove(name.as_bytes());
        self.defined.insert(name.as_bytes().to_vec(), value.trim().as_bytes().to_vec());
    }

    pub fn undefine(&mut self, name: &str) {
        self.defined.remove(name.as_bytes());
        self.undefined.insert(name.as_bytes().to_vec());
    }

    /// A condition file as the settings name it: a path, and for a build
    /// description the configuration meant after `#` (else its first).
    fn read_source(&mut self, root: &Path, entry: &str) {
        let (path, wanted) = split_entry(entry);
        let Ok(text) = std::fs::read(root.join(path)) else { return };
        let text = String::from_utf8_lossy(&text);
        let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let builds = match file {
            ".cproject" => cproject(&text),
            "c_cpp_properties.json" => cpp_properties(&text),
            _ => return self.read_file(&text),
        };
        let Some(b) = builds.iter().find(|b| wanted.is_none_or(|w| b.name == w)) else { return };
        for d in &b.defines {
            // `-DNAME` is `#define NAME 1`.
            let (name, value) = d.split_once('=').unwrap_or((d, "1"));
            self.define(name.trim(), value);
        }
        for u in &b.undefines {
            self.undefine(u.trim());
        }
    }

    /// A header of `#define`s, or a Kconfig `.config`.
    fn read_file(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix('#') {
                let rest = rest.trim_start();
                if let Some(d) = rest.strip_prefix("define").filter(|d| d.starts_with([' ', '\t'])) {
                    let d = d.trim_start();
                    let end = d.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(d.len());
                    // A macro with parameters says nothing a condition can use.
                    if end > 0 && !d[end..].starts_with('(') {
                        self.define(&d[..end], d[end..].split("/*").next().unwrap_or(""));
                    }
                } else if let Some(u) = rest.strip_prefix("undef").filter(|u| u.starts_with([' ', '\t'])) {
                    self.undefine(u.trim());
                } else if let Some(name) = rest.strip_suffix("is not set") {
                    // `# CONFIG_FOO is not set`
                    let name = name.trim();
                    self.undefine(name);
                    self.undefine(&format!("{name}_MODULE"));
                }
                continue;
            }
            let Some((name, value)) = line.split_once('=') else { continue };
            let name = name.trim();
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                continue;
            }
            match value.trim() {
                "y" => self.define(name, "1"),
                "m" => {
                    self.undefine(name);
                    self.define(&format!("{name}_MODULE"), "1");
                }
                "n" => self.undefine(name),
                v => self.define(name, v),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.defined.is_empty() && self.undefined.is_empty() && self.assume_undefined.is_empty() && !self.assume_all
    }

    /// Do these conditions assume anything about names they do not list?
    /// Then they need to be told which names the sources define.
    pub fn assumes(&self) -> bool {
        self.assume_all || !self.assume_undefined.is_empty()
    }

    pub fn set_in_source(&mut self, names: FxHashSet<Vec<u8>>) {
        self.in_source = names;
    }

    /// Are macros the sources define to one value to be taken as known?
    pub fn uses_source(&self) -> bool {
        !self.no_source
    }

    /// Do the settings themselves say whether `name` is defined?
    pub fn settles(&self, name: &[u8]) -> bool {
        self.defined.contains_key(name) || self.undefined.contains(name)
    }

    pub fn source(&self) -> Option<&std::sync::Arc<Source>> {
        self.source.as_ref()
    }

    pub fn set_source(&mut self, source: Option<std::sync::Arc<Source>>) {
        self.source = source;
    }

    /// The same conditions, with nothing taken from the sources.
    pub fn without_source() -> Conditions {
        Conditions { no_source: true, ..Conditions::default() }
    }

    /// How many names are known one way or the other.
    pub fn len(&self) -> usize {
        self.defined.len() + self.undefined.len()
    }

    /// Order-independent digest: facts extracted under other conditions are
    /// other facts.
    pub fn digest(&self) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed as h;
        let d = self.defined.iter().fold(0, |acc, (k, v)| acc ^ h(v, h(k, 11)));
        let u = self.undefined.iter().fold(0, |acc, k| acc ^ h(k, 12));
        let a = self.assume_undefined.iter().fold(self.assume_all as u64, |acc, k| acc ^ h(k, 13));
        let s = self.in_source.iter().fold(0, |acc, k| acc ^ h(k, 14));
        let c = self.source.as_ref().map_or(0, |s| s.digest()) ^ self.no_source as u64;
        d ^ u ^ a ^ s ^ c
    }

    pub fn defined(&self, name: &[u8]) -> Tri {
        if self.defined.contains_key(name) {
            Tri::True
        } else if self.undefined.contains(name) {
            Tri::False
        } else if !self.in_source.contains(name) && (self.assume_all || self.assume_undefined.iter().any(|p| name.starts_with(p))) {
            Tri::False
        } else {
            Tri::Unknown
        }
    }

    /// Whether the condition of an `#if` or `#elif` holds, as far as known.
    pub fn eval(&self, expr: &[u8]) -> Tri {
        static NONE: std::sync::OnceLock<Local> = std::sync::OnceLock::new();
        self.eval_in(NONE.get_or_init(Local::default), None, expr)
    }

    /// The same at a place in a file, after what the file said of macros
    /// above it.
    pub fn eval_in(&self, local: &Local, file: Option<u32>, expr: &[u8]) -> Tri {
        let toks = tokens(expr);
        let mut p = Parser { c: View { c: self, local, file }, toks: &toks, at: 0 };
        let v = p.or();
        if p.at != toks.len() {
            return Tri::Unknown;
        }
        truth(v)
    }

    pub fn defined_in(&self, local: &Local, file: Option<u32>, name: &[u8]) -> Tri {
        View { c: self, local, file }.defined(name)
    }
}

/// A macro's body as a value: comments out, spaces closed up, the
/// parentheses around all of it off. `None` if it is too long to be one.
pub fn value_of(body: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < body.len() {
        match (body[i], body.get(i + 1)) {
            (b'/', Some(b'*')) => {
                i += 2;
                while i < body.len() && !(body[i] == b'*' && body.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
                out.push(b' ');
            }
            (b'/', Some(b'/')) => break,
            (b'\\', _) => i += 1,
            (b, _) if b.is_ascii_whitespace() => {
                if out.last().is_some_and(|l| *l != b' ') {
                    out.push(b' ');
                }
                i += 1;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    let mut v = out.trim_ascii();
    while v.first() == Some(&b'(') && v.last() == Some(&b')') && !v[1..v.len() - 1].iter().any(|b| matches!(b, b'(' | b')')) {
        v = v[1..v.len() - 1].trim_ascii();
    }
    (v.len() <= 120).then(|| v.to_vec())
}

fn number(v: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(v).ok()?.trim();
    let s = s.trim_end_matches(['u', 'U', 'l', 'L']);
    match s.strip_prefix("0x").or(s.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn truth(v: Option<i64>) -> Tri {
    v.map_or(Tri::Unknown, |n| Tri::of(n != 0))
}

#[derive(Clone, Debug, PartialEq)]
enum Tok<'a> {
    Ident(&'a [u8]),
    Num(i64),
    Op(&'a [u8]),
}

fn tokens(s: &[u8]) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if b.is_ascii_whitespace() || b == b'\\' {
            i += 1;
        } else if b == b'/' && s.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < s.len() && !(s[i] == b'*' && s.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i += 2;
        } else if b == b'/' && s.get(i + 1) == Some(&b'/') {
            break;
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let a = i;
            while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(&s[a..i]));
        } else if b.is_ascii_digit() {
            let a = i;
            while i < s.len() && s[i].is_ascii_alphanumeric() {
                i += 1;
            }
            // A number that cannot be read makes the condition unknown.
            out.push(number(&s[a..i]).map_or(Tok::Op(&s[a..i]), Tok::Num));
        } else {
            let two = s.get(i..i + 2).filter(|t| matches!(*t, b"&&" | b"||" | b"==" | b"!=" | b"<=" | b">="));
            let len = if two.is_some() { 2 } else { 1 };
            out.push(Tok::Op(&s[i..i + len]));
            i += len;
        }
    }
    out
}

/// `||` over `&&` over comparisons over `!` over primaries, in three-valued
/// logic: `None` is a value that is not known. Anything else in a condition
/// (arithmetic, a macro with arguments) is not known either.
struct Parser<'a> {
    c: View<'a>,
    toks: &'a [Tok<'a>],
    at: usize,
}

impl Parser<'_> {
    fn op(&mut self, text: &[u8]) -> bool {
        let hit = matches!(self.toks.get(self.at), Some(Tok::Op(o)) if *o == text);
        self.at += hit as usize;
        hit
    }

    fn or(&mut self) -> Option<i64> {
        let mut v = self.and();
        while self.op(b"||") {
            let r = self.and();
            v = as_value(truth(v).or(truth(r)));
        }
        v
    }

    fn and(&mut self) -> Option<i64> {
        let mut v = self.compare();
        while self.op(b"&&") {
            let r = self.compare();
            v = as_value(truth(v).and(truth(r)));
        }
        v
    }

    fn compare(&mut self) -> Option<i64> {
        let l = self.unary();
        for (text, f) in [
            (&b"=="[..], (|a, b| a == b) as fn(i64, i64) -> bool),
            (b"!=", |a, b| a != b),
            (b"<=", |a, b| a <= b),
            (b">=", |a, b| a >= b),
            (b"<", |a, b| a < b),
            (b">", |a, b| a > b),
        ] {
            if self.op(text) {
                let r = self.unary();
                return Some(f(l?, r?) as i64);
            }
        }
        l
    }

    fn unary(&mut self) -> Option<i64> {
        if self.op(b"!") {
            return as_value(truth(self.unary()).not());
        }
        self.primary()
    }

    fn primary(&mut self) -> Option<i64> {
        match self.toks.get(self.at).cloned() {
            Some(Tok::Num(n)) => {
                self.at += 1;
                Some(n)
            }
            Some(Tok::Op(b"(")) => {
                self.at += 1;
                let v = self.or();
                if !self.op(b")") {
                    self.at = usize::MAX / 2; // unbalanced: nothing is known
                }
                v
            }
            Some(Tok::Ident(b"defined")) => {
                self.at += 1;
                let paren = self.op(b"(");
                let Some(Tok::Ident(name)) = self.toks.get(self.at).cloned() else { return None };
                self.at += 1;
                if paren && !self.op(b")") {
                    return None;
                }
                as_value(self.c.defined(name))
            }
            Some(Tok::Ident(name)) => {
                self.at += 1;
                if !self.op(b"(") {
                    return self.c.value(name, 0);
                }
                // A macro with arguments: the kernel's own, or unknown.
                let arg = match (self.toks.get(self.at).cloned(), self.toks.get(self.at + 1)) {
                    (Some(Tok::Ident(a)), Some(Tok::Op(b")"))) => Some(a),
                    _ => None,
                };
                let mut depth = 1;
                while depth > 0 && self.at < self.toks.len() {
                    match &self.toks[self.at] {
                        Tok::Op(b"(") => depth += 1,
                        Tok::Op(b")") => depth -= 1,
                        _ => {}
                    }
                    self.at += 1;
                }
                let arg = arg?;
                let module = [arg, b"_MODULE"].concat();
                let (builtin, as_module) = (self.c.defined(arg), self.c.defined(&module));
                as_value(match name {
                    b"IS_ENABLED" | b"IS_REACHABLE" => builtin.or(as_module),
                    b"IS_BUILTIN" => builtin,
                    b"IS_MODULE" => as_module,
                    _ => Tri::Unknown,
                })
            }
            _ => {
                self.at = usize::MAX / 2;
                None
            }
        }
    }
}

fn as_value(t: Tri) -> Option<i64> {
    match t {
        Tri::True => Some(1),
        Tri::False => Some(0),
        Tri::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conditions() -> Conditions {
        let mut c = Conditions::default();
        c.read_file("CONFIG_SMP=y\nCONFIG_NET=m\n# CONFIG_DEBUG is not set\nCONFIG_HZ=250\n#define ARCH_SIZE 64\n#define HAVE_X\n#undef HAVE_Y\n#define MAX(a, b) a\n");
        c.assume_undefined = vec![b"CONFIG_".to_vec()];
        c
    }

    #[test]
    fn a_configuration_file_is_read_both_ways() {
        let c = conditions();
        assert_eq!(c.defined(b"CONFIG_SMP"), Tri::True);
        assert_eq!((c.defined(b"CONFIG_NET"), c.defined(b"CONFIG_NET_MODULE")), (Tri::False, Tri::True));
        assert_eq!(c.defined(b"CONFIG_DEBUG"), Tri::False);
        assert_eq!(c.defined(b"CONFIG_NEVER_MENTIONED"), Tri::False, "by the prefix");
        assert_eq!((c.defined(b"HAVE_X"), c.defined(b"HAVE_Y"), c.defined(b"HAVE_Z")), (Tri::True, Tri::False, Tri::Unknown));
        assert_eq!(c.defined(b"MAX"), Tri::Unknown, "a macro with parameters is not a setting");
    }

    #[test]
    fn what_the_build_does_not_define_can_be_assumed_undefined() {
        let settings = Settings { assume_undefined: vec!["*".into()], defines: [("LINUX".to_string(), "1".to_string())].into(), ..Settings::default() };
        let mut c = Conditions::from_settings(&settings, Path::new("."));
        assert!(c.assumes());
        c.set_in_source([b"FOO_H".to_vec(), b"HAVE_ZLIB".to_vec()].into_iter().collect());
        assert_eq!((c.defined(b"LINUX"), c.defined(b"_WIN32")), (Tri::True, Tri::False));
        // Defined somewhere in the sources: not for assuming about.
        assert_eq!((c.defined(b"FOO_H"), c.defined(b"HAVE_ZLIB")), (Tri::Unknown, Tri::Unknown));
        assert_eq!(c.eval(b"defined(_WIN32) || defined(HAVE_ZLIB)"), Tri::Unknown);
        assert_eq!(c.eval(b"defined(_WIN32) && defined(HAVE_ZLIB)"), Tri::False);
    }

    #[test]
    fn a_build_description_gives_its_configurations() {
        let xml = r#"<?xml version="1.0"?><cproject><storageModule moduleId="org.eclipse.cdt.core.settings">
          <cconfiguration id="a"><storageModule moduleId="cdtBuildSystem"><configuration name="Debug" artifactName="x">
            <folderInfo id="f" resourcePath=""><toolChain><tool name="ARM C Compiler 5">
              <option id="o1" superClass="com.arm.tool.c.compiler.option.defmac" valueType="definedSymbols">
                <listOptionValue builtIn="false" value="DEBUG"/><listOptionValue builtIn="false" value="BOARD=&quot;evk&quot;"/>
                <listOptionValue value="LEVEL=3"/></option>
              <option id="o2" valueType="undefDefinedSymbols"><listOptionValue value="NDEBUG"/></option>
              <option id="o3" superClass="x.other" value="-O2 -DFROM_FLAGS=7 -UGONE" valueType="string"/>
              <option id="o4" valueType="includePath"><listOptionValue value="../inc"/><listOptionValue value="&quot;${workspace_loc:/${ProjName}/drivers/include}&quot;"/>
                <listOptionValue value="${ProjDirPath}/bsp"/><listOptionValue value="C:\Arm\include"/><listOptionValue value="${SOMETHING}/x"/></option>
            </tool></toolChain></folderInfo>
            <fileInfo id="one" resourcePath="a.c"><tool><option valueType="definedSymbols"><listOptionValue value="ONLY_A"/></option></tool></fileInfo>
          </configuration></storageModule></cconfiguration>
          <cconfiguration id="b"><storageModule moduleId="cdtBuildSystem"><configuration name="Release">
            <folderInfo><toolChain><tool><option valueType="definedSymbols"><listOptionValue value="NDEBUG"/></option></tool></toolChain></folderInfo>
          </configuration></storageModule></cconfiguration></storageModule></cproject>"#;
        let b = cproject(xml);
        assert_eq!(b.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["Debug", "Release"]);
        assert_eq!(b[0].defines, ["DEBUG", "BOARD=\"evk\"", "LEVEL=3", "FROM_FLAGS=7"]);
        assert_eq!(b[0].undefines, ["NDEBUG", "GONE"]);
        assert_eq!(b[1].defines, ["NDEBUG"]);

        let json = "{ // comment\n \"configurations\": [ { \"name\": \"Win32\", \"defines\": [\"_DEBUG\", \"UNICODE\", \"N=2\", \"${default}\",], /* x */\n\"compilerArgs\": [\"-DARG=1\"], }, { \"name\": \"Linux\", \"defines\": [] } ], \"version\": 4 }";
        let b = cpp_properties(json);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].name.as_str(), &b[0].defines), ("Win32", &vec!["_DEBUG".to_string(), "UNICODE".into(), "N=2".into(), "ARG=1".into()]));

        // As condition files, with the configuration after `#`.
        let dir = std::env::temp_dir().join(format!("si-cond-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".vscode")).unwrap();
        std::fs::write(dir.join(".cproject"), xml).unwrap();
        std::fs::write(dir.join(".vscode/c_cpp_properties.json"), json).unwrap();
        assert_eq!(discover(&dir).iter().map(|d| d.1.as_str()).collect::<Vec<_>>(), [".cproject#Debug", ".cproject#Release", ".vscode/c_cpp_properties.json#Win32", ".vscode/c_cpp_properties.json#Linux"]);
        let mut settings = Settings { condition_files: vec![".cproject#Release".into()], ..Settings::default() };
        let c = Conditions::from_settings(&settings, &dir);
        assert_eq!((c.defined(b"NDEBUG"), c.defined(b"DEBUG")), (Tri::True, Tri::Unknown));
        settings.condition_files = vec![".cproject#Debug".into(), ".vscode/c_cpp_properties.json#Win32".into()];
        settings.defines.insert("LEVEL".into(), "9".into());
        let c = Conditions::from_settings(&settings, &dir);
        assert_eq!((c.eval(b"LEVEL == 9 && N == 2 && DEBUG"), c.defined(b"NDEBUG"), c.defined(b"ONLY_A")), (Tri::True, Tri::False, Tri::Unknown));
        assert_eq!(include_paths(&settings, &dir), ["inc", "drivers/include", "bsp", "C:/Arm/include"]);
        // What it all comes to, and who says so.
        let e = effective(&settings, &dir);
        let of = |n: &str| e.iter().find(|x| x.name == n).map(|x| (x.value.as_deref(), x.source.as_str()));
        assert_eq!(of("LEVEL"), Some((Some("9"), "")));
        assert_eq!(of("DEBUG"), Some((Some("1"), ".cproject#Debug")));
        assert_eq!(of("NDEBUG"), Some((None, ".cproject#Debug")));
        assert_eq!(of("N"), Some((Some("2"), ".vscode/c_cpp_properties.json#Win32")));

        // Several sets in one project, one in force; an old file is one set.
        std::fs::create_dir_all(dir.join(".si")).unwrap();
        std::fs::write(dir.join(".si/config.json"), r#"{"include":["src"],"defines":{"OLD":"1"}}"#).unwrap();
        let mut p = Project::load(&dir);
        assert_eq!((p.active.as_deref(), p.current().defines.get("OLD").map(String::as_str)), (Some(DEFAULT), Some("1")));
        p.configurations.insert("Debug".into(), settings.clone());
        p.active = Some("Debug".into());
        p.save(&dir).unwrap();
        assert_eq!((Project::load(&dir), Settings::load(&dir)), (p, settings));
        let written = std::fs::read_to_string(dir.join(".si/config.json")).unwrap();
        assert!(written.contains("\"include\"") && !written.contains("\n  \"defines\""), "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_the_sources_define_is_known_where_it_can_be() {
        let files = ["a.c".to_string(), "b.c".to_string(), "cfg.h".to_string()];
        let mut source = Source::new(&files);
        source.add(b"USE_NET", b"1", &[true, true, true]);
        source.add(b"LEVEL", b"DEFAULT_LEVEL", &[true, false, false]);
        source.add(b"DEFAULT_LEVEL", b"3", &[true, false, false]);
        source.add(b"HAVE_X", b"", &[true, true, false]);
        let mut c = Conditions::default();
        c.set_source(Some(std::sync::Arc::new(source)));
        let none = Local::default();
        let (a, b) = (c.source().unwrap().file("a.c"), c.source().unwrap().file("b.c"));
        assert_eq!((c.eval_in(&none, a, b"USE_NET"), c.eval_in(&none, a, b"defined(HAVE_X) && LEVEL >= 3"), c.eval_in(&none, a, b"OTHER")), (Tri::True, Tri::True, Tri::Unknown));
        // Known in one file is not known in another, or where no file is said.
        assert_eq!((c.eval_in(&none, b, b"LEVEL >= 3"), c.eval_in(&none, b, b"USE_NET"), c.eval(b"USE_NET")), (Tri::Unknown, Tri::True, Tri::Unknown));
        assert_ne!(c.source().unwrap().file_digest("a.c"), c.source().unwrap().file_digest("b.c"));
        // The settings have the last word.
        c.undefine("USE_NET");
        c.define("LEVEL", "1");
        assert_eq!((c.eval_in(&none, a, b"USE_NET"), c.eval_in(&none, a, b"LEVEL >= 3")), (Tri::False, Tri::False));
        // And what the file said above the line comes before both.
        let mut local = Local::default();
        local.insert(b"LEVEL".to_vec(), Some(Some(b"5".to_vec())));
        local.insert(b"HAVE_X".to_vec(), Some(None));
        local.insert(b"DEFAULT_LEVEL".to_vec(), None);
        assert_eq!((c.eval_in(&local, a, b"LEVEL >= 3"), c.eval_in(&local, a, b"defined HAVE_X"), c.eval_in(&local, a, b"DEFAULT_LEVEL == 3")), (Tri::True, Tri::False, Tri::Unknown));
        assert_eq!((value_of(b" ( 0x10 ) /* sixteen */"), value_of(b"a  +\tb // x"), value_of(b"")), (Some(b"0x10".to_vec()), Some(b"a + b".to_vec()), Some(Vec::new())));
    }

    #[test]
    fn conditions_hold_fail_or_are_not_known() {
        let c = conditions();
        for (expr, want) in [
            ("defined(CONFIG_SMP)", Tri::True),
            ("defined CONFIG_DEBUG", Tri::False),
            ("!defined(CONFIG_DEBUG) && defined(HAVE_X)", Tri::True),
            ("defined(HAVE_Z)", Tri::Unknown),
            ("defined(HAVE_Z) || defined(CONFIG_SMP)", Tri::True),
            ("defined(HAVE_Z) && defined(CONFIG_DEBUG)", Tri::False),
            ("defined(HAVE_Z) && defined(CONFIG_SMP)", Tri::Unknown),
            ("ARCH_SIZE == 64", Tri::True),
            ("ARCH_SIZE == 32", Tri::False),
            ("ARCH_SIZE >= 32 && CONFIG_HZ < 1000", Tri::True),
            ("CONFIG_DEBUG", Tri::False),
            ("CONFIG_SMP", Tri::True),
            ("UNKNOWN_THING > 2", Tri::Unknown),
            ("IS_ENABLED(CONFIG_NET)", Tri::True),
            ("IS_BUILTIN(CONFIG_NET)", Tri::False),
            ("IS_ENABLED(CONFIG_DEBUG)", Tri::False),
            ("SOME_MACRO(1, 2) && 0", Tri::False),
            ("SOME_MACRO(1, 2) || 0", Tri::Unknown),
            ("0", Tri::False),
            ("1 /* always */", Tri::True),
            ("(ARCH_SIZE == 64", Tri::Unknown),
            ("ARCH_SIZE + 1 == 65", Tri::Unknown),
        ] {
            assert_eq!(c.eval(expr.as_bytes()), want, "{expr}");
        }
        // With nothing set, only what needs no setting is decided: `#if 0`.
        let none = Conditions::default();
        assert_eq!((none.eval(b"0"), none.eval(b"defined(X)"), none.eval(b"X == 1")), (Tri::False, Tri::Unknown, Tri::Unknown));
    }
}
