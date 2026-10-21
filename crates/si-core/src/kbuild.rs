//! What a Kbuild build compiles, as its makefiles say.
//!
//! Each folder's `Kbuild` (or else `Makefile`) lists what is built there:
//! `obj-$(CONFIG_X) += foo.o sub/`, `lib-y += bar.o`, the parts of a
//! composite object (`foo-y += a.o b.o`, `foo-objs := ...`), all under
//! `ifdef`, `ifeq` and the like. With the configuration's values a line comes
//! to `obj-y` or `obj-m` (built) or to anything else (not). A file or folder
//! is taken as not built only where every line that names it says so: one
//! the makefiles do not name, or name in a way not worked out here, is left
//! alone.

use crate::cond::Tri;
use rustc_hash::FxHashMap;
use std::path::Path;

/// Lists of folders named from the root, whichever makefile they are in
/// (Linux's `core-y`, U-Boot's `libs-y`).
const FROM_ROOT: &[&str] = &["core", "drivers", "libs", "init", "net", "virt", "head"];
/// Lists of what is built in the folder of the makefile.
const BUILT: &[&str] = &["obj", "lib"];

/// The source files of the tree under `root` its makefiles do not build:
/// those named as objects not built, and those in folders not built.
/// (Headers are not for them to say: a folder listed only for a test of
/// its headers, Linux's `obj-$(CONFIG_DRM_HEADER_TEST) += include/`, is
/// still read.) `value`: what make has for a variable (`CONFIG_X`: `y`,
/// `m`, a string, or empty when not set), `None` when not known.
pub fn not_built(root: &Path, value: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let t = std::time::Instant::now();
    let mut sources: Vec<String> = Vec::new();
    let mut said: FxHashMap<String, Tri> = FxHashMap::default();
    let mut note = |key: String, t: Tri| {
        let e = said.entry(key).or_insert(Tri::False);
        *e = best(*e, t);
    };
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else { continue };
        let mut names: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.file_type().ok()?.is_dir())))
            .filter(|(n, _)| !n.starts_with('.'))
            .collect();
        names.sort();
        let at = |n: &str| if dir.is_empty() { n.to_string() } else { format!("{dir}/{n}") };
        dirs.extend(names.iter().filter(|(_, d)| *d).map(|(n, _)| at(n)));
        let source = |n: &str| [".c", ".S", ".s", ".cc", ".cpp"].iter().any(|e| n.ends_with(e));
        sources.extend(names.iter().filter(|(n, d)| !d && source(n)).map(|(n, _)| at(n)));
        // (Both, where there are both: Linux's `arch/x86/Kbuild` lists what
        // is built there, its `Makefile` the folders `core-y` adds.)
        let mut lines = Vec::new();
        for f in ["Kbuild", "Makefile"] {
            if names.iter().any(|(n, d)| n == f && !d) {
                if let Ok(text) = std::fs::read(root.join(at(f))) {
                    lines.extend(assignments(&String::from_utf8_lossy(&text), value));
                }
            }
        }
        if lines.is_empty() {
            continue;
        }
        // What the folder's own lists build, and then the parts of
        // composites they build.
        let mut objects: FxHashMap<&str, Tri> = FxHashMap::default();
        for (base, t, members) in &lines {
            if BUILT.contains(&base.as_str()) {
                for m in members.iter().filter_map(|m| m.strip_suffix(".o")) {
                    let e = objects.entry(m).or_insert(Tri::False);
                    *e = best(*e, *t);
                }
            }
        }
        for (base, t, members) in &lines {
            let from_root = FROM_ROOT.contains(&base.as_str());
            let t = if BUILT.contains(&base.as_str()) || from_root { *t } else { t.and(objects.get(base.as_str()).copied().unwrap_or(Tri::Unknown)) };
            for m in members {
                let path = if from_root { normal(m) } else { normal(&at(m)) };
                if let Some(path) = path.filter(|p| !p.is_empty()) {
                    if from_root && !m.ends_with('/') {
                        continue;
                    }
                    note(path, t);
                }
            }
        }
    }
    // A folder not built is so whatever is said of what is in it.
    let off = |key: &str| said.get(key) == Some(&Tri::False);
    let mut out: Vec<String> = sources
        .into_iter()
        .filter(|p| {
            let object = p.rsplit_once('.').map_or(String::new(), |(stem, _)| format!("{stem}.o"));
            off(&object) || p.match_indices('/').any(|(i, _)| off(&p[..=i]))
        })
        .collect();
    out.sort();
    // `SI_KBUILD_WHY`: what is taken as not built.
    if std::env::var_os("SI_KBUILD_WHY").is_some() {
        eprintln!("kbuild    {} not built, {:.0} ms", out.len(), t.elapsed().as_secs_f64() * 1000.0);
        out.iter().for_each(|p| eprintln!("kbuild    {p}"));
    }
    out
}

