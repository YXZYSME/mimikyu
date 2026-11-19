use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::daemon::{parse_batch_actions, SessionListEntry};
use crate::driver::EngineKind;
use crate::env_profile::PresetKind;
use crate::error::MimikyuError;
use crate::session::{ExtractMode, SessionCreateOpts, SessionManager, WaitRequest};
use crate::snapshot::SnapshotOptions;
use crate::wait::WaitSelectorState;

pub type RpcCallResult = std::result::Result<Value, (i64, String)>;

const INVALID_PARAMS: i64 = -32602;

fn session_prop() -> Value {
    json!({
        "type": "string",
        "description": "Session identifier (defaults to 'default')"
    })
}

fn session_creation_properties() -> Map<String, Value> {
    let mut props = Map::new();
    props.insert("session".into(), session_prop());
    props.insert(
        "profile".into(),
        json!({ "type": "string", "description": "Named profile or directory path for persistent storage" }),
    );
    props.insert(
        "ephemeral".into(),
        json!({ "type": "boolean", "description": "Use a temporary isolated profile directory" }),
    );
    props.insert(
        "virtual".into(),
        json!({ "type": "boolean", "description": "Use a lightweight virtual browser context" }),
    );
    props.insert(
        "engine".into(),
        json!({ "type": "string", "enum": ["chromium", "firefox"], "description": "Browser engine" }),
    );
    props.insert(
        "preset".into(),
        json!({ "type": "string", "enum": ["win11", "macos", "linux"], "description": "Environment profile preset" }),
    );
    props.insert(
        "locale".into(),
        json!({ "type": "string", "description": "BCP 47 locale identifier (e.g. en-US, de-DE)" }),
    );
    props.insert(
        "timezone".into(),
        json!({ "type": "string", "description": "IANA timezone identifier (e.g. America/Chicago)" }),
    );
    props.insert(
        "proxy".into(),
        json!({ "type": "string", "description": "HTTP or SOCKS5 proxy URL" }),
    );
    props.insert(
        "geoip".into(),
        json!({ "type": "string", "description": "IPv4/IPv6 address for local GeoIP region alignment" }),
    );
    props.insert(
        "viewport".into(),
        json!({ "type": "string", "description": "Viewport size in WxH format (e.g. 1280x720)" }),
    );
    props.insert(
        "block_images".into(),
        json!({ "type": "boolean", "description": "Block image requests" }),
    );
    props.insert(
        "block_media".into(),
        json!({ "type": "boolean", "description": "Block audio and video media requests" }),
    );
    props.insert(
        "block_fonts".into(),
        json!({ "type": "boolean", "description": "Block web font requests" }),
    );
    props.insert(
        "instant_animations".into(),
        json!({ "type": "boolean", "description": "Accelerate CSS animations and transitions" }),
    );
    props.insert(
        "humanize".into(),
        json!({ "type": "boolean", "description": "Enable kinematic pointer and typing motion" }),
    );
    props.insert(
        "headless".into(),
        json!({ "type": "boolean", "description": "Run browser in headless mode (defaults to true)" }),
    );
    props.insert(
        "main_world_eval".into(),
        json!({ "type": "boolean", "description": "Allow opt-in main-world evaluation (defaults to true)" }),
    );
    props
}

fn snapshot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": session_prop(),
            "diff": { "type": "boolean", "description": "Include structural diff from the previous snapshot" },
            "max_depth": { "type": "integer", "minimum": 0, "description": "Maximum tree traversal depth" },
            "interactive_only": { "type": "boolean", "description": "Keep only interactive elements and ancestors" },
            "include_bounds": { "type": "boolean", "description": "Resolve bounding boxes for interactive refs" }
        },
        "required": []
    })
}

fn click_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": { "type": "string", "description": "Target @eN ref or CSS selector" },
            "humanize": { "type": "boolean", "description": "Override humanize setting for this click" },
            "session": session_prop()
        },
        "required": ["target"]
    })
}

fn fill_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": { "type": "string", "description": "Target @eN ref or CSS selector" },
            "value": { "type": "string", "description": "Text value to fill" },
            "humanize": { "type": "boolean", "description": "Override humanize setting for this fill" },
            "session": session_prop()
        },
        "required": ["target", "value"]
    })
}

