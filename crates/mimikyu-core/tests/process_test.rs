use std::time::Duration;

use mimikyu_core::driver::{driver_for, EngineKind, LaunchOptions, ProfileMode};
use mimikyu_core::process::ProcessGuard;
use mimikyu_core::MimikyuError;

fn assert_pgid_gone(pgid: libc::pid_t) {
    let rc = unsafe { libc::kill(-pgid, 0) };
    let err = std::io::Error::last_os_error();
    assert_eq!(rc, -1, "expected kill(-pgid, 0) to fail after drop");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::ESRCH),
        "expected ESRCH after drop, got {err}"
    );
}

#[tokio::test]
async fn process_guard_drop_cleans_chromium_group_and_tempdir() {
    let opts = LaunchOptions {
        engine: EngineKind::Chromium,
        headless: true,
        profile_mode: ProfileMode::EphemeralDir,
        ..Default::default()
    };
    let guard = ProcessGuard::launch(&opts)
        .await
        .expect("launch chromium");
    let pgid = guard.pgid();
    let profile_dir = guard.profile_dir().to_path_buf();
    assert!(profile_dir.exists(), "profile_dir should exist while alive");
    assert!(guard.ws_endpoint().starts_with("ws://"));

    // Intentionally drop WITHOUT calling shutdown() to exercise synchronous RAII Drop safety net.
    drop(guard);

    assert_pgid_gone(pgid);
    assert!(
        !profile_dir.exists(),
        "ephemeral profile_dir {:?} should be deleted on drop",
        profile_dir
    );
}

#[tokio::test]
async fn process_guard_drop_cleans_firefox_group_and_tempdir() {
    let opts = LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        profile_mode: ProfileMode::EphemeralDir,
        ..Default::default()
    };
    let guard = ProcessGuard::launch(&opts)
        .await
        .expect("launch firefox");
    let pgid = guard.pgid();
    let profile_dir = guard.profile_dir().to_path_buf();
    assert!(profile_dir.exists(), "profile_dir should exist while alive");
    assert!(profile_dir.join("user.js").exists(), "user.js should be written");
    assert!(guard.ws_endpoint().starts_with("ws://"));

    drop(guard);

    assert_pgid_gone(pgid);
    assert!(
        !profile_dir.exists(),
        "ephemeral profile_dir {:?} should be deleted on drop",
        profile_dir
    );
}

async fn assert_driver_session_drop_without_dispose(engine: EngineKind) {
    let driver = driver_for(engine);
    let opts = LaunchOptions {
        engine,
        headless: true,
        profile_mode: ProfileMode::EphemeralDir,
        ..Default::default()
    };

    // 1. Normal drop without dispose()
    let session = driver.launch(opts.clone()).await.expect("launch session");
    let pid = session.info().pid.expect("pid present");
    let profile_dir = session
        .info()
        .profile_dir
        .clone()
        .expect("profile_dir present");
    assert!(profile_dir.exists());
    drop(session);
    assert_pgid_gone(pid as libc::pid_t);
    assert!(
        !profile_dir.exists(),
        "ephemeral profile_dir {:?} leaked on normal drop",
        profile_dir
    );

    // 2. Panic inside tokio::spawn without dispose()
    let session = driver.launch(opts.clone()).await.expect("launch for panic");
    let pid = session.info().pid.expect("pid present");
    let profile_dir = session
        .info()
        .profile_dir
        .clone()
        .expect("profile_dir present");
    let join_res = tokio::spawn(async move {
        let _held = session;
        panic!("simulated task panic");
    })
    .await;
    assert!(join_res.is_err(), "spawned task should have panicked");
    assert_pgid_gone(pid as libc::pid_t);
    assert!(
        !profile_dir.exists(),
        "ephemeral profile_dir {:?} leaked on panic",
        profile_dir
    );

    // 3. tokio::select! cancellation without dispose()
    let session = driver.launch(opts).await.expect("launch for select");
    let pid = session.info().pid.expect("pid present");
    let profile_dir = session
        .info()
        .profile_dir
        .clone()
        .expect("profile_dir present");
    tokio::select! {
        _ = async move {
            let _held = session;
            tokio::time::sleep(Duration::from_secs(60)).await;
        } => {
            unreachable!("sleep branch should be cancelled");
        }
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    assert_pgid_gone(pid as libc::pid_t);
    assert!(
        !profile_dir.exists(),
        "ephemeral profile_dir {:?} leaked on select cancellation",
        profile_dir
    );
}

#[tokio::test]
async fn driver_session_drop_without_dispose_cleans_chromium() {
    assert_driver_session_drop_without_dispose(EngineKind::Chromium).await;
}

#[tokio::test]
async fn driver_session_drop_without_dispose_cleans_firefox() {
    assert_driver_session_drop_without_dispose(EngineKind::Firefox).await;
}

#[tokio::test]
async fn process_launch_fails_fast_on_immediate_exit() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let script_path = dir.path().join("fail-fast-browser");
    std::fs::write(&script_path, "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"Fake 1.0\"; exit 0; fi\necho \"boom\" >&2\nexit 1\n").expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let start = std::time::Instant::now();
    let err = ProcessGuard::launch(&LaunchOptions {
        engine: EngineKind::Chromium,
        binary_path: Some(script_path),
        launch_timeout_ms: 15_000,
        ..Default::default()
    })
    .await
    .err()
    .expect("expected LaunchFailed");
    let elapsed = start.elapsed();

    assert!(
        matches!(err, MimikyuError::LaunchFailed(_)),
        "expected LaunchFailed, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "immediate exit should fail fast, took {elapsed:?}"
    );
}

