mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mimikyu_core::driver::EngineKind;
use mimikyu_core::env_profile::PresetKind;
use mimikyu_core::input::BatchAction;
use mimikyu_core::session::{
    ExtractMode, LogKind, SessionCreateOpts, SessionManager, WaitRequest,
};
use mimikyu_core::snapshot::SnapshotOptions;
use mimikyu_core::MimikyuError;
use support::{assert_pgid_gone, LocalHttpProxy, TestServer};

fn find_ref_by_name_substr(
    snap: &mimikyu_core::snapshot::SnapshotOutput,
    substr: &str,
) -> String {
    let needle = substr.to_ascii_lowercase();
    for (k, entry) in snap.ref_map.iter() {
        if entry
            .name
            .as_deref()
            .is_some_and(|n| n.to_ascii_lowercase().contains(&needle))
        {
            return k.clone();
        }
    }
    panic!(
        "could not find ref matching {:?} in snapshot:\n{}",
        substr, snap.text
    );
}

#[tokio::test]
async fn session_manager_validation_and_boundary_rejections() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // 1. state_dir as regular file must fail
    let file_state = tmp.path().join("state_file");
    std::fs::write(&file_state, b"not-a-dir").unwrap();
    assert!(matches!(
        SessionManager::new(file_state).err(),
        Some(MimikyuError::InvalidConfig(_))
    ));

    let mgr = SessionManager::new(tmp.path().join("state")).expect("create SessionManager");

    // 2. Invalid session IDs
    for bad_id in [
        "",
        ".",
        "..",
        "-lead",
        "../escape",
        "bad/id\nchars",
        "bad\\id",
        "bad\0id",
        &"a".repeat(129),
    ] {
        let err = mgr
            .create_session(SessionCreateOpts {
                session_id: Some(bad_id.to_string()),
                virtual_ctx: true,
                ..Default::default()
            })
            .await
            .expect_err("invalid session_id must fail");
        assert!(
            matches!(err, MimikyuError::InvalidConfig(_)),
            "expected InvalidConfig for {bad_id:?}, got {err:?}"
        );
    }

    // 3. Mode conflicts
    for opts in [
        SessionCreateOpts {
            session_id: Some("c1".into()),
            ephemeral: true,
            virtual_ctx: true,
            ..Default::default()
        },
        SessionCreateOpts {
            session_id: Some("c2".into()),
            profile: Some("prof".into()),
            ephemeral: true,
            ..Default::default()
        },
        SessionCreateOpts {
            session_id: Some("c3".into()),
            profile: Some("prof".into()),
            virtual_ctx: true,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            mgr.create_session(opts).await.err(),
            Some(MimikyuError::InvalidConfig(_))
        ));
    }

    // 4. Invalid viewport bounds
    for vp in [(0, 720), (1280, 0), (999_999, 999_999)] {
        assert!(matches!(
            mgr.create_session(SessionCreateOpts {
                session_id: Some("bad_vp".into()),
                virtual_ctx: true,
                viewport: Some(vp),
                ..Default::default()
            })
            .await
            .err(),
            Some(MimikyuError::InvalidConfig(_))
        ));
    }

    // 5. Invalid proxy URLs
    for bad_proxy in ["not-a-valid-proxy", "ftp://127.0.0.1:9999", "http://127.0.0.1:0"] {
        assert!(matches!(
            mgr.create_session(SessionCreateOpts {
                session_id: Some("bad_proxy".into()),
                virtual_ctx: true,
                proxy: Some(bad_proxy.to_string()),
                ..Default::default()
            })
            .await
            .err(),
            Some(MimikyuError::InvalidConfig(_))
        ));
    }

    // 6. Invalid timezone / locale / geoip
    assert!(matches!(
        mgr.create_session(SessionCreateOpts {
            session_id: Some("bad_tz".into()),
            virtual_ctx: true,
            timezone: Some("Mars/Olympus_Mons".into()),
            ..Default::default()
        })
        .await
        .err(),
        Some(MimikyuError::InvalidConfig(_))
    ));
    assert!(matches!(
        mgr.create_session(SessionCreateOpts {
            session_id: Some("bad_loc".into()),
            virtual_ctx: true,
            locale: Some("123-invalid-toolongsubtag99".into()),
            ..Default::default()
        })
        .await
        .err(),
        Some(MimikyuError::InvalidConfig(_))
    ));
    assert!(matches!(
        mgr.create_session(SessionCreateOpts {
            session_id: Some("bad_geo".into()),
            virtual_ctx: true,
            geoip: Some("999.999.999.999".into()),
            ..Default::default()
        })
        .await
        .err(),
        Some(MimikyuError::InvalidConfig(_))
    ));

    // 7. Regular file as profile path rejected
    let reg_prof = tmp.path().join("reg-profile-file");
    std::fs::write(&reg_prof, b"file").unwrap();
    assert!(matches!(
        mgr.create_session(SessionCreateOpts {
            session_id: Some("bad_prof".into()),
            profile: Some(reg_prof.to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .err(),
        Some(MimikyuError::InvalidConfig(_))
    ));

    // 8. Operations on nonexistent session return NotFound
    assert!(matches!(
        mgr.open("missing", "http://127.0.0.1:1/").await.err(),
        Some(MimikyuError::NotFound(_))
    ));
    assert!(matches!(
        mgr.close_session("missing").await.err(),
        Some(MimikyuError::NotFound(_))
    ));
    assert!(matches!(
        mgr.eval("missing", "1+1", false).await.err(),
        Some(MimikyuError::NotFound(_))
    ));
    assert!(matches!(
        mgr.env_report("missing").await.err(),
        Some(MimikyuError::NotFound(_))
    ));
}

async fn assert_ephemeral_and_persistent_lifecycle(engine: EngineKind) {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let mgr = SessionManager::new(state_dir.clone()).expect("SessionManager");

    // 1. Ephemeral session: has Some(pid), duplicate ID rejected, PGID reaped on close
    let ephem = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("ephem1".into()),
            engine: Some(engine),
            ephemeral: true,
            ..Default::default()
        })
        .await
        .expect("create ephemeral");
    assert_eq!(ephem.mode, "ephemeral");
    let ephem_pid = ephem.pid.expect("ephemeral session must expose pid");
    assert_eq!(mgr.list_sessions().len(), 1);

    let dup_err = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("ephem1".into()),
            engine: Some(engine),
            ephemeral: true,
            ..Default::default()
        })
        .await
        .expect_err("duplicate session_id must fail");
    assert!(matches!(dup_err, MimikyuError::InvalidConfig(_)));

    mgr.close_session("ephem1").await.expect("close ephemeral");
    assert_pgid_gone(ephem_pid as libc::pid_t);
    assert!(mgr.list_sessions().is_empty());

    // 2. Persistent named profile: flock contention, cookie/storage persistence, profile_list/delete
    let p1 = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("p1".into()),
            engine: Some(engine),
            profile: Some("work-persist".into()),
            ..Default::default()
        })
        .await
        .expect("create persistent");
    assert_eq!(p1.mode, "persistent");
    let p1_pid = p1.pid.expect("persistent session must expose pid");

    mgr.open("p1", &server.url("/iso/set-all"))
        .await
        .expect("open /iso/set-all");
    mgr.wait(
        "p1",
        WaitRequest {
            text: Some("ready".into()),
            timeout_ms: Some(5000),
            ..Default::default()
        },
    )
    .await
    .expect("wait ready");
    // Set a persistent cookie (with Max-Age) in addition to localStorage from /iso/set-all.
    mgr.eval(
        "p1",
        "document.cookie = 'persist_ck=yes; Path=/; Max-Age=3600'",
        false,
    )
    .await
    .expect("set persistent cookie");

    // Concurrent SessionManager targeting the same profile must fail with ProfileLocked
    let mgr2 = SessionManager::new(state_dir).expect("mgr2");
    let lock_err = mgr2
        .create_session(SessionCreateOpts {
            session_id: Some("p1-dup".into()),
            engine: Some(engine),
            profile: Some("work-persist".into()),
            ..Default::default()
        })
        .await
        .expect_err("concurrent persistent profile must fail with ProfileLocked");
    assert!(
        matches!(lock_err, MimikyuError::ProfileLocked(_)),
        "expected ProfileLocked, got {lock_err:?}"
    );

    let listed_profs = mgr.profile_list().expect("profile_list");
    assert_eq!(listed_profs.len(), 1);
    assert_eq!(listed_profs[0].name, "work-persist");
    assert!(listed_profs[0].locked);

    mgr.close_session("p1").await.expect("close p1");
    assert_pgid_gone(p1_pid as libc::pid_t);

    // Reopen persistent profile and confirm cookies + localStorage survived
    let p2 = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("p2".into()),
            engine: Some(engine),
            profile: Some("work-persist".into()),
            ..Default::default()
        })
        .await
        .expect("reopen persistent");
    let p2_pid = p2.pid.expect("p2 pid");
    mgr.open("p2", &server.url("/iso/echo-cookies"))
        .await
        .expect("open /iso/echo-cookies");
    let body = mgr
        .extract("p2", ExtractMode::Text)
        .await
        .expect("extract text");
    let blob = serde_json::to_string(&body).unwrap();
    assert!(
        blob.contains("persist_ck=yes"),
        "{engine}: persisted cookie missing: {blob}"
    );
    let ls_val = mgr
        .eval("p2", "localStorage.getItem('iso_ls')", false)
        .await
        .expect("eval ls");
    assert_eq!(ls_val, serde_json::json!("ls_secret"));

    mgr.close_session("p2").await.expect("close p2");
    assert_pgid_gone(p2_pid as libc::pid_t);

    mgr.profile_delete("work-persist")
        .expect("delete unlocked profile");
    assert!(mgr.profile_list().expect("profile_list after delete").is_empty());
    mgr.shutdown_all().await.expect("shutdown_all");
}

