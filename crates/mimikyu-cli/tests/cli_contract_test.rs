use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mimikyu")
}

struct DaemonCleanupGuard {
    state_dir: PathBuf,
}

impl Drop for DaemonCleanupGuard {
    fn drop(&mut self) {
        let _ = Command::new(bin())
            .args(["--state-dir"])
            .arg(&self.state_dir)
            .args(["daemon", "stop"])
            .output();
    }
}

struct LocalMiniServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl LocalMiniServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("local_addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let html = concat!(
                "<!doctype html><html><head><title>CLI Test</title></head>",
                "<body><h1>CLI Page</h1>",
                "<input id=\"name\" type=\"text\" value=\"\" />",
                "<button id=\"btn\" onclick=\"document.getElementById('out').textContent='ok:' + document.getElementById('name').value\">Go</button>",
                "<div id=\"out\">init</div>",
                "<div style=\"height:1600px;\">spacer</div>",
                "<script>window.__mainSecret = 'main-only-value';</script>",
                "</body></html>"
            );
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0u8; 2048];
                        let _ = stream.read(&mut buf);
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            html.len(),
                            html
                        );
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for LocalMiniServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
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
fn cli_help_all_subcommands_r3_compliant() {
    let help_invocations: &[&[&str]] = &[
        &["--help"],
        &["doctor", "--help"],
        &["daemon", "--help"],
        &["daemon", "status", "--help"],
        &["daemon", "stop", "--help"],
        &["session", "--help"],
        &["session", "new", "--help"],
        &["session", "list", "--help"],
        &["session", "close", "--help"],
        &["session", "export", "--help"],
        &["session", "import", "--help"],
        &["close", "--help"],
        &["profile", "--help"],
        &["profile", "list", "--help"],
        &["profile", "delete", "--help"],
        &["profile", "export", "--help"],
        &["profile", "import", "--help"],
        &["context", "--help"],
        &["context", "list", "--help"],
        &["context", "close", "--help"],
        &["open", "--help"],
        &["back", "--help"],
        &["forward", "--help"],
        &["reload", "--help"],
        &["snapshot", "--help"],
        &["click", "--help"],
        &["fill", "--help"],
        &["type", "--help"],
        &["select", "--help"],
        &["upload", "--help"],
        &["scroll", "--help"],
        &["press", "--help"],
        &["wait", "--help"],
        &["batch", "--help"],
        &["extract", "--help"],
        &["eval", "--help"],
        &["screenshot", "--help"],
        &["pdf", "--help"],
        &["logs", "--help"],
        &["mcp", "--help"],
        &["bench", "--help"],
    ];

    let forbidden = r3_forbidden_keywords();
    for args in help_invocations {
        let out = Command::new(bin()).args(*args).output().expect("help");
        assert!(
            out.status.success(),
            "`mimikyu {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .to_ascii_lowercase();
        assert!(!combined.contains("panicked at"));
        for kw in &forbidden {
            assert!(
                !combined.contains(kw),
                "R3 keyword '{kw}' found in `mimikyu {}`",
                args.join(" ")
            );
        }
    }
}

#[test]
fn cli_doctor_is_standalone_and_ignores_stale_daemon_files() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("doc_state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("daemon.sock"), b"stale_not_a_socket").unwrap();
    fs::write(state_dir.join("daemon.pid"), b"99999999\n").unwrap();

    let out = run_cli(&state_dir, &["doctor", "--json"]);
    assert!(out.status.success());
    let val: Value = serde_json::from_slice(&out.stdout).expect("doctor JSON");
    assert_eq!(val["ok"], true);
    assert!(val["data"]["platform"].is_string());

    // doctor must not clean up or spawn daemon files
    assert_eq!(
        fs::read_to_string(state_dir.join("daemon.sock")).unwrap(),
        "stale_not_a_socket"
    );
    assert_eq!(
        fs::read_to_string(state_dir.join("daemon.pid")).unwrap(),
        "99999999\n"
    );
}

#[test]
fn cli_boundary_validation_and_json_error_envelopes() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("bad_state");
    fs::create_dir_all(&state_dir).unwrap();

    let bad_cases: &[&[&str]] = &[
        &["--engine", "safari", "doctor", "--json"],
        &[
            "--no-daemon",
            "session",
            "new",
            "--viewport",
            "0x720",
            "--json",
        ],
        &["--no-daemon", "snapshot", "--max-depth", "-1", "--json"],
        &["--no-daemon", "wait", "--json"],
        &["--no-daemon", "batch", "--json"],
        &[
            "--no-daemon",
            "batch",
            "--file",
            "/tmp/a",
            "--ops",
            "[]",
            "--json",
        ],
        &["--no-daemon", "eval", "1+1", "--json"],
        &["bench", "--iterations", "0", "--json"],
        &["bench", "--iterations", "-1", "--json"],
        &["bench", "--warmup", "-1", "--json"],
        &["bench", "--bogus-bench-flag", "--json"],
        &["doctor", "--nonexistent-doctor-flag", "--json"],
    ];

    for args in bad_cases {
        let out = run_cli(&state_dir, args);
        assert!(
            !out.status.success(),
            "expected failure for args {:?}",
            args
        );
        let stdout_s = String::from_utf8_lossy(&out.stdout);
        let stderr_s = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout_s.to_ascii_lowercase().contains("panicked at")
                && !stderr_s.to_ascii_lowercase().contains("panicked at"),
            "panic output on {:?}",
            args
        );
        let val: Value = serde_json::from_str(stdout_s.trim()).unwrap_or_else(|e| {
            panic!(
                "expected JSON error envelope for {:?}, got stdout={stdout_s:?} err={e}",
                args
            )
        });
        assert_eq!(val["ok"], false);
        assert!(val["error"].as_str().is_some_and(|s| !s.is_empty()));
    }
}

