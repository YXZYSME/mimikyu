use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::discovery::{discover_browser, ChromiumFlavor, DiscoveredBrowser};
use crate::driver::{EngineKind, LaunchOptions, ProfileMode, WebRtcPolicy};
use crate::error::{MimikyuError, Result};
use crate::isolation::{EphemeralDir, ProfileLock};

/// Owned browser child process isolated in its own POSIX process group (`setpgid(0, 0)`).
///
/// Guarantees zero orphan processes and zero leftover ephemeral temp directories on both
/// async `shutdown()` and synchronous `Drop` (including panics or cancelled tasks).
pub struct ProcessGuard {
    pid: u32,
    pgid: libc::pid_t,
    engine: EngineKind,
    browser_version: String,
    ws_endpoint: String,
    profile_dir: PathBuf,
    ephemeral_dir: Option<EphemeralDir>,
    profile_lock: Option<ProfileLock>,
    child: Option<Child>,
    stderr_drain: Option<JoinHandle<()>>,
    reaped: bool,
}

pub type BrowserProcess = ProcessGuard;

impl ProcessGuard {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn pgid(&self) -> libc::pid_t {
        self.pgid
    }

    pub fn engine(&self) -> EngineKind {
        self.engine
    }

    pub fn browser_version(&self) -> &str {
        &self.browser_version
    }

    pub fn ws_endpoint(&self) -> &str {
        &self.ws_endpoint
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    /// Launch a debloated Chromium or Firefox browser process and wait for its WebSocket endpoint.
    pub async fn launch(opts: &LaunchOptions) -> Result<Self> {
        let browser =
            discover_browser(opts.engine, opts.headless, opts.binary_path.as_deref())?;

        let (profile_dir, ephemeral_dir, profile_lock) = match &opts.profile_mode {
            ProfileMode::EphemeralDir | ProfileMode::VirtualContext => {
                let dir = EphemeralDir::new()?;
                let p = dir.path().to_path_buf();
                (p, Some(dir), None)
            }
            ProfileMode::Persistent(path) => {
                let lock = ProfileLock::try_acquire(path)?;
                (path.clone(), None, Some(lock))
            }
        };

        match opts.engine {
            EngineKind::Chromium => {
                Self::launch_chromium(&browser, opts, profile_dir, ephemeral_dir, profile_lock)
                    .await
            }
            EngineKind::Firefox => {
                Self::launch_firefox(&browser, opts, profile_dir, ephemeral_dir, profile_lock)
                    .await
            }
        }
    }

    async fn launch_chromium(
        browser: &DiscoveredBrowser,
        opts: &LaunchOptions,
        profile_dir: PathBuf,
        ephemeral_dir: Option<EphemeralDir>,
        profile_lock: Option<ProfileLock>,
    ) -> Result<Self> {
        let mut args = build_chromium_args(browser, opts, &profile_dir);
        args.extend(opts.extra_args.iter().cloned());
        args.push("about:blank".to_string());

        remove_stale_marker(&profile_dir.join("DevToolsActivePort"))?;

        let mut cmd = Command::new(&browser.path);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| {
            MimikyuError::LaunchFailed(format!(
                "failed to spawn Chromium at {}: {e}",
                browser.path.display()
            ))
        })?;

        let pid = child.id().ok_or_else(|| {
            MimikyuError::LaunchFailed("Chromium exited immediately after spawn".to_string())
        })?;
        let pgid = pid as libc::pid_t;

        let stderr = child.stderr.take();
        let timeout_dur = Duration::from_millis(opts.launch_timeout_ms.max(5_000));

        let mut guard = Self {
            pid,
            pgid,
            engine: EngineKind::Chromium,
            browser_version: browser.version.clone(),
            ws_endpoint: String::new(),
            profile_dir: profile_dir.clone(),
            ephemeral_dir,
            profile_lock,
            child: Some(child),
            stderr_drain: None,
            reaped: false,
        };

        match wait_for_chromium_ws(guard.child.as_mut(), stderr, &profile_dir, timeout_dur).await {
            Ok((ws_url, drain_task)) => {
                guard.ws_endpoint = ws_url;
                guard.stderr_drain = Some(drain_task);
                Ok(guard)
            }
            Err(e) => {
                let _ = guard.force_kill_and_reap().await;
                Err(e)
            }
        }
    }

