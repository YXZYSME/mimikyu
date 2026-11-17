use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::driver::EngineKind;
use crate::error::{MimikyuError, Result};

pub const ENV_CHROMIUM_PATH: &str = "MIMIKYU_CHROMIUM_PATH";
pub const ENV_FIREFOX_PATH: &str = "MIMIKYU_FIREFOX_PATH";
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChromiumFlavor {
    HeadlessShell,
    ChromeForTesting,
    Chrome,
    Chromium,
    Firefox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    Override,
    EnvOverride,
    PlaywrightCache,
    PuppeteerCache,
    SystemInstall,
    Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredBrowser {
    pub engine: EngineKind,
    pub kind: ChromiumFlavor,
    pub path: PathBuf,
    pub version: String,
    pub source: DiscoverySource,
    pub supports_headed: bool,
    pub supports_headless: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvOverrides {
    pub mimikyu_chromium_path: Option<String>,
    pub mimikyu_firefox_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub platform: String,
    pub default_chromium: Option<PathBuf>,
    pub default_firefox: Option<PathBuf>,
    pub chromium: Vec<DiscoveredBrowser>,
    pub firefox: Vec<DiscoveredBrowser>,
    pub env_overrides: EnvOverrides,
    pub warnings: Vec<String>,
}

/// Discover the best matching browser binary for `engine` and `headless` mode.
///
/// Precedence:
/// 1. Explicit `override_path` (from `LaunchOptions.binary_path`)
/// 2. `MIMIKYU_CHROMIUM_PATH` / `MIMIKYU_FIREFOX_PATH` env override
/// 3. Local caches (Playwright, Puppeteer) and system/PATH installs sorted newest-first
///
/// Never passes `--help` (which hangs `chrome-headless-shell`); probes only `--version`
/// inside an isolated process group (`setpgid(0, 0)`) with a 5000ms timeout.
pub fn discover_browser(
    engine: EngineKind,
    headless: bool,
    override_path: Option<&Path>,
) -> Result<DiscoveredBrowser> {
    if let Some(path) = override_path {
        let browser = validate_and_probe(engine, path, DiscoverySource::Override)?;
        ensure_mode_supported(&browser, headless)?;
        return Ok(browser);
    }

    let env_var = match engine {
        EngineKind::Chromium => ENV_CHROMIUM_PATH,
        EngineKind::Firefox => ENV_FIREFOX_PATH,
    };
    if let Ok(val) = env::var(env_var) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            let browser = validate_and_probe(engine, &path, DiscoverySource::EnvOverride)
                .map_err(|e| {
                    MimikyuError::BrowserNotFound(format!(
                        "{env_var}='{}' is invalid: {e}",
                        path.display()
                    ))
                })?;
            ensure_mode_supported(&browser, headless)?;
            return Ok(browser);
        }
    }

    let candidates = collect_candidates(engine);
    let mut first_headed_error: Option<MimikyuError> = None;

    for (path, source) in candidates {
        if !is_executable_file(&path) {
            continue;
        }
        let flavor_guess = classify_flavor(engine, &path, "");
        if engine == EngineKind::Chromium && !headless && flavor_guess == ChromiumFlavor::HeadlessShell {
            if first_headed_error.is_none() {
                first_headed_error = Some(MimikyuError::Unsupported {
                    engine: engine.to_string(),
                    feature: format!(
                        "headed mode is unsupported by headless-only binary '{}'",
                        path.display()
                    ),
                });
            }
            continue;
        }
        match validate_and_probe(engine, &path, source) {
            Ok(browser) => {
                if let Err(err) = ensure_mode_supported(&browser, headless) {
                    if first_headed_error.is_none() {
                        first_headed_error = Some(err);
                    }
                    continue;
                }
                return Ok(browser);
            }
            Err(_) => continue,
        }
    }

    if let Some(err) = first_headed_error {
        return Err(err);
    }

    Err(MimikyuError::BrowserNotFound(format!(
        "no local {engine} binary found (checked Playwright/Puppeteer caches, standard install paths, and PATH; set {env_var} to override)"
    )))
}

