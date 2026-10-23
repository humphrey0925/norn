//! `norn mcp` as an agent runs it: started, asked what tools there are, and
//! asked of calls a text search would not find.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(root: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_norn"))
            .args(["mcp", root.to_str().unwrap(), "--no-cache"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("norn mcp");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Server { child, input, output }
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.input, "{msg}").unwrap();
        self.input.flush().unwrap();
    }

    /// The answer to request `id`, whole.
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.output.read_line(&mut line).expect("an answer");
        let msg: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
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
    assert!(source.contains("2  /* Sends one byte. */") && source.contains("3  int uart_send(int c) { return c; }"), "{source}");

    let (line, _) = s.call(5, "compiled_lines", json!({ "file": "uart.c", "line": 6 }));
    assert!(line.contains("Line 6 is not compiled"), "{line}");

    let (missing, error) = s.call(6, "find_definition", json!({ "name": "uart_snd" }));
    assert!(error && missing.contains("uart_send"), "{missing}");

    // An edit between calls is seen by the next.
    std::thread::sleep(std::time::Duration::from_millis(600));
    std::fs::write(dir.join("more.c"), "#include \"uart.h\"\nint twice(void) { return uart_send(1) + uart_send(2); }\n").unwrap();
    let (callers, _) = s.call(7, "find_callers", json!({ "name": "uart_send" }));
    assert!(callers.contains("more.c:2  in twice"), "{callers}");

    let unknown = s.ask(8, "tools/call", json!({ "name": "no_such_tool", "arguments": {} }));
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let _ = std::fs::remove_dir_all(&dir);
}