    async fn launch_firefox(
        browser: &DiscoveredBrowser,
        opts: &LaunchOptions,
        profile_dir: PathBuf,
        ephemeral_dir: Option<EphemeralDir>,
        profile_lock: Option<ProfileLock>,
    ) -> Result<Self> {
        write_firefox_user_js(&profile_dir, opts)?;
        remove_stale_marker(&profile_dir.join("WebDriverBiDiServer.json"))?;

        let mut args = Vec::new();
        if opts.headless {
            args.push("--headless".to_string());
        }
        args.push("--remote-debugging-port=0".to_string());
        args.push("-profile".to_string());
        args.push(profile_dir.to_string_lossy().to_string());
        args.push("-no-remote".to_string());
        args.extend(opts.extra_args.iter().cloned());

        let mut cmd = Command::new(&browser.path);
        cmd.args(&args)
            .env("MOZ_REMOTE_SETTINGS_DEVTOOLS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        if opts.profile_mode != ProfileMode::VirtualContext
            && let Some(ref env) = opts.env_profile
            && !env.timezone.is_empty()
        {
            cmd.env("TZ", &env.timezone);
        }

        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| {
            MimikyuError::LaunchFailed(format!(
                "failed to spawn Firefox at {}: {e}",
                browser.path.display()
            ))
        })?;

        let pid = child.id().ok_or_else(|| {
            MimikyuError::LaunchFailed("Firefox exited immediately after spawn".to_string())
        })?;
        let pgid = pid as libc::pid_t;

        let stderr = child.stderr.take();
        let timeout_dur = Duration::from_millis(opts.launch_timeout_ms.max(5_000));

        let mut guard = Self {
            pid,
            pgid,
            engine: EngineKind::Firefox,
            browser_version: browser.version.clone(),
            ws_endpoint: String::new(),
            profile_dir: profile_dir.clone(),
            ephemeral_dir,
            profile_lock,
            child: Some(child),
            stderr_drain: None,
            reaped: false,
        };

        match wait_for_firefox_ws(guard.child.as_mut(), stderr, &profile_dir, timeout_dur).await {
            Ok((ws_url, drain_task)) => {
                guard.ws_endpoint = ws_url;
                guard.stderr_drain = Some(drain_task);
                Ok(guard)
            }
            Err(e) => {
                let _ = guard.force_kill_and_reap().await;
                Err(e)
            }
        }
    }

    /// Gracefully wait up to `grace_timeout` for the process to exit (after protocol `close`),
    /// then escalate to `SIGTERM` on `-pgid`, then `SIGKILL` on `-pgid`, reap, and clean up temp dir.
    pub async fn shutdown(&mut self, grace_timeout: Duration) -> Result<()> {
        if self.reaped {
            drop(self.ephemeral_dir.take());
            drop(self.profile_lock.take());
            return Ok(());
        }

        if let Some(ref mut child) = self.child {
            if grace_timeout > Duration::ZERO
                && let Ok(Ok(_status)) = tokio::time::timeout(grace_timeout, child.wait()).await
            {
                wait_for_pgid_gone(self.pgid, Duration::from_millis(300)).await;
                if !is_pgid_gone(self.pgid) {
                    unsafe {
                        libc::kill(-self.pgid, libc::SIGKILL);
                    }
                    wait_for_pgid_gone(self.pgid, Duration::from_millis(500)).await;
                }
                if let Some(h) = self.stderr_drain.take() {
                    h.abort();
                }
                self.reaped = true;
                drop(self.ephemeral_dir.take());
                drop(self.profile_lock.take());
                return Ok(());
            }

            unsafe {
                libc::kill(-self.pgid, libc::SIGTERM);
            }
            if let Ok(Ok(_status)) =
                tokio::time::timeout(Duration::from_millis(1000), child.wait()).await
            {
                wait_for_pgid_gone(self.pgid, Duration::from_millis(300)).await;
                if !is_pgid_gone(self.pgid) {
                    unsafe {
                        libc::kill(-self.pgid, libc::SIGKILL);
                    }
                    wait_for_pgid_gone(self.pgid, Duration::from_millis(500)).await;
                }
                if let Some(h) = self.stderr_drain.take() {
                    h.abort();
                }
                self.reaped = true;
                drop(self.ephemeral_dir.take());
                drop(self.profile_lock.take());
                return Ok(());
            }
        }

        self.force_kill_and_reap().await
    }