fn extract_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["markdown", "text", "links", "tables"],
                "description": "Extraction mode"
            },
            "session": session_prop()
        },
        "required": ["mode"]
    })
}

fn wait_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "idle": { "type": "boolean", "description": "Wait for page lifecycle, network quiet, and layout stability" },
            "text": { "type": "string", "description": "Wait until page text contains this substring" },
            "url": { "type": "string", "description": "Wait until current URL contains this substring" },
            "selector": { "type": "string", "description": "Wait for CSS selector state" },
            "state": {
                "type": "string",
                "enum": ["attached", "visible", "hidden", "detached"],
                "description": "Desired selector state"
            },
            "timeout_ms": { "type": "integer", "minimum": 1, "description": "Maximum wait duration in milliseconds" },
            "poll_interval_ms": { "type": "integer", "minimum": 1, "description": "Polling interval in milliseconds" },
            "network_quiet_ms": { "type": "integer", "minimum": 0, "description": "Quiet window for idle wait" },
            "stable_target": { "type": "string", "description": "Optional @eN or CSS target for bounding-box stability" },
            "session": session_prop()
        },
        "required": []
    })
}

fn batch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ops": {
                "type": "array",
                "items": { "type": "object" },
                "description": "Ordered array of action objects"
            },
            "file": { "type": "string", "description": "Path to JSON file containing batch actions" },
            "session": session_prop()
        },
        "required": []
    })
}

fn eval_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "js": { "type": "string", "description": "JavaScript expression or function body to evaluate" },
            "main_world": { "type": "boolean", "description": "Evaluate in page main world instead of isolated world" },
            "session": session_prop()
        },
        "required": ["js"]
    })
}

fn screenshot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "out_path": { "type": "string", "description": "Output PNG file path (returns base64 image block if omitted)" },
            "target": { "type": "string", "description": "Optional @eN ref or CSS selector to clip" },
            "full_page": { "type": "boolean", "description": "Capture full scrollable page when target is omitted" },
            "session": session_prop()
        },
        "required": []
    })
}

