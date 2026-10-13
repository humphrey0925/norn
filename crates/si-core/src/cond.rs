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
}

impl Settings {
    pub fn load(root: &Path) -> Settings {
        let path = crate::scope::Scope::config_path(root);
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        *self == Settings::default()
    }

    /// Into `.si/config.json`, beside the scope.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        crate::scope::write_settings(root, serde_json::to_value(self).unwrap())
    }
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
            if let Ok(text) = std::fs::read(root.join(file)) {
                c.read_file(&String::from_utf8_lossy(&text));
            }
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
        d ^ u ^ a ^ s
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

    /// The value of a name in an `#if`: its definition if that is a number,
    /// 0 if it is known not to be defined (as the preprocessor has it).
    fn value(&self, name: &[u8]) -> Option<i64> {
        match self.defined.get(name) {
            Some(v) => number(v).or(if v.is_empty() { None } else { self.value(v).filter(|_| v != name) }),
            None => (self.defined(name) == Tri::False).then_some(0),
        }
    }

    /// Whether the condition of an `#if` or `#elif` holds, as far as known.
    pub fn eval(&self, expr: &[u8]) -> Tri {
        let toks = tokens(expr);
        let mut p = Parser { c: self, toks: &toks, at: 0 };
        let v = p.or();
        if p.at != toks.len() {
            return Tri::Unknown;
        }
        truth(v)
    }
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
    c: &'a Conditions,
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
                    return self.c.value(name);
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
