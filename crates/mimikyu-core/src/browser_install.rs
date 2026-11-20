use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::discovery::{discover_browser, managed_browsers_dir, DiscoveredBrowser};
use crate::driver::EngineKind;
use crate::error::{MimikyuError, Result};

pub const CHROME_FOR_TESTING_LAST_KNOWN_GOOD_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
pub const CHROME_FOR_TESTING_KNOWN_GOOD_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json";
pub const FIREFOX_PRODUCT_DETAILS_URL: &str =
    "https://product-details.mozilla.org/1.0/firefox_versions.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserInstallOptions {
    pub engine: EngineKind,
    pub version: Option<String>,
    pub dry_run: bool,
    pub expected_sha256: Option<String>,
    pub install_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserInstallPlan {
    pub engine: EngineKind,
    pub platform: String,
    pub version: String,
    pub metadata_endpoint: String,
    pub download_url: String,
    pub checksum_url: Option<String>,
    pub expected_sha256: Option<String>,
    pub install_dir: PathBuf,
    pub expected_binary_path: PathBuf,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserInstallResult {
    pub engine: EngineKind,
    pub platform: String,
    pub version: String,
    pub download_url: String,
    pub checksum_url: Option<String>,
    pub install_dir: PathBuf,
    pub binary_path: PathBuf,
    pub sha256: Option<String>,
    pub verified_sha256: bool,
    pub dry_run: bool,
    pub already_installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered: Option<DiscoveredBrowser>,
}

/// Resolve the official Chrome-for-Testing platform slug for `(os, arch)`.
pub fn cft_platform_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "x86_64") => Ok("linux64"),
        ("windows", "x86_64") => Ok("win64"),
        ("windows", "x86") => Ok("win32"),
        _ => Err(MimikyuError::Unsupported {
            engine: "chromium".to_string(),
            feature: format!("official Chrome for Testing builds on {os}-{arch}"),
        }),
    }
}

/// Resolve the official Mozilla Firefox archive platform directory and archive filename.
pub fn firefox_archive_spec_for(
    os: &str,
    arch: &str,
    version: &str,
) -> Result<(&'static str, String)> {
    match (os, arch) {
        ("macos", "aarch64") | ("macos", "x86_64") => Ok(("mac", format!("Firefox {version}.dmg"))),
        ("linux", "x86_64") => Ok(("linux-x86_64", format!("firefox-{version}.tar.xz"))),
        ("linux", "aarch64") => Ok(("linux-aarch64", format!("firefox-{version}.tar.xz"))),
        _ => Err(MimikyuError::Unsupported {
            engine: "firefox".to_string(),
            feature: format!("official Firefox release archive on {os}-{arch}"),
        }),
    }
}

