use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    assert!(val["data"]["skipped_candidates"].is_array());
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
    let skipped = val["data"]["skipped_candidates"]
        .as_array()
        .expect("skipped_candidates array");
    assert!(
        !skipped
            .iter()
            .any(|s| s["path"].as_str() == Some("/nonexistent/mimikyu/chrome")),
        "invalid env override should stay in warnings, not skipped_candidates: {skipped:?}"
    );
}

#[test]
fn mimikyu_doctor_records_broken_candidate_in_skipped_candidates_not_warnings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let broken_bin = tmp.path().join("chromium-0.0.1/chrome");
    fs::create_dir_all(broken_bin.parent().unwrap()).expect("mkdir");
    fs::write(
        &broken_bin,
        "#!/bin/sh\necho 'broken candidate fixture' >&2\nexit 1\n",
    )
    .expect("write broken script");
    let mut perms = fs::metadata(&broken_bin).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&broken_bin, perms).expect("chmod");

    let bin = env!("CARGO_BIN_EXE_mimikyu");
    let json_out = Command::new(bin)
        .args(["doctor", "--json"])
        .env("MIMIKYU_BROWSERS_DIR", tmp.path())
        .output()
        .expect("run mimikyu doctor --json with broken managed candidate");
    assert!(
        json_out.status.success(),
        "doctor --json failed: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let val: Value = serde_json::from_slice(&json_out.stdout).expect("parse JSON envelope");
    assert_eq!(val["ok"], true);
    assert!(val["data"]["default_chromium"].is_string());
    assert!(val["data"]["default_firefox"].is_string());

    let warnings = val["data"]["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.is_empty(),
        "broken unused candidate must not pollute warnings when healthy binaries exist, got {warnings:?}"
    );

    let skipped = val["data"]["skipped_candidates"]
        .as_array()
        .expect("skipped_candidates array");
    let broken_str = broken_bin.to_string_lossy().to_string();
    let entry = skipped
        .iter()
        .find(|s| s["path"].as_str() == Some(broken_str.as_str()))
        .unwrap_or_else(|| {
            panic!(
                "expected broken candidate '{broken_str}' in skipped_candidates, got {skipped:?}"
            )
        });
    assert_eq!(entry["engine"], "chromium");
    assert_eq!(entry["source"], "mimikyu_managed");
    assert_eq!(entry["available"], false);
    assert!(
        entry["reason"]
            .as_str()
            .is_some_and(|r| !r.trim().is_empty()),
        "expected non-empty skip reason, got {entry:?}"
    );

    let human_out = Command::new(bin)
        .args(["doctor"])
        .env("MIMIKYU_BROWSERS_DIR", tmp.path())
        .output()
        .expect("run mimikyu doctor human output");
    assert!(human_out.status.success());
    let stdout = String::from_utf8_lossy(&human_out.stdout);
    assert!(
        stdout.contains("Skipped candidates ("),
        "expected 'Skipped candidates (' section in human output:\n{stdout}"
    );
    assert!(
        stdout.contains(&broken_str),
        "expected broken candidate path in human output:\n{stdout}"
    );
    assert!(
        !stdout.contains("\nWarnings:\n"),
        "human output must not print Warnings section when warnings is empty:\n{stdout}"
    );
}
