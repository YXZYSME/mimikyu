mod support;

use std::collections::HashSet;
use std::sync::Arc;

use mimikyu_core::mcp::serve_rw;
use mimikyu_core::session::SessionManager;
use serde_json::{json, Value};
use support::{assert_pgid_gone, TestServer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const EXPECTED_MIMIKYU_TOOLS: &[&str] = &[
    "mimikyu_session_new",
    "mimikyu_session_close",
    "mimikyu_open",
    "mimikyu_snapshot",
    "mimikyu_click",
    "mimikyu_fill",
    "mimikyu_extract",
    "mimikyu_wait",
    "mimikyu_batch",
    "mimikyu_eval",
    "mimikyu_screenshot",
];

const EXPECTED_BROWSER_TOOLS: &[&str] = &[
    "browser_open",
    "browser_close",
    "browser_snapshot",
    "browser_click",
    "browser_fill",
    "browser_type",
    "browser_press",
    "browser_select",
    "browser_upload",
    "browser_scroll",
    "browser_eval",
    "browser_extract",
    "browser_screenshot",
    "browser_wait",
    "browser_batch",
    "browser_context_list",
    "browser_context_close",
    "browser_profile_export",
    "browser_profile_import",
    "browser_env_report",
];

fn r3_forbidden_keywords() -> Vec<&'static str> {
    vec![
        concat!("disable_", "coop"),
        concat!("cap", "tcha"),
        concat!("recap", "tcha"),
        concat!("hcap", "tcha"),
        concat!("turn", "stile"),
        concat!("sun", "dial"),
        concat!("cf_", "clearance"),
        concat!("waf_", "bypass"),
        concat!("waf_", "evasion"),
        concat!("waf ", "evasion"),
        concat!("gate_", "bypass"),
        concat!("gate ", "bypass"),
        concat!("challenge_", "bypass"),
        concat!("challenge_", "solver"),
        concat!("anti", "bot"),
        concat!("anti-", "bot"),
        concat!("human_", "verification"),
        concat!("human-", "verification"),
    ]
}