    async fn force_kill_and_reap(&mut self) -> Result<()> {
        if let Some(h) = self.stderr_drain.take() {
            h.abort();
        }
        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }
        if let Some(ref mut child) = self.child {
            let _ = tokio::time::timeout(Duration::from_millis(2000), child.wait()).await;
        }
        wait_for_pgid_gone(self.pgid, Duration::from_millis(500)).await;
        self.reaped = true;
        drop(self.ephemeral_dir.take());
        drop(self.profile_lock.take());
        Ok(())
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(h) = self.stderr_drain.take() {
            h.abort();
        }
        if self.reaped {
            drop(self.ephemeral_dir.take());
            drop(self.profile_lock.take());
            return;
        }

        // Send SIGTERM to entire process group first, poll briefly, then escalate to SIGKILL.
        unsafe {
            libc::kill(-self.pgid, libc::SIGTERM);
        }

        let term_deadline = Instant::now() + Duration::from_millis(150);
        let mut main_exited = false;
        while Instant::now() < term_deadline {
            if let Some(ref mut child) = self.child {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        main_exited = true;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        main_exited = true;
                    }
                }
            } else {
                main_exited = true;
            }
            if main_exited && is_pgid_gone(self.pgid) {
                self.reaped = true;
                drop(self.ephemeral_dir.take());
                drop(self.profile_lock.take());
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }

        let kill_deadline = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < kill_deadline {
            if let Some(ref mut child) = self.child {
                match child.try_wait() {
                    Ok(Some(_)) => main_exited = true,
                    Ok(None) => {}
                    Err(_) => main_exited = true,
                }
            } else {
                main_exited = true;
            }
            if main_exited && is_pgid_gone(self.pgid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        if !main_exited {
            let mut status: libc::c_int = 0;
            unsafe {
                libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG);
            }
        }

        self.reaped = true;
        drop(self.ephemeral_dir.take());
        drop(self.profile_lock.take());
    }
}

fn is_pgid_gone(pgid: libc::pid_t) -> bool {
    let rc = unsafe { libc::kill(-pgid, 0) };
    if rc == 0 {
        return false;
    }
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::ESRCH)
}

