pub mod tools;

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::signal::unix::{signal, SignalKind};

use crate::daemon::{read_bounded_line_bytes, BoundedLine};
use crate::error::Result;
use crate::session::SessionManager;

pub use tools::{call_tool, tool_definitions};

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

fn jsonrpc_ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn jsonrpc_err(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

async fn write_json_line<W: AsyncWrite + Unpin>(writer: &mut W, payload: &Value) -> Result<()> {
    let mut encoded = serde_json::to_string(payload)?;
    encoded.push('\n');
    writer.write_all(encoded.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_request_object(
    manager: &SessionManager,
    obj: &serde_json::Map<String, Value>,
) -> Option<Value> {
    let has_id = obj.contains_key("id");
    let raw_id = obj.get("id").cloned();

    if let Some(ref id_val) = raw_id
        && !matches!(id_val, Value::Null | Value::Number(_) | Value::String(_))
    {
        return Some(jsonrpc_err(
            Value::Null,
            INVALID_REQUEST,
            "Invalid Request: 'id' must be string, number, or null",
        ));
    }

    let jsonrpc_ok_ver = obj.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
    let method_opt = obj
        .get("method")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());

    if !jsonrpc_ok_ver || method_opt.is_none() {
        return Some(jsonrpc_err(
            raw_id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "Invalid Request: expected jsonrpc '2.0' and non-empty string 'method'",
        ));
    }

    let method = method_opt.expect("checked Some above");

    if !has_id {
        // JSON-RPC 2.0 notifications must not emit a response.
        tracing::debug!(method = %method, "received MCP notification");
        return None;
    }

    let req_id = raw_id.unwrap_or(Value::Null);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    let resp = match method {
        "initialize" => jsonrpc_ok(
            req_id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "mimikyu",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "ping" => jsonrpc_ok(req_id, json!({})),
        "tools/list" => jsonrpc_ok(
            req_id,
            json!({
                "tools": tools::tool_definitions()
            }),
        ),
        "tools/call" => {
            let params_obj = match params {
                Value::Object(m) => m,
                _ => {
                    return Some(jsonrpc_err(
                        req_id,
                        INVALID_PARAMS,
                        "tools/call params must be an object",
                    ));
                }
            };
            let tool_name = match params_obj.get("name").and_then(Value::as_str) {
                Some(n) if !n.is_empty() => n,
                _ => {
                    return Some(jsonrpc_err(
                        req_id,
                        INVALID_PARAMS,
                        "tools/call requires non-empty string 'name'",
                    ));
                }
            };
            let arguments = params_obj.get("arguments").cloned().unwrap_or(Value::Null);
            match tools::call_tool(manager, tool_name, arguments).await {
                Ok(result) => jsonrpc_ok(req_id, result),
                Err((code, msg)) => jsonrpc_err(req_id, code, msg),
            }
        }
        other => jsonrpc_err(req_id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
    };

    Some(resp)
}

pub async fn serve_rw<R, W>(
    manager: Arc<SessionManager>,
    mut reader: R,
    mut writer: W,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut loop_err = None;

    loop {
        match read_bounded_line_bytes(&mut reader, MAX_LINE_BYTES).await {
            Ok(BoundedLine::Eof) => break,
            Ok(BoundedLine::TooLong) => {
                let resp = jsonrpc_err(
                    Value::Null,
                    INVALID_REQUEST,
                    format!("Invalid Request: request exceeded {MAX_LINE_BYTES} bytes"),
                );
                if let Err(we) = write_json_line(&mut writer, &resp).await {
                    loop_err = Some(we);
                    break;
                }
                continue;
            }
            Ok(BoundedLine::Ok(bytes)) => {
                let line = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        let resp = jsonrpc_err(
                            Value::Null,
                            PARSE_ERROR,
                            format!("Parse error: invalid UTF-8: {e}"),
                        );
                        if let Err(we) = write_json_line(&mut writer, &resp).await {
                            loop_err = Some(we);
                            break;
                        }
                        continue;
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let parsed: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        let resp = jsonrpc_err(
                            Value::Null,
                            PARSE_ERROR,
                            format!("Parse error: {e}"),
                        );
                        if let Err(we) = write_json_line(&mut writer, &resp).await {
                            loop_err = Some(we);
                            break;
                        }
                        continue;
                    }
                };

                let Some(obj) = parsed.as_object() else {
                    let resp = jsonrpc_err(
                        Value::Null,
                        INVALID_REQUEST,
                        "Invalid Request: expected JSON object",
                    );
                    if let Err(we) = write_json_line(&mut writer, &resp).await {
                        loop_err = Some(we);
                        break;
                    }
                    continue;
                };

                if let Some(resp) = handle_request_object(&manager, obj).await
                    && let Err(we) = write_json_line(&mut writer, &resp).await
                {
                    loop_err = Some(we);
                    break;
                }
            }
            Err(e) => {
                loop_err = Some(e.into());
                break;
            }
        }
    }

    let shutdown_res = manager.shutdown_all().await;
    if let Some(err) = loop_err {
        return Err(err);
    }
    shutdown_res
}

pub async fn serve_stdio(manager: Arc<SessionManager>) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        res = serve_rw(Arc::clone(&manager), stdin, stdout) => res,
        _ = sigterm.recv() => {
            tracing::info!("MCP server received SIGTERM");
            manager.shutdown_all().await
        }
        ctrl = tokio::signal::ctrl_c() => {
            if ctrl.is_ok() {
                tracing::info!("MCP server received SIGINT");
            }
            manager.shutdown_all().await
        }
    }
}
