use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::driver::{
    now_ms, AxNode, BoundingBox, BrowserDriver, ConsoleEntry, ContextOptions, Cookie,
    DriverCapabilities, DriverSession, EngineKind, EnvProfile, KeyEvent, KeyEventType,
    LaunchOptions, MouseButton, MouseEvent, MouseEventType, NavInfo, NetworkEvent, OriginStorage,
    PageError, PermissionGrant, PermissionState, ProfileMode, ProxyConfig, RouteMock,
    ScreenshotFormat, ScreenshotOpts, SessionInfo, StorageState, TabInfo, WheelEvent,
};
use crate::env_profile::{build_window_init_script, ProxyRelay};
use crate::error::{MimikyuError, Result};
use crate::network::{
    http_reason_phrase, merge_headers_case_insensitive, BlockPolicy, InterceptDecision,
    InterceptRequest, NetworkInterceptor, ResourceType, RouteRule, TelemetryConfig, TelemetryStore,
};
use crate::process::ProcessGuard;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const ISOLATED_SANDBOX: &str = "mimikyu_isolated";

pub struct BidiDriver;

impl BidiDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BidiDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BrowserDriver for BidiDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Firefox
    }

    fn capabilities(&self) -> DriverCapabilities {
        bidi_capabilities()
    }

    async fn launch(&self, mut opts: LaunchOptions) -> Result<Arc<dyn DriverSession>> {
        if opts.engine != EngineKind::Firefox {
            return Err(MimikyuError::InvalidConfig(format!(
                "BidiDriver cannot launch engine '{}'",
                opts.engine
            )));
        }

        let mut root_relay: Option<ProxyRelay> = None;
        if let Some(ref proxy) = opts.proxy
            && proxy.needs_relay()
        {
            let relay = ProxyRelay::start(proxy).await?;
            let rewritten = relay.browser_proxy_config(proxy.bypass.clone());
            opts.proxy = Some(rewritten);
            root_relay = Some(relay);
        }

        let process = ProcessGuard::launch(&opts).await?;
        let ws_base = process.ws_endpoint().to_string();
        let ws_session_url = format!("{}/session", ws_base.trim_end_matches('/'));
        let browser_version = process.browser_version().to_string();
        let profile_dir = process.profile_dir().to_path_buf();
        let pid = process.pid();

        let conn = BidiConnection::connect(&ws_session_url, process, root_relay).await?;

        let new_sess = conn
            .send(
                "session.new",
                json!({
                    "capabilities": {
                        "alwaysMatch": {
                            "acceptInsecureCerts": true
                        }
                    }
                }),
            )
            .await?;

        let bidi_session_id = new_sess["sessionId"]
            .as_str()
            .unwrap_or("bidi-session")
            .to_string();

        conn.send(
            "session.subscribe",
            json!({
                "events": [
                    "log.entryAdded",
                    "network.beforeRequestSent",
                    "network.responseCompleted",
                    "browsingContext.load",
                    "browsingContext.contextCreated",
                    "browsingContext.contextDestroyed"
                ]
            }),
        )
        .await?;
        let _ = conn
            .send(
                "session.subscribe",
                json!({ "events": ["network.fetchError"] }),
            )
            .await;

        let (user_context, owns_user_context, target_id) = match opts.profile_mode {
            ProfileMode::VirtualContext => {
                let uc = create_user_context(&conn, opts.proxy.as_ref()).await?;
                conn.isolated_contexts.write().insert(uc.clone());
                // Must use type: "window" (type: "tab" with default background:false hangs in headless firefox-1497).
                let created = conn
                    .send(
                        "browsingContext.create",
                        json!({
                            "type": "window",
                            "userContext": uc,
                        }),
                    )
                    .await?;
                let ctx_id = created["context"]
                    .as_str()
                    .ok_or_else(|| MimikyuError::Protocol {
                        code: -1,
                        message: "browsingContext.create missing context".to_string(),
                    })?
                    .to_string();
                (Some(uc), true, ctx_id)
            }
            _ => {
                let tree = conn.send("browsingContext.getTree", json!({})).await?;
                let existing = tree["contexts"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|c| c["context"].as_str())
                    .map(ToString::to_string);
                let ctx_id = if let Some(c) = existing {
                    c
                } else {
                    let created = conn
                        .send("browsingContext.create", json!({ "type": "window" }))
                        .await?;
                    created["context"]
                        .as_str()
                        .ok_or_else(|| MimikyuError::Protocol {
                            code: -1,
                            message: "browsingContext.create missing context".to_string(),
                        })?
                        .to_string()
                };
                (None, false, ctx_id)
            }
        };

        conn.set_active_target(user_context.as_deref(), &target_id);

        let info = SessionInfo {
            session_id: bidi_session_id,
            engine: EngineKind::Firefox,
            browser_version,
            context_id: user_context.clone(),
            target_id: target_id.clone(),
            profile_dir: Some(profile_dir),
            ws_endpoint: ws_session_url,
            pid: Some(pid),
        };

        let session = BidiSession::create(
            conn,
            info,
            user_context,
            owns_user_context,
            true,
            opts.main_world_eval,
            opts.telemetry_capacity,
            None,
        );

        session
            .apply_initial_options(
                opts.env_profile,
                Vec::new(),
                opts.block_images,
                opts.block_media,
                opts.block_fonts,
                opts.instant_animations,
            )
            .await?;

        Ok(session)
    }
}

fn bidi_capabilities() -> DriverCapabilities {
    DriverCapabilities {
        engine: EngineKind::Firefox,
        protocol: "webdriver-bidi".to_string(),
        supports_virtual_context: true,
        supports_per_context_proxy: true,
        supports_native_ax_tree: false,
        supports_isolated_world: true,
        supports_pdf: true,
        supports_network_intercept: true,
    }
}

async fn create_user_context(
    conn: &BidiConnection,
    proxy: Option<&ProxyConfig>,
) -> Result<String> {
    let mut params = json!({ "acceptInsecureCerts": true });
    if let Some(p) = proxy {
        params["proxy"] = proxy_to_bidi_json(p)?;
    }
    let res = conn.send("browser.createUserContext", params).await?;
    res["userContext"]
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| MimikyuError::Protocol {
            code: -1,
            message: "browser.createUserContext missing userContext".to_string(),
        })
}

fn resolve_proxy_port(parsed: &url::Url) -> u16 {
    if let Some(port) = parsed.port_or_known_default() {
        return port;
    }
    match parsed.scheme().to_ascii_lowercase().as_str() {
        "socks" | "socks4" | "socks5" | "socks5h" => 1080,
        _ => 8080,
    }
}

fn proxy_to_bidi_json(proxy: &ProxyConfig) -> Result<Value> {
    let parsed = url::Url::parse(&proxy.server)
        .or_else(|_| url::Url::parse(&format!("http://{}", proxy.server)))?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = resolve_proxy_port(&parsed);
    let host_port = format!("{host}:{port}");

    let mut obj = json!({
        "proxyType": "manual",
    });
    match parsed.scheme().to_ascii_lowercase().as_str() {
        "socks" | "socks5" | "socks5h" => {
            obj["socksProxy"] = json!(host_port);
            obj["socksVersion"] = json!(5);
        }
        "socks4" => {
            obj["socksProxy"] = json!(host_port);
            obj["socksVersion"] = json!(4);
        }
        _ => {
            obj["httpProxy"] = json!(host_port);
            obj["sslProxy"] = json!(host_port);
        }
    }
    if !proxy.bypass.is_empty() {
        obj["noProxy"] = json!(proxy.bypass);
    }
    Ok(obj)
}

struct BidiConnection {
    next_id: AtomicU64,
    write_tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    contexts: RwLock<HashMap<String, Arc<BidiContextState>>>,
    context_parents: RwLock<HashMap<String, String>>,
    isolated_contexts: RwLock<HashSet<String>>,
    active_targets: RwLock<HashMap<String, String>>,
    process: AsyncMutex<Option<ProcessGuard>>,
    _root_proxy_relay: Mutex<Option<ProxyRelay>>,
    closed: AtomicBool,
    io_tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn context_scope_key(user_context: Option<&str>) -> String {
    user_context.unwrap_or("default").to_string()
}

impl BidiConnection {
    fn set_active_target(&self, user_context: Option<&str>, target_id: &str) {
        self.active_targets
            .write()
            .insert(context_scope_key(user_context), target_id.to_string());
    }

    fn active_target_for(&self, user_context: Option<&str>) -> Option<String> {
        self.active_targets
            .read()
            .get(&context_scope_key(user_context))
            .cloned()
    }

    fn clear_active_target_if_matches(&self, user_context: Option<&str>, target_id: &str) {
        let key = context_scope_key(user_context);
        let mut guard = self.active_targets.write();
        if guard.get(&key).map(String::as_str) == Some(target_id) {
            guard.remove(&key);
        }
    }

    fn resolve_context_state(&self, cid: &str) -> Option<Arc<BidiContextState>> {
        let contexts = self.contexts.read();
        if let Some(state) = contexts.get(cid) {
            return Some(Arc::clone(state));
        }
        let parents = self.context_parents.read();
        let mut cur = cid;
        for _ in 0..64 {
            let Some(parent) = parents.get(cur) else {
                break;
            };
            if let Some(state) = contexts.get(parent.as_str()) {
                return Some(Arc::clone(state));
            }
            cur = parent.as_str();
        }
        None
    }