async fn wait_for_pgid_gone(pgid: libc::pid_t, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if is_pgid_gone(pgid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn build_chromium_args(
    browser: &DiscoveredBrowser,
    opts: &LaunchOptions,
    profile_dir: &Path,
) -> Vec<String> {
    let (win_w, win_h) = opts
        .env_profile
        .as_ref()
        .map(|e| (e.viewport_width.max(100), e.viewport_height.max(100)))
        .unwrap_or((1280, 800));

    let mut args = vec![
        "--remote-debugging-port=0".to_string(),
        format!("--user-data-dir={}", profile_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-backgrounding-occluded-windows".to_string(),
        "--disable-renderer-backgrounding".to_string(),
        "--disable-breakpad".to_string(),
        "--disable-client-side-phishing-detection".to_string(),
        "--disable-component-extensions-with-background-pages".to_string(),
        "--disable-component-update".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--disable-domain-reliability".to_string(),
        "--disable-extensions".to_string(),
        "--disable-features=Translate,BackForwardCache,AcceptCHFrame,MediaRouter,OptimizationHints,InterestFeedContentSuggestions,AutofillServerCommunication,CertificateTransparencyComponentUpdater".to_string(),
        "--disable-hang-monitor".to_string(),
        "--disable-ipc-flooding-protection".to_string(),
        "--disable-popup-blocking".to_string(),
        "--disable-prompt-on-repost".to_string(),
        "--disable-sync".to_string(),
        "--metrics-recording-only".to_string(),
        // Mandatory on macOS so full Chrome / Chrome for Testing does not block on Keychain prompt
        "--password-store=basic".to_string(),
        "--use-mock-keychain".to_string(),
        "--hide-scrollbars".to_string(),
        "--mute-audio".to_string(),
        format!("--window-size={win_w},{win_h}"),
    ];

    if opts.headless && browser.kind != ChromiumFlavor::HeadlessShell {
        args.push("--headless=new".to_string());
    }

    if let Some(ref env) = opts.env_profile {
        if env.disable_automation_controlled {
            args.push("--disable-blink-features=AutomationControlled".to_string());
        }
        if !env.locale.is_empty() {
            args.push(format!("--lang={}", env.locale));
        }
        match env.webrtc_policy {
            WebRtcPolicy::Default => {}
            WebRtcPolicy::DisableNonProxiedUdp => {
                args.push("--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string());
            }
            WebRtcPolicy::DisableAll => {
                args.push("--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string());
                args.push("--enforce-webrtc-ip-permission-check".to_string());
            }
        }
    }

    if let Some(ref proxy) = opts.proxy
        && opts.profile_mode != ProfileMode::VirtualContext
    {
        args.push(format!(
            "--proxy-server={}",
            normalize_chromium_proxy_server(&proxy.server)
        ));
        let bypass_list = chromium_proxy_bypass_list(&proxy.bypass);
        args.push(format!("--proxy-bypass-list={bypass_list}"));
    }

    args
}

pub(crate) fn normalize_chromium_proxy_server(raw: &str) -> String {
    if let Some(idx) = raw.find("://") {
        let scheme = &raw[..idx];
        if scheme.eq_ignore_ascii_case("socks5h") {
            return format!("socks5{}", &raw[idx..]);
        }
    }
    raw.to_string()
}

pub(crate) fn chromium_proxy_bypass_list(bypass: &[String]) -> String {
    let mut items = Vec::with_capacity(bypass.len() + 1);
    if !bypass.iter().any(|b| b.eq_ignore_ascii_case("<-loopback>")) {
        items.push("<-loopback>".to_string());
    }
    items.extend(bypass.iter().cloned());
    items.join(",")
}

async fn drain_remaining_stderr(
    reader: &mut Option<tokio::io::Lines<BufReader<tokio::process::ChildStderr>>>,
    collected_stderr: &mut Vec<String>,
) {
    if let Some(lines) = reader {
        while collected_stderr.len() < 20 {
            match tokio::time::timeout(Duration::from_millis(20), lines.next_line()).await {
                Ok(Ok(Some(line))) => collected_stderr.push(line),
                _ => break,
            }
        }
    }
}

async fn wait_for_chromium_ws(
    mut child: Option<&mut Child>,
    stderr: Option<tokio::process::ChildStderr>,
    profile_dir: &Path,
    timeout: Duration,
) -> Result<(String, JoinHandle<()>)> {
    let active_port_path = profile_dir.join("DevToolsActivePort");
    let mut reader = stderr.map(|s| BufReader::new(s).lines());
    let mut collected_stderr = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Some(ws) = try_read_devtools_active_port(&active_port_path) {
            let drain = spawn_stderr_drain(reader);
            return Ok((ws, drain));
        }

        if let Some(c) = child.as_deref_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            drain_remaining_stderr(&mut reader, &mut collected_stderr).await;
            let tail = collected_stderr.join(" | ");
            return Err(MimikyuError::LaunchFailed(format!(
                "Chromium exited immediately with {status} before publishing DevTools WS endpoint ({tail})"
            )));
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let tail = collected_stderr.join(" | ");
            return Err(MimikyuError::Timeout {
                timeout_ms: timeout.as_millis() as u64,
                operation: format!("waiting for Chromium DevTools WS endpoint ({tail})"),
            });
        }

        let step = remaining.min(Duration::from_millis(25));
        if let Some(ref mut lines) = reader {
            match tokio::time::timeout(step, lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    debug!(target: "mimikyu::process::chromium", "stderr: {line}");
                    if let Some(idx) = line.find("DevTools listening on ") {
                        let url = line[idx + "DevTools listening on ".len()..].trim().to_string();
                        if url.starts_with("ws://") || url.starts_with("wss://") {
                            let drain = spawn_stderr_drain(reader);
                            return Ok((url, drain));
                        }
                    }
                    if collected_stderr.len() < 20 {
                        collected_stderr.push(line);
                    }
                }
                Ok(Ok(None)) => {
                    reader = None;
                    if let Some(c) = child.as_deref_mut()
                        && let Ok(Some(status)) = c.try_wait()
                    {
                        let tail = collected_stderr.join(" | ");
                        return Err(MimikyuError::LaunchFailed(format!(
                            "Chromium exited immediately with {status} before publishing DevTools WS endpoint ({tail})"
                        )));
                    }
                }
                Ok(Err(e)) => {
                    warn!("error reading Chromium stderr: {e}");
                    reader = None;
                }
                Err(_) => {
                    // Timeout on line read; loop will check DevToolsActivePort fallback.
                }
            }
        } else {
            tokio::time::sleep(step).await;
        }
    }
}

fn remove_stale_marker(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MimikyuError::Io(e)),
    }
}

