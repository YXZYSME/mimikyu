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
    PageError, PermissionGrant, PermissionState, RouteMock, ScreenshotFormat,
    ScreenshotOpts, SessionInfo, StorageState, TabInfo, WheelEvent,
};
use crate::env_profile::{
    build_window_init_script, build_worker_init_script, ProxyRelay,
};
use crate::error::{MimikyuError, Result};
use crate::network::{
    BlockPolicy, InterceptDecision, InterceptRequest, NetworkInterceptor, ResourceType, RouteRule,
    TelemetryConfig, TelemetryStore,
};
use crate::process::{chromium_proxy_bypass_list, ProcessGuard};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub struct CdpDriver;

impl CdpDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CdpDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BrowserDriver for CdpDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Chromium
    }

    fn capabilities(&self) -> DriverCapabilities {
        cdp_capabilities()
    }

    async fn launch(&self, mut opts: LaunchOptions) -> Result<Arc<dyn DriverSession>> {
        if opts.engine != EngineKind::Chromium {
            return Err(MimikyuError::InvalidConfig(format!(
                "CdpDriver cannot launch engine '{}'",
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
        let ws_url = process.ws_endpoint().to_string();
        let browser_version = process.browser_version().to_string();
        let profile_dir = process.profile_dir().to_path_buf();
        let pid = process.pid();

        let conn = CdpConnection::connect(&ws_url, process, root_relay).await?;

        // Enable target discovery
        let _ = conn
            .send_browser("Target.setDiscoverTargets", json!({ "discover": true }))
            .await;

        let (context_id, owns_context) = match opts.profile_mode {
            crate::driver::ProfileMode::VirtualContext => {
                let mut params = json!({});
                if let Some(ref proxy) = opts.proxy {
                    params["proxyServer"] =
                        json!(crate::process::normalize_chromium_proxy_server(&proxy.server));
                    params["proxyBypassList"] = json!(chromium_proxy_bypass_list(&proxy.bypass));
                }
                let res = conn
                    .send_browser("Target.createBrowserContext", params)
                    .await?;
                let ctx_id = res["browserContextId"]
                    .as_str()
                    .ok_or_else(|| {
                        MimikyuError::Protocol {
                            code: -1,
                            message: "Target.createBrowserContext missing browserContextId"
                                .to_string(),
                        }
                    })?
                    .to_string();
                conn.isolated_contexts.write().insert(ctx_id.clone());
                (Some(ctx_id), true)
            }
            _ => (None, false),
        };

        let target_id = if let Some(ref ctx_id) = context_id {
            let res = conn
                .send_browser(
                    "Target.createTarget",
                    json!({ "url": "about:blank", "browserContextId": ctx_id }),
                )
                .await?;
            res["targetId"]
                .as_str()
                .ok_or_else(|| MimikyuError::Protocol {
                    code: -1,
                    message: "Target.createTarget missing targetId".to_string(),
                })?
                .to_string()
        } else {
            // Find existing initial about:blank page target, or create one.
            let targets_res = conn.send_browser("Target.getTargets", json!({})).await?;
            let existing = targets_res["targetInfos"]
                .as_array()
                .and_then(|arr| arr.iter().find(|t| t["type"].as_str() == Some("page")))
                .and_then(|t| t["targetId"].as_str())
                .map(ToString::to_string);
            if let Some(tid) = existing {
                tid
            } else {
                let res = conn
                    .send_browser("Target.createTarget", json!({ "url": "about:blank" }))
                    .await?;
                res["targetId"]
                    .as_str()
                    .ok_or_else(|| MimikyuError::Protocol {
                        code: -1,
                        message: "Target.createTarget missing targetId".to_string(),
                    })?
                    .to_string()
            }
        };

        let attach_res = conn
            .send_browser(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let cdp_session_id = attach_res["sessionId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.attachToTarget missing sessionId".to_string(),
            })?
            .to_string();

        conn.set_active_target(context_id.as_deref(), &target_id);

        let info = SessionInfo {
            session_id: cdp_session_id.clone(),
            engine: EngineKind::Chromium,
            browser_version,
            context_id: context_id.clone(),
            target_id: target_id.clone(),
            profile_dir: Some(profile_dir),
            ws_endpoint: ws_url,
            pid: Some(pid),
        };

        let session = CdpSession::create(
            conn,
            info,
            context_id,
            owns_context,
            true,
            opts.main_world_eval,
            opts.telemetry_capacity,
            None,
        )
        .await?;

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

fn cdp_capabilities() -> DriverCapabilities {
    DriverCapabilities {
        engine: EngineKind::Chromium,
        protocol: "cdp".to_string(),
        supports_virtual_context: true,
        supports_per_context_proxy: true,
        supports_native_ax_tree: true,
        supports_isolated_world: true,
        supports_pdf: true,
        supports_network_intercept: true,
    }
}

struct CdpConnection {
    next_id: AtomicU64,
    write_tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    sessions: RwLock<HashMap<String, Arc<CdpSessionState>>>,
    isolated_contexts: RwLock<HashSet<String>>,
    active_targets: RwLock<HashMap<String, String>>,
    process: AsyncMutex<Option<ProcessGuard>>,
    _root_proxy_relay: Mutex<Option<ProxyRelay>>,
    closed: AtomicBool,
    io_tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn context_scope_key(context_id: Option<&str>) -> String {
    context_id.unwrap_or("__default__").to_string()
}

impl CdpConnection {
    fn set_active_target(&self, context_id: Option<&str>, target_id: &str) {
        self.active_targets
            .write()
            .insert(context_scope_key(context_id), target_id.to_string());
    }

    fn active_target_for(&self, context_id: Option<&str>) -> Option<String> {
        self.active_targets
            .read()
            .get(&context_scope_key(context_id))
            .cloned()
    }

    fn clear_active_target_if_matches(&self, context_id: Option<&str>, target_id: &str) {
        let key = context_scope_key(context_id);
        let mut guard = self.active_targets.write();
        if guard.get(&key).map(String::as_str) == Some(target_id) {
            guard.remove(&key);
        }
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
            .map_err(|e| MimikyuError::Transport(format!("CDP WebSocket connect failed: {e}")))?;

        let (mut sink, mut stream) = ws_stream.split();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();

        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            write_tx,
            pending: Mutex::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
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
                        debug!("CDP WS read error: {e}");
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
                conn_reader.fail_all_pending("CDP WebSocket connection closed");
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
            warn!("failed to parse CDP JSON frame");
            return;
        };

        if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
            let sender = self.pending.lock().remove(&id);
            if let Some(tx) = sender {
                if let Some(err) = val.get("error") {
                    let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let message = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown CDP error")
                        .to_string();
                    let _ = tx.send(Err(MimikyuError::Protocol { code, message }));
                } else {
                    let result = val.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(result));
                }
            }
            return;
        }

        // Event frame
        let Some(method) = val.get("method").and_then(|v| v.as_str()) else {
            return;
        };
        let params = val.get("params").cloned().unwrap_or(Value::Null);
        let session_id = val.get("sessionId").and_then(|v| v.as_str());

        if let Some(sid) = session_id {
            let state = self.sessions.read().get(sid).cloned();
            if let Some(state) = state {
                state.handle_event(self, method, params);
            } else if method == "Target.attachedToTarget"
                && params.get("waitingForDebugger").and_then(|v| v.as_bool()) == Some(true)
                && let Some(child_sid) = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            {
                let conn_clone = Arc::clone(self);
                tokio::spawn(async move {
                    let _ = conn_clone
                        .send_raw(Some(&child_sid), "Runtime.runIfWaitingForDebugger", json!({}))
                        .await;
                });
            }
        } else if method == "Target.attachedToTarget"
            && params.get("waitingForDebugger").and_then(|v| v.as_bool()) == Some(true)
            && let Some(child_sid) = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
        {
            let conn_clone = Arc::clone(self);
            tokio::spawn(async move {
                let _ = conn_clone
                    .send_raw(Some(&child_sid), "Runtime.runIfWaitingForDebugger", json!({}))
                    .await;
            });
        }
    }

    fn fail_all_pending(&self, reason: &str) {
        let mut pending = self.pending.lock();
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(MimikyuError::Transport(reason.to_string())));
        }
    }

    async fn send_raw(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(sid) = session_id {
            payload["sessionId"] = json!(sid);
        }

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
                "CDP writer channel closed".to_string(),
            ));
        }

        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(MimikyuError::Transport(
                "CDP response channel dropped".to_string(),
            )),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(MimikyuError::Timeout {
                    timeout_ms: RPC_TIMEOUT.as_millis() as u64,
                    operation: format!("CDP {method}"),
                })
            }
        }
    }

    async fn send_browser(&self, method: &str, params: Value) -> Result<Value> {
        self.send_raw(None, method, params).await
    }

    async fn shutdown_all(&self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(1500),
            self.send_browser("Browser.close", json!({})),
        )
        .await;
        self.fail_all_pending("CDP session shut down");
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

