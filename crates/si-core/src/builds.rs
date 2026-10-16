//! What a tree's own build files say the build defines.
//!
//! Besides the descriptions `cond` reads (an Eclipse `.cproject`, the
//! C/C++ extension's properties, a compilation database), most trees
//! carry the build's own input: a Makefile, a `CMakeLists.txt`, an IDE's
//! project. None of them is run or fully understood here. What is taken
//! is what they say outright: a `-D` on a line no condition of the
//! build file guards, with a value that is written rather than computed.
//! That is less than the build defines, and it is what can be had
//! without anybody setting anything up.

use crate::cond::Build;
use std::path::Path;

/// How far below the root build files are looked for.
const DEPTH: usize = 3;
/// No more files than this are read for it.
const MOST: usize = 64;

/// The kinds of build file, the ones that say most first.
fn rank(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |x| x.1);
    Some(match (lower.as_str(), ext) {
        (".cproject", _) => 0,
        (_, "uvprojx" | "uvproj") => 1,
        (_, "ewp") => 2,
        (_, "vcxproj") => 3,
        ("c_cpp_properties.json", _) => 4,
        ("compile_commands.json", _) => 5,
        ("compile_flags.txt", _) => 6,
        ("platformio.ini", _) => 7,
        ("cmakelists.txt", _) => 8,
        ("makefile" | "gnumakefile", _) | (_, "mk") => 9,
        _ => return None,
    })
}

/// A Makefile, a `CMakeLists.txt`: a kind of file any folder may have.
pub(crate) fn is_general(path: &str) -> bool {
    rank(path.rsplit('/').next().unwrap_or(path)).is_some_and(|r| r >= 8)
}

/// The build files of the tree under `root`, as paths relative to it with
/// `/`, the ones that say most first, and nearer the root before deeper.
pub(crate) fn find(root: &Path) -> Vec<String> {
    let mut found: Vec<(u8, usize, String)> = Vec::new();
    let mut work = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, rel, depth)) = work.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                // Hidden folders hold tools' own state; `.vscode` holds the
                // C/C++ extension's properties.
                let hidden = name.starts_with('.') && name != ".vscode";
                if depth < DEPTH && !hidden && !matches!(name.as_str(), "node_modules" | "target") {
                    work.push((entry.path(), path, depth + 1));
                }
            } else if let Some(rank) = rank(&name) {
                found.push((rank, depth, path));
            }
        }
    }
    // Project files first, the ones that say most before the others;
    // then Makefiles and the like, nearest the root first.
    found.sort_by_key(|f| if f.0 >= 8 { (8, f.1, f.0, f.2.clone()) } else { (f.0, f.1, 0, f.2.clone()) });
    found.truncate(MOST);
    found.into_iter().map(|f| f.2).collect()
}

/// The configurations in the build file at `path`; `None` for a file that
/// is none of the kinds read here.
pub(crate) fn of(path: &str, text: &str) -> Option<Vec<Build>> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let one = |b: Build| if b.defines.is_empty() && b.undefines.is_empty() && b.compiler.is_none() { Vec::new() } else { vec![Build { name: name.to_string(), ..b }] };
    Some(match rank(name)? {
        1 => uvision(text),
        2 => iar(text),
        3 => msbuild(text),
        6 => {
            let mut b = Build::default();
            words(&text.split_whitespace().collect::<Vec<_>>(), &mut b);
            one(b)
        }
        7 => platformio(text),
        8 => one(cmake(text)),
        9 => one(makefile(text)),
        _ => return None,
    })
}

/// `-DNAME=value`, `-D NAME`, `-UNAME` among the words of a command line.
/// A value the build file computes (`$(VERSION)`, `${x}`) is not a value
/// to go by: such a name is left out.
fn words(words: &[&str], into: &mut Build) {
    let mut words = words.iter();
    while let Some(word) = words.next() {
        let word = unquoted(word);
        for (flag, define) in [("-D", true), ("-U", false), ("/D", true), ("/U", false)] {
            let Some(rest) = word.strip_prefix(flag) else { continue };
            let value = if rest.is_empty() { words.next().copied().unwrap_or("") } else { rest };
            define_one(value, define, into);
            break;
        }
    }
}

/// `text` without the quotes around all of it, if it has them.
fn unquoted(text: &str) -> &str {
    let text = text.trim();
    match text.chars().next() {
        Some(q @ ('"' | '\'')) if text.len() >= 2 && text.ends_with(q) => &text[1..text.len() - 1],
        _ => text,
    }
}