/// Run full local browser discovery for `mimikyu doctor`.
pub fn run_doctor() -> DoctorReport {
    let platform = format!("{}-{}", env::consts::OS, env::consts::ARCH);
    let env_chromium = env::var(ENV_CHROMIUM_PATH)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let env_firefox = env::var(ENV_FIREFOX_PATH)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut warnings = Vec::new();
    let mut chromium = Vec::new();
    let mut firefox = Vec::new();

    if let Some(ref p) = env_chromium {
        match validate_and_probe(EngineKind::Chromium, Path::new(p), DiscoverySource::EnvOverride)
        {
            Ok(b) => chromium.push(b),
            Err(e) => warnings.push(format!("{ENV_CHROMIUM_PATH}='{p}' failed validation: {e}")),
        }
    }
    if let Some(ref p) = env_firefox {
        match validate_and_probe(EngineKind::Firefox, Path::new(p), DiscoverySource::EnvOverride) {
            Ok(b) => firefox.push(b),
            Err(e) => warnings.push(format!("{ENV_FIREFOX_PATH}='{p}' failed validation: {e}")),
        }
    }

    let mut seen_chromium: HashSet<PathBuf> = chromium.iter().map(|b| b.path.clone()).collect();
    for (path, source) in collect_candidates(EngineKind::Chromium) {
        let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen_chromium.contains(&path) || seen_chromium.contains(&canonical) {
            continue;
        }
        if !is_executable_file(&path) {
            continue;
        }
        match validate_and_probe(EngineKind::Chromium, &path, source) {
            Ok(b) => {
                seen_chromium.insert(b.path.clone());
                seen_chromium.insert(canonical);
                chromium.push(b);
            }
            Err(e) => {
                warnings.push(format!("failed probing {}: {e}", path.display()));
            }
        }
    }

    let mut seen_firefox: HashSet<PathBuf> = firefox.iter().map(|b| b.path.clone()).collect();
    for (path, source) in collect_candidates(EngineKind::Firefox) {
        let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen_firefox.contains(&path) || seen_firefox.contains(&canonical) {
            continue;
        }
        if !is_executable_file(&path) {
            continue;
        }
        match validate_and_probe(EngineKind::Firefox, &path, source) {
            Ok(b) => {
                seen_firefox.insert(b.path.clone());
                seen_firefox.insert(canonical);
                firefox.push(b);
            }
            Err(e) => {
                warnings.push(format!("failed probing {}: {e}", path.display()));
            }
        }
    }

    let default_chromium = if env_chromium.is_some() {
        chromium
            .iter()
            .find(|b| b.source == DiscoverySource::EnvOverride)
            .map(|b| b.path.clone())
    } else {
        chromium.first().map(|b| b.path.clone())
    };

    let default_firefox = if env_firefox.is_some() {
        firefox
            .iter()
            .find(|b| b.source == DiscoverySource::EnvOverride)
            .map(|b| b.path.clone())
    } else {
        firefox.first().map(|b| b.path.clone())
    };

    if default_chromium.is_none() {
        warnings.push("no Chromium binary discovered".to_string());
    }
    if default_firefox.is_none() {
        warnings.push("no Firefox binary discovered".to_string());
    }

    DoctorReport {
        platform,
        default_chromium,
        default_firefox,
        chromium,
        firefox,
        env_overrides: EnvOverrides {
            mimikyu_chromium_path: env_chromium,
            mimikyu_firefox_path: env_firefox,
        },
        warnings,
    }
}

fn ensure_mode_supported(browser: &DiscoveredBrowser, headless: bool) -> Result<()> {
    if !headless && !browser.supports_headed {
        return Err(MimikyuError::Unsupported {
            engine: browser.engine.to_string(),
            feature: format!(
                "headed mode is unsupported by headless-only binary '{}'",
                browser.path.display()
            ),
        });
    }
    Ok(())
}

