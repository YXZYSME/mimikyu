use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mimikyu")
}

fn r3_forbidden_keywords() -> Vec<&'static str> {
    vec![
        concat!("disable_", "coop"),
        concat!("cap", "tcha"),
        concat!("recap", "tcha"),
        concat!("hcap", "tcha"),
        concat!("turn", "stile"),
        concat!("sun", "dial"),
        concat!("cf_", "clearance"),
        concat!("waf_", "bypass"),
        concat!("waf_", "evasion"),
        concat!("waf ", "evasion"),
        concat!("gate_", "bypass"),
        concat!("gate ", "bypass"),
        concat!("challenge_", "bypass"),
        concat!("challenge_", "solver"),
        concat!("anti", "bot"),
        concat!("anti-", "bot"),
        concat!("human_", "verification"),
        concat!("human-", "verification"),
    ]
}

fn run_cli(state_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .arg("--state-dir")
        .arg(state_dir)
        .args(args)
        .output()
        .expect("spawn mimikyu")
}

#[test]
fn bench_cli_json_minimal_iterations_zero_warmup_schema() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_cli(
        tmp.path(),
        &["bench", "--iterations", "1", "--warmup", "0", "--json"],
    );
    assert!(
        out.status.success(),
        "bench failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["ok"], Value::Bool(true));

    let data = v["data"].as_object().expect("data object");
    assert_eq!(data["iterations"], Value::from(1));
    assert_eq!(data["warmup"], Value::from(0));
    assert_eq!(data["engine"], Value::from("chromium"));
    assert_eq!(data["suite"], Value::from("all"));

    for ms_key in [
        "cold_launch_ms",
        "warm_context_ms",
        "navigation_ms",
        "snapshot_ms",
        "extract_ms",
        "batch_ms",
        "avg_iteration_ms",
        "median_iteration_ms",
        "total_ms",
    ] {
        let val = data[ms_key].as_f64().expect(ms_key);
        assert!(val.is_finite() && val > 0.0, "{ms_key}={val}");
    }

    let rss_bytes = data["memory_rss_bytes"].as_u64().expect("memory_rss_bytes");
    let rss_mb = data["memory_rss_mb"].as_f64().expect("memory_rss_mb");
    assert!(rss_bytes > 0);
    assert!(rss_mb.is_finite() && rss_mb > 0.0);

    let comp = data["token_compression"].as_f64().expect("token_compression");
    assert!(comp.is_finite() && comp > 1.0);

    let tasks = data["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 5);
    for t in tasks {
        assert_eq!(t["ok"], Value::Bool(true));
        assert_eq!(t["passed"], Value::Bool(true));
        assert!(t["steps"].as_u64().unwrap_or(0) > 0);
        assert!(t["latency_ms"].as_f64().unwrap_or(0.0) > 0.0);
    }
    assert_eq!(data["completion_rate"].as_f64(), Some(1.0));
}

#[test]
fn bench_cli_reproducibility_identical_keys_and_task_ids() {
    let tmp1 = tempfile::tempdir().expect("tmp1");
    let tmp2 = tempfile::tempdir().expect("tmp2");

    let o1 = run_cli(
        tmp1.path(),
        &["bench", "--iterations", "1", "--warmup", "0", "--json"],
    );
    let o2 = run_cli(
        tmp2.path(),
        &["bench", "--iterations", "1", "--warmup", "0", "--json"],
    );
    assert!(o1.status.success());
    assert!(o2.status.success());

    let v1: Value = serde_json::from_slice(&o1.stdout).unwrap();
    let v2: Value = serde_json::from_slice(&o2.stdout).unwrap();
    let k1: BTreeSet<String> = v1["data"].as_object().unwrap().keys().cloned().collect();
    let k2: BTreeSet<String> = v2["data"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(k1, k2);

    let ids1: Vec<&str> = v1["data"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    let ids2: Vec<&str> = v2["data"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1.len(), 5);
    assert_eq!(ids1, ids2);
}

#[test]
fn bench_cli_non_json_output_and_zero_session_or_daemon_leak() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_cli(tmp.path(), &["bench", "--iterations", "1", "--warmup", "0"]);
    assert!(
        out.status.success(),
        "bench non-json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.trim().is_empty(), "expected non-empty human output");
    assert!(stdout.contains("agentic workflows: 5/5 passed"), "{stdout}");

    assert!(
        !tmp.path().join("daemon.sock").exists(),
        "bench must not spawn daemon.sock"
    );
    assert!(
        !tmp.path().join("daemon.pid").exists(),
        "bench must not create daemon.pid"
    );

    let list_out = run_cli(tmp.path(), &["--no-daemon", "session", "list", "--json"]);
    assert!(list_out.status.success());
    let list_v: Value = serde_json::from_slice(&list_out.stdout).unwrap();
    assert_eq!(list_v["ok"], Value::Bool(true));
    assert_eq!(
        list_v["data"]["sessions"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0
    );
}

#[test]
fn bench_cli_poisoned_proxy_env_immunity() {
    let unused = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let dead_port = unused.local_addr().unwrap().port();
    drop(unused);
    let dead_proxy = format!("http://127.0.0.1:{dead_port}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(bin())
        .arg("--state-dir")
        .arg(tmp.path())
        .args(["bench", "--iterations", "1", "--warmup", "0", "--json"])
        .env("HTTP_PROXY", &dead_proxy)
        .env("HTTPS_PROXY", &dead_proxy)
        .env("http_proxy", &dead_proxy)
        .env("https_proxy", &dead_proxy)
        .output()
        .expect("spawn mimikyu with poisoned proxy env");

    assert!(
        out.status.success(),
        "bench failed under poisoned proxy env: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["ok"], Value::Bool(true));
    assert_eq!(v["data"]["completion_rate"].as_f64(), Some(1.0));
}

#[test]
fn bench_cli_r3_compliance_help_json_and_human() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let forbidden = r3_forbidden_keywords();

    let help = run_cli(tmp.path(), &["bench", "--help"]);
    assert!(help.status.success());
    let help_txt = format!(
        "{}\n{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    )
    .to_ascii_lowercase();
    for kw in &forbidden {
        assert!(!help_txt.contains(kw), "R3 hit '{kw}' in bench --help");
    }

    let json_out = run_cli(
        tmp.path(),
        &[
            "bench",
            "--iterations",
            "1",
            "--warmup",
            "0",
            "--suite",
            "speed",
            "--json",
        ],
    );
    assert!(json_out.status.success());
    let json_txt = format!(
        "{}\n{}",
        String::from_utf8_lossy(&json_out.stdout),
        String::from_utf8_lossy(&json_out.stderr)
    )
    .to_ascii_lowercase();
    for kw in &forbidden {
        assert!(!json_txt.contains(kw), "R3 hit '{kw}' in bench --json");
    }

    let human_out = run_cli(
        tmp.path(),
        &["bench", "--iterations", "1", "--warmup", "0", "--suite", "speed"],
    );
    assert!(human_out.status.success());
    let human_txt = format!(
        "{}\n{}",
        String::from_utf8_lossy(&human_out.stdout),
        String::from_utf8_lossy(&human_out.stderr)
    )
    .to_ascii_lowercase();
    for kw in &forbidden {
        assert!(!human_txt.contains(kw), "R3 hit '{kw}' in bench human");
    }
}

#[test]
fn bench_cli_boundary_validation_rejects_bad_args() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for args in [
        &["bench", "--iterations", "0", "--json"][..],
        &["bench", "--iterations", "-1", "--json"][..],
        &["bench", "--warmup", "-1", "--json"][..],
        &["bench", "--suite", "bogus", "--json"][..],
        &["bench", "--iterations", "abc", "--json"][..],
        &["bench", "--bogus-flag", "--json"][..],
    ] {
        let out = run_cli(tmp.path(), args);
        assert!(
            !out.status.success(),
            "expected failure for {args:?}, got success"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked at"), "panic on {args:?}");
        let v: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("invalid JSON envelope for {args:?}: {e}; stdout={stdout}"));
        assert_eq!(v["ok"], Value::Bool(false));
        assert!(
            !v["error"].as_str().unwrap_or("").is_empty(),
            "empty error for {args:?}"
        );
    }
}
