//! `norn mcp` as an agent runs it: started, asked what tools there are, and
//! asked of calls a text search would not find; several of them, and the
//! editor's language server, on one project; and started with no project.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    /// What it wrote to its error output, line by line.
    said: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(root: &Path) -> Server {
        Server::spawn(Some(root), root, &root.join(".run"), None)
    }

    /// `norn mcp [root]` in `cwd`, keeping its locks in `run` (not the
    /// user's own), with `home` as the home folder if given.
    fn spawn(root: Option<&Path>, cwd: &Path, run: &Path, home: Option<&Path>) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
        command.arg("mcp").args(root).arg("--no-cache").current_dir(cwd).env("NORN_RUN_DIR", run);
        if let Some(home) = home {
            command.env("HOME", home).env("USERPROFILE", home);
        }
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("norn mcp");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let said = Arc::new(Mutex::new(Vec::new()));
        let (into, errors) = (said.clone(), child.stderr.take().unwrap());
        std::thread::spawn(move || {
            for line in BufReader::new(errors).lines().map_while(Result::ok) {
                into.lock().unwrap().push(line);
            }
        });
        Server { child, input, output, said }
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.output.read_line(&mut line).expect("an answer");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    /// Whether it says, within a while, that it is answered from the index
    /// process `pid` keeps.
    fn answered_by(&self, pid: u32) -> bool {
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if self.said.lock().unwrap().iter().any(|l| l.contains(&format!("from the index process {pid} keeps"))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.input, "{msg}").unwrap();
        self.input.flush().unwrap();
    }

    /// The answer to request `id`, whole.
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let msg = self.read();
        assert_eq!(msg["id"], id, "{msg}");
        msg
    }

    /// What a tool says, and whether it says it as an error.
    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> (String, bool) {
        let result = self.ask(id, "tools/call", json!({ "name": tool, "arguments": arguments }))["result"].clone();
        (result["content"][0]["text"].as_str().unwrap().to_string(), result["isError"] == true)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn an_agent_finds_calls_through_macros_and_members() {
    let dir = std::env::temp_dir().join(format!("norn-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("uart.h"), "struct uart_ops { int (*send)(int c); };\nint uart_send(int c);\n#define SEND(c) uart_send(c)\n").unwrap();
    std::fs::write(
        dir.join("uart.c"),
        "#include \"uart.h\"\n\
         /* Sends one byte. */\n\
         int uart_send(int c) { return c; }\n\
         static const struct uart_ops ops = { .send = uart_send };\n\
         #if 0\n\
         void dead(void) { uart_send(9); }\n\
         #endif\n\
         int via_ptr(const struct uart_ops *o) { return o->send(1); }\n\
         int main(void) { SEND(2); return via_ptr(&ops); }\n",
    )
    .unwrap();

    let mut s = Server::start(&dir);
    let init = s.ask(1, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
    assert!(init["result"]["capabilities"]["tools"].is_object(), "{init}");
    s.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let tools = s.ask(2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for tool in ["search_symbols", "find_definition", "read_symbol", "find_callers", "find_callees", "find_references", "call_tree", "compiled_lines"] {
        assert!(names.contains(&tool), "{tool} not in {names:?}");
    }

    let (callers, error) = s.call(3, "find_callers", json!({ "name": "uart_send" }));
    assert!(!error, "{callers}");
    assert!(callers.contains("uart.c:9  in main — via macro `SEND`"), "{callers}");
    assert!(callers.contains("uart.c:8  in via_ptr — through member `send`"), "{callers}");
    // Not compiled: no call.
    assert!(!callers.contains("dead"), "{callers}");

    let (source, _) = s.call(4, "read_symbol", json!({ "name": "uart_send", "file": "uart.c" }));
    assert!(source.contains("     2  /* Sends one byte. */") && source.contains("     3  int uart_send(int c) { return c; }"), "{source}");

    let (line, _) = s.call(5, "compiled_lines", json!({ "file": "uart.c", "line": 6 }));
    assert!(line.contains("Line 6 is not compiled"), "{line}");

    let (missing, error) = s.call(6, "find_definition", json!({ "name": "uart_snd" }));
    assert!(error && missing.contains("uart_send"), "{missing}");

    // A file added between calls is seen by the next (the watcher given a
    // moment to tell, as an agent's next turn gives it).
    std::fs::write(dir.join("more.c"), "#include \"uart.h\"\nint twice(void) { return uart_send(1) + uart_send(2); }\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (callers, _) = s.call(7, "find_callers", json!({ "name": "uart_send" }));
    assert!(callers.contains("more.c:2  in twice"), "{callers}");

    let unknown = s.ask(8, "tools/call", json!({ "name": "no_such_tool", "arguments": {} }));
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A small project in a folder of its own.
fn project(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("norn-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.c"), "int helper(void) { return 1; }\nint main(void) { return helper(); }\n").unwrap();
    dir
}

fn init(s: &mut Server) {
    s.ask(0, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {} }));
    s.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
}

#[test]
fn agents_of_one_project_share_one_index() {
    let dir = project("share");
    let run = dir.with_extension("run");
    let mut a = Server::spawn(Some(&dir), &dir, &run, None);
    init(&mut a);
    assert!(a.call(1, "find_callers", json!({ "name": "helper" })).0.contains("a.c:2  in main"));

    // The second is answered from the first's index.
    let mut b = Server::spawn(Some(&dir), &dir, &run, None);
    init(&mut b);
    assert!(b.call(1, "find_callers", json!({ "name": "helper" })).0.contains("a.c:2  in main"));
    assert!(b.answered_by(a.child.id()), "{:?}", b.said.lock().unwrap());
    // What changes is seen there.
    std::fs::write(dir.join("b.c"), "int helper(void);\nint twice(void) { return helper() + helper(); }\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.call(2, "find_callers", json!({ "name": "helper" })).0.contains("b.c:2  in twice"));

    // The first gone, the second makes the index and answers.
    let _ = a.child.kill();
    let _ = a.child.wait();
    let (callers, error) = b.call(3, "find_callers", json!({ "name": "helper" }));
    assert!(!error && callers.contains("b.c:2  in twice"), "{callers}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&run);
}

#[test]
fn the_editors_language_server_takes_the_project_from_an_agent() {
    let dir = project("editor");
    let run = dir.with_extension("run");
    let mut agent = Server::spawn(Some(&dir), &dir, &run, None);
    init(&mut agent);
    assert!(agent.call(1, "find_callers", json!({ "name": "helper" })).0.contains("a.c:2  in main"));

    // The editor opens the folder: its server holds the project from then,
    // and the agent is answered from its index.
    let mut lsp = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["lsp", dir.to_str().unwrap(), "--no-cache"])
        .env("NORN_RUN_DIR", &run)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("norn lsp");
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "rootUri": null, "capabilities": {} } }).to_string();
    let mut input = lsp.stdin.take().unwrap();
    write!(input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    input.flush().unwrap();
    let mut output = lsp.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut sink = [0u8; 4096];
        while output.read(&mut sink).is_ok_and(|n| n > 0) {}
    });
    assert!(agent.answered_by(lsp.id()), "{:?}", agent.said.lock().unwrap());
    let (callers, error) = agent.call(2, "find_callers", json!({ "name": "helper" }));
    assert!(!error && callers.contains("a.c:2  in main"), "{callers}");
    let _ = lsp.kill();
    let _ = lsp.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&run);
}

