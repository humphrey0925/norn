//! `norn mcp`: the index as a Model Context Protocol server, over
//! stdin/stdout, for AI agents (Claude Code, Cursor, Copilot and others).
//!
//! The same answers as `live`, as tools an agent calls by name, written as
//! text for it to read: where a name is defined, who calls a function and
//! how (through a macro, a member or variable holding it, a pointer), what
//! it calls, the source of a definition, and which lines the build
//! compiles. One JSON-RPC message a line each way.

use serde_json::{json, Value};
use si_core::graph::{Graph, Qualifier};
use si_core::model::{flag, kind, refkind};
use si_core::session::{Session, Update};
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The protocol versions this server speaks, the latest last.
const VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Results listed for one call unless the agent asks for more, and at most.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 2000;

/// Lines of source shown for one definition at most.
const MAX_SOURCE_LINES: u32 = 400;

/// Lines `read_file` shows unless asked for others, and at most.
const READ_LINES: u32 = 300;
const MAX_READ_LINES: u32 = 2000;

/// Where the files cannot be watched: the tree is looked at again for
/// files changed on disk before a tool answers, unless it was this recently.
const SYNC_AFTER: Duration = Duration::from_millis(500);

/// The build's own descriptions, and the project's settings: when one
/// changes, the conditions may have, and the whole tree is looked at again
/// (as the extension does on the same files).
fn is_build_config(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(name, ".cproject" | "compile_commands.json" | "compile_flags.txt" | "platformio.ini" | "c_cpp_properties.json")
        || [".uvprojx", ".uvproj", ".ewp", ".vcxproj"].iter().any(|x| name.ends_with(x))
        || name == "config.json" && path.parent().and_then(Path::file_name).is_some_and(|d| d == ".norn")
}

/// What changed on disk since the index last looked, as the watcher told.
#[derive(Default)]
struct Changes {
    /// Files written, created, deleted or renamed.
    files: std::collections::BTreeSet<PathBuf>,
    /// The whole tree is to be looked at: a build configuration changed, a
    /// folder came or went, or the watcher lost track.
    all: bool,
}

/// Watches the tree, as the editor watches it for the language server:
/// a tool then reads again only what changed, and nothing when nothing did.
struct Watch {
    _watcher: notify::RecommendedWatcher,
    events: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    /// The root as given, and as the system may name it in what it tells
    /// (macOS: `/private/tmp/…` for `/tmp/…`).
    root: PathBuf,
    real_root: PathBuf,
}

impl Watch {
    fn start(root: &Path) -> Option<Watch> {
        use notify::Watcher as _;
        let (tx, events) = std::sync::mpsc::channel();
        let made = notify::recommended_watcher(move |e| {
            let _ = tx.send(e);
        })
        .and_then(|mut w| w.watch(root, notify::RecursiveMode::Recursive).map(|_| w));
        match made {
            Ok(watcher) => {
                let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                Some(Watch { _watcher: watcher, events, root: root.to_path_buf(), real_root })
            }
            Err(e) => {
                eprintln!("norn: cannot watch {} for changes ({e}); the tree is looked at again before each answer instead", root.display());
                None
            }
        }
    }

    /// What the watcher told of since it was last asked; `skip`: what is
    /// no part of the sources (the index's own files).
    fn changes(&self, skip: &dyn Fn(&Path) -> bool) -> Changes {
        use notify::EventKind;
        let mut out = Changes::default();
        for event in self.events.try_iter() {
            let Ok(event) = event else {
                out.all = true;
                continue;
            };
            if event.need_rescan() {
                out.all = true;
            }
            // (A folder made, removed or renamed is told of as itself, not
            // as the files in it: a name with no extension may be one.)
            let comes_or_goes = matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_)));
            for path in event.paths {
                let path = match path.strip_prefix(&self.real_root) {
                    Ok(rel) if !path.starts_with(&self.root) => self.root.join(rel),
                    _ => path,
                };
                if skip(&path) || path.components().any(|c| c.as_os_str() == ".git") {
                    continue;
                }
                if is_build_config(&path) || comes_or_goes && path.extension().is_none() {
                    out.all = true;
                } else if !matches!(event.kind, EventKind::Access(_)) {
                    out.files.insert(path);
                }
            }
        }
        out
    }
}

