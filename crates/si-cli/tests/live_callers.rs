//! `callers` from `norn live`: a name whose callers run to millions (`printk`
//! in Linux, reached through every macro that logs) answers with at most
//! so many of them, and says how many there were.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn callers_past_the_limit_are_counted_but_not_listed() {
    let root = std::env::temp_dir().join(format!("norn-live-callers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Two copies of the logging header, as a tree with tools of its own has.
    let header = "void emit (const char *s);\n#define log(s) emit (s)\n#define warn(s) log (s)\n";
    std::fs::write(root.join("log.h"), header).unwrap();
    std::fs::write(root.join("log_copy.h"), header).unwrap();
    std::fs::write(root.join("emit.c"), "void emit (const char *s) { (void) s; }\n").unwrap();
    std::fs::write(
        root.join("a.c"),
        "#include \"log.h\"\nvoid a (void) { log (\"a\"); }\nvoid b (void) { warn (\"b\"); }\nvoid c (void) { emit (\"c\"); log (\"c\"); }\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["live", root.to_str().unwrap(), "--no-cache"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("norn live");
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert!(ready.contains("\"ready\":true"), "{ready}");
    let mut ask = |req: Value| -> Value {
        writeln!(input, "{req}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let all = ask(json!({ "op": "callers", "name": "emit" }));
    let count = all["count"].as_u64().unwrap() as usize;
    let rows = all["results"].as_array().unwrap();
    assert!(count > 2, "{all}");
    assert_eq!(rows.len(), count);
    assert!(all.get("truncated").is_none(), "{all}");
    let names: std::collections::BTreeSet<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(["a", "b", "c"].iter().all(|n| names.contains(n)), "{all}");

    let some = ask(json!({ "op": "callers", "name": "emit", "limit": 2 }));
    assert_eq!(some["count"], all["count"]);
    assert_eq!(some["truncated"], true);
    assert_eq!(some["results"].as_array().unwrap()[..], rows[..2]);

    drop(input);
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