fn try_read_devtools_active_port(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let ws_path = lines.next()?.trim();
    if ws_path.starts_with('/') {
        Some(format!("ws://127.0.0.1:{port}{ws_path}"))
    } else {
        None
    }
}

async fn wait_for_firefox_ws(
    mut child: Option<&mut Child>,
    stderr: Option<tokio::process::ChildStderr>,
    profile_dir: &Path,
    timeout: Duration,
) -> Result<(String, JoinHandle<()>)> {
    let bidi_file = profile_dir.join("WebDriverBiDiServer.json");
    let mut reader = stderr.map(|s| BufReader::new(s).lines());
    let mut collected_stderr = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Some(ws) = try_read_firefox_bidi_json(&bidi_file) {
            let drain = spawn_stderr_drain(reader);
            return Ok((ws, drain));
        }

        if let Some(c) = child.as_deref_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            drain_remaining_stderr(&mut reader, &mut collected_stderr).await;
            let tail = collected_stderr.join(" | ");
            return Err(MimikyuError::LaunchFailed(format!(
                "Firefox exited immediately with {status} before publishing WebDriver BiDi endpoint ({tail})"
            )));
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let tail = collected_stderr.join(" | ");
            return Err(MimikyuError::Timeout {
                timeout_ms: timeout.as_millis() as u64,
                operation: format!("waiting for Firefox WebDriver BiDi endpoint ({tail})"),
            });
        }

        let step = remaining.min(Duration::from_millis(25));
        if let Some(ref mut lines) = reader {
            match tokio::time::timeout(step, lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    debug!(target: "mimikyu::process::firefox", "stderr: {line}");
                    if let Some(idx) = line.find("WebDriver BiDi listening on ") {
                        let url = line[idx + "WebDriver BiDi listening on ".len()..]
                            .trim()
                            .trim_end_matches('/')
                            .to_string();
                        if url.starts_with("ws://") || url.starts_with("wss://") {
                            let drain = spawn_stderr_drain(reader);
                            return Ok((url, drain));
                        }
                    }
                    if collected_stderr.len() < 20 {
                        collected_stderr.push(line);
                    }
                }
                Ok(Ok(None)) => {
                    reader = None;
                    if let Some(c) = child.as_deref_mut()
                        && let Ok(Some(status)) = c.try_wait()
                    {
                        let tail = collected_stderr.join(" | ");
                        return Err(MimikyuError::LaunchFailed(format!(
                            "Firefox exited immediately with {status} before publishing WebDriver BiDi endpoint ({tail})"
                        )));
                    }
                }
                Ok(Err(e)) => {
                    warn!("error reading Firefox stderr: {e}");
                    reader = None;
                }
                Err(_) => {}
            }
        } else {
            tokio::time::sleep(step).await;
        }
    }
}

fn try_read_firefox_bidi_json(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let host = val.get("ws_host")?.as_str()?;
    let port = val.get("ws_port")?.as_u64()?;
    if port == 0 {
        return None;
    }
    Some(format!("ws://{host}:{port}"))
}

fn spawn_stderr_drain(
    reader: Option<tokio::io::Lines<BufReader<tokio::process::ChildStderr>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Some(mut lines) = reader {
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "mimikyu::process::stderr", "{line}");
            }
        }
    })
}