    async fn connect(
        ws_url: &str,
        process: ProcessGuard,
        root_proxy_relay: Option<ProxyRelay>,
    ) -> Result<Arc<Self>> {
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(128 * 1024 * 1024);
        ws_config.max_frame_size = Some(64 * 1024 * 1024);

        let (ws_stream, _) = connect_async_with_config(ws_url, Some(ws_config), true)
            .await
            .map_err(|e| MimikyuError::Transport(format!("BiDi WebSocket connect failed: {e}")))?;

        let (mut sink, mut stream) = ws_stream.split();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();

        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            write_tx,
            pending: Mutex::new(HashMap::new()),
            contexts: RwLock::new(HashMap::new()),
            context_parents: RwLock::new(HashMap::new()),
            isolated_contexts: RwLock::new(HashSet::new()),
            active_targets: RwLock::new(HashMap::new()),
            process: AsyncMutex::new(Some(process)),
            _root_proxy_relay: Mutex::new(root_proxy_relay),
            closed: AtomicBool::new(false),
            io_tasks: Mutex::new(Vec::new()),
        });

        let write_handle = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let weak_conn: Weak<Self> = Arc::downgrade(&conn);
        let read_handle = tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                let text_owned: String;
                let text_ref: &str = match &item {
                    Ok(Message::Text(text)) => text.as_str(),
                    Ok(Message::Binary(bin)) => {
                        let Ok(s) = std::str::from_utf8(bin) else {
                            continue;
                        };
                        text_owned = s.to_string();
                        &text_owned
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(e) => {
                        debug!("BiDi WS read error: {e}");
                        break;
                    }
                };
                let Some(conn_reader) = weak_conn.upgrade() else {
                    break;
                };
                conn_reader.handle_incoming(text_ref);
                drop(conn_reader);
            }
            if let Some(conn_reader) = weak_conn.upgrade() {
                conn_reader.fail_all_pending("BiDi WebSocket connection closed");
            }
        });

        {
            let mut tasks = conn.io_tasks.lock();
            tasks.push(write_handle);
            tasks.push(read_handle);
        }

        Ok(conn)
    }

    fn handle_incoming(self: &Arc<Self>, raw: &str) {
        let Ok(val) = serde_json::from_str::<Value>(raw) else {
            warn!("failed to parse BiDi JSON frame");
            return;
        };

        let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        match msg_type {
            "success" => {
                if let Some(id) = val.get("id").and_then(|v| v.as_u64())
                    && let Some(tx) = self.pending.lock().remove(&id)
                {
                    let result = val.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(result));
                }
            }
            "error" => {
                if let Some(id) = val.get("id").and_then(|v| v.as_u64())
                    && let Some(tx) = self.pending.lock().remove(&id)
                {
                    let err_kind = val
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let msg = val
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("BiDi protocol error");
                    let _ = tx.send(Err(MimikyuError::Protocol {
                        code: -1,
                        message: format!("{err_kind}: {msg}"),
                    }));
                }
            }
            "event" => {
                let Some(method) = val.get("method").and_then(|v| v.as_str()) else {
                    return;
                };
                let params = val.get("params").cloned().unwrap_or(Value::Null);

                if method == "browsingContext.contextCreated" {
                    if let (Some(cid), Some(parent)) = (
                        params.get("context").and_then(|v| v.as_str()),
                        params.get("parent").and_then(|v| v.as_str()),
                    ) {
                        self.context_parents
                            .write()
                            .insert(cid.to_string(), parent.to_string());
                    }
                    return;
                }
                if method == "browsingContext.contextDestroyed" {
                    if let Some(cid) = params.get("context").and_then(|v| v.as_str()) {
                        self.context_parents.write().remove(cid);
                    }
                    return;
                }

                let ctx_id = params
                    .get("context")
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        params
                            .get("source")
                            .and_then(|s| s.get("context"))
                            .and_then(|v| v.as_str())
                    });

                if let Some(state) = ctx_id.and_then(|cid| self.resolve_context_state(cid)) {
                    state.handle_event(self, method, params);
                } else if method == "network.beforeRequestSent"
                    && params.get("isBlocked").and_then(|v| v.as_bool()) == Some(true)
                    && let Some(req_id) = params
                        .get("request")
                        .and_then(|r| r.get("request"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string)
                {
                    let conn_clone = Arc::clone(self);
                    tokio::spawn(async move {
                        let _ = conn_clone
                            .send("network.continueRequest", json!({ "request": req_id }))
                            .await;
                    });
                }
            }
            _ => {}
        }
    }

    fn fail_all_pending(&self, reason: &str) {
        let mut pending = self.pending.lock();
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(MimikyuError::Transport(reason.to_string())));
        }
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            if self.closed.load(Ordering::SeqCst) {
                return Err(MimikyuError::Disposed);
            }
            pending.insert(id, tx);
        }

        let serialized = serde_json::to_string(&payload)?;
        if self.write_tx.send(serialized).is_err() {
            self.pending.lock().remove(&id);
            return Err(MimikyuError::Transport(
                "BiDi writer channel closed".to_string(),
            ));
        }

        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(MimikyuError::Transport(
                "BiDi response channel dropped".to_string(),
            )),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(MimikyuError::Timeout {
                    timeout_ms: RPC_TIMEOUT.as_millis() as u64,
                    operation: format!("BiDi {method}"),
                })
            }
        }
    }

    async fn shutdown_all(&self) {
        // IMPORTANT: Call browser.close directly without calling session.end first on the same WS.
        let _ = tokio::time::timeout(
            Duration::from_millis(2000),
            self.send("browser.close", json!({})),
        )
        .await;
        self.fail_all_pending("BiDi connection shut down");
        {
            let tasks = self.io_tasks.lock();
            for h in tasks.iter() {
                h.abort();
            }
        }
        let proc_opt = self.process.lock().await.take();
        if let Some(mut proc_guard) = proc_opt {
            let _ = proc_guard.shutdown(Duration::from_millis(1500)).await;
        }
    }
}

impl Drop for BidiConnection {
    fn drop(&mut self) {
        for h in self.io_tasks.get_mut().drain(..) {
            h.abort();
        }
    }
}

struct BidiContextState {
    user_context: Option<String>,
    owns_user_context: bool,
    is_root: bool,
    main_world_eval: bool,
    telemetry_capacity: Option<usize>,
    telemetry: Arc<TelemetryStore>,
    interceptor: Mutex<NetworkInterceptor>,
    env_headers: Mutex<BTreeMap<String, String>>,
    env_script_id: AsyncMutex<Option<String>>,
    pending_permissions: Mutex<Vec<PermissionGrant>>,
    applied_permissions: Mutex<Vec<(String, String)>>,
    pending_storage_scripts: Mutex<Vec<(String, String)>>,
    _proxy_relay: Mutex<Option<ProxyRelay>>,
    intercept_id: AsyncMutex<Option<String>>,
    load_notify: Notify,
    disposed: AtomicBool,
}

