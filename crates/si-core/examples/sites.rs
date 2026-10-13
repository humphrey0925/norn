//! Which expanding macros are used most: `sites <root> [top]`.
use std::collections::HashMap;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().expect("root"));
    let top: usize = args.next().map_or(40, |s| s.parse().expect("top"));
    let files = si_core::scan(&root);
    let sets = si_core::prescan(&files);
    let facts = si_core::extract_all(&root, &files, &sets);
    let mut count: HashMap<&str, usize> = HashMap::new();
    for f in &facts {
        for s in &f.sites {
            *count.entry(f.name(s.name as usize)).or_default() += 1;
        }
    }
    let mut v: Vec<_> = count.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    println!("{} expanding macros, {} used, {} kept bodies", sets.expanding_len(), v.len(), facts.iter().map(|f| f.macros.len()).sum::<usize>());
    for (name, n) in v.into_iter().take(top) {
        println!("{n:8} {name}");
    }
}