/// Write R3-compliant debloated `user.js` into the Firefox profile directory before launch.
pub fn write_firefox_user_js(profile_dir: &Path, opts: &LaunchOptions) -> Result<()> {
    let mut prefs = String::from(FIREFOX_DEBLOATED_USER_JS);

    if let Some(ref env) = opts.env_profile {
        if !env.user_agent.is_empty() {
            prefs.push_str(&format!(
                "user_pref(\"general.useragent.override\", {});\n",
                serde_json::to_string(&env.user_agent)?
            ));
        }
        let accept_langs = if !env.languages.is_empty() {
            env.languages.join(", ")
        } else {
            strip_q_weights(&env.accept_language)
        };
        if !accept_langs.is_empty() {
            prefs.push_str(&format!(
                "user_pref(\"intl.accept_languages\", {});\n",
                serde_json::to_string(&accept_langs)?
            ));
        }
        if !env.locale.is_empty() {
            prefs.push_str(&format!(
                "user_pref(\"intl.locale.requested\", {});\n",
                serde_json::to_string(&env.locale)?
            ));
        }
        match env.webrtc_policy {
            WebRtcPolicy::Default => {}
            WebRtcPolicy::DisableNonProxiedUdp => {
                prefs.push_str("user_pref(\"media.peerconnection.ice.default_address_only\", true);\n");
                prefs.push_str("user_pref(\"media.peerconnection.ice.no_host\", true);\n");
                prefs.push_str("user_pref(\"media.peerconnection.ice.proxy_only_if_behind_proxy\", true);\n");
            }
            WebRtcPolicy::DisableAll => {
                prefs.push_str("user_pref(\"media.peerconnection.enabled\", false);\n");
            }
        }
    }

    if let Some(ref proxy) = opts.proxy
        && opts.profile_mode != ProfileMode::VirtualContext
    {
        append_firefox_proxy_prefs(&mut prefs, &proxy.server, &proxy.bypass)?;
    }

    let extra: &BTreeMap<String, Value> = &opts.extra_prefs;
    for (k, v) in extra {
        prefs.push_str(&format!(
            "user_pref({}, {});\n",
            serde_json::to_string(k)?,
            serde_json::to_string(v)?
        ));
    }

    fs::write(profile_dir.join("user.js"), prefs)?;
    Ok(())
}

