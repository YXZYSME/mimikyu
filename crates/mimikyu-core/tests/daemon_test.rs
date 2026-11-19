mod support;

use std::collections::HashSet;
use std::fs;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::sync::OnceLock;
use std::time::Duration;

use mimikyu_core::daemon::{
    daemon_status, daemon_stop, parse_batch_actions, resolve_socket_paths, DaemonClient,
    DaemonConfig, DaemonOp, DaemonPool, DaemonServer,
};
use mimikyu_core::input::BatchAction;
use mimikyu_core::session::SessionCreateOpts;
use mimikyu_core::snapshot::SnapshotOptions;
use mimikyu_core::MimikyuError;
use serde_json::json;
use support::{assert_pgid_gone, TestServer};

/// Coordinates tests in this binary that spawn child processes under `std::process::id()`
/// against tests that inspect `pgrep -P <self_pid>` to verify OS-level reaping of their
/// daemon's browser process groups:
/// - Tests that spawn child processes hold a shared `.read()` guard so they continue to
///   run in parallel with one another.
/// - Tests that inspect `pgrep -P <self_pid>` and assert every spawned PGID is reaped
///   hold an exclusive `.write()` guard so foreign test children cannot pollute the snapshot.
fn child_process_lock() -> &'static tokio::sync::RwLock<()> {
    static LOCK: OnceLock<tokio::sync::RwLock<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::RwLock::new(()))
}

fn direct_child_pids() -> Vec<libc::pid_t> {
    let my_pid = std::process::id().to_string();
    let out = std::process::Command::new("pgrep")
        .args(["-P", &my_pid])
        .output()
        .expect("pgrep -P");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<libc::pid_t>().ok())
        .collect()
}

#[test]
fn daemon_batch_op_normalization_and_validation() {
    let raw = json!({
        "ops": [
            { "op": "open", "url": "http://127.0.0.1:1/" },
            { "op": "fill", "target": "#name", "value": "alice" },
            { "op": "upload", "target": "#file", "path": "/tmp/a.txt" },
            { "op": "wait", "selector": "#done" },
            { "op": "wait", "url": "/next" },
            { "op": "wait", "idle": true },
            { "op": "wait", "idle": true, "stable_target": "#stable-a" },
            { "op": "wait", "stable_target": "#stable-b" },
            { "action": "wait_idle", "stable_target": "@e7" },
            { "action": "click", "target": "#go" }
        ]
    });
    let actions = parse_batch_actions(raw).expect("normalize ops wrapper");
    assert_eq!(actions.len(), 10);
    assert!(matches!(actions[0], BatchAction::Open { .. }));
    assert!(matches!(actions[1], BatchAction::Fill { .. }));
    assert!(matches!(actions[2], BatchAction::Upload { ref files, .. } if files.len() == 1));
    assert!(matches!(actions[3], BatchAction::WaitSelector { .. }));
    assert!(matches!(actions[4], BatchAction::WaitUrl { ref substr, .. } if substr == "/next"));
    assert!(matches!(actions[5], BatchAction::WaitIdle { target: None, .. }));
    assert!(matches!(actions[6], BatchAction::WaitIdle { ref target, .. } if target.as_deref() == Some("#stable-a")));
    assert!(matches!(actions[7], BatchAction::WaitIdle { ref target, .. } if target.as_deref() == Some("#stable-b")));
    assert!(matches!(actions[8], BatchAction::WaitIdle { ref target, .. } if target.as_deref() == Some("@e7")));
    assert!(matches!(actions[9], BatchAction::Click { .. }));

    let bad = json!([{ "op": "unknown_op_xyz" }]);
    let err = parse_batch_actions(bad).expect_err("unknown op must fail");
    assert!(matches!(err, MimikyuError::InvalidConfig(_)));
}