#[tokio::test]
async fn ephemeral_and_persistent_lifecycle_and_flock_chromium() {
    assert_ephemeral_and_persistent_lifecycle(EngineKind::Chromium).await;
}

#[tokio::test]
async fn ephemeral_and_persistent_lifecycle_and_flock_firefox() {
    assert_ephemeral_and_persistent_lifecycle(EngineKind::Firefox).await;
}

async fn assert_virtual_context_isolation_and_shutdown(engine: EngineKind) {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let mgr = SessionManager::new(tmp.path().join("state")).expect("SessionManager");

    let v1 = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("v1".into()),
            engine: Some(engine),
            virtual_ctx: true,
            ..Default::default()
        })
        .await
        .expect("create v1");
    let v2 = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("v2".into()),
            engine: Some(engine),
            virtual_ctx: true,
            ..Default::default()
        })
        .await
        .expect("create v2");

    assert_eq!(v1.mode, "virtual");
    assert_eq!(v2.mode, "virtual");
    assert_eq!(v1.pid, None, "virtual session must report pid: None");
    assert_eq!(v2.pid, None, "virtual session must report pid: None");

    mgr.open("v1", &server.url("/iso/set-all"))
        .await
        .expect("v1 open");
    mgr.wait(
        "v1",
        WaitRequest {
            text: Some("ready".into()),
            timeout_ms: Some(5000),
            ..Default::default()
        },
    )
    .await
    .expect("v1 wait");

    mgr.open("v2", &server.url("/iso/echo-cookies"))
        .await
        .expect("v2 open");
    let v2_text = serde_json::to_string(&mgr.extract("v2", ExtractMode::Text).await.unwrap())
        .unwrap();
    assert!(
        !v2_text.contains("http_secret") && !v2_text.contains("js_secret"),
        "{engine}: virtual session v2 leaked v1 cookies: {v2_text}"
    );
    assert_eq!(
        mgr.eval("v2", "localStorage.getItem('iso_ls')", false)
            .await
            .unwrap(),
        serde_json::Value::Null
    );

    // Closing v1 must preserve sibling v2
    mgr.close_session("v1").await.expect("close v1");
    assert_eq!(mgr.list_sessions().len(), 1);
    assert_eq!(mgr.list_sessions()[0].session_id, "v2");

    mgr.open("v2", &server.url("/target"))
        .await
        .expect("v2 still navigates after v1 close");
    assert_eq!(mgr.eval("v2", "1 + 2", false).await.unwrap(), serde_json::json!(3));

    mgr.shutdown_all().await.expect("shutdown_all");
    assert!(mgr.list_sessions().is_empty());

    let post_shutdown = mgr
        .create_session(SessionCreateOpts {
            session_id: Some("v3".into()),
            engine: Some(engine),
            virtual_ctx: true,
            ..Default::default()
        })
        .await
        .expect_err("create_session after shutdown_all must fail");
    assert!(matches!(post_shutdown, MimikyuError::Disposed));
}