pub fn tool_definitions() -> Vec<Value> {
    let mut session_new_props = session_creation_properties();
    let session_new_schema = json!({
        "type": "object",
        "properties": Value::Object(session_new_props.clone()),
        "required": []
    });

    session_new_props.insert(
        "url".into(),
        json!({ "type": "string", "description": "Optional URL to navigate to after opening or reusing session" }),
    );
    let browser_open_schema = json!({
        "type": "object",
        "properties": Value::Object(session_new_props),
        "required": []
    });

    vec![
        // 11 mimikyu_* tools
        json!({
            "name": "mimikyu_session_new",
            "description": "Create a new isolated browser session",
            "inputSchema": session_new_schema
        }),
        json!({
            "name": "mimikyu_session_close",
            "description": "Close an active browser session and clean up its resources",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session_prop() },
                "required": []
            }
        }),
        json!({
            "name": "mimikyu_open",
            "description": "Navigate an existing session to a URL",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Target HTTP/HTTPS/data/about URL" },
                    "session": session_prop()
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "mimikyu_snapshot",
            "description": "Capture a compact @eN semantic accessibility tree snapshot",
            "inputSchema": snapshot_schema()
        }),
        json!({
            "name": "mimikyu_click",
            "description": "Click an element by @eN ref or CSS selector",
            "inputSchema": click_schema()
        }),
        json!({
            "name": "mimikyu_fill",
            "description": "Clear and fill a form field by @eN ref or CSS selector",
            "inputSchema": fill_schema()
        }),
        json!({
            "name": "mimikyu_extract",
            "description": "Extract structured content (markdown, text, links, or tables) from the current page",
            "inputSchema": extract_schema()
        }),
        json!({
            "name": "mimikyu_wait",
            "description": "Wait for page idle, text, URL substring, or selector state",
            "inputSchema": wait_schema()
        }),
        json!({
            "name": "mimikyu_batch",
            "description": "Execute an ordered sequence of browser actions in a single call",
            "inputSchema": batch_schema()
        }),
        json!({
            "name": "mimikyu_eval",
            "description": "Evaluate JavaScript in isolated world or opt-in main world",
            "inputSchema": eval_schema()
        }),
        json!({
            "name": "mimikyu_screenshot",
            "description": "Capture a PNG screenshot of the viewport, full page, or target element",
            "inputSchema": screenshot_schema()
        }),
        // 20 browser_* tools
        json!({
            "name": "browser_open",
            "description": "Open a browser session if needed and navigate to a URL",
            "inputSchema": browser_open_schema
        }),
        json!({
            "name": "browser_close",
            "description": "Close an active browser session",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session_prop() },
                "required": []
            }
        }),
        json!({
            "name": "browser_snapshot",
            "description": "Capture a compact @eN semantic accessibility tree snapshot",
            "inputSchema": snapshot_schema()
        }),
        json!({
            "name": "browser_click",
            "description": "Click an element by @eN ref or CSS selector",
            "inputSchema": click_schema()
        }),
        json!({
            "name": "browser_fill",
            "description": "Clear and fill a form field by @eN ref or CSS selector",
            "inputSchema": fill_schema()
        }),
        json!({
            "name": "browser_type",
            "description": "Type text sequentially into an element by @eN ref or CSS selector",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Target @eN ref or CSS selector" },
                    "text": { "type": "string", "description": "Text to type" },
                    "humanize": { "type": "boolean", "description": "Override humanize setting for typing" },
                    "session": session_prop()
                },
                "required": ["target", "text"]
            }
        }),
        json!({
            "name": "browser_press",
            "description": "Press a keyboard key or key combination",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Key name (e.g. Enter, Tab, Escape)" },
                    "session": session_prop()
                },
                "required": ["key"]
            }
        }),
        json!({
            "name": "browser_select",
            "description": "Select an option in a <select> element by value or label",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Target @eN ref or CSS selector" },
                    "value": { "type": "string", "description": "Option value or label" },
                    "session": session_prop()
                },
                "required": ["target", "value"]
            }
        }),
        json!({
            "name": "browser_upload",
            "description": "Attach a local file to a file input element",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Target @eN ref or CSS selector" },
                    "path": { "type": "string", "description": "Path to local file to upload" },
                    "session": session_prop()
                },
                "required": ["target", "path"]
            }
        }),
        json!({
            "name": "browser_scroll",
            "description": "Scroll the viewport by horizontal and vertical deltas",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dx": { "type": "number", "description": "Horizontal scroll delta in pixels" },
                    "dy": { "type": "number", "description": "Vertical scroll delta in pixels" },
                    "humanize": { "type": "boolean", "description": "Override humanize setting for scrolling" },
                    "session": session_prop()
                },
                "required": ["dx", "dy"]
            }
        }),
        json!({
            "name": "browser_eval",
            "description": "Evaluate JavaScript in isolated world or opt-in main world",
            "inputSchema": eval_schema()
        }),
        json!({
            "name": "browser_extract",
            "description": "Extract structured content (markdown, text, links, or tables) from the current page",
            "inputSchema": extract_schema()
        }),
        json!({
            "name": "browser_screenshot",
            "description": "Capture a PNG screenshot of the viewport, full page, or target element",
            "inputSchema": screenshot_schema()
        }),
        json!({
            "name": "browser_wait",
            "description": "Wait for page idle, text, URL substring, or selector state",
            "inputSchema": wait_schema()
        }),
        json!({
            "name": "browser_batch",
            "description": "Execute an ordered sequence of browser actions in a single call",
            "inputSchema": batch_schema()
        }),
        json!({
            "name": "browser_context_list",
            "description": "List all active browser sessions and virtual contexts",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "browser_context_close",
            "description": "Close an active browser session or virtual context",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session_prop(),
                    "context": { "type": "string", "description": "Alias for session identifier" }
                },
                "required": []
            }
        }),
        json!({
            "name": "browser_profile_export",
            "description": "Export session cookies and storage state to a JSON file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "out_path": { "type": "string", "description": "Destination storage state JSON path" },
                    "session": session_prop()
                },
                "required": ["out_path"]
            }
        }),
        json!({
            "name": "browser_profile_import",
            "description": "Import session cookies and storage state from a JSON file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "in_path": { "type": "string", "description": "Source storage state JSON path" },
                    "session": session_prop()
                },
                "required": ["in_path"]
            }
        }),
        json!({
            "name": "browser_env_report",
            "description": "Inspect environment profile, coherence, proxy, and resource policy for a session",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session_prop() },
                "required": []
            }
        }),
    ]
}

