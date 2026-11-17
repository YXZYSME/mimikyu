use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::driver::{
    driver_for, now_ms, ContextOptions, DriverSession, EngineKind, LaunchOptions, ProfileMode,
};
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualContextInfo {
    pub id: String,
    pub label: Option<String>,
    pub engine: EngineKind,
    pub target_id: String,
    pub created_at_ms: u64,
    pub create_duration: Duration,
}

/// Isolated lightweight browser context inside a warm shared browser process.
pub struct VirtualContext {
    session: Arc<dyn DriverSession>,
    info: VirtualContextInfo,
    registry: Weak<Mutex<HashMap<String, Weak<dyn DriverSession>>>>,
    disposed: AtomicBool,
}

impl VirtualContext {
    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn info(&self) -> &VirtualContextInfo {
        &self.info
    }

    pub fn create_duration(&self) -> Duration {
        self.info.create_duration
    }

    pub fn session(&self) -> &Arc<dyn DriverSession> {
        &self.session
    }

    pub async fn new_tab(&self, url: Option<&str>) -> Result<Arc<dyn DriverSession>> {
        self.session.new_tab(url).await
    }

    pub async fn dispose(&self) -> Result<()> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(reg) = self.registry.upgrade() {
            reg.lock().remove(&self.info.id);
        }
        self.session.dispose().await
    }
}

impl Drop for VirtualContext {
    fn drop(&mut self) {
        if let Some(reg) = self.registry.upgrade() {
            reg.lock().remove(&self.info.id);
        }
        if !self.disposed.swap(true, Ordering::SeqCst)
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let s = Arc::clone(&self.session);
            handle.spawn(async move {
                let _ = s.dispose().await;
            });
        }
    }
}

/// Manages a warm browser process hosting isolated `VirtualContext`s.
pub struct VirtualContextPool {
    engine: EngineKind,
    root_session: Arc<dyn DriverSession>,
    active: Arc<Mutex<HashMap<String, Weak<dyn DriverSession>>>>,
}

impl VirtualContextPool {
    pub async fn launch(mut opts: LaunchOptions) -> Result<Self> {
        opts.profile_mode = ProfileMode::VirtualContext;
        let engine = opts.engine;
        let root_session = driver_for(engine).launch(opts).await?;
        Ok(Self::from_root(root_session))
    }

    pub fn from_root(root_session: Arc<dyn DriverSession>) -> Self {
        let engine = root_session.info().engine;
        Self {
            engine,
            root_session,
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn engine(&self) -> EngineKind {
        self.engine
    }

    pub fn root_session(&self) -> &Arc<dyn DriverSession> {
        &self.root_session
    }

    pub async fn create_context(&self, opts: ContextOptions) -> Result<VirtualContext> {
        self.create_context_inner(None, opts).await
    }

    pub async fn create_labeled_context(
        &self,
        label: impl Into<String>,
        opts: ContextOptions,
    ) -> Result<VirtualContext> {
        self.create_context_inner(Some(label.into()), opts).await
    }

    async fn create_context_inner(
        &self,
        label: Option<String>,
        opts: ContextOptions,
    ) -> Result<VirtualContext> {
        let t0 = Instant::now();
        let session = self.root_session.new_context(opts).await?;
        let create_duration = t0.elapsed();

        let sinfo = session.info();
        let id = sinfo
            .context_id
            .clone()
            .unwrap_or_else(|| sinfo.target_id.clone());

        self.active
            .lock()
            .insert(id.clone(), Arc::downgrade(&session));

        let info = VirtualContextInfo {
            id,
            label,
            engine: self.engine,
            target_id: sinfo.target_id.clone(),
            created_at_ms: now_ms(),
            create_duration,
        };

        Ok(VirtualContext {
            session,
            info,
            registry: Arc::downgrade(&self.active),
            disposed: AtomicBool::new(false),
        })
    }

    pub fn active_count(&self) -> usize {
        let mut guard = self.active.lock();
        guard.retain(|_, w| w.strong_count() > 0);
        guard.len()
    }

    pub fn active_ids(&self) -> Vec<String> {
        let mut guard = self.active.lock();
        guard.retain(|_, w| w.strong_count() > 0);
        let mut ids: Vec<String> = guard.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub async fn dispose_all(&self) -> Result<()> {
        let sessions: Vec<Arc<dyn DriverSession>> = {
            let mut guard = self.active.lock();
            guard
                .drain()
                .filter_map(|(_, weak)| weak.upgrade())
                .collect()
        };
        for s in sessions {
            let _ = s.dispose().await;
        }
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        self.dispose_all().await?;
        self.root_session.dispose().await
    }
}