#[tokio::test]
async fn virtual_context_isolation_pid_none_sibling_preservation_and_shutdown_all_chromium() {
    assert_virtual_context_isolation_and_shutdown(EngineKind::Chromium).await;
}

#[tokio::test]
async fn virtual_context_isolation_pid_none_sibling_preservation_and_shutdown_all_firefox() {
    assert_virtual_context_isolation_and_shutdown(EngineKind::Firefox).await;
}

async fn assert_env_profile_geoip_and_schemeless_proxy(engine: EngineKind) {
    let server = TestServer::start().await;
    let proxy = LocalHttpProxy::start(Some(("alice".into(), "s3cr3t!".into()))).await;
    let schemeless_proxy = format!(
        "alice:s3cr3t!@{}",
        proxy.proxy_url().trim_start_matches("http://")
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let mgr = SessionManager::new(tmp.path().join("state")).expect("SessionManager");

    mgr.create_session(SessionCreateOpts {
        session_id: Some("env1".into()),
        engine: Some(engine),
        virtual_ctx: true,
        preset: Some(PresetKind::Linux),
        geoip: Some("DE".into()),
        proxy: Some(schemeless_proxy),
        viewport: Some((1366, 768)),
        ..Default::default()
    })
    .await
    .expect("create session with schemeless proxy + geoip + preset");

    mgr.open("env1", &server.url("/env-probe"))
        .await
        .expect("open /env-probe");
    assert!(
        !proxy.hits().is_empty(),
        "{engine}: expected traffic routed through authenticated schemeless LocalHttpProxy"
    );

    let mut probe = serde_json::Value::Null;
    for _ in 0..80 {
        let val = mgr
            .eval(
                "env1",
                "(() => { const el = document.getElementById('main-out'); return el && el.textContent ? JSON.parse(el.textContent) : null; })()",
                false,
            )
            .await
            .unwrap();
        if !val.is_null() {
            probe = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!probe.is_null(), "{engine}: timed out waiting for /env-probe");
    assert_eq!(probe["timeZone"], "Europe/Berlin");
    assert_eq!(probe["language"], "de-DE");
    assert_eq!(probe["innerWidth"], 1366);
    assert_eq!(probe["innerHeight"], 768);
    assert_eq!(probe["noMimikyuGlobal"], true);

    let report = mgr.env_report("env1").await.expect("env_report");
    assert_eq!(report["configured"], true);
    assert_eq!(report["coherence"]["valid"], true);
    let report_str = serde_json::to_string(&report).unwrap();
    assert!(
        !report_str.contains("s3cr3t!") && !report_str.contains("alice"),
        "env_report must redact proxy credentials: {report_str}"
    );

    mgr.shutdown_all().await.expect("shutdown_all");
}

#[tokio::test]
async fn env_profile_geoip_coherence_and_schemeless_proxy_chromium() {
    assert_env_profile_geoip_and_schemeless_proxy(EngineKind::Chromium).await;
}

#[tokio::test]
async fn env_profile_geoip_coherence_and_schemeless_proxy_firefox() {
    assert_env_profile_geoip_and_schemeless_proxy(EngineKind::Firefox).await;
}

async fn assert_navigation_snapshot_stale_refs_and_m3_ops(engine: EngineKind) {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let upload_file = tmp.path().join("payload.txt");
    std::fs::write(&upload_file, b"hello-upload").unwrap();

    let mgr = SessionManager::new(tmp.path().join("state")).expect("SessionManager");

    // Create two sessions: s_no_mw (main_world_eval=false) and s_main (main_world_eval=true)
    mgr.create_session(SessionCreateOpts {
        session_id: Some("s_no_mw".into()),
        engine: Some(engine),
        virtual_ctx: true,
        main_world_eval: false,
        ..Default::default()
    })
    .await
    .expect("create s_no_mw");

    mgr.create_session(SessionCreateOpts {
        session_id: Some("s_main".into()),
        engine: Some(engine),
        virtual_ctx: true,
        main_world_eval: true,
        ..Default::default()
    })
    .await
    .expect("create s_main");

    // 1. URL scheme validation
    for bad_url in [
        "",
        "not a valid url :::",
        "http://127.0.0.1:99999/",
        "ftp://127.0.0.1/file",
        "file:///etc/passwd",
        "javascript:alert(1)",
    ] {
        assert!(
            matches!(
                mgr.open("s_main", bad_url).await.err(),
                Some(MimikyuError::InvalidConfig(_) | MimikyuError::Url(_))
            ),
            "{engine}: expected URL rejection for {bad_url:?}"
        );
    }

    mgr.open("s_main", "data:text/html;charset=utf-8,<h1 id=\"d\">DataOK</h1>")
        .await
        .expect("data: URL must be accepted");
    assert_eq!(
        mgr.eval("s_main", "document.getElementById('d').textContent", false)
            .await
            .unwrap(),
        serde_json::json!("DataOK")
    );

    // 2. Forms + snapshot + @eN fill/type/select/upload/scroll/click + extract
    mgr.open("s_main", &server.url("/forms-rich"))
        .await
        .expect("open /forms-rich");
    let snap = mgr
        .snapshot(
            "s_main",
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await
        .expect("snapshot /forms-rich");
    assert!(snap.ref_count > 0);

    let user_ref = find_ref_by_name_substr(&snap, "username");
    let country_ref = find_ref_by_name_substr(&snap, "country");
    let upload_ref = find_ref_by_name_substr(&snap, "upload");
    let submit_ref = find_ref_by_name_substr(&snap, "submit form");

    mgr.fill("s_main", &user_ref, "alice", None)
        .await
        .expect("fill @eN");
    mgr.type_text("s_main", "#notes", "", None)
        .await
        .expect("empty type_text must succeed");
    mgr.type_text("s_main", "#notes", "note1", None)
        .await
        .expect("non-empty type_text");
    let selected = mgr
        .select("s_main", &country_ref, "de")
        .await
        .expect("select @eN");
    assert_eq!(selected, "de");

    mgr.upload("s_main", &upload_ref, upload_file.clone())
        .await
        .expect("upload @eN");
    assert!(
        mgr.upload("s_main", &upload_ref, tmp.path().join("missing.txt"))
            .await
            .is_err(),
        "missing upload file must fail"
    );
    assert!(
        mgr.upload("s_main", &upload_ref, tmp.path().to_path_buf())
            .await
            .is_err(),
        "directory upload path must fail"
    );

    mgr.scroll("s_main", 0.0, 400.0, None)
        .await
        .expect("scroll");
    mgr.press("s_main", "Tab").await.expect("press Tab");
    mgr.click("s_main", &submit_ref, None)
        .await
        .expect("click submit @eN");

    let text_extract = mgr
        .extract("s_main", ExtractMode::Text)
        .await
        .expect("extract text");
    let text_blob = serde_json::to_string(&text_extract).unwrap();
    assert!(
        text_blob.contains("submitted:alice:de"),
        "{engine}: form submission missing in text extract: {text_blob}"
    );
    assert!(mgr.extract("s_main", ExtractMode::Markdown).await.is_ok());
    assert!(mgr.extract("s_main", ExtractMode::Links).await.is_ok());
    assert!(mgr.extract("s_main", ExtractMode::Tables).await.is_ok());

    // 3. Stale-ref invalidation across open / back / forward / reload
    mgr.open("s_main", &server.url("/"))
        .await
        .expect("open /");
    let stale_err = mgr
        .click("s_main", &submit_ref, None)
        .await
        .expect_err("stale @eN ref after open() must fail");
    assert!(matches!(stale_err, MimikyuError::ElementNotFound(_)));

    let home_snap_nav = mgr
        .snapshot("s_main", SnapshotOptions::default())
        .await
        .expect("snapshot /");
    let any_ref = home_snap_nav
        .ref_map
        .iter()
        .next()
        .map(|(k, _)| k.clone())
        .expect("at least one ref on /");

    mgr.back("s_main").await.expect("back");
    assert!(matches!(
        mgr.click("s_main", &any_ref, None).await.err(),
        Some(MimikyuError::ElementNotFound(_))
    ));
    mgr.forward("s_main").await.expect("forward");
    let home_snap_nav2 = mgr
        .snapshot("s_main", SnapshotOptions::default())
        .await
        .expect("snapshot / 2");
    let any_ref2 = home_snap_nav2
        .ref_map
        .iter()
        .next()
        .map(|(k, _)| k.clone())
        .unwrap();
    mgr.reload("s_main").await.expect("reload");
    assert!(matches!(
        mgr.click("s_main", &any_ref2, None).await.err(),
        Some(MimikyuError::ElementNotFound(_))
    ));

    // 4. Wait boundaries + /spa-delayed
    assert!(matches!(
        mgr.wait("s_main", WaitRequest::default()).await.err(),
        Some(MimikyuError::InvalidConfig(_))
    ));
    let t0 = Instant::now();
    let bad_css = mgr
        .wait(
            "s_main",
            WaitRequest {
                selector: Some("div[[[".into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("invalid CSS selector must fail fast");
    assert!(matches!(bad_css, MimikyuError::InvalidConfig(_)));
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "invalid CSS selector took too long: {:?}",
        t0.elapsed()
    );

    mgr.open("s_main", &server.url("/spa-delayed"))
        .await
        .expect("open /spa-delayed");
    mgr.wait(
        "s_main",
        WaitRequest {
            idle: true,
            timeout_ms: Some(5000),
            ..Default::default()
        },
    )
    .await
    .expect("wait idle /spa-delayed");
    assert_eq!(
        mgr.eval(
            "s_main",
            "document.getElementById('hydrated-badge')?.textContent || ''",
            false
        )
        .await
        .unwrap(),
        serde_json::json!("HYDRATED_OK")
    );

    // 5. Batch execution: happy path, BatchAction::Open ref invalidation, mid-step abort
    let batch_ok = mgr
        .batch(
            "s_main",
            vec![
                BatchAction::Open {
                    url: server.url("/"),
                },
                BatchAction::Snapshot {
                    max_depth: None,
                    interactive_only: true,
                    diff: false,
                },
                BatchAction::Fill {
                    target: "#inp".into(),
                    value: "batch-val".into(),
                    humanize: None,
                },
                BatchAction::Click {
                    target: "#btn".into(),
                    humanize: None,
                    button: None,
                    click_count: None,
                },
                BatchAction::Extract {
                    mode: "text".into(),
                },
            ],
        )
        .await
        .expect("batch happy path");
    assert!(batch_ok.ok, "expected batch_ok: {batch_ok:?}");
    assert_eq!(batch_ok.completed_steps, 5);

    // BatchAction::Open must invalidate prior @e1
    let home_snap = mgr
        .snapshot("s_main", SnapshotOptions::default())
        .await
        .expect("home snap");
    let first_ref = home_snap
        .ref_map
        .iter()
        .next()
        .map(|(k, _)| k.clone())
        .unwrap();
    let batch_stale = mgr
        .batch(
            "s_main",
            vec![
                BatchAction::Open {
                    url: server.url("/target"),
                },
                BatchAction::Click {
                    target: first_ref,
                    humanize: None,
                    button: None,
                    click_count: None,
                },
                BatchAction::Extract {
                    mode: "text".into(),
                },
            ],
        )
        .await
        .expect("batch with stale ref returns BatchReport");
    assert!(!batch_stale.ok);
    assert_eq!(batch_stale.completed_steps, 1);
    let err = batch_stale.error.expect("batch error details");
    assert_eq!(err.step_index, 1);
    assert!(err.recovery_snapshot.is_some());

    // 6. Eval isolated vs main_world gating
    mgr.open("s_no_mw", &server.url("/"))
        .await
        .expect("s_no_mw open");
    assert!(matches!(
        mgr.eval("s_no_mw", "window.__mainSecret", true).await.err(),
        Some(MimikyuError::MainWorldDisabled)
    ));
    mgr.open("s_main", &server.url("/"))
        .await
        .expect("s_main open /");
    assert_eq!(
        mgr.eval("s_main", "typeof window.__mainSecret", false)
            .await
            .unwrap(),
        serde_json::json!("undefined")
    );
    assert_eq!(
        mgr.eval("s_main", "window.__mainSecret", true)
            .await
            .unwrap(),
        serde_json::json!("main-only-value")
    );

    // 7. Telemetry logs (console + page_errors + network)
    mgr.open("s_main", &server.url("/net/console-and-errors"))
        .await
        .expect("open /net/console-and-errors");
    mgr.wait(
        "s_main",
        WaitRequest {
            idle: true,
            timeout_ms: Some(3000),
            ..Default::default()
        },
    )
    .await
    .expect("wait idle console-and-errors");

    let c_logs = mgr.logs("s_main", LogKind::Console).await.expect("console logs");
    let c_blob = serde_json::to_string(&c_logs).unwrap();
    assert!(c_blob.contains("net-log-1"), "{engine}: {c_blob}");
    assert!(c_blob.contains("net-err-4"), "{engine}: {c_blob}");
    assert!(c_blob.contains("uncaught-boom-42"), "{engine}: {c_blob}");

    let n_logs = mgr.logs("s_main", LogKind::Network).await.expect("network logs");
    let n_blob = serde_json::to_string(&n_logs).unwrap();
    assert!(n_blob.contains("/net/console-and-errors"), "{engine}: {n_blob}");

    mgr.shutdown_all().await.expect("shutdown_all");
}

#[tokio::test]
async fn navigation_snapshot_stale_refs_and_m3_ops_chromium() {
    assert_navigation_snapshot_stale_refs_and_m3_ops(EngineKind::Chromium).await;
}

#[tokio::test]
async fn navigation_snapshot_stale_refs_and_m3_ops_firefox() {
    assert_navigation_snapshot_stale_refs_and_m3_ops(EngineKind::Firefox).await;
}

async fn assert_screenshot_pdf_and_export_import_boundaries(engine: EngineKind) {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let mgr = SessionManager::new(tmp.path().join("state")).expect("SessionManager");

    mgr.create_session(SessionCreateOpts {
        session_id: Some("media1".into()),
        engine: Some(engine),
        virtual_ctx: true,
        ..Default::default()
    })
    .await
    .expect("create media1");

    // 1. Screenshot via @eN, CSS selector, and full_page (scroll-preserving)
    mgr.open("media1", &server.url("/extract-article"))
        .await
        .expect("open /extract-article");
    let snap = mgr
        .snapshot("media1", SnapshotOptions::default())
        .await
        .expect("snapshot /extract-article");
    let offscreen_ref = find_ref_by_name_substr(&snap, "Offscreen Ref Target");

    let scroll_before = mgr.eval("media1", "window.scrollY", false).await.unwrap();
    let ref_png = tmp.path().join("ref.png");
    let ref_res = mgr
        .screenshot("media1", &ref_png, Some(&offscreen_ref), false)
        .await
        .expect("screenshot @eN");
    assert!(ref_res.bytes > 0 && ref_res.width > 0 && ref_res.height > 0);
    assert!(std::fs::read(&ref_png)
        .unwrap()
        .starts_with(b"\x89PNG\r\n\x1a\n"));

    let sel_png = tmp.path().join("sel.png");
    let sel_res = mgr
        .screenshot("media1", &sel_png, Some("#offscreen-card"), false)
        .await
        .expect("screenshot CSS selector");
    assert!(sel_res.bytes > 0 && sel_res.width > 0 && sel_res.height > 0);
    assert!(std::fs::read(&sel_png)
        .unwrap()
        .starts_with(b"\x89PNG\r\n\x1a\n"));

    let full_png = tmp.path().join("full.png");
    let full_res = mgr
        .screenshot("media1", &full_png, None, true)
        .await
        .expect("screenshot full_page");
    assert!(full_res.bytes > 0 && full_res.width > 0 && full_res.height > 0);
    let scroll_after = mgr.eval("media1", "window.scrollY", false).await.unwrap();
    assert_eq!(scroll_before, scroll_after, "{engine}: screenshot mutated scrollY");

    // 2. Invalid screenshot targets & paths MUST fail and leave ZERO 0-byte files
    mgr.open(
        "media1",
        "data:text/html;charset=utf-8,<div id=\"zero\" style=\"width:0;height:0\"></div><div id=\"hidden\" style=\"display:none\">H</div>",
    )
    .await
    .expect("open zero/hidden page");

    for (label, out_p, tgt) in [
        ("zero", tmp.path().join("zero.png"), Some("#zero")),
        ("hidden", tmp.path().join("hidden.png"), Some("#hidden")),
        ("bad_ref", tmp.path().join("bad_ref.png"), Some("@e99999")),
        (
            "missing_parent",
            tmp.path().join("missing_dir/fail.png"),
            None,
        ),
    ] {
        let err = mgr
            .screenshot("media1", &out_p, tgt, false)
            .await
            .expect_err(&format!("screenshot {label} must fail"));
        assert!(
            matches!(
                err,
                MimikyuError::ElementNotFound(_) | MimikyuError::InvalidConfig(_)
            ),
            "{engine}: unexpected error for {label}: {err:?}"
        );
        assert!(
            !out_p.exists(),
            "{engine}: failed screenshot {label} left artifact {:?}",
            out_p
        );
    }
    assert!(mgr
        .screenshot("media1", tmp.path(), None, false)
        .await
        .is_err());

    // 3. PDF valid header + missing parent rejection
    mgr.open("media1", &server.url("/target"))
        .await
        .expect("open /target for pdf");
    let pdf_path = tmp.path().join("out.pdf");
    let pdf_res = mgr.pdf("media1", &pdf_path).await.expect("pdf");
    assert!(pdf_res.bytes > 0);
    assert!(std::fs::read(&pdf_path).unwrap().starts_with(b"%PDF-"));

    let bad_pdf = tmp.path().join("no_such_dir/out.pdf");
    assert!(mgr.pdf("media1", &bad_pdf).await.is_err());
    assert!(!bad_pdf.exists());
    assert!(mgr.pdf("media1", tmp.path()).await.is_err());

    // 4. export_session / import_session boundaries + roundtrip
    let bad_export = tmp.path().join("missing_export_dir/state.json");
    assert!(mgr.export_session("media1", &bad_export).await.is_err());
    assert!(
        !bad_export.parent().unwrap().exists(),
        "export_session must not create missing parent directory"
    );
    assert!(mgr.export_session("media1", tmp.path()).await.is_err());

    mgr.open("media1", &server.url("/iso/set-all"))
        .await
        .expect("open /iso/set-all");
    mgr.wait(
        "media1",
        WaitRequest {
            text: Some("ready".into()),
            timeout_ms: Some(5000),
            ..Default::default()
        },
    )
    .await
    .expect("wait ready");

    let export_file = tmp.path().join("exported.json");
    mgr.export_session("media1", &export_file)
        .await
        .expect("export_session");
    assert!(export_file.is_file());

    mgr.create_session(SessionCreateOpts {
        session_id: Some("media2".into()),
        engine: Some(engine),
        virtual_ctx: true,
        ..Default::default()
    })
    .await
    .expect("create media2");
    mgr.import_session("media2", &export_file)
        .await
        .expect("import_session");
    mgr.open("media2", &server.url("/iso/echo-cookies"))
        .await
        .expect("open /iso/echo-cookies");
    let text = serde_json::to_string(&mgr.extract("media2", ExtractMode::Text).await.unwrap())
        .unwrap();
    assert!(
        text.contains("iso_http=http_secret") && text.contains("iso_js=js_secret"),
        "{engine}: imported cookies missing: {text}"
    );

    mgr.shutdown_all().await.expect("shutdown_all");
}

#[tokio::test]
async fn screenshot_pdf_and_export_import_boundaries_chromium() {
    assert_screenshot_pdf_and_export_import_boundaries(EngineKind::Chromium).await;
}

#[tokio::test]
async fn screenshot_pdf_and_export_import_boundaries_firefox() {
    assert_screenshot_pdf_and_export_import_boundaries(EngineKind::Firefox).await;
}

#[tokio::test]
async fn per_session_concurrency_does_not_block_distinct_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mgr = Arc::new(SessionManager::new(tmp.path().join("state")).expect("SessionManager"));

    mgr.create_session(SessionCreateOpts {
        session_id: Some("slow_a".into()),
        virtual_ctx: true,
        ..Default::default()
    })
    .await
    .expect("create slow_a");
    mgr.create_session(SessionCreateOpts {
        session_id: Some("fast_b".into()),
        virtual_ctx: true,
        ..Default::default()
    })
    .await
    .expect("create fast_b");

    let mgr_a = Arc::clone(&mgr);
    let wait_task = tokio::spawn(async move {
        mgr_a
            .wait(
                "slow_a",
                WaitRequest {
                    text: Some("NEVER_APPEARS_12345".into()),
                    timeout_ms: Some(1500),
                    ..Default::default()
                },
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    let list = mgr.list_sessions();
    assert_eq!(list.len(), 2);
    let eval_b = mgr
        .eval("fast_b", "21 + 21", false)
        .await
        .expect("eval fast_b while slow_a waits");
    assert_eq!(eval_b, serde_json::json!(42));
    assert!(
        t0.elapsed() < Duration::from_millis(800),
        "fast_b/list_sessions blocked behind slow_a wait: {:?}",
        t0.elapsed()
    );

    let wait_res = wait_task.await.expect("join wait_task");
    assert!(matches!(wait_res, Err(MimikyuError::Timeout { .. })));

    mgr.shutdown_all().await.expect("shutdown_all");
}
