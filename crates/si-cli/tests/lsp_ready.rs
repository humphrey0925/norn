//! `norn lsp` as an editor runs it: asked about a name while the index is
//! still being made, and again once it is ready.

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

#[test]
fn a_request_before_the_index_is_ready_does_not_break_one_after() {
    let dir = std::env::temp_dir().join(format!("norn-lsp-ready-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.h"), "/* Adds one. */\nint add_one(int x);\n").unwrap();
    std::fs::write(dir.join("a.c"), "#include \"a.h\"\nint add_one(int x) { return x + 1; }\nint main(void) { return add_one(1); }\n").unwrap();
    let uri = format!("file:///{}", dir.join("a.c").to_string_lossy().replace('\\', "/").trim_start_matches('/'));
    let at = json!({ "textDocument": { "uri": uri }, "position": { "line": 2, "character": 26 } });

    let mut s = Server::start(&dir);
    s.ask(0, "initialize", json!({ "rootUri": null, "capabilities": {} })).expect("initialize");
    s.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    // Asked at once: the index may not be there yet.
    s.ask(1, "textDocument/hover", at.clone()).expect("the server answers while indexing");
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let status = s.ask(2, "si/status", json!({})).expect("the server answers while indexing");
        if status["loading"] == false && status["symbols"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(Instant::now() < end, "the index is not ready after a minute");
        std::thread::sleep(Duration::from_millis(50));
    }
    let hover = s.ask(3, "textDocument/hover", at).expect("the server is still running once the index is ready");
    assert!(hover.to_string().contains("add_one"), "{hover}");
    let _ = std::fs::remove_dir_all(&dir);
}