#[tokio::test]
async fn daemon_stale_socket_and_pid_recovery() {
    // Case 1: regular file at daemon.sock + stale PID
    let tmp1 = tempfile::tempdir().expect("tempdir 1");
    let state1 = tmp1.path().join("state1");
    fs::create_dir_all(&state1).unwrap();
    fs::write(state1.join("daemon.sock"), b"stale_not_a_socket").unwrap();
    fs::write(state1.join("daemon.pid"), b"99999999\n").unwrap();

    let server1 = DaemonServer::bind(DaemonConfig::new(state1.clone()))
        .await
        .expect("bind recovers from regular-file stale socket");
    let handle1 = tokio::spawn(async move { server1.run().await });

    let client1 = DaemonClient::connect(&state1).await.expect("connect");
    let ping = client1.call(DaemonOp::Ping).await.expect("ping");
    assert_eq!(ping["pong"], true);

    let status = daemon_status(&state1).await.expect("status");
    assert!(status.running);
    assert_eq!(status.pid, std::process::id());

    drop(client1);
    assert!(daemon_stop(&state1).await.expect("stop"));
    handle1.await.expect("join1").expect("run1 ok");
    assert!(!state1.join("daemon.sock").exists());
    assert!(!state1.join("daemon.pid").exists());

    // Case 2: bound-then-closed AF_UNIX socket + stale PID
    let tmp2 = tempfile::tempdir().expect("tempdir 2");
    let state2 = tmp2.path().join("state2");
    fs::create_dir_all(&state2).unwrap();
    let sock2 = state2.join("daemon.sock");
    let dead_listener = StdUnixListener::bind(&sock2).expect("bind dead sock");
    drop(dead_listener);
    fs::write(state2.join("daemon.pid"), b"99999999\n").unwrap();

    let server2 = DaemonServer::bind(DaemonConfig::new(state2.clone()))
        .await
        .expect("bind recovers from closed AF_UNIX socket");
    let handle2 = tokio::spawn(async move { server2.run().await });

    let client2 = DaemonClient::connect(&state2).await.expect("connect2");
    let ping2 = client2.call(DaemonOp::Ping).await.expect("ping2");
    assert_eq!(ping2["pong"], true);
    drop(client2);

    assert!(daemon_stop(&state2).await.expect("stop2"));
    handle2.await.expect("join2").expect("run2 ok");
}

#[tokio::test]
async fn daemon_long_state_dir_exceeding_macos_sun_path_uses_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deep = tmp
        .path()
        .join("very_long_segment_for_macos_sun_path_testing_01")
        .join("very_long_segment_for_macos_sun_path_testing_02")
        .join("very_long_segment_for_macos_sun_path_testing_03");
    fs::create_dir_all(&deep).unwrap();
    let sock_link = deep.join("daemon.sock");
    assert!(
        sock_link.as_os_str().as_encoded_bytes().len() > 110,
        "expected path > 110 bytes, got {}",
        sock_link.display()
    );

    let server = DaemonServer::bind(DaemonConfig::new(deep.clone()))
        .await
        .expect("bind with long state_dir");
    let bound_sock = server.paths().bound_sock.clone();
    assert!(
        bound_sock.as_os_str().as_encoded_bytes().len() < 100,
        "bound socket should be short: {}",
        bound_sock.display()
    );
    let meta = fs::symlink_metadata(&sock_link).expect("symlink metadata");
    assert!(meta.file_type().is_symlink());

    let handle = tokio::spawn(async move { server.run().await });

    let client = DaemonClient::connect(&deep)
        .await
        .expect("client resolves symlink and connects");
    let ping = client.call(DaemonOp::Ping).await.expect("ping");
    assert_eq!(ping["pong"], true);

    let status = daemon_status(&deep).await.expect("status");
    assert!(status.running);
    drop(client);

    assert!(daemon_stop(&deep).await.expect("stop"));
    handle.await.expect("join").expect("run ok");

    assert!(fs::symlink_metadata(&sock_link).is_err());
    assert!(!bound_sock.exists());
    assert!(!deep.join("daemon.pid").exists());
}

