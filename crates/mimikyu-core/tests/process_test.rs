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
