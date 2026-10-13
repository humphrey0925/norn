//! Diagnostic: where does tree-sitter-c give up, and on what? Prints the most
//! common first tokens of ERROR nodes and the bytes they cover.
//!
//!     cargo run --release -p si-core --example parse_errors -- <root> [top]

use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let top: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let files = si_core::scan(root);
    // `raw` as third argument measures the parser on unmasked source.
    let raw = args.get(3).is_some_and(|s| s == "raw");
    let transparent = if raw { Default::default() } else { si_core::prescan(&files) };
    let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();

    let whole_file_errors = std::sync::atomic::AtomicU64::new(0);
    // (first token, parent kind) -> (count, bytes, example)
    type Tally = HashMap<(String, &'static str), (u64, u64, String)>;
    let per_file: Vec<(Tally, u64, u64)> = files
        .par_iter()
        .map(|p| {
            let src = std::fs::read(p).unwrap_or_default();
            let src = if raw { src } else { si_core::mask::lex(&src, &transparent).masked };
            // `SI_BLANK_COND=1`: what if no conditional directive were left
            // for the parser at all?
            let mut src = src;
            if std::env::var_os("SI_BLANK_COND").is_some() {
                let mut at = 0;
                while at < src.len() {
                    let end = src[at..].iter().position(|&b| b == b'\n').map_or(src.len(), |e| at + e);
                    let line = src[at..end].trim_ascii_start();
                    let word = line.strip_prefix(b"#").map(|r| r.trim_ascii_start());
                    if word.is_some_and(|w| [&b"if"[..], b"elif", b"else", b"endif"].iter().any(|k| w.starts_with(k))) {
                        src[at..end].iter_mut().for_each(|b| *b = b' ');
                    }
                    at = end + 1;
                }
            }
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&lang).unwrap();
            let tree = parser.parse(&src, None).unwrap();
            let mut tally = Tally::new();
            let (mut err_bytes, mut missing) = (0u64, 0u64);
            let mut cursor = tree.walk();
            // Kinds of the ancestors of the cursor's node (`Node::parent` searches from the root).
            let mut parents: Vec<&'static str> = Vec::new();
            'outer: loop {
                let n = cursor.node();
                let mut descend = n.has_error();
                if n.is_missing() {
                    missing += 1;
                } else if n.is_error() && parents.is_empty() {
                    // The root itself: recovery still produced children worth judging.
                    whole_file_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else if n.is_error() {
                    descend = false;
                    let text = String::from_utf8_lossy(&src[n.byte_range()]);
                    let first: String = text
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '#')
                        .collect();
                    let first = if first.is_empty() { text.trim_start().chars().take(1).collect() } else { first };
                    let parent = parents.last().copied().unwrap_or("");
                    let e = tally.entry((first, parent)).or_insert_with(|| {
                        let line = n.start_position().row + 1;
                        let snip: String = text.chars().take(70).collect::<String>().replace('\n', "\n");
                        (0, 0, format!("{}:{line}: {snip}", si_core::rel_path(root, p)))
                    });
                    e.0 += 1;
                    e.1 += n.byte_range().len() as u64;
                    err_bytes += n.byte_range().len() as u64;
                }
                if descend && cursor.goto_first_child() {
                    parents.push(n.kind());
                    continue;
                }
                loop {
                    if cursor.goto_next_sibling() {
                        break;
                    }
                    if !cursor.goto_parent() {
                        break 'outer;
                    }
                    parents.pop();
                }
            }
            (tally, err_bytes, missing)
        })
        .collect();

    let mut all = Tally::new();
    let (mut err_bytes, mut missing) = (0u64, 0u64);
    for (t, b, m) in per_file {
        err_bytes += b;
        missing += m;
        for (k, v) in t {
            let e = all.entry(k).or_insert((0, 0, v.2));
            e.0 += v.0;
            e.1 += v.1;
        }
    }
    let total: u64 = all.values().map(|v| v.0).sum();
    println!(
        "ERROR nodes: {total}, bytes inside ERROR: {err_bytes}, MISSING nodes: {missing}, files whose root is ERROR: {}",
        whole_file_errors.into_inner()
    );
    let mut rows: Vec<_> = all.into_iter().collect();
    // Rank by bytes lost rather than by count: a few huge ERROR nodes dominate.
    rows.sort_by_key(|r| std::cmp::Reverse(r.1 .1));
    for ((first, parent), (count, bytes, example)) in rows.into_iter().take(top) {
        println!("{count:7} {bytes:9}B  {first:28} in {parent:24} {example}");
    }
}