#[tokio::test]
async fn daemon_idle_timeout_disposes_sessions_and_cleans_files() {
    let _child_guard = child_process_lock().read().await;
    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("idle_state");

    let mut cfg = DaemonConfig::new(state.clone());
    cfg.idle_timeout = Some(Duration::from_millis(450));

    let server = DaemonServer::bind(cfg).await.expect("bind");
    let handle = tokio::spawn(async move { server.run().await });

    let client = DaemonClient::connect(&state).await.expect("connect");
    let created = client
        .call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("idle_s".to_string()),
            ephemeral: true,
            main_world_eval: true,
            ..Default::default()
        }))
        .await
        .expect("create ephemeral session");

    let pid = created["pid"].as_i64().expect("ephemeral pid") as libc::pid_t;
    assert_eq!(created["id"], "idle_s");
    assert_eq!(created["session_id"], "idle_s");

    client
        .call(DaemonOp::Open {
            session_id: "idle_s".to_string(),
            url: srv.url("/"),
        })
        .await
        .expect("open /");

    let status = daemon_status(&state).await.expect("status");
    assert_eq!(status.active_sessions, 1);

    drop(client);

    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("daemon exited on idle timeout")
        .expect("join ok")
        .expect("daemon run ok");

    assert!(!state.join("daemon.sock").exists());
    assert!(!state.join("daemon.pid").exists());
    assert_pgid_gone(pid);
}

#[tokio::test]
async fn daemon_concurrent_multi_client_requests_and_session_isolation() {
    let _child_guard = child_process_lock().read().await;
    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("conc_state");

    let server = DaemonServer::bind(DaemonConfig::new(state.clone()))
        .await
        .expect("bind");
    let handle = tokio::spawn(async move { server.run().await });

    let c1 = DaemonClient::connect(&state).await.expect("c1");
    let c2 = DaemonClient::connect(&state).await.expect("c2");

    let (r1, r2) = tokio::join!(
        c1.call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("s1".to_string()),
            virtual_ctx: true,
            main_world_eval: true,
            ..Default::default()
        })),
        c2.call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("s2".to_string()),
            virtual_ctx: true,
            main_world_eval: true,
            ..Default::default()
        })),
    );
    r1.expect("s1 created");
    r2.expect("s2 created");

    let url_cookie = srv.url("/cookie");
    let url_forms = srv.url("/forms-rich");

    let (o1, o2) = tokio::join!(
        c1.call(DaemonOp::Open {
            session_id: "s1".to_string(),
            url: url_cookie.clone(),
        }),
        c2.call(DaemonOp::Open {
            session_id: "s2".to_string(),
            url: url_forms.clone(),
        }),
    );
    o1.expect("s1 open");
    o2.expect("s2 open");

    let batch_res = c2
        .call(DaemonOp::Batch {
            session_id: "s2".to_string(),
            actions: json!([
                { "op": "fill", "target": "#prefilled", "value": "daemon-batch" },
                { "op": "select", "target": "#country", "value": "jp" }
            ]),
        })
        .await
        .expect("batch");
    assert_eq!(batch_res["ok"], true);
    assert_eq!(batch_res["completed_steps"], 2);

    let (e1, e2, snap2) = tokio::join!(
        c1.call(DaemonOp::Eval {
            session_id: "s1".to_string(),
            js: "document.cookie".to_string(),
            main_world: false,
        }),
        c2.call(DaemonOp::Eval {
            session_id: "s2".to_string(),
            js: "document.cookie".to_string(),
            main_world: false,
        }),
        c2.call(DaemonOp::Snapshot {
            session_id: "s2".to_string(),
            options: SnapshotOptions::default(),
        }),
    );

    let cookie1 = e1.expect("e1")["value"].as_str().unwrap_or("").to_string();
    let cookie2 = e2.expect("e2")["value"].as_str().unwrap_or("").to_string();
    assert!(cookie1.contains("mimikyu_test=1"), "s1 cookie: {cookie1}");
    assert!(!cookie2.contains("mimikyu_test=1"), "s2 leaked cookie: {cookie2}");
    assert!(snap2.expect("snap2")["ref_count"].as_u64().unwrap_or(0) > 0);

    let listed = c1.call(DaemonOp::SessionList).await.expect("list");
    let arr = listed.as_array().expect("list array");
    assert_eq!(arr.len(), 2);
    for item in arr {
        assert!(item.get("id").is_some());
        assert!(item.get("session_id").is_some());
    }

    c1.call(DaemonOp::SessionClose {
        session_id: "s1".to_string(),
    })
    .await
    .expect("close s1");
    c2.call(DaemonOp::SessionClose {
        session_id: "s2".to_string(),
    })
    .await
    .expect("close s2");

    let listed_after = c1.call(DaemonOp::SessionList).await.expect("list after");
    assert!(listed_after.as_array().unwrap().is_empty());

    drop(c1);
    drop(c2);
    assert!(daemon_stop(&state).await.expect("stop"));
    handle.await.expect("join").expect("run ok");
}

