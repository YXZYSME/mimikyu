pub mod conditions;
pub mod idle;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::driver::DriverSession;
use crate::error::{MimikyuError, Result};
use crate::snapshot::RefMap;

pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 25;
pub const DEFAULT_NETWORK_QUIET_MS: u64 = 250;
pub const DEFAULT_BBOX_STABLE_FRAMES: u32 = 2;

pub(crate) fn compute_wait_deadline(
    timeout_ms: u64,
    poll_interval_ms: u64,
) -> Result<(Instant, Duration)> {
    if timeout_ms > MAX_WAIT_TIMEOUT_MS {
        return Err(MimikyuError::InvalidConfig(format!(
            "wait timeout_ms {timeout_ms} exceeds maximum {MAX_WAIT_TIMEOUT_MS}ms"
        )));
    }
    if poll_interval_ms > MAX_WAIT_TIMEOUT_MS {
        return Err(MimikyuError::InvalidConfig(format!(
            "wait poll_interval_ms {poll_interval_ms} exceeds maximum {MAX_WAIT_TIMEOUT_MS}ms"
        )));
    }
    let dur = Duration::from_millis(timeout_ms);
    let deadline = Instant::now().checked_add(dur).ok_or_else(|| {
        MimikyuError::InvalidConfig(format!("wait timeout_ms {timeout_ms} overflows Instant"))
    })?;
    let poll = Duration::from_millis(poll_interval_ms.max(5));
    Ok((deadline, poll))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitOptions {
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_WAIT_TIMEOUT_MS,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitIdleOptions {
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub network_quiet_ms: u64,
    pub ignore_long_lived_sockets: bool,
    pub stable_target: Option<String>,
    pub bbox_stable_frames: u32,
}

impl Default for WaitIdleOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_WAIT_TIMEOUT_MS,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            network_quiet_ms: DEFAULT_NETWORK_QUIET_MS,
            ignore_long_lived_sockets: true,
            stable_target: None,
            bbox_stable_frames: DEFAULT_BBOX_STABLE_FRAMES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WaitSelectorState {
    Attached,
    #[default]
    Visible,
    Hidden,
    Detached,
}

pub struct WaitEngine;

impl WaitEngine {
    pub async fn wait_idle(
        session: &dyn DriverSession,
        ref_map: Option<&RefMap>,
        opts: WaitIdleOptions,
    ) -> Result<()> {
        idle::wait_idle(session, ref_map, opts).await
    }

    pub async fn wait_text(
        session: &dyn DriverSession,
        text: &str,
        opts: WaitOptions,
    ) -> Result<()> {
        conditions::wait_text(session, text, opts).await
    }

    pub async fn wait_url(
        session: &dyn DriverSession,
        substr: &str,
        opts: WaitOptions,
    ) -> Result<()> {
        conditions::wait_url(session, substr, opts).await
    }

    pub async fn wait_selector(
        session: &dyn DriverSession,
        selector: &str,
        state: WaitSelectorState,
        opts: WaitOptions,
    ) -> Result<()> {
        conditions::wait_selector(session, selector, state, opts).await
    }
}