impl BidiContextState {
    fn handle_event(self: &Arc<Self>, conn: &Arc<BidiConnection>, method: &str, params: Value) {
        match method {
            "browsingContext.load" => {
                if let Some(u) = params.get("url").and_then(|v| v.as_str())
                    && let Some(origin) = extract_http_origin(u)
                {
                    let to_remove: Vec<String> = {
                        let mut pending = self.pending_storage_scripts.lock();
                        let mut matched = Vec::new();
                        pending.retain(|(orig, sid)| {
                            if orig == &origin {
                                matched.push(sid.clone());
                                false
                            } else {
                                true
                            }
                        });
                        matched
                    };
                    let perms_to_apply: Vec<PermissionGrant> = {
                        let pending = self.pending_permissions.lock();
                        pending
                            .iter()
                            .filter(|g| {
                                g.origin.is_none() || g.origin.as_deref() == Some(origin.as_str())
                            })
                            .cloned()
                            .collect()
                    };
                    if !to_remove.is_empty() || !perms_to_apply.is_empty() {
                        let conn_clone = Arc::clone(conn);
                        let state_clone = Arc::clone(self);
                        let uc = self.user_context.clone();
                        let orig_clone = origin.clone();
                        tokio::spawn(async move {
                            for sid in to_remove {
                                let _ = conn_clone
                                    .send("script.removePreloadScript", json!({ "script": sid }))
                                    .await;
                            }
                            for g in perms_to_apply {
                                let target_orig = g.origin.as_deref().unwrap_or(&orig_clone);
                                if send_bidi_permission(
                                    &conn_clone,
                                    &g.name,
                                    g.state,
                                    target_orig,
                                    uc.as_deref(),
                                )
                                .await
                                .is_ok()
                                {
                                    let mut applied = state_clone.applied_permissions.lock();
                                    let entry = (g.name, target_orig.to_string());
                                    if !applied.contains(&entry) {
                                        applied.push(entry);
                                    }
                                }
                            }
                        });
                    }
                }
                self.load_notify.notify_waiters();
            }
            "log.entryAdded" => {
                let entry_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let text = params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let timestamp_ms = params
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or_else(now_ms);
                let frames = params
                    .get("stackTrace")
                    .and_then(|st| st.get("callFrames"))
                    .and_then(|cf| cf.as_array());
                let first_frame = frames.and_then(|arr| arr.first());
                let url = first_frame
                    .and_then(|f| f.get("url"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
                let line_raw = first_frame
                    .and_then(|f| f.get("lineNumber"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);

                if entry_type == "javascript" {
                    let column = first_frame
                        .and_then(|f| f.get("columnNumber"))
                        .and_then(|v| v.as_u64())
                        .map(|n| n as u32 + 1);
                    let stack = frames
                        .map(|arr| {
                            arr.iter()
                                .map(|f| {
                                    let fn_name = f
                                        .get("functionName")
                                        .and_then(|v| v.as_str())
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or("<anonymous>");
                                    let u = f.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                    let ln =
                                        f.get("lineNumber").and_then(|v| v.as_u64()).unwrap_or(0)
                                            + 1;
                                    let col = f
                                        .get("columnNumber")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        + 1;
                                    format!("    at {fn_name} ({u}:{ln}:{col})")
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .filter(|s| !s.is_empty());
                    self.telemetry.push_page_error(PageError {
                        timestamp_ms,
                        message: text,
                        name: None,
                        stack,
                        url,
                        line: line_raw.map(|l| l + 1),
                        column,
                    });
                } else {
                    let level = params
                        .get("method")
                        .and_then(|v| v.as_str())
                        .or_else(|| params.get("level").and_then(|v| v.as_str()))
                        .unwrap_or("info")
                        .to_string();
                    self.telemetry.push_console(ConsoleEntry {
                        timestamp_ms,
                        level,
                        text,
                        url,
                        line: line_raw,
                    });
                }
            }
            "network.beforeRequestSent" => {
                let req = &params["request"];
                let Some(req_id) = req
                    .get("request")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                else {
                    return;
                };
                let url = req
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let http_method = req
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GET")
                    .to_string();
                let request_headers = bidi_headers_to_btree(req.get("headers"));
                let destination = req
                    .get("destination")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let initiator_type = params
                    .get("initiator")
                    .and_then(|i| i.get("type"))
                    .and_then(|v| v.as_str());
                let is_navigation = params
                    .get("navigation")
                    .and_then(|v| v.as_str())
                    .is_some();
                let resource_type_enum =
                    ResourceType::from_bidi_event(destination, initiator_type, is_navigation);
                let resource_type_str = if !destination.is_empty() {
                    destination.to_string()
                } else {
                    format!("{resource_type_enum:?}")
                };

                self.telemetry.on_request_started(
                    NetworkEvent {
                        timestamp_ms: now_ms(),
                        request_id: req_id.clone(),
                        url: url.clone(),
                        method: http_method,
                        resource_type: resource_type_str,
                        status: None,
                        mime_type: None,
                        failed: false,
                        error_text: None,
                        request_headers: request_headers.clone(),
                        response_headers: BTreeMap::new(),
                    },
                    None,
                    None,
                    BTreeMap::new(),
                );

                let is_blocked = params
                    .get("isBlocked")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_blocked {
                    let env_hdrs = self.env_headers.lock().clone();
                    let base_headers = if env_hdrs.is_empty() {
                        request_headers
                    } else {
                        merge_headers_case_insensitive(&request_headers, &env_hdrs)
                    };
                    let decision = self.interceptor.lock().decide(&InterceptRequest {
                        url: &url,
                        resource_type: resource_type_enum,
                        headers: &base_headers,
                    });
                    let has_env_hdrs = !env_hdrs.is_empty();
                    let conn_clone = Arc::clone(conn);
                    let telem = Arc::clone(&self.telemetry);

                    tokio::spawn(async move {
                        match decision {
                            InterceptDecision::Block { reason } => {
                                telem.on_intercept_resolved_locally(
                                    &req_id,
                                    None,
                                    BTreeMap::new(),
                                    true,
                                    Some(reason.to_string()),
                                );
                                let _ = conn_clone
                                    .send("network.failRequest", json!({ "request": req_id }))
                                    .await;
                            }
                            InterceptDecision::Fulfill {
                                status,
                                headers,
                                body,
                            } => {
                                telem.on_intercept_resolved_locally(
                                    &req_id,
                                    Some(status),
                                    headers.clone(),
                                    false,
                                    None,
                                );
                                let hdrs: Vec<Value> = headers
                                    .into_iter()
                                    .map(|(name, value)| {
                                        json!({
                                            "name": name,
                                            "value": { "type": "string", "value": value }
                                        })
                                    })
                                    .collect();
                                let mut resp_params = json!({
                                    "request": req_id,
                                    "statusCode": status,
                                    "reasonPhrase": http_reason_phrase(status),
                                    "headers": hdrs,
                                });
                                if !body.is_empty() {
                                    resp_params["body"] = json!({
                                        "type": "base64",
                                        "value": BASE64.encode(body),
                                    });
                                }
                                let _ = conn_clone
                                    .send("network.provideResponse", resp_params)
                                    .await;
                            }
                            InterceptDecision::ModifyHeaders { headers } => {
                                let hdrs = btree_to_bidi_headers(&headers);
                                if let Err(e) = conn_clone
                                    .send(
                                        "network.continueRequest",
                                        json!({ "request": req_id, "headers": hdrs }),
                                    )
                                    .await
                                {
                                    warn!(
                                        target: "mimikyu::bidi::network",
                                        "network.continueRequest(ModifyHeaders) failed: {e}"
                                    );
                                    let _ = conn_clone
                                        .send("network.continueRequest", json!({ "request": req_id }))
                                        .await;
                                }
                            }
                            InterceptDecision::Continue => {
                                if has_env_hdrs {
                                    let hdrs = btree_to_bidi_headers(&base_headers);
                                    if let Err(e) = conn_clone
                                        .send(
                                            "network.continueRequest",
                                            json!({ "request": req_id, "headers": hdrs }),
                                        )
                                        .await
                                    {
                                        warn!(
                                            target: "mimikyu::bidi::network",
                                            "network.continueRequest(env_headers) failed: {e}"
                                        );
                                        let _ = conn_clone
                                            .send(
                                                "network.continueRequest",
                                                json!({ "request": req_id }),
                                            )
                                            .await;
                                    }
                                } else {
                                    let _ = conn_clone
                                        .send("network.continueRequest", json!({ "request": req_id }))
                                        .await;
                                }
                            }
                        }
                    });
                }
            }
            "network.responseCompleted" => {
                let Some(req_id) = params["request"]["request"].as_str() else {
                    return;
                };
                let resp = &params["response"];
                let status = resp
                    .get("status")
                    .and_then(|v| v.as_u64())
                    .map(|s| s as u16);
                let mime_type = resp
                    .get("mimeType")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let response_headers = bidi_headers_to_btree(resp.get("headers"));
                self.telemetry
                    .on_response_received(req_id, status, mime_type, response_headers);
                self.telemetry.on_request_finished(req_id);
            }
            "network.fetchError" => {
                let Some(req_id) = params["request"]["request"].as_str() else {
                    return;
                };
                let error_text = params
                    .get("errorText")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                self.telemetry.on_request_failed(req_id, error_text);
            }
            _ => {}
        }
    }
}

pub struct BidiSession {
    conn: Arc<BidiConnection>,
    info: SessionInfo,
    state: Arc<BidiContextState>,
}

impl BidiSession {
    #[allow(clippy::too_many_arguments)]
    fn create(
        conn: Arc<BidiConnection>,
        info: SessionInfo,
        user_context: Option<String>,
        owns_user_context: bool,
        is_root: bool,
        main_world_eval: bool,
        telemetry_capacity: Option<usize>,
        proxy_relay: Option<ProxyRelay>,
    ) -> Arc<Self> {
        let telem_cfg = telemetry_capacity
            .map(TelemetryConfig::with_capacity)
            .unwrap_or_default();
        let state = Arc::new(BidiContextState {
            user_context,
            owns_user_context,
            is_root,
            main_world_eval,
            telemetry_capacity,
            telemetry: Arc::new(TelemetryStore::new(telem_cfg)),
            interceptor: Mutex::new(NetworkInterceptor::default()),
            env_headers: Mutex::new(BTreeMap::new()),
            env_script_id: AsyncMutex::new(None),
            pending_permissions: Mutex::new(Vec::new()),
            applied_permissions: Mutex::new(Vec::new()),
            pending_storage_scripts: Mutex::new(Vec::new()),
            _proxy_relay: Mutex::new(proxy_relay),
            intercept_id: AsyncMutex::new(None),
            load_notify: Notify::new(),
            disposed: AtomicBool::new(false),
        });

        conn.contexts
            .write()
            .insert(info.target_id.clone(), Arc::clone(&state));

        Arc::new(Self { conn, info, state })
    }

    async fn apply_initial_options(
        &self,
        env_profile: Option<EnvProfile>,
        permissions: Vec<PermissionGrant>,
        block_images: bool,
        block_media: bool,
        block_fonts: bool,
        instant_animations: bool,
    ) -> Result<()> {
        if let Some(profile) = env_profile {
            self.set_env_overrides(profile).await?;
        }
        if !permissions.is_empty() {
            self.set_permissions(permissions).await?;
        }

        if block_images || block_media || block_fonts {
            self.state
                .interceptor
                .lock()
                .set_categories(block_images, block_media, block_fonts);
            self.sync_intercept().await?;
        }
        if instant_animations {
            let _ = self.set_instant_animations(true).await;
        }
        Ok(())
    }

    fn ensure_alive(&self) -> Result<()> {
        if self.state.disposed.load(Ordering::SeqCst) {
            return Err(MimikyuError::Disposed);
        }
        Ok(())
    }

    fn ctx(&self) -> &str {
        &self.info.target_id
    }

    async fn sync_intercept(&self) -> Result<()> {
        let need_intercept = self.state.interceptor.lock().needs_bidi_intercept()
            || !self.state.env_headers.lock().is_empty();
        let mut guard = self.state.intercept_id.lock().await;

        if need_intercept && guard.is_none() {
            // Firefox URLPattern rejects unescaped '*'; use empty urlPatterns + Rust-side matching.
            let res = self
                .conn
                .send(
                    "network.addIntercept",
                    json!({
                        "phases": ["beforeRequestSent"],
                        "urlPatterns": [],
                        "contexts": [self.ctx()],
                    }),
                )
                .await?;
            let id = res["intercept"]
                .as_str()
                .ok_or_else(|| MimikyuError::Protocol {
                    code: -1,
                    message: "network.addIntercept missing intercept id".to_string(),
                })?
                .to_string();
            *guard = Some(id);
        } else if !need_intercept
            && let Some(id) = guard.take()
        {
            let _ = self
                .conn
                .send("network.removeIntercept", json!({ "intercept": id }))
                .await;
        }
        Ok(())
    }

    async fn collect_nav_info(&self) -> Result<NavInfo> {
        let url = self.current_url().await?;
        let title = self.title().await.unwrap_or_default();
        let status = self.state.telemetry.find_latest_status_for_url(&url);
        Ok(NavInfo {
            url,
            title,
            status,
            frame_id: Some(self.ctx().to_string()),
        })
    }

    async fn call_sandbox_fn(&self, fn_decl: &str, args: Vec<Value>) -> Result<Value> {
        self.ensure_alive()?;
        let res = self
            .conn
            .send(
                "script.callFunction",
                json!({
                    "functionDeclaration": fn_decl,
                    "target": { "context": self.ctx(), "sandbox": ISOLATED_SANDBOX },
                    "arguments": args,
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        parse_bidi_script_result(&res)
    }

    async fn resolve_shared_element(
        &self,
        backend_node_id: Option<i64>,
        selector: Option<&str>,
    ) -> Result<Value> {
        if let Some(sel) = selector {
            let res = self
                .conn
                .send(
                    "browsingContext.locateNodes",
                    json!({
                        "context": self.ctx(),
                        "locator": { "type": "css", "value": sel },
                        "maxNodeCount": 1,
                    }),
                )
                .await?;
            let node = res["nodes"]
                .as_array()
                .and_then(|arr| arr.first())
                .cloned()
                .ok_or_else(|| {
                    MimikyuError::ElementNotFound(format!(
                        "selector '{sel}' matched no element"
                    ))
                })?;
            return Ok(node);
        }

        if let Some(bid) = backend_node_id {
            let res = self
                .conn
                .send(
                    "script.callFunction",
                    json!({
                        "functionDeclaration": r#"(bid) => {
                            const m = globalThis.__mimikyuAxMap;
                            return m ? (m.get(bid) || null) : null;
                        }"#,
                        "target": { "context": self.ctx(), "sandbox": ISOLATED_SANDBOX },
                        "arguments": [{ "type": "number", "value": bid }],
                        "awaitPromise": false,
                        "resultOwnership": "root",
                    }),
                )
                .await?;
            if res.get("type").and_then(|v| v.as_str()) == Some("exception") {
                return Err(MimikyuError::ElementNotFound(format!(
                    "backend_node_id {bid} lookup failed"
                )));
            }
            let val = &res["result"];
            if val.get("type").and_then(|v| v.as_str()) == Some("node") {
                return Ok(val.clone());
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "backend_node_id {bid} not found in current document"
            )));
        }

        Err(MimikyuError::InvalidConfig(
            "requires either backend_node_id or selector".to_string(),
        ))
    }
}

#[async_trait]
impl DriverSession for BidiSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn capabilities(&self) -> DriverCapabilities {
        bidi_capabilities()
    }

    async fn new_context(&self, mut opts: ContextOptions) -> Result<Arc<dyn DriverSession>> {
        self.ensure_alive()?;
        let mut relay_opt: Option<ProxyRelay> = None;
        if let Some(ref proxy) = opts.proxy
            && proxy.needs_relay()
        {
            let relay = ProxyRelay::start(proxy).await?;
            let rewritten = relay.browser_proxy_config(proxy.bypass.clone());
            opts.proxy = Some(rewritten);
            relay_opt = Some(relay);
        }
        let uc = create_user_context(&self.conn, opts.proxy.as_ref()).await?;
        self.conn.isolated_contexts.write().insert(uc.clone());
        let created = self
            .conn
            .send(
                "browsingContext.create",
                json!({
                    "type": "window",
                    "userContext": uc,
                }),
            )
            .await?;
        let target_id = created["context"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "browsingContext.create missing context".to_string(),
            })?
            .to_string();
        self.conn.set_active_target(Some(&uc), &target_id);

        let mut info = self.info.clone();
        info.context_id = Some(uc.clone());
        info.target_id = target_id;

        let session = BidiSession::create(
            Arc::clone(&self.conn),
            info,
            Some(uc),
            true,
            false,
            opts.main_world_eval,
            opts.telemetry_capacity,
            relay_opt,
        );

        session
            .apply_initial_options(
                opts.env_profile,
                opts.permissions,
                opts.block_images,
                opts.block_media,
                opts.block_fonts,
                opts.instant_animations,
            )
            .await?;

        Ok(session)
    }

    async fn new_tab(&self, url: Option<&str>) -> Result<Arc<dyn DriverSession>> {
        self.ensure_alive()?;
        let mut params = json!({ "type": "window" });
        if let Some(ref uc) = self.state.user_context {
            params["userContext"] = json!(uc);
        }
        let created = self.conn.send("browsingContext.create", params).await?;
        let target_id = created["context"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "browsingContext.create missing context".to_string(),
            })?
            .to_string();

        let mut info = self.info.clone();
        info.target_id = target_id.clone();

        let session = BidiSession::create(
            Arc::clone(&self.conn),
            info,
            self.state.user_context.clone(),
            false,
            false,
            self.state.main_world_eval,
            self.state.telemetry_capacity,
            None,
        );
        self.conn
            .set_active_target(self.state.user_context.as_deref(), &target_id);

        if let Some(u) = url
            && u != "about:blank"
        {
            session.navigate(u).await?;
        }
        Ok(session)
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        self.ensure_alive()?;
        let tree = self.conn.send("browsingContext.getTree", json!({})).await?;
        let contexts = tree["contexts"].as_array().cloned().unwrap_or_default();
        let active_tid = self
            .conn
            .active_target_for(self.state.user_context.as_deref());
        let mut out = Vec::new();
        for c in contexts {
            let ctx_uc = c["userContext"].as_str();
            if let Some(ref uc) = self.state.user_context {
                if ctx_uc != Some(uc.as_str()) {
                    continue;
                }
            } else if let Some(uc) = ctx_uc
                && self.conn.isolated_contexts.read().contains(uc)
            {
                continue;
            }
            let tid = c["context"].as_str().unwrap_or_default().to_string();
            let title = match self
                .conn
                .send(
                    "script.evaluate",
                    json!({
                        "expression": "document.title",
                        "target": { "context": tid, "sandbox": ISOLATED_SANDBOX },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                )
                .await
            {
                Ok(res) => parse_bidi_script_result(&res)
                    .ok()
                    .and_then(|v| v.as_str().map(ToString::to_string))
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            out.push(TabInfo {
                target_id: tid,
                url: c["url"].as_str().unwrap_or_default().to_string(),
                title,
                active: false,
            });
        }
        if !out.is_empty() {
            let resolved_active = active_tid
                .filter(|aid| out.iter().any(|t| &t.target_id == aid))
                .unwrap_or_else(|| {
                    let self_tid = self.ctx();
                    let fallback = if out.iter().any(|t| t.target_id == self_tid) {
                        self_tid.to_string()
                    } else {
                        out[0].target_id.clone()
                    };
                    self.conn
                        .set_active_target(self.state.user_context.as_deref(), &fallback);
                    fallback
                });
            if let Some(tab) = out.iter_mut().find(|t| t.target_id == resolved_active) {
                tab.active = true;
            }
        }
        Ok(out)
    }

    async fn activate_tab(&self, target_id: &str) -> Result<()> {
        self.ensure_alive()?;
        self.conn
            .send("browsingContext.activate", json!({ "context": target_id }))
            .await?;
        self.conn
            .set_active_target(self.state.user_context.as_deref(), target_id);
        Ok(())
    }

    async fn navigate(&self, url: &str) -> Result<NavInfo> {
        self.ensure_alive()?;
        if let Some(origin) = extract_http_origin(url) {
            let pending = { self.state.pending_permissions.lock().clone() };
            for g in pending {
                if g.origin.is_none() || g.origin.as_deref() == Some(origin.as_str()) {
                    let target_orig = g.origin.as_deref().unwrap_or(&origin);
                    if send_bidi_permission(
                        &self.conn,
                        &g.name,
                        g.state,
                        target_orig,
                        self.state.user_context.as_deref(),
                    )
                    .await
                    .is_ok()
                    {
                        let mut applied = self.state.applied_permissions.lock();
                        let entry = (g.name, target_orig.to_string());
                        if !applied.contains(&entry) {
                            applied.push(entry);
                        }
                    }
                }
            }
        }
        self.conn
            .send(
                "browsingContext.navigate",
                json!({
                    "context": self.ctx(),
                    "url": url,
                    "wait": "complete",
                }),
            )
            .await
            .map_err(|e| MimikyuError::NavigationFailed(e.to_string()))?;
        self.collect_nav_info().await
    }

    async fn reload(&self) -> Result<NavInfo> {
        self.ensure_alive()?;
        self.conn
            .send(
                "browsingContext.reload",
                json!({
                    "context": self.ctx(),
                    "wait": "complete",
                }),
            )
            .await
            .map_err(|e| MimikyuError::NavigationFailed(e.to_string()))?;
        self.collect_nav_info().await
    }

    async fn go_back(&self) -> Result<NavInfo> {
        self.ensure_alive()?;
        let notified = self.state.load_notify.notified();
        self.conn
            .send(
                "browsingContext.traverseHistory",
                json!({ "context": self.ctx(), "delta": -1 }),
            )
            .await
            .map_err(|e| MimikyuError::NavigationFailed(e.to_string()))?;
        let _ = tokio::time::timeout(Duration::from_millis(500), notified).await;
        self.collect_nav_info().await
    }

    async fn go_forward(&self) -> Result<NavInfo> {
        self.ensure_alive()?;
        let notified = self.state.load_notify.notified();
        self.conn
            .send(
                "browsingContext.traverseHistory",
                json!({ "context": self.ctx(), "delta": 1 }),
            )
            .await
            .map_err(|e| MimikyuError::NavigationFailed(e.to_string()))?;
        let _ = tokio::time::timeout(Duration::from_millis(500), notified).await;
        self.collect_nav_info().await
    }

    async fn current_url(&self) -> Result<String> {
        let val = self.eval_isolated("window.location.href").await?;
        Ok(val.as_str().unwrap_or_default().to_string())
    }

    async fn title(&self) -> Result<String> {
        let val = self.eval_isolated("document.title").await?;
        Ok(val.as_str().unwrap_or_default().to_string())
    }

    async fn eval_isolated(&self, expr: &str) -> Result<Value> {
        self.ensure_alive()?;
        let res = self
            .conn
            .send(
                "script.evaluate",
                json!({
                    "expression": expr,
                    "target": { "context": self.ctx(), "sandbox": ISOLATED_SANDBOX },
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        parse_bidi_script_result(&res)
    }

    async fn eval_main_world(&self, expr: &str) -> Result<Value> {
        self.ensure_alive()?;
        if !self.state.main_world_eval {
            return Err(MimikyuError::MainWorldDisabled);
        }
        let res = self
            .conn
            .send(
                "script.evaluate",
                json!({
                    "expression": expr,
                    "target": { "context": self.ctx() },
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        parse_bidi_script_result(&res)
    }

    async fn add_init_script(&self, script: &str) -> Result<String> {
        self.ensure_alive()?;
        let fn_decl = format!("() => {{\n{script}\n}}");
        let res = self
            .conn
            .send(
                "script.addPreloadScript",
                json!({
                    "functionDeclaration": fn_decl,
                    "contexts": [self.ctx()],
                }),
            )
            .await?;
        res["script"]
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "script.addPreloadScript missing script id".to_string(),
            })
    }

    async fn remove_init_script(&self, script_id: &str) -> Result<()> {
        self.ensure_alive()?;
        self.conn
            .send("script.removePreloadScript", json!({ "script": script_id }))
            .await?;
        Ok(())
    }

    async fn accessibility_tree(&self) -> Result<Vec<AxNode>> {
        let walker_js = r#"(() => {
            const nodes = [];
            const map = new Map();
            let nextId = 1;
            globalThis.__mimikyuAxMap = map;

            const tagRole = (el) => {
                const explicit = el.getAttribute('role');
                if (explicit) return explicit.trim().split(/\s+/)[0];
                const tag = el.tagName;
                if (tag === 'A' && el.hasAttribute('href')) return 'link';
                if (tag === 'BUTTON') return 'button';
                if (/^H[1-6]$/.test(tag)) return 'heading';
                if (tag === 'INPUT') {
                    const t = (el.getAttribute('type') || 'text').toLowerCase();
                    if (t === 'checkbox') return 'checkbox';
                    if (t === 'radio') return 'radio';
                    if (t === 'submit' || t === 'button' || t === 'reset') return 'button';
                    if (t === 'hidden') return 'none';
                    return 'textbox';
                }
                if (tag === 'TEXTAREA') return 'textbox';
                if (tag === 'SELECT') return 'combobox';
                if (tag === 'IMG') return 'img';
                if (tag === 'NAV') return 'navigation';
                if (tag === 'MAIN') return 'main';
                if (tag === 'HEADER') return 'banner';
                if (tag === 'FOOTER') return 'contentinfo';
                if (tag === 'FORM') return 'form';
                if (tag === 'UL' || tag === 'OL') return 'list';
                if (tag === 'LI') return 'listitem';
                if (tag === 'TABLE') return 'table';
                if (tag === 'TR') return 'row';
                if (tag === 'TH') return 'columnheader';
                if (tag === 'TD') return 'cell';
                if (tag === 'BODY') return 'RootWebArea';
                return 'generic';
            };

            const computeName = (el) => {
                const al = el.getAttribute('aria-label');
                if (al && al.trim()) return al.trim();
                const labelledBy = el.getAttribute('aria-labelledby');
                if (labelledBy) {
                    const parts = labelledBy.split(/\s+/).map(id => {
                        const ref = document.getElementById(id);
                        return ref ? (ref.textContent || '').trim() : '';
                    }).filter(Boolean);
                    if (parts.length) return parts.join(' ');
                }
                if (el.id) {
                    const lbl = document.querySelector(`label[for="${CSS.escape(el.id)}"]`);
                    if (lbl && lbl.textContent) return lbl.textContent.trim();
                }
                if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
                    if (el.placeholder) return el.placeholder.trim();
                    const t = (el.getAttribute('type') || '').toLowerCase();
                    if ((t === 'submit' || t === 'button') && el.value) return el.value.trim();
                }
                if (el.tagName === 'IMG' && el.alt) return el.alt.trim();
                if (el.title) return el.title.trim();
                if (el.tagName !== 'BODY' && el.tagName !== 'MAIN' && el.tagName !== 'FORM') {
                    const txt = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
                    if (txt && txt.length <= 200) return txt;
                }
                return null;
            };

            const visit = (el, parentId) => {
                if (!el || el.nodeType !== 1) return null;
                const tag = el.tagName;
                if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT') return null;
                const style = getComputedStyle(el);
                if (style.display === 'none' || style.visibility === 'hidden') return null;
                if (el.getAttribute('aria-hidden') === 'true') return null;

                const role = tagRole(el);
                if (role === 'none' || role === 'presentation') return null;

                const bid = nextId++;
                const nodeId = String(bid);
                map.set(bid, el);

                const rect = el.getBoundingClientRect();
                const focusable = el.tabIndex >= 0 || ['A','BUTTON','INPUT','SELECT','TEXTAREA'].includes(tag);
                const focused = document.activeElement === el;
                const disabled = !!el.disabled || el.getAttribute('aria-disabled') === 'true';
                const checkedAttr = el.getAttribute('aria-checked');
                const checked = typeof el.checked === 'boolean' ? el.checked : (checkedAttr === 'true' ? true : checkedAttr === 'false' ? false : null);
                const expAttr = el.getAttribute('aria-expanded');
                const expanded = expAttr === 'true' ? true : expAttr === 'false' ? false : null;
                const selAttr = el.getAttribute('aria-selected');
                const selected = typeof el.selected === 'boolean' ? el.selected : (selAttr === 'true' ? true : selAttr === 'false' ? false : null);
                const required = !!el.required || el.getAttribute('aria-required') === 'true';
                const level = /^H([1-6])$/.test(tag) ? Number(tag.slice(1)) : (el.getAttribute('aria-level') ? Number(el.getAttribute('aria-level')) : null);
                const value = ('value' in el && el.value !== undefined && el.value !== null && tag !== 'BUTTON' && tag !== 'LI') ? String(el.value) : null;

                const idx = nodes.length;
                const entry = {
                    node_id: nodeId,
                    parent_id: parentId,
                    child_ids: [],
                    role,
                    name: computeName(el),
                    value,
                    description: el.getAttribute('aria-description') || null,
                    backend_node_id: bid,
                    frame_id: null,
                    ignored: false,
                    focusable,
                    focused,
                    disabled,
                    checked,
                    expanded,
                    selected,
                    required,
                    level,
                    bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
                };
                nodes.push(entry);

                const pushChildren = (root) => {
                    for (const child of root.children) {
                        const cid = visit(child, nodeId);
                        if (cid) entry.child_ids.push(cid);
                    }
                };
                pushChildren(el);
                if (el.shadowRoot) {
                    pushChildren(el.shadowRoot);
                }
                return nodeId;
            };

            if (document.body) {
                visit(document.body, null);
            }
            return nodes;
        })()"#;

        let val = self.eval_isolated(walker_js).await?;
        let arr = val.as_array().cloned().unwrap_or_default();
        let mut nodes = Vec::with_capacity(arr.len());
        for item in arr {
            if let Ok(node) = serde_json::from_value::<AxNode>(item) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    async fn resolve_node_box(
        &self,
        backend_node_id: i64,
        _frame_id: Option<&str>,
    ) -> Result<BoundingBox> {
        let val = self
            .call_sandbox_fn(
                r#"(bid) => {
                    const m = globalThis.__mimikyuAxMap;
                    const el = m ? m.get(bid) : null;
                    if (!el) return null;
                    const r = el.getBoundingClientRect();
                    return { x: r.x, y: r.y, width: r.width, height: r.height };
                }"#,
                vec![json!({ "type": "number", "value": backend_node_id })],
            )
            .await?;
        if val.is_null() {
            return Err(MimikyuError::ElementNotFound(format!(
                "backend_node_id {backend_node_id} not found"
            )));
        }
        serde_json::from_value::<BoundingBox>(val).map_err(|e| {
            MimikyuError::ElementNotFound(format!("invalid bounding box for {backend_node_id}: {e}"))
        })
    }

    async fn resolve_selector_box(&self, selector: &str) -> Result<BoundingBox> {
        let val = self
            .call_sandbox_fn(
                r#"(sel) => {
                    const el = document.querySelector(sel);
                    if (!el) return null;
                    const r = el.getBoundingClientRect();
                    return { x: r.x, y: r.y, width: r.width, height: r.height };
                }"#,
                vec![json!({ "type": "string", "value": selector })],
            )
            .await?;
        if val.is_null() {
            return Err(MimikyuError::ElementNotFound(format!(
                "selector '{selector}' matched no element"
            )));
        }
        serde_json::from_value::<BoundingBox>(val).map_err(|e| {
            MimikyuError::ElementNotFound(format!("invalid bounding box for '{selector}': {e}"))
        })
    }

    async fn focus_node(
        &self,
        backend_node_id: Option<i64>,
        selector: Option<&str>,
    ) -> Result<()> {
        if let Some(sel) = selector {
            let ok = self
                .call_sandbox_fn(
                    r#"(sel) => {
                        const el = document.querySelector(sel);
                        if (!el) return false;
                        el.focus();
                        return true;
                    }"#,
                    vec![json!({ "type": "string", "value": sel })],
                )
                .await?;
            if ok.as_bool() == Some(true) {
                return Ok(());
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "selector '{sel}' matched no element"
            )));
        }

        if let Some(bid) = backend_node_id {
            let ok = self
                .call_sandbox_fn(
                    r#"(bid) => {
                        const m = globalThis.__mimikyuAxMap;
                        const el = m ? m.get(bid) : null;
                        if (!el) return false;
                        el.focus();
                        return true;
                    }"#,
                    vec![json!({ "type": "number", "value": bid })],
                )
                .await?;
            if ok.as_bool() == Some(true) {
                return Ok(());
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "backend_node_id {bid} not found"
            )));
        }

        Err(MimikyuError::InvalidConfig(
            "focus_node requires either backend_node_id or selector".to_string(),
        ))
    }

    async fn set_file_input_files(
        &self,
        backend_node_id: Option<i64>,
        selector: Option<&str>,
        files: Vec<PathBuf>,
    ) -> Result<()> {
        let elem = self
            .resolve_shared_element(backend_node_id, selector)
            .await?;
        let file_strs: Vec<String> = files
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        self.conn
            .send(
                "input.setFiles",
                json!({
                    "context": self.ctx(),
                    "element": elem,
                    "files": file_strs,
                }),
            )
            .await?;
        Ok(())
    }

    async fn dispatch_mouse(&self, ev: MouseEvent) -> Result<()> {
        self.ensure_alive()?;
        let button = match ev.button {
            MouseButton::Left | MouseButton::None => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };
        let x = ev.x.round() as i64;
        let y = ev.y.round() as i64;
        let clicks = ev.click_count.max(1);

        let mut actions = Vec::new();
        match ev.event_type {
            MouseEventType::Move => {
                actions.push(json!({ "type": "pointerMove", "x": x, "y": y, "duration": 0 }));
            }
            MouseEventType::Down => {
                actions.push(json!({ "type": "pointerMove", "x": x, "y": y, "duration": 0 }));
                actions.push(json!({ "type": "pointerDown", "button": button }));
            }
            MouseEventType::Up => {
                actions.push(json!({ "type": "pointerMove", "x": x, "y": y, "duration": 0 }));
                actions.push(json!({ "type": "pointerUp", "button": button }));
            }
            MouseEventType::Click => {
                actions.push(json!({ "type": "pointerMove", "x": x, "y": y, "duration": 0 }));
                for _ in 0..clicks {
                    actions.push(json!({ "type": "pointerDown", "button": button }));
                    actions.push(json!({ "type": "pointerUp", "button": button }));
                }
            }
        }

        self.conn
            .send(
                "input.performActions",
                json!({
                    "context": self.ctx(),
                    "actions": [{
                        "type": "pointer",
                        "id": "mimikyu_mouse",
                        "parameters": { "pointerType": "mouse" },
                        "actions": actions,
                    }],
                }),
            )
            .await?;
        Ok(())
    }

    async fn dispatch_key(&self, ev: KeyEvent) -> Result<()> {
        self.ensure_alive()?;
        let val = map_webdriver_key(&ev.key);
        let actions = match ev.event_type {
            KeyEventType::Down => vec![json!({ "type": "keyDown", "value": val })],
            KeyEventType::Up => vec![json!({ "type": "keyUp", "value": val })],
            KeyEventType::Char => vec![
                json!({ "type": "keyDown", "value": val }),
                json!({ "type": "keyUp", "value": val }),
            ],
        };
        self.conn
            .send(
                "input.performActions",
                json!({
                    "context": self.ctx(),
                    "actions": [{
                        "type": "key",
                        "id": "mimikyu_keyboard",
                        "actions": actions,
                    }],
                }),
            )
            .await?;
        Ok(())
    }

    async fn dispatch_wheel(&self, ev: WheelEvent) -> Result<()> {
        self.ensure_alive()?;
        self.conn
            .send(
                "input.performActions",
                json!({
                    "context": self.ctx(),
                    "actions": [{
                        "type": "wheel",
                        "id": "mimikyu_wheel",
                        "actions": [{
                            "type": "scroll",
                            "x": ev.x.round() as i64,
                            "y": ev.y.round() as i64,
                            "deltaX": ev.delta_x.round() as i64,
                            "deltaY": ev.delta_y.round() as i64,
                        }],
                    }],
                }),
            )
            .await?;
        Ok(())
    }

    async fn insert_text(&self, text: &str) -> Result<()> {
        self.ensure_alive()?;
        if text.is_empty() {
            return Ok(());
        }
        let mut actions = Vec::with_capacity(text.chars().count() * 2);
        for ch in text.chars() {
            let s = ch.to_string();
            actions.push(json!({ "type": "keyDown", "value": s }));
            actions.push(json!({ "type": "keyUp", "value": s }));
        }
        self.conn
            .send(
                "input.performActions",
                json!({
                    "context": self.ctx(),
                    "actions": [{
                        "type": "key",
                        "id": "mimikyu_keyboard",
                        "actions": actions,
                    }],
                }),
            )
            .await?;
        Ok(())
    }

    async fn screenshot(&self, opts: ScreenshotOpts) -> Result<Vec<u8>> {
        self.ensure_alive()?;
        let mut params = json!({
            "context": self.ctx(),
            "origin": if opts.full_page { "document" } else { "viewport" },
        });
        params["format"] = match opts.format {
            ScreenshotFormat::Png => json!({ "type": "image/png" }),
            ScreenshotFormat::Jpeg { quality } => {
                let q = (f64::from(quality) / 100.0).clamp(0.0, 1.0);
                json!({ "type": "image/jpeg", "quality": q })
            }
        };
        if let Some(clip) = opts.clip {
            params["clip"] = json!({
                "type": "box",
                "x": clip.x,
                "y": clip.y,
                "width": clip.width,
                "height": clip.height,
            });
        }
        let res = self
            .conn
            .send("browsingContext.captureScreenshot", params)
            .await?;
        let b64 = res["data"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "browsingContext.captureScreenshot missing data".to_string(),
            })?;
        BASE64
            .decode(b64)
            .map_err(|e| MimikyuError::Protocol {
                code: -1,
                message: format!("invalid base64 screenshot data: {e}"),
            })
    }

    async fn pdf(&self) -> Result<Vec<u8>> {
        self.ensure_alive()?;
        let res = self
            .conn
            .send(
                "browsingContext.print",
                json!({
                    "context": self.ctx(),
                    "background": true,
                }),
            )
            .await?;
        let b64 = res["data"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "browsingContext.print missing data".to_string(),
            })?;
        BASE64
            .decode(b64)
            .map_err(|e| MimikyuError::Protocol {
                code: -1,
                message: format!("invalid base64 PDF data: {e}"),
            })
    }