#[tokio::test]
async fn daemon_exits_and_reaps_virtual_session_when_state_dir_removed() {
    let _child_guard = child_process_lock().write().await;
    let baseline_pids: HashSet<libc::pid_t> = direct_child_pids().into_iter().collect();

    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("removed_state");

    let server = DaemonServer::bind(DaemonConfig::new(state.clone()))
        .await
        .expect("bind");
    let bound_sock = server.paths().bound_sock.clone();
    let handle = tokio::spawn(async move { server.run().await });

    let client = DaemonClient::connect(&state).await.expect("connect");
    client
        .call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("v1".to_string()),
            virtual_ctx: true,
            ..Default::default()
        }))
        .await
        .expect("create virtual session");
    client
        .call(DaemonOp::Open {
            session_id: "v1".to_string(),
            url: srv.url("/"),
        })
        .await
        .expect("open /");

    let eph = client
        .call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("e1".to_string()),
            ephemeral: true,
            ..Default::default()
        }))
        .await
        .expect("create ephemeral session");
    let eph_pid = eph["pid"].as_i64().expect("ephemeral pid") as libc::pid_t;

    // Record child browser processes spawned by this test before deleting state_dir
    let my_pid = std::process::id();
    let pre_pids: Vec<libc::pid_t> = direct_child_pids()
        .into_iter()
        .filter(|pid| !baseline_pids.contains(pid))
        .collect();
    assert!(
        pre_pids.contains(&eph_pid) && pre_pids.len() >= 2,
        "expected both virtual root browser and ephemeral browser under {my_pid}, got {pre_pids:?}"
    );

    drop(client);
    fs::remove_dir_all(&state).expect("delete state_dir externally");

    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("daemon must exit promptly after state_dir removal")
        .expect("join ok")
        .expect("daemon run ok");

    assert!(!bound_sock.exists(), "bound_sock must be cleaned up");
    for child_pid in pre_pids {
        assert_pgid_gone(child_pid);
    }
}

