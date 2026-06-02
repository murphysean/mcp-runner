use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

struct McpClient {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn new() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-runner"))
            .args(["--port", "19999"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start mcp-runner");

        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        let mut client = Self {
            child,
            reader,
            next_id: 1,
        };

        let resp = client.request(
            "initialize",
            json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}),
        );
        assert!(resp["result"]["serverInfo"]["version"].is_string());
        client.notify("notifications/initialized", json!({}));
        client
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_raw(&msg);
        self.read_response(id)
    }

    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_raw(&msg);
    }

    fn send_raw(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", msg).unwrap();
        stdin.flush().unwrap();
    }

    fn read_response(&mut self, expected_id: u64) -> Value {
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            if line.is_empty() {
                panic!("EOF from mcp-runner");
            }
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").and_then(|v| v.as_u64()) == Some(expected_id) {
                return msg;
            }
        }
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": name, "arguments": args}));
        resp["result"].clone()
    }

    fn tool_text(&mut self, name: &str, args: Value) -> String {
        let result = self.call_tool(name, args);
        result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[test]
fn test_full_workflow() {
    let mut c = McpClient::new();

    // --- Start a simple command and read output ---
    let text = c.tool_text(
        "start_command",
        json!({"command": "echo", "args": ["hello world"], "label": "greeter"}),
    );
    assert!(text.contains("session_id: 1"), "start_command: {}", text);

    let output = c.tool_text(
        "read_output",
        json!({"session_id": "1", "wait_for": "hello", "timeout_ms": 5000}),
    );
    assert!(output.contains("hello world"), "read_output: {}", output);

    // --- List sessions shows the completed greeter ---
    let list = c.tool_text("list_sessions", json!({}));
    assert!(list.contains("[greeter]"), "list_sessions: {}", list);
    assert!(list.contains("echo hello world"), "list_sessions cmd: {}", list);

    // --- Working directory and environment variables ---
    c.tool_text(
        "start_command",
        json!({
            "command": "sh",
            "args": ["-c", "pwd && echo $TEST_VAR"],
            "working_dir": "/tmp",
            "env": {"TEST_VAR": "mcp_works"}
        }),
    );
    let output = c.tool_text(
        "read_output",
        json!({"session_id": "2", "wait_for": "mcp_works", "timeout_ms": 5000}),
    );
    assert!(output.contains("/tmp"), "working_dir: {}", output);
    assert!(output.contains("mcp_works"), "env: {}", output);

    // --- Split stderr ---
    c.tool_text(
        "start_command",
        json!({"command": "sh", "args": ["-c", "echo stdout_line; echo stderr_line >&2"], "split_stderr": true}),
    );
    let stdout = c.tool_text(
        "read_output",
        json!({"session_id": "3", "wait_for": "stdout_line", "timeout_ms": 5000}),
    );
    assert!(stdout.contains("stdout_line"), "stdout: {}", stdout);
    assert!(!stdout.contains("stderr_line"), "stderr leaked to stdout: {}", stdout);

    std::thread::sleep(Duration::from_millis(200));
    let stderr = c.tool_text("read_stderr", json!({"session_id": "3"}));
    assert!(stderr.contains("stderr_line"), "stderr: {}", stderr);

    // --- Search output ---
    c.tool_text(
        "start_command",
        json!({"command": "sh", "args": ["-c", "echo line1 OK; echo line2 ERROR fail; echo line3 OK; echo line4 ERROR again"]}),
    );
    c.tool_text(
        "read_output",
        json!({"session_id": "4", "wait_for": "line4", "timeout_ms": 5000}),
    );
    let search = c.tool_text(
        "search_output",
        json!({"session_id": "4", "pattern": "ERROR"}),
    );
    assert!(search.contains("2 match"), "search count: {}", search);
    assert!(search.contains("ERROR fail"), "search content: {}", search);
    assert!(search.contains("ERROR again"), "search content2: {}", search);

    // --- Send input with await_response ---
    c.tool_text(
        "start_command",
        json!({"command": "cat", "label": "echo-cat"}),
    );
    std::thread::sleep(Duration::from_millis(200));
    let response = c.tool_text(
        "send_input",
        json!({"session_id": "5", "input": "ping", "await_response_ms": 2000}),
    );
    assert!(response.contains("ping"), "send_input response: {}", response);

    // --- Send signal (SIGTERM) ---
    c.tool_text(
        "start_command",
        json!({"command": "sleep", "args": ["60"], "label": "sleeper"}),
    );
    std::thread::sleep(Duration::from_millis(100));
    let sig_result = c.tool_text(
        "send_signal",
        json!({"session_id": "6", "signal": "SIGTERM"}),
    );
    assert!(sig_result.contains("SIGTERM sent"), "signal: {}", sig_result);
    std::thread::sleep(Duration::from_millis(100));
    let status = c.tool_text("get_status", json!({"session_id": "6"}));
    assert!(status.contains("Running: false"), "after signal: {}", status);

    // --- Stop command ---
    c.tool_text(
        "start_command",
        json!({"command": "sleep", "args": ["60"]}),
    );
    std::thread::sleep(Duration::from_millis(100));
    let stop = c.tool_text("stop_command", json!({"session_id": "7"}));
    assert!(stop.contains("stopped"), "stop: {}", stop);
    let status = c.tool_text("get_status", json!({"session_id": "7"}));
    assert!(status.contains("Running: false"), "after stop: {}", status);

    // --- Delete session ---
    let del = c.tool_text("delete_session", json!({"session_id": "7"}));
    assert!(del.contains("deleted"), "delete: {}", del);

    // --- wait_for with delayed output ---
    c.tool_text(
        "start_command",
        json!({"command": "sh", "args": ["-c", "sleep 0.5 && echo BUILD_DONE"]}),
    );
    let wait_output = c.tool_text(
        "read_output",
        json!({"session_id": "8", "wait_for": "BUILD_DONE", "timeout_ms": 10000}),
    );
    assert!(wait_output.contains("BUILD_DONE"), "wait_for: {}", wait_output);

    // --- Timeout seconds (auto-kill) ---
    c.tool_text(
        "start_command",
        json!({"command": "sleep", "args": ["60"], "timeout_seconds": 1}),
    );
    std::thread::sleep(Duration::from_secs(3));
    let status = c.tool_text("get_status", json!({"session_id": "9"}));
    assert!(status.contains("Running: false"), "timeout kill: {}", status);

    // --- run_command (synchronous) ---
    let run_output = c.tool_text(
        "run_command",
        json!({"command": "sh", "args": ["-c", "echo built ok; echo warn >&2"], "working_dir": "/tmp"}),
    );
    assert!(run_output.contains("built ok"), "run_command stdout: {}", run_output);
    assert!(run_output.contains("[stderr]"), "run_command stderr label: {}", run_output);
    assert!(run_output.contains("warn"), "run_command stderr content: {}", run_output);
    assert!(run_output.contains("[exit code: 0]"), "run_command exit: {}", run_output);

    // --- send_input with wait:true (simplified API) ---
    c.tool_text(
        "start_command",
        json!({"command": "cat", "label": "wait-test"}),
    );
    std::thread::sleep(Duration::from_millis(200));
    let wait_resp = c.tool_text(
        "send_input",
        json!({"session_id": "10", "input": "test_wait_true", "wait": true}),
    );
    assert!(wait_resp.contains("test_wait_true"), "wait:true response: {}", wait_resp);

    // --- Final session count ---
    let list = c.tool_text("list_sessions", json!({}));
    assert!(list.contains("Sessions:"), "final list: {}", list);
    // Session 7 was deleted, so it shouldn't appear
    assert!(!list.contains("session_id: 7"), "deleted session still in list");
}