    async fn set_instant_animations(&self, enabled: bool) -> Result<()> {
        self.call_sandbox_fn(
            r#"(enabled) => {
                const id = '__mimikyu_instant_anim';
                let st = document.getElementById(id);
                if (enabled) {
                    if (!st && document.head) {
                        st = document.createElement('style');
                        st.id = id;
                        st.textContent = '*, *::before, *::after { transition-duration: 0s !important; animation-duration: 0s !important; animation-delay: 0s !important; }';
                        document.head.appendChild(st);
                    }
                    for (const anim of document.getAnimations()) {
                        try { anim.finish(); } catch {}
                    }
                } else if (st) {
                    st.remove();
                }
            }"#,
            vec![json!({ "type": "boolean", "value": enabled })],
        )
        .await?;
        Ok(())
    }

    async fn set_env_overrides(&self, profile: EnvProfile) -> Result<()> {
        self.ensure_alive()?;
        self.conn
            .send(
                "browsingContext.setViewport",
                json!({
                    "context": self.ctx(),
                    "viewport": {
                        "width": profile.viewport_width,
                        "height": profile.viewport_height,
                    },
                    "devicePixelRatio": profile.device_scale_factor,
                }),
            )
            .await?;

        if !profile.timezone.is_empty() {
            self.conn
                .send(
                    "emulation.setTimezoneOverride",
                    json!({
                        "timezone": profile.timezone,
                        "contexts": [self.ctx()],
                    }),
                )
                .await?;
        }
        if !profile.locale.is_empty() {
            self.conn
                .send(
                    "emulation.setLocaleOverride",
                    json!({
                        "locale": profile.locale,
                        "contexts": [self.ctx()],
                    }),
                )
                .await?;
        }
        if let Some(ref geo) = profile.geolocation {
            self.conn
                .send(
                    "emulation.setGeolocationOverride",
                    json!({
                        "coordinates": {
                            "latitude": geo.latitude,
                            "longitude": geo.longitude,
                            "accuracy": geo.accuracy,
                        },
                        "contexts": [self.ctx()],
                    }),
                )
                .await?;
        }

        let win_script = build_window_init_script(&profile, EngineKind::Firefox);
        {
            let mut prev = self.state.env_script_id.lock().await;
            if let Some(old_id) = prev.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            let new_id = self.add_init_script(&win_script).await?;
            *prev = Some(new_id);
        }

        {
            let mut hdrs = BTreeMap::new();
            if !profile.user_agent.is_empty() {
                hdrs.insert("User-Agent".to_string(), profile.user_agent.clone());
            }
            if !profile.accept_language.is_empty() {
                hdrs.insert(
                    "Accept-Language".to_string(),
                    profile.accept_language.clone(),
                );
            }
            if !profile.extra_headers.is_empty() {
                hdrs = merge_headers_case_insensitive(&hdrs, &profile.extra_headers);
            }
            *self.state.env_headers.lock() = hdrs;
        }
        self.sync_intercept().await?;

        if profile.geolocation.is_some() {
            let _ = self
                .set_permissions(vec![PermissionGrant {
                    name: "geolocation".to_string(),
                    state: PermissionState::Granted,
                    origin: None,
                }])
                .await;
        }

        Ok(())
    }

    async fn set_permissions(&self, permissions: Vec<PermissionGrant>) -> Result<()> {
        self.ensure_alive()?;
        let current_origin = self
            .current_url()
            .await
            .ok()
            .and_then(|u| extract_http_origin(&u));

        for g in permissions {
            if let Some(ref orig) = g.origin {
                send_bidi_permission(
                    &self.conn,
                    &g.name,
                    g.state,
                    orig,
                    self.state.user_context.as_deref(),
                )
                .await?;
                let mut applied = self.state.applied_permissions.lock();
                let entry = (g.name.clone(), orig.clone());
                if !applied.contains(&entry) {
                    applied.push(entry);
                }
            } else {
                {
                    let mut pending = self.state.pending_permissions.lock();
                    pending.retain(|p| p.name != g.name || p.origin.is_some());
                    pending.push(g.clone());
                }
                if let Some(ref cur_orig) = current_origin {
                    send_bidi_permission(
                        &self.conn,
                        &g.name,
                        g.state,
                        cur_orig,
                        self.state.user_context.as_deref(),
                    )
                    .await?;
                    let mut applied = self.state.applied_permissions.lock();
                    let entry = (g.name.clone(), cur_orig.clone());
                    if !applied.contains(&entry) {
                        applied.push(entry);
                    }
                }
            }
        }
        Ok(())
    }

    async fn reset_permissions(&self) -> Result<()> {
        self.ensure_alive()?;
        self.state.pending_permissions.lock().clear();
        let applied: Vec<(String, String)> = {
            let mut guard = self.state.applied_permissions.lock();
            guard.drain(..).collect()
        };
        for (name, origin) in applied {
            let _ = send_bidi_permission(
                &self.conn,
                &name,
                PermissionState::Prompt,
                &origin,
                self.state.user_context.as_deref(),
            )
            .await;
        }
        Ok(())
    }

    async fn set_block_policy(&self, policy: BlockPolicy) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().set_block_policy(policy)?;
        self.sync_intercept().await
    }

    async fn set_blocked_urls(&self, patterns: Vec<String>) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().set_blocked_urls(patterns)?;
        self.sync_intercept().await
    }

    async fn set_route_mock(&self, mock: RouteMock) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().add_mock(mock)?;
        self.sync_intercept().await
    }

    async fn set_route_rule(&self, rule: RouteRule) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().add_rule(rule)?;
        self.sync_intercept().await
    }

    async fn clear_route_mocks(&self) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().clear_rules();
        self.sync_intercept().await
    }

    async fn cookies(&self) -> Result<Vec<Cookie>> {
        self.ensure_alive()?;
        let partition = match self.state.user_context {
            Some(ref uc) => json!({ "type": "storageKey", "userContext": uc }),
            None => json!({ "type": "storageKey" }),
        };
        let res = self
            .conn
            .send("storage.getCookies", json!({ "partition": partition }))
            .await?;
        let arr = res["cookies"].as_array().cloned().unwrap_or_default();
        Ok(arr
            .into_iter()
            .map(|c| Cookie {
                name: c["name"].as_str().unwrap_or_default().to_string(),
                value: c["value"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                domain: c["domain"].as_str().unwrap_or_default().to_string(),
                path: c["path"].as_str().unwrap_or("/").to_string(),
                expires: c["expiry"].as_f64(),
                http_only: c["httpOnly"].as_bool().unwrap_or(false),
                secure: c["secure"].as_bool().unwrap_or(false),
                same_site: c["sameSite"].as_str().map(ToString::to_string),
            })
            .collect())
    }

    async fn set_cookies(&self, cookies: Vec<Cookie>) -> Result<()> {
        self.ensure_alive()?;
        for c in cookies {
            let mut cookie_obj = json!({
                "name": c.name,
                "value": { "type": "string", "value": c.value },
                "domain": c.domain,
                "path": if c.path.is_empty() { "/" } else { &c.path },
                "httpOnly": c.http_only,
                "secure": c.secure,
            });
            if let Some(exp) = c.expires
                && exp.is_finite()
                && exp > 0.0
            {
                cookie_obj["expiry"] = json!(exp as u64);
            }
            if let Some(ss) = c.same_site.as_deref().and_then(normalize_bidi_same_site) {
                cookie_obj["sameSite"] = json!(ss);
            }
            let partition = match self.state.user_context {
                Some(ref uc) => json!({ "type": "storageKey", "userContext": uc }),
                None => json!({ "type": "storageKey" }),
            };
            self.conn
                .send(
                    "storage.setCookie",
                    json!({
                        "cookie": cookie_obj,
                        "partition": partition,
                    }),
                )
                .await?;
        }
        Ok(())
    }

    async fn clear_cookies(&self) -> Result<()> {
        self.ensure_alive()?;
        let partition = match self.state.user_context {
            Some(ref uc) => json!({ "type": "storageKey", "userContext": uc }),
            None => json!({ "type": "storageKey" }),
        };
        self.conn
            .send("storage.deleteCookies", json!({ "partition": partition }))
            .await?;
        Ok(())
    }

    async fn storage_state(&self) -> Result<StorageState> {
        let cookies = self.cookies().await?;
        let mut origins = Vec::new();
        let val = self
            .eval_isolated(
                r#"(() => {
                    const o = window.location.origin;
                    if (!o || o === 'null') return null;
                    const ls = {};
                    for (let i = 0; i < localStorage.length; i++) {
                        const k = localStorage.key(i);
                        ls[k] = localStorage.getItem(k);
                    }
                    const ss = {};
                    for (let i = 0; i < sessionStorage.length; i++) {
                        const k = sessionStorage.key(i);
                        ss[k] = sessionStorage.getItem(k);
                    }
                    return { origin: o, localStorage: ls, sessionStorage: ss };
                })()"#,
            )
            .await
            .unwrap_or(Value::Null);

        if let Some(origin) = val.get("origin").and_then(|v| v.as_str()) {
            let local_storage = val
                .get("localStorage")
                .and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v.clone()).ok())
                .unwrap_or_default();
            let session_storage = val
                .get("sessionStorage")
                .and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v.clone()).ok())
                .unwrap_or_default();
            origins.push(OriginStorage {
                origin: origin.to_string(),
                local_storage,
                session_storage,
            });
        }

        Ok(StorageState { cookies, origins })
    }

    async fn set_storage_state(&self, state: StorageState) -> Result<()> {
        self.set_cookies(state.cookies).await?;
        let current_origin = self
            .eval_isolated("window.location.origin")
            .await
            .ok()
            .and_then(|v| v.as_str().map(ToString::to_string));

        for entry in state.origins {
            if entry.local_storage.is_empty() && entry.session_storage.is_empty() {
                continue;
            }
            let ls_json = serde_json::to_string(&entry.local_storage)?;
            let ss_json = serde_json::to_string(&entry.session_storage)?;
            if current_origin.as_deref() == Some(entry.origin.as_str())
                && entry.origin != "null"
            {
                let script = format!(
                    r#"(() => {{
                        const ls = {ls_json};
                        for (const [k, v] of Object.entries(ls)) localStorage.setItem(k, v);
                        const ss = {ss_json};
                        for (const [k, v] of Object.entries(ss)) sessionStorage.setItem(k, v);
                    }})()"#
                );
                self.eval_isolated(&script).await?;
            } else {
                let origin_json = serde_json::to_string(&entry.origin)?;
                let preload = format!(
                    r#"(() => {{
                        if (window.location.origin !== {origin_json}) return;
                        const ls = {ls_json};
                        for (const [k, v] of Object.entries(ls)) localStorage.setItem(k, v);
                        const ss = {ss_json};
                        for (const [k, v] of Object.entries(ss)) sessionStorage.setItem(k, v);
                    }})()"#
                );
                let sid = self.add_init_script(&preload).await?;
                self.state
                    .pending_storage_scripts
                    .lock()
                    .push((entry.origin, sid));
            }
        }
        Ok(())
    }

    async fn console_logs(&self) -> Result<Vec<ConsoleEntry>> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.snapshot_console())
    }

    async fn page_errors(&self) -> Result<Vec<PageError>> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.snapshot_page_errors())
    }

    async fn network_events(&self) -> Result<Vec<NetworkEvent>> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.snapshot_network())
    }

    async fn drain_console_logs(&self) -> Result<Vec<ConsoleEntry>> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.drain_console())
    }

    async fn drain_page_errors(&self) -> Result<Vec<PageError>> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.drain_page_errors())
    }

    async fn drain_network_events(&self) -> Result<Vec<NetworkEvent>> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.drain_network())
    }

    async fn clear_telemetry(&self) -> Result<()> {
        self.ensure_alive()?;
        self.state.telemetry.clear();
        Ok(())
    }

    async fn inflight_requests(&self) -> Result<usize> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.inflight_count())
    }

    async fn document_ready_state(&self) -> Result<String> {
        let val = self.eval_isolated("document.readyState").await?;
        Ok(val.as_str().unwrap_or("loading").to_string())
    }

    async fn dispose(&self) -> Result<()> {
        if self.state.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.conn.contexts.write().remove(self.ctx());

        if self.state.is_root {
            self.conn.shutdown_all().await;
        } else if self.state.owns_user_context
            && let Some(ref uc) = self.state.user_context
        {
            let mut removed_cids = Vec::new();
            self.conn.contexts.write().retain(|cid, st| {
                if st.user_context.as_deref() == Some(uc.as_str()) {
                    st.disposed.store(true, Ordering::SeqCst);
                    removed_cids.push(cid.clone());
                    false
                } else {
                    true
                }
            });
            if !removed_cids.is_empty() {
                let mut parents = self.conn.context_parents.write();
                parents.retain(|cid, parent| {
                    !removed_cids.contains(cid) && !removed_cids.contains(parent)
                });
            }
            self.conn.isolated_contexts.write().remove(uc);
            self.conn
                .active_targets
                .write()
                .remove(&context_scope_key(Some(uc)));
            let _ = self
                .conn
                .send("browser.removeUserContext", json!({ "userContext": uc }))
                .await;
        } else {
            self.conn
                .clear_active_target_if_matches(self.state.user_context.as_deref(), self.ctx());
            let _ = self
                .conn
                .send("browsingContext.close", json!({ "context": self.ctx() }))
                .await;
        }
        Ok(())
    }
}