#[tokio::test]
async fn daemon_exits_and_reaps_virtual_session_when_sock_link_removed_on_long_state_dir() {
    let _child_guard = child_process_lock().write().await;
    let baseline_pids: HashSet<libc::pid_t> = direct_child_pids().into_iter().collect();

    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let deep = tmp
        .path()
        .join("long_segment_for_sock_link_removal_test_01")
        .join("long_segment_for_sock_link_removal_test_02")
        .join("long_segment_for_sock_link_removal_test_03");
    fs::create_dir_all(&deep).unwrap();
    let sock_link = deep.join("daemon.sock");
    assert!(
        sock_link.as_os_str().as_encoded_bytes().len() > 110,
        "expected path > 110 bytes, got {}",
        sock_link.display()
    );

    let server = DaemonServer::bind(DaemonConfig::new(deep.clone()))
        .await
        .expect("bind");
    let bound_sock = server.paths().bound_sock.clone();
    assert_ne!(bound_sock, sock_link);
    assert!(fs::symlink_metadata(&sock_link).unwrap().file_type().is_symlink());
    let handle = tokio::spawn(async move { server.run().await });

    let client = DaemonClient::connect(&deep).await.expect("connect");
    client
        .call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("v_link".to_string()),
            virtual_ctx: true,
            ..Default::default()
        }))
        .await
        .expect("create virtual session");
    client
        .call(DaemonOp::Open {
            session_id: "v_link".to_string(),
            url: srv.url("/"),
        })
        .await
        .expect("open /");

    let my_pid = std::process::id();
    let pre_pids: Vec<libc::pid_t> = direct_child_pids()
        .into_iter()
        .filter(|pid| !baseline_pids.contains(pid))
        .collect();
    assert!(
        !pre_pids.is_empty(),
        "expected virtual browser child under {my_pid}"
    );

    drop(client);
    // Remove ONLY the symlink, leaving deep state_dir and bound_sock intact
    fs::remove_file(&sock_link).expect("unlink sock_link");
    assert!(deep.exists());
    assert!(bound_sock.exists());

    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("daemon must exit promptly after sock_link removal")
        .expect("join ok")
        .expect("daemon run ok");

    assert!(!bound_sock.exists(), "bound_sock must be cleaned up");
    assert!(!deep.join("daemon.pid").exists(), "pid_file must be cleaned up");
    for child_pid in pre_pids {
        assert_pgid_gone(child_pid);
    }
}

#[tokio::test]
async fn daemon_status_and_stop_do_not_create_missing_state_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does_not_exist_yet");
    assert!(!missing.exists());

    let st = daemon_status(&missing).await.expect("status");
    assert!(!st.running);
    assert!(!missing.exists(), "daemon_status must not create state_dir");

    let stopped = daemon_stop(&missing).await.expect("stop");
    assert!(!stopped);
    assert!(!missing.exists(), "daemon_stop must not create state_dir");
}

