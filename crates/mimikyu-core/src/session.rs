use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver::{
    driver_for, now_ms, ContextOptions, DriverSession, EngineKind, EnvProfile, LaunchOptions,
    NavInfo, OriginStorage, ProfileMode, ProxyConfig, ScreencastOptions, ScreencastStatus,
    ScreenshotFormat, SelectBy, StorageState, WebRtcPolicy,
};
use crate::env_profile::{validate_profile, CoherenceReport, EnvProfileBuilder, PresetKind};
use crate::error::{MimikyuError, Result};
use crate::extract::{
    capture_screenshot, export_pdf, extract_links, extract_markdown, extract_tables, extract_text,
    CaptureOpts, ExtractOptions, MediaTarget,
};
use crate::input::{
    ActionBatcher, BatchAction, BatchError, BatchReport, BatchStepResult, ChokepointInput,
    ClickOptions, FillOptions, HumanizeConfig, InputOptions, InputTarget, ScrollOptions,
    TypeOptions,
};
use crate::isolation::{
    export_storage_state, import_storage_state, load_session_cookies, load_session_origins,
    merge_origin_storage, save_session_cookies, save_session_origins, save_storage_state,
    NamedProfileInfo, ProfileLock, ProfileStore, VirtualContext, VirtualContextPool,
};
use crate::network::BlockPolicy;
use crate::snapshot::{
    SnapshotEngine, SnapshotOptions, SnapshotOutput, SnapshotState, TargetLocator,
};
use crate::wait::{
    WaitEngine, WaitIdleOptions, WaitOptions, WaitSelectorState, DEFAULT_POLL_INTERVAL_MS,
    DEFAULT_WAIT_TIMEOUT_MS,
};

const MAX_SESSION_ID_LEN: usize = 128;
const MAX_VIEWPORT_DIM: u32 = 16_384;

pub const ENV_ALLOWED_ROOT: &str = "MIMIKYU_ALLOWED_ROOT";
pub const ENV_REQUIRE_OOB_RESUME: &str = "MIMIKYU_REQUIRE_OOB_RESUME";

