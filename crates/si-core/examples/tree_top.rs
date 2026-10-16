//! Diagnostic: top-level shape of one file's parse tree (after masking).
//!
//!     cargo run --release -p si-core --example tree_top -- <file> [raw | <corpus root>]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = std::fs::read(&args[1]).unwrap();
    let src = if args.get(2).is_some_and(|s| s == "raw") {
        src
    } else {
        // With a corpus root, use its transparent macros rather than this file's own.
        let t = match args.get(2) {
            Some(root) => si_core::prescan(&si_core::scan(std::path::Path::new(root))),
            None => si_core::mask::MacroSets::from_defines(si_core::mask::defines(&src, &si_core::cond::Conditions::default())),
        };
        si_core::mask::lex(&src, &t).masked
    };
    let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).unwrap();
    let tree = parser.parse(&src, None).unwrap();
    let root = tree.root_node();
    println!("root {} error={} children={} bytes={}", root.kind(), root.is_error(), root.child_count(), src.len());
    let mut c = root.walk();
    for ch in root.children(&mut c).filter(|n| n.is_error() || n.byte_range().len() > src.len() / 20) {
        let text = String::from_utf8_lossy(&src[ch.start_byte()..(ch.start_byte() + 60).min(src.len())]).replace('\n', "\\n");
        println!("  {:22} lines {}-{} error={} has_error={}  {text}", ch.kind(), ch.start_position().row + 1, ch.end_position().row + 1, ch.is_error(), ch.has_error());
    }
}