async fn send_line(w: &mut tokio::io::DuplexStream, val: &Value) {
    let mut s = serde_json::to_string(val).unwrap();
    s.push('\n');
    w.write_all(s.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

async fn read_json_line(r: &mut BufReader<tokio::io::DuplexStream>) -> Value {
    let mut line = String::new();
    let n = r.read_line(&mut line).await.unwrap();
    assert!(n > 0, "unexpected EOF reading MCP response");
    assert!(
        !line.starts_with("Content-Length:"),
        "unexpected LSP header in MCP output: {line}"
    );
    serde_json::from_str(line.trim()).expect("valid JSON-RPC line")
}

fn parse_tool_json(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text content");
    serde_json::from_str(text).expect("tool JSON payload")
}

#[tokio::test]
async fn mcp_initialize_notifications_and_tools_list_31_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (client_in, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(64 * 1024);

    let srv_mgr = Arc::clone(&mgr);
    let server_task = tokio::spawn(async move {
        serve_rw(srv_mgr, BufReader::new(server_in), server_out).await
    });

    let mut w = client_in;
    let mut r = BufReader::new(client_out);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    )
    .await;
    let init_resp = read_json_line(&mut r).await;
    assert_eq!(init_resp["id"], 1);
    assert!(init_resp["result"]["protocolVersion"].is_string());
    assert!(init_resp["result"]["capabilities"].is_object());
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "mimikyu");

    // Notification must emit zero bytes
    send_line(
        &mut w,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send_line(&mut w, &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })).await;
    let ping_resp = read_json_line(&mut r).await;
    assert_eq!(ping_resp["id"], 2);
    assert!(ping_resp["result"].is_object());

    send_line(
        &mut w,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    let list_resp = read_json_line(&mut r).await;
    assert_eq!(list_resp["id"], 3);
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 31, "expected 11 mimikyu_* + 20 browser_* tools");

    let names: HashSet<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in EXPECTED_MIMIKYU_TOOLS.iter().chain(EXPECTED_BROWSER_TOOLS.iter()) {
        assert!(names.contains(expected), "missing MCP tool: {expected}");
    }
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(tool["inputSchema"]["properties"].is_object());
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
    }

    let lowered = serde_json::to_string(tools).unwrap().to_ascii_lowercase();
    for kw in r3_forbidden_keywords() {
        assert!(!lowered.contains(kw), "R3 forbidden keyword '{kw}' found in tools/list");
    }

    drop(w);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn mcp_malformed_jsonrpc_error_codes_and_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (mut w, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(64 * 1024);

    let server_task = tokio::spawn(async move {
        serve_rw(mgr, BufReader::new(server_in), server_out).await
    });

    let mut r = BufReader::new(client_out);

    let payload = concat!(
        "{not valid json\n",
        "\n",
        "[]\n",
        "{\"jsonrpc\":\"1.0\",\"id\":1,\"method\":\"initialize\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"nonexistent/method\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"mimikyu_unknown\",\"arguments\":{}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"mimikyu_open\",\"arguments\":{}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"0.1\"}}}\n"
    );
    w.write_all(payload.as_bytes()).await.unwrap();
    w.flush().await.unwrap();

    let r_parse = read_json_line(&mut r).await;
    assert_eq!(r_parse["error"]["code"], -32700);

    let r_arr = read_json_line(&mut r).await;
    assert_eq!(r_arr["error"]["code"], -32600);

    let r_ver = read_json_line(&mut r).await;
    assert_eq!(r_ver["id"], 1);
    assert_eq!(r_ver["error"]["code"], -32600);

    let r_meth = read_json_line(&mut r).await;
    assert_eq!(r_meth["id"], 2);
    assert_eq!(r_meth["error"]["code"], -32601);

    let r_unk_tool = read_json_line(&mut r).await;
    assert_eq!(r_unk_tool["id"], 3);
    assert_eq!(r_unk_tool["error"]["code"], -32602);

    let r_bad_args = read_json_line(&mut r).await;
    assert_eq!(r_bad_args["id"], 4);
    assert_eq!(r_bad_args["error"]["code"], -32602);

    let r_init = read_json_line(&mut r).await;
    assert_eq!(r_init["id"], 99);
    assert!(r_init["result"]["capabilities"].is_object());

    drop(w);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn mcp_mimikyu_tools_end_to_end_with_default_session_and_batch_op_normalization() {
    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (mut w, server_in) = tokio::io::duplex(128 * 1024);
    let (server_out, client_out) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        serve_rw(mgr, BufReader::new(server_in), server_out).await
    });
    let mut r = BufReader::new(client_out);

    // 1. mimikyu_session_new without session -> defaults to "default" with main_world_eval=true
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "mimikyu_session_new", "arguments": { "ephemeral": true } }
        }),
    )
    .await;
    let res_new = read_json_line(&mut r).await;
    assert_eq!(res_new["result"]["isError"], false);
    let new_data = parse_tool_json(&res_new);
    assert_eq!(new_data["id"], "default");
    assert_eq!(new_data["session_id"], "default");

    // 2. mimikyu_open without session -> uses "default"
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "mimikyu_open", "arguments": { "url": srv.url("/") } }
        }),
    )
    .await;
    let res_open = read_json_line(&mut r).await;
    assert_eq!(res_open["result"]["isError"], false);

    // 3. mimikyu_snapshot, fill, click, wait, extract
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "mimikyu_snapshot", "arguments": {} }
        }),
    )
    .await;
    let res_snap = read_json_line(&mut r).await;
    assert_eq!(res_snap["result"]["isError"], false);
    let snap_data = parse_tool_json(&res_snap);
    assert!(snap_data["ref_count"].as_u64().unwrap_or(0) > 0);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "mimikyu_fill", "arguments": { "target": "#inp", "value": "mcp-val" } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "mimikyu_click", "arguments": { "target": "#btn" } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "mimikyu_wait", "arguments": { "text": "clicked:1" } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "mimikyu_extract", "arguments": { "mode": "text" } }
        }),
    )
    .await;
    let res_ext = read_json_line(&mut r).await;
    assert_eq!(res_ext["result"]["isError"], false);
    assert!(parse_tool_json(&res_ext)["text"]
        .as_str()
        .unwrap_or("")
        .contains("clicked:1"));

    // 4. mimikyu_eval isolated vs main_world
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": { "name": "mimikyu_eval", "arguments": { "js": "window.__mainSecret", "main_world": true } }
        }),
    )
    .await;
    let res_eval = read_json_line(&mut r).await;
    assert_eq!(res_eval["result"]["isError"], false);
    assert_eq!(parse_tool_json(&res_eval)["value"], "main-only-value");

    // 5. mimikyu_batch with "op" + mid-step failure -> isError: true with recovery_snapshot
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "mimikyu_batch",
                "arguments": {
                    "ops": [
                        { "op": "fill", "target": "#inp", "value": "step1" },
                        { "op": "click", "target": "#missing-xyz" }
                    ]
                }
            }
        }),
    )
    .await;
    let res_batch = read_json_line(&mut r).await;
    assert_eq!(res_batch["result"]["isError"], true);
    let batch_data = parse_tool_json(&res_batch);
    assert_eq!(batch_data["ok"], false);
    assert_eq!(batch_data["completed_steps"], 1);
    assert!(batch_data["error"]["recovery_snapshot"].is_object());

    // 6. mimikyu_screenshot both to file and inline base64
    let shot_path = tmp.path().join("mcp-shot.png");
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "mimikyu_screenshot", "arguments": { "out_path": shot_path } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);
    let png_bytes = std::fs::read(&shot_path).unwrap();
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "mimikyu_screenshot", "arguments": {} }
        }),
    )
    .await;
    let res_shot_inline = read_json_line(&mut r).await;
    assert_eq!(res_shot_inline["result"]["isError"], false);
    assert_eq!(res_shot_inline["result"]["content"][0]["type"], "image");
    assert!(!res_shot_inline["result"]["content"][0]["data"]
        .as_str()
        .unwrap_or("")
        .is_empty());

    // 7. Close session
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": { "name": "mimikyu_session_close", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    drop(w);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn mcp_browser_tools_end_to_end_context_profile_and_env_report() {
    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (mut w, server_in) = tokio::io::duplex(128 * 1024);
    let (server_out, client_out) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        serve_rw(mgr, BufReader::new(server_in), server_out).await
    });
    let mut r = BufReader::new(client_out);

    // browser_open auto-creates virtual context "b_ctx" and navigates
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "browser_open",
                "arguments": {
                    "session": "b_ctx",
                    "virtual": true,
                    "preset": "win11",
                    "locale": "de-DE",
                    "timezone": "Europe/Berlin",
                    "url": srv.url("/forms-rich")
                }
            }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    // browser_type, browser_press, browser_select, browser_upload, browser_scroll
    let upload_file = tmp.path().join("mcp-upload.txt");
    std::fs::write(&upload_file, b"hello-upload").unwrap();

    for (id, name, args) in [
        (
            2,
            "browser_type",
            json!({"session": "b_ctx", "target": "#notes", "text": "hello"}),
        ),
        (3, "browser_press", json!({"session": "b_ctx", "key": "Tab"})),
        (
            4,
            "browser_select",
            json!({"session": "b_ctx", "target": "#country", "value": "de"}),
        ),
        (
            5,
            "browser_upload",
            json!({"session": "b_ctx", "target": "#upload", "path": upload_file}),
        ),
        (
            6,
            "browser_scroll",
            json!({"session": "b_ctx", "dx": 0, "dy": 200}),
        ),
    ] {
        send_line(
            &mut w,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": args }
            }),
        )
        .await;
        let resp = read_json_line(&mut r).await;
        assert_eq!(resp["result"]["isError"], false, "tool {name} failed: {resp}");
    }

    // browser_env_report
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "browser_env_report", "arguments": { "session": "b_ctx" } }
        }),
    )
    .await;
    let env_resp = read_json_line(&mut r).await;
    assert_eq!(env_resp["result"]["isError"], false);
    let env_data = parse_tool_json(&env_resp);
    assert_eq!(env_data["configured"], true);
    assert_eq!(env_data["coherence"]["valid"], true);

    // browser_context_list
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": { "name": "browser_context_list", "arguments": {} }
        }),
    )
    .await;
    let list_resp = read_json_line(&mut r).await;
    assert_eq!(parse_tool_json(&list_resp).as_array().unwrap().len(), 1);

    // browser_profile_export + browser_context_close + browser_open + browser_profile_import + browser_close
    let state_file = tmp.path().join("b_ctx_state.json");
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": { "name": "browser_profile_export", "arguments": { "session": "b_ctx", "out_path": state_file } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "browser_context_close", "arguments": { "context": "b_ctx" } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "browser_open", "arguments": { "session": "b_ctx2", "virtual": true, "url": srv.url("/") } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": { "name": "browser_profile_import", "arguments": { "session": "b_ctx2", "in_path": state_file } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": { "name": "browser_close", "arguments": { "session": "b_ctx2" } }
        }),
    )
    .await;
    assert_eq!(read_json_line(&mut r).await["result"]["isError"], false);

    drop(w);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn mcp_stdin_eof_shuts_down_all_sessions_and_reaps_children() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (mut w, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(64 * 1024);

    let srv_mgr = Arc::clone(&mgr);
    let server_task = tokio::spawn(async move {
        serve_rw(srv_mgr, BufReader::new(server_in), server_out).await
    });
    let mut r = BufReader::new(client_out);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "mimikyu_session_new", "arguments": { "ephemeral": true } }
        }),
    )
    .await;
    let resp = read_json_line(&mut r).await;
    assert_eq!(resp["result"]["isError"], false);
    let pid = parse_tool_json(&resp)["pid"].as_i64().expect("ephemeral pid") as libc::pid_t;

    // Close stdin without calling session_close
    drop(w);
    server_task.await.unwrap().unwrap();

    assert!(mgr.list_sessions().is_empty());
    assert_pgid_gone(pid);
}