#[tokio::test]
async fn daemon_rejects_oversized_frame_without_oom_and_continues() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("oversize_daemon");
    let server = DaemonServer::bind(DaemonConfig::new(state.clone()))
        .await
        .expect("bind");
    let sock = server.paths().bound_sock.clone();
    let handle = tokio::spawn(async move { server.run().await });

    let stream = UnixStream::connect(&sock).await.expect("connect");
    let (rh, mut wh) = stream.into_split();
    let mut reader = BufReader::new(rh);

    // Send >16 MiB single line
    let chunk = vec![b'a'; 1024 * 1024];
    for _ in 0..17 {
        wh.write_all(&chunk).await.unwrap();
    }
    wh.write_all(b"\n").await.unwrap();
    wh.flush().await.unwrap();

    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await.unwrap();
    let v1: serde_json::Value = serde_json::from_str(resp_line.trim()).unwrap();
    assert_eq!(v1["ok"], false);
    assert!(
        v1["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("exceeded"),
        "expected oversized error: {v1}"
    );

    // Next ping on same connection must succeed
    wh.write_all(b"{\"id\":2,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    wh.flush().await.unwrap();
    resp_line.clear();
    reader.read_line(&mut resp_line).await.unwrap();
    let v2: serde_json::Value = serde_json::from_str(resp_line.trim()).unwrap();
    assert_eq!(v2["ok"], true);
    assert_eq!(v2["id"], 2);

    drop(wh);
    drop(reader);
    daemon_stop(&state).await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn daemon_client_does_not_retry_post_send_disconnect() {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("no_retry");
    fs::create_dir_all(&state).unwrap();
    let sock_path = state.join("daemon.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();

    let seen_ops = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_clone = Arc::clone(&seen_ops);

    let mock_srv = tokio::spawn(async move {
        for _ in 0..3 {
            let Ok(Ok((stream, _))) =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await
            else {
                break;
            };
            let (rh, mut wh) = stream.into_split();
            let mut reader = BufReader::new(rh);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                continue;
            }
            let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let op_name = req["method"].as_str().unwrap_or("").to_string();
            seen_clone.lock().unwrap().push(op_name.clone());
            if op_name == "ping" {
                let id = req["id"].as_u64().unwrap_or(0);
                let _ = wh
                    .write_all(format!("{{\"id\":{id},\"ok\":true,\"data\":{{\"pong\":true}}}}\n").as_bytes())
                    .await;
                let _ = wh.flush().await;
            } else {
                // Close immediately after receiving non-idempotent RPC without replying
                drop(wh);
                drop(reader);
            }
        }
    });

    let client = DaemonClient::connect(&state).await.expect("connect mock");
    let err = client
        .call(DaemonOp::Click {
            session_id: "default".into(),
            target: "#btn".into(),
            humanize: None,
        })
        .await
        .expect_err("post-send EOF must return error without retrying");
    assert!(matches!(err, MimikyuError::Transport(_)));

    let _ = mock_srv.await;
    let ops = seen_ops.lock().unwrap().clone();
    assert_eq!(
        ops,
        vec!["click".to_string()],
        "DaemonClient::call must NOT retry post-send RPC: {ops:?}"
    );
}

#[tokio::test]
async fn daemon_concurrent_bind_on_same_state_dir_allows_only_one_server() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("conc_bind");

    let s1_fut = DaemonServer::bind(DaemonConfig::new(state.clone()));
    let s2_fut = DaemonServer::bind(DaemonConfig::new(state.clone()));
    let (r1, r2) = tokio::join!(s1_fut, s2_fut);

    let winner = match (r1, r2) {
        (Ok(s), Err(_)) | (Err(_), Ok(s)) => s,
        (Ok(_), Ok(_)) => panic!("two DaemonServer::bind calls succeeded on same state_dir"),
        (Err(e1), Err(e2)) => panic!("both binds failed: {e1}; {e2}"),
    };
    let handle = tokio::spawn(async move { winner.run().await });

    let st = daemon_status(&state).await.expect("status");
    assert!(st.running);
    daemon_stop(&state).await.expect("stop");
    handle.await.unwrap().unwrap();
}

#[test]
fn daemon_pool_large_idle_timeout_does_not_truncate_to_immediate_expiry() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("large_idle_state");
    let mgr = std::sync::Arc::new(
        mimikyu_core::session::SessionManager::with_config(
            mimikyu_core::session::SessionManagerConfig {
                state_dir: state.clone(),
                ..Default::default()
            },
        )
        .expect("session manager"),
    );
    let paths = resolve_socket_paths(&state).expect("socket paths");

    for secs in [18_446_744_073_709_552u64, u64::MAX] {
        let pool = DaemonPool::new(mgr.clone(), paths.clone(), Some(Duration::from_secs(secs)));
        assert!(
            !pool.is_idle_expired(),
            "idle_timeout={secs}s must not truncate u128 millis and immediately expire"
        );
    }
}