fn tools() -> Value {
    let name = json!({ "type": "string", "description": "The symbol's name as written in the code, e.g. `uart_send`." });
    let file = json!({ "type": "string", "description": "Only the definitions in this file (path relative to the project root), when the name has several." });
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": format!("Most results to list (default {DEFAULT_LIMIT}).") });
    let read_only = json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "search_symbols",
            "title": "Search symbols",
            "description": "Find functions, macros, types, variables and other symbols by part of their name: fragments in any order (`uart snd`), initials (`us`), or letters in order. Best matches first. Use it when the exact name is not known.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "What to look for: one or more fragments of the name." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Most results to list (default 30)." },
            }, "required": ["query"] },
            "annotations": read_only,
        },
        {
            "name": "read_file",
            "title": "Read a file, compiled lines marked",
            "description": "Read a C/C++ file (or some of its lines), numbered, with where the build configuration in force stops compiling it said on a line of its own: `>>> not compiled under X (lines a–b)` before such a stretch and `<<< end of not compiled` after it (the code is there, but this build does not have it); the same for a stretch under an #if the configuration does not settle. Use it instead of reading C/C++ files as plain text, where code in a switched-off #if branch looks like any other. With hide_inactive, a stretch not compiled is that one line.",
            "inputSchema": { "type": "object", "properties": {
                "file": { "type": "string", "description": "The file (relative to the project root)." },
                "from": { "type": "integer", "minimum": 1, "description": "The first line to show (default 1)." },
                "to": { "type": "integer", "minimum": 1, "description": format!("The last line to show (default: {READ_LINES} lines from `from`).") },
                "hide_inactive": { "type": "boolean", "description": "Show a stretch the configuration does not compile as one line instead of its code (default false)." },
            }, "required": ["file"] },
            "annotations": read_only,
        },
        {
            "name": "search_code",
            "title": "Search the code, compiled lines only",
            "description": "Search the text of the project's C/C++ files, as grep does, but only in what the build configuration in force compiles: a match in a switched-off #if branch, or in a file the build does not compile, is left out and counted (include_inactive to see them, marked ✗). A match under an #if the configuration does not settle is marked ?. Each match says which function it is in. Use it instead of grep on C/C++ code; for a symbol's definition, callers or uses, the other tools are more exact.",
            "inputSchema": { "type": "object", "properties": {
                "pattern": { "type": "string", "description": "What to look for: text, or a regular expression with regex." },
                "regex": { "type": "boolean", "description": "pattern is a regular expression (Rust syntax; default false: plain text)." },
                "ignore_case": { "type": "boolean", "description": "Upper and lower case alike (default false)." },
                "in": { "type": "string", "description": "Only in this folder or file (relative to the project root)." },
                "include_inactive": { "type": "boolean", "description": "Also list matches the configuration does not compile, marked ✗ (default false)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": format!("Most matches to list (default {DEFAULT_LIMIT}).") },
            }, "required": ["pattern"] },
            "annotations": read_only,
        },
        {
            "name": "find_definition",
            "title": "Find definition",
            "description": "Where a symbol is defined and declared. Given the file and line where the name is used, only the definition that place sees (as Go to Definition does), where there are several.",
            "inputSchema": { "type": "object", "properties": {
                "name": name,
                "file": { "type": "string", "description": "The file the name is used in (relative to the project root)." },
                "line": { "type": "integer", "minimum": 1, "description": "The line of `file` the name is used on, the first being 1." },
            }, "required": ["name"] },
            "annotations": read_only,
        },
        {
            "name": "read_symbol",
            "title": "Read a symbol's source",
            "description": "The source of a definition (a function's body, a struct's members, a macro's value), with the comment above it, without reading the whole file. Lines are numbered.",
            "inputSchema": { "type": "object", "properties": { "name": name, "file": file }, "required": ["name"] },
            "annotations": read_only,
        },
        {
            "name": "find_callers",
            "title": "Find callers",
            "description": "Every call of a function: the calling function and the line of the call. Includes calls a macro makes and calls through function pointers stored in struct members or variables, which a text search cannot find; each says how it comes about, and calls that depend on an #if the configuration does not settle are marked uncertain.",
            "inputSchema": { "type": "object", "properties": { "name": name, "file": file, "limit": limit }, "required": ["name"] },
            "annotations": read_only,
        },
        {
            "name": "find_callees",
            "title": "Find callees",
            "description": "Every call a function makes, including the functions reached through a struct member or variable that holds them (each that can be stored there), with the line of each call.",
            "inputSchema": { "type": "object", "properties": { "name": name, "file": file, "limit": limit }, "required": ["name"] },
            "annotations": read_only,
        },
        {
            "name": "find_references",
            "title": "Find references",
            "description": "Every place a symbol is used: called, read or written, or a function stored in a member or variable. Each with the function (or file) it is in and the line.",
            "inputSchema": { "type": "object", "properties": { "name": name, "file": file, "limit": limit }, "required": ["name"] },
            "annotations": read_only,
        },
        {
            "name": "call_tree",
            "title": "Call tree",
            "description": "The functions a function reaches through calls (`callees`), or the functions that reach it (`callers`), a few levels deep, as an indented tree. Calls through macros and function pointers included.",
            "inputSchema": { "type": "object", "properties": {
                "name": name,
                "file": file,
                "direction": { "type": "string", "enum": ["callees", "callers"], "description": "Down the calls (default) or up them." },
                "depth": { "type": "integer", "minimum": 1, "maximum": 6, "description": "Levels to follow (default 2)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Most functions to list (default 200)." },
            }, "required": ["name"] },
            "annotations": read_only,
        },
        {
            "name": "compiled_lines",
            "title": "Compiled lines",
            "description": "Which lines of a file the build compiles, under the build configuration Norn found or was given: the stretches of #if/#ifdef branches switched off, and those under an #if the configuration does not settle. Given a line, whether that line is compiled. Check this before editing code inside #if branches.",
            "inputSchema": { "type": "object", "properties": {
                "file": { "type": "string", "description": "The file (relative to the project root)." },
                "line": { "type": "integer", "minimum": 1, "description": "Only this line, the first being 1." },
            }, "required": ["file"] },
            "annotations": read_only,
        },
        {
            "name": "list_configurations",
            "title": "Build configurations",
            "description": "The build configurations Norn knows for this project, which one is in force, and what it defines. A configuration decides which #if branches are compiled, and so which definitions, calls and lines the other tools see: a board, a target, Debug or Release. Call this before analysing C/C++ code that depends on such a choice, and switch if the one in force is not the one the task is about.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "switch_configuration",
            "title": "Switch build configuration",
            "description": "Put another build configuration in force: one of the project's, or one found in its build files (a Keil, IAR, Eclipse or Visual Studio project, a Makefile, CMake, Kconfig...), by the name list_configurations gives. The index is brought in line with it before this answers. It is the project's setting: the editor and other agents of the project see the same; tell the user which one you chose and why.",
            "inputSchema": { "type": "object", "properties": {
                "name": { "type": "string", "description": "The configuration's name, as list_configurations gives it." },
            }, "required": ["name"] },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        },
    ])
}

/// What an agent is told of the server when it starts.
fn instructions(root: Option<&Path>) -> String {
    let under = root.map_or("the project's folder".to_string(), |r| r.display().to_string());
    format!(
        "Norn indexes the C and C++ sources under {under} without a build: definitions, references, and the call graph, \
         including calls made by macros and calls through function pointers stored in struct members or variables, \
         under the #if branches the project's build configuration compiles. For C and C++ symbols use these tools, not \
         text search: grep finds names, not calls through macros or function pointers, and not which #if branch is \
         compiled. First establish the build context: list_configurations, and switch_configuration if the one in \
         force is not the board, target or build the task is about. Then search_symbols or find_definition, read_symbol \
         rather than reading whole files, find_callers / find_callees / call_tree for relations, and compiled_lines \
         before editing code inside #if branches. Read C/C++ files with read_file and search their text with \
         search_code rather than a plain file read or grep: they mark, or leave out, what the configuration does not \
         compile. Paths are relative to that folder. The index follows the files as \
         they change on disk."
    )
}

/// The index `norn mcp` answers from, once it is made.
struct Server {
    root: PathBuf,
    session: Session,
    /// What tells of changes on disk; without it, the tree is looked at
    /// before each answer.
    watch: Option<Watch>,
    /// When the tree was last looked at for changes.
    synced: Instant,
}

impl Server {
    /// Bring the index in line with the disk: the files the watcher told
    /// of, or without one, the whole tree if it was not just now looked at.
    fn sync(&mut self) {
        match &self.watch {
            Some(watch) => {
                let cache = self.session.cache_dir().map(Path::to_path_buf);
                let changes = watch.changes(&|p: &Path| cache.as_deref().is_some_and(|c| p.starts_with(c)));
                if changes.all {
                    self.session.sync();
                    return;
                }
                // Sources, and what else the index has (a file a source
                // includes): a build's objects and the rest are nothing to it.
                let files: Vec<PathBuf> = changes
                    .files
                    .into_iter()
                    .filter(|p| {
                        p.file_name().and_then(|n| n.to_str()).is_some_and(si_core::is_source)
                            || self.session.facts_of(&si_core::rel_path(&self.root, p)).is_some()
                    })
                    .collect();
                if !files.is_empty() {
                    self.session.update(&files);
                }
            }
            None if self.synced.elapsed() < SYNC_AFTER => {}
            None => {
                self.session.sync();
                self.synced = Instant::now();
            }
        }
    }
}

/// The tools, over an index: `norn mcp`'s own, or the language server's
/// (`si/mcp`, for an editor that serves them from the index it has).
pub struct Tools<'a> {
    root: &'a Path,
    session: &'a mut Session,
    /// For each file of the graph: no compilation of the build reads it.
    unread: Vec<bool>,
}

