use std::path::Path;

use mimikyu_core::discovery::{discover_browser, run_doctor, ChromiumFlavor};
use mimikyu_core::driver::EngineKind;
use mimikyu_core::MimikyuError;

#[test]
fn doctor_discovers_local_chromium_and_firefox() {
    let report = run_doctor();
    assert!(
        report.default_chromium.is_some(),
        "expected local Chromium binary to be discovered; warnings={:?}",
        report.warnings
    );
    assert!(
        report.default_firefox.is_some(),
        "expected local Firefox binary to be discovered; warnings={:?}",
        report.warnings
    );
    assert!(!report.chromium.is_empty());
    assert!(!report.firefox.is_empty());

    let serialized = serde_json::to_value(&report).expect("serialize DoctorReport");
    assert!(serialized["default_chromium"].is_string());
    assert!(serialized["default_firefox"].is_string());
}

#[test]
fn discover_browser_explicit_invalid_override_fails() {
    let missing = Path::new("/nonexistent/mimikyu/browser-binary-does-not-exist");
    let err = discover_browser(EngineKind::Chromium, true, Some(missing)).unwrap_err();
    match err {
        MimikyuError::BrowserNotFound(msg) => {
            assert!(msg.contains("does not exist"), "unexpected msg: {msg}");
        }
        other => panic!("expected BrowserNotFound, got {other:?}"),
    }
}

#[test]
fn discover_browser_headless_vs_headed_selection() {
    let headless_chromium = discover_browser(EngineKind::Chromium, true, None)
        .expect("discover headless chromium");
    assert!(headless_chromium.supports_headless);

    let headed_chromium = discover_browser(EngineKind::Chromium, false, None)
        .expect("discover headed chromium");
    assert!(headed_chromium.supports_headed);
    assert_ne!(headed_chromium.kind, ChromiumFlavor::HeadlessShell);

    // Explicitly passing HeadlessShell with headless=false must fail with Unsupported.
    if headless_chromium.kind == ChromiumFlavor::HeadlessShell {
        let err = discover_browser(
            EngineKind::Chromium,
            false,
            Some(&headless_chromium.path),
        )
        .unwrap_err();
        assert!(
            matches!(err, MimikyuError::Unsupported { .. }),
            "expected Unsupported for headless-shell in headed mode, got {err:?}"
        );
    }
}

fn write_executable_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

#[test]
fn firefox_discovery_reads_macos_bundle_application_ini_without_spawning() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sentinel = dir.path().join("spawned.sentinel");
    let bin = dir
        .path()
        .join("Nightly.app/Contents/MacOS/firefox");
    let ini = dir
        .path()
        .join("Nightly.app/Contents/Resources/application.ini");

    write_executable_script(
        &bin,
        &format!("#!/bin/sh\ntouch \"{}\"\nexit 99\n", sentinel.display()),
    );
    std::fs::create_dir_all(ini.parent().unwrap()).unwrap();
    std::fs::write(
        &ini,
        "[App]\nVendor=Mozilla\nName=Firefox\nVersion=144.0.2\n",
    )
    .unwrap();

    let discovered =
        discover_browser(EngineKind::Firefox, true, Some(&bin)).expect("discover via application.ini");
    assert_eq!(discovered.version, "Mozilla Firefox 144.0.2");
    assert_eq!(discovered.kind, ChromiumFlavor::Firefox);
    assert!(
        !sentinel.exists(),
        "discovery must NOT spawn firefox --version when application.ini exists"
    );
}

#[test]
fn firefox_discovery_reads_flat_and_symlinked_application_ini_without_spawning() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sentinel = dir.path().join("spawned.sentinel");
    let real_bin = dir.path().join("opt/firefox/firefox");
    let ini = dir.path().join("opt/firefox/application.ini");
    let symlink_bin = dir.path().join("bin/firefox");

    write_executable_script(
        &real_bin,
        &format!("#!/bin/sh\ntouch \"{}\"\nexit 99\n", sentinel.display()),
    );
    std::fs::write(
        &ini,
        "[App]\nVendor=Mozilla\nName=Firefox\nVersion=145.0\n",
    )
    .unwrap();
    std::fs::create_dir_all(symlink_bin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real_bin, &symlink_bin).expect("create symlink");

    let discovered = discover_browser(EngineKind::Firefox, true, Some(&symlink_bin))
        .expect("discover via canonicalized symlink application.ini");
    assert_eq!(discovered.version, "Mozilla Firefox 145.0");
    assert!(
        !sentinel.exists(),
        "discovery must NOT spawn symlinked firefox --version when application.ini exists"
    );
}

#[test]
fn firefox_discovery_falls_back_to_info_plist_without_spawning() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sentinel = dir.path().join("spawned.sentinel");
    let bin = dir
        .path()
        .join("Firefox.app/Contents/MacOS/firefox");
    let plist = dir
        .path()
        .join("Firefox.app/Contents/Info.plist");

    write_executable_script(
        &bin,
        &format!("#!/bin/sh\ntouch \"{}\"\nexit 99\n", sentinel.display()),
    );
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(
        &plist,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Firefox</string>
    <key>CFBundleShortVersionString</key>
    <string>144.0.2</string>
</dict>
</plist>"#,
    )
    .unwrap();

    let discovered =
        discover_browser(EngineKind::Firefox, true, Some(&bin)).expect("discover via Info.plist");
    assert!(
        discovered.version.contains("144.0.2"),
        "expected version from Info.plist, got {:?}",
        discovered.version
    );
    assert!(
        !sentinel.exists(),
        "discovery must NOT spawn firefox --version when Info.plist exists"
    );
}

#[test]
fn firefox_fallback_probe_sets_crashreporter_env_and_does_not_retry_signal_crash() {
    let dir = tempfile::tempdir().expect("tempdir");

    // 1. Verify MOZ_CRASHREPORTER_* env vars on fallback --version spawn
    let env_bin = dir.path().join("no-meta-firefox-env");
    write_executable_script(
        &env_bin,
        "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$MOZ_CRASHREPORTER_DISABLE\" \"$MOZ_CRASHREPORTER\" \"$MOZ_CRASHREPORTER_NO_REPORT\"\n",
    );
    let env_discovered =
        discover_browser(EngineKind::Firefox, true, Some(&env_bin)).expect("fallback env probe");
    assert_eq!(env_discovered.version, "1:0:1");

    // 2. Verify signal crash on Firefox is attempted at most ONCE (no 3x retry)
    let counter = dir.path().join("attempts.log");
    let crash_bin = dir.path().join("no-meta-firefox-crash");
    write_executable_script(
        &crash_bin,
        &format!(
            "#!/bin/sh\necho attempt >> \"{}\"\nkill -KILL $$\n",
            counter.display()
        ),
    );
    let err = discover_browser(EngineKind::Firefox, true, Some(&crash_bin))
        .expect_err("signal-crashing fallback must error");
    assert!(
        matches!(err, MimikyuError::LaunchFailed(_)),
        "expected LaunchFailed, got {err:?}"
    );
    let log = std::fs::read_to_string(&counter).expect("read counter");
    assert_eq!(
        log.lines().count(),
        1,
        "Firefox fallback --version must not retry 3x on signal crash; log={log:?}"
    );
}
