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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverCapabilities {
    pub engine: EngineKind,
    pub protocol: String,
    pub supports_virtual_context: bool,
    pub supports_per_context_proxy: bool,
    pub supports_native_ax_tree: bool,
    pub supports_isolated_world: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteAction {
    Abort,
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
    async fn focus_node(&self, backend_node_id: Option<i64>, selector: Option<&str>)
        -> Result<()>;
    async fn set_file_input_files(
        &self,
        backend_node_id: Option<i64>,
        selector: Option<&str>,
        files: Vec<PathBuf>,
    ) -> Result<()>;

    // Chokepoint Input Primitives
    async fn dispatch_mouse(&self, ev: MouseEvent) -> Result<()>;
    async fn dispatch_key(&self, ev: KeyEvent) -> Result<()>;
    async fn dispatch_wheel(&self, ev: WheelEvent) -> Result<()>;
    async fn insert_text(&self, text: &str) -> Result<()>;

    // Media & Animations
    async fn screenshot(&self, opts: ScreenshotOpts) -> Result<Vec<u8>>;
    async fn pdf(&self) -> Result<Vec<u8>>;
    async fn set_instant_animations(&self, enabled: bool) -> Result<()>;

    // Environment & Network Control (M2)
    async fn set_env_overrides(&self, profile: EnvProfile) -> Result<()>;
    async fn set_blocked_urls(&self, patterns: Vec<String>) -> Result<()>;
    async fn set_route_mock(&self, mock: RouteMock) -> Result<()>;
    async fn clear_route_mocks(&self) -> Result<()>;

    // Cookies & Storage (M2)
    async fn cookies(&self) -> Result<Vec<Cookie>>;
    async fn set_cookies(&self, cookies: Vec<Cookie>) -> Result<()>;
    async fn clear_cookies(&self) -> Result<()>;
    async fn storage_state(&self) -> Result<StorageState>;
    async fn set_storage_state(&self, state: StorageState) -> Result<()>;

    // Telemetry & Wait Helpers (M2/M3)
    async fn console_logs(&self) -> Result<Vec<ConsoleEntry>>;
    async fn network_events(&self) -> Result<Vec<NetworkEvent>>;
    async fn inflight_requests(&self) -> Result<usize>;
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
/// If `pattern` contains no `*` or `?`, falls back to substring containment.
pub(crate) fn url_matches_pattern(pattern: &str, url: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return url.contains(pattern);
    }
    wildcard_match(pattern.as_bytes(), url.as_bytes())
}

fn wildcard_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while t < text.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_kind_round_trip() {
        assert_eq!("chromium".parse::<EngineKind>().unwrap(), EngineKind::Chromium);
        assert_eq!("chrome".parse::<EngineKind>().unwrap(), EngineKind::Chromium);
        assert_eq!("firefox".parse::<EngineKind>().unwrap(), EngineKind::Firefox);
        assert_eq!("gecko".parse::<EngineKind>().unwrap(), EngineKind::Firefox);
        assert!("webkit".parse::<EngineKind>().is_err());
    }

    #[test]
    fn url_matches_pattern_handles_globs_and_substrings() {
        assert!(url_matches_pattern("*blocked.png*", "http://127.0.0.1:8080/blocked.png"));
        assert!(url_matches_pattern("*/api/mock*", "http://127.0.0.1:8080/api/mock?x=1"));
        assert!(!url_matches_pattern("*/api/mock*", "http://127.0.0.1:8080/api/real"));
        assert!(url_matches_pattern("example.com", "https://example.com/path"));
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
        assert!(!BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 10.0,
        }
        .is_visible());
    }
}

