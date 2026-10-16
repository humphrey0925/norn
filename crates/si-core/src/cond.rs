//! Compile conditions: which macros the build defines, so that `#if`
//! branches the build never compiles are left out of the index.
//!
//! Without them every branch is indexed, and what only some configuration
//! has is marked uncertain. With them, a branch whose condition is known to
//! be false is skipped as the compiler would skip it; one whose condition is
//! not decided by what is known is still read, both ways.
//!
//! They are part of the project's settings in `.norn/config.json`:
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
//! in that header. Where the file brings the definition in itself, with an
//! `#include`, the name is known from the line after that one and not
//! before. (The order of includes in the files that include it is not
//! looked at.) `"sourceDefines": false` turns the second off.
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
use std::ops::Not;
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
    /// What a branch on a macro nothing here settles is taken to be:
    /// `"both"` (the default, as Source Insight does), possibly compiled,
    /// both branches indexed and marked as only possibly compiled; `"off"`,
    /// not compiled, the macro taken as not defined (as a compiler takes a
    /// name it has no definition of).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undecided: Option<String>,
}

impl Settings {
    /// The configuration in force.
    pub fn load(root: &Path) -> Settings {
        Project::load(root).current()
    }

    pub fn is_empty(&self) -> bool {
        *self == Settings::default()
    }

    /// As the configuration in force, into `.norn/config.json`.
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
    /// What the tree's own build descriptions say, when nobody has said
    /// anything: a set for each configuration found, the first in force.
    /// Nothing is written; it is found again each time, until the project
    /// is given sets of its own (or told to have none).
    fn found(root: &Path) -> Project {
        let mut project = Project::default();
        for (name, entry) in discover(root) {
            let mut unique = name.clone();
            for n in 2.. {
                if !project.configurations.contains_key(&unique) {
                    break;
                }
                unique = format!("{name} {n}");
            }
            project.active.get_or_insert_with(|| unique.clone());
            project.configurations.insert(unique, Settings { condition_files: vec![entry], ..Settings::default() });
        }
        project
    }

    pub fn load(root: &Path) -> Project {
        Project::load_told(root).0
    }

    /// The project's sets, and whether they are the tree's own build
    /// descriptions followed unasked rather than anything written.
    pub fn load_told(root: &Path) -> (Project, bool) {
        let path = crate::scope::Scope::config_path(root);
        let found = || {
            let found = Project::found(root);
            let any = !found.configurations.is_empty();
            (found, any)
        };
        let Some(bytes) = std::fs::read(path).ok() else { return found() };
        let mut project: Project = serde_json::from_slice(&bytes).unwrap_or_default();
        let said = serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| v.get("configurations").is_some_and(|c| !c.is_null()));
        // Written before there were configurations: one set, at the top.
        if project.configurations.is_empty() {
            let one: Settings = serde_json::from_slice(&bytes).unwrap_or_default();
            if !one.is_empty() {
                project.configurations.insert(DEFAULT.to_string(), one);
                project.active = Some(DEFAULT.to_string());
            }
        }
        if project.configurations.is_empty() && !said {
            return found();
        }
        if project.active.as_ref().is_some_and(|a| !project.configurations.contains_key(a)) {
            project.active = None;
        }
        (project, false)
    }

    pub fn current(&self) -> Settings {
        self.active.as_ref().and_then(|a| self.configurations.get(a)).cloned().unwrap_or_default()
    }

    /// Into `.norn/config.json`, beside the scope.
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
    // A project file may be anywhere near. Of Makefiles (and the like),
    // which every folder may have, only those nearest the root that say
    // anything: further down they are one part's, not the build's.
    let mut makefiles_at: Option<usize> = None;
    for file in crate::builds::find(root) {
        let depth = file.matches('/').count();
        let general = crate::builds::is_general(&file);
        if general && makefiles_at.is_some_and(|d| d != depth) {
            continue;
        }
        let Ok(text) = std::fs::read(root.join(&file)) else { continue };
        let text = String::from_utf8_lossy(&text);
        let names = builds_of(&file, &text).unwrap_or_default();
        if general && !names.is_empty() {
            makefiles_at = Some(depth);
        }
        // (The configuration written for the C/C++ extension is our own
        // conditions on their way out, not a build's to follow.)
        let theirs = names.into_iter().filter(|c| c.name != WRITTEN && c.name != WRITTEN_BEFORE);
        // Which folder's, for one that is not the root's (or `.vscode`'s).
        let folder = file.rsplit_once('/').map_or("", |f| f.0);
        let label = |name: &str| if folder.is_empty() || folder == ".vscode" { name.to_string() } else { format!("{name} ({folder})") };
        out.extend(theirs.map(|c| (label(&c.name), format!("{file}#{}", c.name))));
    }
    out
}

/// The name of the `c_cpp_properties.json` configuration an editor
/// extension keeps in step with the conditions in force.
pub const WRITTEN: &str = "Norn";
/// What it was called before, and may still be in a file.
const WRITTEN_BEFORE: &str = "SI";

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

/// The configurations in a build description, by what the file is called;
/// `None` for a file that is none.
fn builds_of(path: &str, text: &str) -> Option<Vec<Build>> {
    match path.rsplit(['/', '\\']).next().unwrap_or(path) {
        ".cproject" => Some(cproject(text)),
        "c_cpp_properties.json" => Some(cpp_properties(text)),
        "compile_commands.json" => Some(compile_commands(text)),
        _ => crate::builds::of(path, text),
    }
}

/// A compilation database: the commands the build ran, file by file. What
/// every one of them defines is what the build defines.
fn compile_commands(text: &str) -> Vec<Build> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let mut common: Option<Build> = None;
    for entry in &entries {
        let words: Vec<String> = match (entry.get("arguments").and_then(|a| a.as_array()), entry.get("command").and_then(|c| c.as_str())) {
            (Some(args), _) => args.iter().filter_map(|a| a.as_str()).map(str::to_string).collect(),
            (None, Some(line)) => words(line),
            _ => continue,
        };
        let mut one = Build { compiler: words.first().and_then(|w| compiler_named(w.rsplit(['/', '\\']).next().unwrap_or(w))), ..Build::default() };
        let mut words = words.iter();
        while let Some(word) = words.next() {
            for (flag, define) in [("-D", true), ("-U", false), ("/D", true), ("/U", false)] {
                let Some(rest) = word.strip_prefix(flag) else { continue };
                // `-D NAME` as well as `-DNAME`.
                let value = if rest.is_empty() { words.next().map_or("", String::as_str) } else { rest };
                if !value.is_empty() {
                    if define { &mut one.defines } else { &mut one.undefines }.push(value.to_string());
                }
                break;
            }
        }
        common = Some(match common {
            None => one,
            Some(mut c) => {
                c.defines.retain(|d| one.defines.contains(d));
                c.undefines.retain(|u| one.undefines.contains(u));
                if c.compiler != one.compiler {
                    c.compiler = None;
                }
                c
            }
        });
    }
    common.into_iter().map(|c| Build { name: "compile_commands".to_string(), ..c }).collect()
}

/// A command line in words, as a shell would split it: quotes group, and
/// are not part of the word.
fn words(line: &str) -> Vec<String> {
    let (mut out, mut word, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) => {
                // `\"` is a quote in the word; any other backslash is itself
                // (a Windows path).
                match chars.clone().next() {
                    Some('"') => {
                        word.push('"');
                        chars.next();
                    }
                    _ => word.push(c),
                }
                any = true;
            }
            ('"' | '\'', None) => (quote, any) = (Some(c), true),
            (c, Some(q)) if c == q => quote = None,
            (c, None) if c.is_whitespace() => {
                if std::mem::take(&mut any) {
                    out.push(std::mem::take(&mut word));
                }
            }
            _ => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(word);
    }
    out
}

/// One configuration of a build description.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Build {
    pub(crate) name: String,
    pub(crate) defines: Vec<String>,
    pub(crate) undefines: Vec<String>,
    pub(crate) includes: Vec<String>,
    /// The compiler, where the description names it.
    pub(crate) compiler: Option<Compiler>,
}

