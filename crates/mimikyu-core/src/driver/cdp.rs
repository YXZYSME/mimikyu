use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, warn};

use crate::driver::{
    AxNode, BoundingBox, BrowserDriver, ConsoleEntry, ContextOptions, Cookie, DriverCapabilities,
    DriverSession, EngineKind, EnvProfile, FrameInfo, KeyEvent, KeyEventType, LaunchOptions,
    MOD_ALT, MOD_CONTROL, MOD_META, MouseButton, MouseEvent, MouseEventType, NavInfo, NetworkEvent,
    OriginStorage, PageError, PermissionGrant, PermissionState, RouteMock, ScreencastFormat,
    ScreencastOptions, ScreencastStatus, ScreenshotFormat, ScreenshotOpts, SelectBy, SessionInfo,
    ShadowMatch, StorageState, TabInfo, WheelEvent, now_ms,
};
use crate::env_profile::{
    ProxyRelay, build_webrtc_no_host_init_script, build_window_init_script,
    build_worker_init_script,
};
use crate::error::{MimikyuError, Result};
use crate::network::{
    BlockPolicy, InterceptDecision, InterceptRequest, NetworkInterceptor, ResourceType, RouteRule,
    TelemetryConfig, TelemetryStore,
};
use crate::process::{ProcessGuard, chromium_proxy_bypass_list};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_OBJECT_GROUP_ID: AtomicU64 = AtomicU64::new(1);
const INSTANT_ANIM_CSS: &str = "*, *::before, *::after { transition-duration: 0s !important; transition-delay: 0s !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; scroll-behavior: auto !important; }";
const INSTANT_ANIM_PRELOAD_JS: &str = r#"(() => {
  const css = "*, *::before, *::after { transition-duration: 0s !important; transition-delay: 0s !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; scroll-behavior: auto !important; }";
  const apply = () => {
    try {
      if (document.adoptedStyleSheets && typeof CSSStyleSheet === "function") {
        const already = document.adoptedStyleSheets.some(s => {
          try { return s.cssRules && s.cssRules.length > 0 && s.cssRules[0].cssText.includes("animation-iteration-count: 1"); } catch { return false; }
        });
        if (!already) {
          const sheet = new CSSStyleSheet();
          sheet.replaceSync(css);
          document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
        }
        return;
      }
    } catch {}
    const host = document.head || document.documentElement;
    if (host) {
      const existing = Array.from(host.querySelectorAll("style")).some(el => el.textContent === css);
      if (!existing) {
        const st = document.createElement("style");
        st.textContent = css;
        host.appendChild(st);
      }
    }
  };
  if (document.documentElement) apply();
  else document.addEventListener("DOMContentLoaded", apply, { once: true });
})();"#;

fn instant_anim_dom_js(enabled: bool) -> Result<String> {
    let css_json = serde_json::to_string(INSTANT_ANIM_CSS)?;
    if enabled {
        Ok(format!(
            r#"(() => {{
                const css = {css_json};
                try {{
                    if (document.adoptedStyleSheets && typeof CSSStyleSheet === "function") {{
                        const already = document.adoptedStyleSheets.some(s => {{
                            try {{ return s.cssRules && s.cssRules.length > 0 && s.cssRules[0].cssText.includes("animation-iteration-count: 1"); }} catch {{ return false; }}
                        }});
                        if (!already) {{
                            const sheet = new CSSStyleSheet();
                            sheet.replaceSync(css);
                            document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
                        }}
                    }} else {{
                        const host = document.head || document.documentElement;
                        if (host && !Array.from(host.querySelectorAll("style")).some(el => el.textContent === css)) {{
                            const st = document.createElement("style");
                            st.textContent = css;
                            host.appendChild(st);
                        }}
                    }}
                }} catch {{}}
                try {{
                    for (const anim of document.getAnimations()) {{
                        const t = anim.effect && anim.effect.getComputedTiming ? anim.effect.getComputedTiming() : null;
                        if (!t || Number.isFinite(t.iterations)) {{
                            try {{ anim.finish(); }} catch {{}}
                        }}
                    }}
                }} catch {{}}
            }})()"#
        ))
    } else {
        Ok(format!(
            r#"(() => {{
                const css = {css_json};
                try {{
                    if (document.adoptedStyleSheets) {{
                        document.adoptedStyleSheets = document.adoptedStyleSheets.filter(s => {{
                            try {{ return !(s.cssRules && s.cssRules.length > 0 && s.cssRules[0].cssText.includes("animation-iteration-count: 1")); }} catch {{ return true; }}
                        }});
                    }}
                    for (const st of Array.from(document.querySelectorAll("style"))) {{
                        if (st.textContent === css) st.remove();
                    }}
                }} catch {{}}
            }})()"#
        ))
    }
}

async fn apply_instant_anim_css_on_session(conn: &CdpConnection, sid: &str, enabled: bool) {
    let Ok(js) = instant_anim_dom_js(enabled) else {
        return;
    };
    let mut frame_ids = Vec::new();
    if let Ok(tree) = conn
        .send_raw(Some(sid), "Page.getFrameTree", json!({}))
        .await
    {
        let mut frames = Vec::new();
        collect_cdp_frames(&tree["frameTree"], None, &mut frames);
        for f in frames {
            frame_ids.push(f.frame_id);
        }
    }
    if frame_ids.is_empty() {
        let _ = conn
            .send_raw(
                Some(sid),
                "Runtime.evaluate",
                json!({
                    "expression": js,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await;
        return;
    }
    for fid in frame_ids {
        let Ok(world_res) = conn
            .send_raw(
                Some(sid),
                "Page.createIsolatedWorld",
                json!({
                    "frameId": fid,
                    "worldName": "mimikyu_isolated",
                    "grantUniveralAccess": true,
                }),
            )
            .await
        else {
            continue;
        };
        if let Some(ctx_id) = world_res["executionContextId"].as_i64() {
            let _ = conn
                .send_raw(
                    Some(sid),
                    "Runtime.evaluate",
                    json!({
                        "expression": js,
                        "contextId": ctx_id,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                )
                .await;
        }
    }
}

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
                    params["proxyServer"] = json!(crate::process::normalize_chromium_proxy_server(
                        &proxy.server
                    ));
                    params["proxyBypassList"] = json!(chromium_proxy_bypass_list(&proxy.bypass));
                }
                let res = match conn
                    .send_browser("Target.createBrowserContext", params)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        conn.shutdown_all().await;
                        return Err(e);
                    }
                };
                let Some(ctx_id) = res["browserContextId"].as_str().map(ToString::to_string) else {
                    conn.shutdown_all().await;
                    return Err(MimikyuError::Protocol {
                        code: -1,
                        message: "Target.createBrowserContext missing browserContextId".to_string(),
                    });
                };
                conn.isolated_contexts.write().insert(ctx_id.clone());
                (Some(ctx_id), true)
            }
            _ => (None, false),
        };

        let target_id = if let Some(ref ctx_id) = context_id {
            let res = match conn
                .send_browser(
                    "Target.createTarget",
                    json!({ "url": "about:blank", "browserContextId": ctx_id }),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    conn.shutdown_all().await;
                    return Err(e);
                }
            };
            match res["targetId"].as_str() {
                Some(t) => t.to_string(),
                None => {
                    conn.shutdown_all().await;
                    return Err(MimikyuError::Protocol {
                        code: -1,
                        message: "Target.createTarget missing targetId".to_string(),
                    });
                }
            }
        } else {
            // Find existing initial about:blank page target, or create one.
            let targets_res = match conn.send_browser("Target.getTargets", json!({})).await {
                Ok(r) => r,
                Err(e) => {
                    conn.shutdown_all().await;
                    return Err(e);
                }
            };
            let existing = targets_res["targetInfos"]
                .as_array()
                .and_then(|arr| arr.iter().find(|t| t["type"].as_str() == Some("page")))
                .and_then(|t| t["targetId"].as_str())
                .map(ToString::to_string);
            if let Some(tid) = existing {
                tid
            } else {
                let res = match conn
                    .send_browser("Target.createTarget", json!({ "url": "about:blank" }))
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        conn.shutdown_all().await;
                        return Err(e);
                    }
                };
                match res["targetId"].as_str() {
                    Some(t) => t.to_string(),
                    None => {
                        conn.shutdown_all().await;
                        return Err(MimikyuError::Protocol {
                            code: -1,
                            message: "Target.createTarget missing targetId".to_string(),
                        });
                    }
                }
            }
        };

        let attach_res = match conn
            .send_browser(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                conn.shutdown_all().await;
                return Err(e);
            }
        };
        let Some(cdp_session_id) = attach_res["sessionId"].as_str().map(ToString::to_string) else {
            conn.shutdown_all().await;
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "Target.attachToTarget missing sessionId".to_string(),
            });
        };

        conn.set_active_target(context_id.as_deref(), &target_id);

        let _ = conn
            .send_browser(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true,
                    "filter": [
                        { "type": "page", "exclude": false },
                        { "type": "shared_worker", "exclude": false },
                        { "exclude": true }
                    ],
                }),
            )
            .await;

        let dl_dir = opts
            .download_dir
            .clone()
            .unwrap_or_else(|| profile_dir.join("downloads"));

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

        let session = match CdpSession::create(
            Arc::clone(&conn),
            info,
            context_id.clone(),
            owns_context,
            true,
            opts.main_world_eval,
            opts.telemetry_capacity,
            None,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                conn.shutdown_all().await;
                return Err(e);
            }
        };

        if let Err(err) =
            configure_cdp_download_behavior(&session.conn, context_id.as_deref(), &dl_dir).await
        {
            let _ = session.dispose().await;
            return Err(err);
        }

        let suppress_webrtc_host_without_profile = opts.privacy_mode
            || opts.profile_mode == crate::driver::ProfileMode::VirtualContext
            || opts.proxy.is_some();
        if let Err(err) = session
            .apply_initial_options(InitialSessionOptions {
                env_profile: opts.env_profile,
                suppress_webrtc_host_without_profile,
                permissions: Vec::new(),
                block_images: opts.block_images,
                block_media: opts.block_media,
                block_fonts: opts.block_fonts,
                block_trackers: opts.block_trackers || opts.privacy_mode,
                instant_animations: opts.instant_animations,
            })
            .await
        {
            let _ = session.dispose().await;
            return Err(err);
        }

        let _ = session.call("Page.bringToFront", json!({})).await;
        conn.mark_foreground_target(&target_id);

        Ok(session)
    }
}

async fn configure_cdp_download_behavior(
    conn: &CdpConnection,
    context_id: Option<&str>,
    download_dir: &Path,
) -> Result<()> {
    std::fs::create_dir_all(download_dir)?;
    let abs_dir = std::fs::canonicalize(download_dir).or_else(|_| {
        if download_dir.is_absolute() {
            Ok(download_dir.to_path_buf())
        } else {
            std::env::current_dir().map(|cwd| cwd.join(download_dir))
        }
    })?;
    let mut params = json!({
        "behavior": "allow",
        "downloadPath": abs_dir.to_string_lossy(),
        "eventsEnabled": true,
    });
    if let Some(ctx_id) = context_id {
        params["browserContextId"] = json!(ctx_id);
    }
    conn.send_browser("Browser.setDownloadBehavior", params)
        .await?;
    Ok(())
}

fn cdp_capabilities() -> DriverCapabilities {
    DriverCapabilities {
        engine: EngineKind::Chromium,
        protocol: "cdp".to_string(),
        supports_virtual_context: true,
        supports_per_context_proxy: true,
        supports_native_ax_tree: true,
        supports_isolated_world: true,
        supports_closed_shadow_dom: true,
        supports_pdf: true,
        supports_network_intercept: true,
        supports_screencast: true,
    }
}

struct CdpConnection {
    next_id: AtomicU64,
    write_tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    sessions: RwLock<HashMap<String, Arc<CdpSessionState>>>,
    iframe_owner_sessions: RwLock<HashMap<String, String>>,
    popup_owner_sessions: RwLock<HashMap<String, String>>,
    popup_target_owners: RwLock<HashMap<String, String>>,
    auto_attached_page_sessions: RwLock<HashMap<String, String>>,
    shared_worker_targets: RwLock<HashMap<String, String>>,
    handled_sw_fetch_nids: Mutex<VecDeque<String>>,
    browser_fetch_patterns: AsyncMutex<Option<Vec<Value>>>,
    isolated_contexts: RwLock<HashSet<String>>,
    creating_context_targets: Mutex<HashMap<String, usize>>,
    active_targets: RwLock<HashMap<String, String>>,
    foreground_target: RwLock<Option<String>>,
    process: AsyncMutex<Option<ProcessGuard>>,
    _root_proxy_relay: Mutex<Option<ProxyRelay>>,
    closed: AtomicBool,
    io_tasks: Mutex<Vec<JoinHandle<()>>>,
}

struct CreatingContextTargetGuard<'a> {
    conn: &'a CdpConnection,
    context_id: Option<String>,
}

impl<'a> CreatingContextTargetGuard<'a> {
    fn enter(conn: &'a CdpConnection, context_id: Option<&str>) -> Self {
        let owned = context_id.map(ToString::to_string);
        if let Some(ref cid) = owned {
            *conn
                .creating_context_targets
                .lock()
                .entry(cid.clone())
                .or_insert(0) += 1;
        }
        Self {
            conn,
            context_id: owned,
        }
    }
}