fn env_allowed_root() -> Option<PathBuf> {
    std::env::var_os(ENV_ALLOWED_ROOT)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn env_require_oob_resume() -> bool {
    std::env::var(ENV_REQUIRE_OOB_RESUME)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManagerConfig {
    pub state_dir: PathBuf,
    pub profiles_dir: Option<PathBuf>,
    pub default_engine: EngineKind,
    pub default_headless: bool,
    pub default_telemetry_capacity: Option<usize>,
    pub default_ack_timeout_ms: Option<u64>,
    #[serde(default)]
    pub allowed_root: Option<PathBuf>,
    #[serde(default)]
    pub require_oob_resume: bool,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        let state_dir = dirs::data_local_dir()
            .map(|d| d.join("mimikyu"))
            .unwrap_or_else(|| std::env::temp_dir().join("mimikyu"));
        Self {
            state_dir,
            profiles_dir: None,
            default_engine: EngineKind::Chromium,
            default_headless: true,
            default_telemetry_capacity: None,
            default_ack_timeout_ms: None,
            allowed_root: env_allowed_root(),
            require_oob_resume: env_require_oob_resume(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateOpts {
    pub session_id: Option<String>,
    pub engine: Option<EngineKind>,
    pub profile: Option<String>,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub virtual_ctx: bool,
    pub preset: Option<PresetKind>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    pub proxy: Option<String>,
    pub geoip: Option<String>,
    pub viewport: Option<(u32, u32)>,
    #[serde(default)]
    pub block_images: bool,
    #[serde(default)]
    pub block_media: bool,
    #[serde(default)]
    pub block_fonts: bool,
    #[serde(default)]
    pub block_trackers: bool,
    #[serde(default)]
    pub privacy_mode: bool,
    #[serde(default)]
    pub instant_animations: bool,
    #[serde(default)]
    pub humanize: bool,
    #[serde(default)]
    pub main_world_eval: bool,
    #[serde(default = "default_true")]
    pub headless: bool,
}

impl Default for SessionCreateOpts {
    fn default() -> Self {
        Self {
            session_id: None,
            engine: None,
            profile: None,
            ephemeral: false,
            virtual_ctx: false,
            preset: None,
            locale: None,
            timezone: None,
            proxy: None,
            geoip: None,
            viewport: None,
            block_images: false,
            block_media: false,
            block_fonts: false,
            block_trackers: false,
            privacy_mode: false,
            instant_animations: false,
            humanize: false,
            main_world_eval: false,
            headless: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub engine: EngineKind,
    pub mode: String,
    pub url: String,
    pub title: String,
    pub pid: Option<u32>,
    pub created_at_ms: u64,
    #[serde(default)]
    pub paused_for_human: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseStatus {
    pub session_id: String,
    pub paused_for_human: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_id: Option<String>,
    #[serde(default)]
    pub confirmation_required: bool,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathConfinementMode {
    /// Existing regular file to read; canonicalized path must remain within the allowed root.
    ExistingFile,
    /// Output file path to write; canonicalized parent and target must remain within the allowed root
    /// and must not target a symlink.
    OutputFile,
    /// Output directory (may not yet exist); canonicalized ancestors and target must remain within
    /// the allowed root and must not target a symlink.
    OutputDir,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WaitRequest {
    #[serde(default)]
    pub idle: bool,
    pub text: Option<String>,
    pub url: Option<String>,
    pub selector: Option<String>,
    #[serde(default)]
    pub state: WaitSelectorState,
    pub timeout_ms: Option<u64>,
    pub poll_interval_ms: Option<u64>,
    pub network_quiet_ms: Option<u64>,
    pub stable_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractMode {
    Markdown,
    Text,
    Links,
    Tables,
}

impl fmt::Display for ExtractMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Markdown => write!(f, "markdown"),
            Self::Text => write!(f, "text"),
            Self::Links => write!(f, "links"),
            Self::Tables => write!(f, "tables"),
        }
    }
}

impl FromStr for ExtractMode {
    type Err = MimikyuError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "text" | "txt" => Ok(Self::Text),
            "links" => Ok(Self::Links),
            "tables" => Ok(Self::Tables),
            other => Err(MimikyuError::InvalidConfig(format!(
                "unknown extract mode '{other}' (expected markdown|text|links|tables)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogKind {
    Console,
    Network,
}

impl fmt::Display for LogKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Console => write!(f, "console"),
            Self::Network => write!(f, "network"),
        }
    }
}

impl FromStr for LogKind {
    type Err = MimikyuError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "console" => Ok(Self::Console),
            "network" => Ok(Self::Network),
            other => Err(MimikyuError::InvalidConfig(format!(
                "unknown log kind '{other}' (expected console|network)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotResult {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfResult {
    pub path: PathBuf,
    pub bytes: usize,
}

type PoolSlot = Arc<tokio::sync::Mutex<Option<Arc<VirtualContextPool>>>>;

#[derive(Default)]
struct SessionRegistry {
    active: HashMap<String, SessionEntry>,
    pending: HashSet<String>,
    shutting_down: bool,
}

#[derive(Clone)]
struct SessionEntry {
    meta: Arc<parking_lot::RwLock<SessionSummary>>,
    handle: Arc<tokio::sync::Mutex<ManagedSession>>,
}

struct ManagedSession {
    summary: SessionSummary,
    meta: Arc<parking_lot::RwLock<SessionSummary>>,
    driver: Arc<dyn DriverSession>,
    virtual_ctx: Option<VirtualContext>,
    persistent_path: Option<PathBuf>,
    ephemeral_download_dir: Option<PathBuf>,
    chokepoint: ChokepointInput,
    snapshot_state: SnapshotState,
    default_humanize: bool,
    block_policy: BlockPolicy,
    privacy_mode: bool,
    instant_animations: bool,
    main_world_eval: bool,
    env_profile: Option<EnvProfile>,
    coherence_report: Option<CoherenceReport>,
    proxy_redacted: Option<String>,
    paused_for_human: bool,
    pause_reason: Option<String>,
    paused_at_ms: Option<u64>,
    takeover_id: Option<String>,
    disposed: bool,
}

impl ManagedSession {
    fn ensure_not_paused(&self) -> Result<()> {
        if self.paused_for_human {
            return Err(MimikyuError::PausedForHuman {
                session_id: self.summary.session_id.clone(),
                reason: self.pause_reason.clone(),
            });
        }
        Ok(())
    }

    fn sync_pause_meta(&mut self) {
        self.summary.paused_for_human = self.paused_for_human;
        self.summary.pause_reason = self.pause_reason.clone();
        *self.meta.write() = self.summary.clone();
    }

    fn sync_nav_meta(&mut self, url: String, title: String) {
        self.summary.url = url;
        self.summary.title = title;
        *self.meta.write() = self.summary.clone();
    }

    async fn refresh_meta_best_effort(&mut self) {
        if let Ok(url) = self.driver.current_url().await {
            self.summary.url = url;
        }
        if let Ok(title) = self.driver.title().await {
            self.summary.title = title;
        }
        *self.meta.write() = self.summary.clone();
    }
}

struct PendingIdGuard {
    registry: Arc<parking_lot::Mutex<SessionRegistry>>,
    session_id: String,
    armed: bool,
}

impl PendingIdGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingIdGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut reg = self.registry.lock();
            reg.pending.remove(&self.session_id);
        }
    }
}

pub struct SessionManager {
    config: SessionManagerConfig,
    allowed_root: Option<PathBuf>,
    profile_store: ProfileStore,
    registry: Arc<parking_lot::Mutex<SessionRegistry>>,
    pools: tokio::sync::Mutex<HashMap<(EngineKind, bool), PoolSlot>>,
}

impl SessionManager {
    pub fn new(state_dir: PathBuf) -> Result<Self> {
        Self::with_config(SessionManagerConfig {
            state_dir,
            ..Default::default()
        })
    }

    pub fn with_config(config: SessionManagerConfig) -> Result<Self> {
        if config.state_dir.as_os_str().is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "state_dir cannot be empty".to_string(),
            ));
        }
        if config.state_dir.exists() && !config.state_dir.is_dir() {
            return Err(MimikyuError::InvalidConfig(format!(
                "state_dir '{}' exists and is not a directory",
                config.state_dir.display()
            )));
        }
        fs::create_dir_all(&config.state_dir)?;

        let allowed_root = match config.allowed_root.as_ref() {
            Some(root) => {
                if root.as_os_str().is_empty() {
                    return Err(MimikyuError::InvalidConfig(
                        "allowed_root cannot be empty".to_string(),
                    ));
                }
                let canonical = fs::canonicalize(root).map_err(|e| {
                    MimikyuError::InvalidConfig(format!(
                        "allowed_root '{}' is not accessible: {e}",
                        root.display()
                    ))
                })?;
                if !canonical.is_dir() {
                    return Err(MimikyuError::InvalidConfig(format!(
                        "allowed_root '{}' is not a directory",
                        root.display()
                    )));
                }
                Some(canonical)
            }
            None => None,
        };

        let profiles_dir = config
            .profiles_dir
            .clone()
            .unwrap_or_else(|| config.state_dir.join("profiles"));
        if profiles_dir.exists() && !profiles_dir.is_dir() {
            return Err(MimikyuError::InvalidConfig(format!(
                "profiles_dir '{}' exists and is not a directory",
                profiles_dir.display()
            )));
        }
        fs::create_dir_all(&profiles_dir)?;

        Ok(Self {
            profile_store: ProfileStore::new(profiles_dir),
            allowed_root,
            config,
            registry: Arc::new(parking_lot::Mutex::new(SessionRegistry::default())),
            pools: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.config.state_dir
    }

    pub fn allowed_root(&self) -> Option<&Path> {
        self.allowed_root.as_deref()
    }

    pub fn require_oob_resume(&self) -> bool {
        self.config.require_oob_resume
    }

    pub fn takeover_confirmation_path(&self, session_id: &str) -> PathBuf {
        let safe_id = safe_session_token(session_id);
        self.config
            .state_dir
            .join("takeover")
            .join(format!("{safe_id}.confirmed"))
    }

    fn resolve_path_if_confined(&self, path: &Path, mode: PathConfinementMode) -> Result<PathBuf> {
        if let Some(ref root) = self.allowed_root {
            resolve_confined_path(path, root, mode)
        } else {
            Ok(path.to_path_buf())
        }
    }

    pub fn profile_store(&self) -> &ProfileStore {
        &self.profile_store
    }

    pub async fn create_session(&self, opts: SessionCreateOpts) -> Result<SessionSummary> {
        let session_id = match opts.session_id.as_deref() {
            Some(id) => {
                validate_session_id(id)?;
                id.to_string()
            }
            None => format!("s-{}", uuid::Uuid::new_v4().simple()),
        };

        if opts.ephemeral && opts.virtual_ctx {
            return Err(MimikyuError::InvalidConfig(
                "cannot combine ephemeral and virtual session modes".to_string(),
            ));
        }
        if opts.profile.is_some() && opts.ephemeral {
            return Err(MimikyuError::InvalidConfig(
                "cannot combine persistent profile and ephemeral mode".to_string(),
            ));
        }
        if opts.profile.is_some() && opts.virtual_ctx {
            return Err(MimikyuError::InvalidConfig(
                "cannot combine persistent profile and virtual mode".to_string(),
            ));
        }

        let engine = opts.engine.unwrap_or(self.config.default_engine);
        let proxy_cfg = match opts.proxy.as_deref() {
            Some(raw) => Some(ProxyConfig::parse_normalized(raw)?),
            None => None,
        };
        let proxy_redacted = proxy_cfg.as_ref().map(|p| redact_proxy_url(&p.server));

        if let Some((w, h)) = opts.viewport
            && (w == 0 || h == 0 || w > MAX_VIEWPORT_DIM || h > MAX_VIEWPORT_DIM)
        {
            return Err(MimikyuError::InvalidConfig(format!(
                "viewport {w}x{h} out of valid bounds (1..={MAX_VIEWPORT_DIM})"
            )));
        }

        let (env_profile, coherence_report) =
            build_env_profile(&opts, engine, proxy_cfg.is_some())?;

        let persistent_path = if let Some(ref prof_spec) = opts.profile {
            let resolved = self.profile_store.resolve(prof_spec)?;
            let target_path = if resolved.is_named {
                resolved.path
            } else {
                self.resolve_path_if_confined(&resolved.path, PathConfinementMode::OutputDir)?
            };
            if target_path.exists() && !target_path.is_dir() {
                return Err(MimikyuError::InvalidConfig(format!(
                    "profile path '{}' exists and is not a directory",
                    target_path.display()
                )));
            }
            fs::create_dir_all(&target_path)?;
            if ProfileLock::is_locked(&target_path)? {
                return Err(MimikyuError::ProfileLocked(format!(
                    "profile '{}' is currently locked by another session",
                    target_path.display()
                )));
            }
            Some(target_path)
        } else {
            None
        };

        let mut pending_guard = {
            let mut reg = self.registry.lock();
            if reg.shutting_down {
                return Err(MimikyuError::Disposed);
            }
            if reg.active.contains_key(&session_id) || reg.pending.contains(&session_id) {
                return Err(MimikyuError::InvalidConfig(format!(
                    "session '{session_id}' already exists"
                )));
            }
            reg.pending.insert(session_id.clone());
            PendingIdGuard {
                registry: Arc::clone(&self.registry),
                session_id: session_id.clone(),
                armed: true,
            }
        };

        let effective_block_trackers = opts.block_trackers || opts.privacy_mode;
        let block_policy = BlockPolicy {
            block_images: opts.block_images,
            block_media: opts.block_media,
            block_fonts: opts.block_fonts,
            block_trackers: effective_block_trackers,
            blocked_urls: Vec::new(),
        };

        let (download_dir, mut ephemeral_download_dir) = if let Some(ref prof_dir) = persistent_path
        {
            (prof_dir.join("downloads"), None)
        } else {
            let dl_dir = safe_session_download_dir(&self.config.state_dir, &session_id);
            (dl_dir.clone(), Some(dl_dir))
        };

        let (driver, virtual_ctx, mode, dedicated_pid): (
            Arc<dyn DriverSession>,
            Option<VirtualContext>,
            String,
            Option<u32>,
        ) = if opts.virtual_ctx {
            let ctx_opts = ContextOptions {
                proxy: proxy_cfg,
                env_profile: env_profile.clone(),
                block_images: opts.block_images,
                block_media: opts.block_media,
                block_fonts: opts.block_fonts,
                block_trackers: effective_block_trackers,
                privacy_mode: opts.privacy_mode,
                instant_animations: opts.instant_animations,
                main_world_eval: opts.main_world_eval,
                permissions: Vec::new(),
                telemetry_capacity: self.config.default_telemetry_capacity,
                download_dir: Some(download_dir),
            };
            let vc = match self
                .create_virtual_context(engine, opts.headless, &session_id, ctx_opts)
                .await
            {
                Ok(vc) => vc,
                Err(err) => {
                    if let Some(ref dl_dir) = ephemeral_download_dir {
                        cleanup_ephemeral_download_dir(dl_dir);
                    }
                    return Err(err);
                }
            };
            let drv = Arc::clone(vc.session());
            (drv, Some(vc), "virtual".to_string(), None)
        } else {
            let (profile_mode, mode_label) = match persistent_path.as_ref() {
                Some(path) => (ProfileMode::Persistent(path.clone()), "persistent".to_string()),
                None => (ProfileMode::EphemeralDir, "ephemeral".to_string()),
            };
            let launch_opts = LaunchOptions {
                engine,
                binary_path: None,
                headless: opts.headless,
                profile_mode,
                proxy: proxy_cfg,
                env_profile: env_profile.clone(),
                block_images: opts.block_images,
                block_media: opts.block_media,
                block_fonts: opts.block_fonts,
                block_trackers: effective_block_trackers,
                privacy_mode: opts.privacy_mode,
                instant_animations: opts.instant_animations,
                main_world_eval: opts.main_world_eval,
                telemetry_capacity: self.config.default_telemetry_capacity,
                download_dir: Some(download_dir),
                ..Default::default()
            };
            let drv = match driver_for(engine).launch(launch_opts).await {
                Ok(drv) => drv,
                Err(err) => {
                    if let Some(ref dl_dir) = ephemeral_download_dir {
                        cleanup_ephemeral_download_dir(dl_dir);
                    }
                    return Err(err);
                }
            };
            if let Some(ref prof_dir) = persistent_path {
                let session_cookies = match load_session_cookies(prof_dir) {
                    Ok(c) => c,
                    Err(err) => {
                        let _ = drv.dispose().await;
                        return Err(err);
                    }
                };
                let session_origins = match load_session_origins(prof_dir) {
                    Ok(o) => o,
                    Err(err) => {
                        let _ = drv.dispose().await;
                        return Err(err);
                    }
                };
                if (!session_cookies.is_empty() || !session_origins.is_empty())
                    && let Err(err) = drv
                        .set_storage_state(StorageState {
                            cookies: session_cookies,
                            origins: session_origins,
                        })
                        .await
                {
                    let _ = drv.dispose().await;
                    return Err(err);
                }
            }
            let pid = drv.info().pid;
            (drv, None, mode_label, pid)
        };

        let url = driver
            .current_url()
            .await
            .unwrap_or_else(|_| "about:blank".to_string());
        let title = driver.title().await.unwrap_or_default();

        let summary = SessionSummary {
            session_id: session_id.clone(),
            engine,
            mode,
            url,
            title,
            pid: dedicated_pid,
            created_at_ms: now_ms(),
            paused_for_human: false,
            pause_reason: None,
        };

        let mut chokepoint = ChokepointInput::new(Arc::clone(&driver));
        if let Some(ack_ms) = self.config.default_ack_timeout_ms {
            chokepoint = chokepoint.with_ack_timeout(Duration::from_millis(ack_ms));
        }

        let meta = Arc::new(parking_lot::RwLock::new(summary.clone()));
        let managed = ManagedSession {
            summary: summary.clone(),
            meta: Arc::clone(&meta),
            driver: Arc::clone(&driver),
            virtual_ctx,
            persistent_path,
            ephemeral_download_dir: ephemeral_download_dir.take(),
            chokepoint,
            snapshot_state: SnapshotState::default(),
            default_humanize: opts.humanize,
            block_policy,
            privacy_mode: opts.privacy_mode,
            instant_animations: opts.instant_animations,
            main_world_eval: opts.main_world_eval,
            env_profile,
            coherence_report,
            proxy_redacted,
            paused_for_human: false,
            pause_reason: None,
            paused_at_ms: None,
            takeover_id: None,
            disposed: false,
        };

        let entry = SessionEntry {
            meta,
            handle: Arc::new(tokio::sync::Mutex::new(managed)),
        };

        struct ShutdownDispose {
            vc: Option<VirtualContext>,
            drv: Arc<dyn DriverSession>,
            dl_dir: Option<PathBuf>,
        }
        let dispose_on_shutdown: Option<ShutdownDispose> = {
            let mut reg = self.registry.lock();
            reg.pending.remove(&session_id);
            pending_guard.disarm();
            if reg.shutting_down {
                let mut guard = entry
                    .handle
                    .try_lock()
                    .expect("newly created session handle must be uncontended");
                Some(ShutdownDispose {
                    vc: guard.virtual_ctx.take(),
                    drv: Arc::clone(&guard.driver),
                    dl_dir: guard.ephemeral_download_dir.take(),
                })
            } else {
                reg.active.insert(session_id, entry);
                None
            }
        };

        if let Some(cleanup) = dispose_on_shutdown {
            if let Some(vc) = cleanup.vc {
                let _ = vc.dispose().await;
            } else {
                let _ = cleanup.drv.dispose().await;
            }
            if let Some(ref dl) = cleanup.dl_dir {
                cleanup_ephemeral_download_dir(dl);
            }
            return Err(MimikyuError::Disposed);
        }

        Ok(summary)
    }

    async fn get_pool_slot(&self, engine: EngineKind, headless: bool) -> Result<PoolSlot> {
        let mut pools = self.pools.lock().await;
        if self.registry.lock().shutting_down {
            return Err(MimikyuError::Disposed);
        }
        Ok(Arc::clone(
            pools
                .entry((engine, headless))
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
        ))
    }

    async fn create_virtual_context(
        &self,
        engine: EngineKind,
        headless: bool,
        session_id: &str,
        ctx_opts: ContextOptions,
    ) -> Result<VirtualContext> {
        let slot = self.get_pool_slot(engine, headless).await?;
        let mut slot_guard = slot.lock().await;

        if self.registry.lock().shutting_down {
            return Err(MimikyuError::Disposed);
        }

        if slot_guard.is_none() {
            let pool = self.launch_virtual_pool(engine, headless).await?;
            *slot_guard = Some(Arc::new(pool));
        }

        let first_attempt = {
            let pool = slot_guard.as_ref().expect("pool initialized");
            pool.create_labeled_context(session_id, ctx_opts.clone())
                .await
        };

        match first_attempt {
            Ok(vc) => Ok(vc),
            Err(MimikyuError::Disposed | MimikyuError::Transport(_)) => {
                // Dead pool recovery: dispose stale pool and relaunch once.
                if let Some(old_pool) = slot_guard.take() {
                    dispose_pool_arc(old_pool).await;
                }
                let fresh_pool = self.launch_virtual_pool(engine, headless).await?;
                let pool_arc = Arc::new(fresh_pool);
                let vc = pool_arc
                    .create_labeled_context(session_id, ctx_opts)
                    .await?;
                *slot_guard = Some(pool_arc);
                Ok(vc)
            }
            Err(e) => Err(e),
        }
    }

    async fn launch_virtual_pool(
        &self,
        engine: EngineKind,
        headless: bool,
    ) -> Result<VirtualContextPool> {
        VirtualContextPool::launch(LaunchOptions {
            engine,
            headless,
            profile_mode: ProfileMode::VirtualContext,
            telemetry_capacity: self.config.default_telemetry_capacity,
            ..Default::default()
        })
        .await
    }

    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let metas: Vec<Arc<parking_lot::RwLock<SessionSummary>>> = {
            let reg = self.registry.lock();
            reg.active
                .values()
                .map(|entry| Arc::clone(&entry.meta))
                .collect()
        };
        let mut out: Vec<SessionSummary> = metas.into_iter().map(|m| m.read().clone()).collect();
        out.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        out
    }

    pub async fn close_session(&self, session_id: &str) -> Result<()> {
        let entry = {
            let mut reg = self.registry.lock();
            reg.active.remove(session_id)
        }
        .ok_or_else(|| MimikyuError::NotFound(format!("session '{session_id}' not found")))?;

        dispose_session_entry(entry).await
    }

    pub async fn pause_session(
        &self,
        session_id: &str,
        reason: Option<String>,
    ) -> Result<PauseStatus> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        let cleaned_reason = reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        let ts = now_ms();
        let takeover_id = format!("tk-{}", uuid::Uuid::new_v4().simple());
        // Remove any stale out-of-band confirmation marker from a prior takeover cycle.
        let _ = fs::remove_file(self.takeover_confirmation_path(session_id));
        managed.paused_for_human = true;
        managed.pause_reason = cleaned_reason.clone();
        managed.paused_at_ms = Some(ts);
        managed.takeover_id = Some(takeover_id.clone());
        managed.refresh_meta_best_effort().await;
        managed.sync_pause_meta();
        Ok(PauseStatus {
            session_id: managed.summary.session_id.clone(),
            paused_for_human: true,
            pause_reason: cleaned_reason,
            paused_at_ms: Some(ts),
            takeover_id: Some(takeover_id),
            confirmation_required: true,
            url: managed.summary.url.clone(),
            title: managed.summary.title.clone(),
        })
    }

    pub async fn takeover_session(
        &self,
        session_id: &str,
        reason: Option<String>,
    ) -> Result<PauseStatus> {
        let effective_reason = reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .or_else(|| Some("human takeover requested".to_string()));
        self.pause_session(session_id, effective_reason).await
    }

    pub async fn resume_session(&self, session_id: &str) -> Result<PauseStatus> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.paused_for_human = false;
        managed.pause_reason = None;
        managed.paused_at_ms = None;
        managed.takeover_id = None;
        let _ = fs::remove_file(self.takeover_confirmation_path(session_id));
        // Invalidate snapshot refs because the human operator may have navigated or mutated the DOM.
        managed.snapshot_state.active_ref_map.clear();
        managed.refresh_meta_best_effort().await;
        managed.sync_pause_meta();
        Ok(PauseStatus {
            session_id: managed.summary.session_id.clone(),
            paused_for_human: false,
            pause_reason: None,
            paused_at_ms: None,
            takeover_id: None,
            confirmation_required: false,
            url: managed.summary.url.clone(),
            title: managed.summary.title.clone(),
        })
    }

    /// Resume a paused session only when explicit human confirmation and the matching
    /// `takeover_id` token are provided (and an out-of-band confirmation marker file when
    /// `require_oob_resume` / `MIMIKYU_REQUIRE_OOB_RESUME` is enabled).
    pub async fn resume_session_confirmed(
        &self,
        session_id: &str,
        takeover_id: &str,
        confirmed_by_human: bool,
    ) -> Result<PauseStatus> {
        if !confirmed_by_human {
            return Err(MimikyuError::InvalidConfig(
                "cannot resume paused session without explicit human confirmation (confirmed_by_human must be true)"
                    .to_string(),
            ));
        }
        let provided_token = takeover_id.trim();
        if provided_token.is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "missing or empty 'takeover_id'; resume requires the active takeover token from mimikyu_request_human_takeover"
                    .to_string(),
            ));
        }

        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }

        if managed.paused_for_human {
            let expected = managed.takeover_id.as_deref().unwrap_or("");
            if expected.is_empty() || provided_token != expected {
                return Err(MimikyuError::InvalidConfig(
                    "invalid or mismatched 'takeover_id' for paused session; verify the active takeover token with the human operator"
                        .to_string(),
                ));
            }
            if self.config.require_oob_resume {
                let conf_path = self.takeover_confirmation_path(session_id);
                match fs::symlink_metadata(&conf_path) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(MimikyuError::InvalidConfig(format!(
                            "refusing symlink out-of-band confirmation file '{}'",
                            conf_path.display()
                        )));
                    }
                    Ok(_) => {
                        let content = fs::read_to_string(&conf_path).map_err(|e| {
                            MimikyuError::InvalidConfig(format!(
                                "failed to read out-of-band human confirmation file '{}': {e}",
                                conf_path.display()
                            ))
                        })?;
                        if content.trim() != expected {
                            return Err(MimikyuError::InvalidConfig(format!(
                                "out-of-band human confirmation token in '{}' does not match active takeover_id",
                                conf_path.display()
                            )));
                        }
                        let _ = fs::remove_file(&conf_path);
                    }
                    Err(_) => {
                        return Err(MimikyuError::InvalidConfig(format!(
                            "out-of-band human confirmation required before MCP resume: run `mimikyu --session {session_id} session resume` or write the active takeover_id to '{}'",
                            conf_path.display()
                        )));
                    }
                }
            }
        }

        managed.paused_for_human = false;
        managed.pause_reason = None;
        managed.paused_at_ms = None;
        managed.takeover_id = None;
        let _ = fs::remove_file(self.takeover_confirmation_path(session_id));
        managed.snapshot_state.active_ref_map.clear();
        managed.refresh_meta_best_effort().await;
        managed.sync_pause_meta();
        Ok(PauseStatus {
            session_id: managed.summary.session_id.clone(),
            paused_for_human: false,
            pause_reason: None,
            paused_at_ms: None,
            takeover_id: None,
            confirmation_required: false,
            url: managed.summary.url.clone(),
            title: managed.summary.title.clone(),
        })
    }

    pub async fn start_screencast(
        &self,
        session_id: &str,
        mut opts: ScreencastOptions,
    ) -> Result<ScreencastStatus> {
        if opts.out_dir.as_os_str().is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "screencast out_dir cannot be empty".to_string(),
            ));
        }
        opts.out_dir =
            self.resolve_path_if_confined(&opts.out_dir, PathConfinementMode::OutputDir)?;
        if opts.out_dir.exists() && !opts.out_dir.is_dir() {
            return Err(MimikyuError::InvalidConfig(format!(
                "screencast out_dir '{}' exists and is not a directory",
                opts.out_dir.display()
            )));
        }
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        managed.driver.start_screencast(opts).await
    }

    pub async fn stop_screencast(&self, session_id: &str) -> Result<ScreencastStatus> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.driver.stop_screencast().await
    }

    pub async fn export_session(&self, session_id: &str, out_path: &Path) -> Result<()> {
        let safe_out = self.resolve_path_if_confined(out_path, PathConfinementMode::OutputFile)?;
        validate_output_file_path(&safe_out)?;
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        if let Some(ref prof_dir) = managed.persistent_path {
            let mut state = managed.driver.storage_state().await?;
            state.cookies.retain(|c| !c.name.trim().is_empty());
            if let Ok(pending_origins) = load_session_origins(prof_dir)
                && !pending_origins.is_empty()
            {
                let still_pending: HashSet<String> = managed
                    .driver
                    .pending_storage_origins()
                    .into_iter()
                    .collect();
                let existing_origins: HashSet<String> =
                    state.origins.iter().map(|o| o.origin.clone()).collect();
                for po in pending_origins {
                    if still_pending.contains(&po.origin) && !existing_origins.contains(&po.origin)
                    {
                        state.origins.push(po);
                    }
                }
            }
            save_storage_state(&safe_out, &state)?;
        } else {
            export_storage_state(managed.driver.as_ref(), &safe_out).await?;
        }
        Ok(())
    }

    pub async fn import_session(&self, session_id: &str, in_path: &Path) -> Result<()> {
        let safe_in = self.resolve_path_if_confined(in_path, PathConfinementMode::ExistingFile)?;
        if safe_in.as_os_str().is_empty() || safe_in.is_dir() || !safe_in.exists() {
            return Err(MimikyuError::InvalidConfig(format!(
                "import storage state file '{}' does not exist or is a directory",
                in_path.display()
            )));
        }
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let state = import_storage_state(managed.driver.as_ref(), &safe_in).await?;
        if let Some(ref prof_dir) = managed.persistent_path {
            if let Ok(cookies) = managed.driver.cookies().await {
                save_session_cookies(prof_dir, &cookies)?;
            }
            let pending_origins: HashSet<String> = managed
                .driver
                .pending_storage_origins()
                .into_iter()
                .collect();
            let mut stored_origins = load_session_origins(prof_dir).unwrap_or_default();
            let mut hydrated_in_import: HashSet<String> = HashSet::new();
            let mut pending_in_import: Vec<OriginStorage> = Vec::new();
            for orig in state.origins {
                if orig.local_storage.is_empty() && orig.session_storage.is_empty() {
                    continue;
                }
                if pending_origins.contains(&orig.origin) {
                    pending_in_import.push(orig);
                } else {
                    hydrated_in_import.insert(orig.origin);
                }
            }
            stored_origins.retain(|o| !hydrated_in_import.contains(&o.origin));
            merge_origin_storage(&mut stored_origins, pending_in_import);
            save_session_origins(prof_dir, &stored_origins)?;
        }
        Ok(())
    }

    pub async fn open(&self, session_id: &str, url: &str) -> Result<NavInfo> {
        let valid_url = validate_nav_url(url)?;
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        managed.snapshot_state.active_ref_map.clear();
        let nav = managed.driver.navigate(&valid_url).await?;
        managed.sync_nav_meta(nav.url.clone(), nav.title.clone());
        Ok(nav)
    }

    pub async fn back(&self, session_id: &str) -> Result<NavInfo> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        managed.snapshot_state.active_ref_map.clear();
        let nav = managed.driver.go_back().await?;
        managed.sync_nav_meta(nav.url.clone(), nav.title.clone());
        Ok(nav)
    }

    pub async fn forward(&self, session_id: &str) -> Result<NavInfo> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        managed.snapshot_state.active_ref_map.clear();
        let nav = managed.driver.go_forward().await?;
        managed.sync_nav_meta(nav.url.clone(), nav.title.clone());
        Ok(nav)
    }

    pub async fn reload(&self, session_id: &str) -> Result<NavInfo> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        managed.snapshot_state.active_ref_map.clear();
        let nav = managed.driver.reload().await?;
        managed.sync_nav_meta(nav.url.clone(), nav.title.clone());
        Ok(nav)
    }

    pub async fn snapshot(
        &self,
        session_id: &str,
        opts: SnapshotOptions,
    ) -> Result<SnapshotOutput> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let driver = Arc::clone(&managed.driver);
        let snap = SnapshotEngine::capture(driver.as_ref(), &mut managed.snapshot_state, opts).await?;
        managed.sync_nav_meta(snap.url.clone(), snap.title.clone());
        Ok(snap)
    }

    pub async fn click(
        &self,
        session_id: &str,
        target: &str,
        humanize: Option<bool>,
    ) -> Result<()> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let use_humanize = humanize.unwrap_or(managed.default_humanize);
        let tgt = InputTarget::parse(target);
        let opts = ClickOptions {
            humanize: HumanizeConfig {
                enabled: use_humanize,
                ..Default::default()
            },
            ..Default::default()
        };
        managed
            .chokepoint
            .click(&tgt, Some(&managed.snapshot_state.active_ref_map), &opts)
            .await?;
        managed.refresh_meta_best_effort().await;
        Ok(())
    }

    pub async fn fill(
        &self,
        session_id: &str,
        target: &str,
        value: &str,
        humanize: Option<bool>,
    ) -> Result<()> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let use_humanize = humanize.unwrap_or(managed.default_humanize);
        let tgt = InputTarget::parse(target);
        let opts = FillOptions {
            humanize: HumanizeConfig {
                enabled: use_humanize,
                ..Default::default()
            },
            ..Default::default()
        };
        managed
            .chokepoint
            .fill(
                &tgt,
                Some(&managed.snapshot_state.active_ref_map),
                value,
                &opts,
            )
            .await
    }

    pub async fn type_text(
        &self,
        session_id: &str,
        target: &str,
        text: &str,
        humanize: Option<bool>,
    ) -> Result<()> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let use_humanize = humanize.unwrap_or(managed.default_humanize);
        let tgt = InputTarget::parse(target);
        let opts = TypeOptions {
            humanize: HumanizeConfig {
                enabled: use_humanize,
                ..Default::default()
            },
            ..Default::default()
        };
        managed
            .chokepoint
            .type_text(
                &tgt,
                Some(&managed.snapshot_state.active_ref_map),
                text,
                &opts,
            )
            .await
    }

    pub async fn select(&self, session_id: &str, target: &str, value: &str) -> Result<String> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let tgt = InputTarget::parse(target);
        managed
            .chokepoint
            .select_option(
                &tgt,
                Some(&managed.snapshot_state.active_ref_map),
                SelectBy::Auto(value.to_string()),
                0,
            )
            .await
    }

    pub async fn upload(&self, session_id: &str, target: &str, path: PathBuf) -> Result<()> {
        let safe_path = self.resolve_path_if_confined(&path, PathConfinementMode::ExistingFile)?;
        let meta = fs::metadata(&safe_path).map_err(|e| {
            MimikyuError::InvalidConfig(format!(
                "upload file '{}' is not accessible: {e}",
                path.display()
            ))
        })?;
        if !meta.is_file() {
            return Err(MimikyuError::InvalidConfig(format!(
                "upload path '{}' is not a regular file",
                path.display()
            )));
        }

        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let tgt = InputTarget::parse(target);
        managed
            .chokepoint
            .set_input_files(
                &tgt,
                Some(&managed.snapshot_state.active_ref_map),
                vec![safe_path],
                0,
            )
            .await
    }

    pub async fn scroll(
        &self,
        session_id: &str,
        dx: f64,
        dy: f64,
        humanize: Option<bool>,
    ) -> Result<()> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(MimikyuError::InvalidConfig(
                "scroll deltas must be finite numbers".to_string(),
            ));
        }
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let use_humanize = humanize.unwrap_or(managed.default_humanize);
        let opts = ScrollOptions {
            humanize: HumanizeConfig {
                enabled: use_humanize,
                ..Default::default()
            },
            ..Default::default()
        };
        managed
            .chokepoint
            .scroll(
                dx,
                dy,
                None,
                Some(&managed.snapshot_state.active_ref_map),
                &opts,
            )
            .await
    }

    pub async fn press(&self, session_id: &str, key: &str) -> Result<()> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        managed
            .chokepoint
            .press(key, &InputOptions::default())
            .await
    }

    pub async fn wait(&self, session_id: &str, req: WaitRequest) -> Result<()> {
        if !req.idle && req.text.is_none() && req.url.is_none() && req.selector.is_none() {
            return Err(MimikyuError::InvalidConfig(
                "wait requires at least one condition (--idle, --text, --url, or --selector)"
                    .to_string(),
            ));
        }

        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;

        let timeout_ms = req.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let poll_interval_ms = req.poll_interval_ms.unwrap_or(DEFAULT_POLL_INTERVAL_MS);
        let wait_opts = WaitOptions {
            timeout_ms,
            poll_interval_ms,
        };

        if let Some(ref sel) = req.selector {
            WaitEngine::wait_selector(managed.driver.as_ref(), sel, req.state, wait_opts.clone())
                .await?;
        }
        if let Some(ref text) = req.text {
            WaitEngine::wait_text(managed.driver.as_ref(), text, wait_opts.clone()).await?;
        }
        if let Some(ref url_substr) = req.url {
            WaitEngine::wait_url(managed.driver.as_ref(), url_substr, wait_opts).await?;
        }
        if req.idle {
            let mut idle_opts = WaitIdleOptions {
                timeout_ms,
                poll_interval_ms,
                stable_target: req.stable_target.clone(),
                ..Default::default()
            };
            if let Some(quiet_ms) = req.network_quiet_ms {
                idle_opts.network_quiet_ms = quiet_ms;
            }
            WaitEngine::wait_idle(
                managed.driver.as_ref(),
                Some(&managed.snapshot_state.active_ref_map),
                idle_opts,
            )
            .await?;
        }

        managed.refresh_meta_best_effort().await;
        Ok(())
    }

    pub async fn batch(
        &self,
        session_id: &str,
        mut actions: Vec<BatchAction>,
    ) -> Result<BatchReport> {
        if let Some(ref root) = self.allowed_root {
            for action in &mut actions {
                if let BatchAction::Upload { files, .. } = action {
                    for file in files.iter_mut() {
                        *file = resolve_confined_path(
                            file,
                            root,
                            PathConfinementMode::ExistingFile,
                        )?;
                    }
                }
            }
        }

        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;

        let driver = Arc::clone(&managed.driver);
        let ManagedSession {
            ref chokepoint,
            ref mut snapshot_state,
            ..
        } = *managed;

        let report = if !actions
            .iter()
            .any(|a| matches!(a, BatchAction::Open { .. }))
        {
            ActionBatcher::execute(driver.as_ref(), chokepoint, snapshot_state, &actions).await?
        } else {
            execute_batch_with_open_invalidation(
                driver.as_ref(),
                chokepoint,
                snapshot_state,
                &actions,
            )
            .await?
        };

        managed.refresh_meta_best_effort().await;
        Ok(report)
    }

    pub async fn extract(&self, session_id: &str, mode: ExtractMode) -> Result<Value> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        let opts = ExtractOptions::default();
        match mode {
            ExtractMode::Markdown => {
                Ok(serde_json::to_value(extract_markdown(managed.driver.as_ref(), &opts).await?)?)
            }
            ExtractMode::Text => {
                Ok(serde_json::to_value(extract_text(managed.driver.as_ref(), &opts).await?)?)
            }
            ExtractMode::Links => {
                Ok(serde_json::to_value(extract_links(managed.driver.as_ref(), &opts).await?)?)
            }
            ExtractMode::Tables => {
                Ok(serde_json::to_value(extract_tables(managed.driver.as_ref(), &opts).await?)?)
            }
        }
    }

    pub async fn eval(&self, session_id: &str, js: &str, main_world: bool) -> Result<Value> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        if main_world {
            if !managed.main_world_eval {
                return Err(MimikyuError::MainWorldDisabled);
            }
            managed.driver.eval_main_world(js).await
        } else {
            managed.driver.eval_isolated(js).await
        }
    }

    pub async fn screenshot_bytes(
        &self,
        session_id: &str,
        target: Option<&str>,
        full_page: bool,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;

        let media_target = match target {
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(MimikyuError::InvalidConfig(
                        "screenshot target cannot be empty".to_string(),
                    ));
                }
                match TargetLocator::parse(trimmed) {
                    TargetLocator::Ref(ref_id) => {
                        let ref_entry = managed
                            .snapshot_state
                            .active_ref_map
                            .get(ref_id)
                            .ok_or_else(|| {
                                MimikyuError::ElementNotFound(format!(
                                    "unknown or stale snapshot ref '{ref_id}'"
                                ))
                            })?;
                        let bbox = if let Some(bid) = ref_entry.backend_node_id {
                            match managed
                                .driver
                                .resolve_node_box(bid, ref_entry.frame_id.as_deref())
                                .await
                            {
                                Ok(b) => b,
                                Err(e) => {
                                    if let Some(ref css) = ref_entry.css_fallback {
                                        managed
                                            .driver
                                            .resolve_selector_box(css)
                                            .await
                                            .map_err(|_| e)?
                                    } else {
                                        return Err(e);
                                    }
                                }
                            }
                        } else if let Some(ref css) = ref_entry.css_fallback {
                            managed.driver.resolve_selector_box(css).await?
                        } else {
                            return Err(MimikyuError::ElementNotFound(format!(
                                "ref '{ref_id}' has no resolvable geometry"
                            )));
                        };
                        if !bbox.is_visible() {
                            return Err(MimikyuError::ElementNotFound(format!(
                                "screenshot target '{ref_id}' is not visible (0x0)"
                            )));
                        }
                        Some(MediaTarget::Clip(bbox))
                    }
                    TargetLocator::Selector(sel) => {
                        let bbox = managed.driver.resolve_selector_box(sel).await?;
                        if !bbox.is_visible() {
                            return Err(MimikyuError::ElementNotFound(format!(
                                "screenshot target '{sel}' is not visible (0x0)"
                            )));
                        }
                        Some(MediaTarget::Clip(bbox))
                    }
                }
            }
            None => None,
        };

        let capture = capture_screenshot(
            managed.driver.as_ref(),
            Some(&managed.snapshot_state.active_ref_map),
            CaptureOpts {
                target: media_target,
                format: ScreenshotFormat::Png,
                full_page: target.is_none() && full_page,
                ..Default::default()
            },
        )
        .await?;

        if capture.bytes.is_empty()
            || capture.width == 0
            || capture.height == 0
            || !capture.bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        {
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "screenshot produced empty or invalid PNG bytes".to_string(),
            });
        }

        Ok((capture.bytes, capture.width, capture.height))
    }

    pub async fn screenshot(
        &self,
        session_id: &str,
        out_path: &Path,
        target: Option<&str>,
        full_page: bool,
    ) -> Result<ScreenshotResult> {
        let safe_out = self.resolve_path_if_confined(out_path, PathConfinementMode::OutputFile)?;
        validate_output_file_path(&safe_out)?;
        let (bytes, width, height) = self.screenshot_bytes(session_id, target, full_page).await?;
        fs::write(&safe_out, &bytes)?;
        Ok(ScreenshotResult {
            path: safe_out,
            width,
            height,
            bytes: bytes.len(),
        })
    }

    pub async fn pdf(&self, session_id: &str, out_path: &Path) -> Result<PdfResult> {
        let safe_out = self.resolve_path_if_confined(out_path, PathConfinementMode::OutputFile)?;
        validate_output_file_path(&safe_out)?;
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;

        let bytes = export_pdf(managed.driver.as_ref()).await?;
        if bytes.is_empty() || !bytes.starts_with(b"%PDF-") {
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "pdf export produced empty or invalid PDF bytes".to_string(),
            });
        }

        fs::write(&safe_out, &bytes)?;
        Ok(PdfResult {
            path: safe_out,
            bytes: bytes.len(),
        })
    }

    pub async fn logs(&self, session_id: &str, kind: LogKind) -> Result<Value> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        managed.ensure_not_paused()?;
        match kind {
            LogKind::Console => {
                let entries = managed.driver.console_logs().await?;
                let page_errors = managed.driver.page_errors().await?;
                Ok(serde_json::json!({
                    "kind": "console",
                    "entries": entries,
                    "page_errors": page_errors,
                }))
            }
            LogKind::Network => {
                let entries = managed.driver.network_events().await?;
                Ok(serde_json::json!({
                    "kind": "network",
                    "entries": entries,
                }))
            }
        }
    }

    pub async fn env_report(&self, session_id: &str) -> Result<Value> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
        let coherence = managed.coherence_report.clone().unwrap_or_else(|| {
            if let Some(ref prof) = managed.env_profile {
                validate_profile(prof)
            } else {
                CoherenceReport {
                    valid: true,
                    errors: Vec::new(),
                    warnings: Vec::new(),
                }
            }
        });

        Ok(serde_json::json!({
            "session_id": managed.summary.session_id,
            "engine": managed.summary.engine,
            "mode": managed.summary.mode,
            "configured": managed.env_profile.is_some(),
            "coherence": coherence,
            "profile": managed.env_profile,
            "proxy": managed.proxy_redacted,
            "block_policy": managed.block_policy,
            "privacy_mode": managed.privacy_mode,
            "paused_for_human": managed.paused_for_human,
            "pause_reason": managed.pause_reason,
            "instant_animations": managed.instant_animations,
            "main_world_eval": managed.main_world_eval,
        }))
    }

    pub fn profile_list(&self) -> Result<Vec<NamedProfileInfo>> {
        self.profile_store.list_named()
    }

    pub fn profile_delete(&self, name: &str) -> Result<()> {
        self.profile_store.delete_named(name)
    }

    pub async fn shutdown_all(&self) -> Result<()> {
        let entries: Vec<SessionEntry> = {
            let mut reg = self.registry.lock();
            reg.shutting_down = true;
            reg.active.drain().map(|(_, v)| v).collect()
        };

        let mut first_err: Option<MimikyuError> = None;
        for entry in entries {
            if let Err(e) = dispose_session_entry(entry).await
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }

        let slots: Vec<PoolSlot> = {
            let mut pools = self.pools.lock().await;
            pools.drain().map(|(_, slot)| slot).collect()
        };

        for slot in slots {
            let mut guard = slot.lock().await;
            if let Some(pool) = guard.take() {
                dispose_pool_arc(pool).await;
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn lookup_session(&self, session_id: &str) -> Result<SessionEntry> {
        let reg = self.registry.lock();
        reg.active
            .get(session_id)
            .cloned()
            .ok_or_else(|| MimikyuError::NotFound(format!("session '{session_id}' not found")))
    }
}

async fn dispose_session_entry(entry: SessionEntry) -> Result<()> {
    let mut managed = entry.handle.lock().await;
    if managed.disposed {
        return Ok(());
    }
    managed.disposed = true;
    if let Some(ref prof_dir) = managed.persistent_path {
        if let Ok(cookies) = managed.driver.cookies().await {
            let _ = save_session_cookies(prof_dir, &cookies);
        }
        let still_pending: HashSet<String> = managed
            .driver
            .pending_storage_origins()
            .into_iter()
            .collect();
        if let Ok(mut stored_origins) = load_session_origins(prof_dir)
            && !stored_origins.is_empty()
        {
            stored_origins.retain(|o| still_pending.contains(&o.origin));
            let _ = save_session_origins(prof_dir, &stored_origins);
        }
    }
    let res = if let Some(vc) = managed.virtual_ctx.take() {
        vc.dispose().await
    } else {
        managed.driver.dispose().await
    };
    if let Some(dl_dir) = managed.ephemeral_download_dir.take() {
        cleanup_ephemeral_download_dir(&dl_dir);
    }
    res
}

fn safe_session_token(session_id: &str) -> String {
    let is_lowercase_safe = !session_id.is_empty()
        && session_id != "."
        && session_id != ".."
        && !session_id.contains("..")
        && session_id
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_' | '.'));

    if is_lowercase_safe {
        session_id.to_string()
    } else {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in session_id.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("s-{h:016x}")
    }
}

fn safe_session_download_dir(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join("downloads").join(safe_session_token(session_id))
}

fn cleanup_ephemeral_download_dir(dl_dir: &Path) {
    let _ = fs::remove_dir_all(dl_dir);
    if let Some(parent) = dl_dir.parent() {
        let _ = fs::remove_dir(parent);
    }
}

async fn dispose_pool_arc(pool: Arc<VirtualContextPool>) {
    match Arc::try_unwrap(pool) {
        Ok(owned) => {
            let _ = owned.shutdown().await;
        }
        Err(shared) => {
            let _ = shared.dispose_all().await;
            let _ = shared.root_session().dispose().await;
        }
    }
}

async fn execute_batch_with_open_invalidation(
    driver: &dyn DriverSession,
    chokepoint: &ChokepointInput,
    snapshot_state: &mut SnapshotState,
    actions: &[BatchAction],
) -> Result<BatchReport> {
    let total_steps = actions.len();
    let mut results = Vec::with_capacity(total_steps);
    let mut idx = 0;

    while idx < total_steps {
        let slice_start = idx;
        while idx < total_steps && !matches!(actions[idx], BatchAction::Open { .. }) {
            idx += 1;
        }
        if idx > slice_start {
            let sub = ActionBatcher::execute(
                driver,
                chokepoint,
                snapshot_state,
                &actions[slice_start..idx],
            )
            .await?;
            for mut r in sub.results {
                r.step_index += slice_start;
                results.push(r);
            }
            if !sub.ok {
                let err = sub.error.map(|mut e| {
                    e.step_index += slice_start;
                    e
                });
                return Ok(BatchReport {
                    ok: false,
                    completed_steps: slice_start + sub.completed_steps,
                    total_steps,
                    results,
                    error: err,
                });
            }
        }

        if idx < total_steps
            && let BatchAction::Open { ref url } = actions[idx]
        {
            let started = Instant::now();
            let open_res = async {
                let valid = validate_nav_url(url)?;
                snapshot_state.active_ref_map.clear();
                let nav = driver.navigate(&valid).await?;
                Ok::<Value, MimikyuError>(serde_json::to_value(nav)?)
            }
            .await;
            let duration_ms = started.elapsed().as_millis() as u64;

            match open_res {
                Ok(output) => {
                    results.push(BatchStepResult {
                        step_index: idx,
                        action: "open".to_string(),
                        duration_ms,
                        output: Some(output),
                    });
                }
                Err(err) => {
                    let recovery_snapshot =
                        if !matches!(err, MimikyuError::Disposed | MimikyuError::Transport(_)) {
                            SnapshotEngine::capture(
                                driver,
                                snapshot_state,
                                SnapshotOptions::default(),
                            )
                            .await
                            .ok()
                        } else {
                            None
                        };
                    return Ok(BatchReport {
                        ok: false,
                        completed_steps: idx,
                        total_steps,
                        results,
                        error: Some(BatchError {
                            step_index: idx,
                            action: "open".to_string(),
                            message: err.to_string(),
                            recovery_snapshot,
                        }),
                    });
                }
            }
            idx += 1;
        }
    }

    Ok(BatchReport {
        ok: true,
        completed_steps: total_steps,
        total_steps,
        results,
        error: None,
    })
}

pub fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_SESSION_ID_LEN {
        return Err(MimikyuError::InvalidConfig(format!(
            "session_id must be 1..={MAX_SESSION_ID_LEN} characters"
        )));
    }
    if id.trim() != id || id == "." || id == ".." || id.starts_with('-') || id.contains("..") {
        return Err(MimikyuError::InvalidConfig(format!(
            "invalid session_id '{id}'"
        )));
    }
    if id
        .chars()
        .any(|c| c == '/' || c == '\\' || c == '\0' || c.is_control())
    {
        return Err(MimikyuError::InvalidConfig(format!(
            "session_id '{id}' contains disallowed path or control characters"
        )));
    }
    Ok(())
}

pub fn validate_nav_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_control()) {
        return Err(MimikyuError::InvalidConfig(
            "navigation URL cannot be empty or contain control characters".to_string(),
        ));
    }
    let parsed = url::Url::parse(trimmed)?;
    match parsed.scheme() {
        "http" | "https" => {
            if parsed.host_str().is_none_or(|h| h.is_empty()) {
                return Err(MimikyuError::InvalidConfig(format!(
                    "URL '{trimmed}' is missing a host"
                )));
            }
            Ok(trimmed.to_string())
        }
        "data" => {
            if parsed.path().is_empty() || !parsed.path().contains(',') {
                return Err(MimikyuError::InvalidConfig(format!(
                    "malformed data: URL '{trimmed}'"
                )));
            }
            Ok(trimmed.to_string())
        }
        "about" => {
            if parsed.path().is_empty() {
                return Err(MimikyuError::InvalidConfig(format!(
                    "malformed about: URL '{trimmed}'"
                )));
            }
            Ok(trimmed.to_string())
        }
        other => Err(MimikyuError::InvalidConfig(format!(
            "unsupported URL scheme '{other}' (allowed: http, https, data, about)"
        ))),
    }
}