fn validate_version_string(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 64
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        || trimmed.contains("..")
    {
        return Err(MimikyuError::InvalidConfig(format!(
            "invalid browser version '{raw}'"
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_sha256_hex(raw: &str) -> Result<String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MimikyuError::InvalidConfig(format!(
            "invalid SHA-256 digest '{raw}' (expected 64 hex characters)"
        )));
    }
    Ok(trimmed)
}

fn expected_binary_subpath(engine: EngineKind, os: &str, cft_platform: &str) -> PathBuf {
    match engine {
        EngineKind::Chromium => {
            if os == "macos" {
                PathBuf::from(format!(
                    "chrome-{cft_platform}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
                ))
            } else {
                PathBuf::from(format!("chrome-{cft_platform}/chrome"))
            }
        }
        EngineKind::Firefox => {
            if os == "macos" {
                PathBuf::from("Firefox.app/Contents/MacOS/firefox")
            } else {
                PathBuf::from("firefox/firefox")
            }
        }
    }
}

/// Build an offline deterministic installation plan for `(os, arch)` without performing network I/O.
pub fn plan_browser_install_for(
    opts: &BrowserInstallOptions,
    os: &str,
    arch: &str,
) -> Result<BrowserInstallPlan> {
    let root = opts
        .install_root
        .clone()
        .unwrap_or_else(managed_browsers_dir);
    let expected_sha256 = opts
        .expected_sha256
        .as_deref()
        .map(validate_sha256_hex)
        .transpose()?;
    let requested_version = opts
        .version
        .as_deref()
        .map(validate_version_string)
        .transpose()?;

    match opts.engine {
        EngineKind::Chromium => {
            let platform = cft_platform_for(os, arch)?;
            let version = requested_version
                .clone()
                .unwrap_or_else(|| "latest-stable".to_string());
            let metadata_endpoint = if requested_version.is_some() {
                CHROME_FOR_TESTING_KNOWN_GOOD_URL.to_string()
            } else {
                CHROME_FOR_TESTING_LAST_KNOWN_GOOD_URL.to_string()
            };
            let download_url = if let Some(ref ver) = requested_version {
                format!(
                    "https://storage.googleapis.com/chrome-for-testing-public/{ver}/{platform}/chrome-{platform}.zip"
                )
            } else {
                format!(
                    "https://storage.googleapis.com/chrome-for-testing-public/<latest-stable>/{platform}/chrome-{platform}.zip"
                )
            };
            let install_dir = root.join(format!("chromium-{version}"));
            let expected_binary_path =
                install_dir.join(expected_binary_subpath(EngineKind::Chromium, os, platform));
            Ok(BrowserInstallPlan {
                engine: EngineKind::Chromium,
                platform: platform.to_string(),
                version,
                metadata_endpoint,
                download_url,
                checksum_url: None,
                expected_sha256,
                install_dir,
                expected_binary_path,
                dry_run: opts.dry_run,
            })
        }
        EngineKind::Firefox => {
            let version = requested_version
                .clone()
                .unwrap_or_else(|| "latest-stable".to_string());
            let (ff_platform, archive_name) = firefox_archive_spec_for(os, arch, &version)?;
            let encoded_archive = archive_name.replace(' ', "%20");
            let download_url = format!(
                "https://archive.mozilla.org/pub/firefox/releases/{version}/{ff_platform}/en-US/{encoded_archive}"
            );
            let checksum_url = Some(format!(
                "https://archive.mozilla.org/pub/firefox/releases/{version}/SHA256SUMS"
            ));
            let install_dir = root.join(format!("firefox-{version}"));
            let expected_binary_path =
                install_dir.join(expected_binary_subpath(EngineKind::Firefox, os, ff_platform));
            Ok(BrowserInstallPlan {
                engine: EngineKind::Firefox,
                platform: ff_platform.to_string(),
                version,
                metadata_endpoint: FIREFOX_PRODUCT_DETAILS_URL.to_string(),
                download_url,
                checksum_url,
                expected_sha256,
                install_dir,
                expected_binary_path,
                dry_run: opts.dry_run,
            })
        }
    }
}

/// Build an installation plan for the current host OS and CPU architecture.
pub fn plan_browser_install(opts: &BrowserInstallOptions) -> Result<BrowserInstallPlan> {
    plan_browser_install_for(opts, env::consts::OS, env::consts::ARCH)
}

/// Parse Chrome-for-Testing JSON metadata (`last-known-good` or `known-good`) and return `(version, download_url)`.
pub fn parse_cft_download_url(
    metadata: &Value,
    platform: &str,
    requested_version: Option<&str>,
) -> Result<(String, String)> {
    if let Some(ver) = requested_version {
        let versions = metadata
            .get("versions")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                MimikyuError::InvalidConfig(
                    "Chrome for Testing metadata missing 'versions' array".to_string(),
                )
            })?;
        let entry = versions
            .iter()
            .find(|v| v.get("version").and_then(Value::as_str) == Some(ver))
            .ok_or_else(|| {
                MimikyuError::NotFound(format!(
                    "Chrome for Testing version '{ver}' not found in official metadata"
                ))
            })?;
        let url = extract_cft_chrome_url(entry, platform)?;
        return Ok((ver.to_string(), url));
    }

    let stable = metadata
        .pointer("/channels/Stable")
        .ok_or_else(|| {
            MimikyuError::InvalidConfig(
                "Chrome for Testing metadata missing '/channels/Stable'".to_string(),
            )
        })?;
    let version = stable
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MimikyuError::InvalidConfig(
                "Chrome for Testing Stable channel missing 'version'".to_string(),
            )
        })?
        .to_string();
    let url = extract_cft_chrome_url(stable, platform)?;
    Ok((version, url))
}

fn extract_cft_chrome_url(entry: &Value, platform: &str) -> Result<String> {
    let chrome_downloads = entry
        .pointer("/downloads/chrome")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            MimikyuError::InvalidConfig(
                "Chrome for Testing entry missing 'downloads.chrome' array".to_string(),
            )
        })?;
    chrome_downloads
        .iter()
        .find(|d| d.get("platform").and_then(Value::as_str) == Some(platform))
        .and_then(|d| d.get("url").and_then(Value::as_str))
        .map(ToString::to_string)
        .ok_or_else(|| {
            MimikyuError::NotFound(format!(
                "no Chrome for Testing download URL for platform '{platform}'"
            ))
        })
}