fn extract_http_origin(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let orig = parsed.origin().ascii_serialization();
    if orig == "null" {
        None
    } else {
        Some(orig)
    }
}

async fn send_bidi_permission(
    conn: &BidiConnection,
    name: &str,
    state: PermissionState,
    origin: &str,
    user_context: Option<&str>,
) -> Result<()> {
    let state_str = match state {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Prompt => "prompt",
    };
    let mut params = json!({
        "descriptor": { "name": name },
        "state": state_str,
        "origin": origin,
    });
    if let Some(uc) = user_context {
        params["userContext"] = json!(uc);
    }
    conn.send("permissions.setPermission", params).await?;
    Ok(())
}

fn btree_to_bidi_headers(headers: &BTreeMap<String, String>) -> Vec<Value> {
    headers
        .iter()
        .map(|(name, value)| {
            json!({
                "name": name,
                "value": { "type": "string", "value": value }
            })
        })
        .collect()
}

fn parse_bidi_script_result(res: &Value) -> Result<Value> {
    if res.get("type").and_then(|v| v.as_str()) == Some("exception") {
        let text = res["exceptionDetails"]["text"]
            .as_str()
            .unwrap_or("JavaScript exception")
            .to_string();
        return Err(MimikyuError::EvalFailed(text));
    }
    Ok(bidi_remote_value_to_json(&res["result"]))
}