pub fn validate_output_file_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
        return Err(MimikyuError::InvalidConfig(
            "output file path cannot be empty or contain NUL bytes".to_string(),
        ));
    }
    if path.is_dir() {
        return Err(MimikyuError::InvalidConfig(format!(
            "output path '{}' is a directory",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.exists() || !parent.is_dir() {
        return Err(MimikyuError::InvalidConfig(format!(
            "output parent directory '{}' does not exist",
            parent.display()
        )));
    }
    Ok(())
}

/// Resolve `raw_path` and verify that its canonicalized location strictly stays within
/// `allowed_root`, blocking `../` traversal, absolute escapes, symlink escapes, and
/// symlink-leaf overwrites.
pub fn resolve_confined_path(
    raw_path: &Path,
    allowed_root: &Path,
    mode: PathConfinementMode,
) -> Result<PathBuf> {
    if raw_path.as_os_str().is_empty() || raw_path.to_string_lossy().contains('\0') {
        return Err(MimikyuError::InvalidConfig(
            "file path cannot be empty or contain NUL bytes".to_string(),
        ));
    }

    let canonical_root = fs::canonicalize(allowed_root).map_err(|e| {
        MimikyuError::InvalidConfig(format!(
            "allowed root '{}' is not accessible: {e}",
            allowed_root.display()
        ))
    })?;
    if !canonical_root.is_dir() {
        return Err(MimikyuError::InvalidConfig(format!(
            "allowed root '{}' is not a directory",
            allowed_root.display()
        )));
    }

    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        canonical_root.join(raw_path)
    };

    // Walk components incrementally, resolving symlinks as they are encountered so
    // `symlink/..` and dangling symlinks cannot disguise an escape outside `canonical_root`.
    let mut resolved = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.exists()
                    && let Ok(canon) = fs::canonicalize(&resolved)
                {
                    resolved = canon;
                }
                resolved.pop();
            }
            Component::Normal(seg) => {
                resolved.push(seg);
                match fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        let target = fs::canonicalize(&resolved).map_err(|_| {
                            MimikyuError::InvalidConfig(format!(
                                "path '{}' traverses or targets a dangling or unreadable symlink",
                                raw_path.display()
                            ))
                        })?;
                        resolved = target;
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => {
                        if !resolved.starts_with(&canonical_root) {
                            return Err(MimikyuError::InvalidConfig(format!(
                                "path '{}' escapes allowed root '{}'",
                                raw_path.display(),
                                canonical_root.display()
                            )));
                        }
                        return Err(MimikyuError::InvalidConfig(format!(
                            "path '{}' is not accessible: {e}",
                            raw_path.display()
                        )));
                    }
                }
            }
        }
    }

    if resolved.exists()
        && let Ok(canon) = fs::canonicalize(&resolved)
    {
        resolved = canon;
    }

    if !resolved.starts_with(&canonical_root) {
        return Err(MimikyuError::InvalidConfig(format!(
            "path '{}' escapes allowed root '{}'",
            raw_path.display(),
            canonical_root.display()
        )));
    }

    match mode {
        PathConfinementMode::ExistingFile => {
            let meta = fs::metadata(&resolved).map_err(|e| {
                MimikyuError::InvalidConfig(format!(
                    "file '{}' is not accessible: {e}",
                    raw_path.display()
                ))
            })?;
            if !meta.is_file() {
                return Err(MimikyuError::InvalidConfig(format!(
                    "path '{}' is not a regular file",
                    raw_path.display()
                )));
            }
        }
        PathConfinementMode::OutputFile => {
            if let Ok(meta) = fs::symlink_metadata(&candidate)
                && meta.file_type().is_symlink()
            {
                return Err(MimikyuError::InvalidConfig(format!(
                    "output path '{}' cannot be a symlink",
                    raw_path.display()
                )));
            }
            if resolved == canonical_root {
                return Err(MimikyuError::InvalidConfig(format!(
                    "output path '{}' is a directory",
                    raw_path.display()
                )));
            }
            validate_output_file_path(&resolved)?;
        }
        PathConfinementMode::OutputDir => {
            if let Ok(meta) = fs::symlink_metadata(&candidate)
                && meta.file_type().is_symlink()
            {
                return Err(MimikyuError::InvalidConfig(format!(
                    "output directory '{}' cannot be a symlink",
                    raw_path.display()
                )));
            }
            if resolved.exists() && !resolved.is_dir() {
                return Err(MimikyuError::InvalidConfig(format!(
                    "output directory '{}' exists and is not a directory",
                    raw_path.display()
                )));
            }
        }
    }

    Ok(resolved)
}