fn call_ok_json(data: Value) -> RpcCallResult {
    let text = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    }))
}

fn call_err_text(msg: impl Into<String>) -> RpcCallResult {
    let payload = json!({
        "ok": false,
        "error": msg.into()
    });
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&payload).unwrap_or_default() }],
        "isError": true
    }))
}

fn map_session_result<T: serde::Serialize>(res: crate::error::Result<T>) -> RpcCallResult {
    match res {
        Ok(val) => call_ok_json(serde_json::to_value(val).unwrap_or(Value::Null)),
        Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
        Err(err) => call_err_text(err.to_string()),
    }
}

fn resolve_session_id(args: &Map<String, Value>) -> std::result::Result<String, (i64, String)> {
    match args.get("session").or_else(|| args.get("session_id")) {
        None | Some(Value::Null) => Ok("default".to_string()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err((INVALID_PARAMS, "'session' must be a string".to_string())),
    }
}

fn req_str<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> std::result::Result<&'a str, (i64, String)> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(_) => Err((INVALID_PARAMS, format!("'{key}' must be a string"))),
        None => Err((
            INVALID_PARAMS,
            format!("missing required argument '{key}'"),
        )),
    }
}

fn opt_str<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<&'a str>, (i64, String)> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err((INVALID_PARAMS, format!("'{key}' must be a string"))),
    }
}

fn opt_bool(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<bool>, (i64, String)> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err((INVALID_PARAMS, format!("'{key}' must be a boolean"))),
    }
}

fn opt_u64(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<u64>, (i64, String)> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else {
                Err((
                    INVALID_PARAMS,
                    format!("'{key}' must be a non-negative integer"),
                ))
            }
        }
        Some(_) => Err((
            INVALID_PARAMS,
            format!("'{key}' must be a non-negative integer"),
        )),
    }
}

fn req_f64(args: &Map<String, Value>, key: &str) -> std::result::Result<f64, (i64, String)> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| (INVALID_PARAMS, format!("'{key}' must be a finite number"))),
        Some(_) => Err((INVALID_PARAMS, format!("'{key}' must be a number"))),
        None => Err((
            INVALID_PARAMS,
            format!("missing required argument '{key}'"),
        )),
    }
}

fn parse_viewport_arg(val: &Value) -> std::result::Result<(u32, u32), (i64, String)> {
    let parse_dim = |v: u64, label: &str| -> std::result::Result<u32, (i64, String)> {
        let d = u32::try_from(v).map_err(|_| {
            (
                INVALID_PARAMS,
                format!("viewport {label} exceeds u32 bounds"),
            )
        })?;
        if d == 0 {
            return Err((
                INVALID_PARAMS,
                format!("viewport {label} must be positive"),
            ));
        }
        Ok(d)
    };

    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            let (ws, hs) = trimmed
                .split_once('x')
                .or_else(|| trimmed.split_once('X'))
                .ok_or_else(|| {
                    (
                        INVALID_PARAMS,
                        format!("invalid viewport '{s}' (expected WxH)"),
                    )
                })?;
            let w: u32 = ws.trim().parse().map_err(|_| {
                (
                    INVALID_PARAMS,
                    format!("invalid viewport width in '{s}'"),
                )
            })?;
            let h: u32 = hs.trim().parse().map_err(|_| {
                (
                    INVALID_PARAMS,
                    format!("invalid viewport height in '{s}'"),
                )
            })?;
            if w == 0 || h == 0 {
                return Err((
                    INVALID_PARAMS,
                    "viewport dimensions must be positive".to_string(),
                ));
            }
            Ok((w, h))
        }
        Value::Array(arr) if arr.len() == 2 => {
            let w = arr[0]
                .as_u64()
                .ok_or_else(|| (INVALID_PARAMS, "viewport[0] must be an integer".to_string()))?;
            let h = arr[1]
                .as_u64()
                .ok_or_else(|| (INVALID_PARAMS, "viewport[1] must be an integer".to_string()))?;
            Ok((parse_dim(w, "width")?, parse_dim(h, "height")?))
        }
        Value::Object(obj) => {
            let w = obj
                .get("width")
                .and_then(Value::as_u64)
                .ok_or_else(|| (INVALID_PARAMS, "viewport.width must be an integer".to_string()))?;
            let h = obj
                .get("height")
                .and_then(Value::as_u64)
                .ok_or_else(|| (INVALID_PARAMS, "viewport.height must be an integer".to_string()))?;
            Ok((parse_dim(w, "width")?, parse_dim(h, "height")?))
        }
        _ => Err((
            INVALID_PARAMS,
            "viewport must be 'WxH', [w, h], or {width, height}".to_string(),
        )),
    }
}