fn define_one(text: &str, define: bool, into: &mut Build) {
    // `'NAME="x"'`, and `NAME='"x"'`: the shell's quotes are not the value's.
    let text = unquoted(text);
    let text = match text.split_once('=') {
        Some((name, value)) if value.starts_with('\'') => format!("{name}={}", unquoted(value)),
        _ => text.to_string(),
    };
    let text = text.as_str();
    let name = text.split('=').next().unwrap_or("");
    let is_name = !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !is_name || text.contains(['$', '%', '`']) {
        return;
    }
    let list = if define { &mut into.defines } else { &mut into.undefines };
    if !list.iter().any(|d| d == text) {
        list.push(text.replace("\\\"", "\""));
    }
}

/// Lines joined where a backslash continues them.
fn joined(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let (body, more) = match line.trim_end().strip_suffix('\\') {
            Some(body) => (body, true),
            None => (line, false),
        };
        match out.last_mut().filter(|_| open) {
            Some(last) => {
                last.push(' ');
                last.push_str(body.trim_start());
            }
            None => out.push(body.to_string()),
        }
        open = more;
    }
    out
}

/// A Makefile: the `-D`s on lines outside every `ifdef`/`ifeq`, comments
/// apart. Whether the variable they are added to reaches the compiler is
/// not followed.
fn makefile(text: &str) -> Build {
    let mut b = Build::default();
    let mut depth = 0usize;
    for line in joined(text) {
        let line = line.split('#').next().unwrap_or("");
        let first = line.split_whitespace().next().unwrap_or("");
        match first {
            "ifdef" | "ifndef" | "ifeq" | "ifneq" => depth += 1,
            "endif" => depth = depth.saturating_sub(1),
            _ if depth == 0 && line.contains("-D") | line.contains("-U") => words(&line.split_whitespace().collect::<Vec<_>>(), &mut b),
            // `CC = $(PREFIX)gcc`, `CC := armcc`: the compiler.
            _ if depth == 0 && b.compiler.is_none() && (first == "CC" || first.starts_with("CC=") || first.starts_with("CC:=")) => {
                b.compiler = crate::cond::compiler_named(line.split_once('=').map_or("", |s| s.1));
            }
            _ => {}
        }
    }
    b
}

/// A `CMakeLists.txt`: `add_definitions`, `add_compile_definitions` and
/// `target_compile_definitions` outside every `if ()`.
fn cmake(text: &str) -> Build {
    let mut b = Build::default();
    let mut depth = 0usize;
    // Commands may run over lines: read up to the closing parenthesis.
    let plain: String = text.lines().map(|l| l.split('#').next().unwrap_or("")).collect::<Vec<_>>().join("\n");
    let mut rest = plain.as_str();
    while let Some(open) = rest.find('(') {
        let command = rest[..open].split_whitespace().last().unwrap_or("").to_ascii_lowercase();
        let close = rest[open..].find(')').map_or(rest.len(), |c| open + c);
        let args: Vec<&str> = rest[open + 1..close].split_whitespace().collect();
        match command.as_str() {
            "if" | "foreach" | "while" | "function" | "macro" => depth += 1,
            "endif" | "endforeach" | "endwhile" | "endfunction" | "endmacro" => depth = depth.saturating_sub(1),
            "add_definitions" if depth == 0 => words(&args, &mut b),
            "set" if args.first() == Some(&"CMAKE_C_COMPILER") => b.compiler = args.get(1).and_then(|c| crate::cond::compiler_named(c)),
            "add_compile_definitions" | "target_compile_definitions" if depth == 0 => {
                let skip = if command.starts_with("target") { 1 } else { 0 };
                for arg in args.iter().skip(skip).filter(|a| !matches!(**a, "PRIVATE" | "PUBLIC" | "INTERFACE")) {
                    define_one(arg.strip_prefix("-D").unwrap_or(arg), true, &mut b);
                }
            }
            _ => {}
        }
        rest = &rest[(close + 1).min(rest.len())..];
    }
    b
}

