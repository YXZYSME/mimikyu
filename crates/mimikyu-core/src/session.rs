use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver::{
    driver_for, now_ms, ContextOptions, DriverSession, EngineKind, EnvProfile, LaunchOptions,
    NavInfo, OriginStorage, ProfileMode, ProxyConfig, ScreenshotFormat, SelectBy, StorageState,
    WebRtcPolicy,
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
    instant_animations: bool,
    main_world_eval: bool,
    env_profile: Option<EnvProfile>,
    coherence_report: Option<CoherenceReport>,
    proxy_redacted: Option<String>,
    disposed: bool,
}

impl ManagedSession {
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
            config,
            registry: Arc::new(parking_lot::Mutex::new(SessionRegistry::default())),
            pools: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.config.state_dir
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
            if resolved.path.exists() && !resolved.path.is_dir() {
                return Err(MimikyuError::InvalidConfig(format!(
                    "profile path '{}' exists and is not a directory",
                    resolved.path.display()
                )));
            }
            fs::create_dir_all(&resolved.path)?;
            if ProfileLock::is_locked(&resolved.path)? {
                return Err(MimikyuError::ProfileLocked(format!(
                    "profile '{}' is currently locked by another session",
                    resolved.path.display()
                )));
            }
            Some(resolved.path)
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

        let block_policy = BlockPolicy {
            block_images: opts.block_images,
            block_media: opts.block_media,
            block_fonts: opts.block_fonts,
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
            instant_animations: opts.instant_animations,
            main_world_eval: opts.main_world_eval,
            env_profile,
            coherence_report,
            proxy_redacted,
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

    pub async fn export_session(&self, session_id: &str, out_path: &Path) -> Result<()> {
        validate_output_file_path(out_path)?;
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
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
            save_storage_state(out_path, &state)?;
        } else {
            export_storage_state(managed.driver.as_ref(), out_path).await?;
        }
        Ok(())
    }

    pub async fn import_session(&self, session_id: &str, in_path: &Path) -> Result<()> {
        if in_path.as_os_str().is_empty() || in_path.is_dir() || !in_path.exists() {
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
        let state = import_storage_state(managed.driver.as_ref(), in_path).await?;
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
        let meta = fs::metadata(&path).map_err(|e| {
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
        let tgt = InputTarget::parse(target);
        managed
            .chokepoint
            .set_input_files(
                &tgt,
                Some(&managed.snapshot_state.active_ref_map),
                vec![path],
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

    pub async fn batch(&self, session_id: &str, actions: Vec<BatchAction>) -> Result<BatchReport> {
        let entry = self.lookup_session(session_id)?;
        let mut managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }

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
        if main_world {
            if !managed.main_world_eval {
                return Err(MimikyuError::MainWorldDisabled);
            }
            managed.driver.eval_main_world(js).await
        } else {
            managed.driver.eval_isolated(js).await
        }
    }

    pub async fn screenshot(
        &self,
        session_id: &str,
        out_path: &Path,
        target: Option<&str>,
        full_page: bool,
    ) -> Result<ScreenshotResult> {
        validate_output_file_path(out_path)?;
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }

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

        fs::write(out_path, &capture.bytes)?;
        Ok(ScreenshotResult {
            path: out_path.to_path_buf(),
            width: capture.width,
            height: capture.height,
            bytes: capture.bytes.len(),
        })
    }

    pub async fn pdf(&self, session_id: &str, out_path: &Path) -> Result<PdfResult> {
        validate_output_file_path(out_path)?;
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }

        let bytes = export_pdf(managed.driver.as_ref()).await?;
        if bytes.is_empty() || !bytes.starts_with(b"%PDF-") {
            return Err(MimikyuError::Protocol {
                code: -1,
                message: "pdf export produced empty or invalid PDF bytes".to_string(),
            });
        }

        fs::write(out_path, &bytes)?;
        Ok(PdfResult {
            path: out_path.to_path_buf(),
            bytes: bytes.len(),
        })
    }

    pub async fn logs(&self, session_id: &str, kind: LogKind) -> Result<Value> {
        let entry = self.lookup_session(session_id)?;
        let managed = entry.handle.lock().await;
        if managed.disposed {
            return Err(MimikyuError::Disposed);
        }
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

fn safe_session_download_dir(state_dir: &Path, session_id: &str) -> PathBuf {
    let is_lowercase_safe = !session_id.is_empty()
        && session_id != "."
        && session_id != ".."
        && !session_id.contains("..")
        && session_id
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_' | '.'));

    let safe_id = if is_lowercase_safe {
        session_id.to_string()
    } else {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in session_id.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("s-{h:016x}")
    };
    state_dir.join("downloads").join(safe_id)
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
    if path.as_os_str().is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "output file path cannot be empty".to_string(),
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
    if has_proxy {
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
}