fn build_session_opts(
    args: &Map<String, Value>,
    session_id: String,
) -> std::result::Result<SessionCreateOpts, (i64, String)> {
    let engine = match opt_str(args, "engine")? {
        Some(raw) => Some(
            raw.parse::<EngineKind>()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?,
        ),
        None => None,
    };
    let preset = match opt_str(args, "preset")? {
        Some(raw) => Some(
            raw.parse::<PresetKind>()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?,
        ),
        None => None,
    };
    let viewport = match args.get("viewport") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_viewport_arg(v)?),
    };

    Ok(SessionCreateOpts {
        session_id: Some(session_id),
        engine,
        profile: opt_str(args, "profile")?.map(str::to_string),
        ephemeral: opt_bool(args, "ephemeral")?.unwrap_or(false),
        virtual_ctx: opt_bool(args, "virtual")?.unwrap_or(false),
        preset,
        locale: opt_str(args, "locale")?.map(str::to_string),
        timezone: opt_str(args, "timezone")?.map(str::to_string),
        proxy: opt_str(args, "proxy")?.map(str::to_string),
        geoip: opt_str(args, "geoip")?.map(str::to_string),
        viewport,
        block_images: opt_bool(args, "block_images")?.unwrap_or(false),
        block_media: opt_bool(args, "block_media")?.unwrap_or(false),
        block_fonts: opt_bool(args, "block_fonts")?.unwrap_or(false),
        instant_animations: opt_bool(args, "instant_animations")?.unwrap_or(false),
        humanize: opt_bool(args, "humanize")?.unwrap_or(false),
        main_world_eval: opt_bool(args, "main_world_eval")?.unwrap_or(true),
        headless: opt_bool(args, "headless")?.unwrap_or(true),
    })
}

fn parse_snapshot_opts(args: &Map<String, Value>) -> std::result::Result<SnapshotOptions, (i64, String)> {
    let max_depth = opt_u64(args, "max_depth")?
        .map(|v| {
            usize::try_from(v)
                .map_err(|_| (INVALID_PARAMS, "max_depth exceeds usize".to_string()))
        })
        .transpose()?;
    Ok(SnapshotOptions {
        max_depth,
        interactive_only: opt_bool(args, "interactive_only")?.unwrap_or(false),
        diff: opt_bool(args, "diff")?.unwrap_or(false),
        include_bounds: opt_bool(args, "include_bounds")?.unwrap_or(false),
    })
}