/// A PlatformIO project: `build_flags` of each `[env:name]`, with those of
/// `[env]`, which every environment has.
fn platformio(text: &str) -> Vec<Build> {
    let mut sections: Vec<(String, Build)> = Vec::new();
    let mut in_flags = false;
    for line in text.lines() {
        let plain = line.split([';', '#']).next().unwrap_or("");
        if let Some(name) = plain.trim().strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_string(), Build::default()));
            in_flags = false;
            continue;
        }
        let Some((_, b)) = sections.last_mut() else { continue };
        // A value goes on over the indented lines that follow.
        let continued = line.starts_with([' ', '\t']) && in_flags;
        let value = match plain.split_once('=') {
            Some((key, value)) if !continued => {
                in_flags = key.trim() == "build_flags";
                value
            }
            _ => plain,
        };
        if in_flags {
            words(&value.split_whitespace().collect::<Vec<_>>(), b);
        }
    }
    let common = sections.iter().find(|s| s.0 == "env").map(|s| (s.1.defines.clone(), s.1.undefines.clone())).unwrap_or_default();
    sections
        .into_iter()
        .filter_map(|(name, mut b)| {
            let name = name.strip_prefix("env:")?.to_string();
            b.defines.splice(0..0, common.0.iter().filter(|d| !b.defines.contains(d)).cloned().collect::<Vec<_>>());
            b.undefines.extend(common.1.iter().cloned());
            Some(Build { name, ..b })
        })
        .collect()
}

/// The text between `<tag>` (or `<tag ...>`) and `</tag>`, one after another.
fn elements<'a>(text: &'a str, tag: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut rest = text;
    std::iter::from_fn(move || loop {
        let at = rest.find(&open)?;
        let after = &rest[at + open.len()..];
        // `<Define>` is not `<DefineSomething>`.
        if !after.starts_with(['>', ' ', '\t', '\r', '\n', '/']) {
            rest = after;
            continue;
        }
        let head_end = after.find('>')?;
        let head = &after[..head_end];
        if head.ends_with('/') {
            rest = &after[head_end + 1..];
            return Some((head, ""));
        }
        let body = &after[head_end + 1..];
        let end = body.find(&close)?;
        rest = &body[end + close.len()..];
        return Some((head, &body[..end]));
    })
}

