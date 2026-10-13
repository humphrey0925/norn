//! Shared data model: per-file facts (extraction output) and the linked graph.

use bytemuck::{Pod, Zeroable};

pub mod kind {
    pub const FILE: u8 = 0;
    pub const FUNCTION: u8 = 1;
    pub const MACRO: u8 = 2;
    pub const STRUCT: u8 = 3;
    pub const UNION: u8 = 4;
    pub const ENUM: u8 = 5;
    pub const ENUM_CONST: u8 = 6;
    pub const TYPEDEF: u8 = 7;
    pub const GLOBAL: u8 = 8;

    pub fn name(k: u8) -> &'static str {
        match k {
            FILE => "file",
            FUNCTION => "function",
            MACRO => "macro",
            STRUCT => "struct",
            UNION => "union",
            ENUM => "enum",
            ENUM_CONST => "enum_const",
            TYPEDEF => "typedef",
            GLOBAL => "global",
            _ => "?",
        }
    }
}

pub mod flag {
    /// `static` storage class: invisible outside its translation unit unless in a header.
    pub const STATIC: u8 = 1;
    /// Declaration only (prototype / `extern`), not a definition.
    pub const DECL: u8 = 2;
    /// Defined in a header file.
    pub const HEADER: u8 = 4;
}

pub mod refkind {
    pub const CALL: u8 = 0;
    pub const USE: u8 = 1;
}

pub const NONE: u32 = u32::MAX;

/// Where a use of an expanding macro stands.
pub mod site {
    /// The whole use is directly followed by `(`.
    pub const CALLED: u8 = 1;
    /// Inside a function body.
    pub const IN_FN: u8 = 2;
    /// Outside every bracket, lexically.
    pub const TOP: u8 = 4;
    /// The whole use is directly followed by `{`.
    pub const BRACE_FOLLOWS: u8 = 8;
    /// Inside an initializer's braces.
    pub const INIT: u8 = 16;
}

/// The text of a `#define` an expansion may need: `text` is everything after
/// the name, parameter list included.
#[derive(Clone, Copy, Debug)]
pub struct MacroBody {
    pub name: u32,
    pub text: u32,
    pub function_like: bool,
}

/// A use of an expanding macro, to be expanded at link time, when the
/// definitions in other files are known.
#[derive(Clone, Copy, Debug)]
pub struct Site {
    pub name: u32,
    /// The argument list as written, parentheses included, or `NONE`.
    pub args: u32,
    pub line: u32,
    pub scope: u32,
    /// The definition this use names (`NAME (a, b) (void) { }`), or `NONE`.
    pub def: u32,
    pub flags: u8,
}

/// A definition found in one file. `name` indexes the file-local name table.
#[derive(Clone, Copy, Debug)]
pub struct Def {
    pub name: u32,
    pub line: u32,
    pub end_line: u32,
    pub kind: u8,
    pub flags: u8,
}

/// A reference found in one file. `scope` is the index of the enclosing def
/// in the same file (0 = the file itself).
#[derive(Clone, Copy, Debug)]
pub struct Ref {
    pub name: u32,
    pub line: u32,
    pub scope: u32,
    pub kind: u8,
}

/// Everything extracted from a single file. Depends only on that file's
/// content and the transparent-macro set, so it can be produced in parallel
/// and cached by (content hash, set digest).
#[derive(Clone, Debug, Default)]
pub struct FileFacts {
    pub path: String,
    pub hash: u64,
    pub bytes: u32,
    pub has_error: bool,
    pub indirect_calls: u32,
    /// Wall time spent in the lexical mask, the tree-sitter parse and the tree walk.
    pub mask_ns: u64,
    pub parse_ns: u64,
    pub walk_ns: u64,
    name_buf: String,
    name_off: Vec<u32>,
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    /// `#include` operands as written, as file-local name ids.
    pub includes: Vec<u32>,
    pub macros: Vec<MacroBody>,
    pub sites: Vec<Site>,
}

impl FileFacts {
    pub fn new(path: String) -> Self {
        FileFacts { path, name_off: vec![0], ..Default::default() }
    }

    pub fn push_name(&mut self, s: &str) -> u32 {
        self.name_buf.push_str(s);
        self.name_off.push(self.name_buf.len() as u32);
        (self.name_off.len() - 2) as u32
    }

    pub fn n_names(&self) -> usize {
        self.name_off.len() - 1
    }

    pub fn name(&self, i: usize) -> &str {
        &self.name_buf[self.name_off[i] as usize..self.name_off[i + 1] as usize]
    }

    pub fn heap_bytes(&self) -> usize {
        self.path.capacity()
            + self.name_buf.capacity()
            + self.name_off.capacity() * 4
            + self.defs.capacity() * std::mem::size_of::<Def>()
            + self.refs.capacity() * std::mem::size_of::<Ref>()
            + self.includes.capacity() * 4
            + self.macros.capacity() * std::mem::size_of::<MacroBody>()
            + self.sites.capacity() * std::mem::size_of::<Site>()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Sym {
    pub name: u32,
    pub file: u32,
    pub line: u32,
    pub end_line: u32,
    pub kind: u8,
    pub flags: u8,
    pub _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct FileRec {
    pub hash: u64,
    /// Name id of the file path.
    pub path: u32,
    /// First symbol id of this file; its symbols are contiguous and sorted by line.
    pub sym_start: u32,
}

/// One half of an edge as stored in a CSR adjacency list.
/// `meta` = line << 4 | ambiguous << 3 | refkind.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct EdgeHalf {
    pub other: u32,
    pub meta: u32,
}

impl EdgeHalf {
    pub fn line(&self) -> u32 {
        self.meta >> 4
    }
    pub fn ambiguous(&self) -> bool {
        self.meta & 8 != 0
    }
    pub fn kind(&self) -> u8 {
        (self.meta & 7) as u8
    }
}

pub fn pack_meta(line: u32, ambiguous: bool, kind: u8) -> u32 {
    (line << 4) | ((ambiguous as u32) << 3) | kind as u32
}