fn parse_wait_req(args: &Map<String, Value>) -> std::result::Result<WaitRequest, (i64, String)> {
    let state = match opt_str(args, "state")? {
        None => WaitSelectorState::Visible,
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "attached" => WaitSelectorState::Attached,
            "visible" => WaitSelectorState::Visible,
            "hidden" => WaitSelectorState::Hidden,
            "detached" => WaitSelectorState::Detached,
            other => {
                return Err((
                    INVALID_PARAMS,
                    format!(
                        "unknown wait selector state '{other}' (expected attached|visible|hidden|detached)"
                    ),
                ));
            }
        },
    };
    Ok(WaitRequest {
        idle: opt_bool(args, "idle")?.unwrap_or(false),
        text: opt_str(args, "text")?.map(str::to_string),
        url: opt_str(args, "url")?.map(str::to_string),
        selector: opt_str(args, "selector")?.map(str::to_string),
        state,
        timeout_ms: opt_u64(args, "timeout_ms")?,
        poll_interval_ms: opt_u64(args, "poll_interval_ms")?,
        network_quiet_ms: opt_u64(args, "network_quiet_ms")?,
        stable_target: opt_str(args, "stable_target")?.map(str::to_string),
    })
}

async fn handle_batch_tool(
    manager: &SessionManager,
    session_id: &str,
    args: &Map<String, Value>,
) -> RpcCallResult {
    let ops_val = args.get("ops").filter(|v| !v.is_null());
    let file_val = opt_str(args, "file")?;

    let raw = match (ops_val, file_val) {
        (Some(_), Some(_)) => {
            return Err((
                INVALID_PARAMS,
                "specify either 'ops' or 'file', not both".to_string(),
            ));
        }
        (None, None) => {
            return Err((
                INVALID_PARAMS,
                "batch requires either 'ops' or 'file'".to_string(),
            ));
        }
        (Some(Value::String(s)), None) => serde_json::from_str::<Value>(s)
            .map_err(|e| (INVALID_PARAMS, format!("invalid JSON in 'ops': {e}")))?,
        (Some(v), None) => v.clone(),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                (
                    INVALID_PARAMS,
                    format!("failed to read batch file '{path}': {e}"),
                )
            })?;
            serde_json::from_str::<Value>(&text).map_err(|e| {
                (
                    INVALID_PARAMS,
                    format!("invalid JSON in batch file '{path}': {e}"),
                )
            })?
        }
    };

    let actions = parse_batch_actions(raw).map_err(|e| (INVALID_PARAMS, e.to_string()))?;
    match manager.batch(session_id, actions).await {
        Ok(report) => {
            let is_err = !report.ok;
            let text = serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string());
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_err
            }))
        }
        Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
        Err(err) => call_err_text(err.to_string()),
    }
}

async fn handle_screenshot_tool(
    manager: &SessionManager,
    session_id: &str,
    args: &Map<String, Value>,
) -> RpcCallResult {
    let out_path = opt_str(args, "out_path")?.or(opt_str(args, "path")?);
    let target = opt_str(args, "target")?;
    let full_page = opt_bool(args, "full_page")?.unwrap_or(false);

    if let Some(path) = out_path {
        return map_session_result(
            manager
                .screenshot(session_id, Path::new(path), target, full_page)
                .await,
        );
    }

    let tmp = tempfile::Builder::new()
        .prefix("mimikyu-mcp-shot-")
        .suffix(".png")
        .tempfile()
        .map_err(|e| (INVALID_PARAMS, format!("failed to allocate temp screenshot file: {e}")))?;
    let tmp_path = tmp.path().to_path_buf();
    let res = manager
        .screenshot(session_id, &tmp_path, target, full_page)
        .await;

    match res {
        Ok(info) => {
            let bytes = std::fs::read(&tmp_path).map_err(|e| {
                (
                    INVALID_PARAMS,
                    format!("failed to read captured screenshot: {e}"),
                )
            })?;
            drop(tmp);
            let b64 = BASE64.encode(&bytes);
            let meta = json!({
                "width": info.width,
                "height": info.height,
                "bytes": info.bytes
            });
            Ok(json!({
                "content": [
                    { "type": "image", "data": b64, "mimeType": "image/png" },
                    { "type": "text", "text": serde_json::to_string(&meta).unwrap_or_default() }
                ],
                "isError": false
            }))
        }
        Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
        Err(err) => call_err_text(err.to_string()),
    }
}