impl<'a> Tools<'a> {
    pub fn new(root: &'a Path, session: &'a mut Session) -> Tools<'a> {
        let readings = si_core::readings_of(session.conditions());
        let unread: std::collections::HashSet<&str> = readings.unread.iter().map(String::as_str).collect();
        let g = &session.graph;
        let unread = (0..g.files.len() as u32).map(|f| unread.contains(g.file_path(f))).collect();
        Tools { root, session, unread }
    }

    fn g(&self) -> &Graph {
        &self.session.graph
    }

    /// `file` as the index names it: relative to the root, with `/`.
    fn relative(&self, file: &str) -> String {
        let file = file.replace('\\', "/");
        let root = self.root.to_string_lossy().replace('\\', "/");
        let file = file.strip_prefix(root.trim_end_matches('/')).map_or(file.as_str(), |r| r.trim_start_matches('/'));
        file.trim_start_matches("./").to_string()
    }

    pub fn call(&mut self, tool: &str, args: &Value) -> Result<String, String> {
        let name = args["name"].as_str().unwrap_or("").trim();
        let limit = args["limit"].as_u64().map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));
        let needs_name = !matches!(tool, "search_symbols" | "compiled_lines" | "list_configurations" | "read_file" | "search_code");
        if needs_name && name.is_empty() {
            return Err("`name` is required.".into());
        }
        match tool {
            "search_symbols" => self.search(args["query"].as_str().unwrap_or(""), args["limit"].as_u64().map_or(30, |n| (n as usize).clamp(1, 200))),
            "find_definition" => self.find_definition(name, args["file"].as_str(), args["line"].as_u64()),
            "read_symbol" => self.read_symbol(name, args["file"].as_str()),
            "find_callers" => self.callers(name, args["file"].as_str(), limit),
            "find_callees" => self.callees(name, args["file"].as_str(), limit),
            "find_references" => self.references(name, args["file"].as_str(), limit),
            "call_tree" => {
                let up = args["direction"].as_str() == Some("callers");
                let depth = args["depth"].as_u64().unwrap_or(2).clamp(1, 6) as u32;
                let limit = args["limit"].as_u64().map_or(200, |n| (n as usize).clamp(1, MAX_LIMIT));
                self.call_tree(name, args["file"].as_str(), up, depth, limit)
            }
            "compiled_lines" => self.compiled_lines(args["file"].as_str().unwrap_or(""), args["line"].as_u64()),
            "list_configurations" => self.configurations(),
            "read_file" => self.read_file(
                args["file"].as_str().unwrap_or(""),
                args["from"].as_u64().map(|n| n as u32),
                args["to"].as_u64().map(|n| n as u32),
                args["hide_inactive"].as_bool().unwrap_or(false),
            ),
            "search_code" => self.search_code(
                args["pattern"].as_str().unwrap_or(""),
                args["regex"].as_bool().unwrap_or(false),
                args["ignore_case"].as_bool().unwrap_or(false),
                args["in"].as_str().unwrap_or(""),
                args["include_inactive"].as_bool().unwrap_or(false),
                limit,
            ),
            "switch_configuration" => self.switch_configuration(name),
            other => Err(format!("No tool named `{other}`.")),
        }
    }

    // ------------------------------------------------------------ wording

    fn place(&self, sym: u32) -> String {
        let s = &self.g().syms[sym as usize];
        format!("{}:{}", self.g().file_path(s.file), s.line)
    }

    /// `name (function, src/uart.c:40)`, and what is to be known of it.
    fn describe(&self, q: &Qualifier, sym: u32) -> String {
        let g = self.g();
        let s = &g.syms[sym as usize];
        let mut out = format!("{} ({}", q.name(g, sym), kind::name(s.kind));
        if s.flags & flag::DECL != 0 {
            out.push_str(" declaration");
        }
        let _ = write!(out, ", {})", self.place(sym));
        out.push_str(&self.notes(sym));
        out
    }

    /// Whether a symbol is compiled, when that is in doubt.
    fn notes(&self, sym: u32) -> String {
        let s = &self.g().syms[sym as usize];
        if self.unread.get(s.file as usize).copied().unwrap_or(false) {
            " [in a file the build does not compile]".into()
        } else if s.flags & flag::CONDITIONAL != 0 {
            " [under an #if the configuration does not settle]".into()
        } else {
            String::new()
        }
    }

    /// Whether line `line` of file `file` is in doubt: under an #if the
    /// configuration does not settle (the index has such a line as if it
    /// were compiled).
    fn maybe(&self, file: u32, line: u32) -> String {
        let Some(facts) = self.session.facts_of(self.g().file_path(file)) else { return String::new() };
        // The innermost branch around the line: the last to start.
        match facts.undecided.iter().rev().find(|&&(a, b, _, _)| a <= line && line <= b) {
            Some(&(_, _, directive, _)) => format!(" [maybe not compiled: under the #if at line {directive}]"),
            None => String::new(),
        }
    }

    /// How a call comes about, when it is not by name.
    fn how(&self, c: &si_core::Caller, macro_too: bool) -> String {
        let g = self.g();
        let mut out = String::new();
        if let Some(slot) = c.slot {
            let what = if g.syms[slot as usize].kind == kind::FIELD { "member" } else { "variable" };
            let _ = write!(out, " — through {what} `{}`, which can hold it", g.sym_name(slot));
        } else if c.pointer {
            out.push_str(" — through a pointer that can hold it");
        }
        if macro_too {
            if let Some(m) = c.via.filter(|&m| Some(m) != c.slot) {
                let _ = write!(out, " — via macro `{}`", g.sym_name(m));
            }
        }
        if c.ambiguous {
            out.push_str(" [uncertain]");
        }
        out
    }

    /// The symbols named `name` (in `file`, if given): definitions before
    /// declarations, those the build compiles first. Or why there are none.
    fn named(&self, name: &str, file: Option<&str>) -> Result<Vec<u32>, String> {
        let g = self.g();
        let mut found: Vec<u32> = g.find(name).iter().copied().filter(|&s| g.syms[s as usize].kind != kind::FILE).collect();
        if found.is_empty() {
            return Err(self.not_found(name));
        }
        if let Some(file) = file.filter(|f| !f.is_empty()) {
            let file = self.relative(file);
            found.retain(|&s| g.file_path(g.syms[s as usize].file) == file);
            if found.is_empty() {
                return Err(format!("`{name}` is not defined or declared in {file}. It is at: {}.",
                                   g.find(name).iter().map(|&s| self.place(s)).collect::<Vec<_>>().join(", ")));
            }
        }
        found.sort_by_key(|&s| {
            let sym = &g.syms[s as usize];
            (sym.flags & flag::DECL != 0, self.unread[sym.file as usize], sym.flags & flag::CONDITIONAL != 0, s)
        });
        Ok(found)
    }

    /// The functions and the rest among `syms` that calls can be asked of.
    fn callable(&self, syms: &[u32]) -> Vec<u32> {
        let g = self.g();
        let defs: Vec<u32> = syms.iter().copied().filter(|&s| g.syms[s as usize].flags & flag::DECL == 0).collect();
        if defs.is_empty() { syms.to_vec() } else { defs }
    }

    fn not_found(&self, name: &str) -> String {
        let g = self.g();
        let q = Qualifier::new(g);
        let like: Vec<String> = si_core::search::search(g, name, 8).iter().map(|&(_, s)| self.describe(&q, s)).collect();
        if like.is_empty() {
            format!("No symbol named `{name}` is indexed.")
        } else {
            format!("No symbol named `{name}` is indexed. Names like it:\n{}", like.join("\n"))
        }
    }

    // ------------------------------------------------------------ the tools

    fn search(&self, query: &str, limit: usize) -> Result<String, String> {
        if query.trim().is_empty() {
            return Err("`query` is required.".into());
        }
        let g = self.g();
        let q = Qualifier::new(g);
        let hits = si_core::search::search(g, query, limit);
        if hits.is_empty() {
            return Ok(format!("Nothing matches `{query}`."));
        }
        let mut out = format!("{} symbols match `{query}`, best first:\n", hits.len());
        for &(_, s) in &hits {
            let _ = writeln!(out, "{}", self.describe(&q, s));
        }
        Ok(out)
    }

    fn find_definition(&self, name: &str, file: Option<&str>, line: Option<u64>) -> Result<String, String> {
        let g = self.g();
        let q = Qualifier::new(g);
        let found = match (file, line) {
            (Some(file), Some(line)) => {
                let file = self.relative(file);
                let mut found = si_core::definition_at(g, &file, line as u32, name);
                found.retain(|&s| g.syms[s as usize].kind != kind::FILE);
                found.sort_by_key(|&s| (g.syms[s as usize].flags & flag::DECL != 0, s));
                if found.is_empty() {
                    return Err(self.not_found(name));
                }
                found
            }
            _ => self.named(name, file)?,
        };
        let mut out = String::new();
        for &s in &found {
            let _ = writeln!(out, "{}", self.describe(&q, s));
        }
        Ok(out)
    }

    fn read_symbol(&self, name: &str, file: Option<&str>) -> Result<String, String> {
        let g = self.g();
        let q = Qualifier::new(g);
        let found = self.named(name, file)?;
        let shown = self.callable(&found);
        let mut out = String::new();
        for &s in shown.iter().take(5) {
            let sym = &g.syms[s as usize];
            let path = g.file_path(sym.file);
            let Ok(text) = std::fs::read(self.root.join(path)) else {
                let _ = writeln!(out, "{}: the file cannot be read.\n", self.describe(&q, s));
                continue;
            };
            let text = String::from_utf8_lossy(&text);
            let lines: Vec<&str> = text.lines().collect();
            let last = sym.end_line.max(sym.line).min(lines.len() as u32);
            // The comment just above it, if any.
            let mut first = sym.line;
            while first > 1 {
                let above = lines.get(first as usize - 2).map_or("", |l| l.trim());
                let comment = above.starts_with("//") || above.starts_with("/*") || above.starts_with('*') || above.ends_with("*/");
                if !comment {
                    break;
                }
                first -= 1;
            }
            let end = last.min(first + MAX_SOURCE_LINES - 1);
            let _ = writeln!(out, "{}:", self.describe(&q, s));
            out.push_str(&listing(&lines, first, end, self.session.facts_of(path), &self.config_name(), false));
            if end < last {
                let _ = writeln!(out, "        … {} lines more, to line {last}", last - end);
            }
            out.push('\n');
        }
        if shown.len() > 5 {
            let _ = writeln!(out, "{} more definitions of `{name}`; pass `file` to choose one.", shown.len() - 5);
        }
        Ok(out)
    }

    fn callers(&self, name: &str, file: Option<&str>, limit: usize) -> Result<String, String> {
        let g = self.g();
        let q = Qualifier::new(g);
        let targets = self.callable(&self.named(name, file)?);
        let mut out = String::new();
        let (mut listed, mut all) = (0, 0);
        for &t in &targets {
            let mut calls = si_core::callers_through_macros(g, t);
            if calls.is_empty() && targets.len() > 1 {
                continue;
            }
            calls.sort_by_key(|c| (g.file_path(g.syms[c.caller as usize].file), c.line, c.caller));
            all += calls.len();
            let _ = writeln!(out, "{} of {}:", count(calls.len(), "call", "calls"), self.describe(&q, t));
            for c in &calls {
                if listed == limit {
                    break;
                }
                listed += 1;
                let from = &g.syms[c.caller as usize];
                let at = format!("{}:{}", g.file_path(from.file), c.line);
                let who = if from.kind == kind::FILE { "at file level".to_string() } else { format!("in {}", q.name(g, c.caller)) };
                let _ = writeln!(out, "  {at}  {who}{}{}{}", self.how(c, true), self.notes(c.caller), self.maybe(from.file, c.line));
            }
            out.push('\n');
        }
        self.more(&mut out, listed, limit, all);
        Ok(out)
    }

    fn callees(&self, name: &str, file: Option<&str>, limit: usize) -> Result<String, String> {
        let g = self.g();
        let q = Qualifier::new(g);
        let targets = self.callable(&self.named(name, file)?);
        let mut out = String::new();
        let (mut listed, mut all) = (0, 0);
        for &t in &targets {
            let mut calls = si_core::callees_through_slots(g, t);
            if calls.is_empty() && targets.len() > 1 {
                continue;
            }
            calls.sort_by_key(|c| (c.line, c.caller));
            all += calls.len();
            let _ = writeln!(out, "{} made by {}:", count(calls.len(), "call", "calls"), self.describe(&q, t));
            let file = g.file_path(g.syms[t as usize].file);
            for c in &calls {
                if listed == limit {
                    break;
                }
                listed += 1;
                let _ = writeln!(out, "  {file}:{}  {}{}{}", c.line, self.describe(&q, c.caller), self.how(c, false), self.maybe(g.syms[t as usize].file, c.line));
            }
            out.push('\n');
        }
        self.more(&mut out, listed, limit, all);
        Ok(out)
    }

    fn references(&self, name: &str, file: Option<&str>, limit: usize) -> Result<String, String> {
        let g = self.g();
        let q = Qualifier::new(g);
        let targets = self.named(name, file)?;
        let mut out = String::new();
        let (mut listed, mut all) = (0, 0);
        if targets.iter().all(|&t| g.refs_to(t).is_empty()) {
            return Ok(format!("Nothing refers to {}.", self.describe(&q, targets[0])));
        }
        for &t in &targets {
            let refs = g.refs_to(t);
            if refs.is_empty() && targets.len() > 1 {
                continue;
            }
            all += refs.len();
            let _ = writeln!(out, "{} to {}:", count(refs.len(), "reference", "references"), self.describe(&q, t));
            for e in refs {
                if listed == limit {
                    break;
                }
                listed += 1;
                let from = &g.syms[e.other as usize];
                let uncertain = if e.ambiguous() { " [uncertain]" } else { "" };
                // (Where it is stored is said by the use there, listed too:
                // this says what holds it.)
                if e.kind() == refkind::BIND {
                    let what = if from.kind == kind::FIELD { "member" } else { "variable" };
                    let _ = writeln!(out, "  stored in {what} `{}` ({}){uncertain}", q.name(g, e.other), self.place(e.other));
                    continue;
                }
                let what = match e.kind() {
                    refkind::CALL => "called",
                    refkind::ICALL => "called through it",
                    refkind::PCALL => "called through a pointer",
                    refkind::MEMBER => "member used",
                    _ => "used",
                };
                let who = if from.kind == kind::FILE { "at file level".to_string() } else { format!("in {}", q.name(g, e.other)) };
                let _ = writeln!(out, "  {}:{}  {who}: {what}{uncertain}{}", g.file_path(from.file), e.line(), self.maybe(from.file, e.line()));
            }
            out.push('\n');
        }
        self.more(&mut out, listed, limit, all);
        Ok(out)
    }

    fn call_tree(&self, name: &str, file: Option<&str>, up: bool, depth: u32, limit: usize) -> Result<String, String> {
        let g = self.g();
        let q = Qualifier::new(g);
        let roots = self.callable(&self.named(name, file)?);
        let mut out = String::new();
        let mut seen = std::collections::HashSet::new();
        let mut listed = 0;
        let mut cut = false;
        // Depth first, so each function is under what calls it (or what it
        // calls); one met again is not followed again.
        for &root in &roots {
            let _ = writeln!(out, "{}", self.describe(&q, root));
            seen.insert(root);
            // (What is reached, how deep, the call, and the function the call is written in.)
            let mut stack: Vec<(u32, u32, si_core::Caller, u32)> = Vec::new();
            let next = |s: u32| {
                let mut v = if up { si_core::callers_through_macros(g, s) } else { si_core::callees_through_slots(g, s) };
                v.retain(|c| g.syms[c.caller as usize].kind != kind::FILE);
                v.sort_by_key(|c| (c.line, c.caller));
                v.dedup_by_key(|c| c.caller);
                v
            };
            let written_in = |reached: u32, from: u32| if up { reached } else { from };
            stack.extend(next(root).into_iter().rev().map(|c| (c.caller, 1, c, written_in(c.caller, root))));
            while let Some((s, level, c, site)) = stack.pop() {
                if listed == limit {
                    cut = true;
                    break;
                }
                listed += 1;
                let again = !seen.insert(s);
                let _ = writeln!(out, "{}{} ({}){}{}{}", "  ".repeat(level as usize), q.name(g, s), self.place(s), self.how(&c, false),
                                 self.maybe(g.syms[site as usize].file, c.line), if again { " (already listed)" } else { "" });
                if !again && level < depth {
                    stack.extend(next(s).into_iter().rev().map(|c| (c.caller, level + 1, c, written_in(c.caller, s))));
                }
            }
            out.push('\n');
        }
        if cut {
            let _ = writeln!(out, "Stopped at {limit} functions; pass a larger `limit` or a smaller `depth`.");
        }
        Ok(out)
    }

    /// The build configurations: which is in force, the project's, those
    /// found in its build files, and what the one in force defines.
    fn configurations(&self) -> Result<String, String> {
        use si_core::cond;
        let (project, automatic) = cond::Project::load_told(self.root);
        let mut out = match &project.active {
            Some(active) if automatic => format!("In force: {active} (followed from the project's build files, as nothing else is set).\n"),
            Some(active) => format!("In force: {active}.\n"),
            None => "In force: none. Only what the sources themselves #define is known: a branch nothing settles is indexed both ways, and what is in it is marked as maybe not compiled.\n".into(),
        };
        let few = |items: Vec<String>| {
            let more = items.len().saturating_sub(8);
            let mut shown = items.into_iter().take(8).collect::<Vec<_>>().join(" ");
            if more > 0 {
                let _ = write!(shown, " (+{more} more)");
            }
            shown
        };
        if !project.configurations.is_empty() {
            out.push_str("\nThe project's configurations:\n");
            for (name, s) in &project.configurations {
                let mut what = Vec::new();
                if !s.condition_files.is_empty() {
                    what.push(format!("follows {}", s.condition_files.join(", ")));
                }
                if !s.defines.is_empty() {
                    what.push(format!("defines {}", few(s.defines.iter().map(|(n, v)| if v.is_empty() { n.clone() } else { format!("{n}={v}") }).collect())));
                }
                if !s.undefines.is_empty() {
                    what.push(format!("undefines {}", few(s.undefines.clone())));
                }
                let mark = if project.active.as_ref() == Some(name) { "*" } else { " " };
                let _ = writeln!(out, "  {mark} {name}{}", if what.is_empty() { String::new() } else { format!(" — {}", what.join("; ")) });
            }
        }
        let found: Vec<(String, String)> = cond::discover(self.root)
            .into_iter()
            .filter(|(_, source)| !project.configurations.values().any(|s| s.condition_files.contains(source)))
            .collect();
        if !found.is_empty() {
            out.push_str("\nFound in the project's build files, not yet one of its configurations (switch_configuration takes these names too):\n");
            for (name, source) in found.iter().take(200) {
                let _ = writeln!(out, "  - {name} ({source})");
            }
        }
        let effective = cond::effective(&project.current(), self.root);
        if !effective.is_empty() {
            let shown: Vec<String> = effective
                .iter()
                .take(60)
                .map(|e| match &e.value {
                    Some(v) if v.is_empty() => e.name.clone(),
                    Some(v) => format!("{}={v}", e.name),
                    None => format!("!{}", e.name),
                })
                .collect();
            let first = if effective.len() > 60 { ", the first 60" } else { "" };
            let _ = writeln!(out, "\nWhat the one in force sets ({} macros{first}; ! is not defined): {}", effective.len(), shown.join(" "));
        }
        out.push_str("\nAnother is put in force with switch_configuration and its name; it is the project's setting, the editor's included.\n");
        Ok(out)
    }

    /// Put the configuration `name` in force: one of the project's, or one
    /// found in its build files (then made one of the project's, as the
    /// editor's Switch Compile Configuration does).
    fn switch_configuration(&mut self, name: &str) -> Result<String, String> {
        use si_core::cond;
        let (mut project, _) = cond::Project::load_told(self.root);
        if project.configurations.contains_key(name) {
            if project.active.as_deref() == Some(name) {
                return Ok(format!("{name} is in force already."));
            }
            project.active = Some(name.to_string());
        } else if let Some((found, source)) = cond::discover(self.root).into_iter().find(|(n, s)| n == name || s == name) {
            let mut new = found.clone();
            for n in 2.. {
                if !project.configurations.contains_key(&new) {
                    break;
                }
                new = format!("{found} {n}");
            }
            project.configurations.insert(new.clone(), cond::Settings { condition_files: vec![source], ..Default::default() });
            project.active = Some(new);
        } else {
            return Err(format!("No configuration named `{name}`.\n\n{}", self.configurations()?));
        }
        let t = Instant::now();
        let up = self
            .session
            .set_condition_project(&project)
            .map_err(|e| format!("The configuration could not be saved in {}: {e}", si_core::scope::Scope::config_path(self.root).display()))?;
        Ok(format!(
            "In force now: {}. The index is in line with it ({} files read again, {:.1} s).",
            project.active.unwrap_or_default(),
            up.extracted,
            t.elapsed().as_secs_f64()
        ))
    }

    /// The file `file` as the index names it, and its path, if it is in the
    /// project's folder.
    fn project_file(&self, file: &str) -> Result<(String, PathBuf), String> {
        let rel = self.relative(file);
        let path = self.root.join(&rel);
        let inside = std::fs::canonicalize(&path).ok().zip(std::fs::canonicalize(self.root).ok()).is_some_and(|(p, r)| p.starts_with(r));
        if !inside || !path.is_file() {
            return Err(format!("No file {rel} in the project."));
        }
        Ok((rel, path))
    }

    /// The name of the configuration in force, or that there is none.
    fn config_name(&self) -> String {
        si_core::cond::Project::load_told(self.root).0.active.unwrap_or_else(|| "no configuration".into())
    }

    /// The configuration in force, said in a few words.
    fn in_force(&self) -> String {
        match si_core::cond::Project::load_told(self.root).0.active {
            Some(active) => format!("Configuration in force: {active}."),
            None => "No configuration in force.".into(),
        }
    }

    fn read_file(&self, file: &str, from: Option<u32>, to: Option<u32>, hide: bool) -> Result<String, String> {
        if file.is_empty() {
            return Err("`file` is required.".into());
        }
        let (rel, path) = self.project_file(file)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("{rel} cannot be read: {e}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len() as u32;
        let first = from.unwrap_or(1).clamp(1, total.max(1));
        let last = to.unwrap_or(first + READ_LINES - 1).clamp(first, total.max(1)).min(first + MAX_READ_LINES - 1);
        let facts = self.session.facts_of(&rel);
        let mut out = format!("{rel}, lines {first}–{last} of {total}. ");
        match facts {
            Some(_) => out.push_str(&self.in_force()),
            None => out.push_str("Not indexed: nothing is known of what is compiled in it."),
        }
        out.push('\n');
        out.push_str(&listing(&lines, first, last, facts, &self.config_name(), hide));
        if last < total {
            let _ = writeln!(out, "… lines {}–{total} not shown; pass `from` and `to` for them.", last + 1);
        }
        Ok(out)
    }

    fn search_code(&self, pattern: &str, regex: bool, ignore_case: bool, within: &str, inactive_too: bool, limit: usize) -> Result<String, String> {
        use rayon::prelude::*;
        if pattern.is_empty() {
            return Err("`pattern` is required.".into());
        }
        let source = if regex { pattern.to_string() } else { regex::escape(pattern) };
        let re = regex::RegexBuilder::new(&source).case_insensitive(ignore_case).build().map_err(|e| format!("Not a regular expression: {e}"))?;
        let g = self.g();
        let within = self.relative(within);
        let within = within.trim_end_matches('/');
        let files: Vec<u32> = (0..g.files.len() as u32)
            .filter(|&f| {
                let p = g.file_path(f);
                within.is_empty() || p == within || p.strip_prefix(within).is_some_and(|r| r.starts_with('/'))
            })
            .collect();
        // Every matching line: (file, line, text).
        let mut found: Vec<(u32, u32, String)> = files
            .par_iter()
            .flat_map_iter(|&f| {
                let text = std::fs::read(self.root.join(g.file_path(f))).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let hits: Vec<(u32, u32, String)> = if re.is_match(&text) {
                    text.lines().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, l)| (f, i as u32 + 1, l.to_string())).collect()
                } else {
                    Vec::new()
                };
                hits
            })
            .collect();
        found.sort_by(|a, b| g.file_path(a.0).cmp(g.file_path(b.0)).then(a.1.cmp(&b.1)));
        let q = Qualifier::new(g);
        let (mut shown, mut hidden, mut listed) = (String::new(), 0, 0);
        for (f, line, text) in &found {
            let (mut m, _) = mark(self.session.facts_of(g.file_path(*f)), *line);
            if self.unread.get(*f as usize).copied().unwrap_or(false) {
                m = '✗';
            }
            if m == '✗' && !inactive_too {
                hidden += 1;
                continue;
            }
            if listed == limit {
                listed += 1;
                continue;
            }
            listed += 1;
            let at = match g.symbol_at(*f, *line) {
                Some(s) if g.syms[s as usize].kind != kind::FILE => format!("  [in {}]", q.name(g, s)),
                _ => String::new(),
            };
            let text = text.trim();
            let text = match text.char_indices().nth(200) {
                Some((cut, _)) => format!("{}…", &text[..cut]),
                None => text.to_string(),
            };
            let _ = writeln!(shown, "{}:{line} {m} {text}{at}", g.file_path(*f));
        }
        let matching = found.len() - hidden;
        let config = self.config_name();
        let mut out = if inactive_too {
            format!("{} of `{pattern}`, compiled under {config} or not.", count(matching, "match", "matches"))
        } else {
            format!("{} of `{pattern}` in the code {config} compiles.", count(matching, "match", "matches"))
        };
        if hidden > 0 {
            let _ = write!(out, " {hidden} more where it does not (include_inactive to see them).");
        }
        out.push('\n');
        if shown.contains(" ? ") || shown.contains(" ✗ ") {
            out.push_str("? under an #if the configuration does not settle; ✗ not compiled.\n");
        }
        out.push_str(&shown);
        if listed > limit {
            let _ = writeln!(out, "Listed {limit} of {}; pass a larger `limit`, or `in` for a folder.", listed);
        }
        Ok(out)
    }

    fn compiled_lines(&self, file: &str, line: Option<u64>) -> Result<String, String> {
        if file.is_empty() {
            return Err("`file` is required.".into());
        }
        let file = self.relative(file);
        let Some(facts) = self.session.facts_of(&file) else {
            return Err(format!("{file} is not indexed: not a C or C++ source or header of the project, or outside the folders it is said to be."));
        };
        let (project, automatic) = si_core::cond::Project::load_told(self.root);
        let mut out = match &project.active {
            Some(active) if automatic => format!("Configuration in force: {active} (found in the project's build files).\n"),
            Some(active) => format!("Configuration in force: {active}.\n"),
            None => "No build configuration: only what the sources themselves #define is known.\n".into(),
        };
        let g = self.g();
        if (0..g.files.len() as u32).any(|f| g.file_path(f) == file && self.unread[f as usize]) {
            out.push_str("No compilation of the build reads this file.\n");
        }
        let text = std::fs::read(self.root.join(&file)).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let line_text = |n: u32| text.lines().nth(n as usize - 1).unwrap_or("").trim().to_string();
        match line {
            Some(n) => {
                let n = n as u32;
                if let Some(&(a, b)) = facts.inactive.iter().find(|&&(a, b)| a <= n && n <= b) {
                    let _ = writeln!(out, "Line {n} is not compiled: {} in a branch the configuration switches off.",
                                     if a == b { "it is".to_string() } else { format!("lines {a}–{b} are") });
                } else if let Some(&(_, _, directive, _)) = facts.undecided.iter().rev().find(|&&(a, b, _, _)| a <= n && n <= b) {
                    let _ = writeln!(out, "Line {n} may or may not be compiled: it is under `{}` (line {directive}), which the configuration does not settle.",
                                     line_text(directive));
                } else {
                    let _ = writeln!(out, "Line {n} is compiled.");
                }
            }
            None => {
                if facts.inactive.is_empty() && facts.undecided.is_empty() {
                    out.push_str("Every line is compiled.\n");
                }
                if !facts.inactive.is_empty() {
                    let _ = writeln!(out, "Not compiled ({}):", count(facts.inactive.len(), "stretch", "stretches"));
                    for &(a, b) in facts.inactive.iter().take(MAX_LIMIT) {
                        let _ = writeln!(out, "  {}", lines(a, b));
                    }
                }
                if !facts.undecided.is_empty() {
                    let _ = writeln!(out, "May or may not be compiled, under an #if the configuration does not settle ({}):",
                                     count(facts.undecided.len(), "stretch", "stretches"));
                    for &(a, b, directive, _) in facts.undecided.iter().take(MAX_LIMIT) {
                        let _ = writeln!(out, "  {}, under `{}` (line {directive})", lines(a, b), line_text(directive));
                    }
                }
            }
        }
        Ok(out)
    }

    /// A word on what was left out.
    fn more(&self, out: &mut String, listed: usize, limit: usize, all: usize) {
        if listed == limit && all > listed {
            let _ = writeln!(out, "Listed {listed} of {all}; pass a larger `limit`, or `file` to choose one definition.");
        }
    }
}