#[test]
fn cli_daemon_auto_spawn_concurrent_and_foreground_idle_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let state_conc = tmp.path().join("conc");
    let _guard = DaemonCleanupGuard {
        state_dir: state_conc.clone(),
    };

    let mut handles = Vec::new();
    for _ in 0..3 {
        let sd = state_conc.clone();
        handles.push(thread::spawn(move || {
            run_cli(&sd, &["session", "list", "--json"])
        }));
    }
    for h in handles {
        let out = h.join().unwrap();
        assert!(
            out.status.success(),
            "concurrent session list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let val: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(val["ok"], true);
        assert!(val["data"].is_array());
    }

    let status_out = run_cli(&state_conc, &["daemon", "status", "--json"]);
    assert!(status_out.status.success());
    let status_val: Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_val["data"]["running"], true);

    let stop_out = run_cli(&state_conc, &["daemon", "stop", "--json"]);
    assert!(stop_out.status.success());
    let stop_val: Value = serde_json::from_slice(&stop_out.stdout).unwrap();
    assert_eq!(stop_val["data"]["stopped"], true);

    // Foreground idle timeout test
    let state_fg = tmp.path().join("fg");
    let mut child = Command::new(bin())
        .args(["--state-dir"])
        .arg(&state_fg)
        .args(["daemon", "--foreground", "--idle-timeout", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn foreground daemon");

    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "foreground daemon did not exit within 6s"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "foreground daemon exit code: {status}");
}

#[test]
fn cli_end_to_end_session_page_eval_batch_and_teardown() {
    let srv = LocalMiniServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("e2e_cli");
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    // Create session with suffix global flags (--session / --context alias, --state-dir)
    let out_new = Command::new(bin())
        .args([
            "session",
            "new",
            "--ephemeral",
            "--block-media",
            "--block-fonts",
            "--context",
            "c1",
            "--state-dir",
        ])
        .arg(&state_dir)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        out_new.status.success(),
        "session new failed: {}",
        String::from_utf8_lossy(&out_new.stdout)
    );
    let new_val: Value = serde_json::from_slice(&out_new.stdout).unwrap();
    assert_eq!(new_val["ok"], true);
    let pid = new_val["data"]["pid"].as_i64().expect("ephemeral pid") as i32;

    // session list emits both id and session_id
    let out_list = run_cli(&state_dir, &["session", "list", "--json"]);
    assert!(out_list.status.success());
    let list_val: Value = serde_json::from_slice(&out_list.stdout).unwrap();
    let first = &list_val["data"][0];
    assert_eq!(first["id"], "c1");
    assert_eq!(first["session_id"], "c1");

    // open
    let url = srv.url("/");
    let out_open = run_cli(&state_dir, &["--session", "c1", "open", &url, "--json"]);
    assert!(out_open.status.success());

    // snapshot
    let out_snap = run_cli(&state_dir, &["--session", "c1", "snapshot", "--json"]);
    assert!(out_snap.status.success());
    let snap_val: Value = serde_json::from_slice(&out_snap.stdout).unwrap();
    assert!(snap_val["data"]["ref_count"].as_u64().unwrap_or(0) > 0);

    // eval undefined -> {"value": null, "main_world": false}
    let out_undef = run_cli(
        &state_dir,
        &["--session", "c1", "eval", "undefined", "--json"],
    );
    assert!(out_undef.status.success());
    let undef_val: Value = serde_json::from_slice(&out_undef.stdout).unwrap();
    assert!(undef_val["data"]["value"].is_null());
    assert_eq!(undef_val["data"]["main_world"], false);

    // eval isolated vs --main-world
    let out_iso = run_cli(
        &state_dir,
        &["--session", "c1", "eval", "window.__mainSecret", "--json"],
    );
    assert!(out_iso.status.success());
    let iso_val: Value = serde_json::from_slice(&out_iso.stdout).unwrap();
    assert!(iso_val["data"]["value"].is_null());

    let out_mw = run_cli(
        &state_dir,
        &[
            "--session",
            "c1",
            "eval",
            "window.__mainSecret",
            "--main-world",
            "--json",
        ],
    );
    assert!(out_mw.status.success());
    let mw_val: Value = serde_json::from_slice(&out_mw.stdout).unwrap();
    assert_eq!(mw_val["data"]["value"], "main-only-value");
    assert_eq!(mw_val["data"]["main_world"], true);

    // scroll with negative dy after `--`
    let out_scroll = run_cli(
        &state_dir,
        &["--session", "c1", "scroll", "--json", "--", "0", "-140"],
    );
    assert!(
        out_scroll.status.success(),
        "scroll failed: {}",
        String::from_utf8_lossy(&out_scroll.stdout)
    );

    // batch with "op" + mid-step failure -> must exit non-zero and emit ok: false
    let bad_ops = r##"[{"op":"fill","target":"#name","value":"step1"},{"op":"click","target":"#missing-xyz"}]"##;
    let out_batch_fail = run_cli(
        &state_dir,
        &["--session", "c1", "batch", "--ops", bad_ops, "--json"],
    );
    assert!(
        !out_batch_fail.status.success(),
        "batch mid-step failure must exit non-zero"
    );
    let bf_val: Value = serde_json::from_slice(&out_batch_fail.stdout).unwrap();
    assert_eq!(bf_val["ok"], false);
    assert!(bf_val["error"].as_str().is_some_and(|s| !s.is_empty()));

    // screenshot with CSS target
    let shot_path = tmp.path().join("btn.png");
    let shot_str = shot_path.to_string_lossy().to_string();
    let out_shot = run_cli(
        &state_dir,
        &[
            "--session",
            "c1",
            "screenshot",
            &shot_str,
            "--target",
            "#btn",
            "--json",
        ],
    );
    assert!(
        out_shot.status.success(),
        "screenshot failed: {}",
        String::from_utf8_lossy(&out_shot.stdout)
    );
    assert!(
        fs::read(&shot_path)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );

    // close session and stop daemon
    let out_close = run_cli(&state_dir, &["close", "c1", "--json"]);
    assert!(out_close.status.success());

    let out_stop = run_cli(&state_dir, &["daemon", "stop", "--json"]);
    assert!(out_stop.status.success());

    // Confirm browser PID is reaped
    assert!(
        !mimikyu_core::daemon::is_pid_alive(pid),
        "browser pid {pid} should be reaped"
    );
}