pub async fn call_tool(manager: &SessionManager, name: &str, arguments: Value) -> RpcCallResult {
    let args = match arguments {
        Value::Null => Map::new(),
        Value::Object(m) => m,
        _ => {
            return Err((
                INVALID_PARAMS,
                "tool arguments must be a JSON object".to_string(),
            ));
        }
    };

    match name {
        "mimikyu_session_new" => {
            let session_id = resolve_session_id(&args)?;
            let opts = build_session_opts(&args, session_id)?;
            map_session_result(manager.create_session(opts).await.map(SessionListEntry::from))
        }
        "mimikyu_session_close" | "browser_close" => {
            let session_id = resolve_session_id(&args)?;
            match manager.close_session(&session_id).await {
                Ok(()) => call_ok_json(json!({ "closed": session_id })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "mimikyu_open" => {
            let session_id = resolve_session_id(&args)?;
            let url = req_str(&args, "url")?;
            map_session_result(manager.open(&session_id, url).await)
        }
        "browser_open" => {
            let session_id = resolve_session_id(&args)?;
            let url = opt_str(&args, "url")?;
            if let Some(target_url) = url {
                crate::session::validate_nav_url(target_url)
                    .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
            }
            let exists = manager
                .list_sessions()
                .iter()
                .any(|s| s.session_id == session_id);
            if exists && url.is_none() {
                return Err((
                    INVALID_PARAMS,
                    "missing required argument 'url' for existing session".to_string(),
                ));
            }
            let created_summary = if !exists {
                let opts = build_session_opts(&args, session_id.clone())?;
                match manager.create_session(opts).await {
                    Ok(s) => Some(SessionListEntry::from(s)),
                    Err(MimikyuError::InvalidConfig(msg)) => return Err((INVALID_PARAMS, msg)),
                    Err(err) => return call_err_text(err.to_string()),
                }
            } else {
                None
            };

            if let Some(target_url) = url {
                match manager.open(&session_id, target_url).await {
                    Ok(nav) => {
                        if let Some(sess) = created_summary {
                            call_ok_json(json!({ "session": sess, "navigation": nav }))
                        } else {
                            call_ok_json(serde_json::to_value(nav).unwrap_or(Value::Null))
                        }
                    }
                    Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                    Err(err) => call_err_text(err.to_string()),
                }
            } else if let Some(sess) = created_summary {
                call_ok_json(serde_json::to_value(sess).unwrap_or(Value::Null))
            } else {
                Err((
                    INVALID_PARAMS,
                    "missing required argument 'url' for existing session".to_string(),
                ))
            }
        }
        "mimikyu_snapshot" | "browser_snapshot" => {
            let session_id = resolve_session_id(&args)?;
            let opts = parse_snapshot_opts(&args)?;
            map_session_result(manager.snapshot(&session_id, opts).await)
        }
        "mimikyu_click" | "browser_click" => {
            let session_id = resolve_session_id(&args)?;
            let target = req_str(&args, "target")?;
            let humanize = opt_bool(&args, "humanize")?;
            match manager.click(&session_id, target, humanize).await {
                Ok(()) => call_ok_json(json!({ "clicked": target })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "mimikyu_fill" | "browser_fill" => {
            let session_id = resolve_session_id(&args)?;
            let target = req_str(&args, "target")?;
            let value = req_str(&args, "value")?;
            let humanize = opt_bool(&args, "humanize")?;
            match manager.fill(&session_id, target, value, humanize).await {
                Ok(()) => call_ok_json(json!({ "filled": target })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_type" => {
            let session_id = resolve_session_id(&args)?;
            let target = req_str(&args, "target")?;
            let text = req_str(&args, "text")?;
            let humanize = opt_bool(&args, "humanize")?;
            match manager.type_text(&session_id, target, text, humanize).await {
                Ok(()) => call_ok_json(json!({ "typed": target })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_press" => {
            let session_id = resolve_session_id(&args)?;
            let key = req_str(&args, "key")?;
            match manager.press(&session_id, key).await {
                Ok(()) => call_ok_json(json!({ "pressed": key })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_select" => {
            let session_id = resolve_session_id(&args)?;
            let target = req_str(&args, "target")?;
            let value = req_str(&args, "value")?;
            match manager.select(&session_id, target, value).await {
                Ok(selected) => call_ok_json(json!({ "selected": selected })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_upload" => {
            let session_id = resolve_session_id(&args)?;
            let target = req_str(&args, "target")?;
            let path = req_str(&args, "path")?;
            match manager.upload(&session_id, target, PathBuf::from(path)).await {
                Ok(()) => call_ok_json(json!({ "uploaded": target })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_scroll" => {
            let session_id = resolve_session_id(&args)?;
            let dx = req_f64(&args, "dx")?;
            let dy = req_f64(&args, "dy")?;
            let humanize = opt_bool(&args, "humanize")?;
            match manager.scroll(&session_id, dx, dy, humanize).await {
                Ok(()) => call_ok_json(json!({ "dx": dx, "dy": dy })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "mimikyu_extract" | "browser_extract" => {
            let session_id = resolve_session_id(&args)?;
            let mode_str = req_str(&args, "mode")?;
            let mode = mode_str
                .parse::<ExtractMode>()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
            map_session_result(manager.extract(&session_id, mode).await)
        }
        "mimikyu_wait" | "browser_wait" => {
            let session_id = resolve_session_id(&args)?;
            let req = parse_wait_req(&args)?;
            match manager.wait(&session_id, req).await {
                Ok(()) => call_ok_json(json!({ "waited": true })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "mimikyu_batch" | "browser_batch" => {
            let session_id = resolve_session_id(&args)?;
            handle_batch_tool(manager, &session_id, &args).await
        }
        "mimikyu_eval" | "browser_eval" => {
            let session_id = resolve_session_id(&args)?;
            let js = req_str(&args, "js")?;
            let main_world = opt_bool(&args, "main_world")?.unwrap_or(false);
            match manager.eval(&session_id, js, main_world).await {
                Ok(val) => call_ok_json(json!({ "value": val, "main_world": main_world })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "mimikyu_screenshot" | "browser_screenshot" => {
            let session_id = resolve_session_id(&args)?;
            handle_screenshot_tool(manager, &session_id, &args).await
        }
        "browser_context_list" => {
            let entries: Vec<SessionListEntry> = manager
                .list_sessions()
                .into_iter()
                .map(SessionListEntry::from)
                .collect();
            call_ok_json(serde_json::to_value(entries).unwrap_or_else(|_| json!([])))
        }
        "browser_context_close" => {
            let session_id = match args
                .get("session")
                .or_else(|| args.get("session_id"))
                .or_else(|| args.get("context"))
            {
                None | Some(Value::Null) => "default".to_string(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => {
                    return Err((
                        INVALID_PARAMS,
                        "'session'/'context' must be a string".to_string(),
                    ));
                }
            };
            match manager.close_session(&session_id).await {
                Ok(()) => call_ok_json(json!({ "closed": session_id })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_profile_export" => {
            let session_id = resolve_session_id(&args)?;
            let out_path = opt_str(&args, "out_path")?
                .or(opt_str(&args, "path")?)
                .or(opt_str(&args, "file")?)
                .ok_or_else(|| (INVALID_PARAMS, "missing required argument 'out_path'".to_string()))?;
            match manager.export_session(&session_id, Path::new(out_path)).await {
                Ok(()) => call_ok_json(json!({ "session_id": session_id, "out_path": out_path })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_profile_import" => {
            let session_id = resolve_session_id(&args)?;
            let in_path = opt_str(&args, "in_path")?
                .or(opt_str(&args, "path")?)
                .or(opt_str(&args, "file")?)
                .ok_or_else(|| (INVALID_PARAMS, "missing required argument 'in_path'".to_string()))?;
            match manager.import_session(&session_id, Path::new(in_path)).await {
                Ok(()) => call_ok_json(json!({ "session_id": session_id, "in_path": in_path })),
                Err(MimikyuError::InvalidConfig(msg)) => Err((INVALID_PARAMS, msg)),
                Err(err) => call_err_text(err.to_string()),
            }
        }
        "browser_env_report" => {
            let session_id = resolve_session_id(&args)?;
            map_session_result(manager.env_report(&session_id).await)
        }
        other => Err((INVALID_PARAMS, format!("unknown tool '{other}'"))),
    }
}