/// How line `line` of a file whose facts are `facts` is compiled: ✗ not
/// at all, ? maybe (with the line of the #if that decides), or (' ') so.
fn mark(facts: Option<&si_core::model::FileFacts>, line: u32) -> (char, u32) {
    let Some(facts) = facts else { return (' ', 0) };
    if facts.inactive.iter().any(|&(a, b)| a <= line && line <= b) {
        return ('✗', 0);
    }
    // The innermost branch around the line: the last to start.
    match facts.undecided.iter().rev().find(|&&(a, b, _, _)| a <= line && line <= b) {
        Some(&(_, _, directive, _)) => ('?', directive),
        None => (' ', 0),
    }
}

/// A stretch of a file the configuration has a word on: not compiled
/// (`off`), or under an #if it does not settle (`directive`, that #if's
/// line); from line `a` to line `b`.
#[derive(Clone, Copy, PartialEq)]
struct Region {
    off: bool,
    a: u32,
    b: u32,
    directive: u32,
}

/// The stretch line `line` is in, if it is not simply compiled.
fn region(facts: Option<&si_core::model::FileFacts>, line: u32) -> Option<Region> {
    let facts = facts?;
    if let Some(&(a, b)) = facts.inactive.iter().find(|&&(a, b)| a <= line && line <= b) {
        return Some(Region { off: true, a, b, directive: 0 });
    }
    // The innermost branch around the line: the last to start.
    facts.undecided.iter().rev().find(|&&(a, b, _, _)| a <= line && line <= b).map(|&(a, b, directive, _)| Region { off: false, a, b, directive })
}