fn validate_and_probe(
    engine: EngineKind,
    path: &Path,
    source: DiscoverySource,
) -> Result<DiscoveredBrowser> {
    if !path.exists() {
        return Err(MimikyuError::BrowserNotFound(format!(
            "binary does not exist: {}",
            path.display()
        )));
    }
    if !is_executable_file(path) {
        return Err(MimikyuError::BrowserNotFound(format!(
            "binary is not an executable file: {}",
            path.display()
        )));
    }
    let version = probe_version(path)?;
    let kind = classify_flavor(engine, path, &version);
    let supports_headed = kind != ChromiumFlavor::HeadlessShell;
    Ok(DiscoveredBrowser {
        engine,
        kind,
        path: path.to_path_buf(),
        version,
        source,
        supports_headed,
        supports_headless: true,
    })
}

pub fn classify_flavor(engine: EngineKind, path: &Path, version: &str) -> ChromiumFlavor {
    if engine == EngineKind::Firefox {
        return ChromiumFlavor::Firefox;
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let full = path.to_string_lossy().to_ascii_lowercase();
    let ver_lc = version.to_ascii_lowercase();

    if name == "chrome-headless-shell"
        || name == "headless_shell"
        || full.contains("chromium_headless_shell")
        || full.contains("chrome-headless-shell")
    {
        ChromiumFlavor::HeadlessShell
    } else if full.contains("google chrome for testing") || ver_lc.contains("chrome for testing") {
        ChromiumFlavor::ChromeForTesting
    } else if full.contains("google chrome") || name == "google-chrome" || name == "google-chrome-stable" {
        ChromiumFlavor::Chrome
    } else {
        ChromiumFlavor::Chromium
    }
}

fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Probe `<binary> --version` ONLY (never `--help`) in an isolated process group with a 5000ms timeout.
fn probe_version(path: &Path) -> Result<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Isolate into its own process group so timeouts can SIGKILL the whole subtree cleanly.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| {
        MimikyuError::LaunchFailed(format!("failed to spawn {} --version: {e}", path.display()))
    })?;

    let pgid = child.id() as libc::pid_t;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let out_handle = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(ref mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(ref mut pipe) = stderr_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= VERSION_PROBE_TIMEOUT {
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                    let _ = child.wait();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(MimikyuError::Timeout {
                        timeout_ms: VERSION_PROBE_TIMEOUT.as_millis() as u64,
                        operation: format!("{} --version", path.display()),
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(MimikyuError::Io(e));
            }
        }
    };

    let stdout_bytes = out_handle.join().unwrap_or_default();
    let stderr_bytes = err_handle.join().unwrap_or_default();

    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&stderr_bytes);
        return Err(MimikyuError::LaunchFailed(format!(
            "{} --version exited with {status}: {}",
            path.display(),
            stderr_str.trim()
        )));
    }

    let stdout_str = String::from_utf8_lossy(&stdout_bytes);
    let version_line = stdout_str
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| {
            MimikyuError::LaunchFailed(format!(
                "{} --version produced empty stdout",
                path.display()
            ))
        })?;

    Ok(version_line.to_string())
}

fn playwright_headless_shell_subpaths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            &[
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-mac-arm64/headless_shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-mac/headless_shell",
            ]
        } else {
            &[
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-mac/headless_shell",
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-mac-arm64/headless_shell",
            ]
        }
    } else if cfg!(target_arch = "aarch64") {
        &[
            "chrome-headless-shell-linux-arm64/chrome-headless-shell",
            "chrome-headless-shell-linux/chrome-headless-shell",
            "chrome-linux/headless_shell",
            "chrome-headless-shell-linux-x64/chrome-headless-shell",
        ]
    } else {
        &[
            "chrome-headless-shell-linux-x64/chrome-headless-shell",
            "chrome-headless-shell-linux/chrome-headless-shell",
            "chrome-linux/headless_shell",
            "chrome-headless-shell-linux-arm64/chrome-headless-shell",
        ]
    }
}