impl Drop for CreatingContextTargetGuard<'_> {
    fn drop(&mut self) {
        if let Some(ref cid) = self.context_id {
            let mut guard = self.conn.creating_context_targets.lock();
            if let Some(cnt) = guard.get_mut(cid) {
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    guard.remove(cid);
                }
            }
        }
    }
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

    fn mark_foreground_target(&self, target_id: &str) {
        *self.foreground_target.write() = Some(target_id.to_string());
    }

    fn is_foreground_target(&self, target_id: &str) -> bool {
        self.foreground_target.read().as_deref() == Some(target_id)
    }

    fn clear_foreground_target(&self) {
        *self.foreground_target.write() = None;
    }

    fn clear_foreground_target_if_matches(&self, target_id: &str) {
        let mut guard = self.foreground_target.write();
        if guard.as_deref() == Some(target_id) {
            *guard = None;
        }
    }

    fn session_id_for_target(&self, target_id: &str) -> Option<String> {
        let states: Vec<Arc<CdpSessionState>> = {
            let sessions = self.sessions.read();
            if let Some(st) = sessions
                .values()
                .find(|st| st.target_id == target_id && !st.disposed.load(Ordering::SeqCst))
            {
                return Some(st.session_id.clone());
            }
            sessions.values().cloned().collect()
        };
        for st in states {
            if st.disposed.load(Ordering::SeqCst) {
                continue;
            }
            if let Some(psid) = st.popup_sessions.lock().get(target_id) {
                return Some(psid.clone());
            }
        }
        self.auto_attached_page_sessions
            .read()
            .get(target_id)
            .cloned()
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
            iframe_owner_sessions: RwLock::new(HashMap::new()),
            popup_owner_sessions: RwLock::new(HashMap::new()),
            popup_target_owners: RwLock::new(HashMap::new()),
            auto_attached_page_sessions: RwLock::new(HashMap::new()),
            shared_worker_targets: RwLock::new(HashMap::new()),
            handled_sw_fetch_nids: Mutex::new(VecDeque::new()),
            browser_fetch_patterns: AsyncMutex::new(None),
            isolated_contexts: RwLock::new(HashSet::new()),
            creating_context_targets: Mutex::new(HashMap::new()),
            active_targets: RwLock::new(HashMap::new()),
            foreground_target: RwLock::new(None),
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
        let val = match crate::driver::parse_protocol_json(raw) {
            Ok(v) => v,
            Err((id_opt, err)) => {
                warn!("failed to parse CDP JSON frame: {err}");
                if let Some(id) = id_opt
                    && let Some(tx) = self.pending.lock().remove(&id)
                {
                    let _ = tx.send(Err(MimikyuError::Protocol {
                        code: -1,
                        message: format!("malformed CDP JSON frame: {err}"),
                    }));
                }
                return;
            }
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
            let direct_state = self.sessions.read().get(sid).cloned();
            if let Some(state) = direct_state {
                state.handle_event(self, sid, method, params, false);
            } else {
                let iframe_owner_state = {
                    let owner_sid = self.iframe_owner_sessions.read().get(sid).cloned();
                    owner_sid.and_then(|osid| self.sessions.read().get(&osid).cloned())
                };
                if let Some(state) = iframe_owner_state {
                    state.handle_event(self, sid, method, params, true);
                } else {
                    let popup_owner_state = {
                        let owner_sid = self.popup_owner_sessions.read().get(sid).cloned();
                        owner_sid.and_then(|osid| self.sessions.read().get(&osid).cloned())
                    };
                    if let Some(state) = popup_owner_state {
                        state.handle_popup_event(self, sid, method, params);
                    } else if method == "Target.attachedToTarget"
                        && params.get("waitingForDebugger").and_then(|v| v.as_bool()) == Some(true)
                        && let Some(child_sid) = params
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string)
                    {
                        let conn_clone = Arc::clone(self);
                        if tokio::runtime::Handle::try_current().is_ok() {
                            tokio::spawn(async move {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Runtime.runIfWaitingForDebugger",
                                        json!({}),
                                    )
                                    .await;
                            });
                        }
                    }
                }
            }
        } else if method == "Fetch.requestPaused" {
            self.handle_browser_fetch_paused(&params);
        } else if method == "Target.targetCreated" {
            if params
                .get("targetInfo")
                .and_then(|t| t.get("type"))
                .and_then(|v| v.as_str())
                == Some("page")
            {
                self.clear_foreground_target();
            }
        } else if method == "Target.targetDestroyed" {
            self.handle_target_destroyed(&params);
        } else if method == "Target.attachedToTarget" {
            self.handle_browser_attached_to_target(&params);
        } else if method == "Target.detachedFromTarget"
            && let Some(child_sid) = params.get("sessionId").and_then(|v| v.as_str())
        {
            self.auto_attached_page_sessions
                .write()
                .retain(|_, sid| sid != child_sid);
            let owner_sid_opt = self.iframe_owner_sessions.write().remove(child_sid);
            if let Some(owner_sid) = owner_sid_opt
                && let Some(owner_state) = self.sessions.read().get(&owner_sid).cloned()
            {
                owner_state.prune_detached_iframe_sessions(self, child_sid);
            }
        }
    }

    fn handle_browser_attached_to_target(self: &Arc<Self>, params: &Value) {
        let Some(auto_sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
        else {
            return;
        };
        let waiting = params
            .get("waitingForDebugger")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let info = &params["targetInfo"];
        let target_type = info.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let target_id = info.get("targetId").and_then(|v| v.as_str());
        let opener_tid = info.get("openerId").and_then(|v| v.as_str());
        let target_ctx = info.get("browserContextId").and_then(|v| v.as_str());

        if target_type == "page" {
            self.clear_foreground_target();
            if let Some(ptid) = target_id {
                self.auto_attached_page_sessions
                    .write()
                    .insert(ptid.to_string(), auto_sid.clone());
            }
        }

        if target_type == "page"
            && let Some(ptid) = target_id
        {
            let owner_state = if let Some(otid) = opener_tid {
                let sessions = self.sessions.read();
                let direct = sessions.values().find(|st| st.target_id == otid).cloned();
                if direct.is_some() {
                    direct
                } else {
                    let owner_sid = self.popup_target_owners.read().get(otid).cloned();
                    owner_sid.and_then(|osid| sessions.get(&osid).cloned())
                }
            } else if let Some(tc) = target_ctx
                && self.isolated_contexts.read().contains(tc)
                && self
                    .creating_context_targets
                    .lock()
                    .get(tc)
                    .copied()
                    .unwrap_or(0)
                    == 0
            {
                let preferred_tid = self.active_target_for(Some(tc));
                let sessions = self.sessions.read();
                preferred_tid
                    .as_deref()
                    .and_then(|atid| {
                        sessions
                            .values()
                            .find(|st| {
                                !st.disposed.load(Ordering::SeqCst)
                                    && st.context_id.as_deref() == Some(tc)
                                    && st.target_id == atid
                            })
                            .cloned()
                    })
                    .or_else(|| {
                        sessions
                            .values()
                            .find(|st| {
                                !st.disposed.load(Ordering::SeqCst)
                                    && st.context_id.as_deref() == Some(tc)
                            })
                            .cloned()
                    })
            } else {
                None
            };
            let matched = owner_state
                .filter(|st| {
                    if st.disposed.load(Ordering::SeqCst) {
                        return false;
                    }
                    if let Some(ref ctx_id) = st.context_id {
                        target_ctx == Some(ctx_id.as_str())
                    } else {
                        !target_ctx.is_some_and(|tc| self.isolated_contexts.read().contains(tc))
                    }
                })
                .filter(|_| {
                    let is_existing_session =
                        self.sessions.read().values().any(|st| st.target_id == ptid);
                    !is_existing_session && !self.popup_target_owners.read().contains_key(ptid)
                });
            if let Some(owner_state) = matched {
                let owner_sid = owner_state.session_id.clone();
                self.popup_target_owners
                    .write()
                    .insert(ptid.to_string(), owner_sid.clone());
                self.popup_owner_sessions
                    .write()
                    .insert(auto_sid.clone(), owner_sid.clone());
                owner_state
                    .popup_sessions
                    .lock()
                    .insert(ptid.to_string(), auto_sid.clone());
                if tokio::runtime::Handle::try_current().is_ok() {
                    let conn_clone = Arc::clone(self);
                    let ptid_owned = ptid.to_string();
                    tokio::spawn(async move {
                        conn_clone
                            .configure_popup_session(&owner_sid, &ptid_owned, &auto_sid, waiting)
                            .await;
                    });
                }
                return;
            }
        } else if target_type == "shared_worker" {
            let owner_state = {
                let sessions = self.sessions.read();
                let isolated = self.isolated_contexts.read();
                sessions
                    .values()
                    .find(|st| {
                        if st.disposed.load(Ordering::SeqCst) {
                            return false;
                        }
                        if let Some(ref ctx_id) = st.context_id {
                            target_ctx == Some(ctx_id.as_str())
                        } else {
                            !target_ctx.is_some_and(|tc| isolated.contains(tc))
                        }
                    })
                    .cloned()
            };
            if let Some(owner_state) = owner_state {
                let owner_sid = owner_state.session_id.clone();
                owner_state.worker_sessions.lock().insert(auto_sid.clone());
                self.iframe_owner_sessions
                    .write()
                    .insert(auto_sid.clone(), owner_sid.clone());
                if let Some(tid) = target_id {
                    self.shared_worker_targets
                        .write()
                        .insert(tid.to_string(), owner_sid);
                }
                let worker_script = owner_state.worker_init_script.lock().clone();
                let worker_ua = owner_state.worker_ua_params.lock().clone();
                let target_headers = owner_state.env_extra_headers.lock().clone();
                let (globs, fetch_patterns) = {
                    let guard = owner_state.interceptor.lock();
                    (
                        guard.blocker().cdp_blocked_url_globs(),
                        guard.cdp_fetch_patterns(),
                    )
                };
                if tokio::runtime::Handle::try_current().is_ok() {
                    // Enqueue Fetch.enable FIRST + Network/Emulation/Runtime setup synchronously
                    // onto the FIFO WebSocket before `tokio::spawn` yields, and await all
                    // pre-resume replies BEFORE sending `Runtime.runIfWaitingForDebugger`
                    // (mirroring dedicated `worker` attach in `CdpSessionState::handle_event`).
                    let mut pre_resume_calls: Vec<
                        std::pin::Pin<
                            Box<dyn std::future::Future<Output = Result<Value>> + Send + 'static>,
                        >,
                    > = Vec::with_capacity(6);
                    if let Some(patterns) = fetch_patterns {
                        pre_resume_calls.push(Box::pin(self.queue_raw_owned(
                            Some(&auto_sid),
                            "Fetch.enable",
                            json!({ "patterns": patterns }),
                        )));
                    }
                    pre_resume_calls.push(Box::pin(self.queue_raw_owned(
                        Some(&auto_sid),
                        "Network.enable",
                        json!({}),
                    )));
                    if !globs.is_empty() {
                        pre_resume_calls.push(Box::pin(self.queue_raw_owned(
                            Some(&auto_sid),
                            "Network.setBlockedURLs",
                            json!({ "urls": globs }),
                        )));
                    }
                    if let Some(ua) = worker_ua {
                        pre_resume_calls.push(Box::pin(self.queue_raw_owned(
                            Some(&auto_sid),
                            "Emulation.setUserAgentOverride",
                            ua,
                        )));
                    }
                    if let Some(hdrs) = target_headers {
                        pre_resume_calls.push(Box::pin(self.queue_raw_owned(
                            Some(&auto_sid),
                            "Network.setExtraHTTPHeaders",
                            hdrs,
                        )));
                    }
                    if let Some(js) = worker_script {
                        pre_resume_calls.push(Box::pin(self.queue_raw_owned(
                            Some(&auto_sid),
                            "Runtime.evaluate",
                            json!({
                                "expression": js,
                                "silent": true,
                            }),
                        )));
                    }
                    let conn_clone = Arc::clone(self);
                    tokio::spawn(async move {
                        let _ = futures_util::future::join_all(pre_resume_calls).await;
                        if waiting {
                            let _ = conn_clone
                                .send_raw(
                                    Some(&auto_sid),
                                    "Runtime.runIfWaitingForDebugger",
                                    json!({}),
                                )
                                .await;
                        }
                    });
                }
                return;
            }
        }

        if waiting && tokio::runtime::Handle::try_current().is_ok() {
            let conn_clone = Arc::clone(self);
            tokio::spawn(async move {
                let _ = conn_clone
                    .send_raw(
                        Some(&auto_sid),
                        "Runtime.runIfWaitingForDebugger",
                        json!({}),
                    )
                    .await;
            });
        }
    }

    fn handle_target_destroyed(self: &Arc<Self>, params: &Value) {
        let Some(tid) = params.get("targetId").and_then(|v| v.as_str()) else {
            return;
        };
        self.auto_attached_page_sessions.write().remove(tid);
        self.clear_foreground_target_if_matches(tid);
        self.shared_worker_targets.write().remove(tid);
        let owner_sid_opt = self.popup_target_owners.write().remove(tid);
        if let Some(owner_sid) = owner_sid_opt {
            let owner_state_opt = self.sessions.read().get(&owner_sid).cloned();
            if let Some(owner_state) = owner_state_opt {
                owner_state.prune_destroyed_popup_target(self, tid);
            }
        }
    }

    async fn sync_browser_fetch(&self) {
        let mut guard = self.browser_fetch_patterns.lock().await;
        let sessions: Vec<Arc<CdpSessionState>> = self.sessions.read().values().cloned().collect();
        let mut all_urls = false;
        let mut resource_types: Vec<String> = Vec::new();
        for st in sessions {
            if st.disposed.load(Ordering::SeqCst) {
                continue;
            }
            let pats_opt = st.interceptor.lock().cdp_fetch_patterns();
            if let Some(pats) = pats_opt {
                for p in pats {
                    if let Some(rt) = p.get("resourceType").and_then(|v| v.as_str()) {
                        if !resource_types.iter().any(|x| x == rt) {
                            resource_types.push(rt.to_string());
                        }
                    } else {
                        all_urls = true;
                        break;
                    }
                }
            }
            if all_urls {
                break;
            }
        }
        let desired = if all_urls {
            Some(vec![json!({
                "urlPattern": "*",
                "requestStage": "Request",
            })])
        } else if !resource_types.is_empty() {
            resource_types.sort();
            Some(
                resource_types
                    .into_iter()
                    .map(|rt| {
                        json!({
                            "urlPattern": "*",
                            "resourceType": rt,
                            "requestStage": "Request",
                        })
                    })
                    .collect(),
            )
        } else {
            None
        };
        if *guard != desired {
            match desired {
                Some(ref patterns) => {
                    let _ = self
                        .send_browser("Fetch.enable", json!({ "patterns": patterns }))
                        .await;
                }
                None => {
                    let _ = self.send_browser("Fetch.disable", json!({})).await;
                }
            }
            *guard = desired;
        }
    }

    fn handle_browser_fetch_paused(self: &Arc<Self>, params: &Value) {
        let Some(fetch_id) = params
            .get("requestId")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
        else {
            return;
        };
        let frame_id = params.get("frameId").and_then(|v| v.as_str());
        let network_id = params.get("networkId").and_then(|v| v.as_str());
        let owner_state = frame_id
            .and_then(|fid| self.shared_worker_targets.read().get(fid).cloned())
            .and_then(|osid| self.sessions.read().get(&osid).cloned())
            .filter(|st| !st.disposed.load(Ordering::SeqCst));
        let already_handled = network_id.is_some_and(|nid| {
            let mut q = self.handled_sw_fetch_nids.lock();
            if let Some(pos) = q.iter().position(|x| x == nid) {
                q.remove(pos);
                true
            } else {
                false
            }
        });
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let conn_clone = Arc::clone(self);
        let Some(owner_state) = owner_state.filter(|_| !already_handled) else {
            tokio::spawn(async move {
                let _ = conn_clone
                    .send_browser("Fetch.continueRequest", json!({ "requestId": fetch_id }))
                    .await;
            });
            return;
        };
        let url = params
            .get("request")
            .and_then(|r| r.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let req_headers = json_map_to_btree(params.get("request").and_then(|r| r.get("headers")));
        let resource_type_raw = params
            .get("resourceType")
            .and_then(|v| v.as_str())
            .unwrap_or("Other");
        let resource_type = ResourceType::from_cdp(resource_type_raw);
        let decision = owner_state.interceptor.lock().decide(&InterceptRequest {
            url: &url,
            resource_type,
            headers: &req_headers,
        });
        let nid_owned = network_id.map(ToString::to_string);
        let telem = Arc::clone(&owner_state.telemetry);
        tokio::spawn(async move {
            let res = match decision {
                InterceptDecision::Block { reason } => {
                    conn_clone
                        .send_browser(
                            "Fetch.failRequest",
                            json!({ "requestId": fetch_id, "errorReason": reason }),
                        )
                        .await
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
                    conn_clone
                        .send_browser(
                            "Fetch.fulfillRequest",
                            json!({
                                "requestId": fetch_id,
                                "responseCode": status,
                                "responseHeaders": hdrs,
                                "body": BASE64.encode(body),
                            }),
                        )
                        .await
                }
                InterceptDecision::ModifyHeaders { headers } => {
                    let hdrs: Vec<Value> = headers
                        .into_iter()
                        .map(|(name, value)| json!({ "name": name, "value": value }))
                        .collect();
                    if let Err(e) = conn_clone
                        .send_browser(
                            "Fetch.continueRequest",
                            json!({ "requestId": fetch_id, "headers": hdrs }),
                        )
                        .await
                    {
                        warn!(target: "mimikyu::cdp::fetch", "browser Fetch.continueRequest(ModifyHeaders) failed: {e}");
                        conn_clone
                            .send_browser("Fetch.continueRequest", json!({ "requestId": fetch_id }))
                            .await
                    } else {
                        Ok(Value::Null)
                    }
                }
                InterceptDecision::Continue => {
                    conn_clone
                        .send_browser("Fetch.continueRequest", json!({ "requestId": fetch_id }))
                        .await
                }
            };
            if let Err(e) = res
                && let Some(nid) = nid_owned
            {
                telem.on_intercept_resolved_locally(
                    &nid,
                    None,
                    BTreeMap::new(),
                    true,
                    Some(e.to_string()),
                );
            }
        });
    }

    async fn configure_popup_session(
        self: &Arc<Self>,
        owner_sid: &str,
        popup_tid: &str,
        popup_sid: &str,
        waiting: bool,
    ) {
        let owner_state_opt = self.sessions.read().get(owner_sid).cloned();
        let Some(owner_state) = owner_state_opt else {
            self.popup_target_owners.write().remove(popup_tid);
            self.popup_owner_sessions.write().remove(popup_sid);
            if waiting {
                let _ = self
                    .send_raw(
                        Some(popup_sid),
                        "Runtime.runIfWaitingForDebugger",
                        json!({}),
                    )
                    .await;
            }
            return;
        };
        let still_tracked = self.popup_target_owners.read().contains_key(popup_tid);
        if owner_state.disposed.load(Ordering::SeqCst) || !still_tracked {
            if waiting {
                let _ = self
                    .send_raw(
                        Some(popup_sid),
                        "Runtime.runIfWaitingForDebugger",
                        json!({}),
                    )
                    .await;
            }
            return;
        }

        let (globs, fetch_patterns) = {
            let guard = owner_state.interceptor.lock();
            (
                guard.blocker().cdp_blocked_url_globs(),
                guard.cdp_fetch_patterns(),
            )
        };
        let win_script = owner_state.env_window_script.lock().clone();
        let ua_params = owner_state.worker_ua_params.lock().clone();
        let tz = owner_state.env_timezone.lock().clone();
        let loc = owner_state.env_locale.lock().clone();
        let geo = owner_state.env_geolocation.lock().clone();
        let hc = *owner_state.env_hardware_concurrency.lock();
        let metrics = owner_state.env_device_metrics.lock().clone();
        let extra_headers = owner_state.env_extra_headers.lock().clone();
        let instant_anim = owner_state.instant_animations.load(Ordering::SeqCst);

        // Pipeline all setup commands onto the FIFO WebSocket before/alongside
        // `Runtime.runIfWaitingForDebugger`, and await their replies only AFTER
        // resuming the target. Chromium blocks renderer-domain replies (Network.enable,
        // Page.enable, Runtime.enable, Emulation.*, Page.addScriptToEvaluateOnNewDocument)
        // on `<a target="_blank">` targets until `Runtime.runIfWaitingForDebugger` arrives.
        let mut pending_calls: Vec<
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + '_>>,
        > = Vec::with_capacity(18);

        if let Some(ref ua) = ua_params {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Emulation.setUserAgentOverride",
                ua.clone(),
            )));
        }
        if let Some(ref hdrs) = extra_headers {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Network.setExtraHTTPHeaders",
                hdrs.clone(),
            )));
        }
        if !globs.is_empty() {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Network.setBlockedURLs",
                json!({ "urls": globs }),
            )));
        }
        if let Some(ref patterns) = fetch_patterns {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Fetch.enable",
                json!({ "patterns": patterns }),
            )));
        }
        if let Some(ref js) = win_script {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": js }),
            )));
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Page.addScriptToEvaluateOnNewDocument",
                json!({
                    "source": js,
                    "worldName": "mimikyu_isolated",
                }),
            )));
        }
        pending_calls.push(Box::pin(self.queue_raw(
            Some(popup_sid),
            "Network.enable",
            json!({}),
        )));
        pending_calls.push(Box::pin(self.queue_raw(
            Some(popup_sid),
            "Page.enable",
            json!({}),
        )));
        pending_calls.push(Box::pin(self.queue_raw(
            Some(popup_sid),
            "Runtime.enable",
            json!({}),
        )));
        pending_calls.push(Box::pin(self.queue_raw(
            Some(popup_sid),
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
            }),
        )));
        if let Some(ref tz_id) = tz {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Emulation.setTimezoneOverride",
                json!({ "timezoneId": tz_id }),
            )));
        }
        if let Some(ref loc_id) = loc {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Emulation.setLocaleOverride",
                json!({ "locale": loc_id }),
            )));
        }
        if let Some(ref g) = geo {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Emulation.setGeolocationOverride",
                g.clone(),
            )));
        }
        if let Some(hc_val) = hc {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Emulation.setHardwareConcurrencyOverride",
                json!({ "hardwareConcurrency": hc_val }),
            )));
        }
        if let Some(ref m) = metrics {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Emulation.setDeviceMetricsOverride",
                m.clone(),
            )));
        }
        let mut anim_preload_idx: Option<usize> = None;
        if instant_anim {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Animation.enable",
                json!({}),
            )));
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Animation.setPlaybackRate",
                json!({ "playbackRate": 10_000.0 }),
            )));
            anim_preload_idx = Some(pending_calls.len());
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Page.addScriptToEvaluateOnNewDocument",
                json!({
                    "source": INSTANT_ANIM_PRELOAD_JS,
                    "worldName": "mimikyu_isolated",
                }),
            )));
        }
        if waiting {
            pending_calls.push(Box::pin(self.queue_raw(
                Some(popup_sid),
                "Runtime.runIfWaitingForDebugger",
                json!({}),
            )));
        }

        let results = futures_util::future::join_all(pending_calls).await;

        if let Some(idx) = anim_preload_idx
            && let Some(Ok(v)) = results.get(idx)
            && let Some(script_id) = v["identifier"].as_str()
        {
            if owner_state.instant_animations.load(Ordering::SeqCst) {
                let old_id = owner_state
                    .child_anim_script_ids
                    .lock()
                    .insert(popup_sid.to_string(), script_id.to_string());
                if let Some(old_id) = old_id {
                    let _ = self
                        .send_raw(
                            Some(popup_sid),
                            "Page.removeScriptToEvaluateOnNewDocument",
                            json!({ "identifier": old_id }),
                        )
                        .await;
                }
            } else {
                let _ = self
                    .send_raw(
                        Some(popup_sid),
                        "Page.removeScriptToEvaluateOnNewDocument",
                        json!({ "identifier": script_id }),
                    )
                    .await;
                let _ = self
                    .send_raw(
                        Some(popup_sid),
                        "Animation.setPlaybackRate",
                        json!({ "playbackRate": 1.0 }),
                    )
                    .await;
            }
        }

        if let Some(ref js) = win_script {
            let _ = self
                .send_raw(
                    Some(popup_sid),
                    "Runtime.evaluate",
                    json!({
                        "expression": js,
                        "silent": true,
                    }),
                )
                .await;
        }
        if owner_state.instant_animations.load(Ordering::SeqCst) {
            apply_instant_anim_css_on_session(self, popup_sid, true).await;
        }
    }

    fn fail_all_pending(&self, reason: &str) {
        let mut pending = self.pending.lock();
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(MimikyuError::Transport(reason.to_string())));
        }
    }

    fn queue_raw_owned(
        self: &Arc<Self>,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
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
        let mut init_err: Option<MimikyuError> = None;
        {
            let mut pending = self.pending.lock();
            if self.closed.load(Ordering::SeqCst) {
                init_err = Some(MimikyuError::Disposed);
            } else {
                pending.insert(id, tx);
            }
        }

        if init_err.is_none() {
            match serde_json::to_string(&payload) {
                Ok(serialized) => {
                    if self.write_tx.send(serialized).is_err() {
                        self.pending.lock().remove(&id);
                        init_err = Some(MimikyuError::Transport(
                            "CDP writer channel closed".to_string(),
                        ));
                    }
                }
                Err(e) => {
                    self.pending.lock().remove(&id);
                    init_err = Some(e.into());
                }
            }
        }

        let conn = Arc::clone(self);
        let method_owned = method.to_string();
        async move {
            if let Some(err) = init_err {
                return Err(err);
            }
            match tokio::time::timeout(RPC_TIMEOUT, rx).await {
                Ok(Ok(res)) => res,
                Ok(Err(_)) => Err(MimikyuError::Transport(
                    "CDP response channel dropped".to_string(),
                )),
                Err(_) => {
                    conn.pending.lock().remove(&id);
                    Err(MimikyuError::Timeout {
                        timeout_ms: RPC_TIMEOUT.as_millis() as u64,
                        operation: format!("CDP {method_owned}"),
                    })
                }
            }
        }
    }

    fn queue_raw<'a>(
        &'a self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'a {
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
        let mut init_err: Option<MimikyuError> = None;
        {
            let mut pending = self.pending.lock();
            if self.closed.load(Ordering::SeqCst) {
                init_err = Some(MimikyuError::Disposed);
            } else {
                pending.insert(id, tx);
            }
        }

        if init_err.is_none() {
            match serde_json::to_string(&payload) {
                Ok(serialized) => {
                    if self.write_tx.send(serialized).is_err() {
                        self.pending.lock().remove(&id);
                        init_err = Some(MimikyuError::Transport(
                            "CDP writer channel closed".to_string(),
                        ));
                    }
                }
                Err(e) => {
                    self.pending.lock().remove(&id);
                    init_err = Some(e.into());
                }
            }
        }

        let method_owned = method.to_string();
        async move {
            if let Some(err) = init_err {
                return Err(err);
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
                        operation: format!("CDP {method_owned}"),
                    })
                }
            }
        }
    }

    async fn send_raw(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.queue_raw(session_id, method, params).await
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

#[derive(Debug, Clone)]
struct PendingCdpRequest {
    session_id: String,
    frame_id: Option<String>,
    loader_id: Option<String>,
    resource_type: String,
    received_response: bool,
    committed: bool,
}

/// Per-page/tab CDP session state.
///
/// Canonical lock acquisition order across `CdpSessionState` mutexes (never
/// acquire in reverse; prefer single-lock snapshots where possible):
///   1. `isolated_worlds`
///   2. `default_contexts`
///   3. `default_context_sessions`
///   4. `iframe_session_targets`
///   5. `iframe_sessions`
///
/// All other `CdpSessionState` mutexes (`worker_sessions`, `popup_sessions`,
/// `popup_child_sessions`, `popup_default_contexts`, `main_frame_id`, etc.)
/// are acquired singly and never nested.
#[derive(Debug, Clone)]
struct CdpScreencastState {
    active: bool,
    out_dir: PathBuf,
    format: ScreencastFormat,
    frame_count: u64,
    started_at_ms: u64,
}

impl CdpScreencastState {
    fn next_frame_path(&mut self) -> PathBuf {
        self.frame_count += 1;
        self.out_dir.join(format!(
            "frame-{:06}.{}",
            self.frame_count,
            self.format.file_extension()
        ))
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
    default_contexts: Mutex<HashMap<String, i64>>,
    default_context_sessions: Mutex<HashMap<String, String>>,
    iframe_sessions: Mutex<HashMap<String, String>>,
    iframe_session_targets: Mutex<HashMap<String, String>>,
    iframe_parent_sessions: Mutex<HashMap<String, String>>,
    popup_sessions: Mutex<HashMap<String, String>>,
    popup_child_sessions: Mutex<HashMap<String, String>>,
    popup_default_contexts: Mutex<HashMap<(String, String), i64>>,
    worker_sessions: Mutex<HashSet<String>>,
    pending_cdp_requests: Mutex<HashMap<String, PendingCdpRequest>>,
    finalized_doc_loaders: Mutex<VecDeque<String>>,
    telemetry: Arc<TelemetryStore>,
    interceptor: Mutex<NetworkInterceptor>,
    fetch_enabled: AtomicBool,
    instant_animations: AtomicBool,
    anim_script_id: AsyncMutex<Option<String>>,
    child_anim_script_ids: Mutex<HashMap<String, String>>,
    env_script_id: AsyncMutex<Option<String>>,
    env_isolated_script_id: AsyncMutex<Option<String>>,
    env_window_script: Mutex<Option<String>>,
    worker_init_script: Mutex<Option<String>>,
    worker_ua_params: Mutex<Option<Value>>,
    env_timezone: Mutex<Option<String>>,
    env_locale: Mutex<Option<String>>,
    env_geolocation: Mutex<Option<Value>>,
    env_hardware_concurrency: Mutex<Option<u32>>,
    env_device_metrics: Mutex<Option<Value>>,
    env_extra_headers: Mutex<Option<Value>>,
    env_profile: Mutex<Option<EnvProfile>>,
    pending_storage_scripts: Mutex<Vec<(String, String)>>,
    screencast: Mutex<Option<CdpScreencastState>>,
    _proxy_relay: Mutex<Option<ProxyRelay>>,
    load_notify: Notify,
    disposed: AtomicBool,
}

impl CdpSessionState {
    fn handle_event(
        &self,
        conn: &Arc<CdpConnection>,
        event_sid: &str,
        method: &str,
        params: Value,
        from_child: bool,
    ) {
        match method {
            "Page.screencastFrame" if !from_child => {
                let frame_session_id = params.get("sessionId").and_then(Value::as_i64);
                let data_b64 = params
                    .get("data")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let frame_path = {
                    let mut guard = self.screencast.lock();
                    if let Some(ref mut sc) = *guard
                        && sc.active
                    {
                        Some(sc.next_frame_path())
                    } else {
                        None
                    }
                };
                let conn_clone = Arc::clone(conn);
                let sid = event_sid.to_string();
                tokio::spawn(async move {
                    if let (Some(path), Some(b64)) = (frame_path, data_b64) {
                        match BASE64.decode(b64.as_bytes()) {
                            Ok(bytes) => {
                                if let Err(err) = tokio::fs::write(&path, bytes).await {
                                    warn!(
                                        path = %path.display(),
                                        error = %err,
                                        "failed writing screencast frame"
                                    );
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, "failed decoding base64 screencast frame");
                            }
                        }
                    }
                    if let Some(ack_id) = frame_session_id {
                        let _ = conn_clone
                            .send_raw(
                                Some(&sid),
                                "Page.screencastFrameAck",
                                json!({ "sessionId": ack_id }),
                            )
                            .await;
                    }
                });
            }
            "Page.frameNavigated" => {
                if let Some(frame) = params.get("frame")
                    && let Some(fid) = frame.get("id").and_then(|v| v.as_str())
                {
                    {
                        let mut iw = self.isolated_worlds.lock();
                        let mut dc = self.default_contexts.lock();
                        let mut dcs = self.default_context_sessions.lock();
                        iw.remove(fid);
                        dc.remove(fid);
                        dcs.remove(fid);
                    }
                    let nav_loader_id = frame.get("loaderId").and_then(|v| v.as_str());
                    if from_child {
                        self.iframe_sessions
                            .lock()
                            .entry(fid.to_string())
                            .or_insert_with(|| event_sid.to_string());
                        if let Some(lid) = nav_loader_id {
                            self.prune_pending_requests_for_frame_outside_loader(fid, lid);
                        }
                        self.mark_document_committed_and_prune_superseded(fid, nav_loader_id);
                    } else if frame.get("parentId").and_then(|v| v.as_str()).is_some() {
                        self.iframe_sessions.lock().remove(fid);
                        if let Some(lid) = nav_loader_id {
                            self.prune_pending_requests_for_frame_outside_loader(fid, lid);
                        }
                        self.mark_document_committed_and_prune_superseded(fid, nav_loader_id);
                    } else {
                        *self.main_frame_id.lock() = Some(fid.to_string());
                        if let Some(u) = frame.get("url").and_then(|v| v.as_str())
                            && let Ok(parsed) = url::Url::parse(u)
                        {
                            let origin = parsed.origin().ascii_serialization();
                            if origin != "null" {
                                *self.last_main_origin.lock() = Some(origin);
                            }
                        }
                        if let Some(lid) = nav_loader_id {
                            self.prune_pending_requests_outside_loader(lid);
                        }
                        self.mark_document_committed_and_prune_superseded(fid, nav_loader_id);
                    }
                }
            }
            "Page.frameDetached" => {
                let reason = params.get("reason").and_then(|v| v.as_str());
                if reason != Some("swap")
                    && let Some(fid) = params.get("frameId").and_then(|v| v.as_str())
                {
                    self.prune_pending_requests_for_frame(fid);
                }
            }
            "Runtime.executionContextCreated" => {
                if let Some(ctx) = params.get("context")
                    && let Some(ctx_id) = ctx.get("id").and_then(|v| v.as_i64())
                    && let Some(aux) = ctx.get("auxData")
                {
                    let is_default = aux.get("isDefault").and_then(|v| v.as_bool()) == Some(true);
                    let aux_type = aux.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let ctx_name = ctx.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(fid) = aux.get("frameId").and_then(|v| v.as_str()) {
                        if from_child {
                            self.iframe_sessions
                                .lock()
                                .insert(fid.to_string(), event_sid.to_string());
                        }
                        if is_default && aux_type == "default" {
                            let mut dc = self.default_contexts.lock();
                            let mut dcs = self.default_context_sessions.lock();
                            dc.insert(fid.to_string(), ctx_id);
                            if from_child {
                                dcs.insert(fid.to_string(), event_sid.to_string());
                            } else {
                                dcs.remove(fid);
                            }
                        } else if aux_type == "isolated" && ctx_name == "mimikyu_isolated" {
                            self.isolated_worlds.lock().insert(fid.to_string(), ctx_id);
                        }
                    }
                }
            }
            "Runtime.executionContextsCleared" => {
                self.clear_execution_contexts(event_sid, from_child);
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(ctx_id) = params.get("executionContextId").and_then(|v| v.as_i64()) {
                    self.destroy_execution_context(event_sid, from_child, ctx_id);
                }
            }
            "Page.loadEventFired" | "Page.frameStoppedLoading" => {
                if method == "Page.frameStoppedLoading"
                    && let Some(fid) = params.get("frameId").and_then(|v| v.as_str())
                {
                    let is_oopif = from_child || self.iframe_sessions.lock().contains_key(fid);
                    self.finalize_document_requests_for_frame(fid, is_oopif);
                } else if method == "Page.loadEventFired" && from_child {
                    let tid = self.iframe_session_targets.lock().get(event_sid).cloned();
                    if let Some(fid) = tid {
                        self.finalize_document_requests_for_frame(&fid, true);
                    }
                }
                if !from_child
                    && method == "Page.loadEventFired"
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
                if target_type == "page" {
                    conn.clear_foreground_target();
                }
                let target_id_opt = params
                    .get("targetInfo")
                    .and_then(|t| t.get("targetId"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let waiting = params
                    .get("waitingForDebugger")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(child_sid) = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                {
                    let is_iframe = target_type == "iframe";
                    let tid_for_spawn = target_id_opt.clone();
                    if is_iframe && let Some(tid) = target_id_opt {
                        self.iframe_sessions
                            .lock()
                            .insert(tid.clone(), child_sid.clone());
                        self.iframe_session_targets
                            .lock()
                            .insert(child_sid.clone(), tid);
                        if from_child {
                            self.iframe_parent_sessions
                                .lock()
                                .insert(child_sid.clone(), event_sid.to_string());
                        }
                        conn.iframe_owner_sessions
                            .write()
                            .insert(child_sid.clone(), self.session_id.clone());
                    }
                    let is_worker =
                        matches!(target_type, "worker" | "shared_worker" | "service_worker");
                    let is_dedicated_worker = target_type == "worker";
                    if is_worker {
                        self.worker_sessions.lock().insert(child_sid.clone());
                        if target_type == "shared_worker"
                            && let Some(ref tid) = tid_for_spawn
                        {
                            conn.shared_worker_targets
                                .write()
                                .insert(tid.clone(), self.session_id.clone());
                        }
                        if from_child {
                            self.iframe_parent_sessions
                                .lock()
                                .insert(child_sid.clone(), event_sid.to_string());
                        }
                        conn.iframe_owner_sessions
                            .write()
                            .insert(child_sid.clone(), self.session_id.clone());
                    }
                    let worker_script = if is_worker {
                        self.worker_init_script.lock().clone()
                    } else {
                        None
                    };
                    let worker_ua = if is_worker || is_iframe {
                        self.worker_ua_params.lock().clone()
                    } else {
                        None
                    };
                    let instant_anim = is_iframe && self.instant_animations.load(Ordering::SeqCst);
                    let iframe_env_script = if is_iframe {
                        self.env_window_script.lock().clone()
                    } else {
                        None
                    };
                    let iframe_tz = if is_iframe {
                        self.env_timezone.lock().clone()
                    } else {
                        None
                    };
                    let iframe_locale = if is_iframe {
                        self.env_locale.lock().clone()
                    } else {
                        None
                    };
                    let iframe_geo = if is_iframe {
                        self.env_geolocation.lock().clone()
                    } else {
                        None
                    };
                    let iframe_hc = if is_iframe {
                        *self.env_hardware_concurrency.lock()
                    } else {
                        None
                    };
                    let target_headers = if is_iframe || is_worker {
                        self.env_extra_headers.lock().clone()
                    } else {
                        None
                    };
                    if is_worker || is_iframe || waiting {
                        let conn_clone = Arc::clone(conn);
                        let owner_sid = self.session_id.clone();
                        tokio::spawn(async move {
                            if is_worker {
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Network.enable", json!({}))
                                    .await;
                                let owner_state_opt =
                                    conn_clone.sessions.read().get(&owner_sid).cloned();
                                if let Some(owner_state) = owner_state_opt {
                                    let (globs, fetch_patterns) = {
                                        let guard = owner_state.interceptor.lock();
                                        (
                                            guard.blocker().cdp_blocked_url_globs(),
                                            guard.cdp_fetch_patterns(),
                                        )
                                    };
                                    if !globs.is_empty() {
                                        let _ = conn_clone
                                            .send_raw(
                                                Some(&child_sid),
                                                "Network.setBlockedURLs",
                                                json!({ "urls": globs }),
                                            )
                                            .await;
                                    }
                                    if let Some(patterns) = fetch_patterns {
                                        let _ = conn_clone
                                            .send_raw(
                                                Some(&child_sid),
                                                "Fetch.enable",
                                                json!({ "patterns": patterns }),
                                            )
                                            .await;
                                    }
                                }
                                if is_dedicated_worker {
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Target.setAutoAttach",
                                            json!({
                                                "autoAttach": true,
                                                "waitForDebuggerOnStart": true,
                                                "flatten": true,
                                            }),
                                        )
                                        .await;
                                }
                            }
                            if is_iframe {
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Page.enable", json!({}))
                                    .await;
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Network.enable", json!({}))
                                    .await;
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Runtime.enable", json!({}))
                                    .await;
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "DOM.enable", json!({}))
                                    .await;
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Accessibility.enable", json!({}))
                                    .await;
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Target.setAutoAttach",
                                        json!({
                                            "autoAttach": true,
                                            "waitForDebuggerOnStart": true,
                                            "flatten": true,
                                        }),
                                    )
                                    .await;
                                let owner_state_opt =
                                    conn_clone.sessions.read().get(&owner_sid).cloned();
                                if let Some(owner_state) = owner_state_opt {
                                    let (globs, fetch_patterns) = {
                                        let guard = owner_state.interceptor.lock();
                                        (
                                            guard.blocker().cdp_blocked_url_globs(),
                                            guard.cdp_fetch_patterns(),
                                        )
                                    };
                                    if !globs.is_empty() {
                                        let _ = conn_clone
                                            .send_raw(
                                                Some(&child_sid),
                                                "Network.setBlockedURLs",
                                                json!({ "urls": globs }),
                                            )
                                            .await;
                                    }
                                    if let Some(patterns) = fetch_patterns {
                                        let _ = conn_clone
                                            .send_raw(
                                                Some(&child_sid),
                                                "Fetch.enable",
                                                json!({ "patterns": patterns }),
                                            )
                                            .await;
                                    }
                                }
                                if let Some(ref win_js) = iframe_env_script {
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Page.addScriptToEvaluateOnNewDocument",
                                            json!({ "source": win_js }),
                                        )
                                        .await;
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Page.addScriptToEvaluateOnNewDocument",
                                            json!({
                                                "source": win_js,
                                                "worldName": "mimikyu_isolated",
                                            }),
                                        )
                                        .await;
                                }
                                if instant_anim {
                                    let _ = conn_clone
                                        .send_raw(Some(&child_sid), "Animation.enable", json!({}))
                                        .await;
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Animation.setPlaybackRate",
                                            json!({ "playbackRate": 10_000.0 }),
                                        )
                                        .await;
                                    let preload_res = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Page.addScriptToEvaluateOnNewDocument",
                                            json!({
                                                "source": INSTANT_ANIM_PRELOAD_JS,
                                                "worldName": "mimikyu_isolated",
                                            }),
                                        )
                                        .await;
                                    let preload_res =
                                        match preload_res {
                                            Ok(v) if v["identifier"].as_str().is_some() => Ok(v),
                                            _ => conn_clone
                                                .send_raw(
                                                    Some(&child_sid),
                                                    "Page.addScriptToEvaluateOnNewDocument",
                                                    json!({ "source": INSTANT_ANIM_PRELOAD_JS }),
                                                )
                                                .await,
                                        };
                                    if let Ok(v) = preload_res
                                        && let Some(script_id) = v["identifier"].as_str()
                                    {
                                        let owner_state_opt =
                                            conn_clone.sessions.read().get(&owner_sid).cloned();
                                        if let Some(owner_state) = owner_state_opt {
                                            if owner_state.instant_animations.load(Ordering::SeqCst)
                                            {
                                                let old_id = owner_state
                                                    .child_anim_script_ids
                                                    .lock()
                                                    .insert(
                                                        child_sid.clone(),
                                                        script_id.to_string(),
                                                    );
                                                if let Some(old_id) = old_id {
                                                    let _ = conn_clone
                                                        .send_raw(
                                                            Some(&child_sid),
                                                            "Page.removeScriptToEvaluateOnNewDocument",
                                                            json!({ "identifier": old_id }),
                                                        )
                                                        .await;
                                                }
                                            } else {
                                                let _ = conn_clone
                                                    .send_raw(
                                                        Some(&child_sid),
                                                        "Page.removeScriptToEvaluateOnNewDocument",
                                                        json!({ "identifier": script_id }),
                                                    )
                                                    .await;
                                                let _ = conn_clone
                                                    .send_raw(
                                                        Some(&child_sid),
                                                        "Animation.setPlaybackRate",
                                                        json!({ "playbackRate": 1.0 }),
                                                    )
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(ua) = worker_ua {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Emulation.setUserAgentOverride",
                                        ua,
                                    )
                                    .await;
                            }
                            if let Some(tz) = iframe_tz {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Emulation.setTimezoneOverride",
                                        json!({ "timezoneId": tz }),
                                    )
                                    .await;
                            }
                            if let Some(loc) = iframe_locale {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Emulation.setLocaleOverride",
                                        json!({ "locale": loc }),
                                    )
                                    .await;
                            }
                            if let Some(geo) = iframe_geo {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Emulation.setGeolocationOverride",
                                        geo,
                                    )
                                    .await;
                            }
                            if let Some(hc) = iframe_hc {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Emulation.setHardwareConcurrencyOverride",
                                        json!({ "hardwareConcurrency": hc }),
                                    )
                                    .await;
                            }
                            if let Some(hdrs) = target_headers {
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Network.setExtraHTTPHeaders", hdrs)
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
                            } else if is_iframe
                                && let Some(tid) = tid_for_spawn
                                && let Ok(rs) = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Runtime.evaluate",
                                        json!({
                                            "expression": "document.readyState",
                                            "returnByValue": true,
                                        }),
                                    )
                                    .await
                                && rs["result"]["value"].as_str() == Some("complete")
                                && let Some(owner_state) =
                                    conn_clone.sessions.read().get(&owner_sid).cloned()
                            {
                                owner_state.finalize_document_requests_for_frame(&tid, true);
                            }
                            if is_iframe
                                && conn_clone
                                    .sessions
                                    .read()
                                    .get(&owner_sid)
                                    .is_some_and(|st| st.instant_animations.load(Ordering::SeqCst))
                            {
                                apply_instant_anim_css_on_session(&conn_clone, &child_sid, true)
                                    .await;
                            }
                        });
                    }
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(child_sid) = params.get("sessionId").and_then(|v| v.as_str()) {
                    self.prune_detached_iframe_sessions(conn, child_sid);
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
                                        let ln = f
                                            .get("lineNumber")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
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
                let frame_id = params
                    .get("frameId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let loader_id = params
                    .get("loaderId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let request_headers = json_map_to_btree(req.get("headers"));

                let redir = params.get("redirectResponse").filter(|v| !v.is_null());
                let is_redirect = redir.is_some();
                let redir_status = redir
                    .and_then(|r| r.get("status"))
                    .and_then(|v| v.as_u64())
                    .map(|s| s as u16);
                let redir_mime = redir
                    .and_then(|r| r.get("mimeType"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let redir_headers = json_map_to_btree(redir.and_then(|r| r.get("headers")));

                {
                    let mut pending = self.pending_cdp_requests.lock();
                    if !is_redirect && pending.contains_key(req_id) {
                        return;
                    }
                    if from_child && resource_type == "Document" {
                        if loader_id
                            .as_deref()
                            .is_some_and(|lid| self.is_doc_loader_finalized(lid))
                        {
                            return;
                        }
                        if loader_id.is_some()
                            && pending.values().any(|e| {
                                e.resource_type == "Document"
                                    && e.frame_id == frame_id
                                    && e.loader_id == loader_id
                            })
                        {
                            return;
                        }
                    }
                    pending.insert(
                        req_id.to_string(),
                        PendingCdpRequest {
                            session_id: event_sid.to_string(),
                            frame_id,
                            loader_id,
                            resource_type: resource_type.clone(),
                            received_response: false,
                            committed: false,
                        },
                    );
                }

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
                let is_pending = {
                    let mut pending = self.pending_cdp_requests.lock();
                    if let Some(entry) = pending.get_mut(req_id) {
                        entry.received_response = true;
                        true
                    } else {
                        false
                    }
                };
                if from_child && !is_pending {
                    return;
                }
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
                let removed = self.pending_cdp_requests.lock().remove(req_id);
                let was_pending = if let Some(entry) = removed {
                    if entry.resource_type == "Document" {
                        self.record_finalized_doc_loader(entry.loader_id.as_deref());
                    }
                    true
                } else {
                    false
                };
                if !from_child || was_pending {
                    self.telemetry.on_request_finished(req_id);
                }
            }
            "Network.loadingFailed" => {
                let Some(req_id) = params.get("requestId").and_then(|v| v.as_str()) else {
                    return;
                };
                let removed = self.pending_cdp_requests.lock().remove(req_id);
                let was_pending = if let Some(entry) = removed {
                    if entry.resource_type == "Document" {
                        self.record_finalized_doc_loader(entry.loader_id.as_deref());
                    }
                    true
                } else {
                    false
                };
                if !from_child || was_pending {
                    let error_text = params
                        .get("errorText")
                        .and_then(|v| v.as_str())
                        .or_else(|| params.get("blockedReason").and_then(|v| v.as_str()))
                        .map(ToString::to_string);
                    self.telemetry.on_request_failed(req_id, error_text);
                }
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
                let sid = event_sid.to_string();
                let network_id = params
                    .get("networkId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                if matches!(
                    decision,
                    InterceptDecision::Continue | InterceptDecision::ModifyHeaders { .. }
                ) && let Some(ref nid) = network_id
                    && let Some(fid) = params.get("frameId").and_then(|v| v.as_str())
                    && conn.shared_worker_targets.read().contains_key(fid)
                {
                    let mut q = conn.handled_sw_fetch_nids.lock();
                    if q.len() >= 512 {
                        q.pop_front();
                    }
                    q.push_back(nid.clone());
                }
                let telem = Arc::clone(&self.telemetry);

                tokio::spawn(async move {
                    let res = match decision {
                        InterceptDecision::Block { reason } => {
                            conn_clone
                                .send_raw(
                                    Some(&sid),
                                    "Fetch.failRequest",
                                    json!({ "requestId": fetch_id, "errorReason": reason }),
                                )
                                .await
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
                            conn_clone
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
                                .await
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
                                conn_clone
                                    .send_raw(
                                        Some(&sid),
                                        "Fetch.continueRequest",
                                        json!({ "requestId": fetch_id }),
                                    )
                                    .await
                            } else {
                                Ok(Value::Null)
                            }
                        }
                        InterceptDecision::Continue => {
                            conn_clone
                                .send_raw(
                                    Some(&sid),
                                    "Fetch.continueRequest",
                                    json!({ "requestId": fetch_id }),
                                )
                                .await
                        }
                    };
                    if let Err(e) = res
                        && let Some(nid) = network_id
                    {
                        telem.on_intercept_resolved_locally(
                            &nid,
                            None,
                            BTreeMap::new(),
                            true,
                            Some(e.to_string()),
                        );
                    }
                });
            }
            _ => {}
        }
    }

    fn handle_popup_event(
        &self,
        conn: &Arc<CdpConnection>,
        event_sid: &str,
        method: &str,
        params: Value,
    ) {
        match method {
            "Runtime.consoleAPICalled"
            | "Runtime.exceptionThrown"
            | "Network.requestWillBeSent"
            | "Network.responseReceived"
            | "Network.loadingFinished"
            | "Network.loadingFailed"
            | "Fetch.requestPaused" => {
                self.handle_event(conn, event_sid, method, params, true);
            }
            "Runtime.executionContextCreated" => {
                if let Some(ctx) = params.get("context")
                    && let Some(ctx_id) = ctx.get("id").and_then(|v| v.as_i64())
                    && let Some(aux) = ctx.get("auxData")
                {
                    let is_default = aux.get("isDefault").and_then(|v| v.as_bool()) == Some(true);
                    let aux_type = aux.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if is_default
                        && aux_type == "default"
                        && let Some(fid) = aux.get("frameId").and_then(|v| v.as_str())
                    {
                        self.popup_default_contexts
                            .lock()
                            .insert((event_sid.to_string(), fid.to_string()), ctx_id);
                    }
                }
            }
            "Runtime.executionContextsCleared" => {
                self.popup_default_contexts
                    .lock()
                    .retain(|(sid, _), _| sid != event_sid);
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(ctx_id) = params.get("executionContextId").and_then(|v| v.as_i64()) {
                    self.popup_default_contexts
                        .lock()
                        .retain(|(sid, _), &mut v| !(sid == event_sid && v == ctx_id));
                }
            }
            "Page.loadEventFired" | "Page.frameStoppedLoading" => {
                if method == "Page.frameStoppedLoading"
                    && let Some(fid) = params.get("frameId").and_then(|v| v.as_str())
                {
                    self.finalize_document_requests_for_frame(fid, true);
                }
                self.load_notify.notify_waiters();
            }
            "Target.attachedToTarget" => {
                let target_type = params
                    .get("targetInfo")
                    .and_then(|t| t.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if target_type == "page" {
                    conn.clear_foreground_target();
                }
                let waiting = params
                    .get("waitingForDebugger")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let Some(child_sid) = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                else {
                    return;
                };
                let is_iframe = target_type == "iframe";
                let is_worker =
                    matches!(target_type, "worker" | "shared_worker" | "service_worker");
                let is_dedicated_worker = target_type == "worker";
                if is_iframe || is_worker {
                    self.popup_child_sessions
                        .lock()
                        .insert(child_sid.clone(), event_sid.to_string());
                    conn.popup_owner_sessions
                        .write()
                        .insert(child_sid.clone(), self.session_id.clone());
                }
                if is_worker {
                    self.worker_sessions.lock().insert(child_sid.clone());
                    if target_type == "shared_worker"
                        && let Some(tid) = params
                            .get("targetInfo")
                            .and_then(|t| t.get("targetId"))
                            .and_then(|v| v.as_str())
                    {
                        conn.shared_worker_targets
                            .write()
                            .insert(tid.to_string(), self.session_id.clone());
                    }
                }
                let worker_script = is_worker
                    .then(|| self.worker_init_script.lock().clone())
                    .flatten();
                let worker_ua = (is_worker || is_iframe)
                    .then(|| self.worker_ua_params.lock().clone())
                    .flatten();
                let instant_anim = is_iframe && self.instant_animations.load(Ordering::SeqCst);
                let iframe_env_script = is_iframe
                    .then(|| self.env_window_script.lock().clone())
                    .flatten();
                let iframe_tz = is_iframe
                    .then(|| self.env_timezone.lock().clone())
                    .flatten();
                let iframe_locale = is_iframe.then(|| self.env_locale.lock().clone()).flatten();
                let iframe_geo = is_iframe
                    .then(|| self.env_geolocation.lock().clone())
                    .flatten();
                let iframe_hc = is_iframe
                    .then(|| *self.env_hardware_concurrency.lock())
                    .flatten();
                let target_headers = (is_iframe || is_worker)
                    .then(|| self.env_extra_headers.lock().clone())
                    .flatten();

                if (is_worker || is_iframe || waiting)
                    && tokio::runtime::Handle::try_current().is_ok()
                {
                    let conn_clone = Arc::clone(conn);
                    let owner_sid = self.session_id.clone();
                    tokio::spawn(async move {
                        if is_worker {
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Network.enable", json!({}))
                                .await;
                            let owner_state_opt =
                                conn_clone.sessions.read().get(&owner_sid).cloned();
                            if let Some(owner_state) = owner_state_opt {
                                let (globs, fetch_patterns) = {
                                    let guard = owner_state.interceptor.lock();
                                    (
                                        guard.blocker().cdp_blocked_url_globs(),
                                        guard.cdp_fetch_patterns(),
                                    )
                                };
                                if !globs.is_empty() {
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Network.setBlockedURLs",
                                            json!({ "urls": globs }),
                                        )
                                        .await;
                                }
                                if let Some(patterns) = fetch_patterns {
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Fetch.enable",
                                            json!({ "patterns": patterns }),
                                        )
                                        .await;
                                }
                            }
                            if is_dedicated_worker {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Target.setAutoAttach",
                                        json!({
                                            "autoAttach": true,
                                            "waitForDebuggerOnStart": true,
                                            "flatten": true,
                                        }),
                                    )
                                    .await;
                            }
                        }
                        if is_iframe {
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Page.enable", json!({}))
                                .await;
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Network.enable", json!({}))
                                .await;
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Runtime.enable", json!({}))
                                .await;
                            let _ = conn_clone
                                .send_raw(
                                    Some(&child_sid),
                                    "Target.setAutoAttach",
                                    json!({
                                        "autoAttach": true,
                                        "waitForDebuggerOnStart": true,
                                        "flatten": true,
                                    }),
                                )
                                .await;
                            let owner_state_opt =
                                conn_clone.sessions.read().get(&owner_sid).cloned();
                            if let Some(owner_state) = owner_state_opt {
                                let (globs, fetch_patterns) = {
                                    let guard = owner_state.interceptor.lock();
                                    (
                                        guard.blocker().cdp_blocked_url_globs(),
                                        guard.cdp_fetch_patterns(),
                                    )
                                };
                                if !globs.is_empty() {
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Network.setBlockedURLs",
                                            json!({ "urls": globs }),
                                        )
                                        .await;
                                }
                                if let Some(patterns) = fetch_patterns {
                                    let _ = conn_clone
                                        .send_raw(
                                            Some(&child_sid),
                                            "Fetch.enable",
                                            json!({ "patterns": patterns }),
                                        )
                                        .await;
                                }
                            }
                            if let Some(ref win_js) = iframe_env_script {
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Page.addScriptToEvaluateOnNewDocument",
                                        json!({ "source": win_js }),
                                    )
                                    .await;
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Page.addScriptToEvaluateOnNewDocument",
                                        json!({
                                            "source": win_js,
                                            "worldName": "mimikyu_isolated",
                                        }),
                                    )
                                    .await;
                            }
                            if instant_anim {
                                let _ = conn_clone
                                    .send_raw(Some(&child_sid), "Animation.enable", json!({}))
                                    .await;
                                let _ = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Animation.setPlaybackRate",
                                        json!({ "playbackRate": 10_000.0 }),
                                    )
                                    .await;
                                let preload_res = conn_clone
                                    .send_raw(
                                        Some(&child_sid),
                                        "Page.addScriptToEvaluateOnNewDocument",
                                        json!({
                                            "source": INSTANT_ANIM_PRELOAD_JS,
                                            "worldName": "mimikyu_isolated",
                                        }),
                                    )
                                    .await;
                                let preload_res = match preload_res {
                                    Ok(v) if v["identifier"].as_str().is_some() => Ok(v),
                                    _ => {
                                        conn_clone
                                            .send_raw(
                                                Some(&child_sid),
                                                "Page.addScriptToEvaluateOnNewDocument",
                                                json!({ "source": INSTANT_ANIM_PRELOAD_JS }),
                                            )
                                            .await
                                    }
                                };
                                if let Ok(v) = preload_res
                                    && let Some(script_id) = v["identifier"].as_str()
                                {
                                    let owner_state_opt =
                                        conn_clone.sessions.read().get(&owner_sid).cloned();
                                    if let Some(owner_state) = owner_state_opt {
                                        if owner_state.instant_animations.load(Ordering::SeqCst) {
                                            let old_id = owner_state
                                                .child_anim_script_ids
                                                .lock()
                                                .insert(child_sid.clone(), script_id.to_string());
                                            if let Some(old_id) = old_id {
                                                let _ = conn_clone
                                                    .send_raw(
                                                        Some(&child_sid),
                                                        "Page.removeScriptToEvaluateOnNewDocument",
                                                        json!({ "identifier": old_id }),
                                                    )
                                                    .await;
                                            }
                                        } else {
                                            let _ = conn_clone
                                                .send_raw(
                                                    Some(&child_sid),
                                                    "Page.removeScriptToEvaluateOnNewDocument",
                                                    json!({ "identifier": script_id }),
                                                )
                                                .await;
                                            let _ = conn_clone
                                                .send_raw(
                                                    Some(&child_sid),
                                                    "Animation.setPlaybackRate",
                                                    json!({ "playbackRate": 1.0 }),
                                                )
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(ua) = worker_ua {
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Emulation.setUserAgentOverride", ua)
                                .await;
                        }
                        if let Some(tz) = iframe_tz {
                            let _ = conn_clone
                                .send_raw(
                                    Some(&child_sid),
                                    "Emulation.setTimezoneOverride",
                                    json!({ "timezoneId": tz }),
                                )
                                .await;
                        }
                        if let Some(loc) = iframe_locale {
                            let _ = conn_clone
                                .send_raw(
                                    Some(&child_sid),
                                    "Emulation.setLocaleOverride",
                                    json!({ "locale": loc }),
                                )
                                .await;
                        }
                        if let Some(geo) = iframe_geo {
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Emulation.setGeolocationOverride", geo)
                                .await;
                        }
                        if let Some(hc) = iframe_hc {
                            let _ = conn_clone
                                .send_raw(
                                    Some(&child_sid),
                                    "Emulation.setHardwareConcurrencyOverride",
                                    json!({ "hardwareConcurrency": hc }),
                                )
                                .await;
                        }
                        if let Some(hdrs) = target_headers {
                            let _ = conn_clone
                                .send_raw(Some(&child_sid), "Network.setExtraHTTPHeaders", hdrs)
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
                        if is_iframe
                            && conn_clone
                                .sessions
                                .read()
                                .get(&owner_sid)
                                .is_some_and(|st| st.instant_animations.load(Ordering::SeqCst))
                        {
                            apply_instant_anim_css_on_session(&conn_clone, &child_sid, true).await;
                        }
                    });
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(child_sid) = params.get("sessionId").and_then(|v| v.as_str()) {
                    self.prune_detached_popup_session(conn, child_sid);
                }
            }
            _ => {}
        }
    }

    fn prune_destroyed_popup_target(&self, conn: &Arc<CdpConnection>, popup_tid: &str) {
        let popup_sid_opt = self.popup_sessions.lock().remove(popup_tid);
        if let Some(popup_sid) = popup_sid_opt {
            self.prune_detached_popup_session(conn, &popup_sid);
        }
    }

    fn prune_detached_popup_session(&self, conn: &Arc<CdpConnection>, root_sid: &str) {
        let mut sids_to_clean: HashSet<String> = HashSet::new();
        sids_to_clean.insert(root_sid.to_string());
        {
            let mut child_map = self.popup_child_sessions.lock();
            loop {
                let mut added = false;
                for (csid, psid) in child_map.iter() {
                    if sids_to_clean.contains(psid) && sids_to_clean.insert(csid.clone()) {
                        added = true;
                    }
                }
                if !added {
                    break;
                }
            }
            child_map.retain(|csid, psid| {
                !sids_to_clean.contains(csid) && !sids_to_clean.contains(psid)
            });
        }
        let mut removed_popup_tids = Vec::new();
        self.popup_sessions.lock().retain(|tid, sid| {
            if sids_to_clean.contains(sid) {
                removed_popup_tids.push(tid.clone());
                false
            } else {
                true
            }
        });
        if !removed_popup_tids.is_empty() {
            let mut target_owners = conn.popup_target_owners.write();
            for tid in &removed_popup_tids {
                target_owners.remove(tid);
            }
        }
        {
            let mut owners = conn.popup_owner_sessions.write();
            for sid in &sids_to_clean {
                owners.remove(sid);
            }
        }
        {
            let mut ws = self.worker_sessions.lock();
            for sid in &sids_to_clean {
                ws.remove(sid);
            }
        }
        {
            let mut anim_scripts = self.child_anim_script_ids.lock();
            for sid in &sids_to_clean {
                anim_scripts.remove(sid);
            }
        }
        self.popup_default_contexts
            .lock()
            .retain(|(sid, _), _| !sids_to_clean.contains(sid));
        self.prune_pending_requests_for_detached(&sids_to_clean, &HashSet::new());
    }

    fn invalidate_frame_contexts(
        &self,
        frame_id: Option<&str>,
        resolved_fid: Option<&str>,
        stale_isolated_ctx: Option<i64>,
        stale_default_ctx: Option<i64>,
    ) {
        // Respect canonical lock ordering (`cdp.rs` header comment):
        //   - `main_frame_id` is acquired singly and never nested.
        //   - Nested locks follow: 1. `isolated_worlds`, 2. `default_contexts`,
        //     3. `default_context_sessions`.
        let prev_main = if frame_id.is_none() {
            self.main_frame_id.lock().take()
        } else {
            None
        };

        let mut iw = self.isolated_worlds.lock();
        let mut dc = self.default_contexts.lock();
        let mut dcs = self.default_context_sessions.lock();
        for fid in [frame_id, resolved_fid, prev_main.as_deref()]
            .into_iter()
            .flatten()
        {
            match stale_isolated_ctx {
                Some(stale_id) => {
                    if iw.get(fid) == Some(&stale_id) {
                        iw.remove(fid);
                    }
                }
                None => {
                    iw.remove(fid);
                }
            }

            // `default_contexts` is populated passively by `Runtime.executionContextCreated`
            // events. Compare-and-remove prevents an older RPC failure from wiping a newer
            // default context ID that already arrived for a same-process child frame, while
            // still clearing stale main-frame caches when `frame_id.is_none()`.
            let clear_default = match stale_default_ctx {
                Some(stale_id) => dc.get(fid) == Some(&stale_id),
                None => frame_id.is_none(),
            };
            if clear_default {
                dc.remove(fid);
                dcs.remove(fid);
            }
        }
    }

    fn clear_execution_contexts(&self, event_sid: &str, from_child: bool) {
        let iframe_fids = self.iframe_sessions.lock().clone();
        if from_child {
            self.isolated_worlds
                .lock()
                .retain(|fid, _| iframe_fids.get(fid).map(String::as_str) != Some(event_sid));
            let mut dc = self.default_contexts.lock();
            let mut dcs = self.default_context_sessions.lock();
            dc.retain(|fid, _| {
                iframe_fids.get(fid).map(String::as_str) != Some(event_sid)
                    && dcs.get(fid).map(String::as_str) != Some(event_sid)
            });
            dcs.retain(|_, sid| sid != event_sid);
        } else {
            self.isolated_worlds
                .lock()
                .retain(|fid, _| iframe_fids.contains_key(fid));
            let mut dc = self.default_contexts.lock();
            let dcs = self.default_context_sessions.lock();
            dc.retain(|fid, _| iframe_fids.contains_key(fid) || dcs.contains_key(fid));
        }
    }

    fn destroy_execution_context(&self, event_sid: &str, from_child: bool, ctx_id: i64) {
        let iframe_fids = self.iframe_sessions.lock().clone();
        let mut iw = self.isolated_worlds.lock();
        let mut dc = self.default_contexts.lock();
        let mut dcs = self.default_context_sessions.lock();
        let matches_owner = |fid: &String| -> bool {
            if from_child {
                iframe_fids.get(fid).map(String::as_str) == Some(event_sid)
                    || dcs.get(fid).map(String::as_str) == Some(event_sid)
            } else {
                !iframe_fids.contains_key(fid) && !dcs.contains_key(fid)
            }
        };
        iw.retain(|fid, &mut v| !(v == ctx_id && matches_owner(fid)));
        let mut removed_fids = Vec::new();
        dc.retain(|fid, &mut v| {
            let drop_it = v == ctx_id && matches_owner(fid);
            if drop_it {
                removed_fids.push(fid.clone());
            }
            !drop_it
        });
        for fid in removed_fids {
            dcs.remove(&fid);
        }
    }

    fn collect_same_proc_env_contexts(
        &self,
        main_fid_opt: Option<&str>,
    ) -> Vec<(i64, Option<String>)> {
        // Exclude only OOPIF root frame IDs (`iframe_session_targets.values()`),
        // since `set_env_overrides` already evaluates `win_script` on each
        // child session's root frame without `contextId`. Nested same-process
        // cross-origin frames hosted inside an OOPIF share `child_sid` and are
        // NOT in `iframe_session_targets`, so they must be updated by `contextId`.
        let oopif_roots: HashSet<String> = self
            .iframe_session_targets
            .lock()
            .values()
            .cloned()
            .collect();
        let dc = self.default_contexts.lock();
        let dcs = self.default_context_sessions.lock();
        dc.iter()
            .filter(|(fid, _)| {
                main_fid_opt != Some(fid.as_str()) && !oopif_roots.contains(fid.as_str())
            })
            .map(|(fid, &cid)| (cid, dcs.get(fid).cloned()))
            .collect()
    }

    fn prune_iframe_context_maps(
        &self,
        sids_to_clean: &HashSet<String>,
        removed_tids: HashSet<String>,
    ) -> HashSet<String> {
        let mut fids_to_clean = Vec::new();
        self.iframe_sessions.lock().retain(|fid, sid| {
            if sids_to_clean.contains(sid) || removed_tids.contains(fid) {
                fids_to_clean.push(fid.clone());
                false
            } else {
                true
            }
        });
        let mut all_fids = removed_tids;
        let mut iw = self.isolated_worlds.lock();
        let mut dc = self.default_contexts.lock();
        let mut dcs = self.default_context_sessions.lock();
        for fid in fids_to_clean {
            iw.remove(&fid);
            dc.remove(&fid);
            dcs.remove(&fid);
            all_fids.insert(fid);
        }
        dc.retain(|fid, _| {
            if dcs.get(fid).is_some_and(|sid| sids_to_clean.contains(sid)) {
                dcs.remove(fid);
                iw.remove(fid);
                all_fids.insert(fid.clone());
                false
            } else {
                true
            }
        });
        all_fids
    }

    fn prune_detached_iframe_sessions(&self, conn: &Arc<CdpConnection>, root_sid: &str) {
        let mut sids_to_clean: HashSet<String> = HashSet::new();
        sids_to_clean.insert(root_sid.to_string());
        {
            let mut parent_map = self.iframe_parent_sessions.lock();
            loop {
                let mut added = false;
                for (csid, psid) in parent_map.iter() {
                    if sids_to_clean.contains(psid) && sids_to_clean.insert(csid.clone()) {
                        added = true;
                    }
                }
                if !added {
                    break;
                }
            }
            parent_map.retain(|csid, psid| {
                !sids_to_clean.contains(csid) && !sids_to_clean.contains(psid)
            });
        }
        {
            let mut owners = conn.iframe_owner_sessions.write();
            for sid in &sids_to_clean {
                owners.remove(sid);
            }
        }
        let mut removed_tids: HashSet<String> = HashSet::new();
        {
            let mut targets = self.iframe_session_targets.lock();
            for sid in &sids_to_clean {
                if let Some(tid) = targets.remove(sid) {
                    removed_tids.insert(tid);
                }
            }
        }
        let all_fids = self.prune_iframe_context_maps(&sids_to_clean, removed_tids);
        {
            let mut ws = self.worker_sessions.lock();
            for sid in &sids_to_clean {
                ws.remove(sid);
            }
        }
        {
            let mut anim_scripts = self.child_anim_script_ids.lock();
            for sid in &sids_to_clean {
                anim_scripts.remove(sid);
            }
        }
        self.prune_pending_requests_for_detached(&sids_to_clean, &all_fids);
    }

    fn record_finalized_doc_loader(&self, loader_id: Option<&str>) {
        let Some(lid) = loader_id else {
            return;
        };
        let mut guard = self.finalized_doc_loaders.lock();
        if guard.iter().any(|existing| existing == lid) {
            return;
        }
        if guard.len() >= 256 {
            guard.pop_front();
        }
        guard.push_back(lid.to_string());
    }

    fn is_doc_loader_finalized(&self, loader_id: &str) -> bool {
        self.finalized_doc_loaders
            .lock()
            .iter()
            .any(|existing| existing == loader_id)
    }

    fn mark_document_committed_and_prune_superseded(&self, fid: &str, nav_loader_id: Option<&str>) {
        let mut to_finish: Vec<(String, bool, Option<String>)> = Vec::new();
        {
            let mut pending = self.pending_cdp_requests.lock();
            pending.retain(|req_id, entry| {
                if entry.frame_id.as_deref() != Some(fid) {
                    return true;
                }
                if entry.resource_type == "Document" {
                    if nav_loader_id.is_some() && entry.loader_id.as_deref() == nav_loader_id {
                        entry.committed = true;
                        true
                    } else if nav_loader_id.is_some() {
                        to_finish.push((
                            req_id.clone(),
                            entry.received_response,
                            entry.loader_id.clone(),
                        ));
                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            });
        }
        for (req_id, ok, lid) in to_finish {
            self.record_finalized_doc_loader(lid.as_deref());
            if ok {
                self.telemetry.on_request_finished(&req_id);
            } else {
                self.telemetry
                    .on_request_failed(&req_id, Some("navigation superseded".to_string()));
            }
        }
    }

    fn finalize_document_requests_for_frame(&self, fid: &str, is_oopif: bool) {
        let mut to_finish: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut pending = self.pending_cdp_requests.lock();
            pending.retain(|req_id, entry| {
                if entry.frame_id.as_deref() == Some(fid)
                    && entry.resource_type == "Document"
                    && (is_oopif || entry.committed || entry.received_response)
                {
                    to_finish.push((req_id.clone(), entry.loader_id.clone()));
                    false
                } else {
                    true
                }
            });
        }
        for (req_id, lid) in to_finish {
            self.record_finalized_doc_loader(lid.as_deref());
            self.telemetry.on_request_finished(&req_id);
        }
    }

    fn prune_pending_requests_for_frame_outside_loader(&self, fid: &str, new_loader_id: &str) {
        let mut to_fail: Vec<String> = Vec::new();
        {
            let mut pending = self.pending_cdp_requests.lock();
            pending.retain(|req_id, entry| {
                if entry.frame_id.as_deref() == Some(fid)
                    && entry.resource_type != "Document"
                    && entry.loader_id.as_deref() != Some(new_loader_id)
                {
                    to_fail.push(req_id.clone());
                    false
                } else {
                    true
                }
            });
        }
        for req_id in to_fail {
            self.telemetry
                .on_request_failed(&req_id, Some("frame navigated".to_string()));
        }
    }

    fn prune_pending_requests_for_frame(&self, fid: &str) {
        let mut to_fail: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut pending = self.pending_cdp_requests.lock();
            pending.retain(|req_id, entry| {
                if entry.frame_id.as_deref() == Some(fid) {
                    let lid = (entry.resource_type == "Document")
                        .then(|| entry.loader_id.clone())
                        .flatten();
                    to_fail.push((req_id.clone(), lid));
                    false
                } else {
                    true
                }
            });
        }
        for (req_id, lid) in to_fail {
            self.record_finalized_doc_loader(lid.as_deref());
            self.telemetry
                .on_request_failed(&req_id, Some("frame detached".to_string()));
        }
    }

    fn prune_pending_requests_for_detached(&self, sids: &HashSet<String>, fids: &HashSet<String>) {
        let mut to_fail: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut pending = self.pending_cdp_requests.lock();
            pending.retain(|req_id, entry| {
                let sid_match = sids.contains(&entry.session_id);
                let fid_match = entry
                    .frame_id
                    .as_deref()
                    .is_some_and(|fid| fids.contains(fid));
                if sid_match || fid_match {
                    let lid = (entry.resource_type == "Document")
                        .then(|| entry.loader_id.clone())
                        .flatten();
                    to_fail.push((req_id.clone(), lid));
                    false
                } else {
                    true
                }
            });
        }
        for (req_id, lid) in to_fail {
            self.record_finalized_doc_loader(lid.as_deref());
            self.telemetry
                .on_request_failed(&req_id, Some("target detached".to_string()));
        }
    }

    fn prune_pending_requests_outside_loader(&self, new_loader_id: &str) {
        let mut to_fail: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut pending = self.pending_cdp_requests.lock();
            pending.retain(|req_id, entry| {
                if entry.loader_id.as_deref() != Some(new_loader_id) {
                    let lid = (entry.resource_type == "Document")
                        .then(|| entry.loader_id.clone())
                        .flatten();
                    to_fail.push((req_id.clone(), lid));
                    false
                } else {
                    true
                }
            });
        }
        for (req_id, lid) in to_fail {
            self.record_finalized_doc_loader(lid.as_deref());
            self.telemetry
                .on_request_failed(&req_id, Some("document navigated".to_string()));
        }
    }
}

