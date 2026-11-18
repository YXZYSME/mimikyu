use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        &["--no-daemon", "session", "new", "--viewport", "0x720", "--json"],
        &["--no-daemon", "snapshot", "--max-depth", "-1", "--json"],
        &["--no-daemon", "wait", "--json"],
        &["--no-daemon", "batch", "--json"],
        &["--no-daemon", "batch", "--file", "/tmp/a", "--ops", "[]", "--json"],
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
            panic!("expected JSON error envelope for {:?}, got stdout={stdout_s:?} err={e}", args)
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
        handles.push(thread::spawn(move || run_cli(&sd, &["session", "list", "--json"])));
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
    let out_undef = run_cli(&state_dir, &["--session", "c1", "eval", "undefined", "--json"]);
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
    assert!(fs::read(&shot_path).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));

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
    assert_eq!(lines.len(), 2, "expected 2 JSON-RPC responses, got {lines:?}");

    let init: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(init["id"], 1);
    assert!(init["result"]["capabilities"].is_object());

    let tools_list: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(tools_list["id"], 2);
    assert_eq!(tools_list["result"]["tools"].as_array().unwrap().len(), 31);
}