/// Lines `first` to `last` of `lines`, numbered, with where a stretch the
/// configuration (`config`) does not compile, or does not settle, begins
/// and ends said on a line of its own. `hide`: a stretch not compiled is
/// that one line.
fn listing(text: &[&str], first: u32, last: u32, facts: Option<&si_core::model::FileFacts>, config: &str, hide: bool) -> String {
    let what = |r: &Region| {
        if r.off {
            format!("not compiled under {config}")
        } else {
            format!("maybe not compiled: under the #if at line {}, which {config} does not settle", r.directive)
        }
    };
    let close = |out: &mut String, r: &Region, at: u32| {
        let word = if r.off { "not compiled" } else { "maybe not compiled" };
        if r.b > at {
            let _ = writeln!(out, "        ... still {word}, to line {}", r.b);
        } else {
            let _ = writeln!(out, "        <<< end of {word} ({})", lines(r.a, r.b));
        }
    };
    let mut out = String::new();
    let mut open: Option<Region> = None;
    let mut n = first;
    while n <= last {
        let here = region(facts, n);
        if here != open {
            if let Some(r) = open.take() {
                close(&mut out, &r, n - 1);
            }
            if let Some(r) = here {
                if hide && r.off {
                    let _ = writeln!(out, "        --- {} {} (hidden) ---", lines(r.a, r.b), what(&r));
                    n = r.b.min(last) + 1;
                    continue;
                }
                let _ = writeln!(out, "        >>> {} ({})", what(&r), lines(r.a, r.b));
            }
            open = here;
        }
        let _ = writeln!(out, "{n:>6}  {}", text.get(n as usize - 1).copied().unwrap_or(""));
        n += 1;
    }
    if let Some(r) = open {
        close(&mut out, &r, last);
    }
    out
}