fn strip_q_weights(raw: &str) -> String {
    raw.split(',')
        .filter_map(|part| {
            let tag = part.split(';').next()?.trim();
            if tag.is_empty() {
                None
            } else {
                Some(tag.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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

fn append_firefox_proxy_prefs(
    prefs: &mut String,
    server: &str,
    bypass: &[String],
) -> Result<()> {
    let parsed = url::Url::parse(server).or_else(|_| url::Url::parse(&format!("http://{server}")))?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = resolve_proxy_port(&parsed);

    prefs.push_str("user_pref(\"network.proxy.type\", 1);\n");
    match parsed.scheme().to_ascii_lowercase().as_str() {
        "socks" | "socks5" | "socks5h" => {
            prefs.push_str(&format!(
                "user_pref(\"network.proxy.socks\", {});\n",
                serde_json::to_string(host)?
            ));
            prefs.push_str(&format!("user_pref(\"network.proxy.socks_port\", {port});\n"));
            prefs.push_str("user_pref(\"network.proxy.socks_version\", 5);\n");
            prefs.push_str("user_pref(\"network.proxy.socks_remote_dns\", true);\n");
        }
        "socks4" => {
            prefs.push_str(&format!(
                "user_pref(\"network.proxy.socks\", {});\n",
                serde_json::to_string(host)?
            ));
            prefs.push_str(&format!("user_pref(\"network.proxy.socks_port\", {port});\n"));
            prefs.push_str("user_pref(\"network.proxy.socks_version\", 4);\n");
        }
        _ => {
            prefs.push_str(&format!(
                "user_pref(\"network.proxy.http\", {});\n",
                serde_json::to_string(host)?
            ));
            prefs.push_str(&format!("user_pref(\"network.proxy.http_port\", {port});\n"));
            prefs.push_str(&format!(
                "user_pref(\"network.proxy.ssl\", {});\n",
                serde_json::to_string(host)?
            ));
            prefs.push_str(&format!("user_pref(\"network.proxy.ssl_port\", {port});\n"));
        }
    }

    if !bypass.is_empty() {
        prefs.push_str(&format!(
            "user_pref(\"network.proxy.no_proxies_on\", {});\n",
            serde_json::to_string(&bypass.join(", "))?
        ));
    }
    Ok(())
}

const FIREFOX_DEBLOATED_USER_JS: &str = r#"// mimikyu debloated Firefox user.js (R3-compliant)

// --- WebDriver BiDi & Automation Stability ---
user_pref("remote.log.level", "Warn");
user_pref("remote.prefs.recommended", true);
user_pref("permissions.isolateBy.userContext", true);
user_pref("dom.ipc.processPrelaunch.enabled", false);
user_pref("focusmanager.testmode", true);
user_pref("geo.provider.testing", true);
user_pref("dom.disable_open_during_load", false);
user_pref("dom.ipc.reportProcessHangs", false);
user_pref("hangmonitor.timeout", 0);
user_pref("dom.max_chrome_script_run_time", 0);
user_pref("dom.max_script_run_time", 0);
user_pref("toolkit.shutdown.fastShutdownStage", 3);
user_pref("toolkit.startup.max_resumed_crashes", -1);
user_pref("network.proxy.allow_hijacking_localhost", true);
user_pref("network.proxy.testing_localhost_is_secure_when_hijacked", true);
user_pref("security.enterprise_roots.enabled", true);

// --- History & Session Restore (keep sessionhistory non-zero for back/forward) ---
user_pref("browser.sessionhistory.max_entries", 50);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.sessionstore.max_tabs_undo", 0);
user_pref("browser.sessionstore.max_windows_undo", 0);
user_pref("browser.sessionstore.interval", 60000000);
user_pref("browser.startup.couldRestoreSession.count", -1);

// --- Telemetry, Health Report, Data Reporting & Crash Reporter ---
user_pref("toolkit.telemetry.enabled", false);
user_pref("toolkit.telemetry.unified", false);
user_pref("toolkit.telemetry.archive.enabled", false);
user_pref("toolkit.telemetry.server", "");
user_pref("toolkit.telemetry.newProfilePing.enabled", false);
user_pref("toolkit.telemetry.shutdownPingSender.enabled", false);
user_pref("toolkit.telemetry.updatePing.enabled", false);
user_pref("toolkit.telemetry.bhrPing.enabled", false);
user_pref("toolkit.telemetry.firstShutdownPing.enabled", false);
user_pref("toolkit.coverage.opt-out", true);
user_pref("toolkit.coverage.endpoint.base", "");
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("datareporting.healthreport.service.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("datareporting.usage.uploadEnabled", false);
user_pref("browser.ping-centre.telemetry", false);
user_pref("breakpad.reportURL", "");
user_pref("browser.tabs.crashReporting.sendReport", false);
user_pref("browser.crashReports.unsubmittedCheck.autoSubmit2", false);

// --- Normandy / Shield / Studies / Pocket / Firefox Accounts ---
user_pref("app.normandy.enabled", false);
user_pref("app.normandy.api_url", "");
user_pref("app.shield.optoutstudies.enabled", false);
user_pref("extensions.pocket.enabled", false);
user_pref("browser.pocket.enabled", false);
user_pref("identity.fxaccounts.enabled", false);
user_pref("identity.fxaccounts.auth.uri", "https://127.0.0.1/dummy/fxa");

// --- Updaters, Remote Settings & Safe Browsing Pings ---
user_pref("app.update.enabled", false);
user_pref("app.update.auto", false);
user_pref("app.update.mode", 0);
user_pref("app.update.service.enabled", false);
user_pref("app.update.checkInstallTime", false);
user_pref("app.update.disabledForTesting", true);
user_pref("extensions.update.enabled", false);
user_pref("extensions.update.autoUpdateDefault", false);
user_pref("extensions.systemAddon.update.enabled", false);
user_pref("extensions.blocklist.enabled", false);
user_pref("extensions.getAddons.cache.enabled", false);
user_pref("browser.search.update", false);
user_pref("media.gmp-manager.updateEnabled", false);
user_pref("services.settings.server", "data:,#remote-settings-dummy/v1");
user_pref("services.settings.loglevel", "off");
user_pref("browser.safebrowsing.malware.enabled", false);
user_pref("browser.safebrowsing.phishing.enabled", false);
user_pref("browser.safebrowsing.downloads.enabled", false);
user_pref("browser.safebrowsing.blockedURIs.enabled", false);
user_pref("browser.safebrowsing.passwords.enabled", false);
user_pref("browser.safebrowsing.provider.mozilla.updateURL", "");

// --- Captive Portal, Connectivity, Push, DNS & Speculative Prefetch ---
user_pref("network.captive-portal-service.enabled", false);
user_pref("captivedetect.canonicalURL", "");
user_pref("network.connectivity-service.enabled", false);
user_pref("dom.push.enabled", false);
user_pref("dom.push.connection.enabled", false);
user_pref("dom.push.serverURL", "");
user_pref("network.dns.disablePrefetch", true);
user_pref("network.dns.disablePrefetchFromHTTPS", true);
user_pref("network.prefetch-next", false);
user_pref("network.predictor.enabled", false);
user_pref("network.http.speculative-parallel-limit", 0);
user_pref("browser.urlbar.speculativeConnect.enabled", false);

// --- Onboarding, First-Run, New Tab, Prompts & Translations ---
user_pref("browser.startup.page", 0);
user_pref("browser.startup.homepage", "about:blank");
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("startup.homepage_welcome_url", "about:blank");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("browser.newtabpage.enabled", false);
user_pref("browser.newtabpage.activity-stream.feeds.topsites", false);
user_pref("browser.newtabpage.activity-stream.showSponsoredTopSites", false);
user_pref("browser.newtabpage.activity-stream.feeds.section.topstories", false);
user_pref("browser.newtabpage.activity-stream.asrouter.userprefs.cfr.features", false);
user_pref("browser.topsites.contile.enabled", false);
user_pref("browser.uitour.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.urlbar.suggest.searches", false);
user_pref("browser.urlbar.merino.endpointURL", "");
user_pref("browser.region.network.url", "");
user_pref("browser.translations.enable", false);
user_pref("termsofuse.bypassNotification", true);
user_pref("signon.rememberSignons", false);
user_pref("signon.autofillForms", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.tabs.warnOnOpen", false);
user_pref("browser.warnOnQuit", false);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoverySource;

    #[test]
    fn build_chromium_args_respects_keychain_and_headless_shell_rules() {
        let hs = DiscoveredBrowser {
            engine: EngineKind::Chromium,
            kind: ChromiumFlavor::HeadlessShell,
            path: PathBuf::from("/tmp/chrome-headless-shell"),
            version: "143.0".to_string(),
            source: DiscoverySource::PlaywrightCache,
            supports_headed: false,
            supports_headless: true,
        };
        let opts = LaunchOptions::default();
        let args = build_chromium_args(&hs, &opts, Path::new("/tmp/prof"));
        assert!(args.contains(&"--use-mock-keychain".to_string()));
        assert!(args.contains(&"--password-store=basic".to_string()));
        assert!(!args.contains(&"--headless=new".to_string()));

        let cft = DiscoveredBrowser {
            kind: ChromiumFlavor::ChromeForTesting,
            supports_headed: true,
            ..hs
        };
        let cft_args = build_chromium_args(&cft, &opts, Path::new("/tmp/prof"));
        assert!(cft_args.contains(&"--headless=new".to_string()));
    }

    #[test]
    fn write_firefox_user_js_keeps_sessionhistory_non_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = LaunchOptions {
            engine: EngineKind::Firefox,
            ..Default::default()
        };
        write_firefox_user_js(dir.path(), &opts).expect("write user.js");
        let content = fs::read_to_string(dir.path().join("user.js")).expect("read user.js");
        assert!(content.contains("user_pref(\"browser.sessionhistory.max_entries\", 50);"));
        assert!(content.contains("user_pref(\"toolkit.telemetry.enabled\", false);"));
    }

    #[test]
    fn resolve_proxy_port_uses_scheme_defaults() {
        assert_eq!(
            resolve_proxy_port(&url::Url::parse("http://proxy.local").unwrap()),
            80
        );
        assert_eq!(
            resolve_proxy_port(&url::Url::parse("https://proxy.local").unwrap()),
            443
        );
        assert_eq!(
            resolve_proxy_port(&url::Url::parse("socks5://proxy.local").unwrap()),
            1080
        );
        assert_eq!(
            resolve_proxy_port(&url::Url::parse("socks4://proxy.local").unwrap()),
            1080
        );
        assert_eq!(
            resolve_proxy_port(&url::Url::parse("http://proxy.local:3128").unwrap()),
            3128
        );
    }
}