fn puppeteer_headless_shell_subpaths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            &[
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
            ]
        } else {
            &[
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
            ]
        }
    } else if cfg!(target_arch = "aarch64") {
        &[
            "chrome-headless-shell-linux-arm64/chrome-headless-shell",
            "chrome-headless-shell-linux64/chrome-headless-shell",
        ]
    } else {
        &[
            "chrome-headless-shell-linux64/chrome-headless-shell",
            "chrome-headless-shell-linux-arm64/chrome-headless-shell",
        ]
    }
}

fn playwright_chromium_subpaths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            &[
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
                "chrome-mac/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            ]
        } else {
            &[
                "chrome-mac/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
            ]
        }
    } else {
        &["chrome-linux/chrome", "chrome-linux64/chrome"]
    }
}

fn puppeteer_chromium_subpaths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            &[
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ]
        } else {
            &[
                "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ]
        }
    } else {
        &["chrome-linux64/chrome", "chrome-linux/chrome"]
    }
}

fn system_chromium_paths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
    }
}

fn playwright_firefox_subpaths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &[
            "firefox/Nightly.app/Contents/MacOS/firefox",
            "firefox/Firefox.app/Contents/MacOS/firefox",
            "firefox/firefox",
        ]
    } else {
        &[
            "firefox/firefox",
            "firefox/Nightly.app/Contents/MacOS/firefox",
            "firefox/Firefox.app/Contents/MacOS/firefox",
        ]
    }
}

fn system_firefox_paths() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &[
            "/Applications/Firefox.app/Contents/MacOS/firefox",
            "/Applications/Firefox Nightly.app/Contents/MacOS/firefox",
        ]
    } else {
        &["/usr/bin/firefox", "/usr/local/bin/firefox"]
    }
}

fn collect_candidates(engine: EngineKind) -> Vec<(PathBuf, DiscoverySource)> {
    let mut out = Vec::new();

    match engine {
        EngineKind::Chromium => {
            // 1. Playwright chrome-headless-shell (newest-first, host OS/arch prioritized)
            for root in playwright_cache_roots() {
                for dir in sorted_prefixed_dirs(&root, "chromium_headless_shell-") {
                    for sub in playwright_headless_shell_subpaths() {
                        out.push((dir.join(sub), DiscoverySource::PlaywrightCache));
                    }
                }
            }

            // 2. Puppeteer chrome-headless-shell (newest-first, host OS/arch prioritized)
            for root in puppeteer_cache_roots() {
                let chs_root = root.join("chrome-headless-shell");
                for dir in sorted_versioned_dirs(&chs_root) {
                    for sub in puppeteer_headless_shell_subpaths() {
                        out.push((dir.join(sub), DiscoverySource::PuppeteerCache));
                    }
                }
            }

            // 3. Playwright full Chromium / Chrome for Testing (newest-first, host OS/arch prioritized)
            for root in playwright_cache_roots() {
                for dir in sorted_prefixed_dirs(&root, "chromium-") {
                    for sub in playwright_chromium_subpaths() {
                        out.push((dir.join(sub), DiscoverySource::PlaywrightCache));
                    }
                }
            }

            // 4. Puppeteer Chrome for Testing (newest-first, host OS/arch prioritized)
            for root in puppeteer_cache_roots() {
                let chrome_root = root.join("chrome");
                for dir in sorted_versioned_dirs(&chrome_root) {
                    for sub in puppeteer_chromium_subpaths() {
                        out.push((dir.join(sub), DiscoverySource::PuppeteerCache));
                    }
                }
            }

            // 5. System installs & PATH
            for p in system_chromium_paths() {
                out.push((PathBuf::from(p), DiscoverySource::SystemInstall));
            }

            for name in [
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
                "chrome-headless-shell",
            ] {
                if let Some(p) = find_on_path(name) {
                    out.push((p, DiscoverySource::Path));
                }
            }
        }
        EngineKind::Firefox => {
            // 1. Playwright Firefox (newest-first, host OS prioritized)
            for root in playwright_cache_roots() {
                for dir in sorted_prefixed_dirs(&root, "firefox-") {
                    for sub in playwright_firefox_subpaths() {
                        out.push((dir.join(sub), DiscoverySource::PlaywrightCache));
                    }
                }
            }

            // 2. System installs & PATH
            for p in system_firefox_paths() {
                out.push((PathBuf::from(p), DiscoverySource::SystemInstall));
            }

            for name in ["firefox", "firefox-nightly"] {
                if let Some(p) = find_on_path(name) {
                    out.push((p, DiscoverySource::Path));
                }
            }
        }
    }

    out
}