/// A compiler, as far as which of the macros that tell compilers apart
/// it defines of itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compiler {
    /// Arm Compiler 5 (armcc: DS-5, Keil).
    Armcc,
    /// Arm Compiler 6 (armclang).
    Armclang,
    Gcc,
    Clang,
    Iar,
    Msvc,
}

/// The compiler a toolchain's name or a compiler's path says.
pub(crate) fn compiler_named(text: &str) -> Option<Compiler> {
    let t = text.to_ascii_lowercase();
    let has = |w: &str| t.contains(w);
    Some(if has("armclang") || has(".ac6") || has("toolchain.v6") || has("compiler 6") || has("armcompiler6") {
        Compiler::Armclang
    } else if has("armcc") || has(".ac5") || has("compiler 5") || has("armcompiler5") || has("rvct") {
        Compiler::Armcc
    } else if has("iccarm") || has("iar") {
        Compiler::Iar
    } else if has("clang") {
        Compiler::Clang
    } else if has("gcc") || has("gnu") {
        Compiler::Gcc
    } else if has("cl.exe") || has("msvc") {
        Compiler::Msvc
    } else {
        return None;
    })
}

/// The macros that tell compilers apart, as `cmsis_compiler.h` asks.
const COMPILER_MARKS: [&str; 11] = ["__CC_ARM", "__ARMCC_VERSION", "__GNUC__", "__clang__", "__ICCARM__", "__IAR_SYSTEMS_ICC__", "__TI_ARM__", "__ti__", "__TASKING__", "__CSMC__", "_MSC_VER"];

/// What `compiler` defines of itself, of those.
fn compiler_defines(compiler: Compiler) -> &'static [(&'static str, &'static str)] {
    match compiler {
        Compiler::Armcc => &[("__CC_ARM", "1"), ("__ARMCC_VERSION", "5060960")],
        Compiler::Armclang => &[("__ARMCC_VERSION", "6190004"), ("__clang__", "1"), ("__GNUC__", "4")],
        Compiler::Gcc => &[("__GNUC__", "10")],
        Compiler::Clang => &[("__clang__", "1"), ("__GNUC__", "4")],
        Compiler::Iar => &[("__ICCARM__", "1"), ("__IAR_SYSTEMS_ICC__", "9")],
        Compiler::Msvc => &[("_MSC_VER", "1930"), ("_WIN32", "1")],
    }
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
            // (Or its compiler, `<tool name="ARM C Compiler 5">`.)
            "toolChain" | "tool" if inside && !one_file && (name == "toolChain" || attr("name").is_some_and(|n| n.to_ascii_lowercase().contains("compiler"))) => {
                if let Some(b) = out.last_mut().filter(|b| b.compiler.is_none()) {
                    b.compiler = attr("superClass").and_then(compiler_named).or_else(|| attr("id").and_then(compiler_named)).or_else(|| attr("name").and_then(compiler_named));
                }
            }
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
        let mut b = Build { name: name.to_string(), defines: strings(&c["defines"]), compiler: c["compilerPath"].as_str().and_then(compiler_named), ..Build::default() };
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
    /// The same, the build's own description saying what it defines: but
    /// for what the compiler defines of itself (see `builtin`).
    assume_built: bool,
    /// Names the sources `#define` somewhere: not for assuming about.
    in_source: FxHashSet<Vec<u8>>,
    /// What the sources define, file by file (see [`Source`]). What the
    /// settings say of a name comes first.
    source: Option<std::sync::Arc<Source>>,
    /// Set when the settings say not to use those.
    no_source: bool,
    /// A name nothing settles is taken as not defined (`undecided: off`).
    undecided_off: bool,
}

/// The macros whose value the sources settle, and in which files each is
/// known (see the module's comment). Files are numbered as they were
/// listed when this was made, and found by the path they are indexed by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Source {
    files: FxHashMap<String, u32>,
    /// name -> one entry per value: the value, the files it is known in,
    /// and the files that certainly bring a definition of it in (both as
    /// bit sets).
    names: FxHashMap<Vec<u8>, Vec<(Vec<u8>, Vec<u64>, Vec<u64>)>>,
    /// Per file, its includes that are certainly compiled: the line, and
    /// the file included.
    includes: Vec<Vec<(u32, u32)>>,
    /// Per file, the conditions that come out one way in every
    /// compilation that reads them (see `units`): where the directive
    /// starts, and whether it holds.
    verdicts: std::sync::Arc<crate::units::Verdicts>,
    /// How each compilation reads the headers compiled more than one way.
    readings: std::sync::Arc<crate::units::Readings>,
}

fn bits_of(flags: &[bool]) -> Vec<u64> {
    let mut bits = vec![0u64; flags.len().div_ceil(64)];
    for (i, _) in flags.iter().enumerate().filter(|(_, k)| **k) {
        bits[i / 64] |= 1 << (i % 64);
    }
    bits
}

fn has(bits: &[u64], i: u32) -> bool {
    bits.get(i as usize / 64).is_some_and(|w| w & (1u64 << (i % 64)) != 0)
}

impl Source {
    pub fn new(paths: &[String]) -> Source {
        Source {
            files: paths.iter().enumerate().map(|(i, p)| (p.clone(), i as u32)).collect(),
            names: FxHashMap::default(),
            includes: vec![Vec::new(); paths.len()],
            verdicts: Default::default(),
            readings: Default::default(),
        }
    }

    /// `name` has `value` in the files of `known`; those of `brings`
    /// certainly include a definition of it (one flag per file).
    pub fn add(&mut self, name: &[u8], value: &[u8], known: &[bool], brings: &[bool]) {
        self.names.entry(name.to_vec()).or_default().push((value.to_vec(), bits_of(known), bits_of(brings)));
    }

    /// `file` includes `target` on `line`, for certain.
    pub fn include(&mut self, file: u32, line: u32, target: u32) {
        self.includes[file as usize].push((line, target));
    }

    pub fn set_verdicts(&mut self, verdicts: std::sync::Arc<crate::units::Verdicts>) {
        self.verdicts = verdicts;
    }

    /// Whether the condition of the directive that starts at `at` in
    /// `file` holds, where every compilation agrees.
    pub fn verdict(&self, file: u32, at: u32) -> Option<bool> {
        let of = self.verdicts.get(file as usize)?;
        of.binary_search_by_key(&at, |v| v.0).ok().map(|i| of[i].1)
    }

    pub fn verdicts(&self) -> &std::sync::Arc<crate::units::Verdicts> {
        &self.verdicts
    }

    pub fn set_readings(&mut self, readings: std::sync::Arc<crate::units::Readings>) {
        self.readings = readings;
    }

    pub fn readings(&self) -> &std::sync::Arc<crate::units::Readings> {
        &self.readings
    }

    /// How many conditions are decided that way.
    pub fn decided(&self) -> usize {
        self.verdicts.iter().map(Vec::len).sum()
    }

    pub fn file(&self, path: &str) -> Option<u32> {
        self.files.get(path).copied()
    }

    /// The line of `file` after which the entry's name is known there: the
    /// first of its own includes that brings a definition in, or 0 when it
    /// brings none in itself (the files that include it do).
    fn line_bringing(&self, file: u32, brings: &[u64]) -> u32 {
        self.includes[file as usize].iter().filter(|(_, t)| has(brings, *t)).map(|(l, _)| *l).min().unwrap_or(0)
    }

    /// The value `name` is known to have on `line` of `file`, as the
    /// compilations that read the file have it there.
    pub fn get(&self, file: u32, line: u32, name: &[u8]) -> Option<&[u8]> {
        let (value, _, brings) = self.names.get(name)?.iter().find(|(_, known, _)| has(known, file))?;
        (line > self.line_bringing(file, brings)).then_some(&value[..])
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.verdicts.iter().all(Vec::is_empty)
    }