fn bidi_remote_value_to_json(val: &Value) -> Value {
    let t = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match t {
        "undefined" | "null" => Value::Null,
        "boolean" => val.get("value").cloned().unwrap_or(Value::Null),
        "string" => val.get("value").cloned().unwrap_or(Value::Null),
        "number" => {
            if let Some(n) = val.get("value") {
                if n.is_number() {
                    return n.clone();
                }
                if let Some(s) = n.as_str() {
                    if s == "-0" {
                        return json!(0);
                    }
                    return Value::String(s.to_string());
                }
            }
            Value::Null
        }
        "bigint" => val
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
        "array" | "set" => {
            let items = val["value"]
                .as_array()
                .map(|arr| arr.iter().map(bidi_remote_value_to_json).collect())
                .unwrap_or_default();
            Value::Array(items)
        }
        "object" | "map" => {
            let mut obj = serde_json::Map::new();
            if let Some(pairs) = val["value"].as_array() {
                for pair in pairs {
                    if let Some(pair_arr) = pair.as_array()
                        && pair_arr.len() == 2
                    {
                        let key = if let Some(s) = pair_arr[0].as_str() {
                            s.to_string()
                        } else {
                            bidi_remote_value_to_json(&pair_arr[0])
                                .as_str()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| pair_arr[0].to_string())
                        };
                        obj.insert(key, bidi_remote_value_to_json(&pair_arr[1]));
                    }
                }
            }
            Value::Object(obj)
        }
        _ => val.get("value").cloned().unwrap_or(Value::Null),
    }
}

