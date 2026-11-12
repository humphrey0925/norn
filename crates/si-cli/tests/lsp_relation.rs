//! `si/relation` as an editor asks it: from where a function is defined,
//! and with the callers the build's configuration leaves out.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(root: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_norn"))
            .args(["lsp", root.to_str().unwrap(), "--no-cache"])
            // (Not the user's own locks.)
            .env("NORN_RUN_DIR", root.join(".run"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("norn lsp");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Server { child, input, output }
    }

    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        write!(self.input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.input.flush().unwrap();
    }

    /// The answer to request `id`; None if the server has gone.
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Option<Value> {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut length = 0;
            loop {
                let mut line = String::new();
                if self.output.read_line(&mut line).ok()? == 0 {
                    return None;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some(n) = line.strip_prefix("Content-Length:") {
                    length = n.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            self.output.read_exact(&mut body).ok()?;
            let msg: Value = serde_json::from_slice(&body).unwrap();
            if msg["id"] == id && msg.get("method").is_none() {
                return Some(msg["result"].clone());
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn uri(path: &std::path::Path) -> String {
    format!("file:///{}", path.to_string_lossy().replace('\\', "/").trim_start_matches('/'))
}

fn ready(s: &mut Server) {
    s.ask(0, "initialize", json!({ "rootUri": null, "capabilities": {} })).expect("initialize");
    s.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let status = s.ask(1, "si/status", json!({})).expect("the server answers while indexing");
        if status["loading"] == false && status["symbols"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(Instant::now() < end, "the index is not ready after a minute");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn names(rows: &Value) -> Vec<String> {
    rows.as_array().into_iter().flatten().map(|r| r["name"].as_str().unwrap_or("").to_string()).collect()
}

#[test]
fn a_static_function_s_callees_are_its_own_and_not_another_file_s() {
    let dir = std::env::temp_dir().join(format!("norn-lsp-static-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/s1.c"), "void target(void);\nstatic void helper(void) { target(); }\nvoid s1_entry(void) { helper(); }\n").unwrap();
    std::fs::write(dir.join("src/s2.c"), "void leaf(void);\nstatic void helper(void) { leaf(); }\nvoid s2_entry(void) { helper(); }\n").unwrap();
    let mut s = Server::start(&dir);
    ready(&mut s);
    for (k, (file, callee)) in [("src/s1.c", "target"), ("src/s2.c", "leaf")].into_iter().enumerate() {
        let at = json!({ "textDocument": { "uri": uri(&dir.join(file)) }, "position": { "line": 1, "character": 12 }, "direction": "callees" });
        let answer = s.ask(10 + k as u64, "si/relation", at).expect("an answer");
        assert_eq!(answer["root"]["file"], file, "{answer}");
        assert_eq!(names(&answer["entries"]), [callee], "{answer}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn callers_the_configuration_leaves_out_are_listed_when_asked_for() {
    let dir = std::env::temp_dir().join(format!("norn-lsp-excluded-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join(".vscode")).unwrap();
    std::fs::write(dir.join("src/a.c"), "int g_flag;\nstatic void leaf(void) { g_flag = 1; }\nvoid mid(void) { leaf(); }\n#ifdef FEATURE_X\nvoid only_x(void) { mid(); }\n#endif\nvoid top(void) {\n    mid();\n}\n").unwrap();
    std::fs::write(dir.join("src/b.c"), "void mid(void);\nvoid top(void);\nvoid other(void) { top(); mid(); }\n").unwrap();
    // The C/C++ extension's configuration: all the build defines.
    std::fs::write(dir.join(".vscode/c_cpp_properties.json"), r#"{"configurations": [{"name": "Board", "defines": ["DEBUG=1"]}], "version": 4}"#).unwrap();
    let mut s = Server::start(&dir);
    ready(&mut s);
    let at = json!({ "textDocument": { "uri": uri(&dir.join("src/a.c")) }, "position": { "line": 2, "character": 5 }, "direction": "callers" });
    // As before: what the configuration compiles, and nothing more.
    let answer = s.ask(10, "si/relation", at.clone()).expect("an answer");
    assert_eq!(names(&answer["entries"]), ["top", "other"], "{answer}");
    assert!(answer.get("excluded").is_none(), "{answer}");
    // Asked for, the callers it leaves out, and the #if that does.
    let mut asked = at;
    asked["excluded"] = json!(true);
    let answer = s.ask(11, "si/relation", asked).expect("an answer");
    assert_eq!(names(&answer["entries"]), ["top", "other"], "{answer}");
    assert_eq!(names(&answer["excluded"]), ["only_x"], "{answer}");
    let row = &answer["excluded"][0];
    assert_eq!((row["file"].as_str(), row["line"].as_u64(), row["lines"].clone()), (Some("src/a.c"), Some(5), json!([5])), "{answer}");
    assert_eq!(row["excludedBy"], json!([{ "line": 4, "text": "#ifdef FEATURE_X" }]), "{answer}");
    let _ = std::fs::remove_dir_all(&dir);
}