#[test]
fn cli_mcp_stdio_roundtrip_and_eof_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("mcp_cli");

    let mut child = Command::new(bin())
        .args(["--state-dir"])
        .arg(&state_dir)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mimikyu mcp");

    {
        let stdin = child.stdin.as_mut().unwrap();
        let reqs = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"cli-test\",\"version\":\"0.1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n"
        );
        stdin.write_all(reqs.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "mcp exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_s = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_s.contains("Content-Length:"));
    let lines: Vec<&str> = stdout_s.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 JSON-RPC responses, got {lines:?}"
    );

    let init: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(init["id"], 1);
    assert!(init["result"]["capabilities"].is_object());

    let tools_list: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(tools_list["id"], 2);
    assert_eq!(tools_list["result"]["tools"].as_array().unwrap().len(), 31);
}

#[test]
fn cli_daemon_idle_timeout_zero_and_status_no_dir_create() {
    let tmp = tempfile::tempdir().unwrap();
    let missing_dir = tmp.path().join("missing_state_dir");
    assert!(!missing_dir.exists());

    let st_missing = run_cli(&missing_dir, &["daemon", "status", "--json"]);
    assert!(st_missing.status.success());
    let st_missing_val: Value = serde_json::from_slice(&st_missing.stdout).unwrap();
    assert_eq!(st_missing_val["ok"], true);
    assert_eq!(st_missing_val["data"]["running"], false);
    assert!(
        !missing_dir.exists(),
        "daemon status must not create state_dir"
    );

    let state_dir = tmp.path().join("idle_zero_state");
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    let start_out = run_cli(&state_dir, &["daemon", "--idle-timeout", "0", "--json"]);
    assert!(
        start_out.status.success(),
        "daemon --idle-timeout 0 failed: {}",
        String::from_utf8_lossy(&start_out.stdout)
    );
    thread::sleep(Duration::from_millis(600));
    let st = run_cli(&state_dir, &["daemon", "status", "--json"]);
    assert!(st.status.success());
    let st_val: Value = serde_json::from_slice(&st.stdout).unwrap();
    assert_eq!(st_val["data"]["running"], true);
    assert!(st_val["data"]["idle_timeout_secs"].is_null());

    let stop_out = run_cli(&state_dir, &["daemon", "stop", "--json"]);
    assert!(stop_out.status.success());
}

