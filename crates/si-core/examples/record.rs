//! The members si recorded for a struct: `record <root> <relative path> <name>`.
fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().expect("root"));
    let rel = args.next().expect("file");
    let want = args.next().expect("name");
    let files = si_core::scan(&root);
    let sets = si_core::prescan(&files);
    let mut ex = si_core::extract::Extractor::new();
    let f = si_core::extract_file(&mut ex, &root, &root.join(&rel), &sets);
    for r in f.records.iter().filter(|r| f.name(r.name as usize) == want) {
        for (i, &m) in r.fields.iter().enumerate() {
            println!("{i} {}", if m == si_core::model::NONE { "-" } else { f.name(m as usize) });
        }
    }
}
