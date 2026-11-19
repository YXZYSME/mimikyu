mod support;

use mimikyu_core::driver::{driver_for, EngineKind, LaunchOptions, ProfileMode};
use mimikyu_core::input::{ActionBatcher, BatchAction, ChokepointInput};
use mimikyu_core::snapshot::SnapshotState;
use mimikyu_core::wait::{WaitEngine, WaitIdleOptions, WaitOptions, WaitSelectorState};
use mimikyu_core::MimikyuError;
use support::{assert_pgid_gone, TestServer};

async fn assert_waits_and_batch_parity(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch browser");
    let pid = session.info().pid.expect("pid");

    // 1. SPA delayed fetch hydration + moving bbox stability
    session
        .navigate(&server.url("/spa-delayed"))
        .await
        .expect("navigate /spa-delayed");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 8_000,
            poll_interval_ms: 25,
            network_quiet_ms: 200,
            ignore_long_lived_sockets: true,
            stable_target: Some("#moving-box".into()),
            bbox_stable_frames: 2,
        },
    )
    .await
    .expect("wait_idle on SPA delayed hydration");

    let hydrated = session
        .eval_isolated("document.getElementById('hydrated-badge')?.textContent")
        .await
        .expect("check hydrated-badge");
    assert_eq!(hydrated, "HYDRATED_OK");

    // 2. Short fetches between polls (activity_seq must prevent premature quiet exit)
    session
        .navigate(&server.url("/spa-short-polls"))
        .await
        .expect("navigate /spa-short-polls");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 8_000,
            poll_interval_ms: 25,
            network_quiet_ms: 150,
            ..Default::default()
        },
    )
    .await
    .expect("wait_idle on short polls");
    let polls_done = session
        .eval_isolated("document.getElementById('polls-done')?.textContent")
        .await
        .expect("check polls-done");
    assert_eq!(polls_done, "POLLS_FINISHED");

    // 3. Destroyed iframe with pending fetch (delay=900 > 80ms teardown) must not hang inflight / wait_idle across repeated runs
    for run in 0..2 {
        session
            .navigate(&server.url("/destroyed-iframe-fetch-900"))
            .await
            .expect("navigate /destroyed-iframe-fetch-900");
        WaitEngine::wait_idle(
            session.as_ref(),
            None,
            WaitIdleOptions {
                timeout_ms: 2_500,
                poll_interval_ms: 25,
                network_quiet_ms: 150,
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("{engine} run {run}: wait_idle after destroyed iframe fetch (900ms) leaked: {e}"));
        assert_eq!(session.inflight_requests().await.unwrap(), 0);
    }

    // 3b. Open EventSource SSE stream must be ignored when ignore_long_lived_sockets=true
    session
        .navigate(&server.url("/spa-sse"))
        .await
        .expect("navigate /spa-sse");
    let sse_started = std::time::Instant::now();
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 2_200,
            poll_interval_ms: 25,
            network_quiet_ms: 150,
            ignore_long_lived_sockets: true,
            stable_target: None,
            bbox_stable_frames: 2,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{engine}: wait_idle(ignore_long_lived_sockets=true) failed on live SSE: {e}"));
    assert!(
        sse_started.elapsed().as_millis() < 2_200,
        "{engine}: SSE should not block wait_idle"
    );
    assert_eq!(
        session
            .eval_isolated("document.getElementById('sse-status').textContent")
            .await
            .unwrap(),
        "SSE_FETCH_OK"
    );

    // Re-navigate to /destroyed-iframe-fetch for condition waits check
    session
        .navigate(&server.url("/destroyed-iframe-fetch"))
        .await
        .expect("navigate /destroyed-iframe-fetch");

    // 4. Condition waits + timeout checks
    WaitEngine::wait_url(
        session.as_ref(),
        "/destroyed-iframe-fetch",
        WaitOptions {
            timeout_ms: 2_000,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect("wait_url");

    WaitEngine::wait_text(
        session.as_ref(),
        "removed",
        WaitOptions {
            timeout_ms: 2_000,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect("wait_text");

    WaitEngine::wait_selector(
        session.as_ref(),
        "#status",
        WaitSelectorState::Visible,
        WaitOptions {
            timeout_ms: 2_000,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect("wait_selector visible");

    let timeout_err = WaitEngine::wait_text(
        session.as_ref(),
        "NEVER_EXISTS_TEXT_12345",
        WaitOptions {
            timeout_ms: 150,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect_err("missing text must time out");
    assert!(matches!(timeout_err, MimikyuError::Timeout { .. }));

    // 5. ActionBatcher success workflow
    let chokepoint = ChokepointInput::new(session.clone());
    let mut snap_state = SnapshotState::default();
    let ok_batch = vec![
        BatchAction::Open {
            url: server.url("/forms-rich"),
        },
        BatchAction::WaitSelector {
            selector: "#prefilled".into(),
            timeout_ms: Some(3_000),
            state: Some(WaitSelectorState::Visible),
        },
        BatchAction::Fill {
            target: "#prefilled".into(),
            value: "batch-user".into(),
            humanize: Some(false),
        },
        BatchAction::Select {
            target: "#country".into(),
            value: Some("jp".into()),
            values: vec![],
        },
        BatchAction::Click {
            target: "#submit-bottom".into(),
            humanize: Some(false),
            button: None,
            click_count: None,
        },
        BatchAction::WaitText {
            text: "submitted:batch-user:jp".into(),
            timeout_ms: Some(2_000),
        },
        BatchAction::Snapshot {
            max_depth: None,
            interactive_only: true,
            diff: false,
        },
        BatchAction::Extract {
            mode: "text".into(),
        },
    ];
    let report = ActionBatcher::execute(session.as_ref(), &chokepoint, &mut snap_state, &ok_batch)
        .await
        .expect("execute ok batch");
    assert!(report.ok, "expected ok batch, got error={:?}", report.error);
    assert_eq!(report.completed_steps, 8);

    // 6. ActionBatcher abort-on-first-failure with step_index + recovery_snapshot
    let fail_batch = vec![
        BatchAction::Fill {
            target: "#prefilled".into(),
            value: "step0".into(),
            humanize: Some(false),
        },
        BatchAction::Click {
            target: "#does-not-exist-999".into(),
            humanize: None,
            button: None,
            click_count: None,
        },
        BatchAction::Fill {
            target: "#notes".into(),
            value: "must-not-run".into(),
            humanize: None,
        },
    ];
    let fail_report =
        ActionBatcher::execute(session.as_ref(), &chokepoint, &mut snap_state, &fail_batch)
            .await
            .expect("execute fail batch");
    assert!(!fail_report.ok);
    assert_eq!(fail_report.completed_steps, 1);
    let err = fail_report.error.expect("batch error present");
    assert_eq!(err.step_index, 1);
    assert_eq!(err.action, "click");
    let rec = err.recovery_snapshot.expect("recovery snapshot present");
    assert!(rec.ref_count > 0 && rec.text.contains("@e1"));
    let notes_val = session
        .eval_isolated("document.getElementById('notes').value")
        .await
        .expect("notes value");
    assert_eq!(notes_val, "", "step 2 must not execute after step 1 failure");

    // 7. ActionBatcher Upload, Scroll, and Press steps
    let batch_tmp = tempfile::NamedTempFile::new().expect("batch temp");
    std::fs::write(batch_tmp.path(), b"batch-upload-payload").expect("write batch temp");
    let extra_batch = vec![
        BatchAction::Open {
            url: server.url("/forms-rich"),
        },
        BatchAction::Upload {
            target: "#upload".into(),
            files: vec![batch_tmp.path().to_path_buf()],
        },
        BatchAction::Click {
            target: "#prefilled".into(),
            humanize: Some(false),
            button: None,
            click_count: None,
        },
        BatchAction::Press {
            key: "Alt+x".into(),
        },
        BatchAction::Scroll {
            dx: 0.0,
            dy: 400.0,
            target: None,
            humanize: Some(false),
        },
    ];
    let extra_report =
        ActionBatcher::execute(session.as_ref(), &chokepoint, &mut snap_state, &extra_batch)
            .await
            .expect("execute extra batch");
    assert!(extra_report.ok, "extra batch error={:?}", extra_report.error);
    assert_eq!(extra_report.completed_steps, 5);
    assert_eq!(
        session
            .eval_isolated("document.getElementById('upload').files.length")
            .await
            .unwrap(),
        1
    );
    let scrolled_y = session
        .eval_isolated("window.scrollY")
        .await
        .unwrap()
        .as_f64()
        .unwrap_or(0.0);
    assert!(scrolled_y > 0.0, "{engine}: BatchAction::Scroll must settle scrolled window (got {scrolled_y})");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('prefilled').value")
            .await
            .unwrap(),
        "old-secret-text",
        "{engine}: BatchAction::Press(Alt+x) must not insert literal 'x'"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_waits_and_batch_chromium() {
    assert_waits_and_batch_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_waits_and_batch_firefox() {
    assert_waits_and_batch_parity(EngineKind::Firefox).await;
}

#[tokio::test]
async fn live_oopif_site_per_process_wait_idle_no_leak_chromium() {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;

    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            extra_args: vec!["--site-per-process".to_string()],
            ..Default::default()
        })
        .await
        .expect("launch chromium with --site-per-process");
    let pid = session.info().pid.expect("pid");

    // (a) Alive cross-site OOPIF must not leak inflight += 1, and subsequent wait_idle must succeed
    let xo_alive = server2.localhost_url("/xo-chord-child");
    let alive_host = server.url(&format!("/xo-iframe-wait-host?xo={xo_alive}"));
    session
        .navigate(&alive_host)
        .await
        .expect("navigate /xo-iframe-wait-host");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 4_000,
            poll_interval_ms: 25,
            network_quiet_ms: 150,
            ..Default::default()
        },
    )
    .await
    .expect("wait_idle after alive cross-site OOPIF");
    assert_eq!(
        session.inflight_requests().await.unwrap(),
        0,
        "alive cross-site OOPIF must leave inflight_requests == 0"
    );

    session
        .navigate(&server.url("/forms-rich"))
        .await
        .expect("navigate /forms-rich after alive OOPIF");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 2_500,
            poll_interval_ms: 25,
            network_quiet_ms: 120,
            ..Default::default()
        },
    )
    .await
    .expect("subsequent wait_idle after alive cross-site OOPIF");

    // (b) Destroyed cross-site OOPIF mid-load (both slow response and committed child) must prune inflight
    // and leave subsequent ActionBatcher::WaitIdle clean
    let xo_slow = server2.localhost_url("/slow-iframe-doc?delay=900");
    let destroyed_host = server.url(&format!("/xo-destroyed-iframe-host?xo={xo_slow}"));
    session
        .navigate(&destroyed_host)
        .await
        .expect("navigate /xo-destroyed-iframe-host");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 4_000,
            poll_interval_ms: 25,
            network_quiet_ms: 150,
            ..Default::default()
        },
    )
    .await
    .expect("wait_idle after destroyed cross-site OOPIF");
    assert_eq!(
        session.inflight_requests().await.unwrap(),
        0,
        "destroyed cross-site OOPIF must leave inflight_requests == 0"
    );

    let chokepoint = ChokepointInput::new(session.clone());
    let mut snap_state = SnapshotState::default();
    let batch = vec![
        BatchAction::Open {
            url: server.url("/forms-rich"),
        },
        BatchAction::WaitIdle {
            timeout_ms: Some(2_500),
            network_quiet_ms: Some(120),
            target: None,
        },
    ];
    let report = ActionBatcher::execute(session.as_ref(), &chokepoint, &mut snap_state, &batch)
        .await
        .expect("execute post-OOPIF ActionBatcher::WaitIdle");
    assert!(
        report.ok,
        "ActionBatcher::WaitIdle after destroyed OOPIF failed: {:?}",
        report.error
    );
    assert_eq!(report.completed_steps, 2);

    // (c) Cross-site OOPIF initiating its own delayed fetch() must keep wait_idle waiting until done
    let xo_fetch_child = server2.localhost_url("/xo-oopif-fetch-child?ms=600");
    let xo_fetch_host = server.url(&format!("/xo-iframe-wait-host?xo={xo_fetch_child}"));
    session
        .navigate(&xo_fetch_host)
        .await
        .expect("navigate /xo-iframe-wait-host (fetch)");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 4_000,
            poll_interval_ms: 25,
            network_quiet_ms: 120,
            ..Default::default()
        },
    )
    .await
    .expect("wait_idle after OOPIF-initiated fetch");
    let frames = session.list_frames().await.expect("list_frames oopif fetch");
    let child_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-oopif-fetch-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-oopif-fetch-child frame");
    let fetch_status = session
        .eval_isolated_in_frame(
            "document.getElementById('oopif-fetch-status').textContent",
            Some(&child_fid),
        )
        .await
        .expect("oopif-fetch-status");
    assert_eq!(
        fetch_status, "OOPIF_FETCH_DONE",
        "wait_idle must wait for OOPIF-initiated fetch() under --site-per-process"
    );
    assert_eq!(
        session.inflight_requests().await.unwrap(),
        0,
        "OOPIF-initiated fetch() must leave inflight_requests == 0"
    );

    // (d) Cross-site OOPIF with open EventSource SSE must still obey ignore_long_lived_sockets=true
    let xo_sse_child = server2.localhost_url("/xo-oopif-sse-child");
    let xo_sse_host = server.url(&format!("/xo-iframe-wait-host?xo={xo_sse_child}"));
    session
        .navigate(&xo_sse_host)
        .await
        .expect("navigate /xo-iframe-wait-host (sse)");
    let sse_started = std::time::Instant::now();
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 2_500,
            poll_interval_ms: 25,
            network_quiet_ms: 120,
            ignore_long_lived_sockets: true,
            ..Default::default()
        },
    )
    .await
    .expect("wait_idle with OOPIF EventSource + ignore_long_lived_sockets=true");
    assert!(
        sse_started.elapsed().as_millis() < 2_500,
        "OOPIF EventSource must not block wait_idle"
    );
    let sse_frames = session.list_frames().await.expect("list_frames oopif sse");
    let sse_fid = sse_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-oopif-sse-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-oopif-sse-child frame");
    let sse_status = session
        .eval_isolated_in_frame(
            "document.getElementById('oopif-sse-status').textContent",
            Some(&sse_fid),
        )
        .await
        .expect("oopif-sse-status");
    assert_eq!(sse_status, "OOPIF_SSE_OK");

    // (e) Destroyed cross-site OOPIF mid-fetch (900ms) must prune child-session Fetch without leaking inflight
    let xo_fetch_900 = server2.localhost_url("/xo-oopif-fetch-child?ms=900");
    let xo_destroyed_fetch_host = server.url(&format!("/xo-destroyed-iframe-host?xo={xo_fetch_900}"));
    session
        .navigate(&xo_destroyed_fetch_host)
        .await
        .expect("navigate /xo-destroyed-iframe-host (fetch 900ms)");
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 2_500,
            poll_interval_ms: 25,
            network_quiet_ms: 120,
            ..Default::default()
        },
    )
    .await
    .expect("wait_idle after destroyed OOPIF mid-fetch");
    assert_eq!(
        session.inflight_requests().await.unwrap(),
        0,
        "destroyed OOPIF mid-fetch must leave inflight_requests == 0"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

async fn assert_batch_scroll_immediately_followed_by_click(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch browser");
    let pid = session.info().pid.expect("pid");
    let chokepoint = ChokepointInput::new(session.clone());
    let mut snap_state = SnapshotState::default();

    let actions = vec![
        BatchAction::Open {
            url: server.url("/scroll-click-batch"),
        },
        BatchAction::Scroll {
            dx: 0.0,
            dy: 500.0,
            target: None,
            humanize: Some(false),
        },
        BatchAction::Click {
            target: "#scroll-target-btn".into(),
            humanize: Some(false),
            button: None,
            click_count: None,
        },
    ];
    let report = ActionBatcher::execute(session.as_ref(), &chokepoint, &mut snap_state, &actions)
        .await
        .expect("execute scroll->click batch");
    assert!(
        report.ok,
        "{engine}: scroll->click batch failed: {:?}",
        report.error
    );
    assert_eq!(report.completed_steps, 3);

    let result_text = session
        .eval_isolated("document.getElementById('scroll-click-result').textContent")
        .await
        .expect("read scroll-click-result");
    assert!(
        result_text
            .as_str()
            .is_some_and(|s| s.starts_with("CLICKED_AT_SY:")),
        "{engine}: BatchAction::Scroll immediately followed by BatchAction::Click must hit #scroll-target-btn, got {result_text:?}"
    );
    let active_id = session
        .eval_isolated("document.activeElement && document.activeElement.id")
        .await
        .expect("read activeElement");
    assert_eq!(
        active_id, "scroll-target-btn",
        "{engine}: activeElement must be #scroll-target-btn"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_batch_scroll_immediately_followed_by_click_chromium() {
    assert_batch_scroll_immediately_followed_by_click(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_batch_scroll_immediately_followed_by_click_firefox() {
    assert_batch_scroll_immediately_followed_by_click(EngineKind::Firefox).await;
}

#[tokio::test]
async fn wait_rejects_overflow_or_excessive_timeout_ms() {
    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid") as libc::pid_t;
    session
        .navigate("data:text/html,<p>wait</p>")
        .await
        .expect("nav");

    let excess = WaitOptions {
        timeout_ms: u64::MAX,
        poll_interval_ms: 25,
    };
    let e1 = WaitEngine::wait_text(session.as_ref(), "never", excess.clone())
        .await
        .unwrap_err();
    assert!(matches!(e1, MimikyuError::InvalidConfig(_)));

    let e2 = WaitEngine::wait_url(session.as_ref(), "never", excess.clone())
        .await
        .unwrap_err();
    assert!(matches!(e2, MimikyuError::InvalidConfig(_)));

    let e3 = WaitEngine::wait_selector(
        session.as_ref(),
        "#never",
        WaitSelectorState::Visible,
        excess,
    )
    .await
    .unwrap_err();
    assert!(matches!(e3, MimikyuError::InvalidConfig(_)));

    let e4 = WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: u64::MAX,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(e4, MimikyuError::InvalidConfig(_)));

    let t0 = std::time::Instant::now();
    let bad_sel_err = WaitEngine::wait_selector(
        session.as_ref(),
        "div[[[",
        WaitSelectorState::Visible,
        WaitOptions {
            timeout_ms: 10_000,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect_err("WaitEngine::wait_selector must fail fast on invalid CSS selector");
    assert!(
        matches!(bad_sel_err, MimikyuError::InvalidConfig(_)),
        "expected InvalidConfig, got {bad_sel_err:?}"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "WaitEngine::wait_selector stalled on invalid CSS selector: {:?}",
        t0.elapsed()
    );

    let chokepoint = ChokepointInput::new(session.clone());
    let mut snap_state = SnapshotState::default();
    let t1 = std::time::Instant::now();
    let bad_batch = ActionBatcher::execute(
        session.as_ref(),
        &chokepoint,
        &mut snap_state,
        &[BatchAction::WaitSelector {
            selector: "div[[[".to_string(),
            timeout_ms: Some(10_000),
            state: Some(WaitSelectorState::Visible),
        }],
    )
    .await
    .expect("ActionBatcher returns BatchReport on invalid CSS selector");
    assert!(!bad_batch.ok);
    assert!(
        t1.elapsed() < std::time::Duration::from_secs(2),
        "BatchAction::WaitSelector stalled on invalid CSS selector: {:?}",
        t1.elapsed()
    );
    let b_err = bad_batch.error.expect("batch error present");
    assert_eq!(b_err.action, "wait_selector");
    assert!(
        b_err.message.contains("invalid CSS selector"),
        "expected invalid CSS selector message, got {}",
        b_err.message
    );

    // wait_idle stable_target must fail fast on invalid CSS, empty selector, and unknown @eN ref
    for (label, target, is_ref) in [
        ("invalid CSS", "div[[[", false),
        ("empty selector", "", false),
        ("whitespace selector", "   ", false),
        ("unknown @eN ref", "@e999", true),
    ] {
        let t_idle = std::time::Instant::now();
        let err = WaitEngine::wait_idle(
            session.as_ref(),
            None,
            WaitIdleOptions {
                timeout_ms: 10_000,
                poll_interval_ms: 25,
                stable_target: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("wait_idle stable_target must fail fast");
        assert!(
            t_idle.elapsed() < std::time::Duration::from_secs(2),
            "wait_idle({label}) stalled: {:?}",
            t_idle.elapsed()
        );
        if is_ref {
            assert!(
                matches!(err, MimikyuError::ElementNotFound(_)),
                "wait_idle({label}) expected ElementNotFound, got {err:?}"
            );
        } else {
            assert!(
                matches!(err, MimikyuError::InvalidConfig(_)),
                "wait_idle({label}) expected InvalidConfig, got {err:?}"
            );
        }
    }

    let empty_map = mimikyu_core::snapshot::RefMap::default();
    let t_ref_map = std::time::Instant::now();
    let err_ref = WaitEngine::wait_idle(
        session.as_ref(),
        Some(&empty_map),
        WaitIdleOptions {
            timeout_ms: 10_000,
            poll_interval_ms: 25,
            stable_target: Some("@e999".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect_err("wait_idle unknown @e999 with RefMap must fail fast");
    assert!(t_ref_map.elapsed() < std::time::Duration::from_secs(2));
    assert!(matches!(err_ref, MimikyuError::ElementNotFound(_)));

    let t_idle_batch = std::time::Instant::now();
    let bad_idle_batch = ActionBatcher::execute(
        session.as_ref(),
        &chokepoint,
        &mut snap_state,
        &[BatchAction::WaitIdle {
            timeout_ms: Some(10_000),
            network_quiet_ms: None,
            target: Some("div[[[".to_string()),
        }],
    )
    .await
    .expect("ActionBatcher returns BatchReport");
    assert!(!bad_idle_batch.ok);
    assert!(t_idle_batch.elapsed() < std::time::Duration::from_secs(2));
    assert!(
        bad_idle_batch
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("invalid CSS selector")),
        "unexpected batch wait_idle error: {:?}",
        bad_idle_batch.error
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

async fn assert_wait_text_open_shadow_dom(engine: EngineKind) {
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid");

    let html = r#"<!doctype html><html><body>
        <div id="open-host"></div>
        <div id="hidden-host" style="display:none"></div>
        <script>
            const sr = document.getElementById('open-host').attachShadow({ mode: 'open' });
            sr.innerHTML = `
                <style>/* SHADOW_STYLE_IGNORE */</style>
                <div style="display:none">SHADOW_HIDDEN_DISP</div>
                <div hidden>SHADOW_HIDDEN_ATTR</div>
                <div aria-hidden="true">SHADOW_HIDDEN_ARIA</div>
                <div style="visibility:hidden">SHADOW_HIDDEN_VIS</div>
                <p><span>SHADOW_</span><span>WAIT_OK</span></p>
                <div id="nested-open"></div>
            `;
            const nestedSr = sr.getElementById('nested-open').attachShadow({ mode: 'open' });
            nestedSr.innerHTML = '<p>NESTED_SHADOW_OK</p>';

            const hsr = document.getElementById('hidden-host').attachShadow({ mode: 'open' });
            hsr.innerHTML = '<p>HIDDEN_HOST_SHADOW</p>';
        </script>
    </body></html>"#;
    let data_url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session.navigate(&data_url).await.expect("navigate data");

    WaitEngine::wait_text(
        session.as_ref(),
        "SHADOW_WAIT_OK",
        WaitOptions {
            timeout_ms: 2_000,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect("wait_text must match visible open shadow DOM text");

    WaitEngine::wait_text(
        session.as_ref(),
        "NESTED_SHADOW_OK",
        WaitOptions {
            timeout_ms: 2_000,
            poll_interval_ms: 25,
        },
    )
    .await
    .expect("wait_text must match nested open shadow DOM text");

    for hidden_txt in [
        "SHADOW_STYLE_IGNORE",
        "SHADOW_HIDDEN_DISP",
        "SHADOW_HIDDEN_ATTR",
        "SHADOW_HIDDEN_ARIA",
        "SHADOW_HIDDEN_VIS",
        "HIDDEN_HOST_SHADOW",
    ] {
        let err = WaitEngine::wait_text(
            session.as_ref(),
            hidden_txt,
            WaitOptions {
                timeout_ms: 150,
                poll_interval_ms: 25,
            },
        )
        .await
        .expect_err("hidden shadow text must not match wait_text");
        assert!(
            matches!(err, MimikyuError::Timeout { .. }),
            "{engine} {hidden_txt}: expected Timeout, got {err:?}"
        );
    }

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn wait_text_open_shadow_dom_chromium() {
    assert_wait_text_open_shadow_dom(EngineKind::Chromium).await;
}

#[tokio::test]
async fn wait_text_open_shadow_dom_firefox() {
    assert_wait_text_open_shadow_dom(EngineKind::Firefox).await;
}

async fn assert_wait_idle_succeeds_when_raf_is_throttled(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch browser");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/target"))
        .await
        .expect("navigate /target");

    // Simulate background/multi-tab compositor throttling where requestAnimationFrame never fires.
    session
        .eval_isolated("window.requestAnimationFrame = () => 0; true")
        .await
        .expect("override rAF in isolated world");

    let t0 = std::time::Instant::now();
    WaitEngine::wait_idle(
        session.as_ref(),
        None,
        WaitIdleOptions {
            timeout_ms: 2_000,
            poll_interval_ms: 25,
            network_quiet_ms: 100,
            ..Default::default()
        },
    )
    .await
    .unwrap_or_else(|e| {
        panic!("{engine}: wait_idle timed out when requestAnimationFrame is throttled: {e}")
    });
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(1_500),
        "{engine}: wait_idle took too long ({:?}) with throttled rAF",
        t0.elapsed()
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn wait_idle_succeeds_when_raf_is_throttled_chromium() {
    assert_wait_idle_succeeds_when_raf_is_throttled(EngineKind::Chromium).await;
}

#[tokio::test]
async fn wait_idle_succeeds_when_raf_is_throttled_firefox() {
    assert_wait_idle_succeeds_when_raf_is_throttled(EngineKind::Firefox).await;
}