impl Drop for CdpConnection {
    fn drop(&mut self) {
        for h in self.io_tasks.get_mut().drain(..) {
            h.abort();
        }
        // ProcessGuard inside `self.process` will run its synchronous Drop cleanup here.
    }
}

struct CdpSessionState {
    session_id: String,
    target_id: String,
    context_id: Option<String>,
    owns_context: bool,
    is_root: bool,
    main_world_eval: bool,
    telemetry_capacity: Option<usize>,
    main_frame_id: Mutex<Option<String>>,
    last_main_origin: Mutex<Option<String>>,
    isolated_worlds: Mutex<HashMap<String, i64>>,
    telemetry: TelemetryStore,
    interceptor: Mutex<NetworkInterceptor>,
    fetch_enabled: AtomicBool,
    env_script_id: AsyncMutex<Option<String>>,
    worker_init_script: Mutex<Option<String>>,
    worker_ua_params: Mutex<Option<Value>>,
    pending_storage_scripts: Mutex<Vec<(String, String)>>,
    _proxy_relay: Mutex<Option<ProxyRelay>>,
    load_notify: Notify,
    disposed: AtomicBool,
}

impl CdpSessionState {
    fn handle_event(&self, conn: &Arc<CdpConnection>, method: &str, params: Value) {
        match method {
            "Page.frameNavigated" => {
                if let Some(frame) = params.get("frame")
                    && let Some(fid) = frame.get("id").and_then(|v| v.as_str())
                {
                    self.isolated_worlds.lock().remove(fid);
                    if frame.get("parentId").and_then(|v| v.as_str()).is_none() {
                        *self.main_frame_id.lock() = Some(fid.to_string());
                        if let Some(u) = frame.get("url").and_then(|v| v.as_str())
                            && let Ok(parsed) = url::Url::parse(u)
                        {
                            let origin = parsed.origin().ascii_serialization();
                            if origin != "null" {
                                *self.last_main_origin.lock() = Some(origin);
                            }
                        }
                    }
                }
            }
            "Runtime.executionContextsCleared" => {
                self.isolated_worlds.lock().clear();
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(ctx_id) = params.get("executionContextId").and_then(|v| v.as_i64()) {
                    self.isolated_worlds.lock().retain(|_, &mut v| v != ctx_id);
                }
            }
            "Page.loadEventFired" | "Page.frameStoppedLoading" => {
                if method == "Page.loadEventFired"
                    && let Some(cur_origin) = self.last_main_origin.lock().clone()
                {
                    let to_remove: Vec<String> = {
                        let mut pending = self.pending_storage_scripts.lock();
                        let mut matched = Vec::new();
                        pending.retain(|(orig, sid)| {
                            if orig == &cur_origin {
                                matched.push(sid.clone());
                                false
                            } else {
                                true
                            }
                        });
                        matched
                    };
                    if !to_remove.is_empty() {
                        let conn_clone = Arc::clone(conn);
                        let sid = self.session_id.clone();
                        tokio::spawn(async move {
                            for script_id in to_remove {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&sid),
                                        "Page.removeScriptToEvaluateOnNewDocument",
                                        json!({ "identifier": script_id }),
                                    )
                                    .await;
                            }
                        });
                    }
                }
                self.load_notify.notify_waiters();
            }
            "Target.attachedToTarget" => {
                let target_type = params
                    .get("targetInfo")
                    .and_then(|t| t.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let waiting = params
                    .get("waitingForDebugger")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(child_sid) = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                {
                    let is_worker = matches!(
                        target_type,
                        "worker" | "shared_worker" | "service_worker"
                    );
                    let worker_script = if is_worker {
                        self.worker_init_script.lock().clone()
                    } else {
                        None
                    };
                    let worker_ua = if is_worker {
                        self.worker_ua_params.lock().clone()
                    } else {
                        None
                    };
                    if is_worker || waiting {
                        let conn_clone = Arc::clone(conn);
                        tokio::spawn(async move {
                            if let Some(ua) = worker_ua {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Emulation.setUserAgentOverride",
                                        ua,
                                    )
                                    .await;
                            }
                            if let Some(js) = worker_script {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Runtime.evaluate",
                                        json!({
                                            "expression": js,
                                            "silent": true,
                                        }),
                                    )
                                    .await;
                            }
                            if waiting {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Runtime.runIfWaitingForDebugger",
                                        json!({}),
                                    )
                                    .await;
                            }
                        });
                    }
                }
            }
            "Runtime.consoleAPICalled" => {
                let level = params
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("log")
                    .to_string();
                let text = params
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(format_cdp_remote_object)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let frame = params
                    .get("stackTrace")
                    .and_then(|st| st.get("callFrames"))
                    .and_then(|cf| cf.as_array())
                    .and_then(|arr| arr.first());
                let url = frame
                    .and_then(|f| f.get("url"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
                let line = frame
                    .and_then(|f| f.get("lineNumber"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let timestamp_ms = params
                    .get("timestamp")
                    .and_then(|v| v.as_f64())
                    .map(|t| t as u64)
                    .unwrap_or_else(now_ms);

                self.telemetry.push_console(ConsoleEntry {
                    timestamp_ms,
                    level,
                    text,
                    url,
                    line,
                });
            }
            "Runtime.exceptionThrown" => {
                let timestamp_ms = params
                    .get("timestamp")
                    .and_then(|v| v.as_f64())
                    .map(|t| t as u64)
                    .unwrap_or_else(now_ms);
                let details = &params["exceptionDetails"];
                let exc = &details["exception"];
                let name = exc
                    .get("className")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let stack = exc
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                    .or_else(|| {
                        details
                            .get("stackTrace")
                            .and_then(|st| st.get("callFrames"))
                            .and_then(|cf| cf.as_array())
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
                            .filter(|s| !s.is_empty())
                    });
                let message = exc
                    .get("description")
                    .and_then(|v| v.as_str())
                    .and_then(|d| d.lines().next())
                    .or_else(|| details.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("Uncaught exception")
                    .to_string();
                let url = details
                    .get("url")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
                let line = details
                    .get("lineNumber")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32 + 1);
                let column = details
                    .get("columnNumber")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32 + 1);

                self.telemetry.push_page_error(PageError {
                    timestamp_ms,
                    message,
                    name,
                    stack,
                    url,
                    line,
                    column,
                });
            }
            "Network.requestWillBeSent" => {
                let Some(req_id) = params.get("requestId").and_then(|v| v.as_str()) else {
                    return;
                };
                let req = &params["request"];
                let url = req
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let method = req
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GET")
                    .to_string();
                let resource_type = params
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Other")
                    .to_string();
                let request_headers = json_map_to_btree(req.get("headers"));

                let redir = params.get("redirectResponse");
                let redir_status = redir
                    .and_then(|r| r.get("status"))
                    .and_then(|v| v.as_u64())
                    .map(|s| s as u16);
                let redir_mime = redir
                    .and_then(|r| r.get("mimeType"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let redir_headers = json_map_to_btree(redir.and_then(|r| r.get("headers")));

                self.telemetry.on_request_started(
                    NetworkEvent {
                        timestamp_ms: now_ms(),
                        request_id: req_id.to_string(),
                        url,
                        method,
                        resource_type,
                        status: None,
                        mime_type: None,
                        failed: false,
                        error_text: None,
                        request_headers,
                        response_headers: BTreeMap::new(),
                    },
                    redir_status,
                    redir_mime,
                    redir_headers,
                );
            }
            "Network.responseReceived" => {
                let Some(req_id) = params.get("requestId").and_then(|v| v.as_str()) else {
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
                let response_headers = json_map_to_btree(resp.get("headers"));
                self.telemetry
                    .on_response_received(req_id, status, mime_type, response_headers);
            }
            "Network.loadingFinished" => {
                let Some(req_id) = params.get("requestId").and_then(|v| v.as_str()) else {
                    return;
                };
                self.telemetry.on_request_finished(req_id);
            }
            "Network.loadingFailed" => {
                let Some(req_id) = params.get("requestId").and_then(|v| v.as_str()) else {
                    return;
                };
                let error_text = params
                    .get("errorText")
                    .and_then(|v| v.as_str())
                    .or_else(|| params.get("blockedReason").and_then(|v| v.as_str()))
                    .map(ToString::to_string);
                self.telemetry.on_request_failed(req_id, error_text);
            }
            "Fetch.requestPaused" => {
                let Some(fetch_id) = params
                    .get("requestId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                else {
                    return;
                };
                let url = params
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let req_headers =
                    json_map_to_btree(params.get("request").and_then(|r| r.get("headers")));
                let resource_type_raw = params
                    .get("resourceType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Other");
                let resource_type = ResourceType::from_cdp(resource_type_raw);

                let decision = self.interceptor.lock().decide(&InterceptRequest {
                    url: &url,
                    resource_type,
                    headers: &req_headers,
                });

                let conn_clone = Arc::clone(conn);
                let sid = self.session_id.clone();

                tokio::spawn(async move {
                    match decision {
                        InterceptDecision::Block { reason } => {
                            let _ = conn_clone
                                .send_raw(
                                    Some(&sid),
                                    "Fetch.failRequest",
                                    json!({ "requestId": fetch_id, "errorReason": reason }),
                                )
                                .await;
                        }
                        InterceptDecision::Fulfill {
                            status,
                            headers,
                            body,
                        } => {
                            let hdrs: Vec<Value> = headers
                                .into_iter()
                                .map(|(name, value)| json!({ "name": name, "value": value }))
                                .collect();
                            let _ = conn_clone
                                .send_raw(
                                    Some(&sid),
                                    "Fetch.fulfillRequest",
                                    json!({
                                        "requestId": fetch_id,
                                        "responseCode": status,
                                        "responseHeaders": hdrs,
                                        "body": BASE64.encode(body),
                                    }),
                                )
                                .await;
                        }
                        InterceptDecision::ModifyHeaders { headers } => {
                            let hdrs: Vec<Value> = headers
                                .into_iter()
                                .map(|(name, value)| json!({ "name": name, "value": value }))
                                .collect();
                            if let Err(e) = conn_clone
                                .send_raw(
                                    Some(&sid),
                                    "Fetch.continueRequest",
                                    json!({ "requestId": fetch_id, "headers": hdrs }),
                                )
                                .await
                            {
                                warn!(target: "mimikyu::cdp::fetch", "Fetch.continueRequest(ModifyHeaders) failed: {e}");
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&sid),
                                        "Fetch.continueRequest",
                                        json!({ "requestId": fetch_id }),
                                    )
                                    .await;
                            }
                        }
                        InterceptDecision::Continue => {
                            let _ = conn_clone
                                .send_raw(
                                    Some(&sid),
                                    "Fetch.continueRequest",
                                    json!({ "requestId": fetch_id }),
                                )
                                .await;
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

pub struct CdpSession {
    conn: Arc<CdpConnection>,
    info: SessionInfo,
    state: Arc<CdpSessionState>,
}

impl CdpSession {
    #[allow(clippy::too_many_arguments)]
    async fn create(
        conn: Arc<CdpConnection>,
        info: SessionInfo,
        context_id: Option<String>,
        owns_context: bool,
        is_root: bool,
        main_world_eval: bool,
        telemetry_capacity: Option<usize>,
        proxy_relay: Option<ProxyRelay>,
    ) -> Result<Arc<Self>> {
        let telem_cfg = telemetry_capacity
            .map(TelemetryConfig::with_capacity)
            .unwrap_or_default();
        let state = Arc::new(CdpSessionState {
            session_id: info.session_id.clone(),
            target_id: info.target_id.clone(),
            context_id,
            owns_context,
            is_root,
            main_world_eval,
            telemetry_capacity,
            main_frame_id: Mutex::new(None),
            last_main_origin: Mutex::new(None),
            isolated_worlds: Mutex::new(HashMap::new()),
            telemetry: TelemetryStore::new(telem_cfg),
            interceptor: Mutex::new(NetworkInterceptor::default()),
            fetch_enabled: AtomicBool::new(false),
            env_script_id: AsyncMutex::new(None),
            worker_init_script: Mutex::new(None),
            worker_ua_params: Mutex::new(None),
            pending_storage_scripts: Mutex::new(Vec::new()),
            _proxy_relay: Mutex::new(proxy_relay),
            load_notify: Notify::new(),
            disposed: AtomicBool::new(false),
        });

        conn.sessions
            .write()
            .insert(info.session_id.clone(), Arc::clone(&state));

        let session = Arc::new(Self { conn, info, state });

        let (page_res, runtime_res, dom_res, net_res, _ax_res, frame_tree_res, _attach_res) = tokio::join!(
            session.call("Page.enable", json!({})),
            session.call("Runtime.enable", json!({})),
            session.call("DOM.enable", json!({})),
            session.call("Network.enable", json!({})),
            session.call("Accessibility.enable", json!({})),
            session.call("Page.getFrameTree", json!({})),
            session.call(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true,
                }),
            ),
        );

        page_res?;
        runtime_res?;
        dom_res?;
        net_res?;

        if let Ok(tree) = frame_tree_res
            && let Some(fid) = tree["frameTree"]["frame"]["id"].as_str()
        {
            *session.state.main_frame_id.lock() = Some(fid.to_string());
        }

        Ok(session)
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
            self.sync_cdp_network().await?;
        }
        if instant_animations {
            let _ = self.set_instant_animations(true).await;
        }
        Ok(())
    }

    async fn sync_cdp_network(&self) -> Result<()> {
        self.ensure_alive()?;
        let (globs, fetch_patterns) = {
            let guard = self.state.interceptor.lock();
            (
                guard.blocker().cdp_blocked_url_globs(),
                guard.cdp_fetch_patterns(),
            )
        };

        self.call("Network.setBlockedURLs", json!({ "urls": globs }))
            .await?;

        if let Some(patterns) = fetch_patterns {
            self.call("Fetch.enable", json!({ "patterns": patterns }))
                .await?;
            self.state.fetch_enabled.store(true, Ordering::SeqCst);
        } else if self.state.fetch_enabled.swap(false, Ordering::SeqCst) {
            let _ = self.call("Fetch.disable", json!({})).await;
        }
        Ok(())
    }

    fn ensure_alive(&self) -> Result<()> {
        if self.state.disposed.load(Ordering::SeqCst) {
            return Err(MimikyuError::Disposed);
        }
        Ok(())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.ensure_alive()?;
        self.conn
            .send_raw(Some(&self.info.session_id), method, params)
            .await
    }

    async fn main_frame_id(&self) -> Result<String> {
        if let Some(fid) = self.state.main_frame_id.lock().clone() {
            return Ok(fid);
        }
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let fid = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Page.getFrameTree missing frame.id".to_string(),
            })?
            .to_string();
        *self.state.main_frame_id.lock() = Some(fid.clone());
        Ok(fid)
    }

    async fn get_or_create_isolated_context(&self) -> Result<(String, i64)> {
        let frame_id = self.main_frame_id().await?;
        if let Some(ctx_id) = self.state.isolated_worlds.lock().get(&frame_id).copied() {
            return Ok((frame_id, ctx_id));
        }
        let res = self
            .call(
                "Page.createIsolatedWorld",
                json!({
                    "frameId": frame_id,
                    "worldName": "mimikyu_isolated",
                    "grantUniveralAccess": true,
                }),
            )
            .await?;
        let ctx_id = res["executionContextId"]
            .as_i64()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Page.createIsolatedWorld missing executionContextId".to_string(),
            })?;
        self.state
            .isolated_worlds
            .lock()
            .insert(frame_id.clone(), ctx_id);
        Ok((frame_id, ctx_id))
    }

    async fn wait_for_load_complete(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let notified = self.state.load_notify.notified();
            match self.document_ready_state().await {
                Ok(state) if state == "complete" => return Ok(()),
                Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => return Err(e),
                _ => {}
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(MimikyuError::Timeout {
                    timeout_ms: 30_000,
                    operation: "waiting for document.readyState == 'complete'".to_string(),
                });
            }
            let step = remaining.min(Duration::from_millis(100));
            let _ = tokio::time::timeout(step, notified).await;
        }
    }

    async fn collect_nav_info(&self, frame_id: Option<String>) -> Result<NavInfo> {
        let url = self.current_url().await?;
        let title = self.title().await.unwrap_or_default();
        let status = self.state.telemetry.find_latest_status_for_url(&url);
        Ok(NavInfo {
            url,
            title,
            status,
            frame_id,
        })
    }

    async fn query_backend_node_id(&self, selector: &str) -> Result<i64> {
        let doc = self.call("DOM.getDocument", json!({ "depth": 0 })).await?;
        let root_id = doc["root"]["nodeId"]
            .as_i64()
            .ok_or_else(|| MimikyuError::ElementNotFound(selector.to_string()))?;
        let q = self
            .call(
                "DOM.querySelector",
                json!({ "nodeId": root_id, "selector": selector }),
            )
            .await?;
        let node_id = q["nodeId"].as_i64().unwrap_or(0);
        if node_id == 0 {
            return Err(MimikyuError::ElementNotFound(format!(
                "selector '{selector}' matched no element"
            )));
        }
        let desc = self
            .call("DOM.describeNode", json!({ "nodeId": node_id }))
            .await?;
        desc["node"]["backendNodeId"]
            .as_i64()
            .ok_or_else(|| MimikyuError::ElementNotFound(selector.to_string()))
    }
}

#[async_trait]
impl DriverSession for CdpSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn capabilities(&self) -> DriverCapabilities {
        cdp_capabilities()
    }

    async fn new_context(&self, opts: ContextOptions) -> Result<Arc<dyn DriverSession>> {
        self.ensure_alive()?;
        let mut relay_opt: Option<ProxyRelay> = None;
        let mut params = json!({});
        if let Some(ref proxy) = opts.proxy {
            let effective = if proxy.needs_relay() {
                let relay = ProxyRelay::start(proxy).await?;
                let cfg = relay.browser_proxy_config(proxy.bypass.clone());
                relay_opt = Some(relay);
                cfg
            } else {
                proxy.clone()
            };
            params["proxyServer"] = json!(crate::process::normalize_chromium_proxy_server(&effective.server));
            params["proxyBypassList"] = json!(chromium_proxy_bypass_list(&effective.bypass));
        }
        let res = self
            .conn
            .send_browser("Target.createBrowserContext", params)
            .await?;
        let ctx_id = res["browserContextId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.createBrowserContext missing browserContextId".to_string(),
            })?
            .to_string();
        self.conn.isolated_contexts.write().insert(ctx_id.clone());

        let tgt_res = self
            .conn
            .send_browser(
                "Target.createTarget",
                json!({ "url": "about:blank", "browserContextId": ctx_id }),
            )
            .await?;
        let target_id = tgt_res["targetId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.createTarget missing targetId".to_string(),
            })?
            .to_string();
        self.conn.set_active_target(Some(&ctx_id), &target_id);

        let attach_res = self
            .conn
            .send_browser(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let cdp_session_id = attach_res["sessionId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.attachToTarget missing sessionId".to_string(),
            })?
            .to_string();

        let mut info = self.info.clone();
        info.session_id = cdp_session_id;
        info.context_id = Some(ctx_id.clone());
        info.target_id = target_id;

        let session = CdpSession::create(
            Arc::clone(&self.conn),
            info,
            Some(ctx_id),
            true,
            false,
            opts.main_world_eval,
            opts.telemetry_capacity,
            relay_opt,
        )
        .await?;

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
        let initial_url = url.unwrap_or("about:blank");
        let mut create_params = json!({ "url": initial_url });
        if let Some(ref ctx_id) = self.state.context_id {
            create_params["browserContextId"] = json!(ctx_id);
        }
        let tgt_res = self
            .conn
            .send_browser("Target.createTarget", create_params)
            .await?;
        let target_id = tgt_res["targetId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.createTarget missing targetId".to_string(),
            })?
            .to_string();

        let attach_res = self
            .conn
            .send_browser(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let cdp_session_id = attach_res["sessionId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.attachToTarget missing sessionId".to_string(),
            })?
            .to_string();

        let mut info = self.info.clone();
        info.session_id = cdp_session_id;
        info.target_id = target_id.clone();

        let session = CdpSession::create(
            Arc::clone(&self.conn),
            info,
            self.state.context_id.clone(),
            false,
            false,
            self.state.main_world_eval,
            self.state.telemetry_capacity,
            None,
        )
        .await?;
        self.conn
            .set_active_target(self.state.context_id.as_deref(), &target_id);

        if initial_url != "about:blank" {
            let _ = session.wait_for_load_complete().await;
        }
        Ok(session)
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        self.ensure_alive()?;
        let res = self.conn.send_browser("Target.getTargets", json!({})).await?;
        let targets = res["targetInfos"].as_array().cloned().unwrap_or_default();
        let active_tid = self
            .conn
            .active_target_for(self.state.context_id.as_deref());
        let mut out = Vec::new();
        for t in targets {
            if t["type"].as_str() != Some("page") {
                continue;
            }
            let target_ctx = t["browserContextId"].as_str();
            if let Some(ref ctx_id) = self.state.context_id {
                if target_ctx != Some(ctx_id.as_str()) {
                    continue;
                }
            } else if let Some(tc) = target_ctx
                && self.conn.isolated_contexts.read().contains(tc)
            {
                continue;
            }
            let target_id = t["targetId"].as_str().unwrap_or_default().to_string();
            out.push(TabInfo {
                target_id,
                url: t["url"].as_str().unwrap_or_default().to_string(),
                title: t["title"].as_str().unwrap_or_default().to_string(),
                active: false,
            });
        }
        if !out.is_empty() {
            let resolved_active = active_tid
                .filter(|aid| out.iter().any(|t| &t.target_id == aid))
                .unwrap_or_else(|| {
                    let fallback = if out.iter().any(|t| t.target_id == self.state.target_id) {
                        self.state.target_id.clone()
                    } else {
                        out[0].target_id.clone()
                    };
                    self.conn
                        .set_active_target(self.state.context_id.as_deref(), &fallback);
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
            .send_browser("Target.activateTarget", json!({ "targetId": target_id }))
            .await?;
        self.conn
            .set_active_target(self.state.context_id.as_deref(), target_id);
        Ok(())
    }

    async fn navigate(&self, url: &str) -> Result<NavInfo> {
        self.ensure_alive()?;
        let notified = self.state.load_notify.notified();
        let res = self
            .call("Page.navigate", json!({ "url": url }))
            .await?;
        if let Some(err_text) = res.get("errorText").and_then(|v| v.as_str()) {
            return Err(MimikyuError::NavigationFailed(err_text.to_string()));
        }
        let frame_id = res
            .get("frameId")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        if let Some(ref fid) = frame_id {
            self.state.isolated_worlds.lock().remove(fid);
            *self.state.main_frame_id.lock() = Some(fid.clone());
        }
        if res.get("loaderId").is_some() {
            let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
            self.wait_for_load_complete().await?;
        }
        self.collect_nav_info(frame_id).await
    }

    async fn reload(&self) -> Result<NavInfo> {
        self.ensure_alive()?;
        let notified = self.state.load_notify.notified();
        self.call("Page.reload", json!({})).await?;
        self.state.isolated_worlds.lock().clear();
        let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
        self.wait_for_load_complete().await?;
        let fid = self.state.main_frame_id.lock().clone();
        self.collect_nav_info(fid).await
    }

    async fn go_back(&self) -> Result<NavInfo> {
        self.ensure_alive()?;
        let hist = self.call("Page.getNavigationHistory", json!({})).await?;
        let idx = hist["currentIndex"].as_i64().unwrap_or(0);
        let entries = hist["entries"].as_array().ok_or_else(|| {
            MimikyuError::NavigationFailed("empty navigation history".to_string())
        })?;
        if idx <= 0 {
            return Err(MimikyuError::NavigationFailed(
                "cannot go back: already at earliest history entry".to_string(),
            ));
        }
        let entry_id = entries[(idx - 1) as usize]["id"].as_i64().ok_or_else(|| {
            MimikyuError::NavigationFailed("missing history entryId".to_string())
        })?;
        let notified = self.state.load_notify.notified();
        self.call(
            "Page.navigateToHistoryEntry",
            json!({ "entryId": entry_id }),
        )
        .await?;
        self.state.isolated_worlds.lock().clear();
        let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
        self.wait_for_load_complete().await?;
        let fid = self.state.main_frame_id.lock().clone();
        self.collect_nav_info(fid).await
    }

    async fn go_forward(&self) -> Result<NavInfo> {
        self.ensure_alive()?;
        let hist = self.call("Page.getNavigationHistory", json!({})).await?;
        let idx = hist["currentIndex"].as_i64().unwrap_or(0);
        let entries = hist["entries"].as_array().ok_or_else(|| {
            MimikyuError::NavigationFailed("empty navigation history".to_string())
        })?;
        if idx < 0 || ((idx + 1) as usize) >= entries.len() {
            return Err(MimikyuError::NavigationFailed(
                "cannot go forward: already at latest history entry".to_string(),
            ));
        }
        let entry_id = entries[(idx + 1) as usize]["id"].as_i64().ok_or_else(|| {
            MimikyuError::NavigationFailed("missing history entryId".to_string())
        })?;
        let notified = self.state.load_notify.notified();
        self.call(
            "Page.navigateToHistoryEntry",
            json!({ "entryId": entry_id }),
        )
        .await?;
        self.state.isolated_worlds.lock().clear();
        let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
        self.wait_for_load_complete().await?;
        let fid = self.state.main_frame_id.lock().clone();
        self.collect_nav_info(fid).await
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
        for attempt in 0..2 {
            let (frame_id, ctx_id) = self.get_or_create_isolated_context().await?;
            let res = self
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": expr,
                        "contextId": ctx_id,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                )
                .await;

            match res {
                Ok(v) => return parse_eval_result(v),
                Err(MimikyuError::Protocol { ref message, .. })
                    if attempt == 0 && message.contains("Cannot find context") =>
                {
                    self.state.isolated_worlds.lock().remove(&frame_id);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(MimikyuError::EvalFailed(
            "failed to evaluate in isolated world after context refresh".to_string(),
        ))
    }

    async fn eval_main_world(&self, expr: &str) -> Result<Value> {
        self.ensure_alive()?;
        if !self.state.main_world_eval {
            return Err(MimikyuError::MainWorldDisabled);
        }
        let res = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        parse_eval_result(res)
    }

    async fn add_init_script(&self, script: &str) -> Result<String> {
        let res = self
            .call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": script }),
            )
            .await?;
        res["identifier"]
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Page.addScriptToEvaluateOnNewDocument missing identifier".to_string(),
            })
    }

    async fn remove_init_script(&self, script_id: &str) -> Result<()> {
        self.call(
            "Page.removeScriptToEvaluateOnNewDocument",
            json!({ "identifier": script_id }),
        )
        .await?;
        Ok(())
    }

    async fn accessibility_tree(&self) -> Result<Vec<AxNode>> {
        let res = self.call("Accessibility.getFullAXTree", json!({})).await?;
        let raw_nodes = res["nodes"].as_array().cloned().unwrap_or_default();
        let mut nodes = Vec::with_capacity(raw_nodes.len());

        for n in raw_nodes {
            let node_id = n["nodeId"].as_str().unwrap_or_default().to_string();
            let parent_id = n["parentId"].as_str().map(ToString::to_string);
            let child_ids = n["childIds"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(ToString::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let role = n["role"]["value"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            let name = n["name"]["value"]
                .as_str()
                .map(ToString::to_string);
            let value = n["value"]["value"]
                .as_str()
                .map(ToString::to_string)
                .or_else(|| n["value"]["value"].as_f64().map(|f| f.to_string()));
            let description = n["description"]["value"].as_str().map(ToString::to_string);
            let backend_node_id = n["backendDOMNodeId"].as_i64();
            let frame_id = n["frameId"].as_str().map(ToString::to_string);
            let ignored = n["ignored"].as_bool().unwrap_or(false);

            let mut focusable = false;
            let mut focused = false;
            let mut disabled = false;
            let mut checked = None;
            let mut expanded = None;
            let mut selected = None;
            let mut required = false;
            let mut level = None;

            if let Some(props) = n["properties"].as_array() {
                for prop in props {
                    let pname = prop["name"].as_str().unwrap_or_default();
                    let pval = &prop["value"]["value"];
                    match pname {
                        "focusable" => focusable = pval.as_bool().unwrap_or(false),
                        "focused" => focused = pval.as_bool().unwrap_or(false),
                        "disabled" => disabled = pval.as_bool().unwrap_or(false),
                        "checked" => {
                            checked = match pval.as_str() {
                                Some("true") => Some(true),
                                Some("false") => Some(false),
                                _ => pval.as_bool(),
                            };
                        }
                        "expanded" => expanded = pval.as_bool(),
                        "selected" => selected = pval.as_bool(),
                        "required" => required = pval.as_bool().unwrap_or(false),
                        "level" => level = pval.as_u64().map(|l| l as u32),
                        _ => {}
                    }
                }
            }

            nodes.push(AxNode {
                node_id,
                parent_id,
                child_ids,
                role,
                name,
                value,
                description,
                backend_node_id,
                frame_id,
                ignored,
                focusable,
                focused,
                disabled,
                checked,
                expanded,
                selected,
                required,
                level,
                bounds: None,
            });
        }

        Ok(nodes)
    }

    async fn resolve_node_box(
        &self,
        backend_node_id: i64,
        _frame_id: Option<&str>,
    ) -> Result<BoundingBox> {
        if let Ok(res) = self
            .call(
                "DOM.getContentQuads",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await
            && let Some(bbox) = res["quads"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(quad_to_bbox)
        {
            return Ok(bbox);
        }

        if let Ok(res) = self
            .call(
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await
            && let Some(bbox) = quad_to_bbox(&res["model"]["content"])
        {
            return Ok(bbox);
        }

        Err(MimikyuError::ElementNotFound(format!(
            "cannot resolve bounding box for backendNodeId {backend_node_id}"
        )))
    }

    async fn resolve_selector_box(&self, selector: &str) -> Result<BoundingBox> {
        let backend_id = self.query_backend_node_id(selector).await?;
        self.resolve_node_box(backend_id, None).await
    }

    async fn focus_node(
        &self,
        backend_node_id: Option<i64>,
        selector: Option<&str>,
    ) -> Result<()> {
        if let Some(bid) = backend_node_id {
            self.call("DOM.focus", json!({ "backendNodeId": bid }))
                .await?;
            return Ok(());
        }
        if let Some(sel) = selector {
            let bid = self.query_backend_node_id(sel).await?;
            self.call("DOM.focus", json!({ "backendNodeId": bid }))
                .await?;
            return Ok(());
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
        let file_strs: Vec<String> = files
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        if let Some(bid) = backend_node_id {
            self.call(
                "DOM.setFileInputFiles",
                json!({ "backendNodeId": bid, "files": file_strs }),
            )
            .await?;
            return Ok(());
        }
        if let Some(sel) = selector {
            let bid = self.query_backend_node_id(sel).await?;
            self.call(
                "DOM.setFileInputFiles",
                json!({ "backendNodeId": bid, "files": file_strs }),
            )
            .await?;
            return Ok(());
        }
        Err(MimikyuError::InvalidConfig(
            "set_file_input_files requires either backend_node_id or selector".to_string(),
        ))
    }

    async fn dispatch_mouse(&self, ev: MouseEvent) -> Result<()> {
        let button = match ev.button {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
            MouseButton::None => "none",
        };
        let click_count = ev.click_count.max(1);

        match ev.event_type {
            MouseEventType::Move => {
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseMoved",
                        "x": ev.x,
                        "y": ev.y,
                        "button": button,
                        "modifiers": ev.modifiers,
                    }),
                )
                .await?;
            }
            MouseEventType::Down => {
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mousePressed",
                        "x": ev.x,
                        "y": ev.y,
                        "button": button,
                        "clickCount": click_count,
                        "modifiers": ev.modifiers,
                    }),
                )
                .await?;
            }
            MouseEventType::Up => {
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseReleased",
                        "x": ev.x,
                        "y": ev.y,
                        "button": button,
                        "clickCount": click_count,
                        "modifiers": ev.modifiers,
                    }),
                )
                .await?;
            }
            MouseEventType::Click => {
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseMoved",
                        "x": ev.x,
                        "y": ev.y,
                        "button": "none",
                        "modifiers": ev.modifiers,
                    }),
                )
                .await?;
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mousePressed",
                        "x": ev.x,
                        "y": ev.y,
                        "button": button,
                        "clickCount": click_count,
                        "modifiers": ev.modifiers,
                    }),
                )
                .await?;
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseReleased",
                        "x": ev.x,
                        "y": ev.y,
                        "button": button,
                        "clickCount": click_count,
                        "modifiers": ev.modifiers,
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn dispatch_key(&self, ev: KeyEvent) -> Result<()> {
        let spec = named_key_cdp_spec(&ev.key);

        if ev.event_type == KeyEventType::Char {
            if let Some((cdp_key, default_code, default_vk, default_text)) = spec {
                let code = ev.code.as_deref().unwrap_or(default_code);
                let vk = ev.windows_virtual_key_code.unwrap_or(default_vk);
                let text = ev.text.as_deref().or(default_text);
                let down_type = if text.is_some() {
                    "keyDown"
                } else {
                    "rawKeyDown"
                };
                let mut down_params = json!({
                    "type": down_type,
                    "key": cdp_key,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "modifiers": ev.modifiers,
                });
                if let Some(txt) = text {
                    down_params["text"] = json!(txt);
                    down_params["unmodifiedText"] = json!(txt);
                }
                self.call("Input.dispatchKeyEvent", down_params).await?;

                let up_params = json!({
                    "type": "keyUp",
                    "key": cdp_key,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "modifiers": ev.modifiers,
                });
                self.call("Input.dispatchKeyEvent", up_params).await?;
                return Ok(());
            }

            let (resolved_key, resolved_text) = resolve_cdp_char_key_and_text(&ev);
            let down_type = if resolved_text.is_some() {
                "keyDown"
            } else {
                "rawKeyDown"
            };
            let mut down_params = json!({
                "type": down_type,
                "key": resolved_key,
                "modifiers": ev.modifiers,
            });
            if let Some(ref code) = ev.code {
                down_params["code"] = json!(code);
            }
            if let Some(vk) = ev.windows_virtual_key_code {
                down_params["windowsVirtualKeyCode"] = json!(vk);
            }
            if let Some(ref txt) = resolved_text {
                down_params["text"] = json!(txt);
                down_params["unmodifiedText"] = json!(ev.key);
            }
            self.call("Input.dispatchKeyEvent", down_params).await?;

            let mut up_params = json!({
                "type": "keyUp",
                "key": resolved_key,
                "modifiers": ev.modifiers,
            });
            if let Some(ref code) = ev.code {
                up_params["code"] = json!(code);
            }
            if let Some(vk) = ev.windows_virtual_key_code {
                up_params["windowsVirtualKeyCode"] = json!(vk);
            }
            self.call("Input.dispatchKeyEvent", up_params).await?;
            return Ok(());
        }

        let ev_type = match ev.event_type {
            KeyEventType::Down => "keyDown",
            KeyEventType::Up => "keyUp",
            KeyEventType::Char => unreachable!(),
        };
        let key_str = spec.map(|s| s.0).unwrap_or(ev.key.as_str());
        let mut params = json!({
            "type": ev_type,
            "key": key_str,
            "modifiers": ev.modifiers,
        });
        if let Some(code) = ev.code.as_deref().or_else(|| spec.map(|s| s.1)) {
            params["code"] = json!(code);
        }
        if let Some(ref text) = ev.text {
            params["text"] = json!(text);
        }
        if let Some(vk) = ev.windows_virtual_key_code.or_else(|| spec.map(|s| s.2)) {
            params["windowsVirtualKeyCode"] = json!(vk);
        }
        self.call("Input.dispatchKeyEvent", params).await?;
        Ok(())
    }

    async fn dispatch_wheel(&self, ev: WheelEvent) -> Result<()> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": ev.x,
                "y": ev.y,
                "deltaX": ev.delta_x,
                "deltaY": ev.delta_y,
                "modifiers": ev.modifiers,
            }),
        )
        .await?;
        Ok(())
    }

    async fn insert_text(&self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    async fn screenshot(&self, opts: ScreenshotOpts) -> Result<Vec<u8>> {
        if opts.omit_background {
            let _ = self
                .call(
                    "Emulation.setDefaultBackgroundColorOverride",
                    json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }),
                )
                .await;
        }

        let mut params = match opts.format {
            ScreenshotFormat::Png => json!({ "format": "png" }),
            ScreenshotFormat::Jpeg { quality } => json!({ "format": "jpeg", "quality": quality }),
        };

        if opts.full_page {
            params["captureBeyondViewport"] = json!(true);
            if let Ok(metrics) = self.call("Page.getLayoutMetrics", json!({})).await
                && let (Some(w), Some(h)) = (
                    metrics["cssContentSize"]["width"].as_f64(),
                    metrics["cssContentSize"]["height"].as_f64(),
                )
                && w > 0.0
                && h > 0.0
            {
                params["clip"] = json!({
                    "x": 0.0,
                    "y": 0.0,
                    "width": w,
                    "height": h,
                    "scale": 1.0,
                });
            }
        } else if let Some(clip) = opts.clip {
            params["clip"] = json!({
                "x": clip.x,
                "y": clip.y,
                "width": clip.width,
                "height": clip.height,
                "scale": 1.0,
            });
        }

        let res = self.call("Page.captureScreenshot", params).await;

        if opts.omit_background {
            let _ = self
                .call("Emulation.setDefaultBackgroundColorOverride", json!({}))
                .await;
        }

        let res = res?;
        let b64 = res["data"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Page.captureScreenshot missing data".to_string(),
            })?;
        BASE64
            .decode(b64)
            .map_err(|e| MimikyuError::Protocol {
                code: -1,
                message: format!("invalid base64 screenshot data: {e}"),
            })
    }

    async fn pdf(&self) -> Result<Vec<u8>> {
        let res = self
            .call("Page.printToPDF", json!({ "printBackground": true }))
            .await?;
        let b64 = res["data"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Page.printToPDF missing data".to_string(),
            })?;
        BASE64
            .decode(b64)
            .map_err(|e| MimikyuError::Protocol {
                code: -1,
                message: format!("invalid base64 PDF data: {e}"),
            })
    }

    async fn set_instant_animations(&self, enabled: bool) -> Result<()> {
        let _ = self.call("Animation.enable", json!({})).await;
        let rate = if enabled { 1000.0 } else { 1.0 };
        self.call("Animation.setPlaybackRate", json!({ "playbackRate": rate }))
            .await?;
        Ok(())
    }

    async fn set_env_overrides(&self, profile: EnvProfile) -> Result<()> {
        self.ensure_alive()?;

        let ua_params = if !profile.user_agent.is_empty() || profile.user_agent_metadata.is_some()
        {
            let accept_lang = if !profile.languages.is_empty() {
                profile.languages.join(",")
            } else {
                profile
                    .accept_language
                    .split(',')
                    .filter_map(|p| p.split(';').next().map(str::trim))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let mut p = json!({
                "userAgent": if profile.user_agent.is_empty() { "Mozilla/5.0" } else { &profile.user_agent },
                "acceptLanguage": accept_lang,
                "platform": profile.platform,
            });
            if let Some(ref meta) = profile.user_agent_metadata {
                let brands: Vec<Value> = meta
                    .brands
                    .iter()
                    .map(|b| json!({ "brand": b.brand, "version": b.version }))
                    .collect();
                let full_list: Vec<Value> = meta
                    .full_version_list
                    .iter()
                    .map(|b| json!({ "brand": b.brand, "version": b.version }))
                    .collect();
                p["userAgentMetadata"] = json!({
                    "brands": brands,
                    "fullVersionList": full_list,
                    "platform": meta.platform,
                    "platformVersion": meta.platform_version,
                    "architecture": meta.architecture,
                    "model": meta.model,
                    "mobile": meta.mobile,
                    "bitness": meta.bitness,
                    "wow64": meta.wow64,
                });
            }
            Some(p)
        } else {
            None
        };

        *self.state.worker_ua_params.lock() = ua_params.clone();
        *self.state.worker_init_script.lock() =
            Some(build_worker_init_script(&profile, EngineKind::Chromium, false));

        let win_script = build_window_init_script(&profile, EngineKind::Chromium);
        {
            let mut prev = self.state.env_script_id.lock().await;
            if let Some(old_id) = prev.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            let new_id = self.add_init_script(&win_script).await?;
            *prev = Some(new_id);
        }

        let ua_fut = async {
            if let Some(p) = ua_params {
                self.call("Emulation.setUserAgentOverride", p).await?;
            }
            Ok::<_, MimikyuError>(())
        };
        let tz_fut = async {
            if !profile.timezone.is_empty() {
                self.call(
                    "Emulation.setTimezoneOverride",
                    json!({ "timezoneId": profile.timezone }),
                )
                .await?;
            }
            Ok::<_, MimikyuError>(())
        };
        let loc_fut = async {
            if !profile.locale.is_empty() {
                self.call(
                    "Emulation.setLocaleOverride",
                    json!({ "locale": profile.locale }),
                )
                .await?;
            }
            Ok::<_, MimikyuError>(())
        };
        let metrics_fut = self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": profile.viewport_width,
                "height": profile.viewport_height,
                "deviceScaleFactor": profile.device_scale_factor,
                "mobile": profile.is_mobile,
                "screenWidth": profile.screen_width,
                "screenHeight": profile.screen_height,
            }),
        );
        let geo_fut = async {
            if let Some(ref geo) = profile.geolocation {
                self.call(
                    "Emulation.setGeolocationOverride",
                    json!({
                        "latitude": geo.latitude,
                        "longitude": geo.longitude,
                        "accuracy": geo.accuracy,
                    }),
                )
                .await?;
            }
            Ok::<_, MimikyuError>(())
        };
        let hc_fut = async {
            if profile.hardware_concurrency > 0 {
                let _ = self
                    .call(
                        "Emulation.setHardwareConcurrencyOverride",
                        json!({ "hardwareConcurrency": profile.hardware_concurrency }),
                    )
                    .await;
            }
            Ok::<_, MimikyuError>(())
        };
        let hdr_fut = async {
            self.call(
                "Network.setExtraHTTPHeaders",
                json!({ "headers": profile.extra_headers }),
            )
            .await?;
            Ok::<_, MimikyuError>(())
        };

        tokio::try_join!(ua_fut, tz_fut, loc_fut, metrics_fut, geo_fut, hc_fut, hdr_fut)?;

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
        for g in permissions {
            let setting = match g.state {
                PermissionState::Granted => "granted",
                PermissionState::Denied => "denied",
                PermissionState::Prompt => "prompt",
            };
            let mut params = json!({
                "permission": { "name": g.name },
                "setting": setting,
            });
            if let Some(ref orig) = g.origin {
                params["origin"] = json!(orig);
            }
            if let Some(ref ctx_id) = self.state.context_id {
                params["browserContextId"] = json!(ctx_id);
            }
            if let Err(err) = self.conn.send_browser("Browser.setPermission", params).await {
                if g.state == PermissionState::Granted {
                    let mut gp = json!({ "permissions": [g.name] });
                    if let Some(ref orig) = g.origin {
                        gp["origin"] = json!(orig);
                    }
                    if let Some(ref ctx_id) = self.state.context_id {
                        gp["browserContextId"] = json!(ctx_id);
                    }
                    self.conn.send_browser("Browser.grantPermissions", gp).await?;
                } else {
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    async fn reset_permissions(&self) -> Result<()> {
        self.ensure_alive()?;
        let mut params = json!({});
        if let Some(ref ctx_id) = self.state.context_id {
            params["browserContextId"] = json!(ctx_id);
        }
        self.conn
            .send_browser("Browser.resetPermissions", params)
            .await?;
        Ok(())
    }

    async fn set_block_policy(&self, policy: BlockPolicy) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().set_block_policy(policy)?;
        self.sync_cdp_network().await
    }

    async fn set_blocked_urls(&self, patterns: Vec<String>) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().set_blocked_urls(patterns)?;
        self.sync_cdp_network().await
    }

    async fn set_route_mock(&self, mock: RouteMock) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().add_mock(mock)?;
        self.sync_cdp_network().await
    }

    async fn set_route_rule(&self, rule: RouteRule) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().add_rule(rule)?;
        self.sync_cdp_network().await
    }

    async fn clear_route_mocks(&self) -> Result<()> {
        self.ensure_alive()?;
        self.state.interceptor.lock().clear_rules();
        self.sync_cdp_network().await
    }

    async fn cookies(&self) -> Result<Vec<Cookie>> {
        self.ensure_alive()?;
        let storage_params = match self.state.context_id {
            Some(ref ctx_id) => json!({ "browserContextId": ctx_id }),
            None => json!({}),
        };
        let res = match self.call("Storage.getCookies", storage_params).await {
            Ok(v) => v,
            Err(_) => self.call("Network.getCookies", json!({})).await?,
        };
        let arr = res["cookies"].as_array().cloned().unwrap_or_default();
        Ok(arr
            .into_iter()
            .map(|c| Cookie {
                name: c["name"].as_str().unwrap_or_default().to_string(),
                value: c["value"].as_str().unwrap_or_default().to_string(),
                domain: c["domain"].as_str().unwrap_or_default().to_string(),
                path: c["path"].as_str().unwrap_or("/").to_string(),
                expires: c["expires"].as_f64().filter(|&e| e > 0.0),
                http_only: c["httpOnly"].as_bool().unwrap_or(false),
                secure: c["secure"].as_bool().unwrap_or(false),
                same_site: c["sameSite"].as_str().map(ToString::to_string),
            })
            .collect())
    }

    async fn set_cookies(&self, cookies: Vec<Cookie>) -> Result<()> {
        self.ensure_alive()?;
        if cookies.is_empty() {
            return Ok(());
        }
        let mapped: Vec<Value> = cookies
            .into_iter()
            .map(|c| {
                let mut obj = json!({
                    "name": c.name,
                    "value": c.value,
                    "domain": c.domain,
                    "path": if c.path.is_empty() { "/" } else { &c.path },
                    "httpOnly": c.http_only,
                    "secure": c.secure,
                });
                if let Some(exp) = c.expires
                    && exp.is_finite()
                    && exp > 0.0
                {
                    obj["expires"] = json!(exp);
                }
                if let Some(ss) = c.same_site
                    && let Some(norm) = normalize_cdp_same_site(&ss)
                {
                    obj["sameSite"] = json!(norm);
                }
                obj
            })
            .collect();
        let mut storage_params = json!({ "cookies": mapped });
        if let Some(ref ctx_id) = self.state.context_id {
            storage_params["browserContextId"] = json!(ctx_id);
        }
        if self.call("Storage.setCookies", storage_params).await.is_err() {
            self.call("Network.setCookies", json!({ "cookies": mapped }))
                .await?;
        }
        Ok(())
    }

    async fn clear_cookies(&self) -> Result<()> {
        self.ensure_alive()?;
        let storage_params = match self.state.context_id {
            Some(ref ctx_id) => json!({ "browserContextId": ctx_id }),
            None => json!({}),
        };
        if self.call("Storage.clearCookies", storage_params).await.is_err() {
            self.call("Network.clearBrowserCookies", json!({})).await?;
        }
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
            let local_storage = json_map_to_btree(val.get("localStorage"));
            let session_storage = json_map_to_btree(val.get("sessionStorage"));
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
        self.conn.sessions.write().remove(&self.info.session_id);

        if self.state.is_root {
            self.conn.shutdown_all().await;
        } else if self.state.owns_context {
            if let Some(ref ctx_id) = self.state.context_id {
                self.conn.sessions.write().retain(|_, st| {
                    if st.context_id.as_deref() == Some(ctx_id.as_str()) {
                        st.disposed.store(true, Ordering::SeqCst);
                        false
                    } else {
                        true
                    }
                });
                self.conn.isolated_contexts.write().remove(ctx_id);
                self.conn
                    .active_targets
                    .write()
                    .remove(&context_scope_key(Some(ctx_id)));
                let _ = self
                    .conn
                    .send_browser(
                        "Target.disposeBrowserContext",
                        json!({ "browserContextId": ctx_id }),
                    )
                    .await;
            }
        } else {
            self.conn.clear_active_target_if_matches(
                self.state.context_id.as_deref(),
                &self.state.target_id,
            );
            if let Ok(res) = self
                .conn
                .send_browser(
                    "Target.closeTarget",
                    json!({ "targetId": self.state.target_id }),
                )
                .await
                && res.get("success").and_then(|v| v.as_bool()) != Some(false)
            {
                for _ in 0..50 {
                    let Ok(res) = self.conn.send_browser("Target.getTargets", json!({})).await
                    else {
                        break;
                    };
                    let still_present = res["targetInfos"]
                        .as_array()
                        .map(|arr| {
                            arr.iter().any(|t| {
                                t["targetId"].as_str() == Some(self.state.target_id.as_str())
                            })
                        })
                        .unwrap_or(false);
                    if !still_present {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Ok(())
    }
}

fn normalize_cdp_same_site(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "strict" => Some("Strict"),
        "lax" => Some("Lax"),
        "none" => Some("None"),
        _ => None,
    }
}

fn parse_eval_result(res: Value) -> Result<Value> {
    if let Some(exc) = res.get("exceptionDetails") {
        let text = exc["exception"]["description"]
            .as_str()
            .or_else(|| exc["text"].as_str())
            .unwrap_or("JavaScript exception")
            .to_string();
        return Err(MimikyuError::EvalFailed(text));
    }
    let result = &res["result"];
    if result.get("type").and_then(|v| v.as_str()) == Some("undefined") {
        return Ok(Value::Null);
    }
    if let Some(v) = result.get("value") {
        return Ok(v.clone());
    }
    Ok(Value::Null)
}

fn format_cdp_remote_object(obj: &Value) -> String {
    if let Some(v) = obj.get("value") {
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        return v.to_string();
    }
    if let Some(desc) = obj.get("description").and_then(|v| v.as_str()) {
        return desc.to_string();
    }
    if let Some(unser) = obj.get("unserializableValue").and_then(|v| v.as_str()) {
        return unser.to_string();
    }
    obj.get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn json_map_to_btree(val: Option<&Value>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(obj) = val.and_then(|v| v.as_object()) {
        for (k, v) in obj {
            let s = v
                .as_str()
                .map(ToString::to_string)
                .unwrap_or_else(|| v.to_string());
            map.insert(k.clone(), s);
        }
    }
    map
}

fn quad_to_bbox(quad: &Value) -> Option<BoundingBox> {
    let arr = quad.as_array()?;
    if arr.len() < 8 {
        return None;
    }
    let xs = [
        arr[0].as_f64()?,
        arr[2].as_f64()?,
        arr[4].as_f64()?,
        arr[6].as_f64()?,
    ];
    let ys = [
        arr[1].as_f64()?,
        arr[3].as_f64()?,
        arr[5].as_f64()?,
        arr[7].as_f64()?,
    ];
    let min_x = xs.into_iter().fold(f64::INFINITY, f64::min);
    let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.into_iter().fold(f64::INFINITY, f64::min);
    let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max);
    Some(BoundingBox {
        x: min_x,
        y: min_y,
        width: (max_x - min_x).max(0.0),
        height: (max_y - min_y).max(0.0),
    })
}

fn resolve_cdp_char_key_and_text(ev: &KeyEvent) -> (String, Option<String>) {
    let shift_active = ev.modifiers & 8 != 0;
    let mut chars = ev.key.chars();
    let single_char = match (chars.next(), chars.next()) {
        (Some(ch), None) => Some(ch),
        _ => None,
    };

    let resolved_key = if shift_active
        && let Some(ch) = single_char
        && ch.is_ascii_lowercase()
    {
        ch.to_ascii_uppercase().to_string()
    } else {
        ev.key.clone()
    };

    let resolved_text = ev
        .text
        .clone()
        .or_else(|| single_char.map(|_| resolved_key.clone()));

    (resolved_key, resolved_text)
}

fn named_key_cdp_spec(key: &str) -> Option<(&'static str, &'static str, u32, Option<&'static str>)> {
    match key {
        "Cancel" => Some(("Cancel", "Cancel", 3, None)),
        "Help" => Some(("Help", "Help", 47, None)),
        "Backspace" => Some(("Backspace", "Backspace", 8, None)),
        "Tab" => Some(("Tab", "Tab", 9, None)),
        "Clear" => Some(("Clear", "NumpadClear", 12, None)),
        "Enter" | "Return" => Some(("Enter", "Enter", 13, Some("\r"))),
        "Shift" => Some(("Shift", "ShiftLeft", 16, None)),
        "Control" => Some(("Control", "ControlLeft", 17, None)),
        "Alt" => Some(("Alt", "AltLeft", 18, None)),
        "Pause" => Some(("Pause", "Pause", 19, None)),
        "Escape" => Some(("Escape", "Escape", 27, None)),
        "Space" => Some((" ", "Space", 32, Some(" "))),
        "PageUp" => Some(("PageUp", "PageUp", 33, None)),
        "PageDown" => Some(("PageDown", "PageDown", 34, None)),
        "End" => Some(("End", "End", 35, None)),
        "Home" => Some(("Home", "Home", 36, None)),
        "ArrowLeft" => Some(("ArrowLeft", "ArrowLeft", 37, None)),
        "ArrowUp" => Some(("ArrowUp", "ArrowUp", 38, None)),
        "ArrowRight" => Some(("ArrowRight", "ArrowRight", 39, None)),
        "ArrowDown" => Some(("ArrowDown", "ArrowDown", 40, None)),
        "Insert" => Some(("Insert", "Insert", 45, None)),
        "Delete" => Some(("Delete", "Delete", 46, None)),
        "Meta" => Some(("Meta", "MetaLeft", 91, None)),
        "F1" => Some(("F1", "F1", 112, None)),
        "F2" => Some(("F2", "F2", 113, None)),
        "F3" => Some(("F3", "F3", 114, None)),
        "F4" => Some(("F4", "F4", 115, None)),
        "F5" => Some(("F5", "F5", 116, None)),
        "F6" => Some(("F6", "F6", 117, None)),
        "F7" => Some(("F7", "F7", 118, None)),
        "F8" => Some(("F8", "F8", 119, None)),
        "F9" => Some(("F9", "F9", 120, None)),
        "F10" => Some(("F10", "F10", 121, None)),
        "F11" => Some(("F11", "F11", 122, None)),
        "F12" => Some(("F12", "F12", 123, None)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_key_cdp_spec_covers_editing_and_ignores_printable() {
        assert_eq!(
            named_key_cdp_spec("Backspace"),
            Some(("Backspace", "Backspace", 8, None))
        );
        assert_eq!(
            named_key_cdp_spec("Enter"),
            Some(("Enter", "Enter", 13, Some("\r")))
        );
        assert_eq!(
            named_key_cdp_spec("Return"),
            Some(("Enter", "Enter", 13, Some("\r")))
        );
        assert_eq!(
            named_key_cdp_spec("Space"),
            Some((" ", "Space", 32, Some(" ")))
        );
        assert_eq!(
            named_key_cdp_spec("ArrowLeft"),
            Some(("ArrowLeft", "ArrowLeft", 37, None))
        );
        assert_eq!(named_key_cdp_spec("F1"), Some(("F1", "F1", 112, None)));
        assert_eq!(named_key_cdp_spec("F12"), Some(("F12", "F12", 123, None)));
        assert_eq!(named_key_cdp_spec("a"), None);
        assert_eq!(named_key_cdp_spec("!"), None);
        assert_eq!(named_key_cdp_spec(" "), None);
    }

    #[test]
    fn resolve_cdp_char_key_and_text_normalizes_shift_ascii_letter() {
        let shifted = KeyEvent {
            event_type: KeyEventType::Char,
            key: "a".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 8,
        };
        assert_eq!(
            resolve_cdp_char_key_and_text(&shifted),
            ("A".to_string(), Some("A".to_string()))
        );

        let plain = KeyEvent {
            event_type: KeyEventType::Char,
            key: "é".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        };
        assert_eq!(
            resolve_cdp_char_key_and_text(&plain),
            ("é".to_string(), Some("é".to_string()))
        );

        let explicit_text = KeyEvent {
            event_type: KeyEventType::Char,
            key: "a".to_string(),
            code: None,
            text: Some("custom".to_string()),
            windows_virtual_key_code: None,
            modifiers: 8,
        };
        assert_eq!(
            resolve_cdp_char_key_and_text(&explicit_text),
            ("A".to_string(), Some("custom".to_string()))
        );
    }
}