#[test]
fn started_in_the_home_folder_it_indexes_only_a_project_it_is_told_of() {
    let dir = project("home");
    let home = dir.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    let run = dir.with_extension("run");

    // As a desktop app starts it: no folder, in the home folder.
    let mut s = Server::spawn(None, &home, &run, Some(&home));
    init(&mut s);
    let (said, error) = s.call(1, "find_callers", json!({ "name": "helper" }));
    assert!(error && said.contains("home folder"), "{said}");
    drop(s);

    // A client that says which folder it has open (MCP roots) is asked.
    let mut s = Server::spawn(None, &home, &run, Some(&home));
    s.ask(0, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": { "roots": {} } }));
    s.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let asked = s.read();
    assert_eq!(asked["method"], "roots/list", "{asked}");
    let uri = format!("file:///{}", dir.to_string_lossy().replace('\\', "/").trim_start_matches('/'));
    s.send(json!({ "jsonrpc": "2.0", "id": asked["id"], "result": { "roots": [{ "uri": uri, "name": "project" }] } }));
    let (callers, error) = s.call(1, "find_callers", json!({ "name": "helper" }));
    assert!(!error && callers.contains("a.c:2  in main"), "{callers}");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&run);
}

#[test]
fn an_agent_sees_the_configurations_and_switches_to_the_one_its_task_is_about() {
    let dir = std::env::temp_dir().join(format!("norn-mcp-configurations-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".norn")).unwrap();
    std::fs::write(
        dir.join("a.c"),
        "int uart_a(void) { return 1; }\nint uart_b(void) { return 2; }\nint main(void)\n{\n#ifdef BOARD_A\n    return uart_a();\n#else\n    return uart_b();\n#endif\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".norn").join("config.json"),
        r#"{"include": [], "exclude": [], "configurations": {"BoardA": {"defines": {"BOARD_A": ""}}, "Generic": {}}, "activeConfiguration": "Generic"}"#,
    )
    .unwrap();
    let mut s = Server::start(&dir);
    init(&mut s);

    let (listed, _) = s.call(1, "list_configurations", json!({}));
    assert!(listed.contains("In force: Generic.") && listed.contains("BoardA — defines BOARD_A"), "{listed}");
    // Nothing settles BOARD_A: either call may be the one compiled, and says so.
    let (calls, _) = s.call(2, "find_callees", json!({ "name": "main" }));
    assert!(calls.contains("uart_a") && calls.contains("uart_b") && calls.matches("[maybe not compiled: under the #if at line").count() == 2, "{calls}");

    let (switched, error) = s.call(3, "switch_configuration", json!({ "name": "BoardA" }));
    assert!(!error && switched.contains("In force now: BoardA"), "{switched}");
    let (calls, _) = s.call(4, "find_callees", json!({ "name": "main" }));
    assert!(calls.contains("a.c:6  uart_a") && !calls.contains("uart_b") && !calls.contains("maybe"), "{calls}");
    let saved = std::fs::read_to_string(dir.join(".norn").join("config.json")).unwrap();
    assert!(saved.contains("\"activeConfiguration\": \"BoardA\""), "{saved}");

    // The other board's line is there, where it is said not to be compiled;
    // grep-like search leaves it out.
    let (read, _) = s.call(6, "read_file", json!({ "file": "a.c" }));
    assert!(
        read.contains("     7  #else\n        >>> not compiled under BoardA (line 8)\n     8      return uart_b();\n        <<< end of not compiled (line 8)\n     9  #endif"),
        "{read}"
    );
    let (folded, _) = s.call(7, "read_file", json!({ "file": "a.c", "hide_inactive": true }));
    assert!(folded.contains("     7  #else\n        --- line 8 not compiled under BoardA (hidden) ---\n     9  #endif") && !folded.contains("return uart_b"), "{folded}");
    let (found, _) = s.call(8, "search_code", json!({ "pattern": "uart_b" }));
    assert!(found.contains("a.c:2 ") && !found.contains("a.c:8") && found.contains("1 more where it does not"), "{found}");
    let (all, _) = s.call(9, "search_code", json!({ "pattern": "return uart_[ab]", "regex": true, "include_inactive": true }));
    assert!(all.contains("a.c:6   return uart_a();  [in main]") && all.contains("a.c:8 ✗ return uart_b();  [in main]"), "{all}");

    let (missing, error) = s.call(5, "switch_configuration", json!({ "name": "BoardZ" }));
    assert!(error && missing.contains("No configuration named `BoardZ`") && missing.contains("Generic"), "{missing}");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(dir.join(".run"));
}