#[tokio::test]
async fn mcp_invalid_utf8_and_oversized_line_return_jsonrpc_error_and_recover() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (mut w, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(64 * 1024);

    let srv_mgr = Arc::clone(&mgr);
    let server_task = tokio::spawn(async move {
        serve_rw(srv_mgr, BufReader::new(server_in), server_out).await
    });
    let mut r = BufReader::new(client_out);

    // 1. Raw invalid UTF-8 bytes
    w.write_all(b"\xff\xfe\n").await.unwrap();
    w.flush().await.unwrap();
    let err_utf8 = read_json_line(&mut r).await;
    assert_eq!(err_utf8["error"]["code"], -32700);

    // 2. Oversized (>16 MiB) line written concurrently while reading response
    let writer_task = tokio::spawn(async move {
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..17 {
            w.write_all(&chunk).await.unwrap();
        }
        w.write_all(b"\n").await.unwrap();
        w.flush().await.unwrap();
        w
    });
    let err_big = read_json_line(&mut r).await;
    assert_eq!(err_big["error"]["code"], -32600);
    let mut w = writer_task.await.unwrap();

    // 3. Valid ping still works cleanly after both errors
    send_line(
        &mut w,
        &json!({ "jsonrpc": "2.0", "id": 99, "method": "ping" }),
    )
    .await;
    let ok_ping = read_json_line(&mut r).await;
    assert_eq!(ok_ping["id"], 99);
    assert!(ok_ping["result"].is_object());

    drop(w);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn mcp_browser_open_validates_url_before_session_creation_and_skips_recreate() {
    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).unwrap());

    let (mut w, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(64 * 1024);

    let srv_mgr = Arc::clone(&mgr);
    let server_task = tokio::spawn(async move {
        serve_rw(srv_mgr, BufReader::new(server_in), server_out).await
    });
    let mut r = BufReader::new(client_out);

    // 1. Invalid URL must fail with -32602 WITHOUT creating session "leaktest"
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "browser_open",
                "arguments": { "session": "leaktest", "url": "file:///etc/passwd" }
            }
        }),
    )
    .await;
    let bad_open = read_json_line(&mut r).await;
    assert_eq!(bad_open["error"]["code"], -32602);
    assert!(
        mgr.list_sessions().is_empty(),
        "failed browser_open must not leave behind leaked session"
    );

    // 2. Valid browser_open creates session, and a second browser_open with {"ephemeral": false} navigates instead of re-creating
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "browser_open",
                "arguments": { "session": "reuse", "ephemeral": true, "url": srv.url("/") }
            }
        }),
    )
    .await;
    let ok1 = read_json_line(&mut r).await;
    assert_eq!(ok1["result"]["isError"], false);

    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "browser_open",
                "arguments": { "session": "reuse", "ephemeral": false, "url": srv.url("/target") }
            }
        }),
    )
    .await;
    let ok2 = read_json_line(&mut r).await;
    assert_eq!(ok2["result"]["isError"], false);
    assert!(parse_tool_json(&ok2)["url"]
        .as_str()
        .unwrap_or("")
        .contains("/target"));

    // 3. mimikyu_wait and mimikyu_batch fail fast on invalid CSS selector "div[[["
    let t_wait = std::time::Instant::now();
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "mimikyu_wait",
                "arguments": { "session": "reuse", "selector": "div[[[", "timeout_ms": 5000 }
            }
        }),
    )
    .await;
    let bad_wait = read_json_line(&mut r).await;
    assert_eq!(bad_wait["error"]["code"], -32602);
    assert!(t_wait.elapsed() < std::time::Duration::from_secs(2));

    let t_batch = std::time::Instant::now();
    send_line(
        &mut w,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "mimikyu_batch",
                "arguments": {
                    "session": "reuse",
                    "ops": [{ "op": "wait", "selector": "div[[[", "timeout_ms": 5000 }]
                }
            }
        }),
    )
    .await;
    let bad_batch = read_json_line(&mut r).await;
    assert!(t_batch.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(bad_batch["result"]["isError"], true);
    let batch_report = parse_tool_json(&bad_batch);
    assert_eq!(batch_report["ok"], false);
    assert!(
        batch_report["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("invalid CSS selector"),
        "unexpected mimikyu_batch error: {batch_report}"
    );

    // 4. mimikyu_wait idle + stable_target prevalidation ("div[[[", "", "@e999")
    for (req_id, stable_target, expect_invalid_params) in [
        (6, "div[[[", true),
        (7, "", true),
        (8, "@e999", false),
    ] {
        let t_st = std::time::Instant::now();
        send_line(
            &mut w,
            &json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": {
                    "name": "mimikyu_wait",
                    "arguments": {
                        "session": "reuse",
                        "idle": true,
                        "stable_target": stable_target,
                        "timeout_ms": 5000
                    }
                }
            }),
        )
        .await;
        let resp = read_json_line(&mut r).await;
        assert!(
            t_st.elapsed() < std::time::Duration::from_secs(2),
            "mimikyu_wait(stable_target={stable_target:?}) stalled: {:?}",
            t_st.elapsed()
        );
        if expect_invalid_params {
            assert_eq!(resp["error"]["code"], -32602, "resp={resp}");
        } else {
            assert_eq!(resp["result"]["isError"], true, "resp={resp}");
            let txt = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
            assert!(txt.contains("unknown ref '@e999'"), "resp={resp}");
        }
    }

    drop(w);
    server_task.await.unwrap().unwrap();
}