/// Of True, Unknown and False, the one that says most for being built.
fn best(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
        _ => Tri::False,
    }
}

/// `a/./b/../c` as `a/c`; `None` where it leaves the tree.
fn normal(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    let dir = path.ends_with('/');
    Some(parts.join("/") + if dir && !parts.is_empty() { "/" } else { "" })
}

/// The lists a makefile adds to: each as (its name, whether it is built
/// where the line stands, what it lists), `obj-$(CONFIG_X) += a.o b/` as
/// ("obj", CONFIG_X set, ["a.o", "b/"]).
fn assignments(text: &str, value: &dyn Fn(&str) -> Option<String>) -> Vec<(String, Tri, Vec<String>)> {
    let mut out = Vec::new();
    // Per open `if`: whether its branch in force is, and whether one of its
    // branches has been.
    let mut stack: Vec<(Tri, Tri)> = Vec::new();
    let mut in_define = false;
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    for line in joined.lines() {
        let line = line.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let word = trimmed.split_whitespace().next().unwrap_or("");
        if in_define {
            in_define = word != "endef";
            continue;
        }
        match word {
            "define" => {
                in_define = true;
                continue;
            }
            "ifdef" | "ifndef" | "ifeq" | "ifneq" => {
                let t = test(trimmed, value);
                stack.push((t, t));
                continue;
            }
            "else" => {
                if let Some(top) = stack.last_mut() {
                    let rest = trimmed["else".len()..].trim();
                    let t = if rest.is_empty() { Tri::True } else { test(rest, value) };
                    let now = (!top.1).and(t);
                    *top = (now, top.1.or(now));
                }
                continue;
            }
            "endif" => {
                stack.pop();
                continue;
            }
            _ => {}
        }
        // `name op value`: op one of `+=`, `:=`, `?=`, `=`.
        let Some(eq) = line.find('=') else { continue };
        let lhs = line[..eq].trim_end_matches(['+', ':', '?']).trim();
        if lhs.is_empty() || lhs.contains(char::is_whitespace) || line[..eq].ends_with('!') {
            continue;
        }
        let here = stack.iter().fold(Tri::True, |acc, s| acc.and(s.0));
        let (base, state) = match expand(lhs, value).as_deref().and_then(|l| l.rsplit_once('-').map(|(b, s)| (b.to_string(), s.to_string()))) {
            Some((base, suffix)) => (base, if matches!(suffix.as_str(), "y" | "m" | "objs") { Tri::True } else { Tri::False }),
            // `obj-$(something not known)`: built or not.
            None => match lhs.split_once("-$") {
                Some((base, _)) => (base.to_string(), Tri::Unknown),
                None => continue,
            },
        };
        if base.is_empty() || !base.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            continue;
        }
        let members: Vec<String> = line[eq + 1..]
            .split_whitespace()
            .filter_map(|m| if m.contains('$') { expand(m, value) } else { Some(m.to_string()) })
            .filter(|m| !m.starts_with('-') && (m.ends_with(".o") || m.ends_with('/')) && m.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./+-".contains(&b)))
            .collect();
        if !members.is_empty() {
            out.push((base, here.and(state), members));
        }
    }
    out
}

