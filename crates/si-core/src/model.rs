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
    /// A member of a struct or union.
    pub const FIELD: u8 = 9;

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
            FIELD => "field",
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
    /// A typedef of a function or function pointer type, or a member that
    /// can hold a function.
    pub const CALLABLE: u8 = 8;
}

pub mod refkind {
    pub const CALL: u8 = 0;
    pub const USE: u8 = 1;
    /// A member access: `x.name`, `x->name`, or `.name =` in an initializer.
    pub const MEMBER: u8 = 2;
    /// A call through a member: `x->name (...)`.
    pub const ICALL: u8 = 3;
    /// Not a reference in the source: an edge from a field or variable to a
    /// function stored in it.
    pub const BIND: u8 = 4;
    /// A call through a local pointer variable that the function assigns
    /// this name to: `fn = cond ? f : g; fn (x);`.
    pub const PCALL: u8 = 5;
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
    /// The recorded arguments follow a name that takes none itself.
    pub const TRAILING: u8 = 32;
}

/// The text of a `#define` an expansion may need: `text` is everything after
/// the name, parameter list included.
#[derive(Clone, Copy, Debug)]
pub struct MacroBody {
    pub name: u32,
    pub text: u32,
    pub function_like: bool,
    /// The `#if NAME == value` tests around it, as `NAME == 64;OTHER != 1`,
    /// or `NONE`.
    pub guard: u32,
    /// Defined only under some `#if`.
    pub conditional: bool,
    /// Defined under `#ifndef` of its own name.
    pub fallback: bool,
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

/// A function stored where a call can later find it: in a struct member or
/// in a variable. `func` is only a name; whether it is a function is
/// decided at link time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bind {
    /// The member or variable name; or, when `index` is not `NONE`, the name
    /// of the struct type whose `index`-th member is meant (an initializer
    /// without designators).
    pub slot: u32,
    pub index: u32,
    pub func: u32,
    pub line: u32,
    pub scope: u32,
    /// The slot is a variable, not a member.
    pub var: bool,
    /// The translation unit (a file index, set at link time) in which the
    /// store reads this way, when macros decide that; else `NONE`.
    pub unit: u32,
}

/// A call a function makes through one of its parameters.
#[derive(Clone, Copy, Debug)]
pub struct ParamCall {
    /// The function, as a definition of this file.
    pub scope: u32,
    /// The parameter's position.
    pub index: u32,
    pub line: u32,
}

/// A name passed as an argument in a call by name: if the callee calls
/// through that parameter and the name is a function, that is who it calls.
#[derive(Clone, Copy, Debug)]
pub struct ArgPass {
    pub callee: u32,
    pub index: u32,
    pub func: u32,
}

/// A braced initializer with macro uses in it, kept as text: which member
/// each element initializes is only known once the macros are expanded.
#[derive(Clone, Copy, Debug)]
pub struct InitSite {
    /// The struct type's name, or `NONE`.
    pub type_name: u32,
    pub text: u32,
    pub line: u32,
    pub scope: u32,
    /// The initialized object is an array of the type.
    pub array: bool,
}

/// The members of a struct or union, in order (`NONE` for an unnamed one).
#[derive(Clone, Debug)]
pub struct Record {
    pub name: u32,
    pub fields: Vec<u32>,
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
    pub binds: Vec<Bind>,
    pub inits: Vec<InitSite>,
    pub records: Vec<Record>,
    /// `typedef struct tag name;` as (name, tag).
    pub aliases: Vec<(u32, u32)>,
    /// Members declared with a typedef name, as (definition, type name):
    /// whether they can hold a function depends on the typedef.
    pub field_types: Vec<(u32, u32)>,
    pub param_calls: Vec<ParamCall>,
    pub arg_passes: Vec<ArgPass>,
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
