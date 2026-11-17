use mimikyu_core::driver::{EngineKind, LaunchOptions, ProfileMode};
use mimikyu_core::process::ProcessGuard;

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