async fn assert_persistent_profile_relaunch(engine: EngineKind) {
    let driver = driver_for(engine);
    let root = tempfile::tempdir().expect("tempdir");

    // 1. Relaunch into same persistent profile dir after explicit dispose()
    let path_dispose = root.path().join("persist-dispose");
    std::fs::create_dir_all(&path_dispose).expect("create persist-dispose dir");
    std::fs::write(path_dispose.join("user_sentinel.txt"), b"persist-dispose")
        .expect("write sentinel");

    let opts_dispose = LaunchOptions {
        engine,
        headless: true,
        profile_mode: ProfileMode::Persistent(path_dispose.clone()),
        ..Default::default()
    };

    let s1 = driver
        .launch(opts_dispose.clone())
        .await
        .expect("launch s1 persistent (dispose)");
    let pid1 = s1.info().pid.expect("pid1");
    assert_eq!(
        s1.eval_isolated("1 + 1").await.expect("s1 eval"),
        serde_json::json!(2)
    );
    s1.dispose().await.expect("dispose s1");
    assert_pgid_gone(pid1 as libc::pid_t);
    assert!(path_dispose.exists(), "persistent dir must remain after dispose");
    assert_eq!(
        std::fs::read(path_dispose.join("user_sentinel.txt")).expect("read sentinel"),
        b"persist-dispose"
    );

    let s2 = driver
        .launch(opts_dispose)
        .await
        .expect("relaunch s2 into persistent dir after dispose");
    let pid2 = s2.info().pid.expect("pid2");
    assert_eq!(
        s2.eval_isolated("2 + 2").await.expect("s2 eval"),
        serde_json::json!(4)
    );
    s2.dispose().await.expect("dispose s2");
    assert_pgid_gone(pid2 as libc::pid_t);
    assert_eq!(
        std::fs::read(path_dispose.join("user_sentinel.txt")).expect("read sentinel after s2"),
        b"persist-dispose"
    );

    // 2. Relaunch into same persistent profile dir after RAII drop() (without dispose)
    let path_drop = root.path().join("persist-drop");
    std::fs::create_dir_all(&path_drop).expect("create persist-drop dir");
    std::fs::write(path_drop.join("user_sentinel.txt"), b"persist-drop")
        .expect("write sentinel drop");

    let opts_drop = LaunchOptions {
        engine,
        headless: true,
        profile_mode: ProfileMode::Persistent(path_drop.clone()),
        ..Default::default()
    };

    let d1 = driver
        .launch(opts_drop.clone())
        .await
        .expect("launch d1 persistent (drop)");
    let dpid1 = d1.info().pid.expect("dpid1");
    assert_eq!(
        d1.eval_isolated("3 + 3").await.expect("d1 eval"),
        serde_json::json!(6)
    );
    drop(d1);
    assert_pgid_gone(dpid1 as libc::pid_t);
    assert!(path_drop.exists(), "persistent dir must remain after drop");
    assert_eq!(
        std::fs::read(path_drop.join("user_sentinel.txt")).expect("read sentinel drop"),
        b"persist-drop"
    );

    let d2 = driver
        .launch(opts_drop)
        .await
        .expect("relaunch d2 into persistent dir after drop");
    let dpid2 = d2.info().pid.expect("dpid2");
    assert_eq!(
        d2.eval_isolated("4 + 4").await.expect("d2 eval"),
        serde_json::json!(8)
    );
    drop(d2);
    assert_pgid_gone(dpid2 as libc::pid_t);
    assert_eq!(
        std::fs::read(path_drop.join("user_sentinel.txt")).expect("read sentinel after d2"),
        b"persist-drop"
    );
}