/// `1 call`, `3 calls`.
fn count(n: usize, one: &str, more: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { more })
}

/// `line 8`, `lines 8–12`.
fn lines(a: u32, b: u32) -> String {
    if a == b { format!("line {a}") } else { format!("lines {a}–{b}") }
}

fn reply(id: &Value, result: Result<Value, (i64, String)>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    }
}

/// The answer to one message of the protocol, if it asks for one; `call`
/// runs a tool.
pub fn message(msg: &Value, root: Option<&Path>, call: impl FnOnce(&str, &Value) -> Result<String, String>) -> Option<Value> {
    // A notification (`notifications/initialized`, `…/cancelled`) asks
    // for no answer, and none of them for anything to be done.
    let id = msg.get("id").filter(|_| msg.get("method").is_some())?;
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = VERSIONS.iter().find(|&&v| v == asked).unwrap_or(VERSIONS.last().unwrap());
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "norn", "title": "Norn", "version": env!("CARGO_PKG_VERSION") },
                "instructions": instructions(root),
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let tool = params["name"].as_str().unwrap_or("");
            if !tools().as_array().unwrap().iter().any(|t| t["name"] == tool) {
                Err((-32602, format!("unknown tool `{tool}`")))
            } else {
                let (text, is_error) = match call(tool, &params["arguments"]) {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                Ok(json!({ "content": [{ "type": "text", "text": text.trim_end() }], "isError": is_error }))
            }
        }
        method => Err((-32601, format!("{method} is not supported"))),
    };
    Some(reply(id, result))
}