fn bidi_headers_to_btree(val: Option<&Value>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(arr) = val.and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                let value = item
                    .get("value")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                map.insert(name.to_string(), value);
            }
        }
    }
    map
}

fn map_webdriver_key(key: &str) -> String {
    match key {
        "Cancel" => "\u{E001}".to_string(),
        "Help" => "\u{E002}".to_string(),
        "Backspace" => "\u{E003}".to_string(),
        "Tab" => "\u{E004}".to_string(),
        "Clear" => "\u{E005}".to_string(),
        "Return" => "\u{E006}".to_string(),
        "Enter" => "\u{E007}".to_string(),
        "Shift" => "\u{E008}".to_string(),
        "Control" => "\u{E009}".to_string(),
        "Alt" => "\u{E00A}".to_string(),
        "Pause" => "\u{E00B}".to_string(),
        "Escape" => "\u{E00C}".to_string(),
        "Space" => "\u{E00D}".to_string(),
        "PageUp" => "\u{E00E}".to_string(),
        "PageDown" => "\u{E00F}".to_string(),
        "End" => "\u{E010}".to_string(),
        "Home" => "\u{E011}".to_string(),
        "ArrowLeft" => "\u{E012}".to_string(),
        "ArrowUp" => "\u{E013}".to_string(),
        "ArrowRight" => "\u{E014}".to_string(),
        "ArrowDown" => "\u{E015}".to_string(),
        "Insert" => "\u{E016}".to_string(),
        "Delete" => "\u{E017}".to_string(),
        "F1" => "\u{E031}".to_string(),
        "F2" => "\u{E032}".to_string(),
        "F3" => "\u{E033}".to_string(),
        "F4" => "\u{E034}".to_string(),
        "F5" => "\u{E035}".to_string(),
        "F6" => "\u{E036}".to_string(),
        "F7" => "\u{E037}".to_string(),
        "F8" => "\u{E038}".to_string(),
        "F9" => "\u{E039}".to_string(),
        "F10" => "\u{E03A}".to_string(),
        "F11" => "\u{E03B}".to_string(),
        "F12" => "\u{E03C}".to_string(),
        "Meta" => "\u{E03D}".to_string(),
        other => other.to_string(),
    }
}