#[tokio::test]
async fn daemon_client_preserves_timeout_ms_without_double_prefix() {
    let _child_guard = child_process_lock().read().await;
    let srv = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("timeout_state");

    let server = DaemonServer::bind(DaemonConfig::new(state.clone()))
        .await
        .expect("bind");
    let handle = tokio::spawn(async move { server.run().await });

    let client = DaemonClient::connect(&state).await.expect("connect");
    client
        .call(DaemonOp::SessionNew(SessionCreateOpts {
            session_id: Some("t1".into()),
            virtual_ctx: true,
            ..Default::default()
        }))
        .await
        .expect("session new");
    client
        .call(DaemonOp::Open {
            session_id: "t1".into(),
            url: srv.url("/"),
        })
        .await
        .expect("open /");

    let err = client
        .call(DaemonOp::Wait {
            session_id: "t1".into(),
            request: mimikyu_core::session::WaitRequest {
                text: Some("NEVER_THERE".into()),
                timeout_ms: Some(150),
                ..Default::default()
            },
        })
        .await
        .expect_err("wait_text must time out");

    match &err {
        MimikyuError::Timeout {
            timeout_ms,
            operation,
        } => {
            assert_eq!(*timeout_ms, 150);
            assert!(operation.contains("NEVER_THERE"), "op={operation}");
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.starts_with("operation timed out after 150ms:"),
        "unexpected formatted timeout: {msg}"
    );
    assert!(
        !msg.contains("after 0ms:"),
        "double-prefixed timeout message: {msg}"
    );

    client
        .call(DaemonOp::SessionClose {
            session_id: "t1".into(),
        })
        .await
        .expect("close");
    drop(client);
    daemon_stop(&state).await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn daemon_bind_waits_cleanly_after_active_lock_released_on_shutdown() {
    use fs2::FileExt;
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("slow_shutdown_state");
    fs::create_dir_all(&state).unwrap();

    // Simulate a daemon mid-shutdown (listener dropped, daemon.sock/pid still on disk,
    // daemon-active.lock held for 600ms while browser sessions close).
    let active_lock_path = state.join("daemon-active.lock");
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&active_lock_path)
        .unwrap();
    lock_file.lock_exclusive().unwrap();
    let dead_sock = StdUnixListener::bind(state.join("daemon.sock")).unwrap();
    drop(dead_sock);
    fs::write(state.join("daemon.pid"), format!("{}\n", std::process::id())).unwrap();

    // Spawn a thread that releases daemon-active.lock after 600ms (> old 250ms timeout).
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        let _ = FileExt::unlock(&lock_file);
        drop(lock_file);
    });

    // Polling until active_lock clears allows DaemonServer::bind to succeed cleanly.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let server = loop {
        match DaemonServer::bind(DaemonConfig::new(state.clone())).await {
            Ok(srv) => break srv,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => panic!("bind failed after active lock released: {e}"),
        }
    };
    releaser.join().unwrap();

    let handle = tokio::spawn(async move { server.run().await });
    let st = daemon_status(&state).await.expect("status");
    assert!(st.running);
    daemon_stop(&state).await.expect("stop");
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn daemon_ping_rejects_when_shutting_down_and_idle_expiry_is_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("ping_shutdown_state");

    let mut cfg = DaemonConfig::new(state.clone());
    cfg.idle_timeout = Some(Duration::from_millis(80));

    let server = DaemonServer::bind(cfg).await.expect("bind");
    let pool = std::sync::Arc::clone(server.pool());

    // While an active request guard is held, try_expire_idle must NOT expire even after idle_timeout.
    {
        let _active = pool.try_begin_request().expect("request allowed before shutdown");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !pool.try_expire_idle(),
            "try_expire_idle must not expire while in_flight > 0"
        );
        assert!(!pool.is_shutting_down());
    }

    // After request finishes and timeout elapses again, try_expire_idle atomically sets shutting_down.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(pool.try_expire_idle(), "try_expire_idle must expire when idle");
    assert!(pool.is_shutting_down());
    assert!(
        pool.try_begin_request().is_none(),
        "new requests must be rejected once try_expire_idle sets shutting_down"
    );

    // On a live server, requesting shutdown causes Ping and SessionList on already-accepted
    // connections to return Disposed rather than {"pong": true} or mid-frame EOF.
    let state2 = tmp.path().join("ping_shutdown_live");
    let server2 = DaemonServer::bind(DaemonConfig::new(state2.clone()))
        .await
        .expect("bind2");
    let pool2 = std::sync::Arc::clone(server2.pool());
    let handle2 = tokio::spawn(async move { server2.run().await });

    let c_ping = DaemonClient::connect(&state2).await.expect("connect ping");
    let c_list = DaemonClient::connect(&state2).await.expect("connect list");
    assert_eq!(c_ping.call(DaemonOp::Ping).await.expect("live ping")["pong"], true);

    pool2.request_shutdown();

    let err_ping = c_ping
        .call(DaemonOp::Ping)
        .await
        .expect_err("Ping during shutdown must return Disposed");
    assert!(
        matches!(err_ping, MimikyuError::Disposed),
        "expected Disposed on Ping during shutdown, got {err_ping:?}"
    );

    let err_list = c_list
        .call(DaemonOp::SessionList)
        .await
        .expect_err("SessionList on accepted connection during shutdown must return Disposed");
    assert!(
        matches!(err_list, MimikyuError::Disposed),
        "expected Disposed on accepted connection during shutdown, got {err_list:?}"
    );

    drop(c_ping);
    drop(c_list);
    handle2.await.unwrap().unwrap();
}