fn unescaped(text: &str) -> String {
    text.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// A Keil µVision project: for each target, what its C compiler defines.
fn uvision(text: &str) -> Vec<Build> {
    let mut out = Vec::new();
    for (_, target) in elements(text, "Target") {
        let Some((_, name)) = elements(target, "TargetName").next() else { continue };
        // Arm Compiler 6 where the target says so, else 5.
        let ac6 = elements(target, "uAC6").next().is_some_and(|u| u.1.trim() == "1");
        let compiler = Some(if ac6 { crate::cond::Compiler::Armclang } else { crate::cond::Compiler::Armcc });
        let mut b = Build { name: name.trim().to_string(), compiler, ..Build::default() };
        // The compiler's own settings come first; groups and files may
        // have theirs, further down, which are not the build's.
        if let Some((_, compiler)) = elements(target, "Cads").next() {
            for (_, list) in elements(compiler, "Define") {
                unescaped(list).split([',', ' ']).for_each(|d| define_one(d, true, &mut b));
            }
            for (_, list) in elements(compiler, "Undefine") {
                unescaped(list).split([',', ' ']).for_each(|d| define_one(d, false, &mut b));
            }
        }
        out.push(b);
    }
    out
}

/// An IAR Embedded Workbench project: for each configuration, the
/// compiler's defined symbols.
fn iar(text: &str) -> Vec<Build> {
    let mut out = Vec::new();
    for (_, configuration) in elements(text, "configuration") {
        let Some((_, name)) = elements(configuration, "name").next() else { continue };
        let mut b = Build { name: name.trim().to_string(), compiler: Some(crate::cond::Compiler::Iar), ..Build::default() };
        for (_, option) in elements(configuration, "option") {
            if elements(option, "name").next().is_some_and(|n| n.1.trim() == "CCDefines") {
                elements(option, "state").for_each(|s| define_one(&unescaped(s.1), true, &mut b));
            }
        }
        out.push(b);
    }
    out
}

/// A Visual Studio project: for each configuration, the compiler's
/// preprocessor definitions.
fn msbuild(text: &str) -> Vec<Build> {
    let mut out: Vec<Build> = Vec::new();
    for (head, group) in elements(text, "ItemDefinitionGroup") {
        // Condition="'$(Configuration)|$(Platform)'=='Debug|x64'"
        let name = head.split("==").nth(1).map_or("", |n| n.trim_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace()));
        let mut b = Build { name: if name.is_empty() { "Default".to_string() } else { name.to_string() }, ..Build::default() };
        for (_, compile) in elements(group, "ClCompile") {
            for (_, list) in elements(compile, "PreprocessorDefinitions") {
                unescaped(list).split(';').for_each(|d| define_one(d, true, &mut b));
            }
            for (_, list) in elements(compile, "UndefinePreprocessorDefinitions") {
                unescaped(list).split(';').for_each(|d| define_one(d, false, &mut b));
            }
        }
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defines(builds: &[Build]) -> Vec<(&str, Vec<&str>)> {
        builds.iter().map(|b| (b.name.as_str(), b.defines.iter().map(String::as_str).collect())).collect()
    }

    #[test]
    fn a_makefile_says_what_it_defines_outright() {
        let text = "VERSION = 3\nCFLAGS += -DUSE_NET -DLEVEL=2 \\\n  -DNAME='\"x\"' -DVER=$(VERSION) -Iinc\nifdef NO_ICONV\nCFLAGS += -DNO_ICONV\nendif\n# CFLAGS += -DCOMMENTED\nall:\n\t$(CC) -D BOARD=7 -UOLD -c a.c\n";
        let b = of("sub/Makefile", text).unwrap();
        assert_eq!(defines(&b), [("Makefile", vec!["USE_NET", "LEVEL=2", "NAME=\"x\"", "BOARD=7"])]);
        assert_eq!(b[0].undefines, ["OLD"]);
        // Nothing said outright: no configuration.
        assert_eq!(of("rules.mk", "ifdef X\nCFLAGS += -DX\nendif\n").unwrap().len(), 0);
    }

    #[test]
    fn a_cmake_file_says_what_it_defines_outright() {
        let text = "add_definitions(-DUSE_NET -DLEVEL=2)\nif (WIN32)\n  add_definitions(-DON_WINDOWS)\nendif ()\nadd_compile_definitions(\n  BOARD=7 # which\n  VER=${VERSION})\ntarget_compile_definitions(app PRIVATE APP_ONLY)\n";
        assert_eq!(defines(&of("CMakeLists.txt", text).unwrap()), [("CMakeLists.txt", vec!["USE_NET", "LEVEL=2", "BOARD=7", "APP_ONLY"])]);
    }

    #[test]
    fn an_ide_project_gives_its_configurations() {
        let keil = "<Project><Targets><Target><TargetName>Board A</TargetName><TargetOption><TargetArmAds><Cads><VariousControls><Define>USE_HAL_DRIVER, STM32F407xx,LEVEL=2</Define><Undefine>OLD</Undefine></VariousControls></Cads><Aads><VariousControls><Define>ASM_ONLY</Define></VariousControls></Aads></TargetArmAds></TargetOption></Target><Target><TargetName>Board B</TargetName><Cads><Define></Define></Cads></Target></Targets></Project>";
        assert_eq!(defines(&of("app.uvprojx", keil).unwrap()), [("Board A", vec!["USE_HAL_DRIVER", "STM32F407xx", "LEVEL=2"]), ("Board B", vec![])]);
        let iar = "<project><configuration><name>Debug</name><settings><name>ICCARM</name><data><option><name>CCDefines</name><state>DEBUG</state><state>BOARD=7</state></option><option><name>CCIncludePath2</name><state>$PROJ_DIR$/inc</state></option></data></settings></configuration></project>";
        assert_eq!(defines(&of("app.ewp", iar).unwrap()), [("Debug", vec!["DEBUG", "BOARD=7"])]);
        let vs = "<Project><ItemDefinitionGroup Condition=\"'$(Configuration)|$(Platform)'=='Debug|x64'\"><ClCompile><PreprocessorDefinitions>WIN32;_DEBUG;%(PreprocessorDefinitions)</PreprocessorDefinitions></ClCompile></ItemDefinitionGroup></Project>";
        assert_eq!(defines(&of("app.vcxproj", vs).unwrap()), [("Debug|x64", vec!["WIN32", "_DEBUG"])]);
        let pio = "[env]\nbuild_flags = -DCOMMON\n[env:uno]\nboard = uno\nbuild_flags =\n  -DBOARD_UNO\n  -DLEVEL=2 ; which\nlib_deps = x\n[env:due]\nbuild_flags = -DBOARD_DUE\n";
        assert_eq!(defines(&of("platformio.ini", pio).unwrap()), [("uno", vec!["COMMON", "BOARD_UNO", "LEVEL=2"]), ("due", vec!["COMMON", "BOARD_DUE"])]);
        assert_eq!(defines(&of("compile_flags.txt", "-DFROM_CLANGD\n-Iinc\n-D\nSPACED=1\n").unwrap()), [("compile_flags.txt", vec!["FROM_CLANGD", "SPACED=1"])]);
    }
}