#[test]
fn cli_daemon_resolves_relative_screenshot_and_export_paths_against_client_cwd() {
    let srv = LocalMiniServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("cwd_state");
    let dir_a = tmp.path().join("dir_a");
    let dir_b = tmp.path().join("dir_b");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    // Start daemon with CWD = dir_a
    let start = Command::new(bin())
        .current_dir(&dir_a)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["session", "list", "--json"])
        .output()
        .unwrap();
    assert!(start.status.success());

    // Run client commands from CWD = dir_b using relative paths
    let run_in_b = |args: &[&str]| {
        Command::new(bin())
            .current_dir(&dir_b)
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(args)
            .output()
            .unwrap()
    };

    assert!(
        run_in_b(&[
            "--session",
            "cwd_s",
            "session",
            "new",
            "--ephemeral",
            "--json"
        ])
        .status
        .success()
    );
    assert!(
        run_in_b(&["--session", "cwd_s", "open", &srv.url("/"), "--json"])
            .status
            .success()
    );

    let shot_out = run_in_b(&["--session", "cwd_s", "screenshot", "./shot.png", "--json"]);
    assert!(
        shot_out.status.success(),
        "{}",
        String::from_utf8_lossy(&shot_out.stdout)
    );
    assert!(dir_b.join("shot.png").is_file());
    assert!(!dir_a.join("shot.png").exists());

    let exp_out = run_in_b(&[
        "--session",
        "cwd_s",
        "session",
        "export",
        "./state.json",
        "--json",
    ]);
    assert!(
        exp_out.status.success(),
        "{}",
        String::from_utf8_lossy(&exp_out.stdout)
    );
    assert!(dir_b.join("state.json").is_file());
    assert!(!dir_a.join("state.json").exists());

    let imp_out = run_in_b(&[
        "--session",
        "cwd_s",
        "session",
        "import",
        "./state.json",
        "--json",
    ]);
    assert!(
        imp_out.status.success(),
        "{}",
        String::from_utf8_lossy(&imp_out.stdout)
    );

    assert!(run_in_b(&["close", "cwd_s", "--json"]).status.success());
    let _ = run_cli(&state_dir, &["daemon", "stop", "--json"]);
}

#[test]
fn cli_daemon_resolves_relative_profile_path_against_client_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("prof_cwd_state");
    let dir_a = tmp.path().join("prof_dir_a");
    let dir_b = tmp.path().join("prof_dir_b");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    let start = Command::new(bin())
        .current_dir(&dir_a)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["session", "list", "--json"])
        .output()
        .unwrap();
    assert!(start.status.success());

    let snew = Command::new(bin())
        .current_dir(&dir_b)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--session",
            "rp",
            "session",
            "new",
            "--profile",
            "./rel_prof",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        snew.status.success(),
        "session new --profile ./rel_prof failed: {}",
        String::from_utf8_lossy(&snew.stdout)
    );
    assert!(dir_b.join("rel_prof").is_dir());
    assert!(!dir_a.join("rel_prof").exists());

    assert!(
        run_cli(&state_dir, &["close", "rp", "--json"])
            .status
            .success()
    );
    let _ = run_cli(&state_dir, &["daemon", "stop", "--json"]);
}

#[test]
fn cli_profile_export_nonexistent_fails_without_creating_profile_and_timeout_is_clean() {
    let srv = LocalMiniServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("prof_exp_state");
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    let out_file = tmp.path().join("missing_export.json");
    let out_str = out_file.to_string_lossy().to_string();
    let exp = run_cli(
        &state_dir,
        &["profile", "export", "nonexistent_prof", &out_str, "--json"],
    );
    assert!(
        !exp.status.success(),
        "export of nonexistent profile must fail"
    );
    let exp_val: Value = serde_json::from_slice(&exp.stdout).unwrap();
    assert_eq!(exp_val["ok"], false);
    assert!(!state_dir.join("profiles").join("nonexistent_prof").exists());
    assert!(!out_file.exists());

    assert!(
        run_cli(
            &state_dir,
            &[
                "--session",
                "tmo",
                "session",
                "new",
                "--ephemeral",
                "--json"
            ]
        )
        .status
        .success()
    );
    assert!(
        run_cli(
            &state_dir,
            &["--session", "tmo", "open", &srv.url("/"), "--json"]
        )
        .status
        .success()
    );
    let wait_out = run_cli(
        &state_dir,
        &[
            "--session",
            "tmo",
            "wait",
            "--text",
            "NEVER_THERE",
            "--timeout-ms",
            "150",
            "--json",
        ],
    );
    assert!(!wait_out.status.success());
    let wait_val: Value = serde_json::from_slice(&wait_out.stdout).unwrap();
    assert_eq!(wait_val["ok"], false);
    let err_msg = wait_val["error"].as_str().unwrap_or("");
    assert!(err_msg.contains("150ms"), "unexpected error: {err_msg}");
    assert!(
        !err_msg.contains("after 0ms:"),
        "double-prefixed timeout: {err_msg}"
    );

    assert!(
        run_cli(&state_dir, &["close", "tmo", "--json"])
            .status
            .success()
    );
    let _ = run_cli(&state_dir, &["daemon", "stop", "--json"]);
}