/// Which folder is the project, for a server started without one: the
/// folder it was started in, unless that is no project's (the home folder,
/// the root of the file system, where a desktop app starts it).
fn project_root(dir: &Path) -> Result<PathBuf, String> {
    // (Not `canonicalize`, which on Windows writes `\\?\C:\…`.)
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let same = |a: &Path, b: &Path| std::fs::canonicalize(a).ok().zip(std::fs::canonicalize(b).ok()).is_some_and(|(a, b)| a == b);
    let why = if dir.parent().is_none() {
        "the root of the file system"
    } else if std::env::home_dir().is_some_and(|home| same(&home, &dir)) {
        "your home folder"
    } else {
        return Ok(dir);
    };
    Err(format!(
        "Norn does not know which project to index: it was started in {} ({why}), and was not told of one. \
         Give the project's folder where this MCP server is set up: the command `norn` with the arguments `mcp` and the folder.",
        dir.display()
    ))
}

/// Where an answer goes: to this process's own client, or to another
/// process that asked.
enum To {
    Out,
    Other(std::sync::mpsc::Sender<Value>),
}

enum Ev {
    Line(String),
    Closed,
    /// The index this process made (of the start numbered so).
    Opened(u64, Box<((Session, Update), Option<Watch>)>),
    Asked(crate::share::Asked),
}

/// Whose index answers.
enum Mode {
    /// The project is not known yet (the client is asked for it).
    Unknown,
    /// No project to index, and why.
    Refused(String),
    /// This process's own: being made (`None`), or made. `held`: the others
    /// are answered from it.
    Own { server: Option<Box<Server>>, held: Option<crate::share::Held> },
    /// Another process's, that holds the project.
    Forward(crate::share::Line),
}

type Open = std::sync::Arc<dyn Fn(&Path) -> (Session, Update) + Send + Sync>;

struct Agent {
    root: Option<PathBuf>,
    mode: Mode,
    /// Tool calls that wait for the project or its index.
    waiting: Vec<(Value, To)>,
    events: std::sync::mpsc::Sender<Ev>,
    open: Open,
    /// Starts so far: an index made for an earlier one is let go.
    starts: u64,
    /// The client tells the folders it has open (MCP `roots`).
    roots: bool,
}

/// The id this server asks the client its roots with.
const ROOTS_ID: &str = "norn-roots";

impl Agent {
    fn send(&self, to: To, answer: Value) {
        match to {
            To::Out => {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{answer}").and_then(|_| out.flush());
            }
            To::Other(tx) => {
                let _ = tx.send(answer);
            }
        }
    }