fn default_host_preset() -> PresetKind {
    if cfg!(target_os = "macos") {
        PresetKind::MacOs
    } else if cfg!(target_os = "windows") {
        PresetKind::Win11
    } else {
        PresetKind::Linux
    }
}

fn build_env_profile(
    opts: &SessionCreateOpts,
    engine: EngineKind,
    has_proxy: bool,
) -> Result<(Option<EnvProfile>, Option<CoherenceReport>)> {
    let needs_profile = opts.preset.is_some()
        || opts.locale.is_some()
        || opts.timezone.is_some()
        || opts.geoip.is_some()
        || opts.viewport.is_some()
        || has_proxy;

    if !needs_profile {
        return Ok((None, None));
    }

    let preset = opts.preset.unwrap_or_else(default_host_preset);
    let mut builder = EnvProfileBuilder::from_preset(preset, engine);

    if let Some(ref geo) = opts.geoip {
        builder = builder.geoip(geo)?;
    }
    if let Some(ref loc) = opts.locale {
        builder = builder.locale(loc)?;
    }
    if let Some(ref tz) = opts.timezone {
        let trimmed = tz.trim();
        if trimmed.is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "timezone cannot be empty".to_string(),
            ));
        }
        builder = builder.timezone(trimmed);
    }
    if let Some((w, h)) = opts.viewport {
        builder = builder.viewport(w, h);
    }
    if has_proxy || opts.privacy_mode {
        builder = builder.webrtc(WebRtcPolicy::DisableNonProxiedUdp);
    }

    let profile = builder.build()?;
    let report = validate_profile(&profile);
    Ok((Some(profile), Some(report)))
}