/// Parse Mozilla `firefox_versions.json` and return `LATEST_FIREFOX_VERSION`.
pub fn parse_firefox_latest_version(metadata: &Value) -> Result<String> {
    let ver = metadata
        .get("LATEST_FIREFOX_VERSION")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MimikyuError::InvalidConfig(
                "Mozilla product-details JSON missing 'LATEST_FIREFOX_VERSION'".to_string(),
            )
        })?;
    validate_version_string(ver)
}

/// Extract the SHA-256 hex digest matching `relative_path` (e.g. `mac/en-US/Firefox 136.0.dmg`)
/// from a Mozilla `SHA256SUMS` manifest.
pub fn parse_sha256sums_entry(manifest: &str, relative_path: &str) -> Option<String> {
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let digest = parts.next()?.trim();
        let file = parts.next()?.trim().trim_start_matches('*');
        if file == relative_path && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

/// Compute lowercase hex SHA-256 of a file on disk.
pub fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

/// Verify a downloaded file against an expected SHA-256 hex digest.
pub fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<String> {
    let expected = validate_sha256_hex(expected_hex)?;
    let actual = compute_file_sha256(path)?;
    if actual != expected {
        return Err(MimikyuError::InvalidConfig(format!(
            "SHA-256 checksum mismatch for '{}': expected {expected}, got {actual}",
            path.display()
        )));
    }
    Ok(actual)
}

/// Execute (or dry-run) a managed browser installation under `<data_local_dir>/mimikyu/browsers/`.
///
/// In `--dry-run` mode, returns the resolved installation plan immediately without network access.
pub fn install_browser(opts: &BrowserInstallOptions) -> Result<BrowserInstallResult> {
    let plan = plan_browser_install(opts)?;
    if opts.dry_run {
        return Ok(BrowserInstallResult {
            engine: plan.engine,
            platform: plan.platform,
            version: plan.version,
            download_url: plan.download_url,
            checksum_url: plan.checksum_url,
            install_dir: plan.install_dir,
            binary_path: plan.expected_binary_path,
            sha256: plan.expected_sha256,
            verified_sha256: false,
            dry_run: true,
            already_installed: false,
            discovered: None,
        });
    }

    let root = opts
        .install_root
        .clone()
        .unwrap_or_else(managed_browsers_dir);
    fs::create_dir_all(&root)?;

    let os = env::consts::OS;
    let arch = env::consts::ARCH;

    let (version, platform, download_url, checksum_url, expected_sha256, archive_ext) =
        match opts.engine {
            EngineKind::Chromium => {
                let cft_platform = cft_platform_for(os, arch)?;
                let (ver, url) = if let Some(ref req_ver) = opts.version {
                    let clean_ver = validate_version_string(req_ver)?;
                    let url = format!(
                        "https://storage.googleapis.com/chrome-for-testing-public/{clean_ver}/{cft_platform}/chrome-{cft_platform}.zip"
                    );
                    (clean_ver, url)
                } else {
                    let body = http_get_text(CHROME_FOR_TESTING_LAST_KNOWN_GOOD_URL)?;
                    let meta: Value = serde_json::from_str(&body)?;
                    parse_cft_download_url(&meta, cft_platform, None)?
                };
                let sha = opts
                    .expected_sha256
                    .as_deref()
                    .map(validate_sha256_hex)
                    .transpose()?;
                (ver, cft_platform.to_string(), url, None, sha, "zip")
            }
            EngineKind::Firefox => {
                let ver = if let Some(ref req_ver) = opts.version {
                    validate_version_string(req_ver)?
                } else {
                    let body = http_get_text(FIREFOX_PRODUCT_DETAILS_URL)?;
                    let meta: Value = serde_json::from_str(&body)?;
                    parse_firefox_latest_version(&meta)?
                };
                let (ff_platform, archive_name) = firefox_archive_spec_for(os, arch, &ver)?;
                let encoded_archive = archive_name.replace(' ', "%20");
                let url = format!(
                    "https://archive.mozilla.org/pub/firefox/releases/{ver}/{ff_platform}/en-US/{encoded_archive}"
                );
                let sums_url = format!("https://archive.mozilla.org/pub/firefox/releases/{ver}/SHA256SUMS");
                let sha = if let Some(ref explicit) = opts.expected_sha256 {
                    Some(validate_sha256_hex(explicit)?)
                } else {
                    let sums_text = http_get_text(&sums_url)?;
                    let rel_key = format!("{ff_platform}/en-US/{archive_name}");
                    Some(parse_sha256sums_entry(&sums_text, &rel_key).ok_or_else(|| {
                        MimikyuError::InvalidConfig(format!(
                            "official Firefox SHA256SUMS missing entry for '{rel_key}'"
                        ))
                    })?)
                };
                let ext = if os == "macos" { "dmg" } else { "tar.xz" };
                (ver, ff_platform.to_string(), url, Some(sums_url), sha, ext)
            }
        };

    let install_dir = root.join(format!("{}-{version}", opts.engine));
    let expected_binary =
        install_dir.join(expected_binary_subpath(opts.engine, os, &platform));

    if expected_binary.exists() {
        let discovered = discover_browser(opts.engine, true, Some(&expected_binary)).ok();
        return Ok(BrowserInstallResult {
            engine: opts.engine,
            platform,
            version,
            download_url,
            checksum_url,
            install_dir,
            binary_path: expected_binary,
            sha256: expected_sha256,
            verified_sha256: false,
            dry_run: false,
            already_installed: true,
            discovered,
        });
    }

    let staging = tempfile::Builder::new()
        .prefix(&format!("mimikyu-install-{}-", opts.engine))
        .tempdir_in(&root)?;
    let archive_path = staging.path().join(format!("browser.{archive_ext}"));
    http_download_file(&download_url, &archive_path)?;

    let (actual_sha256, verified_sha256) = match expected_sha256.as_deref() {
        Some(expected) => (verify_file_sha256(&archive_path, expected)?, true),
        None => (compute_file_sha256(&archive_path)?, false),
    };

    let extract_dir = staging.path().join("extracted");
    fs::create_dir_all(&extract_dir)?;
    extract_archive(&archive_path, &extract_dir, archive_ext)?;

    if install_dir.exists() {
        fs::remove_dir_all(&install_dir)?;
    }
    fs::rename(&extract_dir, &install_dir)?;

    let discovered = discover_browser(opts.engine, true, Some(&expected_binary))?;
    Ok(BrowserInstallResult {
        engine: opts.engine,
        platform,
        version,
        download_url,
        checksum_url,
        install_dir,
        binary_path: discovered.path.clone(),
        sha256: Some(actual_sha256),
        verified_sha256,
        dry_run: false,
        already_installed: false,
        discovered: Some(discovered),
    })
}

fn http_get_text(url: &str) -> Result<String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--tlsv1.2", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| MimikyuError::Transport(format!("failed to execute curl for {url}: {e}")))?;
    if !out.status.success() {
        return Err(MimikyuError::Transport(format!(
            "HTTP GET {url} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    String::from_utf8(out.stdout)
        .map_err(|e| MimikyuError::Transport(format!("non-UTF8 response from {url}: {e}")))
}

fn http_download_file(url: &str, dest: &Path) -> Result<()> {
    let out = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "-o"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| MimikyuError::Transport(format!("failed to execute curl for {url}: {e}")))?;
    if !out.status.success() {
        return Err(MimikyuError::Transport(format!(
            "download {url} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn extract_archive(archive: &Path, dest_dir: &Path, ext: &str) -> Result<()> {
    match ext {
        "zip" => {
            let out = Command::new("unzip")
                .args(["-q", "-o"])
                .arg(archive)
                .arg("-d")
                .arg(dest_dir)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| MimikyuError::LaunchFailed(format!("failed to execute unzip: {e}")))?;
            if !out.status.success() {
                return Err(MimikyuError::LaunchFailed(format!(
                    "unzip failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        }
        "tar.xz" => {
            let out = Command::new("tar")
                .args(["-xJf"])
                .arg(archive)
                .arg("-C")
                .arg(dest_dir)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| MimikyuError::LaunchFailed(format!("failed to execute tar: {e}")))?;
            if !out.status.success() {
                return Err(MimikyuError::LaunchFailed(format!(
                    "tar extract failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        }
        "dmg" => {
            let mount_dir = dest_dir.join(".dmg-mount");
            fs::create_dir_all(&mount_dir)?;
            let attach = Command::new("hdiutil")
                .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
                .arg(&mount_dir)
                .arg(archive)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| {
                    MimikyuError::LaunchFailed(format!("failed to execute hdiutil attach: {e}"))
                })?;
            if !attach.status.success() {
                let _ = fs::remove_dir_all(&mount_dir);
                return Err(MimikyuError::LaunchFailed(format!(
                    "hdiutil attach failed ({}): {}",
                    attach.status,
                    String::from_utf8_lossy(&attach.stderr).trim()
                )));
            }
            let copy_res = Command::new("cp")
                .args(["-R"])
                .arg(mount_dir.join("Firefox.app"))
                .arg(dest_dir.join("Firefox.app"))
                .stdin(Stdio::null())
                .output();
            let _ = Command::new("hdiutil")
                .args(["detach", "-quiet"])
                .arg(&mount_dir)
                .stdin(Stdio::null())
                .output();
            let _ = fs::remove_dir_all(&mount_dir);
            let out = copy_res.map_err(|e| {
                MimikyuError::LaunchFailed(format!("failed to copy Firefox.app from DMG: {e}"))
            })?;
            if !out.status.success() {
                return Err(MimikyuError::LaunchFailed(format!(
                    "copying Firefox.app failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        }
        other => Err(MimikyuError::InvalidConfig(format!(
            "unsupported archive format '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dry_run_plan_resolves_cft_and_firefox_urls_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let cft = plan_browser_install_for(
            &BrowserInstallOptions {
                engine: EngineKind::Chromium,
                version: Some("136.0.7103.92".to_string()),
                dry_run: true,
                expected_sha256: None,
                install_root: Some(tmp.path().to_path_buf()),
            },
            "macos",
            "aarch64",
        )
        .unwrap();
        assert_eq!(cft.platform, "mac-arm64");
        assert_eq!(
            cft.download_url,
            "https://storage.googleapis.com/chrome-for-testing-public/136.0.7103.92/mac-arm64/chrome-mac-arm64.zip"
        );
        assert!(cft
            .expected_binary_path
            .ends_with("chromium-136.0.7103.92/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"));

        let ff = plan_browser_install_for(
            &BrowserInstallOptions {
                engine: EngineKind::Firefox,
                version: Some("138.0.1".to_string()),
                dry_run: true,
                expected_sha256: None,
                install_root: Some(tmp.path().to_path_buf()),
            },
            "linux",
            "x86_64",
        )
        .unwrap();
        assert_eq!(ff.platform, "linux-x86_64");
        assert_eq!(
            ff.download_url,
            "https://archive.mozilla.org/pub/firefox/releases/138.0.1/linux-x86_64/en-US/firefox-138.0.1.tar.xz"
        );
        assert_eq!(
            ff.checksum_url.as_deref(),
            Some("https://archive.mozilla.org/pub/firefox/releases/138.0.1/SHA256SUMS")
        );
        assert!(ff
            .expected_binary_path
            .ends_with("firefox-138.0.1/firefox/firefox"));
    }

    #[test]
    fn parse_cft_and_firefox_metadata_extracts_urls_and_sha256sums() {
        let cft_meta = json!({
            "channels": {
                "Stable": {
                    "version": "136.0.7103.92",
                    "downloads": {
                        "chrome": [
                            { "platform": "linux64", "url": "https://example.invalid/linux64.zip" },
                            { "platform": "mac-arm64", "url": "https://example.invalid/mac-arm64.zip" }
                        ]
                    }
                }
            }
        });
        let (ver, url) = parse_cft_download_url(&cft_meta, "mac-arm64", None).unwrap();
        assert_eq!(ver, "136.0.7103.92");
        assert_eq!(url, "https://example.invalid/mac-arm64.zip");

        let ff_meta = json!({ "LATEST_FIREFOX_VERSION": "138.0.4" });
        assert_eq!(parse_firefox_latest_version(&ff_meta).unwrap(), "138.0.4");

        let sums = "\
aaaaaaaa11111111222222223333333344444444555555556666666677777777  mac/en-US/Firefox 138.0.4.dmg\n\
bbbbbbbb11111111222222223333333344444444555555556666666677777777  linux-x86_64/en-US/firefox-138.0.4.tar.xz\n";
        assert_eq!(
            parse_sha256sums_entry(sums, "mac/en-US/Firefox 138.0.4.dmg").as_deref(),
            Some("aaaaaaaa11111111222222223333333344444444555555556666666677777777")
        );
        assert_eq!(
            parse_sha256sums_entry(sums, "linux-x86_64/en-US/firefox-138.0.4.tar.xz").as_deref(),
            Some("bbbbbbbb11111111222222223333333344444444555555556666666677777777")
        );
        assert_eq!(parse_sha256sums_entry(sums, "missing.tar.xz"), None);
    }

    #[test]
    fn verify_file_sha256_accepts_match_and_rejects_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("payload.bin");
        fs::write(&file, b"mimikyu-managed-browser-check").unwrap();
        let actual = compute_file_sha256(&file).unwrap();
        assert_eq!(actual.len(), 64);
        assert_eq!(verify_file_sha256(&file, &actual).unwrap(), actual);

        let wrong = "0".repeat(64);
        let err = verify_file_sha256(&file, &wrong).unwrap_err();
        assert!(matches!(err, MimikyuError::InvalidConfig(_)));
    }
}
