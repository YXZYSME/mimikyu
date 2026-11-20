pub mod bidi;
pub mod cdp;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{MimikyuError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Chromium,
    Firefox,
}

impl fmt::Display for EngineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineKind::Chromium => write!(f, "chromium"),
            EngineKind::Firefox => write!(f, "firefox"),
        }
    }
}

impl FromStr for EngineKind {
    type Err = MimikyuError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chromium" | "chrome" => Ok(EngineKind::Chromium),
            "firefox" | "gecko" => Ok(EngineKind::Firefox),
            other => Err(MimikyuError::InvalidConfig(format!(
                "unknown engine '{other}' (expected 'chromium' or 'firefox')"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "path", rename_all = "snake_case")]
pub enum ProfileMode {
    EphemeralDir,
    Persistent(PathBuf),
    VirtualContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub server: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub bypass: Vec<String>,
}

impl ProxyConfig {
    /// Returns true if the proxy carries credentials requiring the local `ProxyRelay`.
    pub fn needs_relay(&self) -> bool {
        if self
            .username
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty())
            || self.password.is_some()
        {
            return true;
        }
        if let Ok(parsed) = parse_proxy_url(&self.server) {
            return !parsed.username().is_empty() || parsed.password().is_some();
        }
        false
    }

    /// Validate and normalize a user-supplied proxy URL.
    ///
    /// Schemeless values (`host:port` or `user:pass@host:port`) must specify an explicit `:port`
    /// and are normalized to `http://...` so downstream `url::Url::parse` calls never misread
    /// `user:` or `host:` as a URL scheme.
    pub fn parse_normalized(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(MimikyuError::InvalidConfig(
                "proxy URL cannot be empty or contain whitespace/control characters".to_string(),
            ));
        }

        let normalized = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            let host_port = trimmed
                .rsplit_once('@')
                .map(|(_, hp)| hp)
                .unwrap_or(trimmed);
            let valid_host_port = if let Some(rest) = host_port.strip_prefix('[') {
                rest.rsplit_once("]:")
                    .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|n| n > 0))
            } else {
                !host_port.contains('[')
                    && !host_port.contains(']')
                    && host_port.rsplit_once(':').is_some_and(|(h, p)| {
                        !h.is_empty() && p.parse::<u16>().is_ok_and(|n| n > 0)
                    })
            };
            if !valid_host_port {
                return Err(MimikyuError::InvalidConfig(format!(
                    "invalid proxy '{trimmed}': schemeless proxies must be host:port or user:pass@host:port"
                )));
            }
            format!("http://{trimmed}")
        };

        let parsed = url::Url::parse(&normalized)?;
        let scheme = parsed.scheme().to_ascii_lowercase();
        if !matches!(
            scheme.as_str(),
            "http" | "https" | "socks" | "socks4" | "socks5" | "socks5h"
        ) {
            return Err(MimikyuError::InvalidConfig(format!(
                "unsupported proxy scheme '{}' in '{trimmed}'",
                parsed.scheme()
            )));
        }
        if parsed.host_str().is_none_or(|h| h.trim().is_empty()) {
            return Err(MimikyuError::InvalidConfig(format!(
                "proxy '{trimmed}' is missing a host"
            )));
        }
        if parsed.port() == Some(0) {
            return Err(MimikyuError::InvalidConfig(format!(
                "proxy '{trimmed}' has invalid port 0"
            )));
        }

        Ok(Self {
            server: normalized,
            username: None,
            password: None,
            bypass: Vec::new(),
        })
    }
}

