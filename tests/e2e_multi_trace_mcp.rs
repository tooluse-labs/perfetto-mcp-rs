// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_perfetto-mcp-rs");

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    fn start() -> Self {
        let mut child = Command::new(BIN)
            .args([
                "--max-instances",
                "2",
                "--startup-timeout-ms",
                "20000",
                "--query-timeout-ms",
                "30000",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn perfetto-mcp-rs stdio server");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "perfetto-mcp-rs-e2e",
                    "version": "0.0.0"
                }
            }),
        );
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            json!("perfetto-rs"),
            "initialize should return server info: {response}",
        );
        self.notify("notifications/initialized", json!({}));
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> String {
        let response = self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        );
        assert!(
            response.get("error").is_none(),
            "tool call {name} failed at JSON-RPC level: {response}",
        );
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "tool call {name} returned an MCP tool error: {response}",
        );
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool call {name} must return text content: {response}"))
            .to_owned()
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{request}").expect("write JSON-RPC request");
        self.stdin.flush().expect("flush JSON-RPC request");

        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read JSON-RPC response");
        assert!(
            !line.is_empty(),
            "server closed stdout before response to {method}"
        );
        let response: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("response to {method} must be JSON: {e}; line={line:?}"));
        assert_eq!(
            response["id"],
            json!(id),
            "response id mismatch for {method}: {response}",
        );
        response
    }

    fn notify(&mut self, method: &str, params: Value) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{notification}").expect("write JSON-RPC notification");
        self.stdin.flush().expect("flush JSON-RPC notification");
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn e2e_mcp_load_trace_paths_routes_execute_sql_by_trace_id() {
    let mut mcp = McpProcess::start();
    mcp.initialize();

    let load_response = mcp.call_tool(
        "load_trace",
        json!({
            "paths": [
                fixture_path("basic.perfetto-trace"),
                fixture_path("page_loads.pftrace"),
            ],
        }),
    );
    let loaded = loaded_traces_from_batch_response(&load_response);
    let loaded = loaded.as_array().expect("Loaded traces must be an array");
    assert_eq!(
        loaded.len(),
        2,
        "batch load should report two traces: {loaded:?}"
    );

    let basic_trace_id = loaded[0]["trace_id"]
        .as_str()
        .expect("basic trace_id")
        .to_owned();
    let page_load_trace_id = loaded[1]["trace_id"]
        .as_str()
        .expect("page-load trace_id")
        .to_owned();
    assert_ne!(
        basic_trace_id, page_load_trace_id,
        "distinct fixture traces must get distinct trace ids"
    );

    let basic_response = mcp.call_tool(
        "execute_sql",
        json!({
            "trace_id": basic_trace_id,
            "sql": "SELECT COUNT(*) AS c FROM process",
        }),
    );
    assert_eq!(
        first_i64_cell(&basic_response),
        4,
        "execute_sql(trace_id=basic) must route to basic.perfetto-trace"
    );

    let page_load_response = mcp.call_tool(
        "execute_sql",
        json!({
            "trace_id": page_load_trace_id,
            "sql": "INCLUDE PERFETTO MODULE chrome.page_loads; \
                    SELECT COUNT(*) AS c FROM chrome_page_loads",
        }),
    );
    assert!(
        first_i64_cell(&page_load_response) > 0,
        "execute_sql(trace_id=page_load) must route to page_loads.pftrace"
    );
}

fn fixture_path(file_name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(file_name)
        .to_string_lossy()
        .into_owned()
}

fn loaded_traces_from_batch_response(response: &str) -> Value {
    let traces_line = response
        .lines()
        .find_map(|line| line.strip_prefix("Loaded traces: "))
        .expect("batch load response must include Loaded traces JSON");
    serde_json::from_str(traces_line).expect("Loaded traces line must be JSON")
}

fn first_i64_cell(response: &str) -> i64 {
    let parsed: Value = serde_json::from_str(response).expect("execute_sql response must be JSON");
    parsed["rows"][0][0]
        .as_i64()
        .unwrap_or_else(|| panic!("first cell must be i64: {parsed}"))
}
