use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
    DriverCapabilities, DriverSession, EngineKind, EnvProfile, FrameInfo, KeyEvent, KeyEventType,
    LaunchOptions, MouseButton, MouseEvent, MouseEventType, NavInfo, NetworkEvent, OriginStorage,
    PageError, PermissionGrant, PermissionState, ProfileMode, ProxyConfig, RouteMock,
    ScreenshotFormat, ScreenshotOpts, SelectBy, SessionInfo, ShadowMatch, StorageState, TabInfo,
    WheelEvent, MOD_ALT, MOD_CONTROL, MOD_META, MOD_SHIFT,
};
use crate::env_profile::{
    build_firefox_shared_worker_network_clear_script, build_firefox_shared_worker_network_script,
    build_window_init_script, ProxyRelay,
};
use crate::error::{MimikyuError, Result};
use crate::network::{
    http_reason_phrase, merge_headers_case_insensitive, BlockPolicy, InterceptDecision,
    InterceptRequest, NetworkInterceptor, ResourceType, RouteRule, TelemetryConfig, TelemetryStore,
};
use crate::process::ProcessGuard;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const ISOLATED_SANDBOX: &str = "mimikyu_isolated";
static NEXT_NET_CH_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_BIDI_STATE_ID: AtomicU64 = AtomicU64::new(1);

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
        let pool_download_dir = opts
            .download_dir
            .clone()
            .unwrap_or_else(|| profile_dir.join("downloads"));
        let pid = process.pid();

        let conn = BidiConnection::connect(&ws_session_url, process, root_relay, pool_download_dir).await?;

        let new_sess = match conn
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
            .await
        {
            Ok(s) => s,
            Err(e) => {
                conn.shutdown_all().await;
                return Err(e);
            }
        };

        let bidi_session_id = new_sess["sessionId"]
            .as_str()
            .unwrap_or("bidi-session")
            .to_string();

        if let Err(e) = conn
            .send(
                "session.subscribe",
                json!({
                    "events": [
                        "log.entryAdded",
                        "network.beforeRequestSent",
                        "network.responseCompleted",
                        "browsingContext.domContentLoaded",
                        "browsingContext.load",
                        "browsingContext.contextCreated",
                        "browsingContext.contextDestroyed",
                        "browsingContext.downloadWillBegin"
                    ]
                }),
            )
            .await
        {
            conn.shutdown_all().await;
            return Err(e);
        }
        let _ = conn
            .send(
                "session.subscribe",
                json!({ "events": ["network.fetchError", "network.responseStarted"] }),
            )
            .await;
        // Best-effort subscription for W3C BiDi download completion events (Firefox 145+);
        // kept separate so older Firefox builds that do not support `downloadEnd` still
        // receive `network.fetchError` and `network.responseStarted`.
        if conn
            .send(
                "session.subscribe",
                json!({ "events": ["browsingContext.downloadEnd"] }),
            )
            .await
            .is_ok()
        {
            conn.download_end_supported.store(true, Ordering::SeqCst);
        }

        let (user_context, owns_user_context, target_id) = match opts.profile_mode {
            ProfileMode::VirtualContext => {
                let uc = match create_user_context(&conn, opts.proxy.as_ref()).await {
                    Ok(u) => u,
                    Err(e) => {
                        conn.shutdown_all().await;
                        return Err(e);
                    }
                };
                conn.isolated_contexts.write().insert(uc.clone());
                // Must use type: "window" (type: "tab" with default background:false hangs in headless firefox-1497).
                let created = match conn
                    .send(
                        "browsingContext.create",
                        json!({
                            "type": "window",
                            "userContext": uc,
                        }),
                    )
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        conn.shutdown_all().await;
                        return Err(e);
                    }
                };
                let Some(ctx_id) = created["context"].as_str().map(ToString::to_string) else {
                    conn.shutdown_all().await;
                    return Err(MimikyuError::Protocol {
                        code: -1,
                        message: "browsingContext.create missing context".to_string(),
                    });
                };
                (Some(uc), true, ctx_id)
            }
            _ => {
                let tree = match conn.send("browsingContext.getTree", json!({})).await {
                    Ok(t) => t,
                    Err(e) => {
                        conn.shutdown_all().await;
                        return Err(e);
                    }
                };
                let existing = tree["contexts"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|c| c["context"].as_str())
                    .map(ToString::to_string);
                let ctx_id = if let Some(c) = existing {
                    c
                } else {
                    let created = match conn
                        .send("browsingContext.create", json!({ "type": "window" }))
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            conn.shutdown_all().await;
                            return Err(e);
                        }
                    };
                    let Some(c) = created["context"].as_str().map(ToString::to_string) else {
                        conn.shutdown_all().await;
                        return Err(MimikyuError::Protocol {
                            code: -1,
                            message: "browsingContext.create missing context".to_string(),
                        });
                    };
                    c
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
            opts.download_dir,
        );

        if let Err(err) = session
            .apply_initial_options(
                opts.env_profile,
                Vec::new(),
                opts.block_images,
                opts.block_media,
                opts.block_fonts,
                opts.instant_animations,
            )
            .await
        {
            let _ = session.dispose().await;
            return Err(err);
        }

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
        supports_closed_shadow_dom: false,
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
    let parsed = crate::driver::parse_proxy_url(&proxy.server)?;
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
    context_openers: RwLock<HashMap<String, String>>,
    context_user_contexts: RwLock<HashMap<String, String>>,
    user_contexts: RwLock<HashMap<String, Arc<BidiContextState>>>,
    context_requests: RwLock<HashMap<String, HashSet<String>>>,
    request_stores: RwLock<HashMap<String, Weak<TelemetryStore>>>,
    isolated_contexts: RwLock<HashSet<String>>,
    active_targets: RwLock<HashMap<String, String>>,
    global_intercept_id: AsyncMutex<Option<String>>,
    intercept_owners: Mutex<HashSet<u64>>,
    pool_download_dir: PathBuf,
    active_pool_downloads: Arc<Mutex<HashSet<String>>>,
    recent_content_disposition: Mutex<HashMap<(String, String), String>>,
    download_transfers_by_req_id: Mutex<HashMap<String, DownloadTransferState>>,
    pending_download_req_ids: Mutex<HashMap<(String, String), VecDeque<PendingDownloadReq>>>,
    download_ends_by_nav: Mutex<HashMap<String, (DownloadEndStatus, std::time::Instant)>>,
    download_end_supported: AtomicBool,
    process: AsyncMutex<Option<ProcessGuard>>,
    _root_proxy_relay: Mutex<Option<ProxyRelay>>,
    closed: AtomicBool,
    io_tasks: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadEndStatus {
    Complete,
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadTransferStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Debug)]
struct DownloadTransferState {
    status: DownloadTransferStatus,
    content_length: Option<u64>,
    body_non_empty: bool,
    is_attachment: bool,
    cd_filename: Option<String>,
    response_seen: bool,
    cd_consumed: bool,
    updated_at: std::time::Instant,
}

fn is_content_disposition_attachment(headers: &BTreeMap<String, String>) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-disposition")
            && v.split(';')
                .next()
                .is_some_and(|disp| disp.trim().eq_ignore_ascii_case("attachment"))
    })
}

