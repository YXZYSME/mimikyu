mod support;

use std::fs;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::time::Duration;

use mimikyu_core::daemon::{
    daemon_status, daemon_stop, parse_batch_actions, DaemonClient, DaemonConfig, DaemonOp,
    DaemonServer,
};
use mimikyu_core::input::BatchAction;
use mimikyu_core::session::SessionCreateOpts;
use mimikyu_core::snapshot::SnapshotOptions;
use mimikyu_core::MimikyuError;
use serde_json::json;
use support::{assert_pgid_gone, TestServer};

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
            { "action": "click", "target": "#go" }
        ]
    });
    let actions = parse_batch_actions(raw).expect("normalize ops wrapper");
    assert_eq!(actions.len(), 7);
    assert!(matches!(actions[0], BatchAction::Open { .. }));
    assert!(matches!(actions[1], BatchAction::Fill { .. }));
    assert!(matches!(actions[2], BatchAction::Upload { ref files, .. } if files.len() == 1));
    assert!(matches!(actions[3], BatchAction::WaitSelector { .. }));
    assert!(matches!(actions[4], BatchAction::WaitUrl { ref substr, .. } if substr == "/next"));
    assert!(matches!(actions[5], BatchAction::WaitIdle { .. }));
    assert!(matches!(actions[6], BatchAction::Click { .. }));

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