    /// What differs from `other`: the names whose values do, and per file
    /// whether its includes that are certainly compiled do (what a file is
    /// told of a name depends on those of its own alone).
    pub fn changes(&self, other: &Source) -> (FxHashSet<Vec<u8>>, Vec<bool>) {
        let mut names: FxHashSet<Vec<u8>> = self.names.iter().filter(|(k, v)| other.names.get(*k) != Some(*v)).map(|(k, _)| k.clone()).collect();
        names.extend(other.names.keys().filter(|k| !self.names.contains_key(*k)).cloned());
        let files = (0..self.includes.len().max(other.includes.len())).map(|i| self.includes.get(i) != other.includes.get(i)).collect();
        (names, files)
    }

    fn digest(&self) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed as h;
        let each = |(k, values): (&Vec<u8>, &Vec<(Vec<u8>, Vec<u64>, Vec<u64>)>)| {
            values.iter().fold(0, |acc, (v, known, brings)| acc ^ h(bytemuck::cast_slice(brings), h(bytemuck::cast_slice(known), h(v, h(k, 15)))))
        };
        let lines = self.includes.iter().enumerate().fold(0, |acc, (f, list)| acc ^ list.iter().fold(0, |a, (l, t)| a ^ h(&t.to_le_bytes(), ((f as u64) << 32) | *l as u64)));
        let told = self.verdicts.iter().flatten().fold(0u64, |d, &(at, holds)| h(&at.to_le_bytes(), d.rotate_left(1) + holds as u64));
        self.names.iter().map(each).fold(self.files.len() as u64 ^ lines ^ told, |a, b| a ^ b)
    }

    /// What `file` is told: other facts come of it when this changes.
    pub fn file_digest(&self, path: &str) -> u64 {
        use xxhash_rust::xxh3::xxh3_64_with_seed as h;
        let Some(file) = self.file(path) else { return 0 };
        let mut d = self.verdicts.get(file as usize).map_or(0, |v| v.iter().fold(0, |d, &(at, holds)| h(&at.to_le_bytes(), d + 17 + holds as u64)));
        for (k, values) in &self.names {
            for (v, known, brings) in values {
                if has(known, file) {
                    d ^= h(v, h(k, 16 + self.line_bringing(file, brings) as u64));
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
            .flat_map(|(k, values)| values.iter().map(move |(v, bits, _)| (text(k), text(v), bits.iter().map(|w| w.count_ones() as usize).sum())))
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
#[derive(Clone, Copy)]
struct View<'a> {
    c: &'a Conditions,
    local: &'a Local,
    /// The file, as [`Source`] numbers it, and the line.
    at: Option<(u32, u32)>,
    /// How many macro values deep this is being read (a macro whose value
    /// names itself, or one that names it, is not read forever).
    depth: u8,
    /// The values of names worked out so far in this condition, each once:
    /// macros whose values name others, which name the same ones in turn,
    /// would otherwise be read over and over, as many times as there are
    /// ways down to them (exponentially many). A name being worked out is
    /// in it with no value, so one whose value comes back to it has none.
    memo: &'a std::cell::RefCell<FxHashMap<Vec<u8>, Option<Int>>>,
    /// Where asked: every name looked up, for the condition comes out the
    /// same again wherever these are as they were.
    noted: Option<&'a std::cell::RefCell<Vec<Vec<u8>>>>,
}

impl View<'_> {
    /// From the sources, where the settings say nothing of the name.
    fn sourced(&self, name: &[u8]) -> Option<&[u8]> {
        let (file, line) = self.at?;
        self.c.source.as_ref()?.get(file, line, name)
    }

    fn note(&self, name: &[u8]) {
        if let Some(noted) = self.noted {
            noted.borrow_mut().push(name.to_vec());
        }
    }

    fn defined(&self, name: &[u8]) -> Tri {
        self.note(name);
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
        self.note(name);
        match self.local.get(name) {
            Some(Some(v)) => Some(v.as_deref()),
            Some(None) => None,
            None => match self.c.defined.get(name).map(|v| &v[..]).or(if self.c.undefined.contains(name) { None } else { self.sourced(name) }) {
                Some(v) => Some(Some(v)),
                None => (self.c.defined(name) == Tri::False).then_some(None),
            },
        }
    }

    /// The value of a name in an `#if`: its definition if that is a number,
    /// or a name or an expression that has one (`(0 && OTHER)` is 0 whatever
    /// OTHER is), 0 if it is known not to be defined.
    fn value(&self, name: &[u8], depth: u8) -> Option<Int> {
        match self.text(name)? {
            None => Some(Int::signed(0)),
            // (A macro with parameters named without arguments: no value.)
            Some(v) if v.first() == Some(&0) => None,
            Some(v) if v.is_empty() || v == name || depth > 32 => number(v),
            Some(v) => number(v).or_else(|| {
                if let Some(known) = self.memo.borrow().get(name) {
                    return *known;
                }
                self.memo.borrow_mut().insert(name.to_vec(), None);
                COUNTS.with(|c| c.set((c.get().0 + 1, c.get().1)));
                // Read as an `#if` would read it, its names in turn.
                let toks = tokens(v);
                let inner = View { depth: depth + 1, ..*self };
                let mut p = Parser { c: inner, toks: &toks, at: 0 };
                let value = p.ternary();
                let value = (p.at == toks.len()).then_some(value).flatten();
                self.memo.borrow_mut().insert(name.to_vec(), value);
                value
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    False,
    True,
    Unknown,
}

impl Not for Tri {
    type Output = Tri;

    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

impl Tri {
    fn of(b: bool) -> Tri {
        if b { Tri::True } else { Tri::False }
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

/// A macro the compiler defines of itself, never in a build's `-D`s:
/// `__CC_ARM`, `__GNUC__`, `_WIN32`, `_MSC_VER`. What a build does not
/// define is not defined, but these it may, unwritten.
pub(crate) fn builtin(name: &[u8]) -> bool {
    // The kinds compilers define: which compiler, which machine, the sizes
    // of types, the standard. (A project's own `__CMSIS_GENERIC` is none.)
    const KINDS: [&[u8]; 58] = [
        b"__GNUC", b"__clang", b"__llvm", b"__CC_ARM", b"__ARMCC", b"__ARMCOMPILER", b"__ICC", b"__IAR", b"__TI_", b"__ti__", b"__TASKING", b"__CSMC", b"__SDCC", b"__XC",
        b"__EDG", b"__INTEL", b"__MINGW", b"__CYGWIN", b"__ARM", b"__arm", b"__thumb", b"__aarch64", b"__x86", b"__i386", b"__i486", b"__i586", b"__i686", b"__amd64",
        b"__riscv", b"__mips", b"__ppc", b"__powerpc", b"__linux", b"__unix", b"__APPLE", b"__WIN", b"__ELF", b"__SIZEOF_", b"__INT", b"__UINT", b"__CHAR", b"__SCHAR",
        b"__SHRT", b"__LONG", b"__WCHAR", b"__PTRDIFF", b"__FLT", b"__DBL", b"__LDBL", b"__BYTE_ORDER", b"__ORDER_", b"__LITTLE_ENDIAN", b"__BIG_ENDIAN", b"__STDC",
        b"__cplusplus", b"__OPTIMIZE", b"__VFP", b"__SOFTFP",
    ];
    KINDS.iter().any(|k| name.starts_with(k))
        || name.starts_with(b"_M_")
        || matches!(name, b"_WIN32" | b"_WIN64" | b"_MSC_VER" | b"_MSC_FULL_VER" | b"_LP64" | b"_ILP32" | b"_CHAR_UNSIGNED" | b"__LP64__" | b"__ILP32__" | b"__FP_FAST_FMA" | b"__FPU_USED")
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
        // A configuration of the build's own description (an IDE's project,
        // the C/C++ extension's) lists all the build defines: what it does
        // not, the build does not (see `builtin`). (A compilation database
        // has what every command defines; one may define more.)
        let whole = |file: &String| {
            let (path, config) = split_entry(file);
            config.is_some() && matches!(path.rsplit(['/', '\\']).next(), Some(".cproject" | "c_cpp_properties.json"))
        };
        c.assume_built = settings.condition_files.iter().any(whole);
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
        c.undecided_off = settings.undecided.as_deref() == Some("off");
        c
    }

    /// `NAME` as `value`; `NAME(a, b)` (`-D'MAX(a,b)=...'`) a macro with
    /// parameters.
    pub fn define(&mut self, name: &str, value: &str) {
        let (name, value) = match name.split_once('(') {
            Some((n, rest)) => {
                if rest.contains("...") {
                    return;
                }
                let params: Vec<&[u8]> = rest.trim_end().trim_end_matches(')').split(',').map(|p| p.trim().as_bytes()).filter(|p| !p.is_empty()).collect();
                (n.trim(), function_value(&params, value.as_bytes()).unwrap_or_else(|| b"(".to_vec()))
            }
            None => (name, value.trim().as_bytes().to_vec()),
        };
        self.undefined.remove(name.as_bytes());
        self.defined.insert(name.as_bytes().to_vec(), value);
    }

    pub fn undefine(&mut self, name: &str) {
        self.defined.remove(name.as_bytes());
        self.undefined.insert(name.as_bytes().to_vec());
    }

    /// A condition file as the settings name it: a path, and for a build
    /// description the configuration meant after `#` (else its first).
    fn read_source(&mut self, root: &Path, entry: &str) {
        let (path, wanted) = split_entry(entry);
        // A file kept in the settings folder, written down when the folder
        // was called `.si`: it is where the folder is now.
        let moved = path.strip_prefix(".si/").or(path.strip_prefix(".si\\")).map(|rest| crate::scope::home(root).join(rest));
        let at = match moved {
            Some(moved) if !root.join(path).exists() => moved,
            _ => root.join(path),
        };
        let Ok(text) = std::fs::read(at) else { return };
        let text = String::from_utf8_lossy(&text);
        let Some(builds) = builds_of(path, &text) else { return self.read_file(&text) };
        let Some(b) = builds.iter().find(|b| wanted.is_none_or(|w| b.name == w)) else { return };
        for d in &b.defines {
            // `-DNAME` is `#define NAME 1`.
            let (name, value) = d.split_once('=').unwrap_or((d, "1"));
            self.define(name.trim(), value);
        }
        for u in &b.undefines {
            self.undefine(u.trim());
        }
        if let Some(compiler) = b.compiler {
            let own = compiler_defines(compiler);
            for (name, value) in own {
                if !self.settles(name.as_bytes()) {
                    self.define(name, value);
                }
            }
            for mark in COMPILER_MARKS.iter().filter(|m| !own.iter().any(|o| o.0 == **m)) {
                if !self.settles(mark.as_bytes()) {
                    self.undefine(mark);
                }
            }
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
                    if end > 0 && d[end..].starts_with('(') {
                        // A macro with parameters, called in a condition.
                        if let Some(close) = d[end..].find(')') {
                            self.define(&d[..end + close + 1], d[end + close + 1..].split("/*").next().unwrap_or(""));
                        }
                    } else if end > 0 {
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
        self.defined.is_empty() && self.undefined.is_empty() && self.assume_undefined.is_empty() && !self.assume_all && !self.assume_built && !self.undecided_off
    }

    /// Do these conditions assume anything about names they do not list?
    /// Then they need to be told which names the sources define.
    pub fn assumes(&self) -> bool {
        self.assume_all || self.assume_built || !self.assume_undefined.is_empty()
    }

    pub fn set_in_source(&mut self, names: FxHashSet<Vec<u8>>) {
        self.in_source = names;
    }

    /// Are macros the sources define to one value to be taken as known?
    pub fn uses_source(&self) -> bool {
        !self.no_source
    }

    /// Do the settings themselves say whether `name` is defined?
    /// The same conditions, with `names` not defined where the settings
    /// do not say they are.
    pub fn with_undefined(&self, names: impl IntoIterator<Item = Vec<u8>>) -> Conditions {
        let mut c = self.clone();
        c.undefined.extend(names.into_iter().filter(|n| !self.settles(n)));
        c
    }

    /// Whether some source file defines `name`.
    pub fn in_source(&self, name: &[u8]) -> bool {
        self.in_source.contains(name)
    }

    pub fn settles(&self, name: &[u8]) -> bool {
        self.defined.contains_key(name) || self.undefined.contains(name)
    }

    /// The value the settings give `name`, when they define it.
    pub fn setting(&self, name: &[u8]) -> Option<&[u8]> {
        self.defined.get(name).map(|v| &v[..])
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
        let a = self.assume_undefined.iter().fold(self.assume_all as u64 | (self.assume_built as u64) << 1, |acc, k| acc ^ h(k, 13));
        let s = self.in_source.iter().fold(0, |acc, k| acc ^ h(k, 14));
        let c = self.source.as_ref().map_or(0, |s| s.digest()) ^ self.no_source as u64 ^ (self.undecided_off as u64) << 1;
        d ^ u ^ a ^ s ^ c
    }

    pub fn defined(&self, name: &[u8]) -> Tri {
        if self.defined.contains_key(name) {
            Tri::True
        } else if self.undefined.contains(name)
            || self.undecided_off
            || !self.in_source.contains(name) && (self.assume_all || self.assume_built && !builtin(name) || self.assume_undefined.iter().any(|p| name.starts_with(p)))
        {
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
    /// above it. `at` is the file (as the [`Source`] numbers it) and line.
    pub fn eval_in(&self, local: &Local, at: Option<(u32, u32)>, expr: &[u8]) -> Tri {
        self.eval_noting(local, at, expr, None)
    }

    /// The same, noting in `noted` every name the condition comes to
    /// depend on: all it was worked out from, besides the settings.
    pub fn eval_noting(&self, local: &Local, at: Option<(u32, u32)>, expr: &[u8], noted: Option<&std::cell::RefCell<Vec<Vec<u8>>>>) -> Tri {
        let toks = tokens(expr);
        let memo = Default::default();
        let mut p = Parser { c: View { c: self, local, at, depth: 0, memo: &memo, noted }, toks: &toks, at: 0 };
        let v = p.ternary();
        if p.at != toks.len() {
            return Tri::Unknown;
        }
        truth(v)
    }

    /// The value of an expression as an `#if` works it out, names taking
    /// their values from `local`: the number, and whether it is unsigned.
    /// `at`: the file (as the [`Source`] numbers it) and line it is
    /// written on, for what the sources settle there.
    pub fn number_in(&self, local: &Local, at: Option<(u32, u32)>, expr: &[u8]) -> Option<(i64, bool)> {
        let toks = tokens(expr);
        let memo = Default::default();
        let mut p = Parser { c: View { c: self, local, at, depth: 0, memo: &memo, noted: None }, toks: &toks, at: 0 };
        let v = p.ternary()?;
        (p.at == toks.len()).then_some((v.v, v.unsigned))
    }

    pub fn defined_in(&self, local: &Local, at: Option<(u32, u32)>, name: &[u8]) -> Tri {
        View { c: self, local, at, depth: 0, memo: &Default::default(), noted: None }.defined(name)
    }
}

/// A macro's body as a value: comments out, spaces closed up, the
/// parentheses around all of it off. (However long: an `#if` may need all
/// of it, and a value cut short would be read wrong.)
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
    Some(v.to_vec())
}

/// A value in an `#if`: C's `intmax_t`, or `uintmax_t` where `unsigned`
/// (the bits the same).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Int {
    v: i64,
    unsigned: bool,
}

impl Int {
    fn signed(v: i64) -> Int {
        Int { v, unsigned: false }
    }

    /// Of two operands, as C converts them: unsigned if either is.
    fn arith(a: Int, b: Int, signed: fn(i64, i64) -> Option<i64>, unsigned: fn(u64, u64) -> Option<u64>) -> Option<Int> {
        if a.unsigned || b.unsigned {
            unsigned(a.v as u64, b.v as u64).map(|v| Int { v: v as i64, unsigned: true })
        } else {
            signed(a.v, b.v).map(Int::signed)
        }
    }

    fn bits(a: Int, b: Int, f: fn(i64, i64) -> i64) -> Option<Int> {
        Some(Int { v: f(a.v, b.v), unsigned: a.unsigned || b.unsigned })
    }

    fn compare(a: Int, b: Int, holds: fn(std::cmp::Ordering) -> bool) -> Option<Int> {
        let order = if a.unsigned || b.unsigned { (a.v as u64).cmp(&(b.v as u64)) } else { a.v.cmp(&b.v) };
        Some(Int::signed(holds(order) as i64))
    }

    /// How far a shift goes, if it is one C defines.
    fn count(b: Int) -> Option<u32> {
        u32::try_from(b.v).ok().filter(|&c| c < 64 && (b.unsigned || b.v >= 0))
    }

    /// The text of the value, to read again (in a macro's arguments).
    fn text(self) -> String {
        match (self.unsigned, self.v < 0) {
            (true, _) => format!("{}u", self.v as u64),
            (false, true) => format!("({})", self.v),
            (false, false) => self.v.to_string(),
        }
    }
}

/// An integer constant as C writes it (decimal, octal `017`, hex `0x1F`,
/// binary `0b101`; suffixes `u`, `l`, `ll` in any case and order), or a
/// character constant (`'a'`, `'\n'`, `L'x'`).
fn number(v: &[u8]) -> Option<Int> {
    let s = std::str::from_utf8(v).ok()?.trim();
    if s.ends_with('\'') {
        return character(s);
    }
    let digits = s.trim_end_matches(['u', 'U', 'l', 'L']);
    let suffix = &s[digits.len()..];
    let (us, ls) = (suffix.matches(['u', 'U']).count(), suffix.matches(['l', 'L']).count());
    if us > 1 || ls > 2 || (ls == 2 && !(suffix.contains("ll") || suffix.contains("LL"))) {
        return None;
    }
    let (radix, body) = if let Some(h) = digits.strip_prefix("0x").or(digits.strip_prefix("0X")) {
        (16, h)
    } else if let Some(b) = digits.strip_prefix("0b").or(digits.strip_prefix("0B")) {
        (2, b)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let n = u64::from_str_radix(body, radix).ok()?;
    // One too large for `intmax_t` is `uintmax_t`.
    Some(Int { v: n as i64, unsigned: us == 1 || n > i64::MAX as u64 })
}

/// `'a'`, `'\x41'`, `'\0'`, `L'a'`: a single character's value, a plain
/// one as a signed `char` (as gcc and clang have it). One of several
/// characters (`'ab'`) has no value the standard gives.
fn character(s: &str) -> Option<Int> {
    let (wide, rest) = match s.find('\'') {
        Some(0) => (false, &s[1..]),
        Some(at) if matches!(&s[..at], "L" | "u" | "U" | "u8") => (&s[..at] != "u8", &s[at + 1..]),
        _ => return None,
    };
    let inner = rest.strip_suffix('\'')?.as_bytes();
    let (c, used) = match inner {
        [b'\\', b'x', hex @ ..] if !hex.is_empty() => (u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?, inner.len()),
        [b'\\', b'0'..=b'7', ..] => {
            let n = inner[1..].iter().take(3).take_while(|b| (b'0'..=b'7').contains(*b)).count();
            (u32::from_str_radix(std::str::from_utf8(&inner[1..1 + n]).ok()?, 8).ok()?, 1 + n)
        }
        [b'\\', e, ..] => (
            match e {
                b'n' => 10,
                b't' => 9,
                b'r' => 13,
                b'a' => 7,
                b'b' => 8,
                b'f' => 12,
                b'v' => 11,
                b'\\' | b'\'' | b'"' | b'?' => *e as u32,
                _ => return None,
            },
            2,
        ),
        [c, ..] if c.is_ascii() => (*c as u32, 1),
        _ => return None,
    };
    if used != inner.len() {
        return None;
    }
    let v = if wide { c as i64 } else { u8::try_from(c).ok()? as i8 as i64 };
    Some(Int::signed(v))
}

fn truth(v: Option<Int>) -> Tri {
    v.map_or(Tri::Unknown, |n| Tri::of(n.v != 0))
}

#[derive(Clone, Debug, PartialEq)]
enum Tok<'a> {
    Ident(&'a [u8]),
    Num(Int),
    Op(&'a [u8]),
}

impl Tok<'_> {
    fn text(&self) -> std::borrow::Cow<'_, [u8]> {
        match self {
            Tok::Ident(t) | Tok::Op(t) => std::borrow::Cow::Borrowed(t),
            Tok::Num(n) => std::borrow::Cow::Owned(n.text().into_bytes()),
        }
    }
}

/// How a macro with parameters is kept among the values of names: `\0`,
/// its parameters with commas between, `\0`, its body as `value_of` has it.
pub fn function_value(params: &[&[u8]], body: &[u8]) -> Option<Vec<u8>> {
    let body = value_of(body)?;
    Some([&b"\0"[..], &params.join(&b","[..]), b"\0", &body].concat())
}

fn function_parts(v: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
    let rest = v.strip_prefix(b"\0")?;
    let at = rest.iter().position(|&b| b == 0)?;
    let params = if at == 0 { Vec::new() } else { rest[..at].split(|&b| b == b',').collect() };
    Some((params, &rest[at + 1..]))
}

thread_local! {
    /// For `SI_COND_PROFILE`: macro values read, and bytes tokenized, on
    /// this thread.
    pub static COUNTS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn tokens(s: &[u8]) -> Vec<Tok<'_>> {
    COUNTS.with(|c| c.set((c.get().0, c.get().1 + s.len() as u64)));
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
        } else if let Some(prefix) = char_prefix(&s[i..]) {
            // A character constant, `L'x'` and the like too.
            let a = i;
            i += prefix + 1;
            while i < s.len() && s[i] != b'\'' {
                i += if s[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(s.len());
            out.push(number(&s[a..i]).map_or(Tok::Op(&s[a..i]), Tok::Num));
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
            let two = s.get(i..i + 2).filter(|t| matches!(*t, b"&&" | b"||" | b"==" | b"!=" | b"<=" | b">=" | b"<<" | b">>"));
            let len = if two.is_some() { 2 } else { 1 };
            out.push(Tok::Op(&s[i..i + len]));
            i += len;
        }
    }
    out
}

/// Of a character constant at the start of `s` (`'x'`, `L'x'`, `u8'x'`),
/// how long the prefix before the quote is.
fn char_prefix(s: &[u8]) -> Option<usize> {
    [&b""[..], b"u8", b"L", b"u", b"U"].iter().find(|p| s.starts_with(p) && s.get(p.len()) == Some(&b'\'')).map(|p| p.len())
}

/// The operators of C's `#if`, by their precedence: `?:` over `||` over
/// `&&` over `|` `^` `&` over comparisons over shifts over `+ -` over
/// `* / %` over the unary ones over primaries, in three-valued logic:
/// `None` is a value that is not known (as is one that overflows, or a
/// division by zero). Anything else in a condition (a macro with arguments
/// the build does not define) is not known either.
struct Parser<'a> {
    c: View<'a>,
    toks: &'a [Tok<'a>],
    at: usize,
}

/// One level of binary operators: each with what it makes of two values.
type Level = &'static [(&'static [u8], fn(Int, Int) -> Option<Int>)];

impl Parser<'_> {
    fn op(&mut self, text: &[u8]) -> bool {
        let hit = matches!(self.toks.get(self.at), Some(Tok::Op(o)) if *o == text);
        self.at += hit as usize;
        hit
    }

    /// `c ? a : b`; of a condition not known, the value both give, if they
    /// give the same.
    fn ternary(&mut self) -> Option<Int> {
        let c = self.or();
        if !self.op(b"?") {
            return c;
        }
        let a = self.ternary();
        if !self.op(b":") {
            self.at = usize::MAX / 2;
            return None;
        }
        let b = self.ternary();
        // (Of the type both arms are converted to.)
        let unsigned = a.is_some_and(|a| a.unsigned) || b.is_some_and(|b| b.unsigned);
        let common = |x: Option<Int>| x.map(|x| Int { unsigned, ..x });
        match truth(c) {
            Tri::True => common(a),
            Tri::False => common(b),
            Tri::Unknown => common(a).filter(|_| a == b),
        }
    }

    fn or(&mut self) -> Option<Int> {
        let mut v = self.and();
        while self.op(b"||") {
            let r = self.and();
            v = as_value(truth(v).or(truth(r)));
        }
        v
    }

    fn and(&mut self) -> Option<Int> {
        let mut v = self.bits();
        while self.op(b"&&") {
            let r = self.bits();
            v = as_value(truth(v).and(truth(r)));
        }
        v
    }

    /// Left to right, the operators of each level over those of the next.
    fn level(&mut self, levels: &[Level]) -> Option<Int> {
        let Some((ops, next)) = levels.split_first() else { return self.unary() };
        let mut v = self.level(next);
        'more: loop {
            for (text, f) in ops.iter() {
                if self.op(text) {
                    let r = self.level(next);
                    v = match (v, r) {
                        (Some(a), Some(b)) => f(a, b),
                        _ => None,
                    };
                    continue 'more;
                }
            }
            return v;
        }
    }

    fn bits(&mut self) -> Option<Int> {
        use std::cmp::Ordering;
        const LEVELS: &[Level] = &[
            &[(b"|", |a, b| Int::bits(a, b, |a, b| a | b))],
            &[(b"^", |a, b| Int::bits(a, b, |a, b| a ^ b))],
            &[(b"&", |a, b| Int::bits(a, b, |a, b| a & b))],
            &[(b"==", |a, b| Int::compare(a, b, Ordering::is_eq)), (b"!=", |a, b| Int::compare(a, b, Ordering::is_ne))],
            &[
                (b"<=", |a, b| Int::compare(a, b, Ordering::is_le)),
                (b">=", |a, b| Int::compare(a, b, Ordering::is_ge)),
                (b"<", |a, b| Int::compare(a, b, Ordering::is_lt)),
                (b">", |a, b| Int::compare(a, b, Ordering::is_gt)),
            ],
            // (Of the left operand's type; a negative one shifted left, or
            // a bit shifted out of a signed one, is no value C defines.)
            &[
                (b"<<", |a, b| {
                    let c = Int::count(b)?;
                    if a.unsigned {
                        return Some(Int { v: ((a.v as u64) << c) as i64, unsigned: true });
                    }
                    let v = (a.v >= 0).then(|| (a.v as i128) << c)?;
                    i64::try_from(v).ok().map(Int::signed)
                }),
                (b">>", |a, b| {
                    let c = Int::count(b)?;
                    Some(if a.unsigned { Int { v: ((a.v as u64) >> c) as i64, unsigned: true } } else { Int::signed(a.v >> c) })
                }),
            ],
            &[
                (b"+", |a, b| Int::arith(a, b, i64::checked_add, |a, b| Some(a.wrapping_add(b)))),
                (b"-", |a, b| Int::arith(a, b, i64::checked_sub, |a, b| Some(a.wrapping_sub(b)))),
            ],
            &[
                (b"*", |a, b| Int::arith(a, b, i64::checked_mul, |a, b| Some(a.wrapping_mul(b)))),
                (b"/", |a, b| Int::arith(a, b, i64::checked_div, u64::checked_div)),
                (b"%", |a, b| Int::arith(a, b, i64::checked_rem, u64::checked_rem)),
            ],
        ];
        self.level(LEVELS)
    }

    fn unary(&mut self) -> Option<Int> {
        if self.op(b"!") {
            return as_value(truth(self.unary()).not());
        }
        if self.op(b"-") {
            let v = self.unary()?;
            return if v.unsigned { Some(Int { v: v.v.wrapping_neg(), unsigned: true }) } else { v.v.checked_neg().map(Int::signed) };
        }
        if self.op(b"~") {
            return self.unary().map(|v| Int { v: !v.v, ..v });
        }
        if self.op(b"+") {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Option<Int> {
        match self.toks.get(self.at).cloned() {
            Some(Tok::Num(n)) => {
                self.at += 1;
                Some(n)
            }
            Some(Tok::Op(b"(")) => {
                self.at += 1;
                let v = self.ternary();
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
                    return self.c.value(name, self.c.depth);
                }
                // A macro with arguments: its arguments, split at the
                // commas outside parentheses.
                let mut args: Vec<std::ops::Range<usize>> = Vec::new();
                let (mut depth, mut from) = (1, self.at);
                while self.at < self.toks.len() {
                    match &self.toks[self.at] {
                        Tok::Op(b"(") => depth += 1,
                        Tok::Op(b")") => {
                            depth -= 1;
                            if depth == 0 {
                                args.push(from..self.at);
                                self.at += 1;
                                break;
                            }
                        }
                        Tok::Op(b",") if depth == 1 => {
                            args.push(from..self.at);
                            from = self.at + 1;
                        }
                        _ => {}
                    }
                    self.at += 1;
                }
                if depth != 0 {
                    self.at = usize::MAX / 2;
                    return None;
                }
                if let Some((params, body)) = self.c.text(name).flatten().and_then(function_parts) {
                    return self.expand(&params, body, &args);
                }
                // The kernel's own.
                let arg = match &args[..] {
                    [one] if one.len() == 1 => match self.toks[one.start] {
                        Tok::Ident(a) => Some(a),
                        _ => None,
                    },
                    _ => None,
                };
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

impl Parser<'_> {
    /// A macro with parameters called in a condition: its body with each
    /// parameter put for by its argument, read as a condition. (Not one
    /// that pastes or quotes: `#` and `##` make it unknown.)
    fn expand(&self, params: &[&[u8]], body: &[u8], args: &[std::ops::Range<usize>]) -> Option<Int> {
        let none = params.is_empty() && args.len() == 1 && args[0].is_empty();
        if self.c.depth > 8 || (params.len() != args.len() && !none) {
            return None;
        }
        let mut text: Vec<u8> = Vec::new();
        for t in tokens(body) {
            match t {
                Tok::Ident(n) if params.contains(&n) => {
                    let arg = &args[params.iter().position(|p| *p == n)?];
                    text.push(b'(');
                    for t in &self.toks[arg.clone()] {
                        text.extend_from_slice(&t.text());
                        text.push(b' ');
                    }
                    text.push(b')');
                }
                t => text.extend_from_slice(&t.text()),
            }
            text.push(b' ');
        }
        let toks = tokens(&text);
        let inner = View { depth: self.c.depth + 1, ..self.c };
        let mut p = Parser { c: inner, toks: &toks, at: 0 };
        let value = p.ternary();
        (p.at == toks.len()).then_some(value).flatten()
    }
}

fn as_value(t: Tri) -> Option<Int> {
    match t {
        Tri::True => Some(Int::signed(1)),
        Tri::False => Some(Int::signed(0)),
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

    /// What C99 (6.10.1) lets an `#if` say, and what it comes to.
    #[test]
    fn a_condition_is_worked_out_as_c99_has_it() {
        let mut c = Conditions::default();
        c.read_file(
            "#define VER 2\n#define FLAGS 5\n#define NEW (VER + 0 == 2)\n#define MK(a, b) (((a) << 16) | (b))\n\
             #define KV(a, b, c) (((a) << 16) + ((b) << 8) + (c))\n#define CODE KV(5, 4, 0)\n#define ZERO() 0\n\
             #define TWICE(x) MK(0, x) * 2\n#define VA(...) 1\n#define OCT 010\n",
        );
        c.define("SQ(x)", "((x) * (x))");
        for (expr, want) in [
            // Integer constants: decimal, octal, hex, binary; suffixes.
            ("010 == 8", Tri::True),
            ("OCT == 8", Tri::True),
            ("0x1F == 31 && 0X1f == 31", Tri::True),
            ("0b101 == 5", Tri::True),
            ("10u == 10 && 10UL == 10 && 10ull == 10 && 10LLU == 10 && 10lu == 10", Tri::True),
            ("08", Tri::Unknown),
            ("10uu", Tri::Unknown),
            ("10lL", Tri::Unknown),
            ("1.0", Tri::Unknown),
            // Unsigned arithmetic: the usual conversions.
            ("-1 > 0u", Tri::True),
            ("-1 > 0", Tri::False),
            ("0xFFFFFFFFFFFFFFFF == -1", Tri::True),
            ("0xFFFFFFFFFFFFFFFF > 0", Tri::True),
            ("-1u == 0xFFFFFFFFFFFFFFFF", Tri::True),
            ("0u - 1 > 0", Tri::True),
            ("(1 ? -1 : 0u) > 0", Tri::True),
            // Character constants.
            ("'a' == 97", Tri::True),
            (r"'\n' == 10 && '\0' == 0 && '\x41' == 65 && '\101' == 65 && '\'' == 39", Tri::True),
            (r"'\377' < 0", Tri::True),
            ("L'a' == 97", Tri::True),
            ("'ab'", Tri::Unknown),
            // Operators, by precedence.
            ("VER + 0 == 2", Tri::True),
            ("NEW", Tri::True),
            ("(FLAGS & 1) == 1", Tri::True),
            ("FLAGS & 2", Tri::False),
            ("2 + 3 * 4 == 14 && (2 + 3) * 4 == 20", Tri::True),
            ("1 | 2 ^ 3 & 1", Tri::True),
            ("7 / 2 == 3 && -7 / 2 == -3 && -7 % 2 == -1", Tri::True),
            ("1 << 4 == 16 && -16 >> 2 == -4 && 0x80000000u >> 31 == 1", Tri::True),
            ("-VER < 0 && ~0 == -1 && +VER == 2 && !VER == 0", Tri::True),
            ("1 < 2 == 1", Tri::True),
            ("VER == 2 ? 1 : 0", Tri::True),
            ("VER == 3 ? 1 : 0", Tri::False),
            ("0 ? 1 : 2 ? 3 : 0", Tri::True),
            // What C leaves undefined, or what is not known, is unknown...
            ("UNKNOWN + 1", Tri::Unknown),
            ("1 / 0", Tri::Unknown),
            ("VER % 0", Tri::Unknown),
            ("1 << 64", Tri::Unknown),
            ("1 << -1", Tri::Unknown),
            ("-1 << 1", Tri::Unknown),
            ("0x7FFFFFFFFFFFFFFF + 1", Tri::Unknown),
            ("1, 2", Tri::Unknown),
            // ...unless what is known decides it.
            ("0 && UNKNOWN + 1", Tri::False),
            ("1 || 1 / 0", Tri::True),
            ("0 && 1 / 0", Tri::False),
            ("UNKNOWN ? 1 : 1", Tri::True),
            ("1 ? 2 : 1 / 0", Tri::True),
            // Macros with parameters.
            ("MK(1, 2) == 65538", Tri::True),
            ("CODE >= KV(5, 0, 0) && CODE < KV(6, 0, 0)", Tri::True),
            ("KV(4, 19, 0) > CODE", Tri::False),
            ("SQ(VER + 1) == 9", Tri::True),
            ("TWICE((1, 2))", Tri::Unknown),
            ("TWICE(3) == 6", Tri::True),
            ("ZERO() == 0", Tri::True),
            ("MK(1) == 1", Tri::Unknown),
            ("VA(1)", Tri::Unknown),
            ("defined(MK) && defined SQ", Tri::True),
            ("NOT_DEFINED(1)", Tri::Unknown),
        ] {
            assert_eq!(c.eval(expr.as_bytes()), want, "{expr}");
        }
    }

    #[test]
    fn a_configuration_file_is_read_both_ways() {
        let c = conditions();
        assert_eq!(c.defined(b"CONFIG_SMP"), Tri::True);
        assert_eq!((c.defined(b"CONFIG_NET"), c.defined(b"CONFIG_NET_MODULE")), (Tri::False, Tri::True));
        assert_eq!(c.defined(b"CONFIG_DEBUG"), Tri::False);
        assert_eq!(c.defined(b"CONFIG_NEVER_MENTIONED"), Tri::False, "by the prefix");
        assert_eq!((c.defined(b"HAVE_X"), c.defined(b"HAVE_Y"), c.defined(b"HAVE_Z")), (Tri::True, Tri::False, Tri::Unknown));
        assert_eq!(c.defined(b"MAX"), Tri::True, "a macro with parameters is defined too");
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
    fn a_build_description_says_which_compiler_and_so_what_it_defines_of_itself() {
        let dir = std::env::temp_dir().join(format!("si-compiler-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".vscode")).unwrap();
        let cproject = r#"<cproject><configuration name="Debug"><folderInfo><toolChain id="com.arm.toolchain.ac5.exe.debug.1" name="ARM Compiler 5 (DS-5 built in)" superClass="com.arm.toolchain.ac5.exe.debug"><tool name="ARM C Compiler 5"><option valueType="definedSymbols"><listOptionValue value="CHIP=5037"/></option></tool></toolChain></folderInfo></configuration></cproject>"#;
        std::fs::write(dir.join(".cproject"), cproject).unwrap();
        std::fs::write(dir.join(".vscode/c_cpp_properties.json"), r#"{ "configurations": [ { "name": "Arm", "compilerPath": "C:/arm/bin/arm-none-eabi-gcc.exe", "defines": ["X"] } ] }"#).unwrap();
        std::fs::write(dir.join("Makefile"), "PREFIX = arm-none-eabi-
CC = $(PREFIX)gcc
C_DEFS = -DUSE_HAL_DRIVER
").unwrap();
        let of = |entry: &str| Conditions::from_settings(&Settings { condition_files: vec![entry.into()], ..Settings::default() }, &dir);
        let c = of(".cproject#Debug");
        assert_eq!((c.defined(b"__CC_ARM"), c.defined(b"__GNUC__"), c.defined(b"__ICCARM__"), c.defined(b"CHIP")), (Tri::True, Tri::False, Tri::False, Tri::True));
        // As CMSIS picks its header.
        assert_eq!(c.eval(b"defined(__CC_ARM)"), Tri::True);
        assert_eq!(c.eval(b"defined (__ARMCC_VERSION) && (__ARMCC_VERSION >= 6010050)"), Tri::False);
        let c = of(".vscode/c_cpp_properties.json#Arm");
        assert_eq!((c.defined(b"__GNUC__"), c.defined(b"__CC_ARM")), (Tri::True, Tri::False));
        let c = of("Makefile");
        assert_eq!((c.defined(b"__GNUC__"), c.defined(b"__CC_ARM"), c.defined(b"USE_HAL_DRIVER")), (Tri::True, Tri::False, Tri::True));
        // Arm Compiler 6, however a description names it: CMSIS takes
        // cmsis_armclang.h, not the GCC header armclang's __GNUC__ would.
        for name in ["ARM Compiler 6 (DS-5 built in)", "Arm Compiler 6", "com.arm.toolchain.v6.exe.debug", "com.arm.toolchain.ac6.base", "C:/Keil_v5/ARM/ARMCLANG/bin/armclang.exe"] {
            assert_eq!(compiler_named(name), Some(Compiler::Armclang), "{name}");
        }
        for name in ["ARM Compiler 5 (DS-5 built in)", "com.arm.toolchain.ac5.exe.debug", "C:/Keil_v5/ARM/ARMCC/bin/armcc.exe"] {
            assert_eq!(compiler_named(name), Some(Compiler::Armcc), "{name}");
        }
        std::fs::write(dir.join(".vscode/c_cpp_properties.json"), r#"{ "configurations": [ { "name": "AC6", "compilerPath": "C:/Program Files/Arm/Development Studio/sw/ARMCompiler6.18/bin/armclang.exe" } ] }"#).unwrap();
        let c = of(".vscode/c_cpp_properties.json#AC6");
        assert_eq!((c.defined(b"__ARMCC_VERSION"), c.defined(b"__clang__"), c.defined(b"__CC_ARM")), (Tri::True, Tri::True, Tri::False));
        assert_eq!(c.eval(b"defined (__ARMCC_VERSION) && (__ARMCC_VERSION >= 6100100)"), Tri::True);
        std::fs::remove_dir_all(&dir).ok();
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
        // A configuration lists what the build defines: what only another
        // defines is not defined here; what the compiler does of itself may be.
        assert_eq!((c.defined(b"NDEBUG"), c.defined(b"DEBUG")), (Tri::True, Tri::False));
        assert_eq!((c.defined(b"__CC_ARM"), c.defined(b"_WIN32")), (Tri::Unknown, Tri::Unknown));
        settings.condition_files = vec![".cproject#Debug".into(), ".vscode/c_cpp_properties.json#Win32".into()];
        settings.defines.insert("LEVEL".into(), "9".into());
        let c = Conditions::from_settings(&settings, &dir);
        assert_eq!((c.eval(b"LEVEL == 9 && N == 2 && DEBUG"), c.defined(b"NDEBUG"), c.defined(b"ONLY_A")), (Tri::True, Tri::False, Tri::False));
        assert_eq!(include_paths(&settings, &dir), ["inc", "drivers/include", "bsp", "C:/Arm/include"]);
        // What it all comes to, and who says so.
        let e = effective(&settings, &dir);
        let of = |n: &str| e.iter().find(|x| x.name == n).map(|x| (x.value.as_deref(), x.source.as_str()));
        assert_eq!(of("LEVEL"), Some((Some("9"), "")));
        assert_eq!(of("DEBUG"), Some((Some("1"), ".cproject#Debug")));
        assert_eq!(of("NDEBUG"), Some((None, ".cproject#Debug")));
        assert_eq!(of("N"), Some((Some("2"), ".vscode/c_cpp_properties.json#Win32")));

        // Several sets in one project, one in force; an old file is one set.
        std::fs::create_dir_all(dir.join(".norn")).unwrap();
        std::fs::write(dir.join(".norn/config.json"), r#"{"include":["src"],"defines":{"OLD":"1"}}"#).unwrap();
        let mut p = Project::load(&dir);
        assert_eq!((p.active.as_deref(), p.current().defines.get("OLD").map(String::as_str)), (Some(DEFAULT), Some("1")));
        p.configurations.insert("Debug".into(), settings.clone());
        p.active = Some("Debug".into());
        p.save(&dir).unwrap();
        assert_eq!((Project::load(&dir), Settings::load(&dir)), (p, settings));
        let written = std::fs::read_to_string(dir.join(".norn/config.json")).unwrap();
        assert!(written.contains("\"include\"") && !written.contains("\n  \"defines\""), "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_build_description_in_the_tree_is_followed_unasked() {
        let dir = std::env::temp_dir().join(format!("si-cond-found-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".norn")).unwrap();
        // Nothing in the tree: nothing in force.
        assert_eq!(Project::load(&dir), Project::default());
        let commands = r#"[
            {"directory": "/b", "file": "a.c", "command": "cc -DNDEBUG -D LEVEL=3 -DONLY_A -I inc \"-DNAME=\\\"x y\\\"\" -c a.c"},
            {"directory": "/b", "file": "b.c", "arguments": ["cc", "-DNDEBUG", "-D", "LEVEL=3", "-DNAME=\"x y\"", "-UOLD", "-c", "b.c"]}
        ]"#;
        std::fs::write(dir.join("compile_commands.json"), commands).unwrap();
        // The scope written, and no conditions: the build's are followed.
        std::fs::write(dir.join(".norn/config.json"), r#"{"include":["src"]}"#).unwrap();
        let p = Project::load(&dir);
        assert_eq!(p.active.as_deref(), Some("compile_commands"));
        let c = Conditions::from_settings(&Settings::load(&dir), &dir);
        assert_eq!((c.defined(b"NDEBUG"), c.eval(b"LEVEL == 3"), c.defined(b"ONLY_A"), c.defined(b"NAME")), (Tri::True, Tri::True, Tri::Unknown, Tri::True));
        // Once the project says what it has, even nothing, that stands.
        Project::default().save(&dir).unwrap();
        assert_eq!(Project::load(&dir), Project::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_the_sources_define_is_known_where_it_can_be() {
        let files = ["a.c".to_string(), "b.c".to_string(), "cfg.h".to_string()];
        let mut source = Source::new(&files);
        // cfg.h defines USE_NET; a.c includes it on line 5, b.c gets it from elsewhere.
        source.add(b"USE_NET", b"1", &[true, true, true], &[true, false, true]);
        source.include(0, 5, 2);
        let no = [false; 3];
        source.add(b"LEVEL", b"DEFAULT_LEVEL", &[true, false, false], &no);
        source.add(b"DEFAULT_LEVEL", b"3", &[true, false, false], &no);
        source.add(b"HAVE_X", b"", &[true, true, false], &no);
        let mut c = Conditions::default();
        c.set_source(Some(std::sync::Arc::new(source)));
        let none = Local::default();
        let place = |file: &str, line: u32| c.source().unwrap().file(file).map(|f| (f, line));
        let (a, b) = (place("a.c", 9), place("b.c", 9));
        // Known from the line after the include that brings it in, where the file does that itself.
        assert_eq!((c.eval_in(&none, place("a.c", 5), b"USE_NET"), c.eval_in(&none, place("a.c", 6), b"USE_NET"), c.eval_in(&none, place("b.c", 1), b"USE_NET")), (Tri::Unknown, Tri::True, Tri::True));
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
    fn a_macro_whose_value_is_an_expression_is_read_as_an_if_would() {
        let mut local = Local::default();
        let mut set = |name: &str, body: &str| {
            local.insert(name.as_bytes().to_vec(), Some(value_of(body.as_bytes())));
        };
        // 0 && anything is 0, however little is known of the rest.
        set("ENABLE_SELF_TEST", "(0 && ENABLE_OPT && (!ENABLE_ALT_BOOT) && (!ONE_IMAGE_ONLY) && !WIN32)");
        set("ENABLE_ON", "(1 || ENABLE_OPT)");
        set("ENABLE_BOTH", "(ENABLE_ON && !ENABLE_SELF_TEST)");
        set("ENABLE_OPEN", "(ENABLE_OPT && 1)");
        // Each in terms of the other: not read forever.
        set("LOOP_A", "(LOOP_B && 1)");
        set("LOOP_B", "(LOOP_A || 0)");
        // However long the value.
        set("LONG", &format!("(0{})", " && SOME_MACRO_NOBODY_DEFINES".repeat(40)));
        let c = Conditions::default();
        let v = |e: &str| c.eval_in(&local, None, e.as_bytes());
        assert_eq!((v("ENABLE_SELF_TEST"), v("ENABLE_ON"), v("ENABLE_BOTH"), v("ENABLE_OPEN")), (Tri::False, Tri::True, Tri::True, Tri::Unknown));
        assert_eq!((v("LOOP_A"), v("LONG")), (Tri::Unknown, Tri::False));
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
            ("ARCH_SIZE + 1 == 65", Tri::True),
        ] {
            assert_eq!(c.eval(expr.as_bytes()), want, "{expr}");
        }
        // With nothing set, only what needs no setting is decided: `#if 0`.
        let none = Conditions::default();
        assert_eq!((none.eval(b"0"), none.eval(b"defined(X)"), none.eval(b"X == 1")), (Tri::False, Tri::Unknown, Tri::Unknown));
    }
}