fn redact_proxy_url(raw: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(raw)
        && (!parsed.username().is_empty() || parsed.password().is_some())
    {
        let _ = parsed.set_username("***");
        let _ = parsed.set_password(None);
        return parsed.to_string();
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_session_id_accepts_safe_ids_and_rejects_unsafe() {
        for ok in ["default", "s1", "work_agent-01.tab", &"a".repeat(128)] {
            assert!(validate_session_id(ok).is_ok(), "should accept {ok:?}");
        }
        for bad in [
            "",
            ".",
            "..",
            "-flag",
            "../escape",
            "a..b",
            "bad/id",
            "bad\\id",
            "bad\nid",
            "bad\0id",
            " lead",
            "trail ",
            &"a".repeat(129),
        ] {
            assert!(validate_session_id(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn validate_nav_url_allows_http_https_data_about_and_rejects_others() {
        assert!(validate_nav_url("http://127.0.0.1:8080/path").is_ok());
        assert!(validate_nav_url("https://localhost:8443/").is_ok());
        assert!(validate_nav_url("data:text/html;charset=utf-8,<h1>hi</h1>").is_ok());
        assert!(validate_nav_url("about:blank").is_ok());

        for bad in [
            "",
            "   ",
            "not a valid url :::",
            "http://127.0.0.1:99999/",
            "ftp://127.0.0.1/file",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html",
        ] {
            assert!(validate_nav_url(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn validate_output_file_path_rejects_dir_and_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_output_file_path(dir.path()).is_err());
        assert!(validate_output_file_path(&dir.path().join("missing/out.png")).is_err());
        assert!(validate_output_file_path(Path::new("")).is_err());
        assert!(validate_output_file_path(&dir.path().join("ok.png")).is_ok());
    }

    #[test]
    fn redact_proxy_url_strips_credentials() {
        assert_eq!(
            redact_proxy_url("http://secretuser:secretpass@127.0.0.1:8080"),
            "http://***@127.0.0.1:8080/"
        );
        assert_eq!(
            redact_proxy_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn session_manager_new_rejects_regular_file_as_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"hi").unwrap();
        let err = SessionManager::new(file).err().expect("must fail");
        assert!(matches!(err, MimikyuError::InvalidConfig(_)));
    }

    #[test]
    fn resolve_confined_path_allows_in_root_and_blocks_traversal_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        let sub = root.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let inside_file = sub.join("ok.txt");
        fs::write(&inside_file, b"ok").unwrap();
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, b"secret").unwrap();

        // In-root relative & absolute paths succeed
        let ok_rel = resolve_confined_path(
            Path::new("sub/ok.txt"),
            root.path(),
            PathConfinementMode::ExistingFile,
        )
        .unwrap();
        assert_eq!(ok_rel, fs::canonicalize(&inside_file).unwrap());

        let ok_abs = resolve_confined_path(
            &inside_file,
            root.path(),
            PathConfinementMode::ExistingFile,
        )
        .unwrap();
        assert_eq!(ok_abs, fs::canonicalize(&inside_file).unwrap());

        let ok_out = resolve_confined_path(
            Path::new("sub/new.png"),
            root.path(),
            PathConfinementMode::OutputFile,
        )
        .unwrap();
        assert!(ok_out.ends_with("sub/new.png"));

        let ok_dir = resolve_confined_path(
            Path::new("frames/run1"),
            root.path(),
            PathConfinementMode::OutputDir,
        )
        .unwrap();
        assert!(ok_dir.ends_with("frames/run1"));

        // Traversal & absolute escapes are blocked across all modes
        for mode in [
            PathConfinementMode::ExistingFile,
            PathConfinementMode::OutputFile,
            PathConfinementMode::OutputDir,
        ] {
            assert!(
                resolve_confined_path(Path::new("../escape.txt"), root.path(), mode).is_err(),
                "must block ../ traversal in mode {mode:?}"
            );
            assert!(
                resolve_confined_path(Path::new("sub/../../escape.txt"), root.path(), mode)
                    .is_err(),
                "must block nested ../ traversal in mode {mode:?}"
            );
            assert!(
                resolve_confined_path(&outside_file, root.path(), mode).is_err(),
                "must block absolute outside path in mode {mode:?}"
            );
            assert!(
                resolve_confined_path(
                    &outside.path().join("nonexistent.txt"),
                    root.path(),
                    mode
                )
                .is_err(),
                "must block nonexistent outside path in mode {mode:?}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            // Symlink inside root -> outside file
            let link_file = root.path().join("link_secret.txt");
            symlink(&outside_file, &link_file).unwrap();
            assert!(resolve_confined_path(
                &link_file,
                root.path(),
                PathConfinementMode::ExistingFile
            )
            .is_err());
            assert!(resolve_confined_path(
                &link_file,
                root.path(),
                PathConfinementMode::OutputFile
            )
            .is_err());

            // Symlink inside root -> outside directory
            let link_dir = root.path().join("link_outside");
            symlink(outside.path(), &link_dir).unwrap();
            assert!(resolve_confined_path(
                &link_dir.join("secret.txt"),
                root.path(),
                PathConfinementMode::ExistingFile
            )
            .is_err());
            assert!(resolve_confined_path(
                &link_dir.join("pwn.png"),
                root.path(),
                PathConfinementMode::OutputFile
            )
            .is_err());
            assert!(resolve_confined_path(
                &link_dir.join("frames"),
                root.path(),
                PathConfinementMode::OutputDir
            )
            .is_err());

            // Dangling symlink inside root -> outside nonexistent target
            let dangling = root.path().join("dangling.png");
            symlink(outside.path().join("pwned.png"), &dangling).unwrap();
            assert!(resolve_confined_path(
                &dangling,
                root.path(),
                PathConfinementMode::OutputFile
            )
            .is_err());

            // Symlink leaf pointing inside root still blocked for OutputFile/OutputDir
            let in_link = root.path().join("in_link.txt");
            symlink(&inside_file, &in_link).unwrap();
            assert!(resolve_confined_path(
                &in_link,
                root.path(),
                PathConfinementMode::OutputFile
            )
            .is_err());
        }
    }

    struct StubGuardDriver {
        info: crate::driver::SessionInfo,
    }

    impl StubGuardDriver {
        fn new() -> Self {
            Self {
                info: crate::driver::SessionInfo {
                    session_id: "stub".to_string(),
                    engine: EngineKind::Chromium,
                    browser_version: "0".to_string(),
                    context_id: None,
                    target_id: "stub".to_string(),
                    profile_dir: None,
                    ws_endpoint: String::new(),
                    pid: None,
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl DriverSession for StubGuardDriver {
        fn info(&self) -> &crate::driver::SessionInfo {
            &self.info
        }
        fn capabilities(&self) -> crate::driver::DriverCapabilities {
            crate::driver::DriverCapabilities {
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
        async fn new_context(&self, _opts: ContextOptions) -> Result<Arc<dyn DriverSession>> {
            Err(MimikyuError::Disposed)
        }
        async fn new_tab(&self, _url: Option<&str>) -> Result<Arc<dyn DriverSession>> {
            Err(MimikyuError::Disposed)
        }
        async fn list_tabs(&self) -> Result<Vec<crate::driver::TabInfo>> {
            Ok(Vec::new())
        }
        async fn activate_tab(&self, _target_id: &str) -> Result<()> {
            Ok(())
        }
        async fn navigate(&self, url: &str) -> Result<NavInfo> {
            Ok(NavInfo {
                url: url.to_string(),
                status: Some(200),
                title: "Stub".to_string(),
                frame_id: None,
            })
        }
        async fn reload(&self) -> Result<NavInfo> {
            self.navigate("https://example.com").await
        }
        async fn go_back(&self) -> Result<NavInfo> {
            self.navigate("https://example.com").await
        }
        async fn go_forward(&self) -> Result<NavInfo> {
            self.navigate("https://example.com").await
        }
        async fn current_url(&self) -> Result<String> {
            Ok("https://example.com".to_string())
        }
        async fn title(&self) -> Result<String> {
            Ok("Stub".to_string())
        }
        async fn eval_isolated(&self, _expr: &str) -> Result<Value> {
            Ok(Value::String("ok".to_string()))
        }
        async fn eval_main_world(&self, _expr: &str) -> Result<Value> {
            Ok(Value::String("ok".to_string()))
        }
        async fn list_frames(&self) -> Result<Vec<crate::driver::FrameInfo>> {
            Ok(Vec::new())
        }
        async fn eval_isolated_in_frame(
            &self,
            _expr: &str,
            _frame_id: Option<&str>,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn eval_main_world_in_frame(
            &self,
            _expr: &str,
            _frame_id: Option<&str>,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn call_function_on_node(
            &self,
            _backend_node_id: i64,
            _frame_id: Option<&str>,
            _function_declaration: &str,
            _args: Vec<Value>,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn query_shadow(
            &self,
            _selector_chain: &[String],
            _frame_id: Option<&str>,
        ) -> Result<Vec<crate::driver::ShadowMatch>> {
            Ok(Vec::new())
        }
        async fn closed_shadow_hosts(&self, _frame_id: Option<&str>) -> Result<Vec<i64>> {
            Ok(Vec::new())
        }
        async fn add_init_script(&self, _script: &str) -> Result<String> {
            Ok(String::new())
        }
        async fn remove_init_script(&self, _script_id: &str) -> Result<()> {
            Ok(())
        }
        async fn accessibility_tree(&self) -> Result<Vec<crate::driver::AxNode>> {
            Ok(Vec::new())
        }
        async fn resolve_node_box(
            &self,
            _backend_node_id: i64,
            _frame_id: Option<&str>,
        ) -> Result<crate::driver::BoundingBox> {
            Err(MimikyuError::ElementNotFound("stub".to_string()))
        }
        async fn resolve_selector_box(&self, _selector: &str) -> Result<crate::driver::BoundingBox> {
            Err(MimikyuError::ElementNotFound("stub".to_string()))
        }
        async fn viewport_size(&self) -> Result<(f64, f64)> {
            Ok((1280.0, 720.0))
        }
        async fn scroll_into_view(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn clear_node_value(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn select_option(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
            _by: SelectBy,
        ) -> Result<String> {
            Ok(String::new())
        }
        async fn focus_node(
            &self,
            _backend_node_id: Option<i64>,
            _selector: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn set_file_input_files(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
            _files: Vec<PathBuf>,
        ) -> Result<()> {
            Ok(())
        }
        async fn css_selector_for_node(
            &self,
            _backend_node_id: i64,
            _frame_id: Option<&str>,
        ) -> Result<Option<String>> {
            Ok(None)
        }
        async fn dispatch_mouse(&self, _ev: crate::driver::MouseEvent) -> Result<()> {
            Ok(())
        }
        async fn dispatch_key(&self, _ev: crate::driver::KeyEvent) -> Result<()> {
            Ok(())
        }
        async fn dispatch_wheel(&self, _ev: crate::driver::WheelEvent) -> Result<()> {
            Ok(())
        }
        async fn insert_text(&self, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _opts: crate::driver::ScreenshotOpts) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn pdf(&self) -> Result<Vec<u8>> {
            Ok(b"%PDF-1.4\n".to_vec())
        }
        async fn set_instant_animations(&self, _enabled: bool) -> Result<()> {
            Ok(())
        }
        async fn start_screencast(&self, opts: ScreencastOptions) -> Result<ScreencastStatus> {
            Ok(ScreencastStatus {
                recording: true,
                out_dir: opts.out_dir,
                format: opts.format,
                frame_count: 0,
                started_at_ms: now_ms(),
                stopped_at_ms: None,
            })
        }
        async fn stop_screencast(&self) -> Result<ScreencastStatus> {
            Ok(ScreencastStatus {
                recording: false,
                out_dir: PathBuf::new(),
                format: crate::driver::ScreencastFormat::Png,
                frame_count: 0,
                started_at_ms: 0,
                stopped_at_ms: Some(now_ms()),
            })
        }
        async fn set_env_overrides(&self, _profile: EnvProfile) -> Result<()> {
            Ok(())
        }
        async fn set_permissions(&self, _permissions: Vec<crate::driver::PermissionGrant>) -> Result<()> {
            Ok(())
        }
        async fn reset_permissions(&self) -> Result<()> {
            Ok(())
        }
        async fn set_block_policy(&self, _policy: BlockPolicy) -> Result<()> {
            Ok(())
        }
        async fn set_blocked_urls(&self, _patterns: Vec<String>) -> Result<()> {
            Ok(())
        }
        async fn set_route_mock(&self, _mock: crate::driver::RouteMock) -> Result<()> {
            Ok(())
        }
        async fn set_route_rule(&self, _rule: crate::network::RouteRule) -> Result<()> {
            Ok(())
        }
        async fn clear_route_mocks(&self) -> Result<()> {
            Ok(())
        }
        async fn cookies(&self) -> Result<Vec<crate::driver::Cookie>> {
            Ok(Vec::new())
        }
        async fn set_cookies(&self, _cookies: Vec<crate::driver::Cookie>) -> Result<()> {
            Ok(())
        }
        async fn clear_cookies(&self) -> Result<()> {
            Ok(())
        }
        async fn storage_state(&self) -> Result<StorageState> {
            Ok(StorageState::default())
        }
        async fn set_storage_state(&self, _state: StorageState) -> Result<()> {
            Ok(())
        }
        async fn console_logs(&self) -> Result<Vec<crate::driver::ConsoleEntry>> {
            Ok(Vec::new())
        }
        async fn page_errors(&self) -> Result<Vec<crate::driver::PageError>> {
            Ok(Vec::new())
        }
        async fn network_events(&self) -> Result<Vec<crate::driver::NetworkEvent>> {
            Ok(Vec::new())
        }
        async fn drain_console_logs(&self) -> Result<Vec<crate::driver::ConsoleEntry>> {
            Ok(Vec::new())
        }
        async fn drain_page_errors(&self) -> Result<Vec<crate::driver::PageError>> {
            Ok(Vec::new())
        }
        async fn drain_network_events(&self) -> Result<Vec<crate::driver::NetworkEvent>> {
            Ok(Vec::new())
        }
        async fn clear_telemetry(&self) -> Result<()> {
            Ok(())
        }
        async fn inflight_requests(&self) -> Result<usize> {
            Ok(0)
        }
        async fn inflight_activity(&self, _ignore_long_lived: bool) -> Result<(usize, u64)> {
            Ok((0, 0))
        }
        async fn document_ready_state(&self) -> Result<String> {
            Ok("complete".to_string())
        }
        async fn dispose(&self) -> Result<()> {
            Ok(())
        }
    }

    fn insert_stub_session(mgr: &SessionManager, session_id: &str) {
        let driver: Arc<dyn DriverSession> = Arc::new(StubGuardDriver::new());
        let summary = SessionSummary {
            session_id: session_id.to_string(),
            engine: EngineKind::Chromium,
            mode: "ephemeral".to_string(),
            url: "https://example.com".to_string(),
            title: "Stub".to_string(),
            pid: None,
            created_at_ms: now_ms(),
            paused_for_human: false,
            pause_reason: None,
        };
        let meta = Arc::new(parking_lot::RwLock::new(summary.clone()));
        let managed = ManagedSession {
            summary,
            meta: Arc::clone(&meta),
            driver: Arc::clone(&driver),
            virtual_ctx: None,
            persistent_path: None,
            ephemeral_download_dir: None,
            chokepoint: ChokepointInput::new(driver),
            snapshot_state: SnapshotState::default(),
            default_humanize: false,
            block_policy: BlockPolicy::default(),
            privacy_mode: false,
            instant_animations: false,
            main_world_eval: true,
            env_profile: None,
            coherence_report: None,
            proxy_redacted: None,
            paused_for_human: false,
            pause_reason: None,
            paused_at_ms: None,
            takeover_id: None,
            disposed: false,
        };
        mgr.registry.lock().active.insert(
            session_id.to_string(),
            SessionEntry {
                meta,
                handle: Arc::new(tokio::sync::Mutex::new(managed)),
            },
        );
    }

    #[tokio::test]
    async fn paused_for_human_blocks_all_inspection_navigation_media_and_storage_ops() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_config(SessionManagerConfig {
            state_dir: dir.path().join("state"),
            allowed_root: Some(dir.path().to_path_buf()),
            require_oob_resume: false,
            ..Default::default()
        })
        .unwrap();
        insert_stub_session(&mgr, "sec");

        let state_in = dir.path().join("in.json");
        fs::write(&state_in, r#"{"cookies":[],"origins":[]}"#).unwrap();
        let state_out = dir.path().join("out.json");
        let shot_out = dir.path().join("shot.png");
        let pdf_out = dir.path().join("out.pdf");
        let frames_out = dir.path().join("frames");

        let pause = mgr
            .takeover_session("sec", Some("enter 2FA code".to_string()))
            .await
            .unwrap();
        assert!(pause.paused_for_human);
        assert!(pause.confirmation_required);
        let token = pause.takeover_id.expect("takeover_id must be generated");
        assert!(token.starts_with("tk-"));

        // Every inspection, navigation, script, media, and storage op must return PausedForHuman
        assert!(matches!(
            mgr.eval("sec", "1+1", false).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.open("sec", "https://example.com/2").await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.back("sec").await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.forward("sec").await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.reload("sec").await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.snapshot("sec", SnapshotOptions::default()).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.extract("sec", ExtractMode::Text).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.screenshot("sec", &shot_out, None, false).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.screenshot_bytes("sec", None, false).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.pdf("sec", &pdf_out).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.export_session("sec", &state_out).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.import_session("sec", &state_in).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.start_screencast(
                "sec",
                ScreencastOptions {
                    out_dir: frames_out,
                    format: crate::driver::ScreencastFormat::Png,
                    quality: None,
                    max_width: None,
                    max_height: None,
                    every_nth_frame: None,
                }
            )
            .await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.wait(
                "sec",
                WaitRequest {
                    idle: true,
                    ..Default::default()
                }
            )
            .await,
            Err(MimikyuError::PausedForHuman { .. })
        ));
        assert!(matches!(
            mgr.logs("sec", LogKind::Network).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));

        // Unconfirmed or mismatched resume attempts must fail and keep session paused
        assert!(mgr
            .resume_session_confirmed("sec", &token, false)
            .await
            .is_err());
        assert!(mgr
            .resume_session_confirmed("sec", "tk-wrong", true)
            .await
            .is_err());
        assert!(mgr.resume_session_confirmed("sec", "", true).await.is_err());
        assert!(matches!(
            mgr.eval("sec", "1+1", false).await,
            Err(MimikyuError::PausedForHuman { .. })
        ));

        // Confirmed resume with matching token succeeds and restores eval/open
        let resumed = mgr
            .resume_session_confirmed("sec", &token, true)
            .await
            .unwrap();
        assert!(!resumed.paused_for_human);
        assert!(!resumed.confirmation_required);
        assert!(resumed.takeover_id.is_none());
        assert_eq!(mgr.eval("sec", "1+1", false).await.unwrap(), Value::String("ok".to_string()));
        assert!(mgr.open("sec", "https://example.com/after").await.is_ok());
    }

    #[tokio::test]
    async fn require_oob_resume_enforces_out_of_band_confirmation_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_config(SessionManagerConfig {
            state_dir: dir.path().join("state"),
            allowed_root: Some(dir.path().to_path_buf()),
            require_oob_resume: true,
            ..Default::default()
        })
        .unwrap();
        insert_stub_session(&mgr, "oob");

        let pause = mgr
            .takeover_session("oob", Some("enter payment details".to_string()))
            .await
            .unwrap();
        let token = pause.takeover_id.unwrap();

        // Even with matching token + confirmed_by_human=true, missing OOB file blocks resume
        assert!(mgr
            .resume_session_confirmed("oob", &token, true)
            .await
            .is_err());

        let conf_path = mgr.takeover_confirmation_path("oob");
        fs::create_dir_all(conf_path.parent().unwrap()).unwrap();
        fs::write(&conf_path, "tk-wrong-token\n").unwrap();
        assert!(mgr
            .resume_session_confirmed("oob", &token, true)
            .await
            .is_err());

        // Writing the exact active takeover_id authorizes one-time resume and removes the marker
        fs::write(&conf_path, format!("{token}\n")).unwrap();
        let resumed = mgr
            .resume_session_confirmed("oob", &token, true)
            .await
            .unwrap();
        assert!(!resumed.paused_for_human);
        assert!(!conf_path.exists(), "OOB confirmation file must be single-use");
    }

    #[tokio::test]
    async fn session_manager_allowed_root_blocks_file_escapes_across_all_file_ops() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.json");
        fs::write(&outside_file, r#"{"cookies":[],"origins":[]}"#).unwrap();

        let mgr = SessionManager::with_config(SessionManagerConfig {
            state_dir: root.path().join("state"),
            allowed_root: Some(root.path().to_path_buf()),
            require_oob_resume: false,
            ..Default::default()
        })
        .unwrap();
        insert_stub_session(&mgr, "conf");

        assert!(matches!(
            mgr.upload("conf", "#f", outside_file.clone()).await,
            Err(MimikyuError::InvalidConfig(_))
        ));
        assert!(matches!(
            mgr.screenshot("conf", &outside.path().join("shot.png"), None, false).await,
            Err(MimikyuError::InvalidConfig(_))
        ));
        assert!(matches!(
            mgr.pdf("conf", &outside.path().join("out.pdf")).await,
            Err(MimikyuError::InvalidConfig(_))
        ));
        assert!(matches!(
            mgr.export_session("conf", &outside.path().join("exp.json")).await,
            Err(MimikyuError::InvalidConfig(_))
        ));
        assert!(matches!(
            mgr.import_session("conf", &outside_file).await,
            Err(MimikyuError::InvalidConfig(_))
        ));
        assert!(matches!(
            mgr.start_screencast(
                "conf",
                ScreencastOptions {
                    out_dir: outside.path().join("frames"),
                    format: crate::driver::ScreencastFormat::Png,
                    quality: None,
                    max_width: None,
                    max_height: None,
                    every_nth_frame: None,
                }
            )
            .await,
            Err(MimikyuError::InvalidConfig(_))
        ));
        assert!(matches!(
            mgr.batch(
                "conf",
                vec![BatchAction::Upload {
                    target: "#f".to_string(),
                    files: vec![outside_file],
                }]
            )
            .await,
            Err(MimikyuError::InvalidConfig(_))
        ));
    }
}