/// What `ifdef X`, `ifeq (a,b)` and the like come to.
fn test(line: &str, value: &dyn Fn(&str) -> Option<String>) -> Tri {
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let known = |t: Option<bool>| t.map_or(Tri::Unknown, |b| if b { Tri::True } else { Tri::False });
    match word {
        "ifdef" | "ifndef" => {
            let set = expand(rest, value).and_then(|n| value(&n)).map(|v| !v.is_empty());
            let t = known(set);
            if word == "ifdef" { t } else { !t }
        }
        "ifeq" | "ifneq" => {
            let pair = rest
                .strip_prefix('(')
                .and_then(|r| r.strip_suffix(')'))
                .and_then(split_args)
                .or_else(|| {
                    // `ifeq "a" "b"`
                    let q: Vec<&str> = rest.split(['"', '\'']).collect();
                    (q.len() == 5).then(|| (q[1].to_string(), q[3].to_string()))
                });
            let same = pair.and_then(|(a, b)| Some(expand(a.trim(), value)? == expand(b.trim(), value)?));
            let t = known(same);
            if word == "ifeq" { t } else { !t }
        }
        _ => Tri::Unknown,
    }
}

/// `a,b` split at the comma outside `$(...)`.
fn split_args(s: &str) -> Option<(String, String)> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some((s[..i].to_string(), s[i + 1..].to_string())),
            _ => {}
        }
    }
    None
}

/// `s` with its `$(NAME)`s put for, innermost first; `None` where one is not
/// known (a function, or a variable other than a configuration's). U-Boot's
/// `$(PHASE_)`, `$(SPL_)` and the like are empty: the image itself, not SPL's.
fn expand(s: &str, value: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut s = s.to_string();
    for _ in 0..8 {
        let Some(open) = s.rfind("$(").or_else(|| s.rfind("${")) else { return Some(s) };
        let close = s[open..].find([')', '}'])? + open;
        let name = &s[open + 2..close];
        let v = match name {
            "SPL_" | "PHASE_" | "XPL_" | "SPL_TPL_" | "PHASE_PREFIX" => String::new(),
            n if n.starts_with("CONFIG_") && !n.contains(char::is_whitespace) => value(n)?,
            _ => return None,
        };
        s.replace_range(open..=close, &v);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_makefiles_build_and_what_not() {
        let dir = std::env::temp_dir().join(format!("si-kbuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = [
            ("Makefile", "libs-y += lib/
libs-$(CONFIG_API) += api/
"),
            ("api/api.c", ""),
            ("lib/Makefile", "ifeq ($(CONFIG_XPL_BUILD),y)
obj-$(CONFIG_$(PHASE_)SPRINTF) += tiny-printf.o
else
# Main U-Boot always uses the full printf support
obj-y += vsprintf.o \\
	strto.o
endif
obj-$(CONFIG_NET) += net.o
obj-$(CONFIG_ZLIB) += zlib/
obj-$(CONFIG_FS) += fs.o
fs-y := fs_core.o
fs-$(CONFIG_FS_EXT) += ext.o
obj-$(CONFIG_$(UNKNOWN)X) += maybe.o
ifdef CONFIG_NET
obj-y += both.o
endif
obj-$(CONFIG_OTHER) += both.o
"),
            ("lib/tiny-printf.c", ""),
            ("lib/vsprintf.c", ""),
            ("lib/strto.c", ""),
            ("lib/net.c", ""),
            ("lib/fs_core.c", ""),
            ("lib/ext.c", ""),
            ("lib/maybe.c", ""),
            ("lib/both.c", ""),
            ("lib/zlib/inflate.c", ""),
        ];
        for (p, src) in files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), src).unwrap();
        }
        let config: FxHashMap<&str, &str> = [("CONFIG_NET", "y"), ("CONFIG_FS", "m"), ("CONFIG_SPRINTF", "y")].into_iter().collect();
        let value = |n: &str| Some(config.get(n).copied().unwrap_or("").to_string());
        let got = not_built(&dir, &value);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, ["api/api.c", "lib/ext.c", "lib/tiny-printf.c", "lib/zlib/inflate.c"]);
    }

    #[test]
    fn a_path_is_made_plain() {
        assert_eq!(normal("a/./b/../c/"), Some("a/c/".to_string()));
        assert_eq!(normal("../x.o"), None);
    }
}