#[tokio::test]
async fn daemon_stop_escalates_to_sigkill_on_wedged_sigterm_ignoring_daemon_and_cleans_up() {
    let _child_guard = child_process_lock().read().await;
    use mimikyu_core::daemon::is_pid_alive;
    use std::process::Command;

    struct DetachedPidKillGuard(Option<i32>);
    impl Drop for DetachedPidKillGuard {
        fn drop(&mut self) {
            if let Some(pid) = self.0.take() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("wedged_sigterm_ignore_state");
    fs::create_dir_all(&state).unwrap();

    // Spawn a detached grandchild (reparented to PID 1, matching real background daemon)
    // that ignores SIGTERM and holds daemon-active.lock, simulating a wedged daemon.
    let active_lock_path = state.join("daemon-active.lock");
    let lock_str = active_lock_path.to_str().unwrap().to_string();
    let py_script = format!(
        "import fcntl, os, signal, time, sys\n\
         r, w = os.pipe()\n\
         pid = os.fork()\n\
         if pid > 0:\n\
         \x20   os.close(w)\n\
         \x20   os.read(r, 1)\n\
         \x20   print(pid)\n\
         \x20   sys.exit(0)\n\
         os.close(r)\n\
         os.setsid()\n\
         dn = os.open(os.devnull, os.O_RDWR)\n\
         os.dup2(dn, 0)\n\
         os.dup2(dn, 1)\n\
         os.dup2(dn, 2)\n\
         if dn > 2:\n\
         \x20   os.close(dn)\n\
         signal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
         f = open({:?}, 'a+')\n\
         fcntl.flock(f.fileno(), fcntl.LOCK_EX)\n\
         os.write(w, b'1')\n\
         os.close(w)\n\
         time.sleep(60)\n",
        lock_str
    );
    let out = Command::new("python3")
        .arg("-c")
        .arg(py_script)
        .output()
        .expect("spawn detached python flock+sigterm-ign daemon sim");
    assert!(
        out.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid: i32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("parse grandchild pid");
    let mut guard = DetachedPidKillGuard(Some(pid));

    let dead_sock = StdUnixListener::bind(state.join("daemon.sock")).unwrap();
    drop(dead_sock);
    fs::write(state.join("daemon.pid"), format!("{pid}\n")).unwrap();

    let stopped = daemon_stop(&state)
        .await
        .expect("daemon_stop must escalate to SIGKILL and succeed");
    assert!(stopped, "daemon_stop must report stopped=true once SIGKILL kills pid");
    guard.0 = None;

    assert!(!is_pid_alive(pid), "pid {pid} must not remain alive");
    assert!(!state.join("daemon.sock").exists());
    assert!(!state.join("daemon.pid").exists());
}

#[tokio::test]
async fn daemon_supports_hyphen_prefixed_state_dir_name() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("-dash-state-dir");

    let cfg = DaemonConfig::new(state.clone());
    let server = DaemonServer::bind(cfg).await.expect("bind hyphen state dir");
    let handle = tokio::spawn(async move { server.run().await });

    let client = DaemonClient::connect(&state)
        .await
        .expect("connect hyphen state dir");
    let pong = client.call(DaemonOp::Ping).await.expect("ping");
    assert_eq!(pong["pong"], true);

    let st = daemon_status(&state).await.expect("status");
    assert!(st.running);

    drop(client);
    assert!(daemon_stop(&state).await.expect("stop"));
    handle.await.unwrap().unwrap();
}