#[tokio::test]
async fn persistent_profile_relaunch_after_dispose_and_drop_chromium() {
    assert_persistent_profile_relaunch(EngineKind::Chromium).await;
}

#[tokio::test]
async fn persistent_profile_relaunch_after_dispose_and_drop_firefox() {
    assert_persistent_profile_relaunch(EngineKind::Firefox).await;
}

#[tokio::test]
async fn process_guard_shutdown_reaps_chromium_and_firefox() {
    for engine in [EngineKind::Chromium, EngineKind::Firefox] {
        let opts = LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        };
        let mut guard = ProcessGuard::launch(&opts)
            .await
            .unwrap_or_else(|e| panic!("launch {engine}: {e}"));
        let pgid = guard.pgid();
        let profile_dir = guard.profile_dir().to_path_buf();
        assert!(profile_dir.exists());

        guard
            .shutdown(Duration::from_millis(1500))
            .await
            .unwrap_or_else(|e| panic!("shutdown {engine}: {e}"));
        assert_pgid_gone(pgid);
        assert!(
            !profile_dir.exists(),
            "ephemeral profile_dir {:?} should be deleted after shutdown",
            profile_dir
        );
    }
}

#[tokio::test]
async fn firefox_launch_sets_crashreporter_env_and_strips_inherited_proxy_env() {
    let exe = std::env::current_exe().expect("current_exe");
    let out = std::process::Command::new(exe)
        .arg("--exact")
        .arg("firefox_launch_sets_crashreporter_env_and_strips_inherited_proxy_env_child")
        .env("MIMIKYU_TEST_PROXY_STRIP_CHILD", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("ALL_PROXY", "socks5://127.0.0.1:9")
        .env("all_proxy", "socks5://127.0.0.1:9")
        .output()
        .expect("spawn child test");
    assert!(
        out.status.success(),
        "child env-stripping verification failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn firefox_launch_sets_crashreporter_env_and_strips_inherited_proxy_env_child() {
    use std::os::unix::fs::PermissionsExt;

    if std::env::var_os("MIMIKYU_TEST_PROXY_STRIP_CHILD").is_none() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("mock-firefox");
    let ini = dir.path().join("application.ini");
    let env_dump = dir.path().join("child_env.txt");

    std::fs::write(&ini, "[App]\nVendor=Mozilla\nName=Firefox\nVersion=144.0.2\n").unwrap();
    let script = format!(
        r#"#!/bin/sh
cat <<EOF > "{dump}"
MOZ_CRASHREPORTER_DISABLE=${{MOZ_CRASHREPORTER_DISABLE:-UNSET}}
MOZ_CRASHREPORTER=${{MOZ_CRASHREPORTER:-UNSET}}
MOZ_CRASHREPORTER_NO_REPORT=${{MOZ_CRASHREPORTER_NO_REPORT:-UNSET}}
HTTP_PROXY=${{HTTP_PROXY:-UNSET}}
HTTPS_PROXY=${{HTTPS_PROXY:-UNSET}}
http_proxy=${{http_proxy:-UNSET}}
https_proxy=${{https_proxy:-UNSET}}
ALL_PROXY=${{ALL_PROXY:-UNSET}}
all_proxy=${{all_proxy:-UNSET}}
EOF
echo "WebDriver BiDi listening on ws://127.0.0.1:44444" >&2
sleep 10
"#,
        dump = env_dump.display()
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();

    let mut guard = ProcessGuard::launch(&LaunchOptions {
        engine: EngineKind::Firefox,
        binary_path: Some(bin),
        proxy: None,
        ..Default::default()
    })
    .await
    .expect("launch mock firefox");

    let pgid = guard.pgid();
    let dump = std::fs::read_to_string(&env_dump).expect("read child_env.txt");
    guard
        .shutdown(Duration::from_millis(1500))
        .await
        .expect("shutdown mock firefox");
    assert_pgid_gone(pgid);

    assert!(dump.contains("MOZ_CRASHREPORTER_DISABLE=1\n"), "dump={dump}");
    assert!(dump.contains("MOZ_CRASHREPORTER=0\n"), "dump={dump}");
    assert!(dump.contains("MOZ_CRASHREPORTER_NO_REPORT=1\n"), "dump={dump}");
    for var in [
        "HTTP_PROXY=UNSET",
        "HTTPS_PROXY=UNSET",
        "http_proxy=UNSET",
        "https_proxy=UNSET",
        "ALL_PROXY=UNSET",
        "all_proxy=UNSET",
    ] {
        assert!(dump.contains(var), "expected {var} in dump:\n{dump}");
    }
}