#[test]
fn cli_profile_import_prevalidates_and_batch_wait_idle_honors_stable_target() {
    let srv = LocalMiniServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("imp_batch_state");
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    let missing_file = tmp.path().join("does_not_exist.json");
    let imp_missing = run_cli(
        &state_dir,
        &[
            "profile",
            "import",
            "bad_missing",
            missing_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!imp_missing.status.success());
    assert!(!state_dir.join("profiles").join("bad_missing").exists());

    let bad_json = tmp.path().join("bad.json");
    fs::write(&bad_json, b"{not valid json").unwrap();
    let imp_bad = run_cli(
        &state_dir,
        &[
            "profile",
            "import",
            "bad_json",
            bad_json.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!imp_bad.status.success());
    assert!(!state_dir.join("profiles").join("bad_json").exists());

    assert!(
        run_cli(
            &state_dir,
            &["--session", "bw", "session", "new", "--ephemeral", "--json"]
        )
        .status
        .success()
    );
    let ok_ops = format!(
        r##"[{{"op":"open","url":{}}},{{"op":"wait","idle":true,"stable_target":"#out"}}]"##,
        serde_json::to_string(&srv.url("/")).unwrap()
    );
    let batch_ok = run_cli(
        &state_dir,
        &["--session", "bw", "batch", "--ops", &ok_ops, "--json"],
    );
    assert!(
        batch_ok.status.success(),
        "batch wait_idle with stable_target failed: {}",
        String::from_utf8_lossy(&batch_ok.stdout)
    );

    let bad_ops =
        r##"[{"op":"wait","idle":true,"stable_target":"#nonexistent-xyz","timeout_ms":100}]"##;
    let batch_bad = run_cli(
        &state_dir,
        &["--session", "bw", "batch", "--ops", bad_ops, "--json"],
    );
    assert!(!batch_bad.status.success());

    assert!(
        run_cli(&state_dir, &["close", "bw", "--json"])
            .status
            .success()
    );
    let _ = run_cli(&state_dir, &["daemon", "stop", "--json"]);
}

#[test]
fn cli_daemon_concurrent_commands_during_stop_and_idle_timeout_firefox() {
    let tmp = tempfile::tempdir().unwrap();

    for iter in 0..3 {
        let state_dir = tmp.path().join(format!("ff_conc_stop_{iter}"));
        let _guard = DaemonCleanupGuard {
            state_dir: state_dir.clone(),
        };

        for sid in ["ff1", "ff2"] {
            let snew = run_cli(
                &state_dir,
                &[
                    "--engine",
                    "firefox",
                    "--session",
                    sid,
                    "session",
                    "new",
                    "--ephemeral",
                    "--json",
                ],
            );
            assert!(
                snew.status.success(),
                "iter {iter} {sid} session new failed: {}",
                String::from_utf8_lossy(&snew.stdout)
            );
        }

        let sd_stop = state_dir.clone();
        let stop_handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(iter * 2));
            run_cli(&sd_stop, &["daemon", "stop", "--json"])
        });

        let mut list_handles = Vec::new();
        for idx in 0..6 {
            let sd_list = state_dir.clone();
            list_handles.push(thread::spawn(move || {
                thread::sleep(Duration::from_millis(idx * 5));
                run_cli(&sd_list, &["session", "list", "--json"])
            }));
        }

        let stop_out = stop_handle.join().unwrap();
        assert!(
            stop_out.status.success(),
            "iter {iter} daemon stop failed: {}",
            String::from_utf8_lossy(&stop_out.stdout)
        );
        for (idx, h) in list_handles.into_iter().enumerate() {
            let out = h.join().unwrap();
            assert!(
                out.status.success(),
                "iter {iter} list {idx} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(v["ok"], true);
        }
        let _ = run_cli(&state_dir, &["daemon", "stop", "--json"]);
    }

    // Idle timeout = 1s with 3 active Firefox ephemeral sessions + concurrent session list storm
    let idle_state = tmp.path().join("ff_idle_storm");
    let _idle_guard = DaemonCleanupGuard {
        state_dir: idle_state.clone(),
    };
    let dstart = run_cli(
        &idle_state,
        &[
            "--engine",
            "firefox",
            "daemon",
            "--idle-timeout",
            "1",
            "--json",
        ],
    );
    assert!(
        dstart.status.success(),
        "{}",
        String::from_utf8_lossy(&dstart.stdout)
    );

    for sid in ["ffi1", "ffi2", "ffi3"] {
        let snew = run_cli(
            &idle_state,
            &[
                "--engine",
                "firefox",
                "--session",
                sid,
                "session",
                "new",
                "--ephemeral",
                "--json",
            ],
        );
        assert!(
            snew.status.success(),
            "{}",
            String::from_utf8_lossy(&snew.stdout)
        );
    }

    thread::sleep(Duration::from_millis(1150));
    let mut storm = Vec::new();
    for idx in 0..6 {
        let sd = idle_state.clone();
        storm.push(thread::spawn(move || {
            thread::sleep(Duration::from_millis(idx * 40));
            run_cli(&sd, &["session", "list", "--json"])
        }));
    }
    for (idx, h) in storm.into_iter().enumerate() {
        let out = h.join().unwrap();
        assert!(
            out.status.success(),
            "idle storm list {idx} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["ok"], true);
    }
    let _ = run_cli(&idle_state, &["daemon", "stop", "--json"]);
}

#[test]
fn cli_profile_import_persists_origins_chromium_and_firefox_and_rejects_empty_cookie_domain() {
    let srv = LocalMiniServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("imp_origins_state");
    let _guard = DaemonCleanupGuard {
        state_dir: state_dir.clone(),
    };

    let target_url = srv.url("/");
    let origin = target_url.trim_end_matches('/').to_string();

    let bad_cookie_state = tmp.path().join("empty_domain_state.json");
    fs::write(
        &bad_cookie_state,
        serde_json::to_vec(&serde_json::json!({
            "cookies": [{
                "name": "bad",
                "value": "v",
                "domain": "",
                "path": "/",
                "expires": null,
                "http_only": false,
                "secure": false,
                "same_site": null
            }],
            "origins": []
        }))
        .unwrap(),
    )
    .unwrap();
    let bad_imp = run_cli(
        &state_dir,
        &[
            "profile",
            "import",
            "bad_cookie_prof",
            bad_cookie_state.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!bad_imp.status.success(), "empty cookie domain must fail");
    assert!(
        !state_dir.join("profiles").join("bad_cookie_prof").exists(),
        "empty profile dir must be rolled back on cookie import failure"
    );

    let good_state = tmp.path().join("good_storage_state.json");
    fs::write(
        &good_state,
        serde_json::to_vec(&serde_json::json!({
            "cookies": [{
                "name": "cli_ck",
                "value": "cli_cv",
                "domain": "127.0.0.1",
                "path": "/",
                "expires": null,
                "http_only": false,
                "secure": false,
                "same_site": null
            }],
            "origins": [{
                "origin": origin,
                "local_storage": {"ls_key": "ls_cli_val"},
                "session_storage": {"ss_key": "ss_cli_val"}
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let state_str = good_state.to_string_lossy().to_string();

    for engine in ["chromium", "firefox"] {
        let prof_name = format!("imp_prof_{engine}");
        let sid = format!("imp_sid_{engine}");

        let imp = run_cli(
            &state_dir,
            &[
                "--engine", engine, "profile", "import", &prof_name, &state_str, "--json",
            ],
        );
        assert!(
            imp.status.success(),
            "{engine} profile import failed: {}",
            String::from_utf8_lossy(&imp.stdout)
        );

        let snew = run_cli(
            &state_dir,
            &[
                "--engine",
                engine,
                "--session",
                &sid,
                "session",
                "new",
                "--profile",
                &prof_name,
                "--json",
            ],
        );
        assert!(
            snew.status.success(),
            "{engine} session new --profile failed: {}",
            String::from_utf8_lossy(&snew.stdout)
        );

        let open_out = run_cli(
            &state_dir,
            &["--session", &sid, "open", &target_url, "--json"],
        );
        assert!(
            open_out.status.success(),
            "{engine} open failed: {}",
            String::from_utf8_lossy(&open_out.stdout)
        );

        let ls_out = run_cli(
            &state_dir,
            &[
                "--session",
                &sid,
                "eval",
                "localStorage.getItem('ls_key')",
                "--json",
            ],
        );
        assert!(ls_out.status.success());
        let ls_val: Value = serde_json::from_slice(&ls_out.stdout).unwrap();
        assert_eq!(
            ls_val["data"]["value"], "ls_cli_val",
            "{engine} localStorage mismatch: {ls_val}"
        );

        let ss_out = run_cli(
            &state_dir,
            &[
                "--session",
                &sid,
                "eval",
                "sessionStorage.getItem('ss_key')",
                "--json",
            ],
        );
        assert!(ss_out.status.success());
        let ss_val: Value = serde_json::from_slice(&ss_out.stdout).unwrap();
        assert_eq!(
            ss_val["data"]["value"], "ss_cli_val",
            "{engine} sessionStorage mismatch: {ss_val}"
        );

        let ck_out = run_cli(
            &state_dir,
            &["--session", &sid, "eval", "document.cookie", "--json"],
        );
        assert!(ck_out.status.success());
        let ck_val: Value = serde_json::from_slice(&ck_out.stdout).unwrap();
        assert!(
            ck_val["data"]["value"]
                .as_str()
                .unwrap_or("")
                .contains("cli_ck=cli_cv"),
            "{engine} cookie mismatch: {ck_val}"
        );

        assert!(
            run_cli(&state_dir, &["close", &sid, "--json"])
                .status
                .success()
        );
    }

    let _ = run_cli(&state_dir, &["daemon", "stop", "--json"]);
}

#[test]
fn cli_daemon_stop_terminates_daemon_with_sigstop_wedged_browser_child() {
    struct WedgedCleanupGuard {
        state_dir: PathBuf,
        browser_pid: Option<i32>,
        daemon_pid: Option<i32>,
    }
    impl Drop for WedgedCleanupGuard {
        fn drop(&mut self) {
            if let Some(bpid) = self.browser_pid
                && bpid > 0
            {
                unsafe {
                    libc::kill(-(bpid as libc::pid_t), libc::SIGCONT);
                    libc::kill(-(bpid as libc::pid_t), libc::SIGKILL);
                }
            }
            if let Some(dpid) = self.daemon_pid
                && dpid > 0
            {
                unsafe {
                    libc::kill(dpid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = Command::new(bin())
                .args(["--state-dir"])
                .arg(&self.state_dir)
                .args(["daemon", "stop"])
                .output();
        }
    }

    fn pgid_alive(pgid: i32) -> bool {
        if pgid <= 0 {
            return false;
        }
        let rc = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    let tmp = tempfile::tempdir().unwrap();
    for engine in ["chromium", "firefox"] {
        let state_dir = tmp.path().join(format!("wedged_daemon_state_{engine}"));
        let mut guard = WedgedCleanupGuard {
            state_dir: state_dir.clone(),
            browser_pid: None,
            daemon_pid: None,
        };

        let snew = run_cli(
            &state_dir,
            &[
                "--engine",
                engine,
                "--session",
                "w1",
                "session",
                "new",
                "--ephemeral",
                "--json",
            ],
        );
        assert!(
            snew.status.success(),
            "{engine} session new failed: {}",
            String::from_utf8_lossy(&snew.stdout)
        );
        let snew_val: Value = serde_json::from_slice(&snew.stdout).unwrap();
        let browser_pid = snew_val["data"]["pid"].as_i64().expect("browser pid") as i32;
        guard.browser_pid = Some(browser_pid);

        let st_before = run_cli(&state_dir, &["daemon", "status", "--json"]);
        assert!(st_before.status.success());
        let st_before_val: Value = serde_json::from_slice(&st_before.stdout).unwrap();
        assert_eq!(st_before_val["data"]["running"], true);
        let daemon_pid = st_before_val["data"]["pid"].as_i64().expect("daemon pid") as i32;
        guard.daemon_pid = Some(daemon_pid);

        // Freeze the browser process group with SIGSTOP so graceful cookies/dispose hangs.
        assert!(
            browser_pid > 0,
            "{engine} expected positive browser pid, got {browser_pid}"
        );
        let stop_rc = unsafe { libc::kill(-(browser_pid as libc::pid_t), libc::SIGSTOP) };
        assert_eq!(
            stop_rc,
            0,
            "{engine} SIGSTOP on browser pgid {browser_pid} failed: {}",
            std::io::Error::last_os_error()
        );

        let stop_out = run_cli(&state_dir, &["daemon", "stop", "--json"]);
        assert!(
            stop_out.status.success(),
            "{engine} daemon stop must succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&stop_out.stdout),
            String::from_utf8_lossy(&stop_out.stderr)
        );
        let stop_val: Value = serde_json::from_slice(&stop_out.stdout).unwrap();
        assert_eq!(stop_val["ok"], true);
        assert_eq!(stop_val["data"]["stopped"], true);

        // Daemon and wedged browser PGID must both be dead (SIGTERM caught during shutdown -> Drop kills -pgid).
        assert!(
            !mimikyu_core::daemon::is_pid_alive(daemon_pid),
            "{engine} daemon pid {daemon_pid} must not remain alive"
        );
        let reap_deadline = Instant::now() + Duration::from_millis(500);
        while pgid_alive(browser_pid) && Instant::now() < reap_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !pgid_alive(browser_pid),
            "{engine} browser pgid {browser_pid} must not remain orphaned"
        );

        let st_after = run_cli(&state_dir, &["daemon", "status", "--json"]);
        assert!(st_after.status.success());
        let st_after_val: Value = serde_json::from_slice(&st_after.stdout).unwrap();
        assert_eq!(st_after_val["data"]["running"], false);

        guard.browser_pid = None;
        guard.daemon_pid = None;
    }
}

#[test]
fn cli_accepts_hyphen_values_and_hyphen_state_dir_daemon_roundtrip() {
    let srv = LocalMiniServer::start();
    let tmp = tempfile::tempdir().unwrap();

    struct RelativeDashCleanup {
        cwd: PathBuf,
        rel_dir: String,
    }
    impl Drop for RelativeDashCleanup {
        fn drop(&mut self) {
            let _ = Command::new(bin())
                .current_dir(&self.cwd)
                .args(["--state-dir", &self.rel_dir, "daemon", "stop"])
                .output();
        }
    }

    let run_rel = |rel_dir: &str, use_equals: bool, args: &[&str]| -> std::process::Output {
        let mut cmd = Command::new(bin());
        cmd.current_dir(tmp.path());
        if use_equals {
            cmd.arg(format!("--state-dir={rel_dir}"));
        } else {
            cmd.arg("--state-dir").arg(rel_dir);
        }
        cmd.args(args).output().expect("spawn mimikyu")
    };

    // 1. Both `--state-dir -dash` and `--state-dir=-dash` spawn background daemon & stop cleanly
    for (rel_dir, use_eq) in [("-dash-space", false), ("-dash-eq", true)] {
        let _cleanup = RelativeDashCleanup {
            cwd: tmp.path().to_path_buf(),
            rel_dir: rel_dir.to_string(),
        };
        let d_out = run_rel(rel_dir, use_eq, &["daemon", "--json"]);
        assert!(
            d_out.status.success(),
            "daemon start ({rel_dir}) failed: stdout={} stderr={}",
            String::from_utf8_lossy(&d_out.stdout),
            String::from_utf8_lossy(&d_out.stderr)
        );
        let d_val: Value = serde_json::from_slice(&d_out.stdout).expect("daemon JSON");
        assert_eq!(d_val["ok"], true);

        let s_out = run_rel(rel_dir, use_eq, &["daemon", "status", "--json"]);
        assert!(s_out.status.success());
        let s_val: Value = serde_json::from_slice(&s_out.stdout).expect("status JSON");
        assert_eq!(s_val["data"]["running"], true);

        let stop_out = run_rel(rel_dir, use_eq, &["daemon", "stop", "--json"]);
        assert!(stop_out.status.success());
        let stop_val: Value = serde_json::from_slice(&stop_out.stdout).expect("stop JSON");
        assert_eq!(stop_val["data"]["stopped"], true);
    }

    // 2. Hyphen-prefixed values on fill, type, eval, wait --text, wait --url, wait --selector
    let rel_work = "-dash-work";
    let _cleanup = RelativeDashCleanup {
        cwd: tmp.path().to_path_buf(),
        rel_dir: rel_work.to_string(),
    };
    let snew = run_rel(
        rel_work,
        false,
        &[
            "--engine",
            "chromium",
            "session",
            "new",
            "--virtual",
            "--json",
        ],
    );
    assert!(
        snew.status.success(),
        "session new failed: {}",
        String::from_utf8_lossy(&snew.stdout)
    );
    assert!(
        run_rel(
            rel_work,
            false,
            &["open", &srv.url("/page-50pct"), "--json"]
        )
        .status
        .success()
    );

    let fill_out = run_rel(rel_work, false, &["fill", "#name", "-100", "--json"]);
    assert!(
        fill_out.status.success(),
        "fill -100 failed: {}",
        String::from_utf8_lossy(&fill_out.stdout)
    );
    let fill_check = run_rel(
        rel_work,
        false,
        &["eval", "document.getElementById('name').value", "--json"],
    );
    let fill_val: Value = serde_json::from_slice(&fill_check.stdout).unwrap();
    assert_eq!(fill_val["data"]["value"], "-100");

    let type_out = run_rel(rel_work, false, &["type", "#name", "-abc", "--json"]);
    assert!(
        type_out.status.success(),
        "type -abc failed: {}",
        String::from_utf8_lossy(&type_out.stdout)
    );
    let type_check = run_rel(
        rel_work,
        false,
        &["eval", "document.getElementById('name').value", "--json"],
    );
    let type_val: Value = serde_json::from_slice(&type_check.stdout).unwrap();
    assert_eq!(type_val["data"]["value"], "-100-abc");

    let eval_out = run_rel(rel_work, false, &["eval", "-1 + 2", "--json"]);
    assert!(
        eval_out.status.success(),
        "eval -1 + 2 failed: {}",
        String::from_utf8_lossy(&eval_out.stdout)
    );
    let eval_val: Value = serde_json::from_slice(&eval_out.stdout).unwrap();
    assert_eq!(eval_val["data"]["value"], 1);

    let set_text = run_rel(
        rel_work,
        false,
        &[
            "eval",
            "document.getElementById('out').textContent = 'discount -50%'; 'ok'",
            "--json",
        ],
    );
    assert!(set_text.status.success());

    let wait_text_out = run_rel(
        rel_work,
        false,
        &["wait", "--text", "-50%", "--timeout-ms", "1500", "--json"],
    );
    assert!(
        wait_text_out.status.success(),
        "wait --text -50% failed: {}",
        String::from_utf8_lossy(&wait_text_out.stdout)
    );

    let wait_url_out = run_rel(
        rel_work,
        false,
        &["wait", "--url", "-50pct", "--timeout-ms", "1500", "--json"],
    );
    assert!(
        wait_url_out.status.success(),
        "wait --url -50pct failed: {}",
        String::from_utf8_lossy(&wait_url_out.stdout)
    );

    let wait_sel_out = run_rel(
        rel_work,
        false,
        &[
            "wait",
            "--selector",
            "-nonexistent",
            "--timeout-ms",
            "100",
            "--json",
        ],
    );
    // Invalid CSS selector or missing selector returns JSON error envelope, NOT clap argument rejection
    let wait_sel_s = String::from_utf8_lossy(&wait_sel_out.stdout);
    let wait_sel_v: Value = serde_json::from_str(wait_sel_s.trim()).expect("JSON envelope");
    assert_eq!(wait_sel_v["ok"], false);
    assert!(
        !wait_sel_v["error"]
            .as_str()
            .unwrap_or("")
            .contains("unexpected argument"),
        "wait --selector -nonexistent should reach wait engine, not clap: {wait_sel_v:?}"
    );

    assert!(
        run_rel(rel_work, false, &["daemon", "stop", "--json"])
            .status
            .success()
    );
}