/// Parse a proxy URL, normalizing schemeless `host:port` / `user:pass@host:port` to `http://...`.
pub(crate) fn parse_proxy_url(raw: &str) -> std::result::Result<url::Url, url::ParseError> {
    let trimmed = raw.trim();
    if trimmed.contains("://") {
        url::Url::parse(trimmed)
    } else {
        url::Url::parse(&format!("http://{trimmed}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionState {
    Granted,
    Denied,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub name: String,
    pub state: PermissionState,
    #[serde(default)]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebRtcPolicy {
    #[default]
    Default,
    DisableNonProxiedUdp,
    DisableAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAgentBrand {
    pub brand: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAgentMetadata {
    pub brands: Vec<UserAgentBrand>,
    pub full_version_list: Vec<UserAgentBrand>,
    pub platform: String,
    pub platform_version: String,
    pub architecture: String,
    pub model: String,
    pub mobile: bool,
    pub bitness: String,
    pub wow64: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeolocationConfig {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
}

/// Pure-data environment consistency configuration (populated/validated by M2 `env_profile`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvProfile {
    pub os_family: String,
    pub user_agent: String,
    pub platform: String,
    pub accept_language: String,
    pub locale: String,
    pub languages: Vec<String>,
    pub timezone: String,
    pub user_agent_metadata: Option<UserAgentMetadata>,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub screen_width: u32,
    pub screen_height: u32,
    pub device_scale_factor: f64,
    pub is_mobile: bool,
    pub hardware_concurrency: u32,
    pub device_memory_gb: Option<f64>,
    pub webgl_vendor: Option<String>,
    pub webgl_renderer: Option<String>,
    pub audio_sample_rate: Option<u32>,
    #[serde(default)]
    pub audio_base_latency: Option<f64>,
    #[serde(default)]
    pub audio_channel_count: Option<u32>,
    pub geolocation: Option<GeolocationConfig>,
    pub webrtc_policy: WebRtcPolicy,
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    pub disable_automation_controlled: bool,
}

impl Default for EnvProfile {
    fn default() -> Self {
        Self {
            os_family: "macos".to_string(),
            user_agent: String::new(),
            platform: "MacIntel".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "UTC".to_string(),
            user_agent_metadata: None,
            viewport_width: 1280,
            viewport_height: 800,
            screen_width: 1280,
            screen_height: 800,
            device_scale_factor: 1.0,
            is_mobile: false,
            hardware_concurrency: 8,
            device_memory_gb: Some(8.0),
            webgl_vendor: None,
            webgl_renderer: None,
            audio_sample_rate: None,
            audio_base_latency: None,
            audio_channel_count: None,
            geolocation: None,
            webrtc_policy: WebRtcPolicy::Default,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOptions {
    pub engine: EngineKind,
    pub binary_path: Option<PathBuf>,
    pub headless: bool,
    pub profile_mode: ProfileMode,
    pub proxy: Option<ProxyConfig>,
    pub env_profile: Option<EnvProfile>,
    pub block_images: bool,
    pub block_media: bool,
    pub block_fonts: bool,
    pub instant_animations: bool,
    pub main_world_eval: bool,
    pub launch_timeout_ms: u64,
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub extra_prefs: BTreeMap<String, Value>,
    #[serde(default)]
    pub telemetry_capacity: Option<usize>,
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            engine: EngineKind::Chromium,
            binary_path: None,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            proxy: None,
            env_profile: None,
            block_images: false,
            block_media: false,
            block_fonts: false,
            instant_animations: false,
            main_world_eval: false,
            launch_timeout_ms: 15_000,
            extra_args: Vec::new(),
            extra_prefs: BTreeMap::new(),
            telemetry_capacity: None,
            download_dir: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextOptions {
    pub proxy: Option<ProxyConfig>,
    pub env_profile: Option<EnvProfile>,
    pub block_images: bool,
    pub block_media: bool,
    pub block_fonts: bool,
    pub instant_animations: bool,
    pub main_world_eval: bool,
    #[serde(default)]
    pub permissions: Vec<PermissionGrant>,
    #[serde(default)]
    pub telemetry_capacity: Option<usize>,
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
}

pub const MOD_ALT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_META: u32 = 4;
pub const MOD_SHIFT: u32 = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalOptions {
    pub main_world: bool,
    pub frame_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub frame_id: String,
    pub parent_frame_id: Option<String>,
    pub name: Option<String>,
    pub url: String,
    pub is_main: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowMatch {
    pub backend_node_id: Option<i64>,
    pub frame_id: Option<String>,
    pub tag_name: String,
    pub text: String,
    pub html: Option<String>,
    pub attributes: BTreeMap<String, String>,
    pub bounds: Option<BoundingBox>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", content = "value", rename_all = "snake_case")]
pub enum SelectBy {
    Value(String),
    Label(String),
    Index(usize),
    Auto(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverCapabilities {
    pub engine: EngineKind,
    pub protocol: String,
    pub supports_virtual_context: bool,
    pub supports_per_context_proxy: bool,
    pub supports_native_ax_tree: bool,
    pub supports_isolated_world: bool,
    pub supports_closed_shadow_dom: bool,
    pub supports_pdf: bool,
    pub supports_network_intercept: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub engine: EngineKind,
    pub browser_version: String,
    pub context_id: Option<String>,
    pub target_id: String,
    pub profile_dir: Option<PathBuf>,
    pub ws_endpoint: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavInfo {
    pub url: String,
    pub title: String,
    pub status: Option<u16>,
    pub frame_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl BoundingBox {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn is_visible(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxNode {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub child_ids: Vec<String>,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub description: Option<String>,
    pub backend_node_id: Option<i64>,
    pub frame_id: Option<String>,
    pub ignored: bool,
    pub focusable: bool,
    pub focused: bool,
    pub disabled: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    pub required: bool,
    pub level: Option<u32>,
    pub bounds: Option<BoundingBox>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseEventType {
    Move,
    Down,
    Up,
    Click,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseEvent {
    pub event_type: MouseEventType,
    pub x: f64,
    pub y: f64,
    pub button: MouseButton,
    pub click_count: u32,
    pub modifiers: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEventType {
    Down,
    Up,
    Char,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEvent {
    pub event_type: KeyEventType,
    pub key: String,
    pub code: Option<String>,
    pub text: Option<String>,
    pub windows_virtual_key_code: Option<u32>,
    pub modifiers: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WheelEvent {
    pub x: f64,
    pub y: f64,
    pub delta_x: f64,
    pub delta_y: f64,
    pub modifiers: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScreenshotFormat {
    #[default]
    Png,
    Jpeg {
        quality: u8,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScreenshotOpts {
    pub format: ScreenshotFormat,
    pub full_page: bool,
    pub clip: Option<BoundingBox>,
    pub omit_background: bool,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub max_height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteAction {
    Abort,
    Continue,
    Fulfill {
        status: u16,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    },
    ContinueWithHeaders {
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteMock {
    pub id: String,
    pub url_pattern: String,
    pub action: RouteAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: Option<f64>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OriginStorage {
    pub origin: String,
    pub local_storage: BTreeMap<String, String>,
    pub session_storage: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageState {
    pub cookies: Vec<Cookie>,
    pub origins: Vec<OriginStorage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub timestamp_ms: u64,
    pub level: String,
    pub text: String,
    pub url: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageError {
    pub timestamp_ms: u64,
    pub message: String,
    pub name: Option<String>,
    pub stack: Option<String>,
    pub url: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEvent {
    pub timestamp_ms: u64,
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub resource_type: String,
    pub status: Option<u16>,
    pub mime_type: Option<String>,
    pub failed: bool,
    pub error_text: Option<String>,
    pub request_headers: BTreeMap<String, String>,
    pub response_headers: BTreeMap<String, String>,
}

#[async_trait]
pub trait BrowserDriver: Send + Sync {
    fn kind(&self) -> EngineKind;
    fn capabilities(&self) -> DriverCapabilities;
    async fn launch(&self, opts: LaunchOptions) -> Result<Arc<dyn DriverSession>>;
}

#[async_trait]
pub trait DriverSession: Send + Sync {
    // Metadata & virtual contexts / tabs
    fn info(&self) -> &SessionInfo;
    fn capabilities(&self) -> DriverCapabilities;
    async fn new_context(&self, opts: ContextOptions) -> Result<Arc<dyn DriverSession>>;
    async fn new_tab(&self, url: Option<&str>) -> Result<Arc<dyn DriverSession>>;
    async fn list_tabs(&self) -> Result<Vec<TabInfo>>;
    async fn activate_tab(&self, target_id: &str) -> Result<()>;

    // Navigation
    async fn navigate(&self, url: &str) -> Result<NavInfo>;
    async fn reload(&self) -> Result<NavInfo>;
    async fn go_back(&self) -> Result<NavInfo>;
    async fn go_forward(&self) -> Result<NavInfo>;
    async fn current_url(&self) -> Result<String>;
    async fn title(&self) -> Result<String>;

    // Execution (isolated by default; main-world opt-in)
    async fn eval_isolated(&self, expr: &str) -> Result<Value>;
    async fn eval_main_world(&self, expr: &str) -> Result<Value>;
    async fn list_frames(&self) -> Result<Vec<FrameInfo>>;
    async fn eval_isolated_in_frame(&self, expr: &str, frame_id: Option<&str>) -> Result<Value>;
    async fn eval_main_world_in_frame(&self, expr: &str, frame_id: Option<&str>) -> Result<Value>;
    async fn eval_with_options(&self, expr: &str, opts: &EvalOptions) -> Result<Value> {
        if opts.main_world {
            self.eval_main_world_in_frame(expr, opts.frame_id.as_deref())
                .await
        } else {
            self.eval_isolated_in_frame(expr, opts.frame_id.as_deref())
                .await
        }
    }
    async fn call_function_on_node(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
        function_declaration: &str,
        args: Vec<Value>,
    ) -> Result<Value>;
    async fn query_shadow(
        &self,
        selector_chain: &[String],
        frame_id: Option<&str>,
    ) -> Result<Vec<ShadowMatch>>;
    async fn closed_shadow_hosts(&self, frame_id: Option<&str>) -> Result<Vec<i64>>;
    async fn add_init_script(&self, script: &str) -> Result<String>;
    async fn remove_init_script(&self, script_id: &str) -> Result<()>;

    // Accessibility, Geometry & File Input
    async fn accessibility_tree(&self) -> Result<Vec<AxNode>>;
    async fn resolve_node_box(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
    ) -> Result<BoundingBox>;
    async fn resolve_selector_box(&self, selector: &str) -> Result<BoundingBox>;
    async fn viewport_size(&self) -> Result<(f64, f64)>;
    async fn scrollbar_gutters(&self) -> Result<(f64, f64)> {
        Ok((0.0, 0.0))
    }
    async fn viewport_and_gutters(&self) -> Result<(f64, f64, f64, f64)> {
        let (vp_w, vp_h) = self.viewport_size().await?;
        let (gr, gb) = self.scrollbar_gutters().await.unwrap_or((0.0, 0.0));
        Ok((vp_w, vp_h, gr, gb))
    }
    async fn scroll_into_view(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<()>;
    async fn clear_node_value(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
    ) -> Result<()>;
    async fn select_option(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
        by: SelectBy,
    ) -> Result<String>;
    async fn focus_node(&self, backend_node_id: Option<i64>, selector: Option<&str>) -> Result<()>;
    async fn set_file_input_files(
        &self,
        backend_node_id: Option<i64>,
        frame_id: Option<&str>,
        selector: Option<&str>,
        files: Vec<PathBuf>,
    ) -> Result<()>;
    async fn css_selector_for_node(
        &self,
        backend_node_id: i64,
        frame_id: Option<&str>,
    ) -> Result<Option<String>>;

    // Chokepoint Input Primitives
    async fn dispatch_mouse(&self, ev: MouseEvent) -> Result<()>;
    async fn dispatch_key(&self, ev: KeyEvent) -> Result<()>;
    async fn dispatch_wheel(&self, ev: WheelEvent) -> Result<()>;
    async fn insert_text(&self, text: &str) -> Result<()>;

    // Media & Animations
    async fn screenshot(&self, opts: ScreenshotOpts) -> Result<Vec<u8>>;
    async fn pdf(&self) -> Result<Vec<u8>>;
    async fn set_instant_animations(&self, enabled: bool) -> Result<()>;

    // Environment, Permissions & Network Control (M2)
    async fn set_env_overrides(&self, profile: EnvProfile) -> Result<()>;
    async fn set_permissions(&self, permissions: Vec<PermissionGrant>) -> Result<()>;
    async fn reset_permissions(&self) -> Result<()>;
    async fn set_block_policy(&self, policy: crate::network::BlockPolicy) -> Result<()>;
    async fn set_blocked_urls(&self, patterns: Vec<String>) -> Result<()>;
    async fn set_route_mock(&self, mock: RouteMock) -> Result<()>;
    async fn set_route_rule(&self, rule: crate::network::RouteRule) -> Result<()>;
    async fn clear_route_mocks(&self) -> Result<()>;

    // Cookies & Storage (M2)
    async fn cookies(&self) -> Result<Vec<Cookie>>;
    async fn set_cookies(&self, cookies: Vec<Cookie>) -> Result<()>;
    async fn clear_cookies(&self) -> Result<()>;
    async fn storage_state(&self) -> Result<StorageState>;
    async fn set_storage_state(&self, state: StorageState) -> Result<()>;
    fn pending_storage_origins(&self) -> Vec<String> {
        Vec::new()
    }

    // Telemetry & Wait Helpers (M2/M3)
    async fn console_logs(&self) -> Result<Vec<ConsoleEntry>>;
    async fn page_errors(&self) -> Result<Vec<PageError>>;
    async fn network_events(&self) -> Result<Vec<NetworkEvent>>;
    async fn drain_console_logs(&self) -> Result<Vec<ConsoleEntry>>;
    async fn drain_page_errors(&self) -> Result<Vec<PageError>>;
    async fn drain_network_events(&self) -> Result<Vec<NetworkEvent>>;
    async fn clear_telemetry(&self) -> Result<()>;
    async fn inflight_requests(&self) -> Result<usize>;
    async fn inflight_activity(&self, ignore_long_lived: bool) -> Result<(usize, u64)>;
    async fn document_ready_state(&self) -> Result<String>;

    // Lifecycle
    async fn dispose(&self) -> Result<()>;
}

pub fn driver_for(engine: EngineKind) -> Arc<dyn BrowserDriver> {
    match engine {
        EngineKind::Chromium => Arc::new(cdp::CdpDriver::new()),
        EngineKind::Firefox => Arc::new(bidi::BidiDriver::new()),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Simple glob/substring matcher (`*` matches any sequence of characters, `?` matches one char).
/// Delegates to `crate::network::UrlPattern` (also supporting `exact:`, `prefix:`, `regex:`).
#[allow(dead_code)]
pub(crate) fn url_matches_pattern(pattern: &str, url: &str) -> bool {
    crate::network::UrlPattern::parse(pattern)
        .map(|p| p.matches(url))
        .unwrap_or(false)
}

fn parse_hex4(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 4 {
        return None;
    }
    let mut val = 0u16;
    for &b in &bytes[..4] {
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        val = (val << 4) | u16::from(nibble);
    }
    Some(val)
}

/// Replaces unpaired UTF-16 surrogate `\uXXXX` escapes inside JSON string literals
/// with `\uFFFD`, while preserving valid high+low surrogate pairs and escaped backslashes.
pub(crate) fn sanitize_json_lone_surrogates(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut in_string = false;
    let mut i = 0;
    let mut last_copy = 0;

    while i < len {
        let b = bytes[i];
        if !in_string {
            if b == b'"' {
                in_string = true;
            }
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = false;
            i += 1;
            continue;
        }

        if b == b'\\' {
            if i + 6 <= len
                && bytes[i + 1] == b'u'
                && let Some(u1) = parse_hex4(&bytes[i + 2..i + 6])
            {
                if (0xD800..=0xDBFF).contains(&u1) {
                    if i + 12 <= len
                        && bytes[i + 6] == b'\\'
                        && bytes[i + 7] == b'u'
                        && let Some(u2) = parse_hex4(&bytes[i + 8..i + 12])
                        && (0xDC00..=0xDFFF).contains(&u2)
                    {
                        i += 12;
                        continue;
                    }
                    out.push_str(&raw[last_copy..i]);
                    out.push_str("\\uFFFD");
                    i += 6;
                    last_copy = i;
                    continue;
                }
                if (0xDC00..=0xDFFF).contains(&u1) {
                    out.push_str(&raw[last_copy..i]);
                    out.push_str("\\uFFFD");
                    i += 6;
                    last_copy = i;
                    continue;
                }
                i += 6;
                continue;
            }
            i = (i + 2).min(len);
            continue;
        }

        i += 1;
    }

    if last_copy == 0 {
        return raw.to_string();
    }
    out.push_str(&raw[last_copy..]);
    out
}

/// Best-effort extraction of top-level `"id": <u64>` from malformed JSON-RPC / BiDi frames
/// so pending callers fail fast instead of stalling until RPC_TIMEOUT.
fn extract_top_level_json_id(raw: &str) -> Option<u64> {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut depth = 0usize;
    let mut in_string = false;

    while i < len {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                i = (i + 2).min(len);
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                if depth == 1 && i + 4 <= len && &bytes[i..i + 4] == b"\"id\"" {
                    let mut j = i + 4;
                    while j < len && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < len && bytes[j] == b':' {
                        j += 1;
                        while j < len && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        let num_start = j;
                        while j < len && bytes[j].is_ascii_digit() {
                            j += 1;
                        }
                        if j > num_start
                            && (j == len
                                || bytes[j] == b','
                                || bytes[j] == b'}'
                                || bytes[j].is_ascii_whitespace())
                            && let Ok(id) = raw[num_start..j].parse::<u64>()
                        {
                            return Some(id);
                        }
                    }
                }
                in_string = true;
                i += 1;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Parse an incoming CDP/BiDi JSON frame, sanitizing unpaired UTF-16 surrogates when needed.
pub(crate) fn parse_protocol_json(
    raw: &str,
) -> std::result::Result<Value, (Option<u64>, serde_json::Error)> {
    if let Ok(val) = serde_json::from_str::<Value>(raw) {
        return Ok(val);
    }
    let sanitized = sanitize_json_lone_surrogates(raw);
    serde_json::from_str::<Value>(&sanitized).map_err(|err| (extract_top_level_json_id(raw), err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_kind_round_trip() {
        assert_eq!(
            "chromium".parse::<EngineKind>().unwrap(),
            EngineKind::Chromium
        );
        assert_eq!(
            "chrome".parse::<EngineKind>().unwrap(),
            EngineKind::Chromium
        );
        assert_eq!(
            "firefox".parse::<EngineKind>().unwrap(),
            EngineKind::Firefox
        );
        assert_eq!("gecko".parse::<EngineKind>().unwrap(), EngineKind::Firefox);
        assert!("webkit".parse::<EngineKind>().is_err());
    }

    #[test]
    fn url_matches_pattern_handles_globs_and_substrings() {
        assert!(url_matches_pattern(
            "*blocked.png*",
            "http://127.0.0.1:8080/blocked.png"
        ));
        assert!(url_matches_pattern(
            "*/api/mock*",
            "http://127.0.0.1:8080/api/mock?x=1"
        ));
        assert!(!url_matches_pattern(
            "*/api/mock*",
            "http://127.0.0.1:8080/api/real"
        ));
        assert!(url_matches_pattern(
            "example.com",
            "https://example.com/path"
        ));
    }

    #[test]
    fn bounding_box_center_and_visibility() {
        let b = BoundingBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 40.0,
        };
        assert_eq!(b.center(), (60.0, 40.0));
        assert!(b.is_visible());
        assert!(
            !BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 10.0,
            }
            .is_visible()
        );
    }

    #[test]
    fn proxy_config_needs_relay_handles_schemeless_credentials() {
        for raw in [
            "user:pass@127.0.0.1:8080",
            "123:pass@proxy.local:8080",
            "user@host:8080",
            ":pass@host:8080",
            "http://user:pass@127.0.0.1:8080",
            "socks5h://u:p@127.0.0.1:1080",
        ] {
            let cfg = ProxyConfig {
                server: raw.to_string(),
                username: None,
                password: None,
                bypass: Vec::new(),
            };
            assert!(cfg.needs_relay(), "expected needs_relay=true for {raw}");
        }

        for raw in [
            "127.0.0.1:8080",
            "http://127.0.0.1:8080",
            "socks5://127.0.0.1:1080",
        ] {
            let cfg = ProxyConfig {
                server: raw.to_string(),
                username: None,
                password: None,
                bypass: Vec::new(),
            };
            assert!(!cfg.needs_relay(), "expected needs_relay=false for {raw}");
        }
    }

    #[test]
    fn proxy_config_parse_normalized_accepts_schemeless_and_rejects_malformed() {
        let p1 = ProxyConfig::parse_normalized("127.0.0.1:8080").unwrap();
        assert_eq!(p1.server, "http://127.0.0.1:8080");
        assert!(!p1.needs_relay());

        let p2 = ProxyConfig::parse_normalized("user:pass@proxy.local:8080").unwrap();
        assert_eq!(p2.server, "http://user:pass@proxy.local:8080");
        assert!(p2.needs_relay());

        let p3 = ProxyConfig::parse_normalized("[::1]:8080").unwrap();
        assert_eq!(p3.server, "http://[::1]:8080");

        let p4 = ProxyConfig::parse_normalized("socks5h://u:p@127.0.0.1:1080").unwrap();
        assert_eq!(p4.server, "socks5h://u:p@127.0.0.1:1080");
        assert!(p4.needs_relay());

        for bad in [
            "",
            "   ",
            "not-a-valid-proxy",
            "ftp://127.0.0.1:9999",
            "http://127.0.0.1:0",
            "proxy.local:0",
            "user:pass@proxy.local",
        ] {
            assert!(
                ProxyConfig::parse_normalized(bad).is_err(),
                "expected error for invalid proxy {bad:?}"
            );
        }
    }

    #[test]
    fn parse_protocol_json_sanitizes_lone_surrogates_and_preserves_valid_pairs_and_escapes() {
        // 1. Lone high surrogate -> U+FFFD
        let v1 = parse_protocol_json(r#"{"id":1,"result":{"value":"a\ud800b"}}"#).unwrap();
        assert_eq!(v1["result"]["value"], "a\u{FFFD}b");

        // 2. Lone low surrogate -> U+FFFD
        let v2 = parse_protocol_json(r#"{"id":2,"result":{"value":"\uDC00"}}"#).unwrap();
        assert_eq!(v2["result"]["value"], "\u{FFFD}");

        // 3. Reversed pair (low then high) -> two U+FFFD
        let v3 = parse_protocol_json(r#"{"id":3,"result":{"value":"\udc00\ud800"}}"#).unwrap();
        assert_eq!(v3["result"]["value"], "\u{FFFD}\u{FFFD}");

        // 4. Valid surrogate pair -> preserved as emoji
        let v4 = parse_protocol_json(r#"{"id":4,"result":{"value":"\ud83d\ude00"}}"#).unwrap();
        assert_eq!(v4["result"]["value"], "😀");

        // 5. High + valid surrogate pair -> first replaced, pair preserved
        let v5 =
            parse_protocol_json(r#"{"id":5,"result":{"value":"\ud800\uD83D\uDE00"}}"#).unwrap();
        assert_eq!(v5["result"]["value"], "\u{FFFD}😀");

        // 6. Escaped backslash before ud800 -> literal "\ud800" preserved
        let v6 = parse_protocol_json(r#"{"id":6,"result":{"value":"\\ud800"}}"#).unwrap();
        assert_eq!(v6["result"]["value"], "\\ud800");

        // 7. Triple backslash before ud800 -> literal "\" + U+FFFD
        let v7 = parse_protocol_json(r#"{"id":7,"result":{"value":"\\\ud800"}}"#).unwrap();
        assert_eq!(v7["result"]["value"], "\\\u{FFFD}");

        // 8. Unrecoverable malformed JSON extracts top-level id (ignoring nested "id")
        let err = parse_protocol_json(r#"{"nested":{"id":999},"id":42,"broken":tru}"#).unwrap_err();
        assert_eq!(err.0, Some(42));
    }
}