#[derive(Clone, Debug)]
struct PendingDownloadReq {
    req_id: String,
    enqueued_at: std::time::Instant,
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
        let mut last_cur = cid.to_string();
        {
            let contexts = self.contexts.read();
            if let Some(state) = contexts.get(cid) {
                return Some(Arc::clone(state));
            }
            let parents = self.context_parents.read();
            let openers = self.context_openers.read();
            let mut visited: HashSet<&str> = HashSet::new();
            visited.insert(cid);
            let mut cur = cid;
            for _ in 0..64 {
                let next = parents
                    .get(cur)
                    .map(String::as_str)
                    .or_else(|| openers.get(cur).map(String::as_str));
                let Some(next_cid) = next else {
                    break;
                };
                if let Some(state) = contexts.get(next_cid) {
                    return Some(Arc::clone(state));
                }
                if !visited.insert(next_cid) {
                    break;
                }
                last_cur = next_cid.to_string();
                cur = next_cid;
            }
        }
        let uc = {
            let ctx_ucs = self.context_user_contexts.read();
            ctx_ucs
                .get(cid)
                .or_else(|| ctx_ucs.get(&last_cur))
                .cloned()
        }?;
        self.user_contexts.read().get(&uc).cloned()
    }

    async fn connect(
        ws_url: &str,
        process: ProcessGuard,
        root_proxy_relay: Option<ProxyRelay>,
        pool_download_dir: PathBuf,
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
            context_openers: RwLock::new(HashMap::new()),
            context_user_contexts: RwLock::new(HashMap::new()),
            user_contexts: RwLock::new(HashMap::new()),
            context_requests: RwLock::new(HashMap::new()),
            request_stores: RwLock::new(HashMap::new()),
            isolated_contexts: RwLock::new(HashSet::new()),
            active_targets: RwLock::new(HashMap::new()),
            global_intercept_id: AsyncMutex::new(None),
            intercept_owners: Mutex::new(HashSet::new()),
            pool_download_dir,
            active_pool_downloads: Arc::new(Mutex::new(HashSet::new())),
            recent_content_disposition: Mutex::new(HashMap::new()),
            download_transfers_by_req_id: Mutex::new(HashMap::new()),
            pending_download_req_ids: Mutex::new(HashMap::new()),
            download_ends_by_nav: Mutex::new(HashMap::new()),
            download_end_supported: AtomicBool::new(false),
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

    fn track_request(&self, ctx_id: Option<&str>, req_id: &str, telem: &Arc<TelemetryStore>) {
        if let Some(cid) = ctx_id {
            self.context_requests
                .write()
                .entry(cid.to_string())
                .or_default()
                .insert(req_id.to_string());
        }
        let mut stores = self.request_stores.write();
        if stores.len() > 8192 {
            stores.retain(|_, w| w.strong_count() > 0);
        }
        stores.insert(req_id.to_string(), Arc::downgrade(telem));
    }

    fn untrack_request(&self, ctx_id: Option<&str>, req_id: &str) -> Option<Arc<TelemetryStore>> {
        if let Some(cid) = ctx_id {
            let mut ctx_reqs = self.context_requests.write();
            if let Some(set) = ctx_reqs.get_mut(cid) {
                set.remove(req_id);
                if set.is_empty() {
                    ctx_reqs.remove(cid);
                }
            }
        } else {
            let mut ctx_reqs = self.context_requests.write();
            ctx_reqs.retain(|_, set| {
                set.remove(req_id);
                !set.is_empty()
            });
        }
        self.request_stores
            .write()
            .remove(req_id)
            .and_then(|w| w.upgrade())
    }

    fn cleanup_destroyed_context(self: &Arc<Self>, root_cid: &str) {
        let owner_state = self.resolve_context_state(root_cid);
        let mut destroyed: HashSet<String> = HashSet::new();
        destroyed.insert(root_cid.to_string());
        {
            let mut parents = self.context_parents.write();
            loop {
                let mut added = false;
                for (child, parent) in parents.iter() {
                    if destroyed.contains(parent) && !destroyed.contains(child) {
                        destroyed.insert(child.clone());
                        added = true;
                    }
                }
                if !added {
                    break;
                }
            }
            parents.retain(|child, parent| {
                !destroyed.contains(child) && !destroyed.contains(parent)
            });
        }
        {
            self.context_openers
                .write()
                .retain(|cid, _| !destroyed.contains(cid));
            self.context_user_contexts
                .write()
                .retain(|cid, _| !destroyed.contains(cid));
            self.pending_download_req_ids
                .lock()
                .retain(|(cid, _), _| !destroyed.contains(cid));
        }

        if let Some(state) = owner_state {
            state.inline_frame_owners.lock().retain(|child, owner| {
                !destroyed.contains(child) && !destroyed.contains(owner)
            });
            let mut removed_popups = Vec::new();
            {
                let mut pops = state.popup_contexts.lock();
                for cid in &destroyed {
                    if pops.remove(cid) {
                        removed_popups.push(cid.clone());
                    }
                }
            }
            if !removed_popups.is_empty() && tokio::runtime::Handle::try_current().is_ok() {
                let conn_clone = Arc::clone(self);
                let state_clone = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut scripts_to_remove = Vec::new();
                    {
                        let mut ps = state_clone.popup_preload_scripts.lock().await;
                        for cid in &removed_popups {
                            if let Some(ids) = ps.remove(cid) {
                                scripts_to_remove.extend(ids);
                            }
                        }
                    }
                    {
                        let mut pas = state_clone.popup_anim_scripts.lock().await;
                        for cid in &removed_popups {
                            if let Some(id) = pas.remove(cid) {
                                scripts_to_remove.push(id);
                            }
                        }
                    }
                    {
                        let mut pns = state_clone.popup_net_worker_scripts.lock().await;
                        for cid in &removed_popups {
                            if let Some((mid, iid)) = pns.remove(cid) {
                                if !mid.is_empty() {
                                    scripts_to_remove.push(mid);
                                }
                                if !iid.is_empty() {
                                    scripts_to_remove.push(iid);
                                }
                            }
                        }
                    }
                    for sid in scripts_to_remove {
                        let _ = conn_clone
                            .send("script.removePreloadScript", json!({ "script": sid }))
                            .await;
                    }
                });
            }
        }

        let mut orphaned_reqs: Vec<String> = Vec::new();
        {
            let mut ctx_reqs = self.context_requests.write();
            for cid in &destroyed {
                if let Some(reqs) = ctx_reqs.remove(cid) {
                    orphaned_reqs.extend(reqs);
                }
            }
        }

        if orphaned_reqs.is_empty() {
            return;
        }

        let mut to_fail: Vec<(String, Arc<TelemetryStore>)> = Vec::new();
        {
            let mut stores = self.request_stores.write();
            for req_id in orphaned_reqs {
                if let Some(telem) = stores.remove(&req_id).and_then(|w| w.upgrade()) {
                    to_fail.push((req_id, telem));
                }
            }
        }

        for (req_id, telem) in to_fail {
            telem.on_request_failed(&req_id, Some("browsingContext destroyed".to_string()));
        }
    }

    fn handle_incoming(self: &Arc<Self>, raw: &str) {
        let val = match crate::driver::parse_protocol_json(raw) {
            Ok(v) => v,
            Err((id_opt, err)) => {
                warn!("failed to parse BiDi JSON frame: {err}");
                if let Some(id) = id_opt
                    && let Some(tx) = self.pending.lock().remove(&id)
                {
                    let _ = tx.send(Err(MimikyuError::Protocol {
                        code: -1,
                        message: format!("malformed BiDi JSON frame: {err}"),
                    }));
                }
                return;
            }
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
                    if let Some(cid) = params.get("context").and_then(|v| v.as_str()) {
                        let parent_opt = params.get("parent").and_then(|v| v.as_str());
                        let opener_opt = params.get("originalOpener").and_then(|v| v.as_str());
                        let uc_opt = params.get("userContext").and_then(|v| v.as_str());
                        if let Some(parent) = parent_opt {
                            self.context_parents
                                .write()
                                .insert(cid.to_string(), parent.to_string());
                        }
                        if let Some(opener) = opener_opt {
                            self.context_openers
                                .write()
                                .insert(cid.to_string(), opener.to_string());
                        }
                        if let Some(uc) = uc_opt {
                            self.context_user_contexts
                                .write()
                                .insert(cid.to_string(), uc.to_string());
                        }
                        let already_session = self.contexts.read().contains_key(cid);
                        let owner_state_opt = if parent_opt.is_none() && !already_session {
                            opener_opt.and_then(|opener| self.resolve_context_state(opener))
                        } else {
                            None
                        };
                        if let Some(owner_state) = owner_state_opt
                            && !owner_state.disposed.load(Ordering::SeqCst)
                        {
                            owner_state.popup_contexts.lock().insert(cid.to_string());
                            if tokio::runtime::Handle::try_current().is_ok() {
                                let conn_clone = Arc::clone(self);
                                let state_clone = Arc::clone(&owner_state);
                                let popup_cid = cid.to_string();
                                tokio::spawn(async move {
                                    state_clone.setup_popup_context(&conn_clone, &popup_cid).await;
                                });
                            }
                        }
                    }
                    return;
                }
                if method == "browsingContext.contextDestroyed" {
                    if let Some(cid) = params.get("context").and_then(|v| v.as_str()) {
                        self.cleanup_destroyed_context(cid);
                    }
                    return;
                }

                if method == "browsingContext.downloadWillBegin" {
                    if let (Some(cid), Some(suggested)) = (
                        params.get("context").and_then(|v| v.as_str()),
                        params.get("suggestedFilename").and_then(|v| v.as_str()),
                    ) && let Some(state) = self.resolve_context_state(cid)
                        && let Some(ref dest_dir) = state.download_dir
                        && dest_dir != &self.pool_download_dir
                    {
                        let url_str = params
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let nav_id = params
                            .get("navigation")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(ToString::to_string);
                        self.spawn_isolated_download_relocation(
                            cid,
                            suggested,
                            &url_str,
                            nav_id,
                            dest_dir.clone(),
                        );
                    }
                    return;
                }

                if method == "browsingContext.downloadEnd" {
                    if let Some(nav_id) = params
                        .get("navigation")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        let status = match params.get("status").and_then(|v| v.as_str()) {
                            Some("complete") => Some(DownloadEndStatus::Complete),
                            Some("canceled") => Some(DownloadEndStatus::Canceled),
                            _ => None,
                        };
                        if let Some(st) = status {
                            let now = std::time::Instant::now();
                            let mut ends = self.download_ends_by_nav.lock();
                            if ends.len() >= 512 {
                                ends.retain(|_, (_, ts)| {
                                    now.duration_since(*ts) < Duration::from_secs(120)
                                });
                            }
                            ends.insert(nav_id.to_string(), (st, now));
                        }
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

                self.record_download_network_event(method, ctx_id, &params);
                if method == "browsingContext.load"
                    && let Some(cid) = ctx_id
                    && let Some(url_str) = params.get("url").and_then(|v| v.as_str())
                {
                    self.prune_loaded_non_download_candidates(cid, url_str);
                }

                if let Some(state) = ctx_id.and_then(|cid| self.resolve_context_state(cid)) {
                    if method == "network.beforeRequestSent"
                        && let Some(req_id) = params["request"]["request"].as_str()
                    {
                        self.track_request(ctx_id, req_id, &state.telemetry);
                    } else if matches!(method, "network.responseCompleted" | "network.fetchError")
                        && let Some(req_id) = params["request"]["request"].as_str()
                    {
                        let _ = self.untrack_request(ctx_id, req_id);
                    }
                    if matches!(method, "network.responseStarted" | "network.responseCompleted")
                        && let Some(cid) = ctx_id
                        && let Some(url_str) = params["request"]["url"].as_str()
                    {
                        let hdrs = bidi_headers_to_btree(params["response"].get("headers"));
                        let key = (cid.to_string(), normalize_download_url(url_str));
                        let req_consumed = params["request"]["request"]
                            .as_str()
                            .and_then(|rid| {
                                self.download_transfers_by_req_id
                                    .lock()
                                    .get(rid)
                                    .map(|st| st.cd_consumed)
                            })
                            .unwrap_or(false);
                        if let Some(cd_name) = extract_content_disposition_filename(&hdrs) {
                            if !req_consumed {
                                let mut map = self.recent_content_disposition.lock();
                                if map.len() >= 128 {
                                    map.clear();
                                }
                                map.insert(key, cd_name);
                            }
                        } else {
                            self.recent_content_disposition.lock().remove(&key);
                        }
                    }
                    state.handle_event(self, method, params);
                } else if method == "network.responseStarted"
                    && let Some(req_id) = params["request"]["request"].as_str()
                {
                    let telem_opt = self
                        .request_stores
                        .read()
                        .get(req_id)
                        .and_then(|w| w.upgrade());
                    if let Some(telem) = telem_opt {
                        let resp = &params["response"];
                        let status = resp.get("status").and_then(|v| v.as_u64()).map(|s| s as u16);
                        let mime_type = resp
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string);
                        let response_headers = bidi_headers_to_btree(resp.get("headers"));
                        if is_bidi_sse_response(mime_type.as_deref(), &response_headers) {
                            telem.set_pending_resource_type(req_id, "EventSource");
                        }
                        telem.on_response_received(req_id, status, mime_type, response_headers);
                    }
                } else if method == "network.responseCompleted"
                    && let Some(req_id) = params["request"]["request"].as_str()
                {
                    if let Some(telem) = self.untrack_request(ctx_id, req_id) {
                        let resp = &params["response"];
                        let status = resp.get("status").and_then(|v| v.as_u64()).map(|s| s as u16);
                        let mime_type = resp
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string);
                        let response_headers = bidi_headers_to_btree(resp.get("headers"));
                        telem.on_response_received(req_id, status, mime_type, response_headers);
                        telem.on_request_finished(req_id);
                    }
                } else if method == "network.fetchError"
                    && let Some(req_id) = params["request"]["request"].as_str()
                {
                    if let Some(telem) = self.untrack_request(ctx_id, req_id) {
                        let error_text = params
                            .get("errorText")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string);
                        telem.on_request_failed(req_id, error_text);
                    }
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

    fn prune_loaded_non_download_candidates(&self, cid: &str, url: &str) {
        let key = (cid.to_string(), normalize_download_url(url));
        let candidate_ids: Vec<String> = {
            let pending = self.pending_download_req_ids.lock();
            let Some(q) = pending.get(&key) else {
                return;
            };
            q.iter().map(|p| p.req_id.clone()).collect()
        };
        let to_prune: HashSet<String> = {
            let transfers = self.download_transfers_by_req_id.lock();
            candidate_ids
                .into_iter()
                .filter(|rid| {
                    transfers.get(rid).is_some_and(|st| {
                        st.status == DownloadTransferStatus::Completed && !st.is_attachment
                    })
                })
                .collect()
        };
        if to_prune.is_empty() {
            return;
        }
        let mut pending = self.pending_download_req_ids.lock();
        if let Some(q) = pending.get_mut(&key) {
            q.retain(|p| !to_prune.contains(&p.req_id));
            if q.is_empty() {
                pending.remove(&key);
            }
        }
    }

    fn record_download_network_event(&self, method: &str, ctx_id: Option<&str>, params: &Value) {
        if !matches!(
            method,
            "network.beforeRequestSent"
                | "network.responseStarted"
                | "network.responseCompleted"
                | "network.fetchError"
        ) {
            return;
        }
        let Some(rid) = params["request"]["request"].as_str() else {
            return;
        };
        let url_opt = params["request"]["url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(normalize_download_url);
        let now = std::time::Instant::now();

        match method {
            "network.beforeRequestSent" => {
                let destination = params["request"]["destination"].as_str().unwrap_or("");
                let initiator_type = params
                    .get("initiator")
                    .and_then(|i| i.get("type"))
                    .and_then(|v| v.as_str());
                let is_navigation = params
                    .get("navigation")
                    .and_then(|v| v.as_str())
                    .is_some();
                let is_fetch_or_xhr = initiator_type.is_some_and(|t| {
                    matches!(t.to_ascii_lowercase().as_str(), "fetch" | "xmlhttprequest")
                });
                let is_subresource_dest = !destination.is_empty()
                    && !matches!(
                        destination.to_ascii_lowercase().as_str(),
                        "document" | "iframe" | "frame" | "embed" | "object"
                    );
                if !is_navigation && (is_fetch_or_xhr || is_subresource_dest) {
                    return;
                }

                if let (Some(cid), Some(url)) = (ctx_id, url_opt) {
                    let key = (cid.to_string(), url);
                    let mut pending = self.pending_download_req_ids.lock();
                    if pending.len() >= 512 {
                        pending.retain(|_, q| {
                            while let Some(front) = q.front() {
                                if now.duration_since(front.enqueued_at)
                                    >= Duration::from_secs(120)
                                {
                                    q.pop_front();
                                } else {
                                    break;
                                }
                            }
                            !q.is_empty()
                        });
                        if pending.len() >= 512 {
                            pending.clear();
                        }
                    }
                    let q = pending.entry(key).or_default();
                    while let Some(front) = q.front() {
                        if now.duration_since(front.enqueued_at) >= Duration::from_secs(120) {
                            q.pop_front();
                        } else {
                            break;
                        }
                    }
                    if q.len() >= 64 {
                        q.pop_front();
                    }
                    if !q.iter().any(|p| p.req_id == rid) {
                        q.push_back(PendingDownloadReq {
                            req_id: rid.to_string(),
                            enqueued_at: now,
                        });
                    }
                }
                let mut transfers = self.download_transfers_by_req_id.lock();
                if transfers.len() >= 1024 {
                    transfers
                        .retain(|_, v| now.duration_since(v.updated_at) < Duration::from_secs(120));
                }
                transfers.insert(
                    rid.to_string(),
                    DownloadTransferState {
                        status: DownloadTransferStatus::InProgress,
                        content_length: None,
                        body_non_empty: false,
                        is_attachment: false,
                        cd_filename: None,
                        response_seen: false,
                        cd_consumed: false,
                        updated_at: now,
                    },
                );
            }
            "network.responseStarted" => {
                let hdrs = bidi_headers_to_btree(params["response"].get("headers"));
                let cl = extract_content_length(&hdrs);
                let non_empty = extract_response_body_non_empty(&params["response"], cl);
                let cd_fn = extract_content_disposition_filename(&hdrs);
                let is_att = is_content_disposition_attachment(&hdrs) || cd_fn.is_some();
                let mut transfers = self.download_transfers_by_req_id.lock();
                if let Some(entry) = transfers.get_mut(rid) {
                    entry.status = DownloadTransferStatus::InProgress;
                    entry.content_length = cl;
                    entry.body_non_empty |= non_empty;
                    entry.is_attachment |= is_att;
                    if cd_fn.is_some() {
                        entry.cd_filename = cd_fn;
                    }
                    entry.response_seen = true;
                    entry.updated_at = now;
                }
            }
            "network.responseCompleted" => {
                let hdrs = bidi_headers_to_btree(params["response"].get("headers"));
                let cl = extract_content_length(&hdrs);
                let non_empty = extract_response_body_non_empty(&params["response"], cl);
                let cd_fn = extract_content_disposition_filename(&hdrs);
                let is_att = is_content_disposition_attachment(&hdrs) || cd_fn.is_some();
                let mut transfers = self.download_transfers_by_req_id.lock();
                if let Some(entry) = transfers.get_mut(rid) {
                    entry.status = DownloadTransferStatus::Completed;
                    entry.content_length = cl.or(entry.content_length);
                    entry.body_non_empty |= non_empty;
                    entry.is_attachment |= is_att;
                    if cd_fn.is_some() {
                        entry.cd_filename = cd_fn;
                    }
                    entry.response_seen = true;
                    entry.updated_at = now;
                }
            }
            "network.fetchError" => {
                {
                    let mut pending = self.pending_download_req_ids.lock();
                    for q in pending.values_mut() {
                        q.retain(|p| p.req_id != rid);
                    }
                    pending.retain(|_, q| !q.is_empty());
                }
                let mut transfers = self.download_transfers_by_req_id.lock();
                if let Some(entry) = transfers.get_mut(rid) {
                    entry.status = DownloadTransferStatus::Failed;
                    entry.updated_at = now;
                }
            }
            _ => {}
        }
    }

    fn spawn_isolated_download_relocation(
        self: &Arc<Self>,
        cid: &str,
        suggested: &str,
        url: &str,
        nav_id: Option<String>,
        dest_dir: PathBuf,
    ) {
        let safe_suggested = std::path::Path::new(suggested)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(suggested)
            .to_string();
        if safe_suggested.is_empty() {
            return;
        }

        let norm_url = normalize_download_url(url);
        let key = (cid.to_string(), norm_url.clone());
        let bound_req_id: Option<String> = loop {
            let now = std::time::Instant::now();
            let cand_req_id = {
                let mut pending = self.pending_download_req_ids.lock();
                let mut popped = None;
                if let Some(q) = pending.get_mut(&key) {
                    while let Some(front) = q.front() {
                        if now.duration_since(front.enqueued_at) >= Duration::from_secs(120) {
                            q.pop_front();
                        } else {
                            popped = q.pop_front().map(|p| p.req_id);
                            break;
                        }
                    }
                    if q.is_empty() {
                        pending.remove(&key);
                    }
                }
                popped
            };
            let Some(rid) = cand_req_id else {
                break None;
            };
            let failed = self
                .download_transfers_by_req_id
                .lock()
                .get(&rid)
                .is_some_and(|st| st.status == DownloadTransferStatus::Failed);
            if failed {
                continue;
            }
            break Some(rid);
        };

        let fallback_cd = self
            .recent_content_disposition
            .lock()
            .remove(&(cid.to_string(), norm_url.clone()));
        let cd_name = if let Some(ref rid) = bound_req_id {
            let mut transfers = self.download_transfers_by_req_id.lock();
            if let Some(st) = transfers.get_mut(rid) {
                st.cd_consumed = true;
                if st.response_seen {
                    st.cd_filename.clone()
                } else {
                    st.cd_filename.clone().or(fallback_cd)
                }
            } else {
                fallback_cd
            }
        } else {
            fallback_cd
        };

        let desired_name = {
            let mut active = self.active_pool_downloads.lock();
            let chosen = if let Some(cd) = cd_name {
                cd
            } else if let Some(un_suffixed) = strip_firefox_collision_suffix(&safe_suggested)
                && (active.contains(&un_suffixed) || self.pool_download_dir.join(&un_suffixed).exists())
            {
                un_suffixed
            } else {
                safe_suggested.clone()
            };
            active.insert(safe_suggested.clone());
            chosen
        };

        let pool_dir = self.pool_download_dir.clone();
        let active_set = Arc::clone(&self.active_pool_downloads);
        let weak_conn = Arc::downgrade(self);
        let ctx_owned = cid.to_string();
        let is_http_url =
            norm_url.starts_with("http://") || norm_url.starts_with("https://");

        tokio::spawn(async move {
            const ZERO_BYTE_FALLBACK_SETTLE: Duration = Duration::from_millis(250);
            let src = pool_dir.join(&safe_suggested);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            let mut prev_len: Option<u64> = None;
            let mut stable_ticks = 0u32;
            let mut first_zero_ino: Option<u64> = None;
            let mut saw_ino_replaced = false;
            let mut saw_own_part = false;
            let mut zero_settle_since: Option<tokio::time::Instant> = None;

            while tokio::time::Instant::now() < deadline {
                let conn_opt = weak_conn.upgrade();
                if let Some(ref conn) = conn_opt
                    && conn.closed.load(Ordering::SeqCst)
                {
                    break;
                }

                let transfer_state = bound_req_id.as_deref().and_then(|rid| {
                    conn_opt
                        .as_ref()
                        .and_then(|conn| conn.download_transfers_by_req_id.lock().get(rid).cloned())
                });
                let download_end_status = nav_id.as_deref().and_then(|nid| {
                    conn_opt
                        .as_ref()
                        .and_then(|conn| conn.download_ends_by_nav.lock().get(nid).map(|(st, _)| *st))
                });
                let download_end_complete =
                    download_end_status == Some(DownloadEndStatus::Complete);
                let req_pending = conn_opt.as_ref().is_some_and(|conn| {
                    conn.context_requests
                        .read()
                        .get(&ctx_owned)
                        .is_some_and(|s| !s.is_empty())
                });
                let has_part = has_any_part_file(&pool_dir);
                let own_part_exists = pool_dir.join(format!("{safe_suggested}.part")).exists();
                let active_count = active_set.lock().len();
                let part_blocking = own_part_exists || (has_part && active_count <= 1);
                if own_part_exists {
                    saw_own_part = true;
                }
                if part_blocking {
                    zero_settle_since = None;
                    stable_ticks = 0;
                }

                if matches!(
                    transfer_state.as_ref().map(|s| s.status),
                    Some(DownloadTransferStatus::Failed)
                ) || download_end_status == Some(DownloadEndStatus::Canceled)
                {
                    for _ in 0..10 {
                        if !has_any_part_file(&pool_dir) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(15)).await;
                    }
                    if let Ok(meta) = std::fs::metadata(&src)
                        && meta.is_file()
                    {
                        let _ = std::fs::remove_file(&src);
                    }
                    break;
                }

                if let Ok(meta) = std::fs::metadata(&src)
                    && meta.is_file()
                {
                    let len = meta.len();
                    if prev_len == Some(len) {
                        if !part_blocking {
                            stable_ticks += 1;
                        }
                    } else {
                        prev_len = Some(len);
                        stable_ticks = 0;
                    }

                    let is_completed = matches!(
                        transfer_state.as_ref().map(|s| s.status),
                        Some(DownloadTransferStatus::Completed)
                    );
                    let expected_len = transfer_state.as_ref().and_then(|s| s.content_length);
                    let known_non_empty = expected_len.is_some_and(|cl| cl > 0)
                        || transfer_state.as_ref().is_some_and(|s| s.body_non_empty);
                    let download_end_supported = conn_opt
                        .as_ref()
                        .is_some_and(|conn| conn.download_end_supported.load(Ordering::Relaxed));

                    let ready = if len > 0 {
                        zero_settle_since = None;
                        let len_ok = expected_len.is_none_or(|cl| cl == 0 || len >= cl);
                        if download_end_complete {
                            len_ok && !part_blocking
                        } else if is_completed {
                            len_ok
                                && (!has_part
                                    || expected_len == Some(len)
                                    || (stable_ticks >= 2 && !req_pending)
                                    || (bound_req_id.is_some()
                                        && stable_ticks >= 4
                                        && !own_part_exists))
                        } else if !is_http_url || transfer_state.is_none() {
                            !has_part && stable_ticks >= 2 && !req_pending
                        } else {
                            false
                        }
                    } else {
                        // Track inode replacement of Firefox's 0-byte placeholder when
                        // `IOUtils.move(partFilePath, targetPath)` atomically finalizes it.
                        let cur_ino = file_inode(&meta);
                        if first_zero_ino.is_none() {
                            first_zero_ino = cur_ino;
                        } else if cur_ino != first_zero_ino {
                            saw_ino_replaced = true;
                        }
                        let zero_finalized = download_end_complete
                            || saw_ino_replaced
                            || (saw_own_part && !own_part_exists);

                        if known_non_empty {
                            // Response has a non-empty body (Content-Length > 0 or BiDi bodySize/content.size > 0);
                            // a 0-byte file on disk is strictly Firefox's pre-created placeholder.
                            zero_settle_since = None;
                            false
                        } else if download_end_complete {
                            !part_blocking
                        } else if bound_req_id.is_some()
                            && is_completed
                            && expected_len == Some(0)
                        {
                            if !part_blocking && !req_pending {
                                if zero_finalized {
                                    stable_ticks >= 1
                                } else {
                                    let since = zero_settle_since
                                        .get_or_insert_with(tokio::time::Instant::now);
                                    since.elapsed() >= ZERO_BYTE_FALLBACK_SETTLE
                                }
                            } else {
                                zero_settle_since = None;
                                false
                            }
                        } else if !has_part
                            && !req_pending
                            && expected_len.is_none_or(|cl| cl == 0)
                            && (is_completed || !is_http_url)
                        {
                            if zero_finalized {
                                stable_ticks >= 1
                            } else if is_http_url && download_end_supported && nav_id.is_some() {
                                // When Content-Length is absent on Firefox 145+ (`downloadEnd` supported),
                                // wait for `downloadEnd` or `.part` inode swap rather than timer-guessing.
                                zero_settle_since = None;
                                false
                            } else {
                                let settle = if is_http_url && expected_len.is_none() {
                                    Duration::from_millis(1000)
                                } else {
                                    ZERO_BYTE_FALLBACK_SETTLE
                                };
                                let since =
                                    zero_settle_since.get_or_insert_with(tokio::time::Instant::now);
                                stable_ticks >= 5 && since.elapsed() >= settle
                            }
                        } else {
                            zero_settle_since = None;
                            false
                        }
                    };

                    if ready {
                        if len == 0 {
                            let latest_own_part =
                                pool_dir.join(format!("{safe_suggested}.part")).exists();
                            let latest_active = active_set.lock().len();
                            let latest_part_blocking = latest_own_part
                                || (has_any_part_file(&pool_dir) && latest_active <= 1);
                            let grew_non_zero =
                                std::fs::metadata(&src).is_ok_and(|m| m.len() > 0);
                            if latest_part_blocking || grew_non_zero {
                                tokio::time::sleep(Duration::from_millis(15)).await;
                                continue;
                            }
                        }
                        if std::fs::create_dir_all(&dest_dir).is_ok() {
                            let dest = unique_dest_file(&dest_dir, &desired_name);
                            if std::fs::rename(&src, &dest).is_err()
                                && std::fs::copy(&src, &dest).is_ok()
                            {
                                let _ = std::fs::remove_file(&src);
                            }
                        }
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            if let Some(conn) = weak_conn.upgrade() {
                if let Some(ref rid) = bound_req_id {
                    conn.download_transfers_by_req_id.lock().remove(rid);
                }
                if let Some(ref nid) = nav_id {
                    conn.download_ends_by_nav.lock().remove(nid);
                }
            }
            active_set.lock().remove(&safe_suggested);
        });
    }

    async fn sync_global_intercept(&self, state_id: u64, want_intercept: bool) -> Result<()> {
        let mut guard = self.global_intercept_id.lock().await;
        let need = {
            let mut owners = self.intercept_owners.lock();
            if want_intercept {
                owners.insert(state_id);
            } else {
                owners.remove(&state_id);
            }
            !owners.is_empty()
        };

        if need && guard.is_none() {
            // Register connection-wide beforeRequestSent intercept (no "contexts" filter)
            // so newly opened popups / <a target="_blank"> tabs never race ahead of
            // async per-popup intercept setup. handle_incoming + BidiContextState route each
            // paused request exclusively to its owning session or pass untracked requests through.
            let res = self
                .send(
                    "network.addIntercept",
                    json!({
                        "phases": ["beforeRequestSent"],
                        "urlPatterns": [],
                    }),
                )
                .await;
            let res = match res {
                Ok(v) => v,
                Err(e) => {
                    self.intercept_owners.lock().remove(&state_id);
                    return Err(e);
                }
            };
            let id = res["intercept"]
                .as_str()
                .ok_or_else(|| {
                    self.intercept_owners.lock().remove(&state_id);
                    MimikyuError::Protocol {
                        code: -1,
                        message: "network.addIntercept missing intercept id".to_string(),
                    }
                })?
                .to_string();
            *guard = Some(id);
        } else if !need
            && let Some(id) = guard.take()
        {
            let _ = self
                .send("network.removeIntercept", json!({ "intercept": id }))
                .await;
        }
        Ok(())
    }

    fn fail_all_pending(&self, reason: &str) {
        let mut pending = self.pending.lock();
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(MimikyuError::Transport(reason.to_string())));
        }
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_with_timeout(method, params, RPC_TIMEOUT).await
    }

    async fn send_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
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

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(MimikyuError::Transport(
                "BiDi response channel dropped".to_string(),
            )),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(MimikyuError::Timeout {
                    timeout_ms: timeout.as_millis() as u64,
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
    state_id: u64,
    user_context: Option<String>,
    owns_user_context: bool,
    is_root: bool,
    main_world_eval: bool,
    telemetry_capacity: Option<usize>,
    telemetry: Arc<TelemetryStore>,
    interceptor: Mutex<NetworkInterceptor>,
    env_headers: Mutex<BTreeMap<String, String>>,
    env_profile: Mutex<Option<EnvProfile>>,
    env_uses_user_context: AtomicBool,
    env_script_id: AsyncMutex<Option<String>>,
    env_isolated_script_id: AsyncMutex<Option<String>>,
    download_dir: Option<PathBuf>,
    pending_permissions: Mutex<Vec<PermissionGrant>>,
    applied_permissions: Mutex<Vec<(String, String)>>,
    pending_storage_scripts: Mutex<Vec<(String, String)>>,
    inline_frame_owners: Mutex<HashMap<String, String>>,
    popup_contexts: Mutex<HashSet<String>>,
    popup_preload_scripts: AsyncMutex<HashMap<String, Vec<String>>>,
    popup_anim_scripts: AsyncMutex<HashMap<String, String>>,
    net_channel: String,
    net_worker_scripts: AsyncMutex<Option<(String, String)>>,
    popup_net_worker_scripts: AsyncMutex<HashMap<String, (String, String)>>,
    _proxy_relay: Mutex<Option<ProxyRelay>>,
    pressed_modifiers: Mutex<u32>,
    instant_animations: AtomicBool,
    anim_uses_user_context: AtomicBool,
    anim_gen: AtomicU64,
    anim_script_id: AsyncMutex<Option<String>>,
    load_notify: Notify,
    disposed: AtomicBool,
}

impl BidiContextState {
    fn maybe_apply_instant_animations_on_doc_event(
        self: &Arc<Self>,
        conn: &Arc<BidiConnection>,
        params: &Value,
    ) {
        if !self.instant_animations.load(Ordering::SeqCst)
            || self.disposed.load(Ordering::SeqCst)
        {
            return;
        }
        let Some(target_cid) = params
            .get("context")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
        else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let anim_gen = self.anim_gen.load(Ordering::SeqCst);
        let conn_clone = Arc::clone(conn);
        let state_clone = Arc::clone(self);
        tokio::spawn(async move {
            if !state_clone.instant_animations.load(Ordering::SeqCst)
                || state_clone.disposed.load(Ordering::SeqCst)
                || state_clone.anim_gen.load(Ordering::SeqCst) != anim_gen
            {
                return;
            }
            let fn_decl = format!("() => {{\n{}\n}}", BIDI_INSTANT_ANIM_ENABLE_SCRIPT);
            let _ = conn_clone
                .send(
                    "script.callFunction",
                    json!({
                        "functionDeclaration": fn_decl,
                        "target": { "context": target_cid, "sandbox": ISOLATED_SANDBOX },
                        "arguments": [],
                        "awaitPromise": true,
                        "resultOwnership": "none",
                    }),
                )
                .await;
        });
    }

    fn handle_event(self: &Arc<Self>, conn: &Arc<BidiConnection>, method: &str, params: Value) {
        match method {
            "browsingContext.domContentLoaded" => {
                self.maybe_apply_instant_animations_on_doc_event(conn, &params);
            }
            "browsingContext.load" => {
                self.maybe_apply_instant_animations_on_doc_event(conn, &params);
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
                if self.disposed.load(Ordering::SeqCst) {
                    if params.get("isBlocked").and_then(|v| v.as_bool()) == Some(true)
                        && tokio::runtime::Handle::try_current().is_ok()
                    {
                        let conn_clone = Arc::clone(conn);
                        tokio::spawn(async move {
                            let _ = conn_clone
                                .send("network.continueRequest", json!({ "request": req_id }))
                                .await;
                        });
                    }
                    return;
                }
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
                let is_sse_accept = request_headers.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("accept")
                        && v.to_ascii_lowercase().contains("text/event-stream")
                });
                let is_ws_upgrade = request_headers.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket")
                }) || url.starts_with("ws://")
                    || url.starts_with("wss://");
                let resource_type_enum =
                    ResourceType::from_bidi_event(destination, initiator_type, is_navigation);
                let resource_type_str = if is_sse_accept {
                    "EventSource".to_string()
                } else if is_ws_upgrade {
                    "WebSocket".to_string()
                } else if !destination.is_empty() {
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
                    let req_ctx_id = params
                        .get("context")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);

                    tokio::spawn(async move {
                        match decision {
                            InterceptDecision::Block { reason } => {
                                let _ = conn_clone.untrack_request(req_ctx_id.as_deref(), &req_id);
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
                                let _ = conn_clone.untrack_request(req_ctx_id.as_deref(), &req_id);
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
            "network.responseStarted" => {
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
                if is_bidi_sse_response(mime_type.as_deref(), &response_headers) {
                    self.telemetry
                        .set_pending_resource_type(req_id, "EventSource");
                }
                self.telemetry
                    .on_response_received(req_id, status, mime_type, response_headers);
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

    async fn setup_popup_context(self: &Arc<Self>, conn: &Arc<BidiConnection>, popup_cid: &str) {
        if self.disposed.load(Ordering::SeqCst)
            || !self.popup_contexts.lock().contains(popup_cid)
        {
            return;
        }

        let net_policy_opt = {
            let guard = self.interceptor.lock();
            guard
                .needs_bidi_intercept()
                .then(|| guard.to_shared_worker_policy_json())
        };
        let profile_opt = self.env_profile.lock().clone();
        let uses_uc = self.env_uses_user_context.load(Ordering::SeqCst);
        let instant_anim = self.instant_animations.load(Ordering::SeqCst);
        let anim_uses_uc = self.anim_uses_user_context.load(Ordering::SeqCst);
        let net_script_opt = net_policy_opt
            .as_ref()
            .map(|pol| build_firefox_shared_worker_network_script(pol, &self.net_channel));

        let win_script_opt = profile_opt
            .as_ref()
            .filter(|_| !uses_uc)
            .map(|profile| build_window_init_script(profile, EngineKind::Firefox));

        // Pipeline emulation + preload scripts so Firefox installs all overrides and
        // preload scripts in 1 RTT (connection-wide network.addIntercept is already active).
        let env_fut = async {
            let Some(ref profile) = profile_opt else {
                return;
            };
            let mut futs: Vec<
                std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + '_>>,
            > = Vec::new();
            futs.push(Box::pin(conn.send(
                "browsingContext.setViewport",
                json!({
                    "context": popup_cid,
                    "viewport": {
                        "width": profile.viewport_width,
                        "height": profile.viewport_height,
                    },
                    "devicePixelRatio": profile.device_scale_factor,
                }),
            )));
            if !uses_uc {
                if !profile.timezone.is_empty() {
                    futs.push(Box::pin(conn.send(
                        "emulation.setTimezoneOverride",
                        json!({
                            "timezone": profile.timezone,
                            "contexts": [popup_cid],
                        }),
                    )));
                }
                if !profile.locale.is_empty() {
                    futs.push(Box::pin(conn.send(
                        "emulation.setLocaleOverride",
                        json!({
                            "locale": profile.locale,
                            "contexts": [popup_cid],
                        }),
                    )));
                }
                if let Some(ref geo) = profile.geolocation {
                    futs.push(Box::pin(conn.send(
                        "emulation.setGeolocationOverride",
                        json!({
                            "coordinates": {
                                "latitude": geo.latitude,
                                "longitude": geo.longitude,
                                "accuracy": geo.accuracy,
                            },
                            "contexts": [popup_cid],
                        }),
                    )));
                }
                if let Some(ref win_script) = win_script_opt {
                    let fn_decl = format!("() => {{\n{win_script}\n}}");
                    futs.push(Box::pin(conn.send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [popup_cid],
                        }),
                    )));
                    futs.push(Box::pin(conn.send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [popup_cid],
                            "sandbox": ISOLATED_SANDBOX,
                        }),
                    )));
                }
            }
            let results = futures_util::future::join_all(futs).await;
            if !uses_uc {
                let mut added_ids = Vec::new();
                // Preload script responses are the last 2 items when win_script_opt is Some
                if win_script_opt.is_some() && results.len() >= 2 {
                    for res in results.iter().rev().take(2) {
                        if let Ok(v) = res
                            && let Some(sid) = v["script"].as_str()
                        {
                            added_ids.push(sid.to_string());
                        }
                    }
                }
                if !added_ids.is_empty() {
                    self.popup_preload_scripts
                        .lock()
                        .await
                        .entry(popup_cid.to_string())
                        .or_default()
                        .extend(added_ids);
                }
            }
        };

        let anim_fut = async {
            if !instant_anim {
                return;
            }
            let fn_decl = format!("() => {{\n{}\n}}", BIDI_INSTANT_ANIM_ENABLE_SCRIPT);
            if !anim_uses_uc
                && let Ok(res) = conn
                    .send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [popup_cid],
                            "sandbox": ISOLATED_SANDBOX,
                        }),
                    )
                    .await
                && let Some(sid) = res["script"].as_str()
            {
                if self.instant_animations.load(Ordering::SeqCst)
                    && !self.anim_uses_user_context.load(Ordering::SeqCst)
                    && !self.disposed.load(Ordering::SeqCst)
                    && self.popup_contexts.lock().contains(popup_cid)
                {
                    let prev_sid = self
                        .popup_anim_scripts
                        .lock()
                        .await
                        .insert(popup_cid.to_string(), sid.to_string());
                    if let Some(old_sid) = prev_sid {
                        let _ = conn
                            .send("script.removePreloadScript", json!({ "script": old_sid }))
                            .await;
                    }
                } else {
                    let _ = conn
                        .send("script.removePreloadScript", json!({ "script": sid }))
                        .await;
                }
            }
            if !self.instant_animations.load(Ordering::SeqCst)
                || self.disposed.load(Ordering::SeqCst)
            {
                return;
            }
            let mut anim_frames = Vec::new();
            let mut seen_frames = HashSet::new();
            if let Ok(tree) = conn
                .send("browsingContext.getTree", json!({ "root": popup_cid }))
                .await
                && let Some(contexts) = tree["contexts"].as_array()
            {
                let mut collected = Vec::new();
                for c in contexts {
                    collect_bidi_frames(c, None, &mut collected);
                }
                for f in collected {
                    if seen_frames.insert(f.frame_id.clone()) {
                        anim_frames.push(f.frame_id);
                    }
                }
            }
            if seen_frames.insert(popup_cid.to_string()) {
                anim_frames.insert(0, popup_cid.to_string());
            }
            for fid in anim_frames {
                let _ = conn
                    .send(
                        "script.callFunction",
                        json!({
                            "functionDeclaration": fn_decl,
                            "target": { "context": fid, "sandbox": ISOLATED_SANDBOX },
                            "arguments": [],
                            "awaitPromise": true,
                            "resultOwnership": "none",
                        }),
                    )
                    .await;
            }
        };

        let net_worker_fut = async {
            let Some(ref net_script) = net_script_opt else {
                return;
            };
            let fn_decl = format!("() => {{\n{net_script}\n}}");
            let (main_res, iso_res) = tokio::join!(
                conn.send(
                    "script.addPreloadScript",
                    json!({
                        "functionDeclaration": fn_decl,
                        "contexts": [popup_cid],
                    }),
                ),
                conn.send(
                    "script.addPreloadScript",
                    json!({
                        "functionDeclaration": fn_decl,
                        "contexts": [popup_cid],
                        "sandbox": ISOLATED_SANDBOX,
                    }),
                ),
            );
            let mid = main_res
                .ok()
                .and_then(|v| v["script"].as_str().map(ToString::to_string))
                .unwrap_or_default();
            let iid = iso_res
                .ok()
                .and_then(|v| v["script"].as_str().map(ToString::to_string))
                .unwrap_or_default();
            let still_active = !self.disposed.load(Ordering::SeqCst)
                && self.popup_contexts.lock().contains(popup_cid)
                && self.interceptor.lock().needs_bidi_intercept();
            if still_active {
                let prev = self
                    .popup_net_worker_scripts
                    .lock()
                    .await
                    .insert(popup_cid.to_string(), (mid, iid));
                if let Some((old_mid, old_iid)) = prev {
                    if !old_mid.is_empty() {
                        let _ = conn
                            .send("script.removePreloadScript", json!({ "script": old_mid }))
                            .await;
                    }
                    if !old_iid.is_empty() {
                        let _ = conn
                            .send("script.removePreloadScript", json!({ "script": old_iid }))
                            .await;
                    }
                }
            } else {
                if !mid.is_empty() {
                    let _ = conn
                        .send("script.removePreloadScript", json!({ "script": mid }))
                        .await;
                }
                if !iid.is_empty() {
                    let _ = conn
                        .send("script.removePreloadScript", json!({ "script": iid }))
                        .await;
                }
            }
        };

        tokio::join!(env_fut, anim_fut, net_worker_fut);

        if let Some(ref net_script) = net_script_opt {
            let _ = tokio::join!(
                conn.send(
                    "script.evaluate",
                    json!({
                        "expression": net_script,
                        "target": { "context": popup_cid },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                ),
                conn.send(
                    "script.evaluate",
                    json!({
                        "expression": net_script,
                        "target": { "context": popup_cid, "sandbox": ISOLATED_SANDBOX },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                ),
            );
        }

        if let Some(ref win_script) = win_script_opt {
            // Also evaluate immediately in case popup already committed initial document.
            let _ = tokio::join!(
                conn.send(
                    "script.evaluate",
                    json!({
                        "expression": win_script,
                        "target": { "context": popup_cid },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                ),
                conn.send(
                    "script.evaluate",
                    json!({
                        "expression": win_script,
                        "target": { "context": popup_cid, "sandbox": ISOLATED_SANDBOX },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                ),
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
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
        download_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let telem_cfg = telemetry_capacity
            .map(TelemetryConfig::with_capacity)
            .unwrap_or_default();
        let state = Arc::new(BidiContextState {
            state_id: NEXT_BIDI_STATE_ID.fetch_add(1, Ordering::Relaxed),
            user_context,
            owns_user_context,
            is_root,
            main_world_eval,
            telemetry_capacity,
            telemetry: Arc::new(TelemetryStore::new(telem_cfg)),
            interceptor: Mutex::new(NetworkInterceptor::default()),
            env_headers: Mutex::new(BTreeMap::new()),
            env_profile: Mutex::new(None),
            env_uses_user_context: AtomicBool::new(false),
            env_script_id: AsyncMutex::new(None),
            env_isolated_script_id: AsyncMutex::new(None),
            download_dir,
            pending_permissions: Mutex::new(Vec::new()),
            applied_permissions: Mutex::new(Vec::new()),
            pending_storage_scripts: Mutex::new(Vec::new()),
            inline_frame_owners: Mutex::new(HashMap::new()),
            popup_contexts: Mutex::new(HashSet::new()),
            popup_preload_scripts: AsyncMutex::new(HashMap::new()),
            popup_anim_scripts: AsyncMutex::new(HashMap::new()),
            net_channel: format!(
                "nws_{}_{}",
                std::process::id(),
                NEXT_NET_CH_ID.fetch_add(1, Ordering::Relaxed)
            ),
            net_worker_scripts: AsyncMutex::new(None),
            popup_net_worker_scripts: AsyncMutex::new(HashMap::new()),
            _proxy_relay: Mutex::new(proxy_relay),
            pressed_modifiers: Mutex::new(0),
            instant_animations: AtomicBool::new(false),
            anim_uses_user_context: AtomicBool::new(false),
            anim_gen: AtomicU64::new(0),
            anim_script_id: AsyncMutex::new(None),
            load_notify: Notify::new(),
            disposed: AtomicBool::new(false),
        });

        conn.contexts
            .write()
            .insert(info.target_id.clone(), Arc::clone(&state));
        if owns_user_context && let Some(ref uc) = state.user_context {
            conn.user_contexts
                .write()
                .insert(uc.clone(), Arc::clone(&state));
        } else if is_root && state.user_context.is_none() {
            conn.user_contexts
                .write()
                .insert("default".to_string(), Arc::clone(&state));
        }

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
        let (need_sw_net, policy_opt) = {
            let guard = self.state.interceptor.lock();
            let sw = guard.needs_bidi_intercept();
            let pol = sw.then(|| guard.to_shared_worker_policy_json());
            (sw, pol)
        };
        let need_intercept = need_sw_net || !self.state.env_headers.lock().is_empty();
        let popup_cids: Vec<String> = self.state.popup_contexts.lock().iter().cloned().collect();

        self.conn
            .sync_global_intercept(self.state.state_id, need_intercept)
            .await?;

        let eval_expr = {
            let mut main_net = self.state.net_worker_scripts.lock().await;
            let mut pop_net = self.state.popup_net_worker_scripts.lock().await;
            if let Some((old_mid, old_iid)) = main_net.take() {
                if !old_mid.is_empty() {
                    let _ = self.remove_init_script(&old_mid).await;
                }
                if !old_iid.is_empty() {
                    let _ = self.remove_init_script(&old_iid).await;
                }
            }
            for (_, (old_mid, old_iid)) in pop_net.drain() {
                if !old_mid.is_empty() {
                    let _ = self.remove_init_script(&old_mid).await;
                }
                if !old_iid.is_empty() {
                    let _ = self.remove_init_script(&old_iid).await;
                }
            }

            if let Some(ref pol) = policy_opt {
                let net_script =
                    build_firefox_shared_worker_network_script(pol, &self.state.net_channel);
                let fn_decl = format!("() => {{\n{net_script}\n}}");
                let (m_res, i_res) = tokio::join!(
                    self.conn.send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [self.ctx()],
                        }),
                    ),
                    self.conn.send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [self.ctx()],
                            "sandbox": ISOLATED_SANDBOX,
                        }),
                    ),
                );
                let mid = m_res
                    .ok()
                    .and_then(|v| v["script"].as_str().map(ToString::to_string))
                    .unwrap_or_default();
                let iid = i_res
                    .ok()
                    .and_then(|v| v["script"].as_str().map(ToString::to_string))
                    .unwrap_or_default();
                if !mid.is_empty() || !iid.is_empty() {
                    *main_net = Some((mid, iid));
                }
                for pcid in &popup_cids {
                    let (pm_res, pi_res) = tokio::join!(
                        self.conn.send(
                            "script.addPreloadScript",
                            json!({
                                "functionDeclaration": fn_decl,
                                "contexts": [pcid],
                            }),
                        ),
                        self.conn.send(
                            "script.addPreloadScript",
                            json!({
                                "functionDeclaration": fn_decl,
                                "contexts": [pcid],
                                "sandbox": ISOLATED_SANDBOX,
                            }),
                        ),
                    );
                    let pmid = pm_res
                        .ok()
                        .and_then(|v| v["script"].as_str().map(ToString::to_string))
                        .unwrap_or_default();
                    let piid = pi_res
                        .ok()
                        .and_then(|v| v["script"].as_str().map(ToString::to_string))
                        .unwrap_or_default();
                    if !pmid.is_empty() || !piid.is_empty() {
                        pop_net.insert(pcid.clone(), (pmid, piid));
                    }
                }
                net_script
            } else {
                build_firefox_shared_worker_network_clear_script(&self.state.net_channel)
            }
        };

        let mut all_contexts = Vec::with_capacity(1 + popup_cids.len());
        all_contexts.push(self.ctx().to_string());
        all_contexts.extend(popup_cids);
        let mut eval_contexts = Vec::with_capacity(all_contexts.len());
        let mut seen_eval = HashSet::with_capacity(all_contexts.len());
        for cid in &all_contexts {
            if seen_eval.insert(cid.clone()) {
                eval_contexts.push(cid.clone());
            }
            if let Ok(frames) = self.list_frames_for_context(cid).await {
                for f in frames {
                    if seen_eval.insert(f.frame_id.clone()) {
                        eval_contexts.push(f.frame_id);
                    }
                }
            }
        }
        for cid in eval_contexts {
            let _ = tokio::join!(
                self.conn.send(
                    "script.evaluate",
                    json!({
                        "expression": eval_expr,
                        "target": { "context": cid },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                ),
                self.conn.send(
                    "script.evaluate",
                    json!({
                        "expression": eval_expr,
                        "target": { "context": cid, "sandbox": ISOLATED_SANDBOX },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                ),
            );
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
        self.call_sandbox_fn_in_frame(fn_decl, args, None).await
    }

    async fn call_sandbox_fn_in_frame(
        &self,
        fn_decl: &str,
        args: Vec<Value>,
        frame_id: Option<&str>,
    ) -> Result<Value> {
        self.ensure_alive()?;
        let target_ctx = frame_id
            .filter(|f| !f.starts_with("iframe:"))
            .unwrap_or_else(|| self.ctx());
        let res = self
            .conn
            .send(
                "script.callFunction",
                json!({
                    "functionDeclaration": fn_decl,
                    "target": { "context": target_ctx, "sandbox": ISOLATED_SANDBOX },
                    "arguments": args,
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        parse_bidi_script_result_with_window(&res)
    }

    async fn eval_script_in_target(
        &self,
        expr: &str,
        frame_id: Option<&str>,
        sandbox: Option<&str>,
    ) -> Result<Value> {
        self.ensure_alive()?;
        let target_ctx = frame_id.unwrap_or_else(|| self.ctx());
        let target = match sandbox {
            Some(sb) => json!({ "context": target_ctx, "sandbox": sb }),
            None => json!({ "context": target_ctx }),
        };
        let res = self
            .conn
            .send(
                "script.evaluate",
                json!({
                    "expression": expr,
                    "target": target,
                    "awaitPromise": true,
                    "resultOwnership": "root",
                }),
            )
            .await?;
        let handle = res["result"]["handle"].as_str().map(ToString::to_string);
        let parsed = parse_bidi_script_result(&res);
        let out = match parsed {
            Ok(v) => Ok(v),
            Err(MimikyuError::EvalFailed(ref msg))
                if msg.contains("without value") && handle.is_some() =>
            {
                let h = handle.as_deref().unwrap_or_default();
                let check = self
                    .conn
                    .send(
                        "script.callFunction",
                        json!({
                            "functionDeclaration": BIDI_OBJECT_FALLBACK_VALIDATOR,
                            "target": target,
                            "arguments": [{ "handle": h }],
                            "awaitPromise": false,
                            "resultOwnership": "none",
                        }),
                    )
                    .await;
                match check {
                    Ok(cv) => match parse_bidi_script_result(&cv) {
                        Ok(Value::String(s)) => crate::driver::parse_protocol_json(&s)
                            .map_err(|(_, e)| MimikyuError::EvalFailed(e.to_string())),
                        Ok(other) => Ok(other),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        if let Some(h) = handle {
            let _ = self
                .conn
                .send(
                    "script.disown",
                    json!({
                        "handles": [h],
                        "target": target,
                    }),
                )
                .await;
        }
        out
    }

    async fn eval_isolated_with_window(&self, expr: &str, frame_id: Option<&str>) -> Result<Value> {
        self.ensure_alive()?;
        let target_ctx = frame_id.unwrap_or_else(|| self.ctx());
        let res = self
            .conn
            .send(
                "script.evaluate",
                json!({
                    "expression": expr,
                    "target": { "context": target_ctx, "sandbox": ISOLATED_SANDBOX },
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        parse_bidi_script_result_with_window(&res)
    }

    async fn send_modifier_key_actions(&self, mask: u32, down: bool) -> Result<()> {
        let actions = build_modifier_key_actions(mask, down);
        if actions.is_empty() {
            return Ok(());
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

    fn ax_sandbox_candidates(&self, frame_id: Option<&str>) -> Vec<String> {
        let mut out = Vec::with_capacity(3);
        let push_unique = |vec: &mut Vec<String>, ctx: &str| {
            if !vec.iter().any(|existing| existing == ctx) {
                vec.push(ctx.to_string());
            }
        };
        if let Some(fid) = frame_id.filter(|f| !f.starts_with("iframe:")) {
            if let Some(owner) = self.state.inline_frame_owners.lock().get(fid).cloned() {
                push_unique(&mut out, &owner);
            }
            push_unique(&mut out, fid);
        }
        push_unique(&mut out, self.ctx());
        out
    }

    async fn resolve_shared_element_with_ctx(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<(String, Value)> {
        let target_ctx = frame_id
            .filter(|f| !f.starts_with("iframe:"))
            .unwrap_or_else(|| self.ctx());

        if let Some(bid) = backend_node_id {
            for cand_ctx in self.ax_sandbox_candidates(frame_id) {
                let res = self
                    .conn
                    .send(
                        "script.callFunction",
                        json!({
                            "functionDeclaration": r#"(bid) => {
                                const st = globalThis.__mimikyuAxState;
                                let el = null;
                                if (st && st.idToEntry) {
                                    const entry = st.idToEntry.get(bid);
                                    el = entry && entry.ref ? entry.ref.deref() : null;
                                }
                                if (!el && globalThis.__mimikyuAxMap) {
                                    el = globalThis.__mimikyuAxMap.get(bid) || null;
                                }
                                if (!el) return null;
                                const doc = el.ownerDocument;
                                if (!doc) return null;
                                if (doc === document) return el;
                                const win = doc.defaultView;
                                if (!win) return null;
                                const path = [];
                                let cur = el;
                                while (cur && cur !== doc) {
                                    const parent = cur.parentNode;
                                    if (!parent || !parent.children) return null;
                                    const idx = Array.prototype.indexOf.call(parent.children, cur);
                                    if (idx < 0) return null;
                                    const isShadow = parent.nodeType === 11;
                                    path.push({ s: isShadow ? 1 : 0, i: idx });
                                    cur = isShadow ? parent.host : parent;
                                }
                                if (cur !== doc) return null;
                                path.reverse();
                                return [win, JSON.stringify(path)];
                            }"#,
                            "target": { "context": cand_ctx, "sandbox": ISOLATED_SANDBOX },
                            "arguments": [{ "type": "number", "value": bid }],
                            "awaitPromise": false,
                            "resultOwnership": "root",
                        }),
                    )
                    .await?;
                if res.get("type").and_then(|v| v.as_str()) == Some("exception") {
                    continue;
                }
                let val = &res["result"];
                if val.get("type").and_then(|v| v.as_str()) == Some("node") {
                    return Ok((cand_ctx, val.clone()));
                }
                if val.get("type").and_then(|v| v.as_str()) == Some("array")
                    && let Some(items) = val.get("value").and_then(|v| v.as_array())
                    && items.len() == 2
                {
                    let el_ctx = items[0]["value"]["context"]
                        .as_str()
                        .or_else(|| items[0]["context"].as_str())
                        .map(ToString::to_string)
                        .unwrap_or_else(|| target_ctx.to_string());
                    let Some(path_json) = items[1]["value"].as_str() else {
                        continue;
                    };
                    let child_res = self
                        .conn
                        .send(
                            "script.callFunction",
                            json!({
                                "functionDeclaration": r#"(pathJson) => {
                                    const path = JSON.parse(pathJson);
                                    let cur = document;
                                    for (const step of path) {
                                        const container = step.s ? cur.shadowRoot : cur;
                                        if (!container || !container.children) return null;
                                        cur = container.children[step.i];
                                        if (!cur) return null;
                                    }
                                    return cur;
                                }"#,
                                "target": { "context": el_ctx, "sandbox": ISOLATED_SANDBOX },
                                "arguments": [{ "type": "string", "value": path_json }],
                                "awaitPromise": false,
                                "resultOwnership": "root",
                            }),
                        )
                        .await?;
                    if child_res.get("type").and_then(|v| v.as_str()) != Some("exception") {
                        let child_val = &child_res["result"];
                        if child_val.get("type").and_then(|v| v.as_str()) == Some("node") {
                            return Ok((el_ctx, child_val.clone()));
                        }
                    }
                }
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "backend_node_id {bid} not found in current document"
            )));
        }

        if let Some(sel) = selector {
            let res = self
                .conn
                .send(
                    "browsingContext.locateNodes",
                    json!({
                        "context": target_ctx,
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
            return Ok((target_ctx.to_string(), node));
        }

        Err(MimikyuError::InvalidConfig(
            "requires either backend_node_id or selector".to_string(),
        ))
    }

    async fn collect_child_iframe_rects(
        &self,
        parent_ctx: &str,
    ) -> HashMap<String, (bool, FrameTransform)> {
        let mut out = HashMap::new();
        let Ok(val) = self
            .call_sandbox_fn_in_frame(
                r#"() => {
                    const items = [];
                    const visitRoot = (root) => {
                        if (!root || !root.querySelectorAll) return;
                        for (const ifr of root.querySelectorAll('iframe')) {
                            let sameOriginWalked = false;
                            try {
                                const cdoc = ifr.contentDocument;
                                if (cdoc && cdoc.body) sameOriginWalked = true;
                            } catch {}
                            const r = ifr.getBoundingClientRect();
                            const cs = getComputedStyle(ifr);
                            const iw = ifr.offsetWidth;
                            const ih = ifr.offsetHeight;
                            const sx = iw > 0 && r.width > 0 ? r.width / iw : 1;
                            const sy = ih > 0 && r.height > 0 ? r.height / ih : 1;
                            items.push({
                                win: ifr.contentWindow,
                                sameOriginWalked,
                                x: r.x + ((parseFloat(cs.borderLeftWidth) || 0) + (parseFloat(cs.paddingLeft) || 0)) * sx,
                                y: r.y + ((parseFloat(cs.borderTopWidth) || 0) + (parseFloat(cs.paddingTop) || 0)) * sy,
                                sx,
                                sy,
                            });
                        }
                        for (const el of root.querySelectorAll('*')) {
                            if (el.shadowRoot) visitRoot(el.shadowRoot);
                        }
                    };
                    visitRoot(document);
                    return items;
                }"#,
                vec![],
                Some(parent_ctx),
            )
            .await
        else {
            return out;
        };
        if let Some(arr) = val.as_array() {
            for item in arr {
                if let Some(child_ctx) = item["win"]["context"].as_str() {
                    let walked = item["sameOriginWalked"].as_bool().unwrap_or(false);
                    let tf = FrameTransform {
                        ox: item["x"].as_f64().unwrap_or(0.0),
                        oy: item["y"].as_f64().unwrap_or(0.0),
                        sx: item["sx"].as_f64().unwrap_or(1.0),
                        sy: item["sy"].as_f64().unwrap_or(1.0),
                    };
                    out.insert(child_ctx.to_string(), (walked, tf));
                }
            }
        }
        out
    }

    async fn resolve_single_child_iframe_offset(
        &self,
        child_ctx: &str,
        parent_ctx: &str,
    ) -> Option<FrameTransform> {
        // Closed shadow roots hide <iframe> elements from parent querySelectorAll;
        // same-origin child contexts still expose their host element via window.frameElement.
        if let Ok(val) = self
            .call_sandbox_fn_in_frame(
                r#"() => {
                    try {
                        const fe = window.frameElement;
                        if (!fe) return null;
                        const r = fe.getBoundingClientRect();
                        const pw = (fe.ownerDocument && fe.ownerDocument.defaultView) || window.parent;
                        const cs = pw.getComputedStyle(fe);
                        const iw = fe.offsetWidth;
                        const ih = fe.offsetHeight;
                        const sx = iw > 0 && r.width > 0 ? r.width / iw : 1;
                        const sy = ih > 0 && r.height > 0 ? r.height / ih : 1;
                        return {
                            x: r.x + ((parseFloat(cs.borderLeftWidth) || 0) + (parseFloat(cs.paddingLeft) || 0)) * sx,
                            y: r.y + ((parseFloat(cs.borderTopWidth) || 0) + (parseFloat(cs.paddingTop) || 0)) * sy,
                            sx,
                            sy,
                        };
                    } catch {
                        return null;
                    }
                }"#,
                vec![],
                Some(child_ctx),
            )
            .await
            && let (Some(ox), Some(oy)) = (val["x"].as_f64(), val["y"].as_f64())
        {
            return Some(FrameTransform {
                ox,
                oy,
                sx: val["sx"].as_f64().unwrap_or(1.0),
                sy: val["sy"].as_f64().unwrap_or(1.0),
            });
        }

        // Cross-origin closed-shadow-DOM iframe fallback via Firefox viewport screen coordinates.
        let screen_fn = r#"() => ({
            sx: typeof window.mozInnerScreenX === 'number' && Number.isFinite(window.mozInnerScreenX) ? window.mozInnerScreenX : null,
            sy: typeof window.mozInnerScreenY === 'number' && Number.isFinite(window.mozInnerScreenY) ? window.mozInnerScreenY : null
        })"#;
        if let (Ok(cv), Ok(pv)) = (
            self.call_sandbox_fn_in_frame(screen_fn, vec![], Some(child_ctx))
                .await,
            self.call_sandbox_fn_in_frame(screen_fn, vec![], Some(parent_ctx))
                .await,
        ) && let (Some(cx), Some(cy), Some(px), Some(py)) = (
            cv["sx"].as_f64(),
            cv["sy"].as_f64(),
            pv["sx"].as_f64(),
            pv["sy"].as_f64(),
        ) {
            return Some(FrameTransform {
                ox: cx - px,
                oy: cy - py,
                sx: 1.0,
                sy: 1.0,
            });
        }
        None
    }

    async fn resolve_frame_cumulative_offset(
        &self,
        frame_id: &str,
        parent_map: &HashMap<String, String>,
        cache: &mut HashMap<String, HashMap<String, (bool, FrameTransform)>>,
    ) -> (bool, FrameTransform) {
        let mut total = FrameTransform::identity();
        let mut immediate_walked = false;
        let mut cur = frame_id.to_string();
        for hop in 0..32 {
            if cur == self.ctx() {
                break;
            }
            let Some(parent) = parent_map.get(&cur) else {
                break;
            };
            if !cache.contains_key(parent) {
                let rects = self.collect_child_iframe_rects(parent).await;
                cache.insert(parent.clone(), rects);
            }
            if let Some(&(walked, tf)) = cache.get(parent).and_then(|m| m.get(&cur)) {
                if hop == 0 {
                    immediate_walked = walked;
                }
                total = tf.compose_inner(total);
            } else if let Some(tf) = self.resolve_single_child_iframe_offset(&cur, parent).await {
                cache
                    .entry(parent.clone())
                    .or_default()
                    .insert(cur.clone(), (false, tf));
                total = tf.compose_inner(total);
            }
            cur = parent.clone();
        }
        (immediate_walked, total)
    }

    async fn list_frames_for_context(&self, root_ctx: &str) -> Result<Vec<FrameInfo>> {
        self.ensure_alive()?;
        let tree = self
            .conn
            .send("browsingContext.getTree", json!({ "root": root_ctx }))
            .await?;
        let mut out = Vec::new();
        if let Some(contexts) = tree["contexts"].as_array() {
            for c in contexts {
                collect_bidi_frames(c, None, &mut out);
            }
        }
        Ok(out)
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

        let cleanup_uc = |conn: Arc<BidiConnection>, uc: String| async move {
            conn.user_contexts.write().remove(&uc);
            conn.isolated_contexts.write().remove(&uc);
            conn.active_targets
                .write()
                .remove(&context_scope_key(Some(&uc)));
            let _ = conn
                .send("browser.removeUserContext", json!({ "userContext": uc }))
                .await;
        };

        let created = match self
            .conn
            .send(
                "browsingContext.create",
                json!({
                    "type": "window",
                    "userContext": uc,
                }),
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                cleanup_uc(Arc::clone(&self.conn), uc).await;
                return Err(e);
            }
        };
        let Some(target_id) = created["context"].as_str().map(ToString::to_string) else {
            cleanup_uc(Arc::clone(&self.conn), uc).await;
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "browsingContext.create missing context".to_string(),
            });
        };
        self.conn.set_active_target(Some(&uc), &target_id);

        let fallback_profile_dir = self
            .info
            .profile_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let dl_dir = opts
            .download_dir
            .clone()
            .unwrap_or_else(|| fallback_profile_dir.join("downloads").join(&uc));
        if let Err(e) = std::fs::create_dir_all(&dl_dir) {
            cleanup_uc(Arc::clone(&self.conn), uc).await;
            return Err(e.into());
        }

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
            Some(dl_dir),
        );

        if let Err(err) = session
            .apply_initial_options(
                opts.env_profile,
                opts.permissions,
                opts.block_images,
                opts.block_media,
                opts.block_fonts,
                opts.instant_animations,
            )
            .await
        {
            let _ = session.dispose().await;
            return Err(err);
        }

        Ok(session)
    }

    async fn new_tab(&self, url: Option<&str>) -> Result<Arc<dyn DriverSession>> {
        self.ensure_alive()?;
        let parent_env_profile = self.state.env_profile.lock().clone();
        let parent_env_headers = self.state.env_headers.lock().clone();
        let parent_interceptor = self.state.interceptor.lock().clone();
        let parent_pending_perms = self.state.pending_permissions.lock().clone();
        let parent_instant_anim = self.state.instant_animations.load(Ordering::SeqCst);

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
            self.state.download_dir.clone(),
        );

        *session.state.interceptor.lock() = parent_interceptor;
        *session.state.pending_permissions.lock() = parent_pending_perms;
        if parent_env_profile.is_none() && !parent_env_headers.is_empty() {
            *session.state.env_headers.lock() = parent_env_headers;
        }
        let need_intercept = {
            let guard = session.state.interceptor.lock();
            guard.needs_bidi_intercept()
        } || !session.state.env_headers.lock().is_empty();

        if let Some(profile) = parent_env_profile {
            if let Err(err) = session.set_env_overrides(profile).await {
                let _ = session.dispose().await;
                return Err(err);
            }
        } else if need_intercept && let Err(err) = session.sync_intercept().await {
            let _ = session.dispose().await;
            return Err(err);
        }
        if parent_instant_anim && let Err(err) = session.set_instant_animations(true).await {
            let _ = session.dispose().await;
            return Err(err);
        }

        self.conn
            .set_active_target(self.state.user_context.as_deref(), &target_id);

        if let Some(u) = url
            && u != "about:blank"
            && let Err(err) = session.navigate(u).await
        {
            let _ = session.dispose().await;
            return Err(err);
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
        // In headless Firefox, `browsingContext.activate` switches `selectedTab` immediately on
        // same-window background tabs (such as `<a target="_blank">` / `window.open()` popups)
        // but can hang waiting for an OS window-focus event. Retry once with a short timeout
        // (once selected, the second `activate` returns immediately) and fall back to verifying
        // context existence via `browsingContext.getTree` + `window.focus()`.
        match self
            .conn
            .send_with_timeout(
                "browsingContext.activate",
                json!({ "context": target_id }),
                Duration::from_millis(400),
            )
            .await
        {
            Ok(_) => {}
            Err(MimikyuError::Timeout { .. }) => {
                match self
                    .conn
                    .send_with_timeout(
                        "browsingContext.activate",
                        json!({ "context": target_id }),
                        Duration::from_millis(1500),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(MimikyuError::Timeout { .. }) => {
                        self.conn
                            .send("browsingContext.getTree", json!({ "root": target_id }))
                            .await?;
                        let _ = self
                            .conn
                            .send(
                                "script.evaluate",
                                json!({
                                    "expression": "(() => { try { window.focus(); } catch {} })()",
                                    "target": { "context": target_id, "sandbox": ISOLATED_SANDBOX },
                                    "awaitPromise": false,
                                    "resultOwnership": "none",
                                }),
                            )
                            .await;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
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
        self.state.inline_frame_owners.lock().clear();
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
        self.state.inline_frame_owners.lock().clear();
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
        self.state.inline_frame_owners.lock().clear();
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
        self.state.inline_frame_owners.lock().clear();
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
        self.eval_isolated_in_frame(expr, None).await
    }

    async fn eval_main_world(&self, expr: &str) -> Result<Value> {
        self.eval_main_world_in_frame(expr, None).await
    }

    async fn list_frames(&self) -> Result<Vec<FrameInfo>> {
        self.list_frames_for_context(self.ctx()).await
    }

    async fn eval_isolated_in_frame(&self, expr: &str, frame_id: Option<&str>) -> Result<Value> {
        self.eval_script_in_target(expr, frame_id, Some(ISOLATED_SANDBOX))
            .await
    }

    async fn eval_main_world_in_frame(&self, expr: &str, frame_id: Option<&str>) -> Result<Value> {
        self.ensure_alive()?;
        if !self.state.main_world_eval {
            return Err(MimikyuError::MainWorldDisabled);
        }
        self.eval_script_in_target(expr, frame_id, None).await
    }

    async fn call_function_on_node(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
        function_declaration: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        self.ensure_alive()?;
        let bidi_args_json = serde_json::to_string(&args)?;
        let wrapper = format!(
            r#"(bid, argsJson) => {{
                const st = globalThis.__mimikyuAxState;
                const entry = st && st.idToEntry ? st.idToEntry.get(bid) : null;
                let el = entry && entry.ref ? entry.ref.deref() : null;
                if (!el && globalThis.__mimikyuAxMap) el = globalThis.__mimikyuAxMap.get(bid) || null;
                if (!el) return {{ __notFound: true }};
                const fn = ({function_declaration});
                const parsedArgs = JSON.parse(argsJson);
                return {{ __ok: true, value: fn.apply(el, parsedArgs) }};
            }}"#
        );
        let call_args = vec![
            json!({ "type": "number", "value": backend_node_id }),
            json!({ "type": "string", "value": bidi_args_json }),
        ];
        let mut last_err: Option<MimikyuError> = None;
        for cand_ctx in self.ax_sandbox_candidates(frame_id) {
            match self
                .call_sandbox_fn_in_frame(&wrapper, call_args.clone(), Some(&cand_ctx))
                .await
            {
                Ok(res) => {
                    if res.get("__notFound").and_then(|v| v.as_bool()) == Some(true) {
                        continue;
                    }
                    return Ok(res.get("value").cloned().unwrap_or(Value::Null));
                }
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        Err(MimikyuError::ElementNotFound(format!(
            "backend_node_id {backend_node_id} not found"
        )))
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
        let chain_json = serde_json::to_string(selector_chain)?;
        let res = self
            .call_sandbox_fn_in_frame(
                r#"(chainJson) => {
                    const chain = JSON.parse(chainJson);
                    let current = Array.from(document.querySelectorAll(chain[0]));
                    if (chain.length === 1) {
                        return { status: 'ok', matches: current.map(el => {
                            const attrs = {};
                            for (const a of el.attributes) attrs[a.name] = a.value;
                            let r = (() => {
                const raw = el.getBoundingClientRect();
                if (raw.width > 0 || raw.height > 0) return raw;
                const doc = el.ownerDocument || document;
                const cs = (doc.defaultView || window).getComputedStyle(el);
                if (cs && cs.display === 'contents') {
                    try {
                        const range = doc.createRange();
                        range.selectNodeContents(el);
                        const rr = range.getBoundingClientRect();
                        if (rr && rr.width > 0 && rr.height > 0) return rr;
                    } catch {}
                }
                return raw;
            })();
                            return {
                                tag_name: el.tagName.toLowerCase(),
                                text: (el.innerText || el.textContent || '').trim(),
                                html: el.outerHTML || null,
                                attributes: attrs,
                                bounds: { x: r.x, y: r.y, width: r.width, height: r.height }
                            };
                        }) };
                    }
                    for (let i = 1; i < chain.length; i++) {
                        if (current.length === 0) return { status: 'ok', matches: [] };
                        const roots = current.map(el => el.shadowRoot).filter(Boolean);
                        if (roots.length === 0) {
                            return { status: 'closed_or_missing_shadow' };
                        }
                        const next = [];
                        for (const sr of roots) {
                            for (const found of sr.querySelectorAll(chain[i])) {
                                next.push(found);
                            }
                        }
                        current = next;
                    }
                    return {
                        status: 'ok',
                        matches: current.map(el => {
                            const attrs = {};
                            for (const a of el.attributes) attrs[a.name] = a.value;
                            let r = (() => {
                const raw = el.getBoundingClientRect();
                if (raw.width > 0 || raw.height > 0) return raw;
                const doc = el.ownerDocument || document;
                const cs = (doc.defaultView || window).getComputedStyle(el);
                if (cs && cs.display === 'contents') {
                    try {
                        const range = doc.createRange();
                        range.selectNodeContents(el);
                        const rr = range.getBoundingClientRect();
                        if (rr && rr.width > 0 && rr.height > 0) return rr;
                    } catch {}
                }
                return raw;
            })();
                            return {
                                tag_name: el.tagName.toLowerCase(),
                                text: (el.innerText || el.textContent || '').trim(),
                                html: el.outerHTML || null,
                                attributes: attrs,
                                bounds: { x: r.x, y: r.y, width: r.width, height: r.height }
                            };
                        })
                    };
                }"#,
                vec![json!({ "type": "string", "value": chain_json })],
                frame_id,
            )
            .await?;

        if res["status"].as_str() == Some("closed_or_missing_shadow") {
            return Err(MimikyuError::Unsupported {
                engine: "firefox".to_string(),
                feature: "closed shadow DOM traversal".to_string(),
            });
        }

        let target_fid = frame_id.unwrap_or_else(|| self.ctx()).to_string();
        let arr = res["matches"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(arr.len());
        for m in arr {
            let attributes: BTreeMap<String, String> =
                serde_json::from_value(m["attributes"].clone()).unwrap_or_default();
            let bounds: Option<BoundingBox> = serde_json::from_value(m["bounds"].clone()).ok();
            out.push(ShadowMatch {
                backend_node_id: None,
                frame_id: Some(target_fid.clone()),
                tag_name: m["tag_name"].as_str().unwrap_or_default().to_string(),
                text: m["text"].as_str().unwrap_or_default().to_string(),
                html: m["html"].as_str().map(ToString::to_string),
                attributes,
                bounds,
            });
        }
        Ok(out)
    }

    async fn closed_shadow_hosts(&self, _frame_id: Option<&str>) -> Result<Vec<i64>> {
        self.ensure_alive()?;
        Ok(Vec::new())
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
        let frames = self.list_frames().await.unwrap_or_default();
        let walker_js = r#"(() => {
            const nodes = [];
            const iframeWins = [];
            if (!globalThis.__mimikyuAxState) {
                globalThis.__mimikyuAxState = {
                    nextId: 1,
                    elToId: new WeakMap(),
                    idToEntry: new Map(),
                };
            }
            const state = globalThis.__mimikyuAxState;
            const getOrAssignId = (el, frameId) => {
                let bid = state.elToId.get(el);
                if (!bid) {
                    bid = state.nextId++;
                    state.elToId.set(el, bid);
                }
                state.idToEntry.set(bid, { ref: new WeakRef(el), frameId });
                return bid;
            };

            const tagRole = (el) => {
                const explicit = el.getAttribute('role');
                if (explicit) return explicit.trim().split(/\s+/)[0];
                const tag = el.tagName;
                if (tag === 'A' && el.hasAttribute('href')) return 'link';
                if (tag === 'BUTTON' || tag === 'SUMMARY') return 'button';
                if (/^H[1-6]$/.test(tag)) return 'heading';
                if (tag === 'INPUT') {
                    const t = (el.getAttribute('type') || 'text').toLowerCase();
                    if (t === 'checkbox') return 'checkbox';
                    if (t === 'radio') return 'radio';
                    if (t === 'submit' || t === 'button' || t === 'reset' || t === 'image' || t === 'file') return 'button';
                    if (t === 'search') return 'searchbox';
                    if (t === 'number') return 'spinbutton';
                    if (t === 'range') return 'slider';
                    if (t === 'hidden') return 'none';
                    return 'textbox';
                }
                if (tag === 'TEXTAREA') return 'textbox';
                if (tag === 'SELECT') return 'combobox';
                if (tag === 'OPTION') return 'option';
                if (tag === 'DIALOG') return 'dialog';
                if (tag === 'IFRAME') return 'Iframe';
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
                if (el.isContentEditable) return 'textbox';
                return 'generic';
            };

            const LEAF_NAME_ROLES = new Set([
                'button', 'link', 'heading', 'menuitem', 'menuitemcheckbox',
                'menuitemradio', 'tab', 'option', 'checkbox', 'radio', 'switch',
                'cell', 'columnheader', 'rowheader', 'listitem', 'label',
                'alert', 'status',
            ]);

            const computeName = (el, role) => {
                const al = el.getAttribute('aria-label');
                if (al && al.trim()) return al.trim();
                const root = el.getRootNode ? el.getRootNode() : document;
                const lookupById = (id) => {
                    if (root && typeof root.getElementById === 'function') {
                        const found = root.getElementById(id);
                        if (found) return found;
                    }
                    return el.ownerDocument ? el.ownerDocument.getElementById(id) : null;
                };
                const labelledBy = el.getAttribute('aria-labelledby');
                if (labelledBy) {
                    const parts = labelledBy.split(/\s+/).map(id => {
                        const ref = lookupById(id);
                        return ref ? (ref.textContent || '').trim() : '';
                    }).filter(Boolean);
                    if (parts.length) return parts.join(' ');
                }
                if (el.id) {
                    const sel = `label[for="${CSS.escape(el.id)}"]`;
                    const lbl = (root && root.querySelector ? root.querySelector(sel) : null)
                        || (el.ownerDocument ? el.ownerDocument.querySelector(sel) : null);
                    if (lbl && lbl.textContent) return lbl.textContent.trim();
                }
                if (['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName) && el.closest) {
                    const wrapLbl = el.closest('label');
                    if (wrapLbl) {
                        const clone = wrapLbl.cloneNode(true);
                        for (const ctrl of clone.querySelectorAll('input,select,textarea')) ctrl.remove();
                        const txt = (clone.textContent || '').replace(/\s+/g, ' ').trim();
                        if (txt) return txt;
                    }
                }
                if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
                    const t = (el.getAttribute('type') || '').toLowerCase();
                    if ((t === 'submit' || t === 'button' || t === 'reset') && el.value) return el.value.trim();
                    if (t === 'file') return 'Choose File';
                    if (el.placeholder) return el.placeholder.trim();
                }
                if (el.tagName === 'IMG' && el.alt) return el.alt.trim();
                if (role === 'button' || role === 'link') {
                    const img = el.querySelector('img[alt]');
                    if (img && img.alt && img.alt.trim()) return img.alt.trim();
                    const svgTitle = el.querySelector('svg title');
                    if (svgTitle && svgTitle.textContent && svgTitle.textContent.trim()) {
                        return svgTitle.textContent.trim();
                    }
                }
                if (el.title) return el.title.trim();
                if (LEAF_NAME_ROLES.has(role)) {
                    const txt = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
                    if (txt && txt.length <= 200) return txt;
                }
                return null;
            };

            const visit = (el, parentId, frameId, offX, offY, scaleX, scaleY, depth) => {
                if (!el || el.nodeType !== 1 || depth > 64) return null;
                const tag = el.tagName;
                if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT' || tag === 'TEMPLATE') return null;
                const win = el.ownerDocument && el.ownerDocument.defaultView ? el.ownerDocument.defaultView : window;
                const style = win.getComputedStyle(el);
                if (style.display === 'none' || style.visibility === 'hidden') return null;
                if (el.getAttribute('aria-hidden') === 'true') return null;

                const role = tagRole(el);
                if (role === 'none' || role === 'presentation') return null;

                const isContents = style.display === 'contents';
                const isFileInput = tag === 'INPUT' && (el.getAttribute('type') || '').toLowerCase() === 'file';
                const focusable = el.tabIndex >= 0 || ['A','BUTTON','INPUT','SELECT','TEXTAREA','SUMMARY'].includes(tag) || el.isContentEditable;
                const computeContentsRect = (target) => {
                    try {
                        const d = target.ownerDocument || document;
                        const range = d.createRange();
                        range.selectNodeContents(target);
                        const rr = range.getBoundingClientRect();
                        if (rr && rr.width > 0 && rr.height > 0) return rr;
                        const rects = range.getClientRects();
                        if (rects && rects.length > 0) {
                            let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
                            for (const cr of rects) {
                                if (cr.width <= 0 && cr.height <= 0) continue;
                                if (cr.x < minX) minX = cr.x;
                                if (cr.y < minY) minY = cr.y;
                                if (cr.right > maxX) maxX = cr.right;
                                if (cr.bottom > maxY) maxY = cr.bottom;
                            }
                            if (maxX > minX && maxY > minY) {
                                return { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
                            }
                        }
                    } catch {}
                    return target.getBoundingClientRect();
                };
                const rect = isContents ? computeContentsRect(el) : el.getBoundingClientRect();
                const bounds = isFileInput
                    ? { x: offX + rect.x * scaleX, y: offY + rect.y * scaleY, width: Math.max(rect.width * scaleX, 1), height: Math.max(rect.height * scaleY, 1) }
                    : { x: offX + rect.x * scaleX, y: offY + rect.y * scaleY, width: rect.width * scaleX, height: rect.height * scaleY };

                const bid = getOrAssignId(el, frameId);
                const nodeId = String(bid);
                const doc = el.ownerDocument || document;
                const focused = doc.activeElement === el;
                const disabled = !!el.disabled || el.getAttribute('aria-disabled') === 'true';
                const checkedAttr = el.getAttribute('aria-checked');
                const checked = (tag === 'INPUT' && (el.type === 'checkbox' || el.type === 'radio'))
                    ? !!el.checked
                    : (checkedAttr === 'true' ? true : checkedAttr === 'false' ? false : null);
                const expAttr = el.getAttribute('aria-expanded');
                const expanded = expAttr === 'true' ? true : expAttr === 'false' ? false : null;
                const selAttr = el.getAttribute('aria-selected');
                const selected = typeof el.selected === 'boolean' ? el.selected : (selAttr === 'true' ? true : selAttr === 'false' ? false : null);
                const required = !!el.required || el.getAttribute('aria-required') === 'true';
                const level = /^H([1-6])$/.test(tag) ? Number(tag.slice(1)) : (el.getAttribute('aria-level') ? Number(el.getAttribute('aria-level')) : null);

                let value = null;
                if (tag === 'INPUT') {
                    const t = (el.getAttribute('type') || 'text').toLowerCase();
                    if (t !== 'checkbox' && t !== 'radio' && t !== 'submit' && t !== 'button' && t !== 'reset' && t !== 'file') {
                        value = el.value !== undefined && el.value !== null ? String(el.value) : null;
                    }
                } else if (tag === 'TEXTAREA') {
                    value = el.value !== undefined && el.value !== null ? String(el.value) : null;
                } else if (tag === 'SELECT') {
                    const opt = el.options && el.selectedIndex >= 0 ? el.options[el.selectedIndex] : null;
                    value = opt ? (opt.textContent || opt.value || '').trim() : (el.value || null);
                } else if (el.isContentEditable && tag !== 'BODY') {
                    value = (el.innerText || '').trim();
                }

                const entry = {
                    node_id: nodeId,
                    parent_id: parentId,
                    child_ids: [],
                    role,
                    name: computeName(el, role),
                    value,
                    description: el.getAttribute('aria-description') || null,
                    backend_node_id: bid,
                    frame_id: frameId,
                    ignored: isContents && role === 'generic' && !focusable,
                    focusable,
                    focused,
                    disabled,
                    checked,
                    expanded,
                    selected,
                    required,
                    level,
                    bounds,
                };
                nodes.push(entry);

                const skipTextChildren = tag === 'SELECT' || tag === 'OPTION' || tag === 'TEXTAREA';
                const pushChildren = (root) => {
                    for (const child of root.childNodes) {
                        if (child.nodeType === 1) {
                            const cid = visit(child, nodeId, frameId, offX, offY, scaleX, scaleY, depth + 1);
                            if (cid) entry.child_ids.push(cid);
                        } else if (child.nodeType === 3 && !skipTextChildren) {
                            const txt = (child.textContent || '').replace(/\s+/g, ' ').trim();
                            if (!txt) continue;
                            let tr = null;
                            try {
                                const range = doc.createRange();
                                range.selectNodeContents(child);
                                tr = range.getBoundingClientRect();
                            } catch {}
                            if (!tr || tr.width <= 0 || tr.height <= 0) continue;
                            const tid = String(state.nextId++);
                            nodes.push({
                                node_id: tid,
                                parent_id: nodeId,
                                child_ids: [],
                                role: 'StaticText',
                                name: txt,
                                value: null,
                                description: null,
                                backend_node_id: null,
                                frame_id: frameId,
                                ignored: false,
                                focusable: false,
                                focused: false,
                                disabled: false,
                                checked: null,
                                expanded: null,
                                selected: null,
                                required: false,
                                level: null,
                                bounds: {
                                    x: offX + tr.x * scaleX,
                                    y: offY + tr.y * scaleY,
                                    width: tr.width * scaleX,
                                    height: tr.height * scaleY,
                                },
                            });
                            entry.child_ids.push(tid);
                        }
                    }
                };
                pushChildren(el);
                if (el.shadowRoot) {
                    pushChildren(el.shadowRoot);
                }
                if (tag === 'IFRAME') {
                    try {
                        const cdoc = el.contentDocument;
                        if (cdoc && cdoc.body) {
                            const bl = parseFloat(style.borderLeftWidth) || 0;
                            const pl = parseFloat(style.paddingLeft) || 0;
                            const bt = parseFloat(style.borderTopWidth) || 0;
                            const pt = parseFloat(style.paddingTop) || 0;
                            const iw = el.offsetWidth;
                            const ih = el.offsetHeight;
                            const localSx = iw > 0 && rect.width > 0 ? rect.width / iw : 1;
                            const localSy = ih > 0 && rect.height > 0 ? rect.height / ih : 1;
                            const nextSx = scaleX * localSx;
                            const nextSy = scaleY * localSy;
                            const nextOffX = offX + (rect.x + (bl + pl) * localSx) * scaleX;
                            const nextOffY = offY + (rect.y + (bt + pt) * localSy) * scaleY;
                            const subFid = 'iframe:' + bid;
                            if (el.contentWindow) {
                                iframeWins.push([subFid, el.contentWindow]);
                            }
                            const cid = visit(cdoc.body, nodeId, subFid, nextOffX, nextOffY, nextSx, nextSy, depth + 1);
                            if (cid) entry.child_ids.push(cid);
                        }
                    } catch {}
                }
                return nodeId;
            };

            if (document.body) {
                visit(document.body, null, null, 0, 0, 1, 1, 0);
            }
            return { nodes, iframeWins };
        })()"#;

        let mut new_inline_owners: HashMap<String, String> = HashMap::new();
        let val = self.eval_isolated_with_window(walker_js, None).await?;
        let mut nodes = parse_bidi_walker_result(val, self.ctx(), &mut new_inline_owners);

        // Cross-origin child contexts (if any whose contentDocument wasn't accessible)
        let parent_map: HashMap<String, String> = frames
            .iter()
            .filter_map(|f| {
                f.parent_frame_id
                    .as_ref()
                    .map(|p| (f.frame_id.clone(), p.clone()))
            })
            .collect();
        let mut rect_cache: HashMap<String, HashMap<String, (bool, FrameTransform)>> = HashMap::new();

        for f in frames {
            if f.is_main {
                continue;
            }
            let already_has_frame = nodes.iter().any(|n| n.frame_id.as_deref() == Some(&f.frame_id));
            if already_has_frame {
                continue;
            }
            let (same_origin_walked, tf) = self
                .resolve_frame_cumulative_offset(&f.frame_id, &parent_map, &mut rect_cache)
                .await;
            if same_origin_walked {
                continue;
            }
            if let Ok(sub_val) = self
                .eval_isolated_with_window(walker_js, Some(&f.frame_id))
                .await
            {
                let sub_nodes =
                    parse_bidi_walker_result(sub_val, &f.frame_id, &mut new_inline_owners);
                let prefix = format!("{}::", f.frame_id);
                for mut sn in sub_nodes {
                    sn.node_id = format!("{prefix}{}", sn.node_id);
                    sn.parent_id = sn.parent_id.map(|p| format!("{prefix}{p}"));
                    sn.child_ids = sn
                        .child_ids
                        .into_iter()
                        .map(|c| format!("{prefix}{c}"))
                        .collect();
                    if let Some(ref mut b) = sn.bounds {
                        tf.apply(b);
                    }
                    nodes.push(sn);
                }
            }
        }

        *self.state.inline_frame_owners.lock() = new_inline_owners;
        Ok(nodes)
    }

    async fn resolve_node_box(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
    ) -> Result<BoundingBox> {
        let lookup_js = r#"(bid) => {
            const st = globalThis.__mimikyuAxState;
            const entry = st && st.idToEntry ? st.idToEntry.get(bid) : null;
            let el = entry && entry.ref ? entry.ref.deref() : null;
            if (!el && globalThis.__mimikyuAxMap) el = globalThis.__mimikyuAxMap.get(bid) || null;
            if (!el) return null;
            let r = (() => {
                const raw = el.getBoundingClientRect();
                if (raw.width > 0 || raw.height > 0) return raw;
                const doc = el.ownerDocument || document;
                const cs = (doc.defaultView || window).getComputedStyle(el);
                if (cs && cs.display === 'contents') {
                    try {
                        const range = doc.createRange();
                        range.selectNodeContents(el);
                        const rr = range.getBoundingClientRect();
                        if (rr && rr.width > 0 && rr.height > 0) return rr;
                    } catch {}
                }
                return raw;
            })();
            let bx = r.x;
            let by = r.y;
            let bw = r.width;
            let bh = r.height;
            try {
                let curWin = el.ownerDocument && el.ownerDocument.defaultView ? el.ownerDocument.defaultView : null;
                while (curWin && curWin !== window) {
                    const fe = curWin.frameElement;
                    if (!fe) break;
                    const fr = fe.getBoundingClientRect();
                    const cs = curWin.parent.getComputedStyle(fe);
                    const iw = fe.offsetWidth;
                    const ih = fe.offsetHeight;
                    const sx = iw > 0 && fr.width > 0 ? fr.width / iw : 1;
                    const sy = ih > 0 && fr.height > 0 ? fr.height / ih : 1;
                    const ox = fr.x + ((parseFloat(cs.borderLeftWidth) || 0) + (parseFloat(cs.paddingLeft) || 0)) * sx;
                    const oy = fr.y + ((parseFloat(cs.borderTopWidth) || 0) + (parseFloat(cs.paddingTop) || 0)) * sy;
                    bx = ox + bx * sx;
                    by = oy + by * sy;
                    bw *= sx;
                    bh *= sy;
                    curWin = curWin.parent;
                }
            } catch {}
            return { x: bx, y: by, width: bw, height: bh };
        }"#;
        let args = vec![json!({ "type": "number", "value": backend_node_id })];
        let mut val = Value::Null;
        let mut found_in_ctx: Option<String> = None;
        let mut last_err: Option<MimikyuError> = None;
        for cand_ctx in self.ax_sandbox_candidates(frame_id) {
            match self
                .call_sandbox_fn_in_frame(lookup_js, args.clone(), Some(&cand_ctx))
                .await
            {
                Ok(v) if !v.is_null() => {
                    val = v;
                    found_in_ctx = Some(cand_ctx);
                    break;
                }
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
        if val.is_null() {
            if let Some(err) = last_err {
                return Err(err);
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "backend_node_id {backend_node_id} not found"
            )));
        }
        let mut bbox = serde_json::from_value::<BoundingBox>(val).map_err(|e| {
            MimikyuError::ElementNotFound(format!("invalid bounding box for {backend_node_id}: {e}"))
        })?;
        if let Some(ref fid) = found_in_ctx
            && !fid.starts_with("iframe:")
            && fid != self.ctx()
        {
            let parent_map = self.conn.context_parents.read().clone();
            let mut has_chain_to_main = false;
            {
                let mut cur = fid.as_str();
                for _ in 0..32 {
                    if cur == self.ctx() {
                        has_chain_to_main = true;
                        break;
                    }
                    let Some(p) = parent_map.get(cur) else {
                        break;
                    };
                    cur = p.as_str();
                }
            }
            let parent_map = if has_chain_to_main {
                parent_map
            } else {
                self.list_frames()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|f| f.parent_frame_id.map(|p| (f.frame_id, p)))
                    .collect()
            };
            let mut cache = HashMap::new();
            let (_, tf) = self
                .resolve_frame_cumulative_offset(fid, &parent_map, &mut cache)
                .await;
            tf.apply(&mut bbox);
        }
        Ok(bbox)
    }

    async fn resolve_selector_box(&self, selector: &str) -> Result<BoundingBox> {
        let val = self
            .call_sandbox_fn(
                r#"(sel) => {
                    const el = document.querySelector(sel);
                    if (!el) return null;
                    let r = (() => {
                const raw = el.getBoundingClientRect();
                if (raw.width > 0 || raw.height > 0) return raw;
                const doc = el.ownerDocument || document;
                const cs = (doc.defaultView || window).getComputedStyle(el);
                if (cs && cs.display === 'contents') {
                    try {
                        const range = doc.createRange();
                        range.selectNodeContents(el);
                        const rr = range.getBoundingClientRect();
                        if (rr && rr.width > 0 && rr.height > 0) return rr;
                    } catch {}
                }
                return raw;
            })();
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

    async fn viewport_size(&self) -> Result<(f64, f64)> {
        let val = self
            .eval_isolated(
                r#"(() => {
                    const iw = window.innerWidth || 1280;
                    const ih = window.innerHeight || 800;
                    const cw = (document.documentElement && document.documentElement.clientWidth) || iw;
                    const ch = (document.documentElement && document.documentElement.clientHeight) || ih;
                    return { w: Math.min(iw, cw), h: Math.min(ih, ch) };
                })()"#,
            )
            .await?;
        let w = val["w"].as_f64().unwrap_or(1280.0).max(1.0);
        let h = val["h"].as_f64().unwrap_or(800.0).max(1.0);
        Ok((w, h))
    }

    async fn scrollbar_gutters(&self) -> Result<(f64, f64)> {
        let val = self
            .eval_isolated(
                r#"(() => {
                    const iw = window.innerWidth || 0;
                    const ih = window.innerHeight || 0;
                    const de = document.documentElement;
                    const cw = (de && de.clientWidth) || iw;
                    const ch = (de && de.clientHeight) || ih;
                    const se = document.scrollingElement || de;
                    if (!se) return { right: 0, bottom: 0 };
                    const deStyle = de ? getComputedStyle(de) : null;
                    const bodyStyle = document.body ? getComputedStyle(document.body) : null;
                    const deSb = deStyle ? deStyle.scrollbarWidth : 'auto';
                    const sbWidth = deSb || 'auto';
                    const ovY = deStyle && deStyle.overflowY !== 'visible'
                        ? deStyle.overflowY
                        : (bodyStyle ? bodyStyle.overflowY : 'visible');
                    const ovX = deStyle && deStyle.overflowX !== 'visible'
                        ? deStyle.overflowX
                        : (bodyStyle ? bodyStyle.overflowX : 'visible');
                    const hasV = cw >= iw && se.scrollHeight > ih + 1 && ovY !== 'hidden' && ovY !== 'clip' && sbWidth !== 'none';
                    const hasH = ch >= ih && se.scrollWidth > iw + 1 && ovX !== 'hidden' && ovX !== 'clip' && sbWidth !== 'none';
                    let right = 0;
                    if (hasV) {
                        if (sbWidth === 'thin') {
                            right = 12;
                            const probeY = Math.min(Math.max(Math.floor(ih / 2), 1), Math.max(ih - 1, 1));
                            const interiorHit = iw > 24 ? document.elementFromPoint(iw - 24, probeY) : null;
                            const canUseDeHit = interiorHit !== null && interiorHit !== de;
                            for (let d = 16; d >= 6; d--) {
                                if (iw > d) {
                                    const hit = document.elementFromPoint(iw - d, probeY);
                                    if (hit === null || (d <= 12 && canUseDeHit && hit === de)) {
                                        right = d;
                                        break;
                                    }
                                }
                            }
                        } else {
                            right = 16;
                        }
                    }
                    let bottom = 0;
                    if (hasH) {
                        if (sbWidth === 'thin') {
                            bottom = 12;
                            const probeX = Math.min(Math.max(Math.floor(iw / 2), 1), Math.max(iw - 1, 1));
                            const interiorHit = ih > 24 ? document.elementFromPoint(probeX, ih - 24) : null;
                            const canUseDeHit = interiorHit !== null && interiorHit !== de;
                            for (let d = 16; d >= 6; d--) {
                                if (ih > d) {
                                    const hit = document.elementFromPoint(probeX, ih - d);
                                    if (hit === null || (d <= 12 && canUseDeHit && hit === de)) {
                                        bottom = d;
                                        break;
                                    }
                                }
                            }
                        } else {
                            bottom = 16;
                        }
                    }
                    return { right, bottom };
                })()"#,
            )
            .await?;
        let right = val["right"].as_f64().unwrap_or(0.0).max(0.0);
        let bottom = val["bottom"].as_f64().unwrap_or(0.0).max(0.0);
        Ok((right, bottom))
    }

    async fn scroll_into_view(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<()> {
        if let Some(bid) = backend_node_id {
            let res = self
                .call_function_on_node(
                    bid,
                    frame_id,
                    "function() { this.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' }); return true; }",
                    vec![],
                )
                .await?;
            if res.as_bool() == Some(true) {
                return Ok(());
            }
        }
        if let Some(sel) = selector {
            let ok = self
                .call_sandbox_fn_in_frame(
                    r#"(sel) => {
                        const el = document.querySelector(sel);
                        if (!el) return false;
                        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
                        return true;
                    }"#,
                    vec![json!({ "type": "string", "value": sel })],
                    frame_id,
                )
                .await?;
            if ok.as_bool() == Some(true) {
                return Ok(());
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "selector '{sel}' matched no element"
            )));
        }
        Err(MimikyuError::InvalidConfig(
            "scroll_into_view requires backend_node_id or selector".to_string(),
        ))
    }

    async fn clear_node_value(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<()> {
        let clear_fn = r#"function() {
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
        }"#;
        if let Some(bid) = backend_node_id {
            let ok = self
                .call_function_on_node(bid, frame_id, clear_fn, vec![])
                .await?;
            if ok.as_bool() == Some(true) {
                return Ok(());
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "backend_node_id {bid} is not clearable"
            )));
        }
        if let Some(sel) = selector {
            let ok = self
                .call_sandbox_fn_in_frame(
                    r#"(sel) => {
                        const el = document.querySelector(sel);
                        if (!el) return false;
                        el.focus();
                        const tag = el.tagName;
                        if (tag === 'INPUT' || tag === 'TEXTAREA') {
                            const proto = tag === 'INPUT' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
                            const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
                            if (setter) setter.call(el, '');
                            else el.value = '';
                            el.dispatchEvent(new Event('input', { bubbles: true }));
                            return true;
                        }
                        if (el.isContentEditable) {
                            el.textContent = '';
                            el.dispatchEvent(new Event('input', { bubbles: true }));
                            return true;
                        }
                        return false;
                    }"#,
                    vec![json!({ "type": "string", "value": sel })],
                    frame_id,
                )
                .await?;
            if ok.as_bool() == Some(true) {
                return Ok(());
            }
            return Err(MimikyuError::ElementNotFound(format!(
                "selector '{sel}' matched no clearable element"
            )));
        }
        Err(MimikyuError::InvalidConfig(
            "clear_node_value requires backend_node_id or selector".to_string(),
        ))
    }

    async fn select_option(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
        by: SelectBy,
    ) -> Result<String> {
        let (mode, val_arg): (&str, Value) = match by {
            SelectBy::Value(v) => ("value", json!(v)),
            SelectBy::Label(l) => ("label", json!(l)),
            SelectBy::Index(i) => ("index", json!(i)),
            SelectBy::Auto(a) => ("auto", json!(a)),
        };
        let select_fn = r#"function(mode, target) {
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
        }"#;
        let res = if let Some(bid) = backend_node_id {
            self.call_function_on_node(bid, frame_id, select_fn, vec![json!(mode), val_arg])
                .await?
        } else if let Some(sel) = selector {
            let target_json = serde_json::to_string(&val_arg)?;
            self.call_sandbox_fn_in_frame(
                r#"(sel, mode, targetJson) => {
                    const el = document.querySelector(sel);
                    if (!el || el.tagName !== 'SELECT') {
                        return { ok: false, error: 'element is not a <select>' };
                    }
                    const target = JSON.parse(targetJson);
                    const opts = Array.from(el.options || []);
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
                    el.selectedIndex = idx;
                    opts[idx].selected = true;
                    el.dispatchEvent(new Event('input', { bubbles: true }));
                    el.dispatchEvent(new Event('change', { bubbles: true }));
                    return { ok: true, value: opts[idx].value };
                }"#,
                vec![
                    json!({ "type": "string", "value": sel }),
                    json!({ "type": "string", "value": mode }),
                    json!({ "type": "string", "value": target_json }),
                ],
                frame_id,
            )
            .await?
        } else {
            return Err(MimikyuError::InvalidConfig(
                "select_option requires backend_node_id or selector".to_string(),
            ));
        };
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
                    if (this.ownerDocument !== document) return null;
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
                        const st = globalThis.__mimikyuAxState;
                        const entry = st && st.idToEntry ? st.idToEntry.get(bid) : null;
                        let el = entry && entry.ref ? entry.ref.deref() : null;
                        if (!el && globalThis.__mimikyuAxMap) el = globalThis.__mimikyuAxMap.get(bid) || null;
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
        frame_id: Option<&str>,
        selector: Option<&str>,
        files: Vec<PathBuf>,
    ) -> Result<()> {
        let (target_ctx, elem) = self
            .resolve_shared_element_with_ctx(backend_node_id, frame_id, selector)
            .await?;
        let file_strs: Vec<String> = files
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        self.conn
            .send(
                "input.setFiles",
                json!({
                    "context": target_ctx,
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
        let x = (ev.x.round() as i64).max(0);
        let y = (ev.y.round() as i64).max(0);
        let clicks = ev.click_count.max(1);

        let pressed = *self.state.pressed_modifiers.lock();
        let missing = ev.modifiers & !pressed;
        if missing != 0 {
            self.send_modifier_key_actions(missing, true).await?;
        }

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

        let res = self
            .conn
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
            .await;

        if missing != 0 {
            let _ = self.send_modifier_key_actions(missing, false).await;
        }
        res?;
        Ok(())
    }

    async fn dispatch_key(&self, ev: KeyEvent) -> Result<()> {
        self.ensure_alive()?;
        let val = map_webdriver_key(&ev.key);
        let mod_bit = modifier_bit_for_key(&ev.key);
        let pressed = *self.state.pressed_modifiers.lock();
        let next_pressed = if mod_bit != 0 {
            match ev.event_type {
                KeyEventType::Down => pressed | mod_bit,
                KeyEventType::Up => pressed & !mod_bit,
                KeyEventType::Char => pressed,
            }
        } else {
            pressed
        };

        let missing = ev.modifiers & !next_pressed;
        let effective_mods = ev.modifiers | next_pressed;
        let non_text_mod_active = (effective_mods & (MOD_ALT | MOD_CONTROL | MOD_META)) != 0;
        let suppress_text_insert = non_text_mod_active
            && mod_bit == 0
            && ev.text.is_none()
            && matches!(ev.event_type, KeyEventType::Char | KeyEventType::Down);

        let mut actions = Vec::new();
        if missing != 0 {
            actions.extend(build_modifier_key_actions(missing, true));
        }
        match ev.event_type {
            KeyEventType::Down => actions.push(json!({ "type": "keyDown", "value": val })),
            KeyEventType::Up => actions.push(json!({ "type": "keyUp", "value": val })),
            KeyEventType::Char => {
                actions.push(json!({ "type": "keyDown", "value": val }));
                actions.push(json!({ "type": "keyUp", "value": val }));
            }
        }
        if missing != 0 {
            actions.extend(build_modifier_key_actions(missing, false));
        }

        let guard_frames: Vec<String> = if suppress_text_insert {
            let mut frames = self
                .list_frames()
                .await
                .map(|fs| fs.into_iter().map(|f| f.frame_id).collect::<Vec<_>>())
                .unwrap_or_default();
            let root = self.ctx().to_string();
            if !frames.iter().any(|f| f == &root) {
                frames.insert(0, root);
            }
            let install_js = r#"(() => {
                if (!globalThis.__mimikyuBeforeInputGuard) {
                    globalThis.__mimikyuBeforeInputGuard = (e) => {
                        if (e.inputType && e.inputType.startsWith('insert')) {
                            e.preventDefault();
                        }
                    };
                }
                window.addEventListener('beforeinput', globalThis.__mimikyuBeforeInputGuard, true);
            })()"#;
            for fid in &frames {
                let _ = self.eval_isolated_in_frame(install_js, Some(fid)).await;
            }
            frames
        } else {
            Vec::new()
        };

        let send_res = self
            .conn
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
            .await;

        if suppress_text_insert {
            let remove_js = r#"(() => {
                if (globalThis.__mimikyuBeforeInputGuard) {
                    window.removeEventListener('beforeinput', globalThis.__mimikyuBeforeInputGuard, true);
                }
            })()"#;
            for fid in &guard_frames {
                let _ = self.eval_isolated_in_frame(remove_js, Some(fid)).await;
            }
        }

        send_res?;
        if mod_bit != 0 {
            let mut guard = self.state.pressed_modifiers.lock();
            match ev.event_type {
                KeyEventType::Down => *guard |= mod_bit,
                KeyEventType::Up => *guard &= !mod_bit,
                KeyEventType::Char => {}
            }
        }
        Ok(())
    }

    async fn dispatch_wheel(&self, ev: WheelEvent) -> Result<()> {
        self.ensure_alive()?;
        let pressed = *self.state.pressed_modifiers.lock();
        let missing = ev.modifiers & !pressed;
        if missing != 0 {
            self.send_modifier_key_actions(missing, true).await?;
        }
        let res = self
            .conn
            .send(
                "input.performActions",
                json!({
                    "context": self.ctx(),
                    "actions": [{
                        "type": "wheel",
                        "id": "mimikyu_wheel",
                        "actions": [{
                            "type": "scroll",
                            "x": (ev.x.round() as i64).max(0),
                            "y": (ev.y.round() as i64).max(0),
                            "deltaX": ev.delta_x.round() as i64,
                            "deltaY": ev.delta_y.round() as i64,
                        }],
                    }],
                }),
            )
            .await;
        if missing != 0 {
            let _ = self.send_modifier_key_actions(missing, false).await;
        }
        res?;
        Ok(())
    }

    async fn insert_text(&self, text: &str) -> Result<()> {
        self.ensure_alive()?;
        if text.is_empty() {
            return Ok(());
        }
        let mut actions = Vec::with_capacity(text.chars().count() * 2);
        for ch in text.chars() {
            if ch == '\r' {
                continue;
            }
            let s = match ch {
                '\n' => "\u{E007}".to_string(),
                '\t' => "\u{E004}".to_string(),
                other => other.to_string(),
            };
            actions.push(json!({ "type": "keyDown", "value": s }));
            actions.push(json!({ "type": "keyUp", "value": s }));
        }
        if actions.is_empty() {
            return Ok(());
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
        let use_document_origin = opts.full_page || opts.clip.is_some();
        let mut params = json!({
            "context": self.ctx(),
            "origin": if use_document_origin { "document" } else { "viewport" },
        });
        params["format"] = match opts.format {
            ScreenshotFormat::Png => json!({ "type": "image/png" }),
            ScreenshotFormat::Jpeg { quality } => {
                let q = (f64::from(quality) / 100.0).clamp(0.0, 1.0);
                json!({ "type": "image/jpeg", "quality": q })
            }
        };
        if let Some(clip) = opts.clip
            && !opts.full_page
        {
            let scroll = self
                .eval_isolated("({ sx: window.scrollX || 0, sy: window.scrollY || 0 })")
                .await
                .unwrap_or(Value::Null);
            let sx = scroll["sx"].as_f64().unwrap_or(0.0);
            let sy = scroll["sy"].as_f64().unwrap_or(0.0);
            params["clip"] = json!({
                "type": "box",
                "x": (clip.x + sx).max(0.0),
                "y": (clip.y + sy).max(0.0),
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
        self.ensure_alive()?;
        self.state.anim_gen.fetch_add(1, Ordering::SeqCst);
        self.state
            .instant_animations
            .store(enabled, Ordering::SeqCst);

        let mut uses_uc = false;
        {
            let mut guard = self.state.anim_script_id.lock().await;
            if let Some(old_id) = guard.take() {
                let _ = self
                    .conn
                    .send("script.removePreloadScript", json!({ "script": old_id }))
                    .await;
            }
            self.state
                .anim_uses_user_context
                .store(false, Ordering::SeqCst);
            if enabled {
                let fn_decl = format!(
                    "() => {{\n{}\n}}",
                    BIDI_INSTANT_ANIM_ENABLE_SCRIPT
                );
                if self.state.owns_user_context
                    && let Some(ref uc) = self.state.user_context
                    && let Ok(res) = self
                        .conn
                        .send(
                            "script.addPreloadScript",
                            json!({
                                "functionDeclaration": fn_decl,
                                "userContexts": [uc],
                                "sandbox": ISOLATED_SANDBOX,
                            }),
                        )
                        .await
                    && let Some(sid) = res["script"].as_str()
                {
                    *guard = Some(sid.to_string());
                    self.state
                        .anim_uses_user_context
                        .store(true, Ordering::SeqCst);
                    uses_uc = true;
                }
                if !uses_uc
                    && let Ok(res) = self
                        .conn
                        .send(
                            "script.addPreloadScript",
                            json!({
                                "functionDeclaration": fn_decl,
                                "contexts": [self.ctx()],
                                "sandbox": ISOLATED_SANDBOX,
                            }),
                        )
                        .await
                    && let Some(sid) = res["script"].as_str()
                {
                    *guard = Some(sid.to_string());
                }
            }
        }

        let popup_cids: Vec<String> = self.state.popup_contexts.lock().iter().cloned().collect();
        {
            let mut pop_anim = self.state.popup_anim_scripts.lock().await;
            for (_, old_sid) in pop_anim.drain() {
                let _ = self
                    .conn
                    .send("script.removePreloadScript", json!({ "script": old_sid }))
                    .await;
            }
            if enabled && !uses_uc {
                let fn_decl = format!(
                    "() => {{\n{}\n}}",
                    BIDI_INSTANT_ANIM_ENABLE_SCRIPT
                );
                for pcid in &popup_cids {
                    if let Ok(res) = self
                        .conn
                        .send(
                            "script.addPreloadScript",
                            json!({
                                "functionDeclaration": fn_decl,
                                "contexts": [pcid],
                                "sandbox": ISOLATED_SANDBOX,
                            }),
                        )
                        .await
                        && let Some(sid) = res["script"].as_str()
                    {
                        if self.state.instant_animations.load(Ordering::SeqCst)
                            && !self.state.anim_uses_user_context.load(Ordering::SeqCst)
                        {
                            pop_anim.insert(pcid.clone(), sid.to_string());
                        } else {
                            let _ = self
                                .conn
                                .send("script.removePreloadScript", json!({ "script": sid }))
                                .await;
                        }
                    }
                }
            }
        }

        let mut all_contexts = Vec::with_capacity(1 + popup_cids.len());
        all_contexts.push(self.ctx().to_string());
        all_contexts.extend(popup_cids);

        let mut target_frames = Vec::new();
        let mut seen_frames = HashSet::new();
        for cid in &all_contexts {
            if seen_frames.insert(cid.clone()) {
                target_frames.push(cid.clone());
            }
            if let Ok(frames) = self.list_frames_for_context(cid).await {
                for f in frames {
                    if seen_frames.insert(f.frame_id.clone()) {
                        target_frames.push(f.frame_id);
                    }
                }
            }
        }

        let fn_decl = if enabled {
            format!("() => {{\n{}\n}}", BIDI_INSTANT_ANIM_ENABLE_SCRIPT)
        } else {
            BIDI_INSTANT_ANIM_DISABLE_FN.to_string()
        };
        for fid in target_frames {
            let _ = self
                .call_sandbox_fn_in_frame(&fn_decl, vec![], Some(&fid))
                .await;
        }
        Ok(())
    }

    async fn set_env_overrides(&self, profile: EnvProfile) -> Result<()> {
        self.ensure_alive()?;
        *self.state.env_profile.lock() = Some(profile.clone());

        let popup_cids: Vec<String> = self.state.popup_contexts.lock().iter().cloned().collect();
        let mut live_popups = Vec::with_capacity(popup_cids.len());

        let main_vp_res = self
            .conn
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
            .await;
        main_vp_res?;

        for pcid in popup_cids {
            let res = self
                .conn
                .send(
                    "browsingContext.setViewport",
                    json!({
                        "context": pcid,
                        "viewport": {
                            "width": profile.viewport_width,
                            "height": profile.viewport_height,
                        },
                        "devicePixelRatio": profile.device_scale_factor,
                    }),
                )
                .await;
            match res {
                Ok(_) => live_popups.push(pcid),
                Err(ref e) if is_stale_context_error(e) => {
                    self.state.popup_contexts.lock().remove(&pcid);
                }
                Err(_) => live_popups.push(pcid),
            }
        }

        let uc_scope = if self.state.owns_user_context {
            self.state.user_context.clone()
        } else {
            None
        };
        let mut use_uc = false;
        let win_script = build_window_init_script(&profile, EngineKind::Firefox);
        let fn_decl = format!("() => {{\n{win_script}\n}}");

        if let Some(ref uc) = uc_scope {
            let mut uc_ok = true;
            if !profile.timezone.is_empty()
                && self
                    .conn
                    .send(
                        "emulation.setTimezoneOverride",
                        json!({
                            "timezone": profile.timezone,
                            "userContexts": [uc],
                        }),
                    )
                    .await
                    .is_err()
            {
                uc_ok = false;
            }
            if uc_ok
                && !profile.locale.is_empty()
                && self
                    .conn
                    .send(
                        "emulation.setLocaleOverride",
                        json!({
                            "locale": profile.locale,
                            "userContexts": [uc],
                        }),
                    )
                    .await
                    .is_err()
            {
                uc_ok = false;
            }
            if uc_ok
                && let Some(ref geo) = profile.geolocation
                && self
                    .conn
                    .send(
                        "emulation.setGeolocationOverride",
                        json!({
                            "coordinates": {
                                "latitude": geo.latitude,
                                "longitude": geo.longitude,
                                "accuracy": geo.accuracy,
                            },
                            "userContexts": [uc],
                        }),
                    )
                    .await
                    .is_err()
            {
                uc_ok = false;
            }
            if uc_ok {
                let mut prev = self.state.env_script_id.lock().await;
                if let Some(old_id) = prev.take() {
                    let _ = self.remove_init_script(&old_id).await;
                }
                let mut prev_iso = self.state.env_isolated_script_id.lock().await;
                if let Some(old_id) = prev_iso.take() {
                    let _ = self.remove_init_script(&old_id).await;
                }
                let main_res = self
                    .conn
                    .send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "userContexts": [uc],
                        }),
                    )
                    .await;
                let iso_res = self
                    .conn
                    .send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "userContexts": [uc],
                            "sandbox": ISOLATED_SANDBOX,
                        }),
                    )
                    .await;
                match (main_res, iso_res) {
                    (Ok(mr), Ok(ir)) => {
                        if let (Some(mid), Some(iid)) =
                            (mr["script"].as_str(), ir["script"].as_str())
                        {
                            *prev = Some(mid.to_string());
                            *prev_iso = Some(iid.to_string());
                            use_uc = true;
                        }
                    }
                    (Ok(mr), Err(_)) => {
                        if let Some(mid) = mr["script"].as_str() {
                            let _ = self.remove_init_script(mid).await;
                        }
                    }
                    (Err(_), Ok(ir)) => {
                        if let Some(iid) = ir["script"].as_str() {
                            let _ = self.remove_init_script(iid).await;
                        }
                    }
                    (Err(_), Err(_)) => {}
                }
            }
        }

        self.state
            .env_uses_user_context
            .store(use_uc, Ordering::SeqCst);

        {
            let mut pop_scripts = self.state.popup_preload_scripts.lock().await;
            for (_, ids) in pop_scripts.drain() {
                for sid in ids {
                    let _ = self.remove_init_script(&sid).await;
                }
            }
        }

        if !use_uc {
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
                let mut kept = Vec::with_capacity(live_popups.len());
                for pcid in live_popups {
                    match self
                        .conn
                        .send(
                            "emulation.setTimezoneOverride",
                            json!({
                                "timezone": profile.timezone,
                                "contexts": [pcid],
                            }),
                        )
                        .await
                    {
                        Ok(_) => kept.push(pcid),
                        Err(ref e) if is_stale_context_error(e) => {
                            self.state.popup_contexts.lock().remove(&pcid);
                        }
                        Err(e) => return Err(e),
                    }
                }
                live_popups = kept;
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
                let mut kept = Vec::with_capacity(live_popups.len());
                for pcid in live_popups {
                    match self
                        .conn
                        .send(
                            "emulation.setLocaleOverride",
                            json!({
                                "locale": profile.locale,
                                "contexts": [pcid],
                            }),
                        )
                        .await
                    {
                        Ok(_) => kept.push(pcid),
                        Err(ref e) if is_stale_context_error(e) => {
                            self.state.popup_contexts.lock().remove(&pcid);
                        }
                        Err(e) => return Err(e),
                    }
                }
                live_popups = kept;
            }
            if let Some(ref geo) = profile.geolocation {
                let coords = json!({
                    "latitude": geo.latitude,
                    "longitude": geo.longitude,
                    "accuracy": geo.accuracy,
                });
                self.conn
                    .send(
                        "emulation.setGeolocationOverride",
                        json!({
                            "coordinates": coords,
                            "contexts": [self.ctx()],
                        }),
                    )
                    .await?;
                let mut kept = Vec::with_capacity(live_popups.len());
                for pcid in live_popups {
                    match self
                        .conn
                        .send(
                            "emulation.setGeolocationOverride",
                            json!({
                                "coordinates": coords,
                                "contexts": [pcid],
                            }),
                        )
                        .await
                    {
                        Ok(_) => kept.push(pcid),
                        Err(ref e) if is_stale_context_error(e) => {
                            self.state.popup_contexts.lock().remove(&pcid);
                        }
                        Err(e) => return Err(e),
                    }
                }
                live_popups = kept;
            }

            {
                let mut prev = self.state.env_script_id.lock().await;
                if let Some(old_id) = prev.take() {
                    let _ = self.remove_init_script(&old_id).await;
                }
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
                let new_id = res["script"]
                    .as_str()
                    .ok_or_else(|| MimikyuError::Protocol {
                        code: -1,
                        message: "script.addPreloadScript missing script id".to_string(),
                    })?
                    .to_string();
                *prev = Some(new_id);
            }
            {
                let mut prev_iso = self.state.env_isolated_script_id.lock().await;
                if let Some(old_id) = prev_iso.take() {
                    let _ = self.remove_init_script(&old_id).await;
                }
                let iso_res = self
                    .conn
                    .send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [self.ctx()],
                            "sandbox": ISOLATED_SANDBOX,
                        }),
                    )
                    .await?;
                if let Some(iso_id) = iso_res["script"].as_str() {
                    *prev_iso = Some(iso_id.to_string());
                }
            }

            let mut kept_popups = Vec::with_capacity(live_popups.len());
            for pcid in live_popups {
                let main_res = self
                    .conn
                    .send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [pcid],
                        }),
                    )
                    .await;
                let main_id = match main_res {
                    Ok(v) => v["script"].as_str().map(ToString::to_string),
                    Err(ref e) if is_stale_context_error(e) => {
                        self.state.popup_contexts.lock().remove(&pcid);
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let iso_res = self
                    .conn
                    .send(
                        "script.addPreloadScript",
                        json!({
                            "functionDeclaration": fn_decl,
                            "contexts": [pcid],
                            "sandbox": ISOLATED_SANDBOX,
                        }),
                    )
                    .await;
                let iso_id = match iso_res {
                    Ok(v) => v["script"].as_str().map(ToString::to_string),
                    Err(ref e) if is_stale_context_error(e) => {
                        if let Some(mid) = main_id {
                            let _ = self.remove_init_script(&mid).await;
                        }
                        self.state.popup_contexts.lock().remove(&pcid);
                        continue;
                    }
                    Err(e) => {
                        if let Some(mid) = main_id {
                            let _ = self.remove_init_script(&mid).await;
                        }
                        return Err(e);
                    }
                };
                let mut ids = Vec::new();
                if let Some(mid) = main_id {
                    ids.push(mid);
                }
                if let Some(iid) = iso_id {
                    ids.push(iid);
                }
                if !ids.is_empty() {
                    self.state
                        .popup_preload_scripts
                        .lock()
                        .await
                        .entry(pcid.clone())
                        .or_default()
                        .extend(ids);
                }
                kept_popups.push(pcid);
            }
            live_popups = kept_popups;
        }

        let mut all_contexts = Vec::with_capacity(1 + live_popups.len());
        all_contexts.push(self.ctx().to_string());
        all_contexts.extend(live_popups);

        let mut eval_contexts = Vec::with_capacity(all_contexts.len());
        let mut seen_eval_contexts = HashSet::with_capacity(all_contexts.len());
        for cid in &all_contexts {
            if seen_eval_contexts.insert(cid.clone()) {
                eval_contexts.push(cid.clone());
            }
            if let Ok(frames) = self.list_frames_for_context(cid).await {
                for f in frames {
                    if seen_eval_contexts.insert(f.frame_id.clone()) {
                        eval_contexts.push(f.frame_id);
                    }
                }
            }
        }
        for cid in &eval_contexts {
            let _ = self
                .conn
                .send(
                    "script.evaluate",
                    json!({
                        "expression": win_script,
                        "target": { "context": cid },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                )
                .await;
            let _ = self
                .conn
                .send(
                    "script.evaluate",
                    json!({
                        "expression": win_script,
                        "target": { "context": cid, "sandbox": ISOLATED_SANDBOX },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                )
                .await;
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
        let val = self.eval_isolated("document.readyState").await?;
        Ok(val.as_str().unwrap_or("loading").to_string())
    }

    async fn dispose(&self) -> Result<()> {
        if self.state.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.state.anim_gen.fetch_add(1, Ordering::SeqCst);
        self.state
            .instant_animations
            .store(false, Ordering::SeqCst);
        self.state
            .anim_uses_user_context
            .store(false, Ordering::SeqCst);
        self.conn.contexts.write().remove(self.ctx());

        if self.state.is_root {
            if let Some(ref uc) = self.state.user_context {
                self.conn.user_contexts.write().remove(uc);
            } else {
                self.conn.user_contexts.write().remove("default");
            }
            self.conn.shutdown_all().await;
        } else {
            let mut scripts_to_remove = Vec::new();
            if let Some(sid) = self.state.anim_script_id.lock().await.take() {
                scripts_to_remove.push(sid);
            }
            {
                let mut pop_anim = self.state.popup_anim_scripts.lock().await;
                for (_, sid) in pop_anim.drain() {
                    scripts_to_remove.push(sid);
                }
            }
            if let Some(sid) = self.state.env_script_id.lock().await.take() {
                scripts_to_remove.push(sid);
            }
            if let Some(sid) = self.state.env_isolated_script_id.lock().await.take() {
                scripts_to_remove.push(sid);
            }
            {
                let mut pop_env = self.state.popup_preload_scripts.lock().await;
                for (_, ids) in pop_env.drain() {
                    scripts_to_remove.extend(ids);
                }
            }
            if let Some((mid, iid)) = self.state.net_worker_scripts.lock().await.take() {
                if !mid.is_empty() {
                    scripts_to_remove.push(mid);
                }
                if !iid.is_empty() {
                    scripts_to_remove.push(iid);
                }
            }
            {
                let mut pop_net = self.state.popup_net_worker_scripts.lock().await;
                for (_, (mid, iid)) in pop_net.drain() {
                    if !mid.is_empty() {
                        scripts_to_remove.push(mid);
                    }
                    if !iid.is_empty() {
                        scripts_to_remove.push(iid);
                    }
                }
            }
            for sid in scripts_to_remove {
                let _ = self
                    .conn
                    .send("script.removePreloadScript", json!({ "script": sid }))
                    .await;
            }
            let _ = self
                .conn
                .sync_global_intercept(self.state.state_id, false)
                .await;

            if self.state.owns_user_context
                && let Some(ref uc) = self.state.user_context
            {
                self.conn.user_contexts.write().remove(uc);
                let mut removed_cids = Vec::new();
                let mut removed_states = Vec::new();
                self.conn.contexts.write().retain(|cid, st| {
                    if st.user_context.as_deref() == Some(uc.as_str()) {
                        st.disposed.store(true, Ordering::SeqCst);
                        removed_cids.push(cid.clone());
                        removed_states.push(Arc::clone(st));
                        false
                    } else {
                        true
                    }
                });
                for st in removed_states {
                    st.anim_gen.fetch_add(1, Ordering::SeqCst);
                    st.instant_animations.store(false, Ordering::SeqCst);
                    st.anim_uses_user_context.store(false, Ordering::SeqCst);
                    let mut child_scripts = Vec::new();
                    if let Some(sid) = st.anim_script_id.lock().await.take() {
                        child_scripts.push(sid);
                    }
                    {
                        let mut pop_anim = st.popup_anim_scripts.lock().await;
                        for (_, sid) in pop_anim.drain() {
                            child_scripts.push(sid);
                        }
                    }
                    if let Some(sid) = st.env_script_id.lock().await.take() {
                        child_scripts.push(sid);
                    }
                    if let Some(sid) = st.env_isolated_script_id.lock().await.take() {
                        child_scripts.push(sid);
                    }
                    {
                        let mut pop_env = st.popup_preload_scripts.lock().await;
                        for (_, ids) in pop_env.drain() {
                            child_scripts.extend(ids);
                        }
                    }
                    if let Some((mid, iid)) = st.net_worker_scripts.lock().await.take() {
                        if !mid.is_empty() {
                            child_scripts.push(mid);
                        }
                        if !iid.is_empty() {
                            child_scripts.push(iid);
                        }
                    }
                    {
                        let mut pop_net = st.popup_net_worker_scripts.lock().await;
                        for (_, (mid, iid)) in pop_net.drain() {
                            if !mid.is_empty() {
                                child_scripts.push(mid);
                            }
                            if !iid.is_empty() {
                                child_scripts.push(iid);
                            }
                        }
                    }
                    for sid in child_scripts {
                        let _ = self
                            .conn
                            .send("script.removePreloadScript", json!({ "script": sid }))
                            .await;
                    }
                    let _ = self.conn.sync_global_intercept(st.state_id, false).await;
                }
                self.conn.cleanup_destroyed_context(self.ctx());
                for cid in &removed_cids {
                    self.conn.cleanup_destroyed_context(cid);
                }
                let mut uc_cids = Vec::new();
                self.conn.context_user_contexts.write().retain(|cid, ctx_uc| {
                    if ctx_uc == uc {
                        uc_cids.push(cid.clone());
                        false
                    } else {
                        true
                    }
                });
                if !uc_cids.is_empty() {
                    let mut openers = self.conn.context_openers.write();
                    for cid in &uc_cids {
                        openers.remove(cid);
                    }
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
        }
        Ok(())
    }
}

fn is_stale_context_error(err: &MimikyuError) -> bool {
    match err {
        MimikyuError::Protocol { message, .. } => {
            let m = message.to_ascii_lowercase();
            m.contains("no such frame")
                || m.contains("unknown context")
                || m.contains("no such context")
                || (m.contains("browsing context") && m.contains("not found"))
        }
        _ => false,
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

fn parse_bidi_walker_result(
    val: Value,
    owner_ctx: &str,
    inline_owners: &mut HashMap<String, String>,
) -> Vec<AxNode> {
    let mut win_map: HashMap<String, String> = HashMap::new();
    if let Some(wins) = val.get("iframeWins").and_then(|v| v.as_array()) {
        for pair in wins {
            if let Some(arr) = pair.as_array()
                && arr.len() == 2
                && let (Some(sub_fid), Some(ctx)) =
                    (arr[0].as_str(), arr[1].get("context").and_then(|c| c.as_str()))
            {
                win_map.insert(sub_fid.to_string(), ctx.to_string());
                inline_owners.insert(ctx.to_string(), owner_ctx.to_string());
            }
        }
    }
    let arr = val
        .get("nodes")
        .and_then(|v| v.as_array())
        .or_else(|| val.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Ok(mut node) = serde_json::from_value::<AxNode>(item) {
            match node.frame_id.as_deref() {
                Some(fid) if fid.starts_with("iframe:") => {
                    if let Some(real_ctx) = win_map.get(fid) {
                        node.frame_id = Some(real_ctx.clone());
                    }
                }
                None => {
                    node.frame_id = Some(owner_ctx.to_string());
                }
                _ => {}
            }
            out.push(node);
        }
    }
    out
}

fn parse_bidi_script_result(res: &Value) -> Result<Value> {
    parse_bidi_script_result_inner(res, false)
}

fn parse_bidi_script_result_with_window(res: &Value) -> Result<Value> {
    parse_bidi_script_result_inner(res, true)
}

fn parse_bidi_script_result_inner(res: &Value, allow_window: bool) -> Result<Value> {
    if res.get("type").and_then(|v| v.as_str()) == Some("exception") {
        let text = res["exceptionDetails"]["text"]
            .as_str()
            .unwrap_or("JavaScript exception")
            .to_string();
        return Err(MimikyuError::EvalFailed(text));
    }
    let mut ancestors = HashSet::new();
    let mut seen = HashMap::new();
    bidi_remote_value_to_json(&res["result"], &mut ancestors, &mut seen, allow_window)
}

fn bidi_remote_value_to_json(
    val: &Value,
    ancestors: &mut HashSet<String>,
    seen: &mut HashMap<String, Value>,
    allow_window: bool,
) -> Result<Value> {
    let t = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match t {
        "undefined" | "null" => Ok(Value::Null),
        "boolean" => val
            .get("value")
            .and_then(|v| v.as_bool())
            .map(Value::Bool)
            .ok_or_else(|| {
                MimikyuError::EvalFailed("JavaScript evaluation returned invalid boolean value".into())
            }),
        "string" => val
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.to_string()))
            .ok_or_else(|| {
                MimikyuError::EvalFailed("JavaScript evaluation returned invalid string value".into())
            }),
        "number" => {
            if let Some(n) = val.get("value") {
                if n.is_number() {
                    return Ok(n.clone());
                }
                if let Some(s) = n.as_str() {
                    if s == "-0" {
                        return Ok(json!(0));
                    }
                    return Err(MimikyuError::EvalFailed(format!(
                        "JavaScript evaluation returned non-JSON-serializable value (number: {s})"
                    )));
                }
            }
            Err(MimikyuError::EvalFailed(
                "JavaScript evaluation returned non-JSON-serializable number".into(),
            ))
        }
        "bigint" => {
            let detail = val
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("bigint");
            Err(MimikyuError::EvalFailed(format!(
                "JavaScript evaluation returned non-JSON-serializable value (bigint: {detail})"
            )))
        }
        "window" if allow_window => val.get("value").cloned().ok_or_else(|| {
            MimikyuError::EvalFailed("JavaScript evaluation returned window without context".into())
        }),
        "array" => {
            let internal_id = val.get("internalId").and_then(|v| v.as_str());
            if let Some(id) = internal_id {
                if ancestors.contains(id) {
                    return Err(MimikyuError::EvalFailed(
                        "JavaScript evaluation returned cyclic value that cannot be serialized to JSON"
                            .into(),
                    ));
                }
                if val.get("value").is_none() {
                    if let Some(cached) = seen.get(id) {
                        return Ok(cached.clone());
                    }
                    return Err(MimikyuError::EvalFailed(
                        "JavaScript evaluation returned cyclic or unserializable collection reference"
                            .into(),
                    ));
                }
            }
            let Some(arr) = val.get("value").and_then(|v| v.as_array()) else {
                return Err(MimikyuError::EvalFailed(format!(
                    "JavaScript evaluation returned non-JSON-serializable {t} without value"
                )));
            };
            if let Some(id) = internal_id {
                ancestors.insert(id.to_string());
            }
            let mut items = Vec::with_capacity(arr.len());
            let mut err = None;
            for item in arr {
                match bidi_remote_value_to_json(item, ancestors, seen, allow_window) {
                    Ok(v) => items.push(v),
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            if let Some(id) = internal_id {
                ancestors.remove(id);
            }
            if let Some(e) = err {
                return Err(e);
            }
            let out = Value::Array(items);
            if let Some(id) = internal_id {
                seen.insert(id.to_string(), out.clone());
            }
            Ok(out)
        }
        "object" => {
            let internal_id = val.get("internalId").and_then(|v| v.as_str());
            if let Some(id) = internal_id {
                if ancestors.contains(id) {
                    return Err(MimikyuError::EvalFailed(
                        "JavaScript evaluation returned cyclic object that cannot be serialized to JSON"
                            .into(),
                    ));
                }
                if val.get("value").is_none() {
                    if let Some(cached) = seen.get(id) {
                        return Ok(cached.clone());
                    }
                    return Err(MimikyuError::EvalFailed(
                        "JavaScript evaluation returned cyclic or unserializable object reference"
                            .into(),
                    ));
                }
            }
            let Some(pairs) = val.get("value").and_then(|v| v.as_array()) else {
                return Err(MimikyuError::EvalFailed(format!(
                    "JavaScript evaluation returned non-JSON-serializable {t} without value"
                )));
            };
            if let Some(id) = internal_id {
                ancestors.insert(id.to_string());
            }
            let mut obj = serde_json::Map::new();
            let mut err = None;
            for pair in pairs {
                if let Some(pair_arr) = pair.as_array()
                    && pair_arr.len() == 2
                {
                    let key = if let Some(s) = pair_arr[0].as_str() {
                        s.to_string()
                    } else {
                        match bidi_remote_value_to_json(&pair_arr[0], ancestors, seen, allow_window)
                        {
                            Ok(v) => v
                                .as_str()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| v.to_string()),
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    };
                    if pair_arr[1].get("type").and_then(|v| v.as_str()) == Some("undefined") {
                        continue;
                    }
                    match bidi_remote_value_to_json(&pair_arr[1], ancestors, seen, allow_window) {
                        Ok(v) => {
                            obj.insert(key, v);
                        }
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
            }
            if let Some(id) = internal_id {
                ancestors.remove(id);
            }
            if let Some(e) = err {
                return Err(e);
            }
            let out = Value::Object(obj);
            if let Some(id) = internal_id {
                seen.insert(id.to_string(), out.clone());
            }
            Ok(out)
        }
        other => {
            let kind = if other.is_empty() { "unknown" } else { other };
            Err(MimikyuError::EvalFailed(format!(
                "JavaScript evaluation returned non-JSON-serializable value ({kind})"
            )))
        }
    }
}

fn is_bidi_sse_response(
    mime_type: Option<&str>,
    response_headers: &BTreeMap<String, String>,
) -> bool {
    mime_type.is_some_and(|m| m.to_ascii_lowercase().contains("text/event-stream"))
        || response_headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("content-type")
                && v.to_ascii_lowercase().contains("text/event-stream")
        })
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

fn normalize_download_url(raw: &str) -> String {
    raw.split('#').next().unwrap_or(raw).to_string()
}

fn extract_content_length(headers: &BTreeMap<String, String>) -> Option<u64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<u64>().ok())
}

fn extract_response_body_non_empty(resp: &Value, cl: Option<u64>) -> bool {
    if cl.is_some_and(|n| n > 0) {
        return true;
    }
    if resp
        .get("bodySize")
        .and_then(|v| v.as_u64())
        .is_some_and(|n| n > 0)
    {
        return true;
    }
    if resp
        .get("content")
        .and_then(|c| c.get("size"))
        .and_then(|v| v.as_u64())
        .is_some_and(|n| n > 0)
    {
        return true;
    }
    false
}

fn has_any_part_file(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".part"))
    })
}

fn file_inode(meta: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn extract_content_disposition_filename(headers: &BTreeMap<String, String>) -> Option<String> {
    let val = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-disposition"))?
        .1;
    for part in val.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed
            .strip_prefix("filename=")
            .or_else(|| trimmed.strip_prefix("FILENAME="))
        {
            let unquoted = rest.trim().trim_matches('"').trim_matches('\'');
            if let Some(base) = std::path::Path::new(unquoted)
                .file_name()
                .and_then(|s| s.to_str())
                && !base.is_empty()
            {
                return Some(base.to_string());
            }
        }
    }
    None
}

fn strip_firefox_collision_suffix(filename: &str) -> Option<String> {
    let path = std::path::Path::new(filename);
    let stem = path.file_stem()?.to_str()?;
    let ext = path.extension().and_then(|s| s.to_str());
    let open_idx = stem.rfind('(')?;
    if !stem.ends_with(')') || open_idx == 0 {
        return None;
    }
    let digits = &stem[open_idx + 1..stem.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let base_stem = &stem[..open_idx];
    if base_stem.is_empty() {
        return None;
    }
    Some(match ext {
        Some(e) => format!("{base_stem}.{e}"),
        None => base_stem.to_string(),
    })
}

fn unique_dest_file(dir: &std::path::Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let p = std::path::Path::new(filename);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(filename);
    let ext = p.extension().and_then(|s| s.to_str());
    for n in 1..10_000 {
        let next_name = match ext {
            Some(e) => format!("{stem}({n}).{e}"),
            None => format!("{stem}({n})"),
        };
        let next = dir.join(next_name);
        if !next.exists() {
            return next;
        }
    }
    candidate
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

const BIDI_OBJECT_FALLBACK_VALIDATOR: &str = r#"(rootVal) => {
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
    return JSON.stringify(walk(rootVal));
}"#;

const BIDI_INSTANT_ANIM_ENABLE_SCRIPT: &str = r#"
const css = '*, *::before, *::after { transition-duration: 0s !important; transition-delay: 0s !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; scroll-behavior: auto !important; }';
const apply = () => {
    try {
        if (document.adoptedStyleSheets && typeof window.CSSStyleSheet === 'function') {
            if (!globalThis.__mimikyuAnimSheet || globalThis.__mimikyuAnimSheetDoc !== document) {
                const sheet = new window.CSSStyleSheet();
                sheet.replaceSync(css);
                globalThis.__mimikyuAnimSheet = sheet;
                globalThis.__mimikyuAnimSheetDoc = document;
            }
            const cur = Array.from(document.adoptedStyleSheets);
            if (!cur.includes(globalThis.__mimikyuAnimSheet)) {
                document.adoptedStyleSheets = [...cur, globalThis.__mimikyuAnimSheet];
            }
            return;
        }
    } catch {}
    const host = document.head || document.documentElement;
    if (host) {
        if (
            !globalThis.__mimikyuAnimEl ||
            !globalThis.__mimikyuAnimEl.isConnected ||
            globalThis.__mimikyuAnimEl.ownerDocument !== document ||
            !document.contains(globalThis.__mimikyuAnimEl)
        ) {
            const st = document.createElement('style');
            st.textContent = css;
            host.appendChild(st);
            globalThis.__mimikyuAnimEl = st;
        }
    }
};
if (document.documentElement) apply();
else document.addEventListener('DOMContentLoaded', apply, { once: true });
try {
    for (const anim of document.getAnimations()) {
        const t = anim.effect && anim.effect.getComputedTiming ? anim.effect.getComputedTiming() : null;
        if (!t || Number.isFinite(t.iterations)) {
            try { anim.finish(); } catch {}
        }
    }
} catch {}
"#;

const BIDI_INSTANT_ANIM_DISABLE_FN: &str = r#"() => {
    const css = '*, *::before, *::after { transition-duration: 0s !important; transition-delay: 0s !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; scroll-behavior: auto !important; }';
    try {
        if (document.adoptedStyleSheets) {
            document.adoptedStyleSheets = Array.from(document.adoptedStyleSheets).filter(s => {
                if (globalThis.__mimikyuAnimSheet && s === globalThis.__mimikyuAnimSheet) return false;
                try {
                    return !(s.cssRules && s.cssRules.length > 0 && s.cssRules[0].cssText.includes('animation-iteration-count: 1'));
                } catch {
                    return true;
                }
            });
            globalThis.__mimikyuAnimSheet = null;
        }
    } catch {}
    if (globalThis.__mimikyuAnimEl) {
        try { globalThis.__mimikyuAnimEl.remove(); } catch {}
        globalThis.__mimikyuAnimEl = null;
    }
    try {
        for (const st of Array.from(document.querySelectorAll('style'))) {
            if (st.textContent === css) st.remove();
        }
    } catch {}
}"#;

fn collect_bidi_frames(ctx_node: &Value, parent_id: Option<&str>, out: &mut Vec<FrameInfo>) {
    let Some(cid) = ctx_node["context"].as_str() else {
        return;
    };
    let url = ctx_node["url"].as_str().unwrap_or_default().to_string();
    out.push(FrameInfo {
        frame_id: cid.to_string(),
        parent_frame_id: parent_id.map(ToString::to_string),
        name: None,
        url,
        is_main: parent_id.is_none(),
    });
    if let Some(children) = ctx_node["children"].as_array() {
        for child in children {
            collect_bidi_frames(child, Some(cid), out);
        }
    }
}

fn modifier_bit_for_key(key: &str) -> u32 {
    match key {
        "Alt" => MOD_ALT,
        "Control" => MOD_CONTROL,
        "Meta" => MOD_META,
        "Shift" => MOD_SHIFT,
        _ => 0,
    }
}

fn build_modifier_key_actions(mask: u32, down: bool) -> Vec<Value> {
    let mut mods = Vec::new();
    if mask & MOD_CONTROL != 0 {
        mods.push("\u{E009}");
    }
    if mask & MOD_ALT != 0 {
        mods.push("\u{E00A}");
    }
    if mask & MOD_SHIFT != 0 {
        mods.push("\u{E008}");
    }
    if mask & MOD_META != 0 {
        mods.push("\u{E03D}");
    }
    if !down {
        mods.reverse();
    }
    let kind = if down { "keyDown" } else { "keyUp" };
    mods.into_iter()
        .map(|val| json!({ "type": kind, "value": val }))
        .collect()
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

    #[test]
    fn frame_transform_composes_nested_scale_and_offset() {
        // Outer iframe content origin at (20, 10) with scale (2.0, 2.0)
        let outer = FrameTransform {
            ox: 20.0,
            oy: 10.0,
            sx: 2.0,
            sy: 2.0,
        };
        // Inner iframe content origin at (39, 15) inside outer, with scale (1.5, 1.5)
        let inner = FrameTransform {
            ox: 39.0,
            oy: 15.0,
            sx: 1.5,
            sy: 1.5,
        };
        let total = outer.compose_inner(inner.compose_inner(FrameTransform::identity()));
        let mut bbox = BoundingBox {
            x: 10.0,
            y: 4.0,
            width: 50.0,
            height: 20.0,
        };
        total.apply(&mut bbox);
        // Expected x = 20 + (39 + 10 * 1.5) * 2 = 128
        // Expected y = 10 + (15 + 4 * 1.5) * 2 = 52
        // Expected w = 50 * 1.5 * 2 = 150, h = 20 * 1.5 * 2 = 60
        assert!((bbox.x - 128.0).abs() < 1e-6);
        assert!((bbox.y - 52.0).abs() < 1e-6);
        assert!((bbox.width - 150.0).abs() < 1e-6);
        assert!((bbox.height - 60.0).abs() < 1e-6);
    }

    #[test]
    fn parse_bidi_script_result_matches_cdp_eval_semantics() {
        assert_eq!(
            parse_bidi_script_result(&json!({
                "type": "success",
                "result": { "type": "undefined" }
            }))
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            parse_bidi_script_result(&json!({
                "type": "success",
                "result": { "type": "null" }
            }))
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            parse_bidi_script_result(&json!({
                "type": "success",
                "result": { "type": "number", "value": "-0" }
            }))
            .unwrap(),
            json!(0)
        );

        // Shared DAG reference (same internalId appearing in sibling properties) is valid JSON
        let dag = parse_bidi_script_result(&json!({
            "type": "success",
            "result": {
                "type": "object",
                "value": [
                    ["a", { "type": "object", "internalId": "shared", "value": [["x", { "type": "number", "value": 7 }]] }],
                    ["b", { "type": "object", "internalId": "shared" }]
                ]
            }
        }))
        .unwrap();
        assert_eq!(dag, json!({ "a": { "x": 7 }, "b": { "x": 7 } }));

        // Cyclic object via ancestor internalId must fail
        let cyclic_err = parse_bidi_script_result(&json!({
            "type": "success",
            "result": {
                "type": "object",
                "internalId": "1",
                "value": [
                    ["x", { "type": "number", "value": 1 }],
                    ["self", { "type": "object", "internalId": "1" }]
                ]
            }
        }))
        .expect_err("cyclic object must fail");
        assert!(matches!(cyclic_err, MimikyuError::EvalFailed(_)));

        // Non-JSON-serializable values must fail in public eval
        for bad in [
            json!({ "type": "success", "result": { "type": "bigint", "value": "42" } }),
            json!({ "type": "success", "result": { "type": "number", "value": "NaN" } }),
            json!({ "type": "success", "result": { "type": "number", "value": "Infinity" } }),
            json!({ "type": "success", "result": { "type": "number", "value": "-Infinity" } }),
            json!({ "type": "success", "result": { "type": "symbol" } }),
            json!({ "type": "success", "result": { "type": "function" } }),
            json!({ "type": "success", "result": { "type": "object" } }),
            json!({ "type": "success", "result": { "type": "map", "value": [["k", { "type": "string", "value": "v" }]] } }),
            json!({ "type": "success", "result": { "type": "set", "value": [{ "type": "number", "value": 1 }] } }),
            json!({ "type": "success", "result": { "type": "date", "value": "2026-01-01T00:00:00.000Z" } }),
            json!({ "type": "success", "result": { "type": "regexp", "value": { "pattern": "a" } } }),
            json!({ "type": "success", "result": { "type": "error" } }),
            json!({ "type": "success", "result": { "type": "node", "value": {} } }),
            json!({ "type": "success", "result": { "type": "window", "value": { "context": "ctx-1" } } }),
        ] {
            let err = parse_bidi_script_result(&bad)
                .expect_err(&format!("expected EvalFailed for {bad}"));
            assert!(matches!(err, MimikyuError::EvalFailed(_)));
        }

        // Internal walker/frame-rect helpers retain window context handles
        let win_val = parse_bidi_script_result_with_window(&json!({
            "type": "success",
            "result": { "type": "window", "value": { "context": "ctx-1" } }
        }))
        .unwrap();
        assert_eq!(win_val, json!({ "context": "ctx-1" }));
    }

    fn make_test_conn(pool_dir: PathBuf) -> Arc<BidiConnection> {
        let (write_tx, _write_rx) = mpsc::unbounded_channel();
        Arc::new(BidiConnection {
            next_id: AtomicU64::new(1),
            write_tx,
            pending: Mutex::new(HashMap::new()),
            contexts: RwLock::new(HashMap::new()),
            context_parents: RwLock::new(HashMap::new()),
            context_openers: RwLock::new(HashMap::new()),
            context_user_contexts: RwLock::new(HashMap::new()),
            user_contexts: RwLock::new(HashMap::new()),
            context_requests: RwLock::new(HashMap::new()),
            request_stores: RwLock::new(HashMap::new()),
            isolated_contexts: RwLock::new(HashSet::new()),
            active_targets: RwLock::new(HashMap::new()),
            global_intercept_id: AsyncMutex::new(None),
            intercept_owners: Mutex::new(HashSet::new()),
            pool_download_dir: pool_dir,
            active_pool_downloads: Arc::new(Mutex::new(HashSet::new())),
            recent_content_disposition: Mutex::new(HashMap::new()),
            download_transfers_by_req_id: Mutex::new(HashMap::new()),
            pending_download_req_ids: Mutex::new(HashMap::new()),
            download_ends_by_nav: Mutex::new(HashMap::new()),
            download_end_supported: AtomicBool::new(false),
            process: AsyncMutex::new(None),
            _root_proxy_relay: Mutex::new(None),
            closed: AtomicBool::new(false),
            io_tasks: Mutex::new(Vec::new()),
        })
    }

    fn make_test_ctx_state(uc: Option<&str>, dl_dir: Option<PathBuf>) -> Arc<BidiContextState> {
        Arc::new(BidiContextState {
            state_id: NEXT_BIDI_STATE_ID.fetch_add(1, Ordering::Relaxed),
            user_context: uc.map(ToString::to_string),
            owns_user_context: uc.is_some(),
            is_root: false,
            main_world_eval: false,
            telemetry_capacity: None,
            telemetry: Arc::new(TelemetryStore::new(TelemetryConfig::default())),
            interceptor: Mutex::new(NetworkInterceptor::default()),
            env_headers: Mutex::new(BTreeMap::new()),
            env_profile: Mutex::new(None),
            env_uses_user_context: AtomicBool::new(false),
            env_script_id: AsyncMutex::new(None),
            env_isolated_script_id: AsyncMutex::new(None),
            download_dir: dl_dir,
            pending_permissions: Mutex::new(Vec::new()),
            applied_permissions: Mutex::new(Vec::new()),
            pending_storage_scripts: Mutex::new(Vec::new()),
            inline_frame_owners: Mutex::new(HashMap::new()),
            popup_contexts: Mutex::new(HashSet::new()),
            popup_preload_scripts: AsyncMutex::new(HashMap::new()),
            popup_anim_scripts: AsyncMutex::new(HashMap::new()),
            net_channel: "nws_test".to_string(),
            net_worker_scripts: AsyncMutex::new(None),
            popup_net_worker_scripts: AsyncMutex::new(HashMap::new()),
            _proxy_relay: Mutex::new(None),
            pressed_modifiers: Mutex::new(0),
            instant_animations: AtomicBool::new(false),
            anim_uses_user_context: AtomicBool::new(false),
            anim_gen: AtomicU64::new(0),
            anim_script_id: AsyncMutex::new(None),
            load_notify: Notify::new(),
            disposed: AtomicBool::new(false),
        })
    }

    #[test]
    fn resolve_context_state_handles_popups_cycles_and_non_cascading_cleanup() {
        let conn = make_test_conn(PathBuf::from("/tmp/pool"));
        let dl1 = PathBuf::from("/tmp/dl1");
        let st = make_test_ctx_state(Some("uc-1"), Some(dl1.clone()));
        conn.contexts.write().insert("tab-1".into(), Arc::clone(&st));
        conn.user_contexts.write().insert("uc-1".into(), Arc::clone(&st));

        // Simulate browsingContext.contextCreated for popup + iframe inside popup
        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.contextCreated",
                "params": {
                    "context": "popup-1",
                    "parent": Value::Null,
                    "originalOpener": "tab-1",
                    "userContext": "uc-1"
                }
            })
            .to_string(),
        );
        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.contextCreated",
                "params": {
                    "context": "popup-frame-1",
                    "parent": "popup-1",
                    "originalOpener": Value::Null,
                    "userContext": "uc-1"
                }
            })
            .to_string(),
        );

        assert_eq!(
            conn.resolve_context_state("popup-1")
                .and_then(|s| s.download_dir.clone()),
            Some(dl1.clone())
        );
        assert_eq!(
            conn.resolve_context_state("popup-frame-1")
                .and_then(|s| s.download_dir.clone()),
            Some(dl1.clone())
        );

        // Cycle guard must terminate without infinite loop
        conn.context_openers
            .write()
            .insert("cyc-a".into(), "cyc-b".into());
        conn.context_openers
            .write()
            .insert("cyc-b".into(), "cyc-a".into());
        assert!(conn.resolve_context_state("cyc-a").is_none());

        // Closing opener tab-1 must NOT cascade-remove popup-1; popup-1 still resolves via user_contexts
        conn.contexts.write().remove("tab-1");
        conn.cleanup_destroyed_context("tab-1");
        assert!(conn.context_openers.read().contains_key("popup-1"));
        assert_eq!(
            conn.resolve_context_state("popup-1")
                .and_then(|s| s.download_dir.clone()),
            Some(dl1)
        );
    }

    #[test]
    fn record_download_network_event_isolates_concurrent_same_url_req_ids() {
        let conn = make_test_conn(PathBuf::from("/tmp/pool"));
        let url = "http://127.0.0.1:9999/flaky.bin";

        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-1"),
            &json!({ "request": { "request": "req-1", "url": url } }),
        );
        conn.record_download_network_event(
            "network.responseStarted",
            Some("cid-1"),
            &json!({
                "request": { "request": "req-1", "url": url },
                "response": { "headers": [{ "name": "Content-Length", "value": { "type": "string", "value": "4096" } }] }
            }),
        );

        // Second concurrent transfer to exact same (cid, url) fails before downloadWillBegin:
        // req-2 is pruned from pending_download_req_ids while remaining Failed in download_transfers_by_req_id
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-1"),
            &json!({ "request": { "request": "req-2", "url": url } }),
        );
        conn.record_download_network_event(
            "network.fetchError",
            Some("cid-1"),
            &json!({ "request": { "request": "req-2", "url": url }, "errorText": "NS_ERROR_NET_RESET" }),
        );

        // Pending queue retains only req-1 (req-2 was pruned on fetchError so it cannot poison future downloads)
        {
            let mut pending = conn.pending_download_req_ids.lock();
            let q = pending
                .get_mut(&("cid-1".to_string(), url.to_string()))
                .expect("pending queue");
            assert_eq!(q.pop_front().unwrap().req_id, "req-1");
            assert!(q.is_empty());
        }

        // req-1 remains InProgress(4096) while req-2 is Failed
        let transfers = conn.download_transfers_by_req_id.lock();
        let st1 = transfers.get("req-1").expect("req-1 state");
        assert_eq!(st1.status, DownloadTransferStatus::InProgress);
        assert_eq!(st1.content_length, Some(4096));
        let st2 = transfers.get("req-2").expect("req-2 state");
        assert_eq!(st2.status, DownloadTransferStatus::Failed);
        drop(transfers);

        // Completing req-1 transitions req-1 cleanly to Completed(4096)
        conn.record_download_network_event(
            "network.responseCompleted",
            Some("cid-1"),
            &json!({
                "request": { "request": "req-1", "url": url },
                "response": { "headers": [{ "name": "Content-Length", "value": { "type": "string", "value": "4096" } }] }
            }),
        );
        let st1_done = conn
            .download_transfers_by_req_id
            .lock()
            .get("req-1")
            .cloned()
            .expect("req-1 completed");
        assert_eq!(st1_done.status, DownloadTransferStatus::Completed);
        assert_eq!(st1_done.content_length, Some(4096));
    }

    #[test]
    fn non_download_fetch_xhr_and_loaded_nav_do_not_poison_download_queue() {
        let conn = make_test_conn(PathBuf::from("/tmp/pool"));
        let url = "http://127.0.0.1:9999/asset.bin";

        // 1. Aborted and completed fetch()/XHR requests must never enter pending_download_req_ids
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-1"),
            &json!({
                "request": { "request": "fetch-1", "url": url, "destination": "" },
                "initiator": { "type": "fetch" }
            }),
        );
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-1"),
            &json!({
                "request": { "request": "xhr-1", "url": url, "destination": "" },
                "initiator": { "type": "xmlhttprequest" }
            }),
        );
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-1"),
            &json!({
                "request": { "request": "img-1", "url": url, "destination": "image" },
                "initiator": { "type": "other" }
            }),
        );
        assert!(conn.pending_download_req_ids.lock().is_empty());

        // 2. A completed non-attachment page navigation pruned on browsingContext.load
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-1"),
            &json!({
                "request": { "request": "nav-1", "url": url, "destination": "document" },
                "navigation": "nav-id-1"
            }),
        );
        conn.record_download_network_event(
            "network.responseCompleted",
            Some("cid-1"),
            &json!({
                "request": { "request": "nav-1", "url": url },
                "response": { "headers": [{ "name": "Content-Type", "value": { "type": "string", "value": "text/html" } }] }
            }),
        );
        assert_eq!(conn.pending_download_req_ids.lock().len(), 1);
        conn.prune_loaded_non_download_candidates("cid-1", url);
        assert!(conn.pending_download_req_ids.lock().is_empty());
    }

    #[test]
    fn bidi_handle_incoming_resolves_lone_surrogate_and_fails_malformed_id_immediately() {
        let conn = make_test_conn(PathBuf::from("/tmp/pool"));
        let (tx1, mut rx1) = tokio::sync::oneshot::channel();
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        conn.pending.lock().insert(101, tx1);
        conn.pending.lock().insert(102, tx2);

        conn.handle_incoming(r#"{"type":"success","id":101,"result":{"type":"string","value":"\ud800"}}"#);
        let res1 = rx1.try_recv().expect("101 must resolve immediately").unwrap();
        assert_eq!(res1["value"], "\u{FFFD}");

        conn.handle_incoming(r#"{"type":"success","id":102,"result":BROKEN}"#);
        let res2 = rx2.try_recv().expect("102 must fail immediately");
        assert!(matches!(res2, Err(MimikyuError::Protocol { .. })));
    }

    #[tokio::test]
    async fn zero_byte_download_waits_for_part_finalization_and_download_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool_dir = tmp.path().join("pool");
        let dest_dir = tmp.path().join("dest");
        std::fs::create_dir_all(&pool_dir).unwrap();

        let conn = make_test_conn(pool_dir.clone());
        let st = make_test_ctx_state(Some("uc-dl"), Some(dest_dir.clone()));
        conn.contexts.write().insert("cid-dl".into(), Arc::clone(&st));
        conn.user_contexts.write().insert("uc-dl".into(), Arc::clone(&st));

        // 1. 0-byte placeholder + random Firefox .part file: must NOT relocate until .part moves onto target
        let url1 = "http://127.0.0.1:9999/zero1.bin";
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-dl"),
            &json!({ "request": { "request": "r1", "url": url1 }, "navigation": "nav-1" }),
        );
        conn.record_download_network_event(
            "network.responseCompleted",
            Some("cid-dl"),
            &json!({
                "request": { "request": "r1", "url": url1 },
                "response": { "headers": [{ "name": "Content-Length", "value": { "type": "string", "value": "0" } }] }
            }),
        );
        let target1 = pool_dir.join("zero1.bin");
        let random_part = pool_dir.join("xY9zAb12.bin.part");
        std::fs::write(&target1, b"").unwrap();
        std::fs::write(&random_part, b"").unwrap();

        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.downloadWillBegin",
                "params": {
                    "context": "cid-dl",
                    "navigation": "nav-1",
                    "url": url1,
                    "suggestedFilename": "zero1.bin"
                }
            })
            .to_string(),
        );

        // Must stay in pool_dir while random .part file exists (> old 30ms window)
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert!(
            !dest_dir.join("zero1.bin").exists(),
            "must not relocate 0-byte placeholder while .part still exists"
        );

        // Simulate Firefox IOUtils.move(partFilePath, targetPath) replacing inode
        std::fs::rename(&random_part, &target1).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !dest_dir.join("zero1.bin").exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(dest_dir.join("zero1.bin").exists(), "zero1.bin should relocate after .part finalization");
        assert!(!target1.exists(), "pool_dir must not retain zero1.bin");

        // 2. browsingContext.downloadEnd(canceled) cleans up placeholder without relocating
        let url2 = "http://127.0.0.1:9999/zero2.bin";
        let target2 = pool_dir.join("zero2.bin");
        std::fs::write(&target2, b"").unwrap();
        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.downloadWillBegin",
                "params": {
                    "context": "cid-dl",
                    "navigation": "nav-2",
                    "url": url2,
                    "suggestedFilename": "zero2.bin"
                }
            })
            .to_string(),
        );
        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.downloadEnd",
                "params": {
                    "context": "cid-dl",
                    "navigation": "nav-2",
                    "status": "canceled"
                }
            })
            .to_string(),
        );
        let deadline2 = tokio::time::Instant::now() + Duration::from_secs(2);
        while target2.exists() && tokio::time::Instant::now() < deadline2 {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(!target2.exists(), "canceled placeholder must be cleaned up");
        assert!(!dest_dir.join("zero2.bin").exists(), "canceled download must not be relocated");
    }

    #[tokio::test]
    async fn nonzero_or_chunked_download_never_relocates_zero_byte_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool_dir = tmp.path().join("pool");
        let dest_dir = tmp.path().join("dest");
        std::fs::create_dir_all(&pool_dir).unwrap();

        let conn = make_test_conn(pool_dir.clone());
        conn.download_end_supported.store(true, Ordering::SeqCst);
        let st = make_test_ctx_state(Some("uc-chunk"), Some(dest_dir.clone()));
        conn.contexts.write().insert("cid-chunk".into(), Arc::clone(&st));
        conn.user_contexts
            .write()
            .insert("uc-chunk".into(), Arc::clone(&st));

        // Chunked 4096-byte HTTP download (no Content-Length header, bodySize = 4096)
        // where Firefox pre-creates a 0-byte placeholder before opening/renaming `.part`.
        let url = "http://127.0.0.1:9999/chunked4k.bin";
        conn.record_download_network_event(
            "network.beforeRequestSent",
            Some("cid-chunk"),
            &json!({ "request": { "request": "r-chunk", "url": url }, "navigation": "nav-chunk" }),
        );
        conn.record_download_network_event(
            "network.responseCompleted",
            Some("cid-chunk"),
            &json!({
                "request": { "request": "r-chunk", "url": url },
                "response": {
                    "headers": [],
                    "bodySize": 4096,
                    "content": { "size": 4096 }
                }
            }),
        );

        let placeholder = pool_dir.join("chunked4k.bin");
        std::fs::write(&placeholder, b"").unwrap();

        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.downloadWillBegin",
                "params": {
                    "context": "cid-chunk",
                    "navigation": "nav-chunk",
                    "url": url,
                    "suggestedFilename": "chunked4k.bin"
                }
            })
            .to_string(),
        );

        // Even past the 250ms zero-byte fallback window (and even if downloadEnd arrives
        // slightly before IOUtils.move completes on disk), 0-byte placeholder must NOT relocate.
        conn.handle_incoming(
            &json!({
                "type": "event",
                "method": "browsingContext.downloadEnd",
                "params": {
                    "context": "cid-chunk",
                    "navigation": "nav-chunk",
                    "status": "complete"
                }
            })
            .to_string(),
        );
        tokio::time::sleep(Duration::from_millis(320)).await;
        assert!(
            !dest_dir.join("chunked4k.bin").exists(),
            "must never relocate 0-byte placeholder when response body is non-empty"
        );

        // Finalize `.part` -> target with 4096 bytes
        let part_file = pool_dir.join("rnd8slug.bin.part");
        let payload = vec![0xABu8; 4096];
        std::fs::write(&part_file, &payload).unwrap();
        std::fs::rename(&part_file, &placeholder).unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !dest_dir.join("chunked4k.bin").exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let relocated = std::fs::read(dest_dir.join("chunked4k.bin")).expect("read chunked4k.bin");
        assert_eq!(relocated.len(), 4096);
        assert_eq!(relocated, payload);
    }
}
