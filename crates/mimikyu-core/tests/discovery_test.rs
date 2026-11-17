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
