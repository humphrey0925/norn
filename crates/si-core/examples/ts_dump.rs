//! Print the tree a grammar makes of a file: `ts_dump c|cpp <file>`.
use tree_sitter::{Language, Parser};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let lang: Language = if args[1] == "cpp" { tree_sitter_cpp::LANGUAGE.into() } else { tree_sitter_c::LANGUAGE.into() };
    let src = std::fs::read(&args[2]).unwrap();
    let mut p = Parser::new();
    p.set_language(&lang).unwrap();
    let t = std::time::Instant::now();
    let tree = p.parse(&src, None).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    if args.get(3).map(String::as_str) != Some("--time") {
        let mut c = tree.walk();
        let mut depth = 0;
        'outer: loop {
            let n = c.node();
            if n.is_named() {
                let text = if n.child_count() == 0 { format!(" {:?}", n.utf8_text(&src).unwrap_or("")) } else { String::new() };
                let field = c.field_name().map(|f| format!("{f}: ")).unwrap_or_default();
                println!("{:w$}{field}{}{text}  [{}]", "", n.kind(), n.start_position().row + 1, w = depth * 2);
            }
            if c.goto_first_child() { depth += 1; continue; }
            loop {
                if c.goto_next_sibling() { break; }
                if !c.goto_parent() { break 'outer; }
                depth -= 1;
            }
        }
    }
    eprintln!("{} bytes, {:.2} ms, error: {}", src.len(), ms, tree.root_node().has_error());
}
