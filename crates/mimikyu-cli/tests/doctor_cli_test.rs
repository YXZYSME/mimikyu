use std::process::Command;

use serde_json::Value;

#[test]
fn mimikyu_doctor_json_outputs_valid_envelope() {
    let bin = env!("CARGO_BIN_EXE_mimikyu");
    let out = Command::new(bin)
        .args(["doctor", "--json"])
        .output()
        .expect("run mimikyu doctor --json");
    assert!(
        out.status.success(),
        "doctor --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let val: Value = serde_json::from_slice(&out.stdout).expect("parse JSON envelope");
    assert_eq!(val["ok"], true);
    assert!(val["data"]["default_chromium"].is_string());
    assert!(val["data"]["default_firefox"].is_string());
}

#[test]
fn mimikyu_doctor_warns_on_invalid_env_override() {
    let bin = env!("CARGO_BIN_EXE_mimikyu");
    let out = Command::new(bin)
        .args(["doctor", "--json"])
        .env("MIMIKYU_CHROMIUM_PATH", "/nonexistent/mimikyu/chrome")
        .output()
        .expect("run mimikyu doctor --json with bad env");
    assert!(out.status.success());
    let val: Value = serde_json::from_slice(&out.stdout).expect("parse JSON envelope");
    assert_eq!(val["ok"], true);
    assert!(val["data"]["default_chromium"].is_null());
    let warnings = val["data"]["warnings"].as_array().expect("warnings array");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("MIMIKYU_CHROMIUM_PATH")),
        "expected MIMIKYU_CHROMIUM_PATH warning, got {warnings:?}"
    );
}