    /// Index `root`, or reach the process that does.
    fn start(&mut self, root: PathBuf) {
        use crate::share::{claim, reach, Claim};
        self.starts += 1;
        self.root = Some(root.clone());
        self.mode = Mode::Unknown;
        for _ in 0..3 {
            match claim(&root) {
                Ok(Claim::Held(held, listener, word)) => {
                    let events = self.events.clone();
                    crate::share::serve(listener, word, move |asked| events.send(Ev::Asked(asked)).is_ok());
                    self.own(&root, Some(held));
                    break;
                }
                Ok(Claim::Taken(_)) | Ok(Claim::Busy) => match reach(&root, Duration::from_secs(3)) {
                    Ok((line, at)) => {
                        eprintln!("norn: answering from the index process {} keeps of {}", at.pid, root.display());
                        self.mode = Mode::Forward(line);
                        break;
                    }
                    // Let go meanwhile: try again to hold it.
                    Err(true) => continue,
                    // Of another version, or not answering: an index of its own.
                    Err(false) => {
                        eprintln!("norn: another process holds {} but does not answer; indexing it apart", root.display());
                        self.own(&root, None);
                        break;
                    }
                },
                Err(e) => {
                    eprintln!("norn: cannot share the index of {} ({e}); indexing it apart", root.display());
                    self.own(&root, None);
                    break;
                }
            }
        }
        if matches!(self.mode, Mode::Unknown) {
            self.own(&root, None);
        }
        for (msg, to) in std::mem::take(&mut self.waiting) {
            self.call(msg, to);
        }
    }

    fn own(&mut self, root: &Path, held: Option<crate::share::Held>) {
        let (events, open, start, root) = (self.events.clone(), self.open.clone(), self.starts, root.to_path_buf());
        std::thread::spawn(move || {
            // Watched first: what changes while it is indexed is read again.
            let watch = Watch::start(&root);
            let _ = events.send(Ev::Opened(start, Box::new((open(&root), watch))));
        });
        self.mode = Mode::Own { server: None, held };
    }

    /// A tool call, answered now or once there is an index to answer it.
    fn call(&mut self, msg: Value, to: To) {
        let root = self.root.clone();
        match &mut self.mode {
            Mode::Unknown | Mode::Own { server: None, .. } => self.waiting.push((msg, to)),
            Mode::Refused(why) => {
                let why = why.clone();
                let answer = message(&msg, root.as_deref(), |_, _| Err(why)).unwrap_or(Value::Null);
                self.send(to, answer);
            }
            Mode::Own { server: Some(server), .. } => {
                server.sync();
                let answer = message(&msg, Some(&server.root), |tool, args| Tools::new(&server.root, &mut server.session).call(tool, args)).unwrap_or(Value::Null);
                self.send(to, answer);
            }
            Mode::Forward(line) => match line.ask(&msg) {
                Ok(answer) => self.send(to, answer),
                // The holder is gone: hold the project now, or reach who does.
                Err(_) => {
                    let root = root.expect("a project to forward for");
                    self.start(root);
                    self.call(msg, to);
                }
            },
        }
    }

    /// The client's folders (`roots/list`): the first is the project.
    fn rooted(&mut self, result: &Value) {
        let first = result["roots"].as_array().and_then(|r| r.first()).and_then(|r| r["uri"].as_str()).filter(|u| u.starts_with("file:"));
        let dir = match first {
            Some(uri) => crate::lsp::uri_to_path(uri),
            None => std::env::current_dir().unwrap_or_default(),
        };
        match project_root(&dir) {
            Ok(root) => self.start(root),
            Err(why) => self.refuse(why),
        }
    }

    fn refuse(&mut self, why: String) {
        self.mode = Mode::Refused(why);
        for (msg, to) in std::mem::take(&mut self.waiting) {
            self.call(msg, to);
        }
    }

    fn line(&mut self, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            self.send(To::Out, reply(&Value::Null, Err((-32700, "not JSON".into()))));
            return;
        };
        // The client's answer to what this server asked it.
        if msg.get("method").is_none() {
            if msg["id"] == ROOTS_ID && self.root.is_none() {
                self.rooted(&msg["result"]);
            }
            return;
        }
        match msg["method"].as_str().unwrap_or("") {
            "tools/call" => self.call(msg, To::Out),
            "initialize" => {
                // Not given a folder: the client's, if it says, else the one started in.
                if self.root.is_none() {
                    self.roots = msg["params"]["capabilities"]["roots"].is_object();
                    if !self.roots {
                        let dir = std::env::current_dir().unwrap_or_default();
                        match project_root(&dir) {
                            Ok(root) => self.start(root),
                            Err(why) => self.refuse(why),
                        }
                    }
                }
                let answer = message(&msg, self.root.as_deref(), |_, _| unreachable!()).unwrap_or(Value::Null);
                self.send(To::Out, answer);
            }
            "notifications/initialized" if self.roots && self.root.is_none() => {
                self.send(To::Out, json!({ "jsonrpc": "2.0", "id": ROOTS_ID, "method": "roots/list" }));
            }
            _ => {
                if let Some(answer) = message(&msg, self.root.as_deref(), |_, _| unreachable!()) {
                    self.send(To::Out, answer);
                }
            }
        }
    }

    /// A language server wants the project: let it have it, and be
    /// answered from its index.
    fn yield_project(&mut self) {
        let Mode::Own { held: Some(_), .. } = &self.mode else { return };
        let root = self.root.clone().expect("a project held");
        // What this process made is let go, and the memory with it.
        self.mode = Mode::Unknown;
        self.starts += 1;
        si_core::trim_memory();
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            std::thread::sleep(Duration::from_millis(200));
            if let Ok((line, at)) = crate::share::reach(&root, Duration::from_millis(500)) {
                eprintln!("norn: answering from the index process {} keeps of {}", at.pid, root.display());
                self.mode = Mode::Forward(line);
                for (msg, to) in std::mem::take(&mut self.waiting) {
                    self.call(msg, to);
                }
                return;
            }
        }
        self.start(root);
    }
}

pub fn run(root: Option<&Path>, open: impl Fn(&Path) -> (Session, Update) + Send + Sync + 'static) {
    let (events, inbox) = std::sync::mpsc::channel();
    {
        let events = events.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty() && events.send(Ev::Line(line)).is_err() {
                    return;
                }
            }
            let _ = events.send(Ev::Closed);
        });
    }
    let mut agent = Agent { root: None, mode: Mode::Unknown, waiting: Vec::new(), events, open: std::sync::Arc::new(open), starts: 0, roots: false };
    // Given the folder: indexed at once, the agent answered meanwhile; its
    // first tool call waits for the index.
    if let Some(root) = root {
        agent.start(std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf()));
    }
    // The client gone: what it asked is still answered, once the index is
    // there to answer it.
    let mut closed = false;
    for event in inbox {
        match event {
            Ev::Line(line) => agent.line(&line),
            Ev::Closed => closed = true,
            Ev::Opened(start, opened) => {
                let ((session, _), watch) = *opened;
                if start != agent.starts {
                    continue;
                }
                if let (Mode::Own { server, .. }, Some(root)) = (&mut agent.mode, &agent.root) {
                    *server = Some(Box::new(Server { root: root.clone(), session, watch, synced: Instant::now() }));
                }
                for (msg, to) in std::mem::take(&mut agent.waiting) {
                    agent.call(msg, to);
                }
            }
            Ev::Asked(crate::share::Asked::Message(msg, tx)) => agent.call(msg, To::Other(tx)),
            Ev::Asked(crate::share::Asked::Yield) => agent.yield_project(),
        }
        // (Waiting on the client's roots, nothing more is to come.)
        if closed && (agent.waiting.is_empty() || matches!(agent.mode, Mode::Unknown) && agent.root.is_none()) {
            break;
        }
    }
    // The graph as it is now, for the next start to begin with.
    if let Mode::Own { server: Some(server), held: Some(_) } = &mut agent.mode {
        while server.session.save_some(4096) {}
        server.session.save_graph();
    }
}