pub struct CdpSession {
    conn: Arc<CdpConnection>,
    info: SessionInfo,
    state: Arc<CdpSessionState>,
}

struct InitialSessionOptions {
    env_profile: Option<EnvProfile>,
    suppress_webrtc_host_without_profile: bool,
    permissions: Vec<PermissionGrant>,
    block_images: bool,
    block_media: bool,
    block_fonts: bool,
    block_trackers: bool,
    instant_animations: bool,
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
            default_contexts: Mutex::new(HashMap::new()),
            default_context_sessions: Mutex::new(HashMap::new()),
            iframe_sessions: Mutex::new(HashMap::new()),
            iframe_session_targets: Mutex::new(HashMap::new()),
            iframe_parent_sessions: Mutex::new(HashMap::new()),
            popup_sessions: Mutex::new(HashMap::new()),
            popup_child_sessions: Mutex::new(HashMap::new()),
            popup_default_contexts: Mutex::new(HashMap::new()),
            worker_sessions: Mutex::new(HashSet::new()),
            pending_cdp_requests: Mutex::new(HashMap::new()),
            finalized_doc_loaders: Mutex::new(VecDeque::new()),
            telemetry: Arc::new(TelemetryStore::new(telem_cfg)),
            interceptor: Mutex::new(NetworkInterceptor::default()),
            fetch_enabled: AtomicBool::new(false),
            instant_animations: AtomicBool::new(false),
            anim_script_id: AsyncMutex::new(None),
            child_anim_script_ids: Mutex::new(HashMap::new()),
            env_script_id: AsyncMutex::new(None),
            env_isolated_script_id: AsyncMutex::new(None),
            env_window_script: Mutex::new(None),
            worker_init_script: Mutex::new(None),
            worker_ua_params: Mutex::new(None),
            env_timezone: Mutex::new(None),
            env_locale: Mutex::new(None),
            env_geolocation: Mutex::new(None),
            env_hardware_concurrency: Mutex::new(None),
            env_device_metrics: Mutex::new(None),
            env_extra_headers: Mutex::new(None),
            env_profile: Mutex::new(None),
            pending_storage_scripts: Mutex::new(Vec::new()),
            screencast: Mutex::new(None),
            _proxy_relay: Mutex::new(proxy_relay),
            load_notify: Notify::new(),
            disposed: AtomicBool::new(false),
        });

        let cdp_sid = info.session_id.clone();
        conn.sessions
            .write()
            .insert(cdp_sid.clone(), Arc::clone(&state));

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

        if let Err(err) = page_res.and(runtime_res).and(dom_res).and(net_res) {
            session.state.disposed.store(true, Ordering::SeqCst);
            session.conn.sessions.write().remove(&cdp_sid);
            return Err(err);
        }

        if let Ok(tree) = frame_tree_res
            && let Some(fid) = tree["frameTree"]["frame"]["id"].as_str()
        {
            *session.state.main_frame_id.lock() = Some(fid.to_string());
        }

        Ok(session)
    }

    async fn install_window_preload_script(&self, win_script: &str) -> Result<()> {
        *self.state.env_window_script.lock() = Some(win_script.to_string());
        {
            let mut prev = self.state.env_script_id.lock().await;
            if let Some(old_id) = prev.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            let new_id = self.add_init_script(win_script).await?;
            *prev = Some(new_id);
        }
        {
            let mut prev_iso = self.state.env_isolated_script_id.lock().await;
            if let Some(old_id) = prev_iso.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            let iso_res = self
                .call(
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({
                        "source": win_script,
                        "worldName": "mimikyu_isolated",
                    }),
                )
                .await?;
            if let Some(iso_id) = iso_res["identifier"].as_str() {
                *prev_iso = Some(iso_id.to_string());
            }
        }
        self.state.isolated_worlds.lock().clear();
        let _ = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": win_script,
                    "returnByValue": true,
                    "silent": true,
                }),
            )
            .await;
        Ok(())
    }

    async fn apply_initial_options(&self, init: InitialSessionOptions) -> Result<()> {
        if let Some(profile) = init.env_profile {
            self.set_env_overrides(profile).await?;
        } else if init.suppress_webrtc_host_without_profile {
            let rtc_script = build_webrtc_no_host_init_script(EngineKind::Chromium);
            self.install_window_preload_script(&rtc_script).await?;
        }
        if !init.permissions.is_empty() {
            self.set_permissions(init.permissions).await?;
        }

        if init.block_images || init.block_media || init.block_fonts || init.block_trackers {
            {
                let mut guard = self.state.interceptor.lock();
                guard.set_categories(init.block_images, init.block_media, init.block_fonts);
                guard.set_block_trackers(init.block_trackers);
            }
            self.sync_cdp_network().await?;
        }
        if init.instant_animations {
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

        let existing_targets: Vec<String> = {
            let mut sids: Vec<String> = self
                .state
                .iframe_sessions
                .lock()
                .values()
                .cloned()
                .collect();
            sids.extend(self.state.popup_sessions.lock().values().cloned());
            sids.extend(self.state.popup_child_sessions.lock().keys().cloned());
            sids.extend(self.state.worker_sessions.lock().iter().cloned());
            sids.sort();
            sids.dedup();
            sids
        };

        for child_sid in &existing_targets {
            let _ = self
                .conn
                .send_raw(
                    Some(child_sid),
                    "Network.setBlockedURLs",
                    json!({ "urls": globs }),
                )
                .await;
        }

        if let Some(patterns) = fetch_patterns {
            self.call("Fetch.enable", json!({ "patterns": patterns }))
                .await?;
            for child_sid in &existing_targets {
                let _ = self
                    .conn
                    .send_raw(
                        Some(child_sid),
                        "Fetch.enable",
                        json!({ "patterns": patterns }),
                    )
                    .await;
            }
            self.state.fetch_enabled.store(true, Ordering::SeqCst);
        } else if self.state.fetch_enabled.swap(false, Ordering::SeqCst) {
            let _ = self.call("Fetch.disable", json!({})).await;
            for child_sid in &existing_targets {
                let _ = self
                    .conn
                    .send_raw(Some(child_sid), "Fetch.disable", json!({}))
                    .await;
            }
        }
        self.conn.sync_browser_fetch().await;
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

    async fn ensure_compositor_foreground(&self) -> Result<()> {
        self.ensure_alive()?;
        if !self.conn.is_foreground_target(&self.info.target_id) {
            let _ = self
                .conn
                .send_browser(
                    "Target.activateTarget",
                    json!({ "targetId": self.info.target_id }),
                )
                .await;
            let _ = self.call("Page.bringToFront", json!({})).await;
            self.conn.mark_foreground_target(&self.info.target_id);
        }
        Ok(())
    }

    async fn dispatch_cdp_mouse_event(&self, params: Value) -> Result<()> {
        self.ensure_compositor_foreground().await?;
        let mouse_fut = self.call("Input.dispatchMouseEvent", params);
        tokio::pin!(mouse_fut);
        match tokio::time::timeout(Duration::from_millis(250), &mut mouse_fut).await {
            Ok(res) => res.map(|_| ()),
            Err(_) => {
                let _ = self
                    .conn
                    .send_browser(
                        "Target.activateTarget",
                        json!({ "targetId": self.info.target_id }),
                    )
                    .await;
                let _ = self.call("Page.bringToFront", json!({})).await;
                self.conn.mark_foreground_target(&self.info.target_id);
                mouse_fut.await.map(|_| ())
            }
        }
    }

    fn invalidate_frame_contexts(
        &self,
        frame_id: Option<&str>,
        resolved_fid: Option<&str>,
        stale_isolated_ctx: Option<i64>,
        stale_default_ctx: Option<i64>,
    ) {
        self.state.invalidate_frame_contexts(
            frame_id,
            resolved_fid,
            stale_isolated_ctx,
            stale_default_ctx,
        );
    }

    async fn call_for_frame(
        &self,
        frame_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.ensure_alive()?;
        let child_sid =
            frame_id.and_then(|fid| self.state.iframe_sessions.lock().get(fid).cloned());
        let sid = child_sid.as_deref().unwrap_or(&self.info.session_id);
        self.conn.send_raw(Some(sid), method, params).await
    }

    async fn release_cdp_eval_objects(&self, target_frame_id: Option<&str>, res: &Value) {
        let exc_oid = res["exceptionDetails"]["exception"]["objectId"].as_str();
        let res_oid = res["result"]["objectId"].as_str();
        if let Some(oid) = exc_oid {
            let _ = self
                .call_for_frame(
                    target_frame_id,
                    "Runtime.releaseObject",
                    json!({ "objectId": oid }),
                )
                .await;
        }
        if let Some(oid) = res_oid
            && Some(oid) != exc_oid
        {
            let _ = self
                .call_for_frame(
                    target_frame_id,
                    "Runtime.releaseObject",
                    json!({ "objectId": oid }),
                )
                .await;
        }
    }

    async fn resolve_cdp_wrapped_eval_result(
        &self,
        target_frame_id: Option<&str>,
        res: Value,
    ) -> Result<Value> {
        self.release_cdp_eval_objects(target_frame_id, &res).await;
        parse_eval_result(res)
    }

    async fn resolve_cdp_eval_result(
        &self,
        target_frame_id: Option<&str>,
        res: Value,
    ) -> Result<Value> {
        if let Some(exc) = res.get("exceptionDetails") {
            self.release_cdp_eval_objects(target_frame_id, &res).await;
            let text = exc["exception"]["description"]
                .as_str()
                .or_else(|| exc["text"].as_str())
                .unwrap_or("JavaScript exception")
                .to_string();
            return Err(MimikyuError::EvalFailed(text));
        }
        let result = &res["result"];
        match classify_cdp_eval_remote_object(result) {
            CdpEvalAction::Resolved(outcome) => {
                if let Some(oid) = result.get("objectId").and_then(|v| v.as_str()) {
                    let _ = self
                        .call_for_frame(
                            target_frame_id,
                            "Runtime.releaseObject",
                            json!({ "objectId": oid }),
                        )
                        .await;
                }
                outcome
            }
            CdpEvalAction::ValidateObject { object_id } => {
                let fn_res = self
                    .call_for_frame(
                        target_frame_id,
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": CDP_EVAL_JSON_VALIDATOR,
                            "returnByValue": true,
                        }),
                    )
                    .await;
                let _ = self
                    .call_for_frame(
                        target_frame_id,
                        "Runtime.releaseObject",
                        json!({ "objectId": object_id }),
                    )
                    .await;
                let val = fn_res?;
                self.release_cdp_eval_objects(target_frame_id, &val).await;
                parse_eval_result(val)
            }
        }
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

    async fn get_or_create_isolated_context_for(
        &self,
        frame_id: Option<&str>,
    ) -> Result<(String, i64)> {
        let fid = match frame_id {
            Some(f) => f.to_string(),
            None => self.main_frame_id().await?,
        };
        if let Some(ctx_id) = self.state.isolated_worlds.lock().get(&fid).copied() {
            return Ok((fid, ctx_id));
        }
        let res = self
            .call_for_frame(
                Some(&fid),
                "Page.createIsolatedWorld",
                json!({
                    "frameId": fid,
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
            .insert(fid.clone(), ctx_id);
        let env_js = self.state.env_window_script.lock().clone();
        if let Some(js) = env_js {
            let _ = self
                .call_for_frame(
                    Some(&fid),
                    "Runtime.evaluate",
                    json!({
                        "expression": js,
                        "contextId": ctx_id,
                        "returnByValue": true,
                        "silent": true,
                    }),
                )
                .await;
        }
        Ok((fid, ctx_id))
    }

    async fn extract_array_object_ids(
        &self,
        fid: &str,
        array_object_id: &str,
    ) -> Result<Vec<String>> {
        let props = self
            .call_for_frame(
                Some(fid),
                "Runtime.getProperties",
                json!({
                    "objectId": array_object_id,
                    "ownProperties": true,
                }),
            )
            .await?;
        let mut indexed: Vec<(usize, String)> = Vec::new();
        if let Some(arr) = props["result"].as_array() {
            for prop in arr {
                if let Some(name) = prop["name"].as_str()
                    && let Ok(idx) = name.parse::<usize>()
                    && let Some(oid) = prop["value"]["objectId"].as_str()
                {
                    indexed.push((idx, oid.to_string()));
                }
            }
        }
        indexed.sort_by_key(|(i, _)| *i);
        Ok(indexed.into_iter().map(|(_, o)| o).collect())
    }

    async fn query_shadow_inner(
        &self,
        fid: &str,
        ctx_id: i64,
        group: &str,
        selector_chain: &[String],
    ) -> Result<Vec<ShadowMatch>> {
        let sel0_json = serde_json::to_string(&selector_chain[0])?;
        let eval_res = self
            .call_for_frame(
                Some(fid),
                "Runtime.evaluate",
                json!({
                    "expression": format!("Array.from(document.querySelectorAll({sel0_json}))"),
                    "contextId": ctx_id,
                    "objectGroup": group,
                    "returnByValue": false,
                }),
            )
            .await?;
        if let Some(exc) = eval_res.get("exceptionDetails") {
            let msg = exc["text"].as_str().unwrap_or("querySelectorAll failed");
            return Err(MimikyuError::EvalFailed(msg.to_string()));
        }
        let Some(arr_oid) = eval_res["result"]["objectId"].as_str() else {
            return Ok(Vec::new());
        };
        let mut current_oids = self.extract_array_object_ids(fid, arr_oid).await?;

        for sel in &selector_chain[1..] {
            let mut next_oids = Vec::new();
            for oid in current_oids {
                let desc_val = match self
                    .call_for_frame(
                        Some(fid),
                        "DOM.describeNode",
                        json!({ "objectId": oid, "depth": 1, "pierce": true }),
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(err) if is_transient_context_error(&err) => return Err(err),
                    Err(_) => continue,
                };
                let Some(srs) = desc_val["node"]["shadowRoots"].as_array() else {
                    continue;
                };
                for sr in srs {
                    let Some(sr_bid) = sr["backendNodeId"].as_i64() else {
                        continue;
                    };
                    let sr_resolved = match self
                        .call_for_frame(
                            Some(fid),
                            "DOM.resolveNode",
                            json!({
                                "backendNodeId": sr_bid,
                                "executionContextId": ctx_id,
                                "objectGroup": group,
                            }),
                        )
                        .await
                    {
                        Ok(v) => v,
                        Err(err) if is_transient_context_error(&err) => return Err(err),
                        Err(_) => continue,
                    };
                    let Some(sr_oid) = sr_resolved["object"]["objectId"].as_str() else {
                        continue;
                    };
                    let call_res = match self
                        .call_for_frame(
                            Some(fid),
                            "Runtime.callFunctionOn",
                            json!({
                                "objectId": sr_oid,
                                "functionDeclaration": "function(s) { return Array.from(this.querySelectorAll(s)); }",
                                "arguments": [{ "value": sel }],
                                "objectGroup": group,
                                "returnByValue": false,
                            }),
                        )
                        .await
                    {
                        Ok(v) => v,
                        Err(err) if is_transient_context_error(&err) => return Err(err),
                        Err(_) => continue,
                    };
                    if let Some(sub_arr_oid) = call_res["result"]["objectId"].as_str() {
                        match self.extract_array_object_ids(fid, sub_arr_oid).await {
                            Ok(found) => next_oids.extend(found),
                            Err(err) if is_transient_context_error(&err) => return Err(err),
                            Err(_) => {}
                        }
                    }
                }
            }
            current_oids = next_oids;
            if current_oids.is_empty() {
                break;
            }
        }

        let mut matches = Vec::with_capacity(current_oids.len());
        for oid in current_oids {
            let bid = match self
                .call_for_frame(Some(fid), "DOM.describeNode", json!({ "objectId": oid }))
                .await
            {
                Ok(d) => d["node"]["backendNodeId"].as_i64(),
                Err(err) if is_transient_context_error(&err) => return Err(err),
                Err(_) => None,
            };
            let info_res = self
                .call_for_frame(
                    Some(fid),
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": oid,
                        "functionDeclaration": r#"function() {
                            const attrs = {};
                            if (this.attributes) {
                                for (const a of this.attributes) attrs[a.name] = a.value;
                            }
                            const r = this.getBoundingClientRect ? this.getBoundingClientRect() : null;
                            return {
                                tagName: (this.tagName || '').toLowerCase(),
                                text: (this.innerText || this.textContent || '').trim(),
                                html: this.outerHTML || null,
                                attributes: attrs,
                                bounds: r ? { x: r.x, y: r.y, width: r.width, height: r.height } : null
                            };
                        }"#,
                        "returnByValue": true,
                    }),
                )
                .await?;
            let val = parse_eval_result(info_res)?;
            let attributes: BTreeMap<String, String> =
                serde_json::from_value(val["attributes"].clone()).unwrap_or_default();
            let bounds: Option<BoundingBox> = serde_json::from_value(val["bounds"].clone()).ok();
            matches.push(ShadowMatch {
                backend_node_id: bid,
                frame_id: Some(fid.to_string()),
                tag_name: val["tagName"].as_str().unwrap_or_default().to_string(),
                text: val["text"].as_str().unwrap_or_default().to_string(),
                html: val["html"].as_str().map(ToString::to_string),
                attributes,
                bounds,
            });
        }
        Ok(matches)
    }

    async fn resolve_frame_transform(
        &self,
        frame_id: &str,
        main_fid: Option<&str>,
        parent_map: &HashMap<String, String>,
        cache: &mut HashMap<String, FrameTransform>,
    ) -> FrameTransform {
        if Some(frame_id) == main_fid {
            return FrameTransform::identity();
        }
        if let Some(&cached) = cache.get(frame_id) {
            return cached;
        }
        let mut chain = Vec::new();
        let mut cur = frame_id.to_string();
        for _ in 0..32 {
            if Some(cur.as_str()) == main_fid {
                break;
            }
            if let Some(parent) = parent_map.get(&cur) {
                chain.push((cur.clone(), parent.clone()));
                cur = parent.clone();
            } else {
                break;
            }
        }
        let mut total = FrameTransform::identity();
        for (child_fid, parent_fid) in chain.into_iter().rev() {
            if let Some(&cached) = cache.get(&child_fid) {
                total = cached;
                continue;
            }
            if let Ok(owner) = self
                .call_for_frame(
                    Some(&parent_fid),
                    "DOM.getFrameOwner",
                    json!({ "frameId": child_fid }),
                )
                .await
                && let Some(obid) = owner["backendNodeId"].as_i64()
                && let Ok(val) = self
                    .call_function_on_node(
                        obid,
                        Some(&parent_fid),
                        r#"function() {
                            const r = this.getBoundingClientRect();
                            const cs = getComputedStyle(this);
                            const iw = this.offsetWidth;
                            const ih = this.offsetHeight;
                            const sx = iw > 0 && r.width > 0 ? r.width / iw : 1;
                            const sy = ih > 0 && r.height > 0 ? r.height / ih : 1;
                            return {
                                ox: r.x + ((parseFloat(cs.borderLeftWidth) || 0) + (parseFloat(cs.paddingLeft) || 0)) * sx,
                                oy: r.y + ((parseFloat(cs.borderTopWidth) || 0) + (parseFloat(cs.paddingTop) || 0)) * sy,
                                sx,
                                sy
                            };
                        }"#,
                        vec![],
                    )
                    .await
                && let (Some(ox), Some(oy)) = (
                    val["ox"].as_f64().filter(|v| v.is_finite()),
                    val["oy"].as_f64().filter(|v| v.is_finite()),
                )
            {
                let sx = val["sx"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(1.0);
                let sy = val["sy"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(1.0);
                total = total.compose_inner(FrameTransform { ox, oy, sx, sy });
            }
            cache.insert(child_fid, total);
        }
        total
    }

    async fn capture_dom_snapshot_bounds(
        &self,
        frames: &[FrameInfo],
    ) -> (
        HashMap<(Option<String>, i64), BoundingBox>,
        HashSet<(Option<String>, i64)>,
    ) {
        let mut map = HashMap::new();
        let mut shell_bids = HashSet::new();
        let main_fid = self.main_frame_id().await.ok();
        let parent_map: HashMap<String, String> = frames
            .iter()
            .filter_map(|f| {
                f.parent_frame_id
                    .as_ref()
                    .map(|p| (f.frame_id.clone(), p.clone()))
            })
            .collect();
        let mut frame_transforms: HashMap<String, FrameTransform> = HashMap::new();
        if let Some(ref mfid) = main_fid {
            frame_transforms.insert(mfid.clone(), FrameTransform::identity());
        }

        let mut snapshots_to_collect: Vec<(Option<String>, Option<String>)> =
            vec![(None, main_fid.clone())];
        {
            let targets = self.state.iframe_session_targets.lock();
            for (sid, tid) in targets.iter() {
                snapshots_to_collect.push((Some(sid.clone()), Some(tid.clone())));
            }
        }

        for (sid_opt, fallback_fid) in snapshots_to_collect {
            let snap_res = match sid_opt.as_deref() {
                Some(sid) => {
                    self.conn
                        .send_raw(
                            Some(sid),
                            "DOMSnapshot.captureSnapshot",
                            json!({
                                "computedStyles": ["display", "visibility"],
                                "includeDOMRects": true,
                            }),
                        )
                        .await
                }
                None => {
                    self.call(
                        "DOMSnapshot.captureSnapshot",
                        json!({
                            "computedStyles": ["display", "visibility"],
                            "includeDOMRects": true,
                        }),
                    )
                    .await
                }
            };
            let Ok(snap) = snap_res else {
                continue;
            };
            let strings = snap["strings"].as_array();
            let str_at = |v: &Value| -> Option<&str> {
                let idx = v.as_u64()? as usize;
                strings?.get(idx)?.as_str()
            };
            let Some(docs) = snap["documents"].as_array() else {
                continue;
            };

            for doc in docs {
                let doc_fid = doc
                    .get("frameId")
                    .and_then(&str_at)
                    .map(ToString::to_string)
                    .or_else(|| fallback_fid.clone());
                let tf = if let Some(ref fid) = doc_fid {
                    self.resolve_frame_transform(
                        fid,
                        main_fid.as_deref(),
                        &parent_map,
                        &mut frame_transforms,
                    )
                    .await
                } else {
                    FrameTransform::identity()
                };

                let Some(backend_ids) = doc["nodes"]["backendNodeId"].as_array() else {
                    continue;
                };
                let node_names = doc["nodes"]["nodeName"].as_array();
                if let Some(names) = node_names {
                    for (ni, bid_val) in backend_ids.iter().enumerate() {
                        if let Some(bid) = bid_val.as_i64()
                            && let Some(tag) = names.get(ni).and_then(&str_at)
                            && (tag.eq_ignore_ascii_case("HTML")
                                || tag.eq_ignore_ascii_case("BODY"))
                        {
                            shell_bids.insert((doc_fid.clone(), bid));
                        }
                    }
                }
                let node_attrs = doc["nodes"]["attributes"].as_array();
                let Some(node_indices) = doc["layout"]["nodeIndex"].as_array() else {
                    continue;
                };
                let Some(bounds_arr) = doc["layout"]["bounds"].as_array() else {
                    continue;
                };
                let styles_arr = doc["layout"]["styles"].as_array();
                let scroll_x = doc["scrollOffsetX"].as_f64().unwrap_or(0.0);
                let scroll_y = doc["scrollOffsetY"].as_f64().unwrap_or(0.0);
                for (layout_idx, (ni_val, rect_val)) in
                    node_indices.iter().zip(bounds_arr.iter()).enumerate()
                {
                    let Some(ni) = ni_val.as_u64().map(|u| u as usize) else {
                        continue;
                    };
                    let Some(bid) = backend_ids.get(ni).and_then(|v| v.as_i64()) else {
                        continue;
                    };
                    let Some(rect) = rect_val.as_array() else {
                        continue;
                    };
                    if rect.len() < 4 {
                        continue;
                    }
                    let (Some(x), Some(y), Some(mut w), Some(mut h)) = (
                        rect[0].as_f64(),
                        rect[1].as_f64(),
                        rect[2].as_f64(),
                        rect[3].as_f64(),
                    ) else {
                        continue;
                    };

                    let is_file_input = node_names
                        .and_then(|arr| arr.get(ni))
                        .and_then(&str_at)
                        .is_some_and(|name| name.eq_ignore_ascii_case("INPUT"))
                        && node_attrs
                            .and_then(|arr| arr.get(ni))
                            .and_then(|v| v.as_array())
                            .is_some_and(|pairs| {
                                pairs.as_chunks::<2>().0.iter().any(|[k, v]| {
                                    str_at(k).is_some_and(|key| key.eq_ignore_ascii_case("type"))
                                        && str_at(v)
                                            .is_some_and(|val| val.eq_ignore_ascii_case("file"))
                                })
                            });
                    if is_file_input {
                        let is_css_hidden = styles_arr
                            .and_then(|arr| arr.get(layout_idx))
                            .and_then(|v| v.as_array())
                            .is_some_and(|st| {
                                let disp = st.first().and_then(&str_at).unwrap_or("");
                                let vis = st.get(1).and_then(&str_at).unwrap_or("");
                                disp.eq_ignore_ascii_case("none")
                                    || vis.eq_ignore_ascii_case("hidden")
                                    || vis.eq_ignore_ascii_case("collapse")
                            });
                        if !is_css_hidden {
                            w = w.max(1.0);
                            h = h.max(1.0);
                        }
                    }

                    let mut bbox = BoundingBox {
                        x: x - scroll_x,
                        y: y - scroll_y,
                        width: w,
                        height: h,
                    };
                    tf.apply(&mut bbox);
                    map.entry((doc_fid.clone(), bid)).or_insert(bbox);
                }
            }
        }
        (map, shell_bids)
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
        self.query_backend_node_id_in_frame(selector, None).await
    }

    async fn query_backend_node_id_in_frame(
        &self,
        selector: &str,
        frame_id: Option<&str>,
    ) -> Result<i64> {
        self.ensure_alive()?;
        if let Some(fid_in) = frame_id {
            let sel_json = serde_json::to_string(selector)?;
            let expr = format!("document.querySelector({sel_json})");
            let mut last_transient_err: Option<MimikyuError> = None;
            for attempt in 0..CONTEXT_MAX_ATTEMPTS {
                self.ensure_alive()?;
                let (fid, ctx_id) =
                    match self.get_or_create_isolated_context_for(Some(fid_in)).await {
                        Ok(pair) => pair,
                        Err(err) if is_transient_context_error(&err) => {
                            self.invalidate_frame_contexts(Some(fid_in), None, None, None);
                            last_transient_err = Some(err);
                            if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                && !self.conn.closed.load(Ordering::SeqCst)
                            {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                            break;
                        }
                        Err(err) => return Err(err),
                    };
                let eval_val = match self
                    .call_for_frame(
                        Some(&fid),
                        "Runtime.evaluate",
                        json!({
                            "expression": expr,
                            "contextId": ctx_id,
                            "returnByValue": false,
                            "awaitPromise": false,
                        }),
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(err) if is_transient_context_error(&err) => {
                        self.invalidate_frame_contexts(
                            Some(fid_in),
                            Some(&fid),
                            Some(ctx_id),
                            None,
                        );
                        last_transient_err = Some(err);
                        if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                            && !self.conn.closed.load(Ordering::SeqCst)
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        break;
                    }
                    Err(err) => return Err(err),
                };
                if let Some(obj_id) = eval_val["result"]["objectId"].as_str() {
                    let desc = self
                        .call_for_frame(
                            Some(&fid),
                            "DOM.describeNode",
                            json!({ "objectId": obj_id }),
                        )
                        .await;
                    let _ = self
                        .call_for_frame(
                            Some(&fid),
                            "Runtime.releaseObject",
                            json!({ "objectId": obj_id }),
                        )
                        .await;
                    let desc = match desc {
                        Ok(d) => d,
                        Err(err)
                            if is_transient_context_error(&err)
                                || is_stale_dom_node_error(&err) =>
                        {
                            self.invalidate_frame_contexts(
                                Some(fid_in),
                                Some(&fid),
                                Some(ctx_id),
                                None,
                            );
                            last_transient_err = Some(err);
                            if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                && !self.conn.closed.load(Ordering::SeqCst)
                            {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                            break;
                        }
                        Err(err) => return Err(err),
                    };
                    return desc["node"]["backendNodeId"]
                        .as_i64()
                        .ok_or_else(|| MimikyuError::ElementNotFound(selector.to_string()));
                }
                return Err(MimikyuError::ElementNotFound(format!(
                    "selector '{selector}' matched no element"
                )));
            }
            return Err(format_exhausted_context_error(
                "query_backend_node_id_in_frame",
                CONTEXT_MAX_ATTEMPTS,
                last_transient_err.unwrap_or_else(|| MimikyuError::Protocol {
                    code: -32000,
                    message: "Execution context was destroyed".to_string(),
                }),
            ));
        }

        let mut last_transient_err: Option<MimikyuError> = None;
        for attempt in 0..CONTEXT_MAX_ATTEMPTS {
            self.ensure_alive()?;
            let doc = match self
                .call_for_frame(None, "DOM.getDocument", json!({ "depth": 0 }))
                .await
            {
                Ok(v) => v,
                Err(err) if is_transient_context_error(&err) => {
                    self.invalidate_frame_contexts(None, None, None, None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            };
            let root_id = doc["root"]["nodeId"]
                .as_i64()
                .ok_or_else(|| MimikyuError::ElementNotFound(selector.to_string()))?;
            let q = match self
                .call_for_frame(
                    None,
                    "DOM.querySelector",
                    json!({ "nodeId": root_id, "selector": selector }),
                )
                .await
            {
                Ok(v) => v,
                Err(err) if is_transient_context_error(&err) || is_stale_dom_node_error(&err) => {
                    self.invalidate_frame_contexts(None, None, None, None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            };
            let node_id = q["nodeId"].as_i64().unwrap_or(0);
            if node_id == 0 {
                return Err(MimikyuError::ElementNotFound(format!(
                    "selector '{selector}' matched no element"
                )));
            }
            let desc = match self
                .call_for_frame(None, "DOM.describeNode", json!({ "nodeId": node_id }))
                .await
            {
                Ok(v) => v,
                Err(err) if is_transient_context_error(&err) || is_stale_dom_node_error(&err) => {
                    self.invalidate_frame_contexts(None, None, None, None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            };
            return desc["node"]["backendNodeId"]
                .as_i64()
                .ok_or_else(|| MimikyuError::ElementNotFound(selector.to_string()));
        }

        Err(format_exhausted_context_error(
            "query_backend_node_id",
            CONTEXT_MAX_ATTEMPTS,
            last_transient_err.unwrap_or_else(|| MimikyuError::Protocol {
                code: -32000,
                message: "Execution context was destroyed".to_string(),
            }),
        ))
    }

    async fn eval_isolated_readonly(&self, expr: &str, frame_id: Option<&str>) -> Result<Value> {
        self.eval_isolated_in_frame_with_policy(expr, frame_id, true)
            .await
    }

    async fn eval_isolated_in_frame_with_policy(
        &self,
        expr: &str,
        frame_id: Option<&str>,
        allow_mid_execution_retry: bool,
    ) -> Result<Value> {
        self.ensure_alive()?;
        let wrapped_expr = can_wrap_cdp_eval_expr(expr).then(|| build_wrapped_cdp_eval_expr(expr));
        let mut use_wrapper = wrapped_expr.is_some();
        let mut last_transient_err: Option<MimikyuError> = None;

        for attempt in 0..CONTEXT_MAX_ATTEMPTS {
            self.ensure_alive()?;
            let (fid, ctx_id) = match self.get_or_create_isolated_context_for(frame_id).await {
                Ok(pair) => pair,
                Err(err) if is_transient_context_error(&err) => {
                    // Context acquisition happens strictly before `expr` runs.
                    self.invalidate_frame_contexts(frame_id, None, None, None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            };

            if use_wrapper && let Some(ref wrapped) = wrapped_expr {
                let eval_res = self
                    .call_for_frame(
                        Some(&fid),
                        "Runtime.evaluate",
                        json!({
                            "expression": wrapped,
                            "contextId": ctx_id,
                            "returnByValue": true,
                            "awaitPromise": true,
                        }),
                    )
                    .await;

                match eval_res {
                    Ok(v) if is_compile_time_syntax_error(&v) => {
                        // V8 failed to parse the wrapped expression (0 bytes executed);
                        // release exception handles and fall back to bare 2-RPC evaluation.
                        self.release_cdp_eval_objects(Some(&fid), &v).await;
                        use_wrapper = false;
                    }
                    Ok(v) => return self.resolve_cdp_wrapped_eval_result(Some(&fid), v).await,
                    Err(err) => match classify_transient_context_error(&err) {
                        Some(TransientContextErrorKind::PreExecution) => {
                            self.invalidate_frame_contexts(
                                frame_id,
                                Some(&fid),
                                Some(ctx_id),
                                None,
                            );
                            last_transient_err = Some(err);
                            if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                && !self.conn.closed.load(Ordering::SeqCst)
                            {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                            break;
                        }
                        Some(TransientContextErrorKind::MidOrPostExecution) => {
                            self.invalidate_frame_contexts(
                                frame_id,
                                Some(&fid),
                                Some(ctx_id),
                                None,
                            );
                            if allow_mid_execution_retry {
                                last_transient_err = Some(err);
                                if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                    && !self.conn.closed.load(Ordering::SeqCst)
                                {
                                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                    continue;
                                }
                                break;
                            }
                            return Err(format_mid_eval_context_destroyed_error(
                                "isolated-world evaluation",
                                err,
                            ));
                        }
                        None => return Err(err),
                    },
                }
            }

            let eval_res = self
                .call_for_frame(
                    Some(&fid),
                    "Runtime.evaluate",
                    json!({
                        "expression": expr,
                        "contextId": ctx_id,
                        "returnByValue": false,
                        "awaitPromise": true,
                    }),
                )
                .await;

            let raw_val = match eval_res {
                Ok(v) => v,
                Err(err) => match classify_transient_context_error(&err) {
                    Some(TransientContextErrorKind::PreExecution) => {
                        self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                        last_transient_err = Some(err);
                        if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                            && !self.conn.closed.load(Ordering::SeqCst)
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        break;
                    }
                    Some(TransientContextErrorKind::MidOrPostExecution) => {
                        self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                        if allow_mid_execution_retry {
                            last_transient_err = Some(err);
                            if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                && !self.conn.closed.load(Ordering::SeqCst)
                            {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                            break;
                        }
                        return Err(format_mid_eval_context_destroyed_error(
                            "isolated-world evaluation",
                            err,
                        ));
                    }
                    None => return Err(err),
                },
            };

            match self.resolve_cdp_eval_result(Some(&fid), raw_val).await {
                Ok(val) => return Ok(val),
                Err(err) if is_transient_context_error(&err) => {
                    // Fallback Runtime.callFunctionOn failed after `expr` already ran.
                    self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                    if allow_mid_execution_retry {
                        last_transient_err = Some(err);
                        if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                            && !self.conn.closed.load(Ordering::SeqCst)
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        break;
                    }
                    return Err(format_mid_eval_context_destroyed_error(
                        "isolated-world result serialization",
                        err,
                    ));
                }
                Err(err) => return Err(err),
            }
        }

        Err(format_exhausted_context_error(
            "isolated-world evaluation",
            CONTEXT_MAX_ATTEMPTS,
            last_transient_err.unwrap_or_else(|| MimikyuError::Protocol {
                code: -32000,
                message: "Execution context was destroyed".to_string(),
            }),
        ))
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
            params["proxyServer"] = json!(crate::process::normalize_chromium_proxy_server(
                &effective.server
            ));
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

        let cleanup_context = |conn: Arc<CdpConnection>, ctx_id: String| async move {
            conn.isolated_contexts.write().remove(&ctx_id);
            conn.active_targets
                .write()
                .remove(&context_scope_key(Some(&ctx_id)));
            let _ = conn
                .send_browser(
                    "Target.disposeBrowserContext",
                    json!({ "browserContextId": ctx_id }),
                )
                .await;
        };

        let tgt_res = {
            let _creating = CreatingContextTargetGuard::enter(&self.conn, Some(ctx_id.as_str()));
            self.conn
                .send_browser(
                    "Target.createTarget",
                    json!({ "url": "about:blank", "browserContextId": ctx_id }),
                )
                .await
        };
        let tgt_res = match tgt_res {
            Ok(r) => r,
            Err(e) => {
                cleanup_context(Arc::clone(&self.conn), ctx_id).await;
                return Err(e);
            }
        };
        let Some(target_id) = tgt_res["targetId"].as_str().map(ToString::to_string) else {
            cleanup_context(Arc::clone(&self.conn), ctx_id).await;
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "Target.createTarget missing targetId".to_string(),
            });
        };
        self.conn.set_active_target(Some(&ctx_id), &target_id);

        let attach_res = match self
            .conn
            .send_browser(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self
                    .conn
                    .send_browser("Target.closeTarget", json!({ "targetId": target_id }))
                    .await;
                cleanup_context(Arc::clone(&self.conn), ctx_id).await;
                return Err(e);
            }
        };
        let Some(cdp_session_id) = attach_res["sessionId"].as_str().map(ToString::to_string) else {
            let _ = self
                .conn
                .send_browser("Target.closeTarget", json!({ "targetId": target_id }))
                .await;
            cleanup_context(Arc::clone(&self.conn), ctx_id).await;
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "Target.attachToTarget missing sessionId".to_string(),
            });
        };

        let fallback_profile_dir = self
            .info
            .profile_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let dl_dir = opts
            .download_dir
            .clone()
            .unwrap_or_else(|| fallback_profile_dir.join("downloads").join(&ctx_id));

        let mut info = self.info.clone();
        info.session_id = cdp_session_id;
        info.context_id = Some(ctx_id.clone());
        info.target_id = target_id.clone();

        let session = match CdpSession::create(
            Arc::clone(&self.conn),
            info,
            Some(ctx_id.clone()),
            true,
            false,
            opts.main_world_eval,
            opts.telemetry_capacity,
            relay_opt,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .conn
                    .send_browser("Target.closeTarget", json!({ "targetId": target_id }))
                    .await;
                cleanup_context(Arc::clone(&self.conn), ctx_id).await;
                return Err(e);
            }
        };

        if let Err(err) =
            configure_cdp_download_behavior(&session.conn, Some(ctx_id.as_str()), &dl_dir).await
        {
            let _ = session.dispose().await;
            return Err(err);
        }

        if let Err(err) = session
            .apply_initial_options(InitialSessionOptions {
                env_profile: opts.env_profile,
                suppress_webrtc_host_without_profile: true,
                permissions: opts.permissions,
                block_images: opts.block_images,
                block_media: opts.block_media,
                block_fonts: opts.block_fonts,
                block_trackers: opts.block_trackers || opts.privacy_mode,
                instant_animations: opts.instant_animations,
            })
            .await
        {
            let _ = session.dispose().await;
            return Err(err);
        }

        let _ = session.call("Page.bringToFront", json!({})).await;
        self.conn.mark_foreground_target(&target_id);

        Ok(session)
    }

    async fn new_tab(&self, url: Option<&str>) -> Result<Arc<dyn DriverSession>> {
        self.ensure_alive()?;
        let parent_env_profile = self.state.env_profile.lock().clone();
        let parent_win_script = self.state.env_window_script.lock().clone();
        let parent_interceptor = self.state.interceptor.lock().clone();
        let parent_instant_anim = self.state.instant_animations.load(Ordering::SeqCst);

        let initial_url = url.unwrap_or("about:blank");
        let mut create_params = json!({ "url": "about:blank" });
        if let Some(ref ctx_id) = self.state.context_id {
            create_params["browserContextId"] = json!(ctx_id);
        }
        let tgt_res = {
            let _creating =
                CreatingContextTargetGuard::enter(&self.conn, self.state.context_id.as_deref());
            self.conn
                .send_browser("Target.createTarget", create_params)
                .await?
        };
        let target_id = tgt_res["targetId"]
            .as_str()
            .ok_or_else(|| MimikyuError::Protocol {
                code: -1,
                message: "Target.createTarget missing targetId".to_string(),
            })?
            .to_string();

        let attach_res = match self
            .conn
            .send_browser(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self
                    .conn
                    .send_browser("Target.closeTarget", json!({ "targetId": target_id }))
                    .await;
                return Err(e);
            }
        };
        let Some(cdp_session_id) = attach_res["sessionId"].as_str().map(ToString::to_string) else {
            let _ = self
                .conn
                .send_browser("Target.closeTarget", json!({ "targetId": target_id }))
                .await;
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "Target.attachToTarget missing sessionId".to_string(),
            });
        };

        let mut info = self.info.clone();
        info.session_id = cdp_session_id;
        info.target_id = target_id.clone();

        let session = match CdpSession::create(
            Arc::clone(&self.conn),
            info,
            self.state.context_id.clone(),
            false,
            false,
            self.state.main_world_eval,
            self.state.telemetry_capacity,
            None,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .conn
                    .send_browser("Target.closeTarget", json!({ "targetId": target_id }))
                    .await;
                return Err(e);
            }
        };

        let network_sync_needed = {
            let mut guard = session.state.interceptor.lock();
            *guard = parent_interceptor;
            !guard.blocker().cdp_blocked_url_globs().is_empty()
                || guard.cdp_fetch_patterns().is_some()
        };

        if let Some(profile) = parent_env_profile {
            if let Err(err) = session.set_env_overrides(profile).await {
                let _ = session.dispose().await;
                return Err(err);
            }
        } else if let Some(ref win_script) = parent_win_script
            && let Err(err) = session.install_window_preload_script(win_script).await
        {
            let _ = session.dispose().await;
            return Err(err);
        }
        if network_sync_needed && let Err(err) = session.sync_cdp_network().await {
            let _ = session.dispose().await;
            return Err(err);
        }
        if parent_instant_anim && let Err(err) = session.set_instant_animations(true).await {
            let _ = session.dispose().await;
            return Err(err);
        }

        self.conn
            .set_active_target(self.state.context_id.as_deref(), &target_id);
        let _ = session.call("Page.bringToFront", json!({})).await;
        self.conn.mark_foreground_target(&target_id);

        if initial_url != "about:blank"
            && let Err(err) = session.navigate(initial_url).await
        {
            let _ = session.dispose().await;
            return Err(err);
        }
        Ok(session)
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        self.ensure_alive()?;
        let res = self
            .conn
            .send_browser("Target.getTargets", json!({}))
            .await?;
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
        if let Some(sid) = self.conn.session_id_for_target(target_id) {
            let _ = self
                .conn
                .send_raw(Some(&sid), "Page.bringToFront", json!({}))
                .await;
        }
        self.conn.mark_foreground_target(target_id);
        self.conn
            .set_active_target(self.state.context_id.as_deref(), target_id);
        Ok(())
    }

    async fn navigate(&self, url: &str) -> Result<NavInfo> {
        self.ensure_compositor_foreground().await?;
        let notified = self.state.load_notify.notified();
        let res = self.call("Page.navigate", json!({ "url": url })).await?;
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
        self.ensure_compositor_foreground().await?;
        let notified = self.state.load_notify.notified();
        self.call("Page.reload", json!({})).await?;
        self.state.isolated_worlds.lock().clear();
        let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
        self.wait_for_load_complete().await?;
        let fid = self.state.main_frame_id.lock().clone();
        self.collect_nav_info(fid).await
    }

    async fn go_back(&self) -> Result<NavInfo> {
        self.ensure_compositor_foreground().await?;
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
        let entry_id = entries[(idx - 1) as usize]["id"]
            .as_i64()
            .ok_or_else(|| MimikyuError::NavigationFailed("missing history entryId".to_string()))?;
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
        self.ensure_compositor_foreground().await?;
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
        let entry_id = entries[(idx + 1) as usize]["id"]
            .as_i64()
            .ok_or_else(|| MimikyuError::NavigationFailed("missing history entryId".to_string()))?;
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
        let val = self
            .eval_isolated_readonly("window.location.href", None)
            .await?;
        Ok(val.as_str().unwrap_or_default().to_string())
    }

    async fn title(&self) -> Result<String> {
        let val = self.eval_isolated_readonly("document.title", None).await?;
        Ok(val.as_str().unwrap_or_default().to_string())
    }

    async fn eval_isolated(&self, expr: &str) -> Result<Value> {
        self.eval_isolated_in_frame(expr, None).await
    }

    async fn eval_main_world(&self, expr: &str) -> Result<Value> {
        self.eval_main_world_in_frame(expr, None).await
    }

    async fn list_frames(&self) -> Result<Vec<FrameInfo>> {
        self.ensure_alive()?;
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let mut out = Vec::new();
        collect_cdp_frames(&tree["frameTree"], None, &mut out);

        let mut oopifs: Vec<(String, String)> = self
            .state
            .iframe_session_targets
            .lock()
            .iter()
            .map(|(sid, tid)| (tid.clone(), sid.clone()))
            .collect();
        oopifs.sort_by(|a, b| a.0.cmp(&b.0));

        if oopifs.is_empty() {
            return Ok(out);
        }

        let known_oopif_fids: HashSet<String> = {
            let guard = self.state.iframe_sessions.lock();
            guard.keys().cloned().collect()
        };
        let main_session_fids: Vec<String> = out
            .iter()
            .filter(|f| !known_oopif_fids.contains(&f.frame_id))
            .map(|f| f.frame_id.clone())
            .collect();

        struct OopifTree {
            tid: String,
            sid: String,
            frames: Vec<FrameInfo>,
            raw_parent_id: Option<String>,
        }

        let mut oopif_trees: Vec<OopifTree> = Vec::with_capacity(oopifs.len());
        for (tid, child_sid) in oopifs {
            let sub_tree = match self
                .conn
                .send_raw(Some(&child_sid), "Page.getFrameTree", json!({}))
                .await
            {
                Ok(t) => t,
                Err(MimikyuError::Protocol { ref message, .. })
                    if message.contains("Session with given id not found")
                        || message.contains("Target closed") =>
                {
                    self.state
                        .prune_detached_iframe_sessions(&self.conn, &child_sid);
                    continue;
                }
                Err(_) => continue,
            };
            let mut sub_frames = Vec::new();
            collect_cdp_frames(&sub_tree["frameTree"], None, &mut sub_frames);
            if sub_frames.is_empty() {
                continue;
            }
            for sf in &mut sub_frames {
                sf.is_main = false;
            }
            {
                let targets_guard = self.state.iframe_session_targets.lock();
                if !targets_guard.contains_key(&child_sid) {
                    continue;
                }
                let mut sessions_guard = self.state.iframe_sessions.lock();
                for sf in &sub_frames {
                    sessions_guard.insert(sf.frame_id.clone(), child_sid.clone());
                }
            }
            let raw_parent_id = sub_tree["frameTree"]["frame"]["parentId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            oopif_trees.push(OopifTree {
                tid,
                sid: child_sid,
                frames: sub_frames,
                raw_parent_id,
            });
        }

        let main_fid_fallback = out.first().map(|f| f.frame_id.clone());
        let oopif_fids_by_sid: HashMap<String, Vec<String>> = oopif_trees
            .iter()
            .map(|ot| {
                (
                    ot.sid.clone(),
                    ot.frames.iter().map(|f| f.frame_id.clone()).collect(),
                )
            })
            .collect();
        let candidate_sessions: Vec<(Option<String>, Vec<String>)> = {
            let mut v = Vec::with_capacity(1 + oopif_trees.len());
            for ot in &oopif_trees {
                let fids: Vec<String> = ot.frames.iter().map(|f| f.frame_id.clone()).collect();
                v.push((Some(ot.sid.clone()), fids));
            }
            v.push((None, main_session_fids));
            v
        };

        let parent_sess_map = self.state.iframe_parent_sessions.lock().clone();
        for ot in &mut oopif_trees {
            let root_fid = ot.frames[0].frame_id.clone();
            let attached_parent_sid = parent_sess_map.get(&ot.sid).cloned();
            let mut parent_fid = ot.raw_parent_id.clone();

            if parent_fid.is_none() {
                if let Some(ref psid) = attached_parent_sid {
                    if let Some(pfids) = oopif_fids_by_sid.get(psid) {
                        if pfids.len() == 1 {
                            parent_fid = Some(pfids[0].clone());
                        } else if pfids.len() > 1 {
                            if let Ok(owner) = self
                                .conn
                                .send_raw(
                                    Some(psid),
                                    "DOM.getFrameOwner",
                                    json!({ "frameId": root_fid }),
                                )
                                .await
                                && let Some(obid) = owner["backendNodeId"].as_i64()
                            {
                                for cfid in pfids {
                                    if let Ok(Value::Bool(true)) = self
                                        .call_function_on_node(
                                            obid,
                                            Some(cfid),
                                            "function() { return this.ownerDocument === document; }",
                                            vec![],
                                        )
                                        .await
                                    {
                                        parent_fid = Some(cfid.clone());
                                        break;
                                    }
                                }
                            }
                            if parent_fid.is_none() {
                                parent_fid = pfids.first().cloned();
                            }
                        }
                    }
                } else {
                    parent_fid = out
                        .iter()
                        .find(|f| f.frame_id == root_fid)
                        .and_then(|f| f.parent_frame_id.clone());
                }
            }

            if parent_fid.is_none() {
                for (cand_sid, cand_fids) in &candidate_sessions {
                    if cand_sid.as_deref() == Some(ot.sid.as_str()) {
                        continue;
                    }
                    if attached_parent_sid.is_some() && cand_sid.is_none() {
                        continue;
                    }
                    let owner_res = match cand_sid.as_deref() {
                        Some(sid) => {
                            self.conn
                                .send_raw(
                                    Some(sid),
                                    "DOM.getFrameOwner",
                                    json!({ "frameId": root_fid }),
                                )
                                .await
                        }
                        None => {
                            self.call("DOM.getFrameOwner", json!({ "frameId": root_fid }))
                                .await
                        }
                    };
                    if let Ok(owner) = owner_res
                        && let Some(obid) = owner["backendNodeId"].as_i64()
                    {
                        if cand_fids.len() == 1 {
                            parent_fid = Some(cand_fids[0].clone());
                            break;
                        } else if cand_fids.len() > 1 {
                            for cfid in cand_fids {
                                if let Ok(Value::Bool(true)) = self
                                    .call_function_on_node(
                                        obid,
                                        Some(cfid),
                                        "function() { return this.ownerDocument === document; }",
                                        vec![],
                                    )
                                    .await
                                {
                                    parent_fid = Some(cfid.clone());
                                    break;
                                }
                            }
                            if parent_fid.is_some() {
                                break;
                            }
                        }
                    }
                }
            }

            if parent_fid.is_none()
                && let Ok(tinfo) = self
                    .conn
                    .send_browser("Target.getTargetInfo", json!({ "targetId": ot.tid }))
                    .await
            {
                parent_fid = tinfo["targetInfo"]["parentFrameId"]
                    .as_str()
                    .or_else(|| tinfo["targetInfo"]["openerFrameId"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
            }

            ot.frames[0].parent_frame_id = if parent_fid.is_some() {
                parent_fid
            } else if attached_parent_sid.is_none() {
                main_fid_fallback.clone()
            } else {
                None
            };
        }

        let mut idx_by_fid: HashMap<String, usize> = out
            .iter()
            .enumerate()
            .map(|(i, f)| (f.frame_id.clone(), i))
            .collect();
        for ot in oopif_trees {
            for sf in ot.frames {
                if let Some(&idx) = idx_by_fid.get(&sf.frame_id) {
                    if sf.parent_frame_id.is_some() {
                        out[idx].parent_frame_id = sf.parent_frame_id;
                    }
                    if out[idx].url.is_empty() && !sf.url.is_empty() {
                        out[idx].url = sf.url;
                    }
                    if out[idx].name.is_none() && sf.name.is_some() {
                        out[idx].name = sf.name;
                    }
                } else {
                    idx_by_fid.insert(sf.frame_id.clone(), out.len());
                    out.push(sf);
                }
            }
        }

        Ok(out)
    }

    async fn eval_isolated_in_frame(&self, expr: &str, frame_id: Option<&str>) -> Result<Value> {
        self.eval_isolated_in_frame_with_policy(expr, frame_id, false)
            .await
    }

    async fn eval_main_world_in_frame(&self, expr: &str, frame_id: Option<&str>) -> Result<Value> {
        self.ensure_alive()?;
        if !self.state.main_world_eval {
            return Err(MimikyuError::MainWorldDisabled);
        }

        let wrapped_expr = can_wrap_cdp_eval_expr(expr).then(|| build_wrapped_cdp_eval_expr(expr));
        let mut use_wrapper = wrapped_expr.is_some();
        let mut last_transient_err: Option<MimikyuError> = None;

        for attempt in 0..CONTEXT_MAX_ATTEMPTS {
            self.ensure_alive()?;
            let (target_fid, used_default_ctx) = match frame_id {
                None => (None, None),
                Some(fid) => {
                    let main_fid = self.main_frame_id().await.ok();
                    let is_oopif = self.state.iframe_sessions.lock().contains_key(fid);
                    let cached_ctx = self.state.default_contexts.lock().get(fid).copied();
                    if main_fid.as_deref() == Some(fid) {
                        (None, None)
                    } else if is_oopif {
                        (Some(fid), cached_ctx)
                    } else if let Some(ctx_id) = cached_ctx {
                        (None, Some(ctx_id))
                    } else {
                        // The frame's default execution context may have just been cleared
                        // by Page.frameNavigated / Runtime.executionContextsCleared while
                        // navigating. Back off briefly to allow Runtime.executionContextCreated
                        // to repopulate it; if it never arrives and no transient context error
                        // occurred, preserve NotFound.
                        if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                            && !self.conn.closed.load(Ordering::SeqCst)
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        if let Some(prev_err) = last_transient_err.take() {
                            return Err(format_exhausted_context_error(
                                "main-world evaluation",
                                CONTEXT_MAX_ATTEMPTS,
                                prev_err,
                            ));
                        }
                        return Err(MimikyuError::NotFound(format!(
                            "frame '{fid}' has no default execution context"
                        )));
                    }
                }
            };

            if use_wrapper && let Some(ref wrapped) = wrapped_expr {
                let mut params = json!({
                    "expression": wrapped,
                    "returnByValue": true,
                    "awaitPromise": true,
                });
                if let Some(cid) = used_default_ctx {
                    params["contextId"] = json!(cid);
                }
                let eval_res = match target_fid {
                    Some(fid) => {
                        self.call_for_frame(Some(fid), "Runtime.evaluate", params)
                            .await
                    }
                    None => self.call("Runtime.evaluate", params).await,
                };
                match eval_res {
                    Ok(v) if is_compile_time_syntax_error(&v) => {
                        self.release_cdp_eval_objects(target_fid, &v).await;
                        use_wrapper = false;
                    }
                    Ok(v) => return self.resolve_cdp_wrapped_eval_result(target_fid, v).await,
                    Err(err) => match classify_transient_context_error(&err) {
                        Some(TransientContextErrorKind::PreExecution) => {
                            self.invalidate_frame_contexts(
                                frame_id,
                                target_fid,
                                None,
                                used_default_ctx,
                            );
                            last_transient_err = Some(err);
                            if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                && !self.conn.closed.load(Ordering::SeqCst)
                            {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                            break;
                        }
                        Some(TransientContextErrorKind::MidOrPostExecution) => {
                            self.invalidate_frame_contexts(
                                frame_id,
                                target_fid,
                                None,
                                used_default_ctx,
                            );
                            return Err(format_mid_eval_context_destroyed_error(
                                "main-world evaluation",
                                err,
                            ));
                        }
                        None => return Err(err),
                    },
                }
            }

            let mut params = json!({
                "expression": expr,
                "returnByValue": false,
                "awaitPromise": true,
            });
            if let Some(cid) = used_default_ctx {
                params["contextId"] = json!(cid);
            }
            let eval_res = match target_fid {
                Some(fid) => {
                    self.call_for_frame(Some(fid), "Runtime.evaluate", params)
                        .await
                }
                None => self.call("Runtime.evaluate", params).await,
            };

            let raw_val = match eval_res {
                Ok(v) => v,
                Err(err) => match classify_transient_context_error(&err) {
                    Some(TransientContextErrorKind::PreExecution) => {
                        self.invalidate_frame_contexts(
                            frame_id,
                            target_fid,
                            None,
                            used_default_ctx,
                        );
                        last_transient_err = Some(err);
                        if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                            && !self.conn.closed.load(Ordering::SeqCst)
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        break;
                    }
                    Some(TransientContextErrorKind::MidOrPostExecution) => {
                        self.invalidate_frame_contexts(
                            frame_id,
                            target_fid,
                            None,
                            used_default_ctx,
                        );
                        return Err(format_mid_eval_context_destroyed_error(
                            "main-world evaluation",
                            err,
                        ));
                    }
                    None => return Err(err),
                },
            };

            match self.resolve_cdp_eval_result(target_fid, raw_val).await {
                Ok(val) => return Ok(val),
                Err(err) if is_transient_context_error(&err) => {
                    self.invalidate_frame_contexts(frame_id, target_fid, None, used_default_ctx);
                    return Err(format_mid_eval_context_destroyed_error(
                        "main-world result serialization",
                        err,
                    ));
                }
                Err(err) => return Err(err),
            }
        }

        Err(format_exhausted_context_error(
            "main-world evaluation",
            CONTEXT_MAX_ATTEMPTS,
            last_transient_err.unwrap_or_else(|| MimikyuError::Protocol {
                code: -32000,
                message: "Execution context was destroyed".to_string(),
            }),
        ))
    }

    async fn call_function_on_node(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
        function_declaration: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        self.ensure_alive()?;
        let call_args: Vec<Value> = args.into_iter().map(|v| json!({ "value": v })).collect();
        let mut last_transient_err: Option<MimikyuError> = None;

        for attempt in 0..CONTEXT_MAX_ATTEMPTS {
            self.ensure_alive()?;
            let (fid, ctx_id) = match self.get_or_create_isolated_context_for(frame_id).await {
                Ok(pair) => pair,
                Err(err) if is_transient_context_error(&err) => {
                    self.invalidate_frame_contexts(frame_id, None, None, None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            };

            let group = format!(
                "mimikyu_node_{}",
                NEXT_OBJECT_GROUP_ID.fetch_add(1, Ordering::Relaxed)
            );
            let resolved = match self
                .call_for_frame(
                    Some(&fid),
                    "DOM.resolveNode",
                    json!({
                        "backendNodeId": backend_node_id,
                        "executionContextId": ctx_id,
                        "objectGroup": group,
                    }),
                )
                .await
            {
                Ok(v) => v,
                Err(err) if is_transient_context_error(&err) => {
                    let _ = self
                        .call_for_frame(
                            Some(&fid),
                            "Runtime.releaseObjectGroup",
                            json!({ "objectGroup": group }),
                        )
                        .await;
                    self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) if is_stale_dom_node_error(&err) => {
                    let _ = self
                        .call_for_frame(
                            Some(&fid),
                            "Runtime.releaseObjectGroup",
                            json!({ "objectGroup": group }),
                        )
                        .await;
                    return Err(MimikyuError::ElementNotFound(format!(
                        "backendNodeId {backend_node_id} could not be resolved: {err}"
                    )));
                }
                Err(err) => {
                    let _ = self
                        .call_for_frame(
                            Some(&fid),
                            "Runtime.releaseObjectGroup",
                            json!({ "objectGroup": group }),
                        )
                        .await;
                    return Err(err);
                }
            };

            let Some(obj_id) = resolved["object"]["objectId"]
                .as_str()
                .map(ToString::to_string)
            else {
                let _ = self
                    .call_for_frame(
                        Some(&fid),
                        "Runtime.releaseObjectGroup",
                        json!({ "objectGroup": group }),
                    )
                    .await;
                return Err(MimikyuError::ElementNotFound(format!(
                    "backendNodeId {backend_node_id} could not be resolved"
                )));
            };

            let res = self
                .call_for_frame(
                    Some(&fid),
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": obj_id,
                        "functionDeclaration": function_declaration,
                        "arguments": call_args,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                )
                .await;
            let _ = self
                .call_for_frame(
                    Some(&fid),
                    "Runtime.releaseObjectGroup",
                    json!({ "objectGroup": group }),
                )
                .await;

            let val = match res {
                Ok(v) => v,
                Err(err) => match classify_transient_context_error(&err) {
                    Some(TransientContextErrorKind::PreExecution) => {
                        self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                        last_transient_err = Some(err);
                        if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                            && !self.conn.closed.load(Ordering::SeqCst)
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        break;
                    }
                    Some(TransientContextErrorKind::MidOrPostExecution) => {
                        self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                        // If V8 rejected `objectId` lookup before invoking `functionDeclaration`,
                        // retrying re-resolves `backendNodeId` (failing with ElementNotFound if the
                        // document navigated away, or succeeding if only the isolated world recycled).
                        if matches!(
                            &err,
                            MimikyuError::Protocol { message, .. }
                                if message
                                    .to_ascii_lowercase()
                                    .contains("could not find object with given id")
                        ) {
                            last_transient_err = Some(err);
                            if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                                && !self.conn.closed.load(Ordering::SeqCst)
                            {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                            break;
                        }
                        return Err(format_mid_eval_context_destroyed_error(
                            "call_function_on_node",
                            err,
                        ));
                    }
                    None => return Err(err),
                },
            };

            if let Some(oid) = val["exceptionDetails"]["exception"]["objectId"].as_str() {
                let _ = self
                    .call_for_frame(
                        Some(&fid),
                        "Runtime.releaseObject",
                        json!({ "objectId": oid }),
                    )
                    .await;
            }
            return parse_eval_result(val);
        }

        Err(format_exhausted_context_error(
            "call_function_on_node",
            CONTEXT_MAX_ATTEMPTS,
            last_transient_err.unwrap_or_else(|| MimikyuError::Protocol {
                code: -32000,
                message: "Execution context was destroyed".to_string(),
            }),
        ))
    }

    async fn query_shadow(
        &self,
        selector_chain: &[String],
        frame_id: Option<&str>,
    ) -> Result<Vec<ShadowMatch>> {
        self.ensure_alive()?;
        if selector_chain.is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "query_shadow requires at least one selector".to_string(),
            ));
        }

        let mut last_transient_err: Option<MimikyuError> = None;
        for attempt in 0..CONTEXT_MAX_ATTEMPTS {
            self.ensure_alive()?;
            let (fid, ctx_id) = match self.get_or_create_isolated_context_for(frame_id).await {
                Ok(pair) => pair,
                Err(err) if is_transient_context_error(&err) => {
                    self.invalidate_frame_contexts(frame_id, None, None, None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            };
            let group = format!(
                "mimikyu_shadow_{}",
                NEXT_OBJECT_GROUP_ID.fetch_add(1, Ordering::Relaxed)
            );
            let res = self
                .query_shadow_inner(&fid, ctx_id, &group, selector_chain)
                .await;
            let _ = self
                .call_for_frame(
                    Some(&fid),
                    "Runtime.releaseObjectGroup",
                    json!({ "objectGroup": group }),
                )
                .await;
            match res {
                Ok(matches) => return Ok(matches),
                Err(err) if is_transient_context_error(&err) => {
                    self.invalidate_frame_contexts(frame_id, Some(&fid), Some(ctx_id), None);
                    last_transient_err = Some(err);
                    if let Some(&delay_ms) = CONTEXT_RETRY_BACKOFF_MS.get(attempt)
                        && !self.conn.closed.load(Ordering::SeqCst)
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    break;
                }
                Err(err) => return Err(err),
            }
        }

        Err(format_exhausted_context_error(
            "query_shadow",
            CONTEXT_MAX_ATTEMPTS,
            last_transient_err.unwrap_or_else(|| MimikyuError::Protocol {
                code: -32000,
                message: "Execution context was destroyed".to_string(),
            }),
        ))
    }

    async fn closed_shadow_hosts(&self, frame_id: Option<&str>) -> Result<Vec<i64>> {
        self.ensure_alive()?;
        let is_oopif =
            frame_id.is_some_and(|fid| self.state.iframe_sessions.lock().contains_key(fid));
        let main_fid = if !is_oopif {
            self.main_frame_id().await.ok()
        } else {
            None
        };
        let doc = self
            .call_for_frame(
                frame_id,
                "DOM.getDocument",
                json!({ "depth": -1, "pierce": true }),
            )
            .await?;
        let root_fid = doc["root"]
            .get("frameId")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .or_else(|| {
                if is_oopif {
                    frame_id.map(ToString::to_string)
                } else {
                    main_fid
                }
            });
        let mut hosts = Vec::new();
        collect_closed_shadow_roots(&doc["root"], root_fid.as_deref(), frame_id, &mut hosts);
        Ok(hosts)
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
        self.ensure_alive()?;
        let main_fid = self.main_frame_id().await.ok();
        let frames = self.list_frames().await.unwrap_or_default();

        // Capture bulk bounding boxes via DOMSnapshot (best-effort single RPC per renderer).
        let (bounds_map, shell_bids) = self.capture_dom_snapshot_bounds(&frames).await;

        let res = self.call("Accessibility.getFullAXTree", json!({})).await?;
        let main_raw = res["nodes"].as_array().cloned().unwrap_or_default();
        let mut seen_frames: HashSet<String> = HashSet::new();
        for n in &main_raw {
            if let Some(fid) = n["frameId"].as_str() {
                seen_frames.insert(fid.to_string());
            }
        }
        let mut nodes = parse_cdp_ax_nodes(&main_raw, None, main_fid.as_deref(), &bounds_map);

        // Order non-main frames topologically (parent before child) so nested OOPIFs attach cleanly.
        let mut remaining: Vec<FrameInfo> = frames
            .iter()
            .filter(|f| !f.is_main && !seen_frames.contains(&f.frame_id))
            .cloned()
            .collect();
        let mut ordered_frames: Vec<FrameInfo> = Vec::with_capacity(remaining.len());
        let mut resolved_fids: HashSet<String> = seen_frames.clone();
        if let Some(ref mfid) = main_fid {
            resolved_fids.insert(mfid.clone());
        }
        while !remaining.is_empty() {
            let prev_len = remaining.len();
            let mut next_remaining = Vec::new();
            for f in remaining {
                let parent_ready = f
                    .parent_frame_id
                    .as_deref()
                    .is_none_or(|p| resolved_fids.contains(p));
                if parent_ready {
                    resolved_fids.insert(f.frame_id.clone());
                    ordered_frames.push(f);
                } else {
                    next_remaining.push(f);
                }
            }
            if next_remaining.len() == prev_len {
                ordered_frames.extend(next_remaining);
                break;
            }
            remaining = next_remaining;
        }

        // Fetch child frame AX trees if not already included in main getFullAXTree.
        for f in &ordered_frames {
            if f.is_main || seen_frames.contains(&f.frame_id) {
                continue;
            }
            let prefix = format!("{}::", f.frame_id);
            let sub_res = self
                .call_for_frame(
                    Some(&f.frame_id),
                    "Accessibility.getFullAXTree",
                    json!({ "frameId": f.frame_id }),
                )
                .await;
            let Ok(sub_val) = sub_res else {
                continue;
            };
            let sub_raw = sub_val["nodes"].as_array().cloned().unwrap_or_default();
            if sub_raw.is_empty() {
                continue;
            }
            for n in &sub_raw {
                if let Some(sfid) = n["frameId"].as_str() {
                    seen_frames.insert(sfid.to_string());
                }
            }
            seen_frames.insert(f.frame_id.clone());

            let mut sub_nodes =
                parse_cdp_ax_nodes(&sub_raw, Some(&prefix), Some(&f.frame_id), &bounds_map);
            for sn in &mut sub_nodes {
                if sn.frame_id.is_none() {
                    sn.frame_id = Some(f.frame_id.clone());
                }
            }
            // Link subframe root node under the parent Iframe node via DOM.getFrameOwner on the owning parent frame.
            let owner_bid = self
                .call_for_frame(
                    f.parent_frame_id.as_deref(),
                    "DOM.getFrameOwner",
                    json!({ "frameId": f.frame_id }),
                )
                .await
                .ok()
                .and_then(|v| v["backendNodeId"].as_i64());
            if let Some(obid) = owner_bid
                && let Some(parent_idx) = nodes
                    .iter()
                    .position(|n| {
                        n.backend_node_id == Some(obid)
                            && f.parent_frame_id
                                .as_deref()
                                .is_none_or(|pfid| n.frame_id.as_deref() == Some(pfid))
                    })
                    .or_else(|| {
                        nodes.iter().position(|n| {
                            n.backend_node_id == Some(obid)
                                && (n.role.eq_ignore_ascii_case("Iframe")
                                    || n.role.eq_ignore_ascii_case("IframePresentational"))
                        })
                    })
            {
                let parent_node_id = nodes[parent_idx].node_id.clone();
                let mut root_ids = Vec::new();
                for sn in &mut sub_nodes {
                    if sn.parent_id.is_none() {
                        sn.parent_id = Some(parent_node_id.clone());
                        root_ids.push(sn.node_id.clone());
                    }
                }
                nodes[parent_idx].child_ids.extend(root_ids);
            } else if main_fid.is_some() {
                // Keep subframe nodes with parent_id=None as top-level sibling roots if owner not found
            }
            nodes.extend(sub_nodes);
        }

        Ok(unwrap_document_shell_nodes(nodes, &shell_bids))
    }

    async fn resolve_node_box(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
    ) -> Result<BoundingBox> {
        self.ensure_compositor_foreground().await?;
        let mut bbox_opt = None;
        let mut from_local_root = false;
        if let Ok(res) = self
            .call_for_frame(
                frame_id,
                "DOM.getContentQuads",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await
            && let Some(bbox) = res["quads"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(quad_to_bbox)
        {
            bbox_opt = Some(bbox);
            from_local_root = true;
        }

        if bbox_opt.is_none()
            && let Ok(res) = self
                .call_for_frame(
                    frame_id,
                    "DOM.getBoxModel",
                    json!({ "backendNodeId": backend_node_id }),
                )
                .await
            && let Some(bbox) = quad_to_bbox(&res["model"]["content"])
        {
            bbox_opt = Some(bbox);
            from_local_root = true;
        }

        if bbox_opt.is_none()
            && let Ok(val) = self
                .call_function_on_node(
                    backend_node_id,
                    frame_id,
                    r#"function() {
                        const r = this.getBoundingClientRect ? this.getBoundingClientRect() : null;
                        if (!r) return null;
                        return { x: r.x, y: r.y, width: r.width, height: r.height };
                    }"#,
                    vec![],
                )
                .await
            && !val.is_null()
            && let Ok(bbox) = serde_json::from_value::<BoundingBox>(val)
        {
            bbox_opt = Some(bbox);
        }

        let mut bbox = bbox_opt.ok_or_else(|| {
            MimikyuError::ElementNotFound(format!(
                "cannot resolve bounding box for backendNodeId {backend_node_id}"
            ))
        })?;

        // If frame_id is hosted in an OOPIF child session, accumulate host <iframe> offsets up to main frame.
        // DOM.getContentQuads / DOM.getBoxModel already include same-origin subframe offsets up to the
        // OOPIF's own local root frame, so when `from_local_root` is true we walk to that local root
        // before accumulating cross-process offsets (avoiding double-offsetting same-origin subframes
        // inside an OOPIF).
        let child_sid_opt =
            frame_id.and_then(|fid| self.state.iframe_sessions.lock().get(fid).cloned());
        if let Some(fid) = frame_id
            && (child_sid_opt.is_some() || !from_local_root)
        {
            let main_fid = self.main_frame_id().await.ok();
            if child_sid_opt.is_some() || main_fid.as_deref() != Some(fid) {
                let frames = self.list_frames().await.unwrap_or_default();
                let parent_map: HashMap<String, String> = frames
                    .iter()
                    .filter_map(|f| {
                        f.parent_frame_id
                            .as_ref()
                            .map(|p| (f.frame_id.clone(), p.clone()))
                    })
                    .collect();
                let offset_fid = if from_local_root && let Some(ref child_sid) = child_sid_opt {
                    let iframe_map = self.state.iframe_sessions.lock().clone();
                    let mut cur = fid.to_string();
                    for _ in 0..32 {
                        let Some(p) = parent_map.get(&cur) else {
                            break;
                        };
                        if iframe_map.get(p).map(String::as_str) == Some(child_sid.as_str()) {
                            cur = p.clone();
                        } else {
                            break;
                        }
                    }
                    cur
                } else {
                    fid.to_string()
                };
                let mut cache = HashMap::new();
                let tf = self
                    .resolve_frame_transform(
                        &offset_fid,
                        main_fid.as_deref(),
                        &parent_map,
                        &mut cache,
                    )
                    .await;
                tf.apply(&mut bbox);
            }
        }

        Ok(bbox)
    }

    async fn resolve_selector_box(&self, selector: &str) -> Result<BoundingBox> {
        let backend_id = self.query_backend_node_id(selector).await?;
        self.resolve_node_box(backend_id, None).await
    }

    async fn viewport_size(&self) -> Result<(f64, f64)> {
        self.ensure_compositor_foreground().await?;
        let val = self
            .eval_isolated_readonly(
                r#"(() => {
                    const iw = window.innerWidth || 1280;
                    const ih = window.innerHeight || 800;
                    const cw = (document.documentElement && document.documentElement.clientWidth) || iw;
                    const ch = (document.documentElement && document.documentElement.clientHeight) || ih;
                    return { w: Math.min(iw, cw), h: Math.min(ih, ch) };
                })()"#,
                None,
            )
            .await?;
        let w = val["w"].as_f64().unwrap_or(1280.0).max(1.0);
        let h = val["h"].as_f64().unwrap_or(800.0).max(1.0);
        Ok((w, h))
    }

    async fn viewport_and_gutters(&self) -> Result<(f64, f64, f64, f64)> {
        self.ensure_compositor_foreground().await?;
        let val = self
            .eval_isolated_readonly(
                r#"(() => {
                    const iw = window.innerWidth || 1280;
                    const ih = window.innerHeight || 800;
                    const cw = (document.documentElement && document.documentElement.clientWidth) || iw;
                    const ch = (document.documentElement && document.documentElement.clientHeight) || ih;
                    return {
                        w: iw,
                        h: ih,
                        right: Math.max(0, iw - cw),
                        bottom: Math.max(0, ih - ch),
                    };
                })()"#,
                None,
            )
            .await?;
        let w = val["w"].as_f64().unwrap_or(1280.0).max(1.0);
        let h = val["h"].as_f64().unwrap_or(800.0).max(1.0);
        let right = val["right"].as_f64().unwrap_or(0.0).max(0.0);
        let bottom = val["bottom"].as_f64().unwrap_or(0.0).max(0.0);
        Ok((w, h, right, bottom))
    }

    async fn scroll_into_view(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<()> {
        let bid = match (backend_node_id, selector) {
            (Some(b), _) => b,
            (None, Some(sel)) => self.query_backend_node_id_in_frame(sel, frame_id).await?,
            (None, None) => {
                return Err(MimikyuError::InvalidConfig(
                    "scroll_into_view requires backend_node_id or selector".to_string(),
                ));
            }
        };
        if self
            .call_for_frame(
                frame_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": bid }),
            )
            .await
            .is_ok()
        {
            return Ok(());
        }
        let _ = self
            .call_function_on_node(
                bid,
                frame_id,
                "function() { this.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' }); return true; }",
                vec![],
            )
            .await?;
        Ok(())
    }

    async fn clear_node_value(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<()> {
        let bid = match (backend_node_id, selector) {
            (Some(b), _) => b,
            (None, Some(sel)) => self.query_backend_node_id_in_frame(sel, frame_id).await?,
            (None, None) => {
                return Err(MimikyuError::InvalidConfig(
                    "clear_node_value requires backend_node_id or selector".to_string(),
                ));
            }
        };
        let ok = self
            .call_function_on_node(
                bid,
                frame_id,
                r#"function() {
                    if (!this) return false;
                    if (typeof this.focus === 'function') this.focus();
                    const tag = this.tagName;
                    if (tag === 'INPUT' || tag === 'TEXTAREA') {
                        const proto = tag === 'INPUT' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
                        const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
                        if (setter) setter.call(this, '');
                        else this.value = '';
                        this.dispatchEvent(new Event('input', { bubbles: true }));
                        return true;
                    }
                    if (this.isContentEditable) {
                        this.textContent = '';
                        this.dispatchEvent(new Event('input', { bubbles: true }));
                        return true;
                    }
                    return false;
                }"#,
                vec![],
            )
            .await?;
        if ok.as_bool() != Some(true) {
            return Err(MimikyuError::ElementNotFound(format!(
                "element {bid} is not a clearable input/textarea/contenteditable"
            )));
        }
        Ok(())
    }

    async fn select_option(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
        by: SelectBy,
    ) -> Result<String> {
        let bid = match (backend_node_id, selector) {
            (Some(b), _) => b,
            (None, Some(sel)) => self.query_backend_node_id_in_frame(sel, frame_id).await?,
            (None, None) => {
                return Err(MimikyuError::InvalidConfig(
                    "select_option requires backend_node_id or selector".to_string(),
                ));
            }
        };
        let (mode, val_arg): (&str, Value) = match by {
            SelectBy::Value(v) => ("value", json!(v)),
            SelectBy::Label(l) => ("label", json!(l)),
            SelectBy::Index(i) => ("index", json!(i)),
            SelectBy::Auto(a) => ("auto", json!(a)),
        };
        let res = self
            .call_function_on_node(
                bid,
                frame_id,
                r#"function(mode, target) {
                    if (!this || this.tagName !== 'SELECT') {
                        return { ok: false, error: 'element is not a <select>' };
                    }
                    const opts = Array.from(this.options || []);
                    let idx = -1;
                    if (mode === 'index') {
                        const i = Number(target);
                        if (Number.isInteger(i) && i >= 0 && i < opts.length) idx = i;
                    } else if (mode === 'value') {
                        idx = opts.findIndex(o => o.value === String(target));
                    } else if (mode === 'label') {
                        idx = opts.findIndex(o => (o.textContent || '').trim() === String(target));
                    } else {
                        idx = opts.findIndex(o => o.value === String(target));
                        if (idx < 0) idx = opts.findIndex(o => (o.textContent || '').trim() === String(target));
                    }
                    if (idx < 0 || opts[idx].disabled) {
                        return { ok: false, error: 'option not found or disabled' };
                    }
                    this.selectedIndex = idx;
                    opts[idx].selected = true;
                    this.dispatchEvent(new Event('input', { bubbles: true }));
                    this.dispatchEvent(new Event('change', { bubbles: true }));
                    return { ok: true, value: opts[idx].value };
                }"#,
                vec![json!(mode), val_arg],
            )
            .await?;
        if res["ok"].as_bool() == Some(true) {
            return Ok(res["value"].as_str().unwrap_or_default().to_string());
        }
        Err(MimikyuError::ElementNotFound(
            res["error"]
                .as_str()
                .unwrap_or("select_option failed")
                .to_string(),
        ))
    }

    async fn css_selector_for_node(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Ok(val) = self
            .call_function_on_node(
                backend_node_id,
                frame_id,
                r#"function() {
                    if (!this || this.nodeType !== 1) return null;
                    if (this.getRootNode && this.getRootNode() instanceof ShadowRoot) return null;
                    if (this.id) return '#' + CSS.escape(this.id);
                    const parts = [];
                    let cur = this;
                    while (cur && cur.nodeType === 1 && cur !== document.documentElement) {
                        let sel = cur.tagName.toLowerCase();
                        if (cur.id) {
                            parts.unshift('#' + CSS.escape(cur.id));
                            break;
                        }
                        const parent = cur.parentElement;
                        if (parent) {
                            const sameTag = Array.from(parent.children).filter(c => c.tagName === cur.tagName);
                            if (sameTag.length > 1) {
                                sel += `:nth-of-type(${sameTag.indexOf(cur) + 1})`;
                            }
                        }
                        parts.unshift(sel);
                        cur = parent;
                    }
                    return parts.join(' > ');
                }"#,
                vec![],
            )
            .await
        else {
            return Ok(None);
        };
        Ok(val.as_str().map(ToString::to_string))
    }

    async fn focus_node(&self, backend_node_id: Option<i64>, selector: Option<&str>) -> Result<()> {
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
        frame_id: Option<&str>,
        selector: Option<&str>,
        files: Vec<PathBuf>,
    ) -> Result<()> {
        let file_strs: Vec<String> = files
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        if let Some(bid) = backend_node_id {
            self.call_for_frame(
                frame_id,
                "DOM.setFileInputFiles",
                json!({ "backendNodeId": bid, "files": file_strs }),
            )
            .await?;
            return Ok(());
        }
        if let Some(sel) = selector {
            let bid = self.query_backend_node_id_in_frame(sel, frame_id).await?;
            self.call_for_frame(
                frame_id,
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
                self.dispatch_cdp_mouse_event(json!({
                    "type": "mouseMoved",
                    "x": ev.x,
                    "y": ev.y,
                    "button": button,
                    "modifiers": ev.modifiers,
                }))
                .await?;
            }
            MouseEventType::Down => {
                self.dispatch_cdp_mouse_event(json!({
                    "type": "mousePressed",
                    "x": ev.x,
                    "y": ev.y,
                    "button": button,
                    "clickCount": click_count,
                    "modifiers": ev.modifiers,
                }))
                .await?;
            }
            MouseEventType::Up => {
                self.dispatch_cdp_mouse_event(json!({
                    "type": "mouseReleased",
                    "x": ev.x,
                    "y": ev.y,
                    "button": button,
                    "clickCount": click_count,
                    "modifiers": ev.modifiers,
                }))
                .await?;
            }
            MouseEventType::Click => {
                self.dispatch_cdp_mouse_event(json!({
                    "type": "mouseMoved",
                    "x": ev.x,
                    "y": ev.y,
                    "button": "none",
                    "modifiers": ev.modifiers,
                }))
                .await?;
                self.dispatch_cdp_mouse_event(json!({
                    "type": "mousePressed",
                    "x": ev.x,
                    "y": ev.y,
                    "button": button,
                    "clickCount": click_count,
                    "modifiers": ev.modifiers,
                }))
                .await?;
                self.dispatch_cdp_mouse_event(json!({
                    "type": "mouseReleased",
                    "x": ev.x,
                    "y": ev.y,
                    "button": button,
                    "clickCount": click_count,
                    "modifiers": ev.modifiers,
                }))
                .await?;
            }
        }
        Ok(())
    }

    async fn dispatch_key(&self, ev: KeyEvent) -> Result<()> {
        self.ensure_compositor_foreground().await?;
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
            let fallback = ascii_key_code_and_vk(&ev.key);
            let code_opt = ev.code.as_deref().or_else(|| fallback.map(|f| f.0));
            let vk_opt = ev
                .windows_virtual_key_code
                .or_else(|| fallback.map(|f| f.1));
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
            if let Some(code) = code_opt {
                down_params["code"] = json!(code);
            }
            if let Some(vk) = vk_opt {
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
            if let Some(code) = code_opt {
                up_params["code"] = json!(code);
            }
            if let Some(vk) = vk_opt {
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
        let dx = crate::input::humanize::sanitize_wheel_delta(ev.delta_x);
        let dy = crate::input::humanize::sanitize_wheel_delta(ev.delta_y);
        self.dispatch_cdp_mouse_event(json!({
            "type": "mouseWheel",
            "x": ev.x,
            "y": ev.y,
            "deltaX": dx,
            "deltaY": dy,
            "modifiers": ev.modifiers,
        }))
        .await
    }

    async fn insert_text(&self, text: &str) -> Result<()> {
        self.ensure_compositor_foreground().await?;
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

        let scale = opts.scale.unwrap_or(1.0).max(0.05);
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
                    "scale": scale,
                });
            }
        } else if let Some(clip) = opts.clip {
            params["captureBeyondViewport"] = json!(true);
            let (page_x, page_y) = match self.call("Page.getLayoutMetrics", json!({})).await {
                Ok(metrics) => {
                    let px = metrics["cssVisualViewport"]["pageX"]
                        .as_f64()
                        .or_else(|| metrics["cssLayoutViewport"]["pageX"].as_f64())
                        .unwrap_or(0.0);
                    let py = metrics["cssVisualViewport"]["pageY"]
                        .as_f64()
                        .or_else(|| metrics["cssLayoutViewport"]["pageY"].as_f64())
                        .unwrap_or(0.0);
                    (px, py)
                }
                Err(_) => (0.0, 0.0),
            };
            params["clip"] = json!({
                "x": clip.x + page_x,
                "y": clip.y + page_y,
                "width": clip.width,
                "height": clip.height,
                "scale": scale,
            });
        }

        let res = self.call("Page.captureScreenshot", params).await;

        if opts.omit_background {
            let _ = self
                .call("Emulation.setDefaultBackgroundColorOverride", json!({}))
                .await;
        }

        let res = res?;
        let b64 = res["data"].as_str().ok_or_else(|| MimikyuError::Protocol {
            code: -1,
            message: "Page.captureScreenshot missing data".to_string(),
        })?;
        BASE64.decode(b64).map_err(|e| MimikyuError::Protocol {
            code: -1,
            message: format!("invalid base64 screenshot data: {e}"),
        })
    }

    async fn pdf(&self) -> Result<Vec<u8>> {
        let res = self
            .call("Page.printToPDF", json!({ "printBackground": true }))
            .await?;
        let b64 = res["data"].as_str().ok_or_else(|| MimikyuError::Protocol {
            code: -1,
            message: "Page.printToPDF missing data".to_string(),
        })?;
        BASE64.decode(b64).map_err(|e| MimikyuError::Protocol {
            code: -1,
            message: format!("invalid base64 PDF data: {e}"),
        })
    }

    async fn start_screencast(&self, opts: ScreencastOptions) -> Result<ScreencastStatus> {
        self.ensure_alive()?;
        if opts.out_dir.as_os_str().is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "screencast out_dir cannot be empty".to_string(),
            ));
        }
        if opts.out_dir.exists() && !opts.out_dir.is_dir() {
            return Err(MimikyuError::InvalidConfig(format!(
                "screencast out_dir '{}' exists and is not a directory",
                opts.out_dir.display()
            )));
        }
        std::fs::create_dir_all(&opts.out_dir)?;

        let started_at_ms = now_ms();
        {
            let mut guard = self.state.screencast.lock();
            if guard.as_ref().is_some_and(|s| s.active) {
                return Err(MimikyuError::InvalidConfig(
                    "screencast is already active for this session".to_string(),
                ));
            }
            *guard = Some(CdpScreencastState {
                active: true,
                out_dir: opts.out_dir.clone(),
                format: opts.format,
                frame_count: 0,
                started_at_ms,
            });
        }

        let mut params = json!({
            "format": opts.format.protocol_name(),
        });
        if let Some(q) = opts.quality {
            params["quality"] = json!(q.clamp(1, 100));
        } else if opts.format == ScreencastFormat::Jpeg {
            params["quality"] = json!(80);
        }
        if let Some(w) = opts.max_width {
            params["maxWidth"] = json!(w);
        }
        if let Some(h) = opts.max_height {
            params["maxHeight"] = json!(h);
        }
        if let Some(nth) = opts.every_nth_frame {
            params["everyNthFrame"] = json!(nth.max(1));
        }

        if let Err(err) = self.call("Page.startScreencast", params).await {
            *self.state.screencast.lock() = None;
            return Err(err);
        }

        Ok(ScreencastStatus {
            recording: true,
            out_dir: opts.out_dir,
            format: opts.format,
            frame_count: 0,
            started_at_ms,
            stopped_at_ms: None,
        })
    }

    async fn stop_screencast(&self) -> Result<ScreencastStatus> {
        self.ensure_alive()?;
        let state = {
            let mut guard = self.state.screencast.lock();
            match guard.take() {
                Some(sc) if sc.active => sc,
                _ => {
                    return Err(MimikyuError::InvalidConfig(
                        "no active screencast for this session".to_string(),
                    ));
                }
            }
        };

        self.call("Page.stopScreencast", json!({})).await?;

        Ok(ScreencastStatus {
            recording: false,
            out_dir: state.out_dir,
            format: state.format,
            frame_count: state.frame_count,
            started_at_ms: state.started_at_ms,
            stopped_at_ms: Some(now_ms()),
        })
    }

    async fn set_instant_animations(&self, enabled: bool) -> Result<()> {
        self.ensure_alive()?;
        self.state
            .instant_animations
            .store(enabled, Ordering::SeqCst);

        let _ = self.call("Animation.enable", json!({})).await;
        let rate = if enabled { 10_000.0 } else { 1.0 };
        self.call("Animation.setPlaybackRate", json!({ "playbackRate": rate }))
            .await?;

        {
            let mut guard = self.state.anim_script_id.lock().await;
            if let Some(old_id) = guard.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            if enabled {
                let res = self
                    .call(
                        "Page.addScriptToEvaluateOnNewDocument",
                        json!({
                            "source": INSTANT_ANIM_PRELOAD_JS,
                            "worldName": "mimikyu_isolated",
                        }),
                    )
                    .await;
                if let Ok(v) = res
                    && let Some(id) = v["identifier"].as_str()
                {
                    *guard = Some(id.to_string());
                } else if let Ok(id) = self.add_init_script(INSTANT_ANIM_PRELOAD_JS).await {
                    *guard = Some(id);
                }
            }
        }

        let js = instant_anim_dom_js(enabled)?;
        let frames = self.list_frames().await.unwrap_or_default();
        let target_frames: Vec<Option<String>> = if frames.is_empty() {
            vec![None]
        } else {
            frames.into_iter().map(|f| Some(f.frame_id)).collect()
        };

        for fid in target_frames {
            let _ = self.eval_isolated_in_frame(&js, fid.as_deref()).await;
        }

        let prev_child_scripts: Vec<(String, String)> = {
            let mut guard = self.state.child_anim_script_ids.lock();
            guard.drain().collect()
        };
        for (csid, old_script_id) in prev_child_scripts {
            let _ = self
                .conn
                .send_raw(
                    Some(&csid),
                    "Page.removeScriptToEvaluateOnNewDocument",
                    json!({ "identifier": old_script_id }),
                )
                .await;
        }

        let oopif_sids: Vec<String> = self
            .state
            .iframe_session_targets
            .lock()
            .keys()
            .cloned()
            .collect();
        let popup_sids: Vec<String> = self.state.popup_sessions.lock().values().cloned().collect();
        let popup_child_sids: Vec<String> = {
            let workers: HashSet<String> = self.state.worker_sessions.lock().clone();
            self.state
                .popup_child_sessions
                .lock()
                .keys()
                .filter(|sid| !workers.contains(*sid))
                .cloned()
                .collect()
        };

        let mut all_child_sids: Vec<String> =
            Vec::with_capacity(oopif_sids.len() + popup_sids.len() + popup_child_sids.len());
        all_child_sids.extend(oopif_sids);
        all_child_sids.extend(popup_sids.iter().cloned());
        all_child_sids.extend(popup_child_sids.iter().cloned());
        all_child_sids.sort();
        all_child_sids.dedup();

        for child_sid in &all_child_sids {
            let _ = self
                .conn
                .send_raw(Some(child_sid), "Animation.enable", json!({}))
                .await;
            let _ = self
                .conn
                .send_raw(
                    Some(child_sid),
                    "Animation.setPlaybackRate",
                    json!({ "playbackRate": rate }),
                )
                .await;
            if enabled {
                let res = self
                    .conn
                    .send_raw(
                        Some(child_sid),
                        "Page.addScriptToEvaluateOnNewDocument",
                        json!({
                            "source": INSTANT_ANIM_PRELOAD_JS,
                            "worldName": "mimikyu_isolated",
                        }),
                    )
                    .await;
                let res = match res {
                    Ok(v) if v["identifier"].as_str().is_some() => Ok(v),
                    _ => {
                        self.conn
                            .send_raw(
                                Some(child_sid),
                                "Page.addScriptToEvaluateOnNewDocument",
                                json!({ "source": INSTANT_ANIM_PRELOAD_JS }),
                            )
                            .await
                    }
                };
                if let Ok(v) = res
                    && let Some(script_id) = v["identifier"].as_str()
                {
                    if self.state.instant_animations.load(Ordering::SeqCst) {
                        self.state
                            .child_anim_script_ids
                            .lock()
                            .insert(child_sid.clone(), script_id.to_string());
                    } else {
                        let _ = self
                            .conn
                            .send_raw(
                                Some(child_sid),
                                "Page.removeScriptToEvaluateOnNewDocument",
                                json!({ "identifier": script_id }),
                            )
                            .await;
                    }
                }
            }
        }

        let mut popup_eval_sids: Vec<String> =
            Vec::with_capacity(popup_sids.len() + popup_child_sids.len());
        popup_eval_sids.extend(popup_sids);
        popup_eval_sids.extend(popup_child_sids);
        popup_eval_sids.sort();
        popup_eval_sids.dedup();
        for pop_sid in popup_eval_sids {
            apply_instant_anim_css_on_session(&self.conn, &pop_sid, enabled).await;
        }
        Ok(())
    }

    async fn set_env_overrides(&self, profile: EnvProfile) -> Result<()> {
        self.ensure_alive()?;
        *self.state.env_profile.lock() = Some(profile.clone());

        let ua_params = if !profile.user_agent.is_empty() || profile.user_agent_metadata.is_some() {
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

        let tz_val = (!profile.timezone.is_empty()).then(|| profile.timezone.clone());
        let loc_val = (!profile.locale.is_empty()).then(|| profile.locale.clone());
        let geo_val = profile.geolocation.as_ref().map(|geo| {
            json!({
                "latitude": geo.latitude,
                "longitude": geo.longitude,
                "accuracy": geo.accuracy,
            })
        });
        let hc_val = (profile.hardware_concurrency > 0).then_some(profile.hardware_concurrency);
        let metrics_val = json!({
            "width": profile.viewport_width,
            "height": profile.viewport_height,
            "deviceScaleFactor": profile.device_scale_factor,
            "mobile": profile.is_mobile,
            "screenWidth": profile.screen_width,
            "screenHeight": profile.screen_height,
        });
        let hdr_val = (!profile.extra_headers.is_empty())
            .then(|| json!({ "headers": profile.extra_headers }));

        *self.state.worker_ua_params.lock() = ua_params.clone();
        let worker_js = build_worker_init_script(&profile, EngineKind::Chromium, false);
        *self.state.worker_init_script.lock() = Some(worker_js.clone());
        *self.state.env_timezone.lock() = tz_val.clone();
        *self.state.env_locale.lock() = loc_val.clone();
        *self.state.env_geolocation.lock() = geo_val.clone();
        *self.state.env_hardware_concurrency.lock() = hc_val;
        *self.state.env_device_metrics.lock() = Some(metrics_val.clone());
        *self.state.env_extra_headers.lock() = hdr_val;

        let win_script = build_window_init_script(&profile, EngineKind::Chromium);
        *self.state.env_window_script.lock() = Some(win_script.clone());
        {
            let mut prev = self.state.env_script_id.lock().await;
            if let Some(old_id) = prev.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            let new_id = self.add_init_script(&win_script).await?;
            *prev = Some(new_id);
        }
        {
            let mut prev_iso = self.state.env_isolated_script_id.lock().await;
            if let Some(old_id) = prev_iso.take() {
                let _ = self.remove_init_script(&old_id).await;
            }
            let iso_res = self
                .call(
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({
                        "source": win_script,
                        "worldName": "mimikyu_isolated",
                    }),
                )
                .await?;
            if let Some(iso_id) = iso_res["identifier"].as_str() {
                *prev_iso = Some(iso_id.to_string());
            }
        }
        // Clear cached isolated worlds so the next eval picks up the newly registered isolated preload or runs env_window_script.
        self.state.isolated_worlds.lock().clear();
        let existing_iframes: Vec<String> = {
            let mut sids: Vec<String> = self
                .state
                .iframe_sessions
                .lock()
                .values()
                .cloned()
                .collect();
            sids.extend(self.state.popup_sessions.lock().values().cloned());
            let workers: HashSet<String> = self.state.worker_sessions.lock().clone();
            for csid in self.state.popup_child_sessions.lock().keys() {
                if !workers.contains(csid) {
                    sids.push(csid.clone());
                }
            }
            sids.sort();
            sids.dedup();
            sids
        };
        let popup_sids: HashSet<String> =
            self.state.popup_sessions.lock().values().cloned().collect();
        for child_sid in existing_iframes {
            let _ = self
                .conn
                .send_raw(
                    Some(&child_sid),
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({ "source": win_script }),
                )
                .await;
            let _ = self
                .conn
                .send_raw(
                    Some(&child_sid),
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({
                        "source": win_script,
                        "worldName": "mimikyu_isolated",
                    }),
                )
                .await;
            if let Some(ref p) = ua_params {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setUserAgentOverride",
                        p.clone(),
                    )
                    .await;
            }
            if let Some(ref tz) = tz_val {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setTimezoneOverride",
                        json!({ "timezoneId": tz }),
                    )
                    .await;
            }
            if let Some(ref loc) = loc_val {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setLocaleOverride",
                        json!({ "locale": loc }),
                    )
                    .await;
            }
            if let Some(ref geo) = geo_val {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setGeolocationOverride",
                        geo.clone(),
                    )
                    .await;
            }
            if let Some(hc) = hc_val {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setHardwareConcurrencyOverride",
                        json!({ "hardwareConcurrency": hc }),
                    )
                    .await;
            }
            if popup_sids.contains(&child_sid) {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setDeviceMetricsOverride",
                        metrics_val.clone(),
                    )
                    .await;
            }
            let _ = self
                .conn
                .send_raw(
                    Some(&child_sid),
                    "Network.setExtraHTTPHeaders",
                    json!({ "headers": profile.extra_headers }),
                )
                .await;
            let _ = self
                .conn
                .send_raw(
                    Some(&child_sid),
                    "Runtime.evaluate",
                    json!({
                        "expression": win_script,
                        "silent": true,
                    }),
                )
                .await;
        }
        let existing_workers: Vec<String> =
            self.state.worker_sessions.lock().iter().cloned().collect();
        for child_sid in existing_workers {
            if let Some(ref p) = ua_params {
                let _ = self
                    .conn
                    .send_raw(
                        Some(&child_sid),
                        "Emulation.setUserAgentOverride",
                        p.clone(),
                    )
                    .await;
            }
            let _ = self
                .conn
                .send_raw(
                    Some(&child_sid),
                    "Network.setExtraHTTPHeaders",
                    json!({ "headers": profile.extra_headers }),
                )
                .await;
            let _ = self
                .conn
                .send_raw(
                    Some(&child_sid),
                    "Runtime.evaluate",
                    json!({
                        "expression": worker_js,
                        "silent": true,
                    }),
                )
                .await;
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

        tokio::try_join!(
            ua_fut,
            tz_fut,
            loc_fut,
            metrics_fut,
            geo_fut,
            hc_fut,
            hdr_fut
        )?;

        let _ = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": win_script,
                    "silent": true,
                }),
            )
            .await;

        let main_fid_opt = self.state.main_frame_id.lock().clone();
        let same_proc_frames = self
            .state
            .collect_same_proc_env_contexts(main_fid_opt.as_deref());
        for (ctx_id, child_sid_opt) in same_proc_frames {
            let target_sid = child_sid_opt.as_deref().unwrap_or(&self.info.session_id);
            let _ = self
                .conn
                .send_raw(
                    Some(target_sid),
                    "Runtime.evaluate",
                    json!({
                        "expression": win_script,
                        "contextId": ctx_id,
                        "silent": true,
                    }),
                )
                .await;
        }
        let popup_frames: Vec<(String, i64)> = self
            .state
            .popup_default_contexts
            .lock()
            .iter()
            .map(|((sid, _fid), &cid)| (sid.clone(), cid))
            .collect();
        for (popup_sid, ctx_id) in popup_frames {
            let _ = self
                .conn
                .send_raw(
                    Some(&popup_sid),
                    "Runtime.evaluate",
                    json!({
                        "expression": win_script,
                        "contextId": ctx_id,
                        "silent": true,
                    }),
                )
                .await;
        }

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
            if let Err(err) = self
                .conn
                .send_browser("Browser.setPermission", params)
                .await
            {
                if g.state == PermissionState::Granted {
                    let mut gp = json!({ "permissions": [g.name] });
                    if let Some(ref orig) = g.origin {
                        gp["origin"] = json!(orig);
                    }
                    if let Some(ref ctx_id) = self.state.context_id {
                        gp["browserContextId"] = json!(ctx_id);
                    }
                    self.conn
                        .send_browser("Browser.grantPermissions", gp)
                        .await?;
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
        if self
            .call("Storage.setCookies", storage_params)
            .await
            .is_err()
        {
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
        if self
            .call("Storage.clearCookies", storage_params)
            .await
            .is_err()
        {
            self.call("Network.clearBrowserCookies", json!({})).await?;
        }
        Ok(())
    }

    async fn storage_state(&self) -> Result<StorageState> {
        let cookies = self.cookies().await?;
        let mut origins = Vec::new();
        let val = self
            .eval_isolated_readonly(
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
                None,
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
            .eval_isolated_readonly("window.location.origin", None)
            .await
            .ok()
            .and_then(|v| v.as_str().map(ToString::to_string));

        for entry in state.origins {
            if entry.local_storage.is_empty() && entry.session_storage.is_empty() {
                continue;
            }
            let ls_json = serde_json::to_string(&entry.local_storage)?;
            let ss_json = serde_json::to_string(&entry.session_storage)?;
            if current_origin.as_deref() == Some(entry.origin.as_str()) && entry.origin != "null" {
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

    fn pending_storage_origins(&self) -> Vec<String> {
        self.state
            .pending_storage_scripts
            .lock()
            .iter()
            .map(|(orig, _)| orig.clone())
            .collect()
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

    async fn inflight_activity(&self, ignore_long_lived: bool) -> Result<(usize, u64)> {
        self.ensure_alive()?;
        Ok(self.state.telemetry.inflight_stats(ignore_long_lived))
    }

    async fn document_ready_state(&self) -> Result<String> {
        let val = self
            .eval_isolated_readonly("document.readyState", None)
            .await?;
        Ok(val.as_str().unwrap_or("loading").to_string())
    }

    async fn dispose(&self) -> Result<()> {
        if self.state.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let had_active_screencast = self
            .state
            .screencast
            .lock()
            .take()
            .is_some_and(|sc| sc.active);
        if had_active_screencast {
            let _ = self
                .conn
                .send_raw(
                    Some(&self.info.session_id),
                    "Page.stopScreencast",
                    json!({}),
                )
                .await;
        }
        self.conn.sessions.write().remove(&self.info.session_id);
        self.conn
            .auto_attached_page_sessions
            .write()
            .remove(&self.state.target_id);
        self.conn
            .clear_foreground_target_if_matches(&self.state.target_id);
        self.conn
            .iframe_owner_sessions
            .write()
            .retain(|_, owner| owner != &self.info.session_id);
        self.conn
            .popup_owner_sessions
            .write()
            .retain(|_, owner| owner != &self.info.session_id);
        self.conn
            .popup_target_owners
            .write()
            .retain(|_, owner| owner != &self.info.session_id);
        self.conn
            .shared_worker_targets
            .write()
            .retain(|_, owner| owner != &self.info.session_id);

        if self.state.is_root {
            self.conn.shutdown_all().await;
        } else if self.state.owns_context {
            if let Some(ref ctx_id) = self.state.context_id {
                let mut removed_sids = HashSet::new();
                self.conn.sessions.write().retain(|sid, st| {
                    if st.context_id.as_deref() == Some(ctx_id.as_str()) {
                        st.disposed.store(true, Ordering::SeqCst);
                        removed_sids.insert(sid.clone());
                        false
                    } else {
                        true
                    }
                });
                if !removed_sids.is_empty() {
                    self.conn
                        .popup_owner_sessions
                        .write()
                        .retain(|_, owner| !removed_sids.contains(owner));
                    self.conn
                        .popup_target_owners
                        .write()
                        .retain(|_, owner| !removed_sids.contains(owner));
                    self.conn
                        .shared_worker_targets
                        .write()
                        .retain(|_, owner| !removed_sids.contains(owner));
                }
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
                self.conn.sync_browser_fetch().await;
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
            self.conn.sync_browser_fetch().await;
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

macro_rules! cdp_eval_json_validator_core {
    () => {
        r#"
    const ancestors = new Set();
    const toString = Object.prototype.toString;
    const nonJsonTags = new Set([
        '[object Window]',
        '[object global]',
        '[object Document]',
        '[object HTMLDocument]',
        '[object XMLDocument]',
        '[object DocumentFragment]',
        '[object ShadowRoot]',
        '[object Text]',
        '[object Comment]',
        '[object Map]',
        '[object Set]',
        '[object WeakMap]',
        '[object WeakSet]',
        '[object Date]',
        '[object RegExp]',
        '[object Error]',
        '[object Promise]',
        '[object ArrayBuffer]',
        '[object SharedArrayBuffer]',
        '[object DataView]',
        '[object Generator]',
        '[object AsyncGenerator]',
        '[object Blob]',
        '[object File]',
        '[object FormData]',
        '[object AbortController]',
        '[object AbortSignal]',
        '[object Event]',
        '[object CustomEvent]',
        '[object MessageEvent]',
        '[object ReadableStream]',
        '[object WritableStream]',
        '[object TransformStream]',
        '[object DOMException]',
        '[object Location]',
        '[object History]',
        '[object Navigator]',
        '[object Screen]',
        '[object Storage]',
        '[object CSSStyleDeclaration]',
        '[object CSS2Properties]'
    ]);
    const isRejectedObject = (v) => {
        if (v === globalThis) return true;
        if (typeof Node === 'function' && v instanceof Node) return true;
        if (typeof EventTarget === 'function' && v instanceof EventTarget) return true;
        if (typeof CSSStyleDeclaration === 'function' && v instanceof CSSStyleDeclaration) return true;
        if (ArrayBuffer.isView(v)) return true;
        const tag = toString.call(v);
        if (nonJsonTags.has(tag)) return true;
        if (tag.startsWith('[object HTML') || tag.startsWith('[object SVG') || tag.startsWith('[object MathML')) {
            return true;
        }
        return false;
    };
    const walk = (v) => {
        if (v === null) return null;
        const t = typeof v;
        if (t === 'boolean' || t === 'string') return v;
        if (t === 'number') {
            if (!Number.isFinite(v)) {
                throw new TypeError('JavaScript evaluation returned non-JSON-serializable value (number: ' + String(v) + ')');
            }
            return Object.is(v, -0) ? 0 : v;
        }
        if (t !== 'object') {
            throw new TypeError('JavaScript evaluation returned non-JSON-serializable value (' + t + ')');
        }
        if (ancestors.has(v)) {
            throw new TypeError('Converting circular structure to JSON');
        }
        if (Array.isArray(v)) {
            ancestors.add(v);
            try {
                const out = new Array(v.length);
                for (let i = 0; i < v.length; i++) {
                    if (i in v) {
                        const item = v[i];
                        out[i] = item === undefined ? null : walk(item);
                    } else {
                        out[i] = null;
                    }
                }
                return out;
            } finally {
                ancestors.delete(v);
            }
        }
        if (isRejectedObject(v)) {
            throw new TypeError('JavaScript evaluation returned non-JSON-serializable value (' + toString.call(v) + ')');
        }
        ancestors.add(v);
        try {
            const out = {};
            for (const k of Object.keys(v)) {
                const prop = v[k];
                if (prop !== undefined) {
                    Object.defineProperty(out, k, {
                        value: walk(prop),
                        enumerable: true,
                        configurable: true,
                        writable: true
                    });
                }
            }
            return out;
        } finally {
            ancestors.delete(v);
        }
    };
"#
    };
}

const CDP_EVAL_JSON_VALIDATOR_CORE: &str = cdp_eval_json_validator_core!();

const CDP_EVAL_JSON_VALIDATOR: &str = concat!(
    "function() {",
    cdp_eval_json_validator_core!(),
    "    return walk(this);\n}",
);

/// Backoff schedule between bounded CDP transient context recovery attempts.
/// 4 attempts total (initial + 3 retries: 25ms -> 75ms -> 150ms; 250ms cap).
const CONTEXT_RETRY_BACKOFF_MS: [u64; 3] = [25, 75, 150];
const CONTEXT_MAX_ATTEMPTS: usize = CONTEXT_RETRY_BACKOFF_MS.len() + 1;

/// Two-tier classification of transient CDP execution-context / navigation races.
///
/// - `PreExecution` (Tier A): The frame or execution context was already gone
///   before the script/command could execute (zero side effects possible).
/// - `MidOrPostExecution` (Tier B): The execution context, remote object handle,
///   or target was invalidated during or after script execution (side effects
///   may already have occurred; retry only for read-only internal queries).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransientContextErrorKind {
    PreExecution,
    MidOrPostExecution,
}

fn classify_transient_context_error(err: &MimikyuError) -> Option<TransientContextErrorKind> {
    let MimikyuError::Protocol { code, message } = err else {
        return None;
    };
    if *code != -32000 && *code != -32602 {
        return None;
    }
    let msg = message.to_ascii_lowercase();
    if msg.contains("cannot find context")
        || msg.contains("cannot find default execution context")
        || msg.contains("no frame for given id found")
    {
        return Some(TransientContextErrorKind::PreExecution);
    }
    if msg.contains("execution context was destroyed")
        || msg.contains("could not find object with given id")
        || msg.contains("inspected target navigated or closed")
    {
        return Some(TransientContextErrorKind::MidOrPostExecution);
    }
    None
}

fn is_transient_context_error(err: &MimikyuError) -> bool {
    classify_transient_context_error(err).is_some()
}

fn is_stale_dom_node_error(err: &MimikyuError) -> bool {
    let MimikyuError::Protocol { code, message } = err else {
        return false;
    };
    if *code != -32000 {
        return false;
    }
    let msg = message.to_ascii_lowercase();
    msg.contains("could not find node with given id")
        || msg.contains("no node with given id found")
        || msg.contains("node with given id does not belong to the document")
}

fn format_exhausted_context_error(
    operation: &str,
    attempts: usize,
    last_err: MimikyuError,
) -> MimikyuError {
    let (code, detail) = match last_err {
        MimikyuError::Protocol { code, message } => (code, message),
        other => (-32000, other.to_string()),
    };
    MimikyuError::Protocol {
        code,
        message: format!(
            "Execution context was destroyed during {operation} (exhausted {attempts} attempts while frame navigated: {detail})"
        ),
    }
}

fn format_mid_eval_context_destroyed_error(operation: &str, err: MimikyuError) -> MimikyuError {
    let (code, detail) = match err {
        MimikyuError::Protocol { code, message } => (code, message),
        other => (-32000, other.to_string()),
    };
    MimikyuError::Protocol {
        code,
        message: format!(
            "Execution context was destroyed during {operation}; not re-executed to preserve side-effect safety ({detail})"
        ),
    }
}

fn starts_with_js_keyword(s: &str, kw: &str) -> bool {
    let Some(rest) = s.strip_prefix(kw) else {
        return false;
    };
    rest.chars()
        .next()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
}

/// Returns true when `expr` can be safely wrapped inside a single-RPC
/// `Runtime.evaluate(returnByValue: true)` validator call without changing
/// top-level JS semantics (such as bare `{ ... }` block statements or
/// `function`/`class` declarations). Any remaining multi-statement scripts
/// that fail to parse inside `(...)` are detected via `is_compile_time_syntax_error`
/// before any JS executes and fall back cleanly to the 2-RPC path.
fn can_wrap_cdp_eval_expr(expr: &str) -> bool {
    let trimmed = expr.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('{')
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.ends_with(';')
    {
        return false;
    }
    const STATEMENT_PREFIXES: &[&str] = &[
        "function", "class", "var", "let", "const", "if", "for", "while", "do", "switch", "try",
        "throw", "with", "debugger", "return", "import", "export",
    ];
    if STATEMENT_PREFIXES
        .iter()
        .any(|kw| starts_with_js_keyword(trimmed, kw))
    {
        return false;
    }
    if starts_with_js_keyword(trimmed, "async") {
        let after_async = trimmed["async".len()..].trim_start();
        if starts_with_js_keyword(after_async, "function") {
            return false;
        }
    }
    true
}

/// Wraps a JS expression so `CDP_EVAL_JSON_VALIDATOR_CORE` validates and
/// normalizes the return value (or resolved Promise value) in a single atomic
/// `Runtime.evaluate(..., returnByValue: true, awaitPromise: true)` RPC.
///
/// Because `expr` is passed as the call argument, it evaluates in the outer
/// global scope (preserving `this === globalThis`, zero identifier shadowing,
/// and zero `eval()` / CSP dependency).
fn build_wrapped_cdp_eval_expr(expr: &str) -> String {
    format!(
        "((__mimikyu_eval_val) => {{\n\
            const __validate = (function() {{\n\
                {CDP_EVAL_JSON_VALIDATOR_CORE}\n\
                return (root) => (root === undefined ? null : walk(root));\n\
            }})();\n\
            if (__mimikyu_eval_val !== null && typeof __mimikyu_eval_val === 'object' && (\n\
                (typeof Promise === 'function' && __mimikyu_eval_val instanceof Promise) ||\n\
                (Object.prototype.toString.call(__mimikyu_eval_val) === '[object Promise]' &&\n\
                    typeof __mimikyu_eval_val.then === 'function' &&\n\
                    Function.prototype.toString.call(__mimikyu_eval_val.then).includes('[native code]'))\n\
            )) {{\n\
                return __mimikyu_eval_val.then(__validate);\n\
            }}\n\
            return __validate(__mimikyu_eval_val);\n\
        }})(\n{expr}\n)"
    )
}

/// Detects a V8 compile-time `SyntaxError` (zero instructions executed).
/// Runtime `SyntaxError`s (e.g. `JSON.parse('{')`, `throw new SyntaxError(...)`)
/// always carry non-empty `stackTrace.callFrames` and return `false` so
/// side-effecting scripts are NEVER re-executed.
fn is_compile_time_syntax_error(res: &Value) -> bool {
    let Some(exc) = res.get("exceptionDetails") else {
        return false;
    };
    let class_name = exc
        .get("exception")
        .and_then(|e| e.get("className"))
        .and_then(|v| v.as_str());
    let has_stack_frames = exc
        .get("stackTrace")
        .and_then(|st| st.get("callFrames"))
        .and_then(|cf| cf.as_array())
        .is_some_and(|frames| !frames.is_empty());
    class_name == Some("SyntaxError") && !has_stack_frames
}

#[derive(Debug)]
enum CdpEvalAction {
    Resolved(Result<Value>),
    ValidateObject { object_id: String },
}

fn classify_cdp_eval_remote_object(result: &Value) -> CdpEvalAction {
    let r_type = result.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let subtype = result.get("subtype").and_then(|v| v.as_str());
    match r_type {
        "undefined" => CdpEvalAction::Resolved(Ok(Value::Null)),
        "boolean" | "string" => match result.get("value") {
            Some(v) => CdpEvalAction::Resolved(Ok(v.clone())),
            None => CdpEvalAction::Resolved(Err(MimikyuError::EvalFailed(format!(
                "JavaScript evaluation returned {r_type} without value"
            )))),
        },
        "number" => {
            if result.get("unserializableValue").and_then(|v| v.as_str()) == Some("-0") {
                return CdpEvalAction::Resolved(Ok(json!(0)));
            }
            if let Some(v) = result.get("value")
                && v.as_f64().is_some_and(f64::is_finite)
            {
                return CdpEvalAction::Resolved(Ok(v.clone()));
            }
            let detail = result
                .get("unserializableValue")
                .and_then(|v| v.as_str())
                .or_else(|| result.get("description").and_then(|v| v.as_str()))
                .unwrap_or("non-finite number");
            CdpEvalAction::Resolved(Err(MimikyuError::EvalFailed(format!(
                "JavaScript evaluation returned non-JSON-serializable value (number: {detail})"
            ))))
        }
        "object" if subtype == Some("null") => CdpEvalAction::Resolved(Ok(Value::Null)),
        "object" if subtype.is_none() || subtype == Some("array") => {
            if let Some(oid) = result.get("objectId").and_then(|v| v.as_str()) {
                CdpEvalAction::ValidateObject {
                    object_id: oid.to_string(),
                }
            } else if let Some(v) = result.get("value") {
                CdpEvalAction::Resolved(Ok(v.clone()))
            } else {
                CdpEvalAction::Resolved(Err(MimikyuError::EvalFailed(
                    "JavaScript evaluation returned non-JSON-serializable object without objectId"
                        .to_string(),
                )))
            }
        }
        _ => {
            let detail = result
                .get("unserializableValue")
                .and_then(|v| v.as_str())
                .or_else(|| result.get("description").and_then(|v| v.as_str()))
                .unwrap_or("non-JSON-serializable value");
            let kind = if let Some(st) = subtype {
                st
            } else if r_type.is_empty() {
                "unknown"
            } else {
                r_type
            };
            CdpEvalAction::Resolved(Err(MimikyuError::EvalFailed(format!(
                "JavaScript evaluation returned non-JSON-serializable value ({kind}: {detail})"
            ))))
        }
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
    let r_type = result.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if r_type == "undefined" {
        return Ok(Value::Null);
    }
    if let Some(v) = result.get("value") {
        return Ok(v.clone());
    }
    if r_type == "number"
        && result.get("unserializableValue").and_then(|v| v.as_str()) == Some("-0")
    {
        return Ok(json!(0));
    }
    let detail = result
        .get("unserializableValue")
        .and_then(|v| v.as_str())
        .or_else(|| result.get("description").and_then(|v| v.as_str()))
        .unwrap_or("non-JSON-serializable value");
    let kind = if r_type.is_empty() { "unknown" } else { r_type };
    Err(MimikyuError::EvalFailed(format!(
        "JavaScript evaluation returned non-JSON-serializable value ({kind}: {detail})"
    )))
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameTransform {
    ox: f64,
    oy: f64,
    sx: f64,
    sy: f64,
}

impl FrameTransform {
    fn identity() -> Self {
        Self {
            ox: 0.0,
            oy: 0.0,
            sx: 1.0,
            sy: 1.0,
        }
    }

    fn compose_inner(self, inner: Self) -> Self {
        Self {
            ox: self.ox + inner.ox * self.sx,
            oy: self.oy + inner.oy * self.sy,
            sx: self.sx * inner.sx,
            sy: self.sy * inner.sy,
        }
    }

    fn apply(self, bbox: &mut BoundingBox) {
        bbox.x = self.ox + bbox.x * self.sx;
        bbox.y = self.oy + bbox.y * self.sy;
        bbox.width *= self.sx;
        bbox.height *= self.sy;
    }
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
    let non_text_mod_active = ev.modifiers & (MOD_ALT | MOD_CONTROL | MOD_META) != 0;
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

    let resolved_text = ev.text.clone().or_else(|| {
        if non_text_mod_active {
            None
        } else {
            single_char.map(|_| resolved_key.clone())
        }
    });

    (resolved_key, resolved_text)
}

fn ascii_key_code_and_vk(key: &str) -> Option<(&'static str, u32)> {
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return None;
    };
    match ch.to_ascii_uppercase() {
        'A' => Some(("KeyA", 65)),
        'B' => Some(("KeyB", 66)),
        'C' => Some(("KeyC", 67)),
        'D' => Some(("KeyD", 68)),
        'E' => Some(("KeyE", 69)),
        'F' => Some(("KeyF", 70)),
        'G' => Some(("KeyG", 71)),
        'H' => Some(("KeyH", 72)),
        'I' => Some(("KeyI", 73)),
        'J' => Some(("KeyJ", 74)),
        'K' => Some(("KeyK", 75)),
        'L' => Some(("KeyL", 76)),
        'M' => Some(("KeyM", 77)),
        'N' => Some(("KeyN", 78)),
        'O' => Some(("KeyO", 79)),
        'P' => Some(("KeyP", 80)),
        'Q' => Some(("KeyQ", 81)),
        'R' => Some(("KeyR", 82)),
        'S' => Some(("KeyS", 83)),
        'T' => Some(("KeyT", 84)),
        'U' => Some(("KeyU", 85)),
        'V' => Some(("KeyV", 86)),
        'W' => Some(("KeyW", 87)),
        'X' => Some(("KeyX", 88)),
        'Y' => Some(("KeyY", 89)),
        'Z' => Some(("KeyZ", 90)),
        '0' => Some(("Digit0", 48)),
        '1' => Some(("Digit1", 49)),
        '2' => Some(("Digit2", 50)),
        '3' => Some(("Digit3", 51)),
        '4' => Some(("Digit4", 52)),
        '5' => Some(("Digit5", 53)),
        '6' => Some(("Digit6", 54)),
        '7' => Some(("Digit7", 55)),
        '8' => Some(("Digit8", 56)),
        '9' => Some(("Digit9", 57)),
        _ => None,
    }
}

fn collect_cdp_frames(tree: &Value, parent_id: Option<&str>, out: &mut Vec<FrameInfo>) {
    let frame = &tree["frame"];
    let Some(fid) = frame["id"].as_str() else {
        return;
    };
    let name = frame["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let url = frame["url"].as_str().unwrap_or_default().to_string();
    out.push(FrameInfo {
        frame_id: fid.to_string(),
        parent_frame_id: parent_id.map(ToString::to_string),
        name,
        url,
        is_main: parent_id.is_none(),
    });
    if let Some(children) = tree["childFrames"].as_array() {
        for child in children {
            collect_cdp_frames(child, Some(fid), out);
        }
    }
}

fn collect_closed_shadow_roots(
    node: &Value,
    current_frame_id: Option<&str>,
    filter_frame_id: Option<&str>,
    out: &mut Vec<i64>,
) {
    let node_fid = node.get("frameId").and_then(|v| v.as_str());
    let is_document = node.get("nodeType").and_then(|v| v.as_i64()) == Some(9);
    let effective_frame_id = if is_document {
        node_fid.or(current_frame_id)
    } else {
        current_frame_id
    };
    let frame_matches = filter_frame_id.is_none_or(|target| effective_frame_id == Some(target));
    if let Some(srs) = node.get("shadowRoots").and_then(|v| v.as_array()) {
        for sr in srs {
            if frame_matches
                && sr.get("shadowRootType").and_then(|v| v.as_str()) == Some("closed")
                && let Some(bid) = sr.get("backendNodeId").and_then(|v| v.as_i64())
            {
                out.push(bid);
            }
            collect_closed_shadow_roots(sr, effective_frame_id, filter_frame_id, out);
        }
    }
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for ch in children {
            collect_closed_shadow_roots(ch, effective_frame_id, filter_frame_id, out);
        }
    }
    if let Some(cd) = node.get("contentDocument") {
        let cd_frame_id = node_fid.or(effective_frame_id);
        collect_closed_shadow_roots(cd, cd_frame_id, filter_frame_id, out);
    }
}

fn parse_cdp_ax_nodes(
    raw_nodes: &[Value],
    id_prefix: Option<&str>,
    default_frame_id: Option<&str>,
    bounds_map: &HashMap<(Option<String>, i64), BoundingBox>,
) -> Vec<AxNode> {
    let scope_id = |id: &str| -> String {
        match id_prefix {
            Some(p) => format!("{p}{id}"),
            None => id.to_string(),
        }
    };
    let mut nodes = Vec::with_capacity(raw_nodes.len());
    for n in raw_nodes {
        let raw_nid = n["nodeId"].as_str().unwrap_or_default();
        let node_id = scope_id(raw_nid);
        let parent_id = n["parentId"].as_str().map(&scope_id);
        let child_ids = n["childIds"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(&scope_id))
                    .collect()
            })
            .unwrap_or_default();
        let role = n["role"]["value"].as_str().unwrap_or("unknown").to_string();
        let name = n["name"]["value"].as_str().map(ToString::to_string);
        let value = n["value"]["value"]
            .as_str()
            .map(ToString::to_string)
            .or_else(|| n["value"]["value"].as_f64().map(|f| f.to_string()));
        let description = n["description"]["value"].as_str().map(ToString::to_string);
        let backend_node_id = n["backendDOMNodeId"].as_i64();
        let frame_id = n["frameId"]
            .as_str()
            .map(ToString::to_string)
            .or_else(|| default_frame_id.map(ToString::to_string));
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

        let bounds =
            backend_node_id.and_then(|bid| bounds_map.get(&(frame_id.clone(), bid)).copied());

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
            bounds,
        });
    }
    nodes
}

fn is_unwrappable_doc_shell(node: &AxNode, shell_bids: &HashSet<(Option<String>, i64)>) -> bool {
    if node.parent_id.is_none() || node.role.eq_ignore_ascii_case("RootWebArea") {
        return false;
    }
    let Some(bid) = node.backend_node_id else {
        return false;
    };
    if !shell_bids.contains(&(node.frame_id.clone(), bid)) {
        return false;
    }
    let role_ok = node.role.eq_ignore_ascii_case("none")
        || node.role.eq_ignore_ascii_case("generic")
        || node.ignored;
    if !role_ok {
        return false;
    }
    node.name.as_deref().map(str::trim).unwrap_or("").is_empty()
        && !node.focusable
        && !node.focused
        && !node.disabled
        && node.checked.is_none()
        && node.expanded.is_none()
        && node.selected.is_none()
}

fn unwrap_document_shell_nodes(
    nodes: Vec<AxNode>,
    shell_bids: &HashSet<(Option<String>, i64)>,
) -> Vec<AxNode> {
    if shell_bids.is_empty() || nodes.is_empty() {
        return nodes;
    }
    let removed: HashSet<String> = nodes
        .iter()
        .filter(|n| is_unwrappable_doc_shell(n, shell_bids))
        .map(|n| n.node_id.clone())
        .collect();
    if removed.is_empty() {
        return nodes;
    }

    let mut parent_of: HashMap<String, Option<String>> = HashMap::with_capacity(nodes.len());
    let mut children_of: HashMap<String, Vec<String>> = HashMap::with_capacity(nodes.len());
    for n in &nodes {
        parent_of.insert(n.node_id.clone(), n.parent_id.clone());
        children_of
            .entry(n.node_id.clone())
            .or_default()
            .extend(n.child_ids.iter().cloned());
    }
    for n in &nodes {
        if let Some(ref pid) = n.parent_id {
            let list = children_of.entry(pid.clone()).or_default();
            if !list.contains(&n.node_id) {
                list.push(n.node_id.clone());
            }
        }
    }

    let resolve_parent = |start: Option<&str>| -> Option<String> {
        let mut cur = start;
        let mut seen = HashSet::new();
        while let Some(pid) = cur {
            if !removed.contains(pid) {
                return Some(pid.to_string());
            }
            if !seen.insert(pid) {
                return None;
            }
            cur = parent_of.get(pid).and_then(|p| p.as_deref());
        }
        None
    };

    fn expand_children(
        cids: &[String],
        removed: &HashSet<String>,
        children_of: &HashMap<String, Vec<String>>,
        visited: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) {
        for cid in cids {
            if removed.contains(cid) {
                if visited.insert(cid.clone())
                    && let Some(grand) = children_of.get(cid)
                {
                    expand_children(grand, removed, children_of, visited, out);
                }
            } else if !out.contains(cid) {
                out.push(cid.clone());
            }
        }
    }

    let mut out = Vec::with_capacity(nodes.len().saturating_sub(removed.len()));
    for mut node in nodes {
        if removed.contains(&node.node_id) {
            continue;
        }
        node.parent_id = resolve_parent(node.parent_id.as_deref());
        let raw_cids = children_of
            .get(&node.node_id)
            .cloned()
            .unwrap_or_else(|| std::mem::take(&mut node.child_ids));
        let mut expanded = Vec::with_capacity(raw_cids.len());
        let mut visited = HashSet::new();
        expand_children(
            &raw_cids,
            &removed,
            &children_of,
            &mut visited,
            &mut expanded,
        );
        node.child_ids = expanded;
        out.push(node);
    }
    out
}

fn named_key_cdp_spec(
    key: &str,
) -> Option<(&'static str, &'static str, u32, Option<&'static str>)> {
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

        let ctrl_chord = KeyEvent {
            event_type: KeyEventType::Char,
            key: "a".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: MOD_CONTROL,
        };
        assert_eq!(
            resolve_cdp_char_key_and_text(&ctrl_chord),
            ("a".to_string(), None)
        );
        assert_eq!(ascii_key_code_and_vk("a"), Some(("KeyA", 65)));
    }

    #[test]
    fn cdp_screencast_state_numbers_frames_and_parses_formats() {
        let mut sc = CdpScreencastState {
            active: true,
            out_dir: PathBuf::from("/tmp/frames"),
            format: ScreencastFormat::Png,
            frame_count: 0,
            started_at_ms: 100,
        };
        assert_eq!(
            sc.next_frame_path(),
            PathBuf::from("/tmp/frames/frame-000001.png")
        );
        assert_eq!(
            sc.next_frame_path(),
            PathBuf::from("/tmp/frames/frame-000002.png")
        );
        assert_eq!(sc.frame_count, 2);

        let mut sc_jpg = CdpScreencastState {
            active: true,
            out_dir: PathBuf::from("/tmp/jpg"),
            format: ScreencastFormat::Jpeg,
            frame_count: 0,
            started_at_ms: 100,
        };
        assert_eq!(
            sc_jpg.next_frame_path(),
            PathBuf::from("/tmp/jpg/frame-000001.jpg")
        );
        assert_eq!(
            "png".parse::<ScreencastFormat>().unwrap(),
            ScreencastFormat::Png
        );
        assert_eq!(
            "jpeg".parse::<ScreencastFormat>().unwrap(),
            ScreencastFormat::Jpeg
        );
        assert_eq!(
            "jpg".parse::<ScreencastFormat>().unwrap(),
            ScreencastFormat::Jpeg
        );
        assert!("gif".parse::<ScreencastFormat>().is_err());
    }

    fn dummy_session_state() -> CdpSessionState {
        CdpSessionState {
            session_id: "main-sid".to_string(),
            target_id: "main-tid".to_string(),
            context_id: None,
            owns_context: false,
            is_root: true,
            main_world_eval: false,
            telemetry_capacity: None,
            main_frame_id: Mutex::new(Some("main-fid".to_string())),
            last_main_origin: Mutex::new(None),
            isolated_worlds: Mutex::new(HashMap::new()),
            default_contexts: Mutex::new(HashMap::new()),
            default_context_sessions: Mutex::new(HashMap::new()),
            iframe_sessions: Mutex::new(HashMap::new()),
            iframe_session_targets: Mutex::new(HashMap::new()),
            iframe_parent_sessions: Mutex::new(HashMap::new()),
            popup_sessions: Mutex::new(HashMap::new()),
            popup_child_sessions: Mutex::new(HashMap::new()),
            popup_default_contexts: Mutex::new(HashMap::new()),
            worker_sessions: Mutex::new(HashSet::new()),
            pending_cdp_requests: Mutex::new(HashMap::new()),
            finalized_doc_loaders: Mutex::new(VecDeque::new()),
            telemetry: Arc::new(TelemetryStore::default()),
            interceptor: Mutex::new(NetworkInterceptor::default()),
            fetch_enabled: AtomicBool::new(false),
            instant_animations: AtomicBool::new(false),
            anim_script_id: AsyncMutex::new(None),
            child_anim_script_ids: Mutex::new(HashMap::new()),
            env_script_id: AsyncMutex::new(None),
            env_isolated_script_id: AsyncMutex::new(None),
            env_window_script: Mutex::new(None),
            worker_init_script: Mutex::new(None),
            worker_ua_params: Mutex::new(None),
            env_timezone: Mutex::new(None),
            env_locale: Mutex::new(None),
            env_geolocation: Mutex::new(None),
            env_hardware_concurrency: Mutex::new(None),
            env_device_metrics: Mutex::new(None),
            env_extra_headers: Mutex::new(None),
            env_profile: Mutex::new(None),
            pending_storage_scripts: Mutex::new(Vec::new()),
            screencast: Mutex::new(None),
            _proxy_relay: Mutex::new(None),
            load_notify: Notify::new(),
            disposed: AtomicBool::new(false),
        }
    }

    #[test]
    fn cdp_pending_oopif_document_finalizes_and_prunes_cleanly() {
        let state = dummy_session_state();
        let insert_req = |req_id: &str, fid: &str, lid: &str, rt: &str| {
            state.pending_cdp_requests.lock().insert(
                req_id.to_string(),
                PendingCdpRequest {
                    session_id: "main-sid".to_string(),
                    frame_id: Some(fid.to_string()),
                    loader_id: Some(lid.to_string()),
                    resource_type: rt.to_string(),
                    received_response: true,
                    committed: false,
                },
            );
            state.telemetry.on_request_started(
                NetworkEvent {
                    timestamp_ms: 1,
                    request_id: req_id.to_string(),
                    url: "http://localhost/child".to_string(),
                    method: "GET".to_string(),
                    resource_type: rt.to_string(),
                    status: Some(200),
                    mime_type: Some("text/html".to_string()),
                    failed: false,
                    error_text: None,
                    request_headers: BTreeMap::new(),
                    response_headers: BTreeMap::new(),
                },
                None,
                None,
                BTreeMap::new(),
            );
        };

        // 1. Committed OOPIF Document finalizes on frameStoppedLoading
        insert_req("req-oopif-1", "child-fid-1", "req-oopif-1", "Document");
        assert_eq!(state.telemetry.inflight_count(), 1);
        state.mark_document_committed_and_prune_superseded("child-fid-1", Some("req-oopif-1"));
        assert_eq!(state.telemetry.inflight_count(), 1);
        state.finalize_document_requests_for_frame("child-fid-1", true);
        assert_eq!(state.telemetry.inflight_count(), 0);

        // 2. Frame detached prunes pending requests on that frame
        insert_req("req-oopif-2", "child-fid-2", "req-oopif-2", "Document");
        assert_eq!(state.telemetry.inflight_count(), 1);
        state.prune_pending_requests_for_frame("child-fid-2");
        assert_eq!(state.telemetry.inflight_count(), 0);

        // 3. Top-level navigation prunes requests outside new loaderId
        insert_req("req-old-fetch", "main-fid", "loader-old", "Fetch");
        insert_req("req-new-doc", "main-fid", "loader-new", "Document");
        assert_eq!(state.telemetry.inflight_count(), 2);
        state.prune_pending_requests_outside_loader("loader-new");
        assert_eq!(state.telemetry.inflight_count(), 1);
        assert!(
            state
                .pending_cdp_requests
                .lock()
                .contains_key("req-new-doc")
        );

        // 4. Child-session Fetch + Document deduplication + SSE ignore_long_lived + detach pruning
        state.finalize_document_requests_for_frame("main-fid", false);
        assert_eq!(state.telemetry.inflight_count(), 0);
        // Simulate finalized loaderId for child Document: re-sent from child must be ignored
        state.record_finalized_doc_loader(Some("loader-oopif-done"));
        assert!(state.is_doc_loader_finalized("loader-oopif-done"));
        state.pending_cdp_requests.lock().insert(
            "child-fetch-1".to_string(),
            PendingCdpRequest {
                session_id: "child-sid-1".to_string(),
                frame_id: Some("child-fid-1".to_string()),
                loader_id: Some("loader-child-1".to_string()),
                resource_type: "Fetch".to_string(),
                received_response: false,
                committed: false,
            },
        );
        state.telemetry.on_request_started(
            NetworkEvent {
                timestamp_ms: 2,
                request_id: "child-fetch-1".to_string(),
                url: "http://localhost/api/delayed".to_string(),
                method: "GET".to_string(),
                resource_type: "Fetch".to_string(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        state.pending_cdp_requests.lock().insert(
            "child-sse-1".to_string(),
            PendingCdpRequest {
                session_id: "child-sid-1".to_string(),
                frame_id: Some("child-fid-1".to_string()),
                loader_id: Some("loader-child-1".to_string()),
                resource_type: "EventSource".to_string(),
                received_response: true,
                committed: false,
            },
        );
        state.telemetry.on_request_started(
            NetworkEvent {
                timestamp_ms: 3,
                request_id: "child-sse-1".to_string(),
                url: "http://localhost/sse".to_string(),
                method: "GET".to_string(),
                resource_type: "EventSource".to_string(),
                status: Some(200),
                mime_type: Some("text/event-stream".to_string()),
                failed: false,
                error_text: None,
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            },
            None,
            None,
            BTreeMap::new(),
        );
        assert_eq!(state.telemetry.inflight_stats(false).0, 2);
        assert_eq!(state.telemetry.inflight_stats(true).0, 1);
        let mut det_sids = HashSet::new();
        det_sids.insert("child-sid-1".to_string());
        state.prune_pending_requests_for_detached(&det_sids, &HashSet::new());
        assert_eq!(state.telemetry.inflight_count(), 0);
    }

    #[test]
    fn cdp_frame_transform_composes_nested_nonuniform_scale_and_offset() {
        let outer = FrameTransform {
            ox: 24.0 + 10.0 * 1.5,
            oy: 16.0 + 8.0 * 0.8,
            sx: 1.5,
            sy: 0.8,
        };
        let inner = FrameTransform {
            ox: 10.0 + 5.0 * 0.8,
            oy: 20.0 + 4.0 * 1.25,
            sx: 0.8,
            sy: 1.25,
        };
        let composed = outer.compose_inner(inner);
        assert!((composed.sx - 1.2).abs() < 1e-9);
        assert!((composed.sy - 1.0).abs() < 1e-9);
        assert!((composed.ox - (39.0 + 14.0 * 1.5)).abs() < 1e-9);
        assert!((composed.oy - (22.4 + 25.0 * 0.8)).abs() < 1e-9);

        let mut bbox = BoundingBox {
            x: 100.0,
            y: 40.0,
            width: 80.0,
            height: 30.0,
        };
        composed.apply(&mut bbox);
        assert!((bbox.x - (60.0 + 100.0 * 1.2)).abs() < 1e-9);
        assert!((bbox.y - (42.4 + 40.0 * 1.0)).abs() < 1e-9);
        assert!((bbox.width - 96.0).abs() < 1e-9);
        assert!((bbox.height - 30.0).abs() < 1e-9);
    }

    #[test]
    fn parse_eval_result_handles_undefined_null_neg_zero_and_rejects_unserializable() {
        assert_eq!(
            parse_eval_result(json!({ "result": { "type": "undefined" } })).unwrap(),
            Value::Null
        );
        assert_eq!(
            parse_eval_result(json!({
                "result": { "type": "object", "subtype": "null", "value": null }
            }))
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            parse_eval_result(json!({
                "result": { "type": "number", "value": 42 }
            }))
            .unwrap(),
            json!(42)
        );
        assert_eq!(
            parse_eval_result(json!({
                "result": { "type": "number", "unserializableValue": "-0", "description": "-0" }
            }))
            .unwrap(),
            json!(0)
        );

        for unserializable in [
            json!({ "result": { "type": "bigint", "unserializableValue": "42n", "description": "42n" } }),
            json!({ "result": { "type": "number", "unserializableValue": "NaN", "description": "NaN" } }),
            json!({ "result": { "type": "number", "unserializableValue": "Infinity", "description": "Infinity" } }),
            json!({ "result": { "type": "number", "unserializableValue": "-Infinity", "description": "-Infinity" } }),
            json!({ "result": { "type": "symbol", "description": "Symbol(s)", "objectId": "obj-1" } }),
            json!({ "result": { "type": "function", "description": "() => {}", "objectId": "obj-2" } }),
            json!({ "result": { "type": "object", "description": "Object", "objectId": "obj-3" } }),
        ] {
            let err = parse_eval_result(unserializable.clone())
                .expect_err(&format!("expected EvalFailed for {unserializable}"));
            assert!(
                matches!(err, MimikyuError::EvalFailed(_)),
                "unexpected error type: {err:?}"
            );
        }

        let exc_err = parse_eval_result(json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": { "description": "TypeError: Converting circular structure to JSON" }
            }
        }))
        .expect_err("exceptionDetails must fail");
        assert!(matches!(exc_err, MimikyuError::EvalFailed(msg) if msg.contains("circular")));
    }

    #[test]
    fn classify_cdp_eval_remote_object_handles_primitives_rejects_non_json_and_validates_objects() {
        // Primitives resolve immediately
        match classify_cdp_eval_remote_object(&json!({ "type": "undefined" })) {
            CdpEvalAction::Resolved(Ok(v)) => assert_eq!(v, Value::Null),
            other => panic!("unexpected action: {other:?}"),
        }
        match classify_cdp_eval_remote_object(
            &json!({ "type": "object", "subtype": "null", "value": null }),
        ) {
            CdpEvalAction::Resolved(Ok(v)) => assert_eq!(v, Value::Null),
            other => panic!("unexpected action: {other:?}"),
        }
        match classify_cdp_eval_remote_object(
            &json!({ "type": "number", "unserializableValue": "-0" }),
        ) {
            CdpEvalAction::Resolved(Ok(v)) => assert_eq!(v, json!(0)),
            other => panic!("unexpected action: {other:?}"),
        }
        match classify_cdp_eval_remote_object(&json!({ "type": "boolean", "value": true })) {
            CdpEvalAction::Resolved(Ok(v)) => assert_eq!(v, json!(true)),
            other => panic!("unexpected action: {other:?}"),
        }
        match classify_cdp_eval_remote_object(&json!({ "type": "string", "value": "hi" })) {
            CdpEvalAction::Resolved(Ok(v)) => assert_eq!(v, json!("hi")),
            other => panic!("unexpected action: {other:?}"),
        }

        // Functions, symbols, bigint, non-finite numbers, and non-JSON object subtypes reject in 1 call
        for bad in [
            json!({ "type": "function", "description": "() => {}", "objectId": "f-1" }),
            json!({ "type": "function", "description": "class Foo {}", "objectId": "f-2" }),
            json!({ "type": "bigint", "unserializableValue": "42n" }),
            json!({ "type": "symbol", "description": "Symbol(s)", "objectId": "s-1" }),
            json!({ "type": "number", "unserializableValue": "NaN" }),
            json!({ "type": "number", "unserializableValue": "Infinity" }),
            json!({ "type": "number", "unserializableValue": "-Infinity" }),
            json!({ "type": "object", "subtype": "node", "objectId": "n-1" }),
            json!({ "type": "object", "subtype": "map", "objectId": "m-1" }),
            json!({ "type": "object", "subtype": "set", "objectId": "st-1" }),
            json!({ "type": "object", "subtype": "date", "objectId": "d-1" }),
            json!({ "type": "object", "subtype": "regexp", "objectId": "r-1" }),
            json!({ "type": "object", "subtype": "error", "objectId": "e-1" }),
        ] {
            match classify_cdp_eval_remote_object(&bad) {
                CdpEvalAction::Resolved(Err(MimikyuError::EvalFailed(_))) => {}
                other => panic!("expected EvalFailed for {bad}, got {other:?}"),
            }
        }

        // Plain objects & arrays trigger deep JSON validator
        for obj in [
            json!({ "type": "object", "description": "Object", "objectId": "o-1" }),
            json!({ "type": "object", "subtype": "array", "objectId": "a-1" }),
        ] {
            match classify_cdp_eval_remote_object(&obj) {
                CdpEvalAction::ValidateObject { object_id } => assert!(!object_id.is_empty()),
                other => panic!("expected ValidateObject for {obj}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unwrap_document_shell_nodes_strips_html_and_body_only() {
        let mk = |id: &str,
                  pid: Option<&str>,
                  cids: Vec<&str>,
                  role: &str,
                  bid: Option<i64>,
                  ignored: bool| {
            AxNode {
                node_id: id.to_string(),
                parent_id: pid.map(ToString::to_string),
                child_ids: cids.into_iter().map(ToString::to_string).collect(),
                role: role.to_string(),
                name: None,
                value: None,
                description: None,
                backend_node_id: bid,
                frame_id: Some("f1".to_string()),
                ignored,
                focusable: false,
                focused: false,
                disabled: false,
                checked: None,
                expanded: None,
                selected: None,
                required: false,
                level: None,
                bounds: None,
            }
        };

        let raw = vec![
            mk("1", None, vec!["2"], "RootWebArea", Some(1), false),
            mk("2", Some("1"), vec!["3"], "none", Some(2), true), // <html>
            mk("3", Some("2"), vec!["4", "5"], "generic", Some(3), false), // <body>
            mk("4", Some("3"), vec![], "button", Some(4), false), // <button>
            mk("5", Some("3"), vec!["6"], "generic", Some(5), false), // user <div>
            mk("6", Some("5"), vec![], "button", Some(6), false), // nested <button>
        ];
        let mut shell_bids = HashSet::new();
        shell_bids.insert((Some("f1".to_string()), 2));
        shell_bids.insert((Some("f1".to_string()), 3));

        let unwrapped = unwrap_document_shell_nodes(raw, &shell_bids);
        let ids: Vec<&str> = unwrapped.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(ids, vec!["1", "4", "5", "6"]);
        assert_eq!(unwrapped[0].child_ids, vec!["4", "5"]);
        assert_eq!(unwrapped[1].parent_id.as_deref(), Some("1"));
        assert_eq!(unwrapped[2].parent_id.as_deref(), Some("1"));
        // User <div> ("5") is NOT unwrapped
        assert_eq!(unwrapped[2].child_ids, vec!["6"]);
        assert_eq!(unwrapped[3].parent_id.as_deref(), Some("5"));
    }

    #[test]
    fn collect_same_proc_env_contexts_includes_nested_xo_frame_inside_oopif() {
        let state = dummy_session_state();
        state
            .iframe_session_targets
            .lock()
            .insert("child-sid-1".to_string(), "oopif-root-fid".to_string());
        {
            let mut ifs = state.iframe_sessions.lock();
            ifs.insert("oopif-root-fid".to_string(), "child-sid-1".to_string());
            ifs.insert("oopif-nested-xo-fid".to_string(), "child-sid-1".to_string());
        }
        {
            let mut dc = state.default_contexts.lock();
            dc.insert("main-fid".to_string(), 1);
            dc.insert("oopif-root-fid".to_string(), 2);
            dc.insert("oopif-nested-xo-fid".to_string(), 3);
            dc.insert("same-proc-xo-fid".to_string(), 4);
        }
        {
            let mut dcs = state.default_context_sessions.lock();
            dcs.insert("oopif-root-fid".to_string(), "child-sid-1".to_string());
            dcs.insert("oopif-nested-xo-fid".to_string(), "child-sid-1".to_string());
        }

        let mut got = state.collect_same_proc_env_contexts(Some("main-fid"));
        got.sort_by_key(|(cid, _)| *cid);
        assert_eq!(got, vec![(3, Some("child-sid-1".to_string())), (4, None),]);
    }

    #[test]
    fn cdp_session_state_concurrent_lock_safety() {
        let state = Arc::new(dummy_session_state());
        let (done_tx, done_rx) = std::sync::mpsc::channel::<usize>();
        let stop = Arc::new(AtomicBool::new(false));
        let iters = 3_000usize;

        let mut handles = Vec::new();
        for tid in 0..4usize {
            let st = Arc::clone(&state);
            let tx = done_tx.clone();
            let stop_flag = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                for i in 0..iters {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    match tid {
                        0 => {
                            let fid = format!("fid-{}", i % 8);
                            let sid = format!("child-sid-{}", i % 3);
                            st.iframe_session_targets
                                .lock()
                                .insert(sid.clone(), "oopif-root".to_string());
                            st.iframe_sessions.lock().insert(fid.clone(), sid.clone());
                            {
                                let mut dc = st.default_contexts.lock();
                                let mut dcs = st.default_context_sessions.lock();
                                dc.insert(fid.clone(), (i % 16) as i64);
                                dcs.insert(fid.clone(), sid.clone());
                            }
                            st.isolated_worlds.lock().insert(fid, (i % 16) as i64);
                            st.clear_execution_contexts(&sid, true);
                            st.clear_execution_contexts("main-sid", false);
                        }
                        1 => {
                            let fid = format!("fid-{}", i % 8);
                            let sid = format!("child-sid-{}", i % 3);
                            let cid = (i % 16) as i64;
                            st.isolated_worlds.lock().insert(fid.clone(), cid);
                            {
                                let mut dc = st.default_contexts.lock();
                                let mut dcs = st.default_context_sessions.lock();
                                dc.insert(fid.clone(), cid);
                                if i % 2 == 0 {
                                    dcs.insert(fid, sid.clone());
                                }
                            }
                            st.destroy_execution_context(&sid, true, cid);
                            st.destroy_execution_context("main-sid", false, cid);
                        }
                        2 => {
                            let sid = format!("child-sid-{}", i % 3);
                            let mut sids = HashSet::new();
                            sids.insert(sid.clone());
                            let mut tids = HashSet::new();
                            tids.insert("oopif-root".to_string());
                            let _ = st.prune_iframe_context_maps(&sids, tids);
                            let _targets = st.iframe_session_targets.lock();
                            let _sessions = st.iframe_sessions.lock();
                        }
                        _ => {
                            let _ = st.collect_same_proc_env_contexts(Some("main-fid"));
                            st.invalidate_frame_contexts(None, Some("main-fid"), None, None);
                            st.invalidate_frame_contexts(
                                Some("fid-0"),
                                Some("fid-0"),
                                Some(0),
                                Some(0),
                            );
                            st.isolated_worlds.lock().clear();
                        }
                    }
                }
                let _ = tx.send(tid);
            }));
        }
        drop(done_tx);

        for _ in 0..4 {
            if done_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_err()
            {
                stop.store(true, Ordering::Relaxed);
                panic!("deadlock detected across concurrent CdpSessionState lock operations");
            }
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
    }

    #[test]
    fn transient_context_error_classification_covers_all_six_patterns_and_rejects_non_transient() {
        // Tier A — Pre-execution lookup failures (expression never ran)
        let tier_a_cases = [
            MimikyuError::Protocol {
                code: -32000,
                message: "Cannot find context with specified id".to_string(),
            },
            MimikyuError::Protocol {
                code: -32000,
                message: "Cannot find default execution context".to_string(),
            },
            MimikyuError::Protocol {
                code: -32000,
                message: "No frame for given id found".to_string(),
            },
            MimikyuError::Protocol {
                code: -32602,
                message: "No frame for given id found".to_string(),
            },
        ];
        for err in &tier_a_cases {
            assert_eq!(
                classify_transient_context_error(err),
                Some(TransientContextErrorKind::PreExecution),
                "expected PreExecution for {err:?}"
            );
            assert!(
                is_transient_context_error(err),
                "expected is_transient_context_error=true for {err:?}"
            );
        }

        // Tier B — Mid/post-execution invalidations (expression may have already run)
        let tier_b_cases = [
            MimikyuError::Protocol {
                code: -32000,
                message: "Execution context was destroyed.".to_string(),
            },
            MimikyuError::Protocol {
                code: -32000,
                message: "Could not find object with given id".to_string(),
            },
            MimikyuError::Protocol {
                code: -32000,
                message: "Inspected target navigated or closed".to_string(),
            },
        ];
        for err in &tier_b_cases {
            assert_eq!(
                classify_transient_context_error(err),
                Some(TransientContextErrorKind::MidOrPostExecution),
                "expected MidOrPostExecution for {err:?}"
            );
            assert!(
                is_transient_context_error(err),
                "expected is_transient_context_error=true for {err:?}"
            );
        }

        // Non-transient errors MUST NOT be retried as transient context races
        let non_transient = [
            MimikyuError::EvalFailed("SyntaxError: Unexpected token '}'".to_string()),
            MimikyuError::EvalFailed("ReferenceError: x is not defined".to_string()),
            MimikyuError::Timeout {
                timeout_ms: 30_000,
                operation: "CDP Runtime.evaluate".to_string(),
            },
            MimikyuError::Disposed,
            MimikyuError::Transport("CDP WebSocket connection closed".to_string()),
            MimikyuError::ElementNotFound("selector '#missing' matched no element".to_string()),
            MimikyuError::MainWorldDisabled,
            MimikyuError::Protocol {
                code: -32000,
                message: "No node with given id found".to_string(),
            },
            MimikyuError::Protocol {
                code: -32000,
                message: "Could not find node with given id".to_string(),
            },
            MimikyuError::Protocol {
                code: -32601,
                message: "'Runtime.bogus' wasn't found".to_string(),
            },
            MimikyuError::Protocol {
                code: -32000,
                message: "DOM Error while querying".to_string(),
            },
            MimikyuError::Protocol {
                code: -32001,
                message: "Cannot find context with specified id".to_string(),
            },
        ];
        for err in &non_transient {
            assert_eq!(
                classify_transient_context_error(err),
                None,
                "expected None for non-transient {err:?}"
            );
            assert!(
                !is_transient_context_error(err),
                "expected is_transient_context_error=false for {err:?}"
            );
        }

        // Stale DOM node helper recognizes missing nodeId/backendNodeId distinctly
        assert!(is_stale_dom_node_error(&MimikyuError::Protocol {
            code: -32000,
            message: "No node with given id found".to_string(),
        }));
        assert!(is_stale_dom_node_error(&MimikyuError::Protocol {
            code: -32000,
            message: "Could not find node with given id".to_string(),
        }));
        assert!(is_stale_dom_node_error(&MimikyuError::Protocol {
            code: -32000,
            message: "Node with given id does not belong to the document".to_string(),
        }));
        assert!(!is_transient_context_error(&MimikyuError::Protocol {
            code: -32000,
            message: "Node with given id does not belong to the document".to_string(),
        }));
        assert!(!is_stale_dom_node_error(&MimikyuError::Protocol {
            code: -32000,
            message: "Cannot find context with specified id".to_string(),
        }));
    }

    #[test]
    fn cdp_exhausted_and_mid_eval_errors_are_clean_descriptive_retryable_protocol_errors() {
        let raw_stale = MimikyuError::Protocol {
            code: -32000,
            message: "Cannot find context with specified id".to_string(),
        };
        let exhausted = format_exhausted_context_error(
            "isolated-world evaluation",
            CONTEXT_MAX_ATTEMPTS,
            raw_stale,
        );
        match &exhausted {
            MimikyuError::Protocol { code, message } => {
                assert_eq!(*code, -32000);
                assert!(message.contains("Execution context was destroyed"));
                assert!(message.contains("exhausted 4 attempts"));
                assert!(message.contains("isolated-world evaluation"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
        assert!(is_transient_context_error(&exhausted));

        let raw_mid = MimikyuError::Protocol {
            code: -32000,
            message: "Execution context was destroyed.".to_string(),
        };
        let mid_err = format_mid_eval_context_destroyed_error("isolated-world evaluation", raw_mid);
        match &mid_err {
            MimikyuError::Protocol { code, message } => {
                assert_eq!(*code, -32000);
                assert!(message.contains("Execution context was destroyed"));
                assert!(message.contains("side-effect safety"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
        assert!(is_transient_context_error(&mid_err));
    }

    #[test]
    fn cdp_invalidate_frame_contexts_respects_frame_scope_and_cas() {
        let state = dummy_session_state();
        {
            let mut iw = state.isolated_worlds.lock();
            iw.insert("main-fid".to_string(), 10);
            iw.insert("child-fid".to_string(), 20);
        }
        {
            let mut dc = state.default_contexts.lock();
            dc.insert("main-fid".to_string(), 1);
            dc.insert("child-fid".to_string(), 2);
        }
        {
            let mut dcs = state.default_context_sessions.lock();
            dcs.insert("child-fid".to_string(), "child-sid".to_string());
        }

        // Invalidating a child frame's isolated world must preserve main_frame_id,
        // main frame caches, AND the child frame's live default_contexts entry!
        state.invalidate_frame_contexts(Some("child-fid"), Some("child-fid"), Some(20), None);
        assert_eq!(state.main_frame_id.lock().as_deref(), Some("main-fid"));
        assert_eq!(
            state.isolated_worlds.lock().get("main-fid").copied(),
            Some(10)
        );
        assert!(!state.isolated_worlds.lock().contains_key("child-fid"));
        assert_eq!(
            state.default_contexts.lock().get("child-fid").copied(),
            Some(2)
        );
        assert_eq!(
            state
                .default_context_sessions
                .lock()
                .get("child-fid")
                .map(String::as_str),
            Some("child-sid")
        );

        // Compare-and-remove on default_contexts: stale ctx 99 must NOT wipe newer ctx 2,
        // whereas matching stale ctx 2 removes it cleanly.
        state.invalidate_frame_contexts(Some("child-fid"), Some("child-fid"), None, Some(99));
        assert_eq!(
            state.default_contexts.lock().get("child-fid").copied(),
            Some(2)
        );
        state.invalidate_frame_contexts(Some("child-fid"), Some("child-fid"), None, Some(2));
        assert!(!state.default_contexts.lock().contains_key("child-fid"));
        assert!(
            !state
                .default_context_sessions
                .lock()
                .contains_key("child-fid")
        );

        // Re-populate child-fid, then invalidate main frame (frame_id == None):
        // main_frame_id and main-fid entries must be cleared; child-fid stays intact.
        state
            .isolated_worlds
            .lock()
            .insert("child-fid".to_string(), 21);
        state
            .default_contexts
            .lock()
            .insert("child-fid".to_string(), 3);
        state.invalidate_frame_contexts(None, Some("main-fid"), Some(10), None);
        assert!(state.main_frame_id.lock().is_none());
        assert!(!state.isolated_worlds.lock().contains_key("main-fid"));
        assert!(!state.default_contexts.lock().contains_key("main-fid"));
        assert_eq!(
            state.isolated_worlds.lock().get("child-fid").copied(),
            Some(21)
        );
        assert_eq!(
            state.default_contexts.lock().get("child-fid").copied(),
            Some(3)
        );
    }

    #[test]
    fn cdp_single_rpc_wrapper_guards_statements_and_distinguishes_compile_vs_runtime_syntax_errors()
    {
        // Expressions that safely fold into 1-RPC validation
        for expr in [
            "1 + 1",
            "({ a: 1, b: [2, 3] })",
            "(() => ({ ok: true }))()",
            "(function foo() {})",
            "(class Foo {})",
            "Promise.resolve({ answer: 42 })",
            "async () => 42",
            "document.title",
            "new DataView(new ArrayBuffer(4))",
        ] {
            assert!(
                can_wrap_cdp_eval_expr(expr),
                "expected expression to be wrappable: {expr}"
            );
            let wrapped = build_wrapped_cdp_eval_expr(expr);
            assert!(wrapped.contains("ArrayBuffer.isView"));
            assert!(wrapped.contains(expr));
        }

        // Statements / declarations / comments / empty / trailing semicolon skip wrapper
        for stmt in [
            "",
            "   ",
            "{ a: 1 }",
            "function foo() {}",
            "function* gen() {}",
            "async function foo() {}",
            "async function* gen() {}",
            "class Foo {}",
            "const x = 1; x + 2",
            "let x = 1; x + 2",
            "var x = 1; x + 2",
            "if (true) 1",
            "for (let i = 0; i < 1; i++) {}",
            "throw new Error('boom')",
            "document.getElementById('x').focus();",
            "// comment\n1 + 1",
            "/* comment */ { a: 1 }",
        ] {
            assert!(
                !can_wrap_cdp_eval_expr(stmt),
                "expected statement/declaration NOT to be wrapped: {stmt}"
            );
        }

        // Compile-time SyntaxError (no stack frames) vs runtime SyntaxError (has callFrames)
        let compile_err = json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": {
                    "type": "object",
                    "subtype": "error",
                    "className": "SyntaxError",
                    "description": "SyntaxError: Unexpected token ';'",
                    "objectId": "err-1"
                }
            }
        });
        assert!(is_compile_time_syntax_error(&compile_err));

        let runtime_syntax_err = json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "stackTrace": {
                    "callFrames": [{ "functionName": "parse", "lineNumber": 0, "columnNumber": 0 }]
                },
                "exception": {
                    "type": "object",
                    "subtype": "error",
                    "className": "SyntaxError",
                    "description": "SyntaxError: Expected property name",
                    "objectId": "err-2"
                }
            }
        });
        assert!(!is_compile_time_syntax_error(&runtime_syntax_err));

        let runtime_type_err = json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": {
                    "type": "object",
                    "subtype": "error",
                    "className": "TypeError",
                    "description": "TypeError: boom"
                }
            }
        });
        assert!(!is_compile_time_syntax_error(&runtime_type_err));
    }
}