fn normalize_bidi_same_site(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "strict" => Some("strict"),
        "lax" => Some("lax"),
        "none" => Some("none"),
        "default" => Some("default"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bidi_same_site_validates_known_values() {
        assert_eq!(normalize_bidi_same_site("Strict"), Some("strict"));
        assert_eq!(normalize_bidi_same_site(" LAX "), Some("lax"));
        assert_eq!(normalize_bidi_same_site("none"), Some("none"));
        assert_eq!(normalize_bidi_same_site("Default"), Some("default"));
        assert_eq!(normalize_bidi_same_site("bogus"), None);
    }

    #[test]
    fn map_webdriver_key_covers_function_navigation_and_printable_keys() {
        assert_eq!(map_webdriver_key("F1"), "\u{E031}");
        assert_eq!(map_webdriver_key("F2"), "\u{E032}");
        assert_eq!(map_webdriver_key("F3"), "\u{E033}");
        assert_eq!(map_webdriver_key("F4"), "\u{E034}");
        assert_eq!(map_webdriver_key("F5"), "\u{E035}");
        assert_eq!(map_webdriver_key("F6"), "\u{E036}");
        assert_eq!(map_webdriver_key("F7"), "\u{E037}");
        assert_eq!(map_webdriver_key("F8"), "\u{E038}");
        assert_eq!(map_webdriver_key("F9"), "\u{E039}");
        assert_eq!(map_webdriver_key("F10"), "\u{E03A}");
        assert_eq!(map_webdriver_key("F11"), "\u{E03B}");
        assert_eq!(map_webdriver_key("F12"), "\u{E03C}");
        assert_eq!(map_webdriver_key("Enter"), "\u{E007}");
        assert_eq!(map_webdriver_key("Space"), "\u{E00D}");
        assert_eq!(map_webdriver_key("Meta"), "\u{E03D}");
        assert_eq!(map_webdriver_key("a"), "a");
        assert_eq!(map_webdriver_key("é"), "é");
    }
}