fn playwright_cache_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(custom) = env::var("PLAYWRIGHT_BROWSERS_PATH") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() && trimmed != "0" {
            roots.push(PathBuf::from(trimmed));
        }
    }
    if let Some(cache) = dirs::cache_dir() {
        roots.push(cache.join("ms-playwright"));
    }
    if let Some(home) = dirs::home_dir() {
        let mac_cache = home.join("Library/Caches/ms-playwright");
        if !roots.contains(&mac_cache) {
            roots.push(mac_cache);
        }
        let xdg_cache = home.join(".cache/ms-playwright");
        if !roots.contains(&xdg_cache) {
            roots.push(xdg_cache);
        }
    }
    roots
}

fn puppeteer_cache_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(custom) = env::var("PUPPETEER_CACHE_DIR") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            roots.push(PathBuf::from(trimmed));
        }
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".cache/puppeteer"));
    }
    if let Some(cache) = dirs::cache_dir() {
        let c = cache.join("puppeteer");
        if !roots.contains(&c) {
            roots.push(c);
        }
    }
    roots
}

fn sorted_prefixed_dirs(root: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut matched: Vec<(Vec<u64>, String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().to_string();
            if !file_name.starts_with(prefix) || !e.path().is_dir() {
                return None;
            }
            let suffix = &file_name[prefix.len()..];
            let nums = parse_numeric_tuple(suffix);
            Some((nums, file_name, e.path()))
        })
        .collect();

    matched.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    matched.into_iter().map(|(_, _, p)| p).collect()
}

fn sorted_versioned_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut matched: Vec<(Vec<u64>, String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let ver_part = name.split('-').next_back().unwrap_or(&name);
            let nums = parse_numeric_tuple(ver_part);
            Some((nums, name, path))
        })
        .collect();

    matched.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    matched.into_iter().map(|(_, _, p)| p).collect()
}

fn parse_numeric_tuple(s: &str) -> Vec<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(binary_name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_flavor_distinguishes_all_variants() {
        assert_eq!(
            classify_flavor(
                EngineKind::Chromium,
                Path::new("/tmp/chromium_headless_shell-1200/chrome-headless-shell"),
                "Google Chrome for Testing 143.0.7499.4",
            ),
            ChromiumFlavor::HeadlessShell
        );
        assert_eq!(
            classify_flavor(
                EngineKind::Chromium,
                Path::new("/tmp/chromium_headless_shell-1161/chrome-mac/headless_shell"),
                "Chromium 134.0.6998.35",
            ),
            ChromiumFlavor::HeadlessShell
        );
        assert_eq!(
            classify_flavor(
                EngineKind::Chromium,
                Path::new("/tmp/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
                "Google Chrome for Testing 143.0.7499.4",
            ),
            ChromiumFlavor::ChromeForTesting
        );
        assert_eq!(
            classify_flavor(
                EngineKind::Chromium,
                Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
                "Google Chrome 157.0.8103.79",
            ),
            ChromiumFlavor::Chrome
        );
        assert_eq!(
            classify_flavor(
                EngineKind::Firefox,
                Path::new("/tmp/firefox-1497/firefox"),
                "Mozilla Firefox 144.0.2",
            ),
            ChromiumFlavor::Firefox
        );
    }

    #[test]
    fn parse_numeric_tuple_orders_revisions_newest_first() {
        let a = parse_numeric_tuple("1200");
        let b = parse_numeric_tuple("1161");
        assert!(a > b);

        let v1 = parse_numeric_tuple("155.0.8059.39");
        let v2 = parse_numeric_tuple("148.0.7778.97");
        assert!(v1 > v2);
    }
}

