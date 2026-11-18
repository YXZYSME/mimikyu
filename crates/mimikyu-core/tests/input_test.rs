mod support;

use std::sync::Arc;
use std::time::Duration;

use mimikyu_core::driver::{driver_for, EngineKind, LaunchOptions, ProfileMode, SelectBy};
use mimikyu_core::input::{
    ChokepointInput, ClickOptions, FillOptions, HumanizeConfig, InputOptions, InputTarget,
    ScrollOptions, TypeOptions,
};
use mimikyu_core::snapshot::{SnapshotEngine, SnapshotOptions, SnapshotState};
use mimikyu_core::MimikyuError;
use support::{assert_pgid_gone, TestServer};

async fn assert_chokepoint_and_humanize_parity(engine: EngineKind) {
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

    let input = Arc::new(ChokepointInput::new(session.clone()));

    // 1. Humanize trajectory & typing cadence vs non-humanized direct input
    session
        .navigate(&server.url("/humanize-tracker"))
        .await
        .expect("navigate /humanize-tracker");

    input
        .click(
            &InputTarget::Selector("#target-btn".into()),
            None,
            &ClickOptions {
                humanize: HumanizeConfig {
                    enabled: true,
                    seed: Some(123),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .expect("humanized click");

    let track_val = session
        .eval_isolated(
            "({ track: JSON.parse(document.documentElement.dataset.mouseTrack || '[]'), clicks: Number(document.documentElement.dataset.clickCount || '0'), w: window.innerWidth, h: window.innerHeight })",
        )
        .await
        .expect("read mouseTrack");
    assert_eq!(track_val["clicks"], 1);
    let pts = track_val["track"].as_array().expect("mouseTrack array");
    assert!(
        pts.len() > 2,
        "{engine}: humanize=true must emit >2 mousemove points, got {}",
        pts.len()
    );
    let w = track_val["w"].as_f64().unwrap();
    let h = track_val["h"].as_f64().unwrap();
    let x0 = pts.first().unwrap()["x"].as_f64().unwrap();
    let y0 = pts.first().unwrap()["y"].as_f64().unwrap();
    let x1 = pts.last().unwrap()["x"].as_f64().unwrap();
    let y1 = pts.last().unwrap()["y"].as_f64().unwrap();
    let chord_len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt().max(1.0);
    let mut max_perp = 0.0f64;
    for p in pts {
        let px = p["x"].as_f64().unwrap();
        let py = p["y"].as_f64().unwrap();
        assert!(
            px >= 0.0 && px < w && py >= 0.0 && py < h,
            "{engine}: point ({px},{py}) out of viewport ({w},{h})"
        );
        let area2 = ((x1 - x0) * (py - y0) - (y1 - y0) * (px - x0)).abs();
        max_perp = max_perp.max(area2 / chord_len);
    }
    assert!(
        max_perp > 1.0,
        "{engine}: humanized trajectory must be non-collinear (max_perp={max_perp})"
    );

    // Humanized typing cadence
    input
        .type_text(
            &InputTarget::Selector("#type-target".into()),
            None,
            "hello",
            &TypeOptions {
                humanize: HumanizeConfig {
                    enabled: true,
                    seed: Some(9),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .expect("humanized type_text");
    let typed = session
        .eval_isolated("document.getElementById('type-target').value")
        .await
        .expect("type-target value");
    assert_eq!(typed, "hello");

    // Direct non-humanized click (minimal points)
    session
        .navigate(&server.url("/humanize-tracker"))
        .await
        .expect("reload /humanize-tracker");
    input
        .click(
            &InputTarget::Selector("#target-btn".into()),
            None,
            &ClickOptions::default(),
        )
        .await
        .expect("direct click");
    let direct_track = session
        .eval_isolated("({ len: JSON.parse(document.documentElement.dataset.mouseTrack || '[]').length, clicks: Number(document.documentElement.dataset.clickCount || '0') })")
        .await
        .expect("direct track");
    assert_eq!(direct_track["clicks"], 1);
    assert!(
        direct_track["len"].as_u64().unwrap() <= 2,
        "{engine}: humanize=false should emit minimal moves, got {}",
        direct_track["len"]
    );

    // 2. Edge clamping (y=-1 wedge avoidance) + offscreen scroll-into-view + wheel-notch scrolling
    session
        .navigate(&server.url("/edge-and-scroll"))
        .await
        .expect("navigate /edge-and-scroll");

    for sel in ["#top-edge-btn", "#above-edge-btn", "#bottom-right-btn", "#offscreen-btn"] {
        input
            .click(&InputTarget::Selector(sel.into()), None, &ClickOptions::default())
            .await
            .unwrap_or_else(|e| panic!("{engine}: failed clicking {sel}: {e}"));
    }
    let hits = session
        .eval_isolated("JSON.parse(document.documentElement.dataset.edgeHits || '[]')")
        .await
        .expect("read __edgeHits");
    let hits_arr = hits.as_array().expect("__edgeHits array");
    assert_eq!(hits_arr.len(), 4, "{engine}: expected 4 edge/offscreen hits, got {hits_arr:?}");

    input
        .scroll(
            0.0,
            -320.0,
            None,
            None,
            &ScrollOptions {
                humanize: HumanizeConfig {
                    enabled: true,
                    seed: Some(5),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .expect("humanized wheel scroll");
    let wheel_count = session
        .eval_isolated("JSON.parse(document.documentElement.dataset.scrollEvents || '[]').length")
        .await
        .expect("scrollEvents len");
    assert!(
        wheel_count.as_u64().unwrap_or(0) >= 2,
        "{engine}: humanized scroll should emit multiple notches"
    );

    // 3. Clipboard-free fill, select_option, multi-file upload, and chorded shortcuts
    session
        .navigate(&server.url("/forms-rich"))
        .await
        .expect("navigate /forms-rich");

    input
        .fill(
            &InputTarget::Selector("#prefilled".into()),
            None,
            "fresh-value",
            &FillOptions::default(),
        )
        .await
        .expect("fill #prefilled");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('prefilled').value")
            .await
            .unwrap(),
        "fresh-value"
    );

    input
        .fill(
            &InputTarget::Selector("#notes".into()),
            None,
            "line1\nline2",
            &FillOptions::default(),
        )
        .await
        .expect("fill multiline textarea");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('notes').value")
            .await
            .unwrap(),
        "line1\nline2"
    );

    input
        .fill(
            &InputTarget::Selector("#ce".into()),
            None,
            "ce-updated",
            &FillOptions::default(),
        )
        .await
        .expect("fill contenteditable");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('ce').textContent")
            .await
            .unwrap(),
        "ce-updated"
    );

    assert_eq!(
        input
            .select_option(
                &InputTarget::Selector("#country".into()),
                None,
                SelectBy::Label("Japan".into()),
                0
            )
            .await
            .unwrap(),
        "jp"
    );
    assert_eq!(
        input
            .select_option(
                &InputTarget::Selector("#country".into()),
                None,
                SelectBy::Index(1),
                0
            )
            .await
            .unwrap(),
        "de"
    );
    let dis_err = input
        .select_option(
            &InputTarget::Selector("#country".into()),
            None,
            SelectBy::Value("dis".into()),
            0,
        )
        .await
        .expect_err("disabled option must error");
    assert!(matches!(dis_err, MimikyuError::ElementNotFound(_)));

    let f1 = tempfile::NamedTempFile::new().unwrap();
    let f2 = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f1.path(), b"one").unwrap();
    std::fs::write(f2.path(), b"two").unwrap();
    input
        .set_input_files(
            &InputTarget::Selector("#upload".into()),
            None,
            vec![f1.path().to_path_buf(), f2.path().to_path_buf()],
            0,
        )
        .await
        .expect("multi-file upload");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('upload').files.length")
            .await
            .unwrap(),
        2
    );

    // Chorded shortcuts (verify ctrlKey/altKey/shiftKey/metaKey & no literal text inserted into focused input)
    session
        .eval_isolated("const pf = document.getElementById('prefilled'); pf.value = 'sentinel'; pf.focus();")
        .await
        .unwrap();
    let baseline_chords = session
        .eval_isolated("JSON.parse(document.documentElement.dataset.chords || '[]').length")
        .await
        .unwrap()
        .as_u64()
        .unwrap_or(0) as usize;
    for chord in ["Control+a", "Alt+x", "Meta+k", "Control+Space"] {
        input
            .press(chord, &InputOptions::default())
            .await
            .unwrap_or_else(|e| panic!("{engine}: press({chord}) failed: {e}"));
        let val = session
            .eval_isolated("document.getElementById('prefilled').value")
            .await
            .expect("read #prefilled after chord");
        assert_eq!(
            val, "sentinel",
            "{engine}: chord {chord} must NOT insert literal characters into focused input"
        );
    }
    // Also verify Alt+x does not insert literal 'x' into a focused <textarea>
    session
        .eval_isolated("const ta = document.getElementById('notes'); ta.value = 'ta-sentinel'; ta.focus();")
        .await
        .unwrap();
    input
        .press("Alt+x", &InputOptions::default())
        .await
        .expect("press Alt+x in textarea");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('notes').value")
            .await
            .unwrap(),
        "ta-sentinel",
        "{engine}: Alt+x must NOT insert literal 'x' into focused textarea"
    );
    session
        .eval_isolated("document.getElementById('prefilled').focus();")
        .await
        .unwrap();
    for chord in ["Shift+a", "F1", "F12"] {
        input
            .press(chord, &InputOptions::default())
            .await
            .unwrap_or_else(|e| panic!("{engine}: press({chord}) failed: {e}"));
    }
    let chords = session
        .eval_isolated("JSON.parse(document.documentElement.dataset.chords || '[]')")
        .await
        .expect("read __chords");
    let chords_arr = &chords.as_array().expect("__chords array")[baseline_chords..];
    assert!(
        chords_arr
            .iter()
            .any(|c| c["ctrlKey"] == true && c["key"].as_str().map(|s| s.eq_ignore_ascii_case("a")).unwrap_or(false)),
        "{engine}: expected Control+a keydown: {chords_arr:?}"
    );
    assert!(
        chords_arr.iter().any(|c| {
            c["altKey"] == true
                && c["key"]
                    .as_str()
                    .map(|s| s.eq_ignore_ascii_case("x"))
                    .unwrap_or(false)
                && c["code"] == "KeyX"
        }),
        "{engine}: expected Alt+x keydown with code KeyX: {chords_arr:?}"
    );
    assert!(
        chords_arr.iter().any(|c| c["shiftKey"] == true && c["key"] == "A"),
        "{engine}: expected Shift+a keydown: {chords_arr:?}"
    );
    assert!(
        chords_arr
            .iter()
            .any(|c| c["metaKey"] == true && c["key"].as_str().map(|s| s.eq_ignore_ascii_case("k")).unwrap_or(false)),
        "{engine}: expected Meta+k keydown: {chords_arr:?}"
    );
    assert!(chords_arr.iter().any(|c| c["key"] == "F1"));
    assert!(chords_arr.iter().any(|c| c["key"] == "F12"));

    // 3b. Alt+x inside BOTH same-origin and cross-origin <iframe> <input> (no literal 'x' leak, events fired, post fill/type works)
    let chord_server2 = TestServer::start().await;
    let xo_chord_url = chord_server2.localhost_url("/xo-chord-child");
    session
        .navigate(&server.url(&format!("/iframe-chord-host?xo={xo_chord_url}")))
        .await
        .expect("navigate /iframe-chord-host");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut chord_snap_state = SnapshotState::default();
    let chord_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut chord_snap_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("capture /iframe-chord-host snapshot");

    let chord_frames = session.list_frames().await.expect("list_frames chord");
    let so_chord_fid = chord_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/so-chord-child"))
        .map(|f| f.frame_id.clone())
        .expect("so-chord-child frame");
    let xo_chord_fid = chord_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-chord-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-chord-child frame");

    let so_inp_ref = chord_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("SOChordInput"))
        .map(|(k, _)| k.clone())
        .expect("SOChordInput ref");
    let xo_inp_ref = chord_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("XOChordInput"))
        .map(|(k, _)| k.clone())
        .expect("XOChordInput ref");

    for (label, tgt_ref, fid, inp_id, expected_init) in [
        ("same-origin", &so_inp_ref, &so_chord_fid, "so-chord-inp", "so-sentinel"),
        ("cross-origin", &xo_inp_ref, &xo_chord_fid, "xo-chord-inp", "xo-sentinel"),
    ] {
        input
            .click(
                &InputTarget::Ref(tgt_ref.clone()),
                Some(&chord_snap_state.active_ref_map),
                &ClickOptions::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{engine}: click {label} iframe input failed: {e}"));
        input
            .press("Alt+x", &InputOptions::default())
            .await
            .unwrap_or_else(|e| panic!("{engine}: press Alt+x in {label} iframe failed: {e}"));

        let after_alt = session
            .eval_isolated_in_frame(
                &format!("document.getElementById('{inp_id}').value"),
                Some(fid),
            )
            .await
            .expect("read iframe input after Alt+x");
        assert_eq!(
            after_alt, expected_init,
            "{engine}: Alt+x inside {label} iframe must NOT insert literal 'x'"
        );

        let ifr_chords = session
            .eval_isolated_in_frame(
                "JSON.parse(document.documentElement.dataset.chords || '[]')",
                Some(fid),
            )
            .await
            .expect("read iframe chords");
        let ifr_arr = ifr_chords.as_array().expect("iframe chords array");
        assert!(
            ifr_arr.iter().any(|c| {
                c["type"] == "keydown"
                    && c["altKey"] == true
                    && c["key"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("x"))
                    && c["code"] == "KeyX"
            }),
            "{engine}: expected Alt+x keydown inside {label} iframe: {ifr_arr:?}"
        );
        assert!(
            ifr_arr.iter().any(|c| {
                c["type"] == "keyup"
                    && c["altKey"] == true
                    && c["key"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("x"))
                    && c["code"] == "KeyX"
            }),
            "{engine}: expected Alt+x keyup inside {label} iframe: {ifr_arr:?}"
        );

        input
            .fill(
                &InputTarget::Ref(tgt_ref.clone()),
                Some(&chord_snap_state.active_ref_map),
                "post-alt",
                &FillOptions::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{engine}: post-Alt+x fill in {label} iframe failed: {e}"));
        input
            .type_text(
                &InputTarget::Ref(tgt_ref.clone()),
                Some(&chord_snap_state.active_ref_map),
                "-typed",
                &TypeOptions::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{engine}: post-Alt+x type_text in {label} iframe failed: {e}"));
        let after_fill_type = session
            .eval_isolated_in_frame(
                &format!("document.getElementById('{inp_id}').value"),
                Some(fid),
            )
            .await
            .expect("read iframe input after fill+type");
        assert_eq!(after_fill_type, "post-alt-typed");
    }

    // 4. Concurrent serialized chokepoint stress (zero deadlock)
    session
        .navigate(&server.url("/forms-rich"))
        .await
        .expect("return to /forms-rich for concurrent stress");
    let i1 = Arc::clone(&input);
    let i2 = Arc::clone(&input);
    let i3 = Arc::clone(&input);
    let i4 = Arc::clone(&input);
    let inp_opts = InputOptions::default();
    let agree_tgt = InputTarget::Selector("#agree".into());
    let click_opts = ClickOptions::default();
    let notes_tgt = InputTarget::Selector("#notes".into());
    let type_opts = TypeOptions::default();
    let scroll_opts = ScrollOptions::default();
    let (r1, r2, r3, r4) = tokio::time::timeout(
        Duration::from_secs(15),
        async {
            tokio::join!(
                i1.press("Escape", &inp_opts),
                i2.click(&agree_tgt, None, &click_opts),
                i3.type_text(&notes_tgt, None, "-appended", &type_opts),
                i4.scroll(0.0, 50.0, None, None, &scroll_opts),
            )
        },
    )
    .await
    .expect("concurrent chokepoint join must not deadlock");
    r1.unwrap();
    r2.unwrap();
    r3.unwrap();
    r4.unwrap();

    // 5. Chromium closed shadow root click/fill via @eN
    if engine == EngineKind::Chromium {
        session
            .navigate(&server.url("/nested-frames-shadow"))
            .await
            .expect("navigate /nested-frames-shadow");
        let mut s_state = SnapshotState::default();
        let snap = SnapshotEngine::capture(
            session.as_ref(),
            &mut s_state,
            SnapshotOptions::default(),
        )
        .await
        .expect("capture shadow snapshot");

        let closed_inp_ref = snap
            .ref_map
            .iter()
            .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("ClosedInput"))
            .map(|(k, _)| k.clone())
            .expect("closed shadow textbox ref");
        let closed_btn_ref = snap
            .ref_map
            .iter()
            .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("ClosedShadowBtn"))
            .map(|(k, _)| k.clone())
            .expect("ClosedShadowBtn ref");

        input
            .fill(
                &InputTarget::Ref(closed_inp_ref),
                Some(&s_state.active_ref_map),
                "shadow-ok",
                &FillOptions::default(),
            )
            .await
            .expect("fill closed shadow input");
        input
            .click(
                &InputTarget::Ref(closed_btn_ref),
                Some(&s_state.active_ref_map),
                &ClickOptions::default(),
            )
            .await
            .expect("click closed shadow button");
        let st = session
            .eval_isolated("document.getElementById('shadow-status').textContent")
            .await
            .expect("shadow-status");
        assert_eq!(st, "closed-clicked:shadow-ok");
    }

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_input_chokepoint_and_humanize_chromium() {
    assert_chokepoint_and_humanize_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_input_chokepoint_and_humanize_firefox() {
    assert_chokepoint_and_humanize_parity(EngineKind::Firefox).await;
}

async fn assert_iframe_focus_transitions(engine: EngineKind) {
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
    let input = ChokepointInput::new(session.clone());

    // 1. iframe -> main click via InputTarget::Selector (no frame_id)
    session
        .navigate(&server.url("/iframe-focus-host"))
        .await
        .expect("navigate /iframe-focus-host");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut f_state = SnapshotState::default();
    let f_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut f_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("snapshot /iframe-focus-host");
    let so_inp_ref = f_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("SOChordInput"))
        .map(|(k, _)| k.clone())
        .expect("SOChordInput ref");
    let f_frames = session.list_frames().await.expect("list_frames focus");
    let so_fid = f_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/so-chord-child"))
        .map(|f| f.frame_id.clone())
        .expect("so-chord-child frame");

    input
        .click(
            &InputTarget::Ref(so_inp_ref.clone()),
            Some(&f_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click SOChordInput");
    assert_eq!(
        session
            .eval_isolated_in_frame("document.activeElement && document.activeElement.id", Some(&so_fid))
            .await
            .expect("so activeElement"),
        "so-chord-inp"
    );

    // Transition from iframe back to main frame using InputTarget::Selector (frame_id == None)
    input
        .click(
            &InputTarget::Selector("#main-inp".into()),
            None,
            &ClickOptions::default(),
        )
        .await
        .expect("click #main-inp via Selector");
    assert_eq!(
        session
            .eval_isolated("document.activeElement && document.activeElement.id")
            .await
            .expect("main activeElement after Selector click"),
        "main-inp",
        "{engine}: iframe -> main click via InputTarget::Selector must focus #main-inp"
    );
    input
        .press("Alt+x", &InputOptions::default())
        .await
        .expect("press Alt+x in #main-inp");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('main-inp').value")
            .await
            .expect("main-inp value"),
        "main-init"
    );
    let main_chords = session
        .eval_isolated("JSON.parse(document.documentElement.dataset.chords || '[]')")
        .await
        .expect("main chords");
    let main_arr = main_chords.as_array().expect("main chords array");
    assert!(
        main_arr
            .iter()
            .any(|c| c["type"] == "keydown" && c["altKey"] == true && c["code"] == "KeyX"),
        "{engine}: expected Alt+x keydown in main frame: {main_arr:?}"
    );
    assert!(
        main_arr
            .iter()
            .any(|c| c["type"] == "keyup" && c["altKey"] == true && c["code"] == "KeyX"),
        "{engine}: expected Alt+x keyup in main frame: {main_arr:?}"
    );

    // 2. Nested 2-level iframe cross-focus: top -> #outer-ifr -> {#inner-1, #inner-2} + Alt+x
    session
        .navigate(&server.url("/nested-iframe-focus-host"))
        .await
        .expect("navigate /nested-iframe-focus-host");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut n_state = SnapshotState::default();
    let n_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut n_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("snapshot /nested-iframe-focus-host");
    let n_frames = session.list_frames().await.expect("list_frames nested focus");
    let outer_fid = n_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/nested-iframe-focus-outer"))
        .map(|f| f.frame_id.clone())
        .expect("outer focus frame");
    let in1_fid = n_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/nested-iframe-focus-inner1"))
        .map(|f| f.frame_id.clone())
        .expect("inner1 focus frame");
    let in2_fid = n_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/nested-iframe-focus-inner2"))
        .map(|f| f.frame_id.clone())
        .expect("inner2 focus frame");

    let in1_ref = n_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("Inner1Input"))
        .map(|(k, _)| k.clone())
        .expect("Inner1Input ref");
    let in2_ref = n_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("Inner2Input"))
        .map(|(k, _)| k.clone())
        .expect("Inner2Input ref");
    let outer_btn_ref = n_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("OuterFocusBtn"))
        .map(|(k, _)| k.clone())
        .expect("OuterFocusBtn ref");

    input
        .click(
            &InputTarget::Ref(in1_ref.clone()),
            Some(&n_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click Inner1Input");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&in1_fid),
            )
            .await
            .expect("in1 activeElement"),
        "in1-inp"
    );
    input
        .press("Alt+x", &InputOptions::default())
        .await
        .expect("press Alt+x in Inner1Input");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('in1-inp').value",
                Some(&in1_fid),
            )
            .await
            .expect("in1 value"),
        "in1-sentinel"
    );

    // Cross-focus from sibling nested iframe inner-1 -> inner-2
    input
        .click(
            &InputTarget::Ref(in2_ref.clone()),
            Some(&n_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click Inner2Input");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&in2_fid),
            )
            .await
            .expect("in2 activeElement"),
        "in2-inp",
        "{engine}: nested 2-level iframe cross-focus must focus #in2-inp"
    );
    let in2_fevs = session
        .eval_isolated_in_frame(
            "JSON.parse(document.documentElement.dataset.fevs || '[]')",
            Some(&in2_fid),
        )
        .await
        .expect("in2 fevs");
    let in2_fevs_arr = in2_fevs.as_array().expect("in2 fevs array");
    assert!(
        in2_fevs_arr.iter().any(|v| v == "focus:in2-inp"),
        "{engine}: expected focus:in2-inp event in inner-2: {in2_fevs_arr:?}"
    );

    input
        .press("Alt+x", &InputOptions::default())
        .await
        .expect("press Alt+x in Inner2Input");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('in2-inp').value",
                Some(&in2_fid),
            )
            .await
            .expect("in2 value"),
        "in2-sentinel"
    );
    let in2_chords = session
        .eval_isolated_in_frame(
            "JSON.parse(document.documentElement.dataset.chords || '[]')",
            Some(&in2_fid),
        )
        .await
        .expect("in2 chords");
    let in2_arr = in2_chords.as_array().expect("in2 chords array");
    assert!(
        in2_arr
            .iter()
            .any(|c| c["type"] == "keydown" && c["altKey"] == true && c["code"] == "KeyX"),
        "{engine}: expected Alt+x keydown in inner-2: {in2_arr:?}"
    );
    assert!(
        in2_arr
            .iter()
            .any(|c| c["type"] == "keyup" && c["altKey"] == true && c["code"] == "KeyX"),
        "{engine}: expected Alt+x keyup in inner-2: {in2_arr:?}"
    );

    // Transition from inner-2 back up to parent outer iframe button
    input
        .click(
            &InputTarget::Ref(outer_btn_ref),
            Some(&n_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click OuterFocusBtn");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('outer-status').textContent",
                Some(&outer_fid),
            )
            .await
            .expect("outer-status"),
        "outer-clicked"
    );

    // Humanized fill transition from inner-1 -> inner-2 without separate click()
    let hum_fill = FillOptions {
        humanize: HumanizeConfig {
            enabled: true,
            seed: Some(7),
            duration_ms: Some(100),
            key_delay_mean_ms: 10.0,
            key_delay_std_ms: 5.0,
            ..Default::default()
        },
        ack_timeout_ms: 0,
    };
    input
        .fill(
            &InputTarget::Ref(in1_ref),
            Some(&n_state.active_ref_map),
            "h1",
            &hum_fill,
        )
        .await
        .expect("humanize fill inner-1");
    input
        .fill(
            &InputTarget::Ref(in2_ref),
            Some(&n_state.active_ref_map),
            "h2",
            &hum_fill,
        )
        .await
        .expect("humanize fill inner-2");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('in2-inp').value",
                Some(&in2_fid),
            )
            .await
            .expect("in2 humanized value"),
        "h2"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_iframe_focus_transitions_chromium() {
    assert_iframe_focus_transitions(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_iframe_focus_transitions_firefox() {
    assert_iframe_focus_transitions(EngineKind::Firefox).await;
}

async fn assert_closed_shadow_iframe_bounds_and_focus(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
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
    let input = ChokepointInput::new(session.clone());

    let xo_child = server2.localhost_url("/xo-chord-child");
    let host_url = server.url(&format!("/shadow-iframe-focus-host?xo={xo_child}"));
    session
        .navigate(&host_url)
        .await
        .expect("navigate /shadow-iframe-focus-host");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut state = SnapshotState::default();
    let snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot /shadow-iframe-focus-host");

    let frames = session.list_frames().await.expect("list_frames shadow");
    let so2_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/so-chord-child-2"))
        .map(|f| f.frame_id.clone())
        .expect("so-chord-child-2 frame");
    let xo_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-chord-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-chord-child frame");

    let so2_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("SO2ChordInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("SO2ChordInput ref");
    let so2_bounds = so2_entry.1.bounds.expect("SO2ChordInput bounds");
    // #closed-sh-ifr sits below #open-sh-ifr (90px) and #main-after-sh, so viewport y must exceed 100px
    assert!(
        so2_bounds.y > 100.0,
        "{engine}: closed-shadow-DOM iframe input bounds must include iframe viewport offset, got {so2_bounds:?}"
    );

    let xo_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("XOChordInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("XOChordInput ref");
    let xo_bounds = xo_entry.1.bounds.expect("XOChordInput bounds");
    assert!(
        xo_bounds.y > so2_bounds.y + 50.0,
        "{engine}: cross-origin closed-shadow-DOM iframe input bounds must include iframe viewport offset, got {xo_bounds:?} vs {so2_bounds:?}"
    );

    // 1. Click @eN inside same-origin closed-shadow-DOM iframe
    input
        .click(
            &InputTarget::Ref(so2_entry.0.clone()),
            Some(&state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click SO2ChordInput inside closed shadow root");
    assert_eq!(
        session
            .eval_isolated_in_frame("document.activeElement && document.activeElement.id", Some(&so2_fid))
            .await
            .expect("so2 activeElement"),
        "so2-chord-inp",
        "{engine}: click(@eN) must focus #so2-chord-inp inside closed-shadow-DOM iframe"
    );
    let fevs = session
        .eval_isolated_in_frame("JSON.parse(document.documentElement.dataset.fevs || '[]')", Some(&so2_fid))
        .await
        .expect("so2 fevs");
    assert!(
        fevs.as_array()
            .is_some_and(|a| a.iter().any(|v| v == "click:so2-chord-inp")),
        "{engine}: expected click:so2-chord-inp inside closed-shadow-DOM iframe: {fevs:?}"
    );

    // 2. Transition from closed-shadow-DOM iframe -> main input via InputTarget::Selector
    input
        .click(
            &InputTarget::Selector("#main-after-sh".into()),
            None,
            &ClickOptions::default(),
        )
        .await
        .expect("click #main-after-sh from closed-shadow-DOM iframe");
    assert_eq!(
        session
            .eval_isolated("document.activeElement && document.activeElement.id")
            .await
            .expect("main activeElement after closed-shadow iframe"),
        "main-after-sh",
        "{engine}: closed-shadow iframe -> main Selector click must focus #main-after-sh"
    );
    input
        .press("Alt+x", &InputOptions::default())
        .await
        .expect("press Alt+x in #main-after-sh");
    let mchords = session
        .eval_isolated("JSON.parse(document.documentElement.dataset.chords || '[]')")
        .await
        .expect("main chords");
    assert!(
        mchords
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c["type"] == "keydown" && c["altKey"] == true && c["code"] == "KeyX")),
        "{engine}: expected Alt+x in main after closed-shadow iframe transition: {mchords:?}"
    );

    // 3. Click @eN inside cross-origin closed-shadow-DOM iframe, then non-humanized fill back to main
    input
        .click(
            &InputTarget::Ref(xo_entry.0.clone()),
            Some(&state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click XOChordInput inside closed shadow root");
    assert_eq!(
        session
            .eval_isolated_in_frame("document.activeElement && document.activeElement.id", Some(&xo_fid))
            .await
            .expect("xo activeElement"),
        "xo-chord-inp",
        "{engine}: click(@eN) must focus #xo-chord-inp inside cross-origin closed-shadow iframe"
    );

    input
        .fill(
            &InputTarget::Selector("#main-after-sh".into()),
            None,
            "after-xo-closed",
            &FillOptions::default(),
        )
        .await
        .expect("non-humanized fill #main-after-sh from xo closed-shadow iframe");
    assert_eq!(
        session
            .eval_isolated("document.activeElement && document.activeElement.id")
            .await
            .expect("main activeElement after non-humanized fill"),
        "main-after-sh"
    );
    assert_eq!(
        session
            .eval_isolated("document.getElementById('main-after-sh').value")
            .await
            .expect("main-after-sh value"),
        "after-xo-closed"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_closed_shadow_iframe_bounds_and_focus_chromium() {
    assert_closed_shadow_iframe_bounds_and_focus(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_closed_shadow_iframe_bounds_and_focus_firefox() {
    assert_closed_shadow_iframe_bounds_and_focus(EngineKind::Firefox).await;
}

async fn assert_non_humanized_regular_so_iframe_type_and_fill(engine: EngineKind) {
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
    let input = ChokepointInput::new(session.clone());

    session
        .navigate(&server.url("/regular-so-iframe-host"))
        .await
        .expect("navigate /regular-so-iframe-host");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut state = SnapshotState::default();
    let snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot /regular-so-iframe-host");

    let frames = session.list_frames().await.expect("list_frames regular so");
    let so1_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/so-chord-child") && !f.url.contains("/so-chord-child-2"))
        .map(|f| f.frame_id.clone())
        .expect("so-chord-child frame");
    let so2_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/so-chord-child-2"))
        .map(|f| f.frame_id.clone())
        .expect("so-chord-child-2 frame");

    let so1_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("SOChordInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("SOChordInput ref");
    let so2_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("SO2ChordInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("SO2ChordInput ref");
    let main_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("MainFocusInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("MainFocusInput ref");

    assert_eq!(
        so1_entry.1.frame_id.as_deref(),
        Some(so1_fid.as_str()),
        "{engine}: regular same-origin iframe ref must resolve to real child frame_id"
    );
    assert_eq!(
        so2_entry.1.frame_id.as_deref(),
        Some(so2_fid.as_str()),
        "{engine}: id-less same-origin iframe ref must resolve to real child frame_id"
    );

    // 1. Focus #main-inp, then non-humanized type_text into #reg-so-ifr @eN
    input
        .click(
            &InputTarget::Selector("#main-inp".into()),
            None,
            &ClickOptions::default(),
        )
        .await
        .expect("click #main-inp");
    input
        .type_text(
            &InputTarget::Ref(so1_entry.0.clone()),
            Some(&state.active_ref_map),
            "-reg",
            &TypeOptions::default(),
        )
        .await
        .expect("non-humanized type_text into SOChordInput");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&so1_fid),
            )
            .await
            .expect("so1 activeElement after type_text"),
        "so-chord-inp",
        "{engine}: non-humanized type_text must focus #so-chord-inp inside same-origin iframe"
    );
    let so1_val = session
        .eval_isolated_in_frame("document.getElementById('so-chord-inp').value", Some(&so1_fid))
        .await
        .expect("so1 value after type_text");
    let so1_val_str = so1_val.as_str().unwrap_or_default();
    assert!(
        so1_val_str.contains("so-sentinel") && so1_val_str.contains("-reg"),
        "{engine}: non-humanized type_text must preserve existing text and insert '-reg', got '{so1_val_str}'"
    );
    let main_val_after_so1_type = session
        .eval_isolated("document.getElementById('main-inp').value")
        .await
        .expect("main-inp value after so1 type");
    assert_eq!(
        main_val_after_so1_type, "main-init",
        "{engine}: typed '-reg' must NOT leak into #main-inp"
    );

    // Alt+x chord right after non-humanized type_text must go to so1_fid, not main
    input
        .press("Alt+x", &InputOptions::default())
        .await
        .expect("press Alt+x in so1 after type_text");
    let so1_chords = session
        .eval_isolated_in_frame(
            "JSON.parse(document.documentElement.dataset.chords || '[]')",
            Some(&so1_fid),
        )
        .await
        .expect("so1 chords");
    assert!(
        so1_chords
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c["type"] == "keydown" && c["altKey"] == true && c["code"] == "KeyX")),
        "{engine}: expected Alt+x inside so1 after non-humanized type_text: {so1_chords:?}"
    );

    // 2. Focus #main-inp, then non-humanized fill into #reg-so-ifr @eN
    input
        .click(
            &InputTarget::Selector("#main-inp".into()),
            None,
            &ClickOptions::default(),
        )
        .await
        .expect("click #main-inp before fill");
    input
        .fill(
            &InputTarget::Ref(so1_entry.0.clone()),
            Some(&state.active_ref_map),
            "nh-fill-reg",
            &FillOptions::default(),
        )
        .await
        .expect("non-humanized fill into SOChordInput");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&so1_fid),
            )
            .await
            .expect("so1 activeElement after fill"),
        "so-chord-inp"
    );
    assert_eq!(
        session
            .eval_isolated_in_frame("document.getElementById('so-chord-inp').value", Some(&so1_fid))
            .await
            .expect("so1 value after fill"),
        "nh-fill-reg"
    );

    // 3. Focus #main-inp, then non-humanized type_text & fill into id-less same-origin iframe @eN
    input
        .click(
            &InputTarget::Selector("#main-inp".into()),
            None,
            &ClickOptions::default(),
        )
        .await
        .expect("click #main-inp before id-less iframe");
    input
        .type_text(
            &InputTarget::Ref(so2_entry.0.clone()),
            Some(&state.active_ref_map),
            "-idless",
            &TypeOptions::default(),
        )
        .await
        .expect("non-humanized type_text into id-less SO2ChordInput");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&so2_fid),
            )
            .await
            .expect("so2 activeElement after type_text"),
        "so2-chord-inp"
    );
    let so2_val = session
        .eval_isolated_in_frame("document.getElementById('so2-chord-inp').value", Some(&so2_fid))
        .await
        .expect("so2 value after type_text");
    let so2_val_str = so2_val.as_str().unwrap_or_default();
    assert!(
        so2_val_str.contains("so2-sentinel") && so2_val_str.contains("-idless"),
        "{engine}: id-less iframe non-humanized type_text got '{so2_val_str}'"
    );

    input
        .fill(
            &InputTarget::Ref(so2_entry.0),
            Some(&state.active_ref_map),
            "nh-fill-so2",
            &FillOptions::default(),
        )
        .await
        .expect("non-humanized fill into id-less SO2ChordInput");
    assert_eq!(
        session
            .eval_isolated_in_frame("document.getElementById('so2-chord-inp').value", Some(&so2_fid))
            .await
            .expect("so2 value after fill"),
        "nh-fill-so2"
    );

    // 4. Transition back from iframe -> main via non-humanized type_text and fill
    input
        .type_text(
            &InputTarget::Ref(main_entry.0.clone()),
            Some(&state.active_ref_map),
            "-back",
            &TypeOptions::default(),
        )
        .await
        .expect("non-humanized type_text back to main");
    assert_eq!(
        session
            .eval_isolated("document.activeElement && document.activeElement.id")
            .await
            .expect("main activeElement after type_text"),
        "main-inp"
    );
    let main_val = session
        .eval_isolated("document.getElementById('main-inp').value")
        .await
        .expect("main value after type_text");
    let main_val_str = main_val.as_str().unwrap_or_default();
    assert!(
        main_val_str.contains("main-init") && main_val_str.contains("-back"),
        "{engine}: main non-humanized type_text got '{main_val_str}'"
    );
    input
        .fill(
            &InputTarget::Ref(main_entry.0),
            Some(&state.active_ref_map),
            "main-done",
            &FillOptions::default(),
        )
        .await
        .expect("non-humanized fill back to main");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('main-inp').value")
            .await
            .expect("main value after fill"),
        "main-done"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_non_humanized_regular_so_iframe_type_and_fill_chromium() {
    assert_non_humanized_regular_so_iframe_type_and_fill(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_non_humanized_regular_so_iframe_type_and_fill_firefox() {
    assert_non_humanized_regular_so_iframe_type_and_fill(EngineKind::Firefox).await;
}

async fn assert_inline_so_iframe_upload(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
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
    let input = ChokepointInput::new(session.clone());

    let xo_outer = server2.localhost_url("/xo-upload-outer");
    let host_url = server.url(&format!("/so-upload-host?xo={xo_outer}"));
    session
        .navigate(&host_url)
        .await
        .expect("navigate /so-upload-host");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut state = SnapshotState::default();
    let snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot /so-upload-host");

    let frames = session.list_frames().await.expect("list_frames upload");
    let cases = [
        ("withid", "Upload-withid", "tag=withid"),
        ("noid", "Upload-noid", "tag=noid"),
        ("nested2", "Upload-nested2", "tag=nested2"),
        ("xoso", "Upload-xoso", "tag=xoso"),
    ];

    let tmp_file = tempfile::NamedTempFile::new().expect("temp upload file");
    std::fs::write(tmp_file.path(), b"so-iframe-upload-payload").expect("write temp file");
    let expected_name = tmp_file
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    for (tag, label, url_substr) in cases {
        let fid = frames
            .iter()
            .find(|f| !f.is_main && f.url.contains(url_substr))
            .map(|f| f.frame_id.clone())
            .unwrap_or_else(|| panic!("{engine}: missing frame for {url_substr}: {frames:?}"));
        let (ref_id, entry) = snap
            .ref_map
            .iter()
            .find(|(_, e)| e.name.as_deref() == Some(label))
            .map(|(k, e)| (k.clone(), e.clone()))
            .unwrap_or_else(|| panic!("{engine}: missing ref for {label}"));
        assert_eq!(
            entry.frame_id.as_deref(),
            Some(fid.as_str()),
            "{engine}: {label} frame_id mismatch"
        );

        input
            .set_input_files(
                &InputTarget::Ref(ref_id),
                Some(&state.active_ref_map),
                vec![tmp_file.path().to_path_buf()],
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("{engine}: set_input_files failed for {label}: {e}"));

        let check = session
            .eval_isolated_in_frame(
                &format!(
                    "(() => {{ const el = document.getElementById('file-inp-{tag}'); return {{ len: el.files.length, name: el.files[0] ? el.files[0].name : '' }}; }})()"
                ),
                Some(&fid),
            )
            .await
            .unwrap_or_else(|e| panic!("{engine}: eval files for {label}: {e}"));
        assert_eq!(check["len"], 1, "{engine}: {label} files.length must be 1");
        assert_eq!(
            check["name"], expected_name,
            "{engine}: {label} filename mismatch"
        );
    }

    // Also verify selector + frame_id upload path directly
    let withid_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("tag=withid"))
        .map(|f| f.frame_id.clone())
        .expect("withid fid");
    let tmp_file2 = tempfile::NamedTempFile::new().expect("temp upload file 2");
    std::fs::write(tmp_file2.path(), b"selector-frame-upload").expect("write temp file 2");
    let expected_name2 = tmp_file2
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    session
        .set_file_input_files(
            None,
            Some(&withid_fid),
            Some("#file-inp-withid"),
            vec![tmp_file2.path().to_path_buf()],
        )
        .await
        .expect("selector + frame_id set_file_input_files");
    let sel_check = session
        .eval_isolated_in_frame(
            "(() => { const el = document.getElementById('file-inp-withid'); return { len: el.files.length, name: el.files[0] ? el.files[0].name : '' }; })()",
            Some(&withid_fid),
        )
        .await
        .expect("eval selector upload");
    assert_eq!(sel_check["len"], 1);
    assert_eq!(sel_check["name"], expected_name2);

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_inline_so_iframe_upload_chromium() {
    assert_inline_so_iframe_upload(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_inline_so_iframe_upload_firefox() {
    assert_inline_so_iframe_upload(EngineKind::Firefox).await;
}

async fn assert_scaled_shadow_and_regular_iframe_click(engine: EngineKind) {
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
    let input = ChokepointInput::new(session.clone());

    session
        .navigate(&server.url("/shadow-scale-host"))
        .await
        .expect("navigate /shadow-scale-host");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut state = SnapshotState::default();
    let snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot /shadow-scale-host");

    let frames = session.list_frames().await.expect("list_frames scale");
    let cs_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.ends_with("/scaled-child"))
        .map(|f| f.frame_id.clone())
        .expect("scaled-child frame");
    let reg_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.ends_with("/scaled-child-reg"))
        .map(|f| f.frame_id.clone())
        .expect("scaled-child-reg frame");

    let cs_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("ScaledInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("ScaledInput ref");
    let reg_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("ScaledRegInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("ScaledRegInput ref");

    let cs_bounds = cs_entry.1.bounds.expect("ScaledInput bounds");
    let reg_bounds = reg_entry.1.bounds.expect("ScaledRegInput bounds");
    assert!(
        cs_bounds.x > 250.0 && cs_bounds.width > 120.0,
        "{engine}: scaled closed-shadow iframe input bounds must include 1.6x scale, got {cs_bounds:?}"
    );
    assert!(
        reg_bounds.x > 240.0 && reg_bounds.width > 110.0,
        "{engine}: scaled regular iframe input bounds must include 1.5x scale, got {reg_bounds:?}"
    );
    let cs_live = session
        .resolve_node_box(
            cs_entry.1.backend_node_id.expect("cs bid"),
            cs_entry.1.frame_id.as_deref(),
        )
        .await
        .expect("cs live box");
    assert!(
        (cs_bounds.x - cs_live.x).abs() <= 2.0
            && (cs_bounds.y - cs_live.y).abs() <= 2.0
            && (cs_bounds.width - cs_live.width).abs() <= 2.0
            && (cs_bounds.height - cs_live.height).abs() <= 2.0,
        "{engine}: closed-shadow snap bounds {cs_bounds:?} != live {cs_live:?}"
    );
    let reg_live = session
        .resolve_node_box(
            reg_entry.1.backend_node_id.expect("reg bid"),
            reg_entry.1.frame_id.as_deref(),
        )
        .await
        .expect("reg live box");
    assert!(
        (reg_bounds.x - reg_live.x).abs() <= 2.0
            && (reg_bounds.y - reg_live.y).abs() <= 2.0
            && (reg_bounds.width - reg_live.width).abs() <= 2.0
            && (reg_bounds.height - reg_live.height).abs() <= 2.0,
        "{engine}: regular scaled snap bounds {reg_bounds:?} != live {reg_live:?}"
    );

    input
        .click(
            &InputTarget::Ref(cs_entry.0),
            Some(&state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click ScaledInput in closed-shadow scaled iframe");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&cs_fid),
            )
            .await
            .expect("scaled-child activeElement"),
        "scaled-inp",
        "{engine}: click(@eN) must focus #scaled-inp inside scale(1.6) closed-shadow iframe"
    );

    input
        .click(
            &InputTarget::Ref(reg_entry.0),
            Some(&state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click ScaledRegInput in regular scaled iframe");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&reg_fid),
            )
            .await
            .expect("scaled-child-reg activeElement"),
        "scaled-reg-inp",
        "{engine}: click(@eN) must focus #scaled-reg-inp inside scale(1.5) regular iframe"
    );

    // Non-uniform scale(1.4, 0.8) and nested 2-level scale(1.5) -> scale(0.8)
    session
        .navigate(&server.url("/scaled-complex-host"))
        .await
        .expect("navigate /scaled-complex-host");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut cstate = SnapshotState::default();
    let csnap = SnapshotEngine::capture(
        session.as_ref(),
        &mut cstate,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot /scaled-complex-host");
    let cframes = session.list_frames().await.expect("list_frames complex");
    let nu_fid = cframes
        .iter()
        .find(|f| !f.is_main && f.url.ends_with("/scaled-child-nonuniform"))
        .map(|f| f.frame_id.clone())
        .expect("nonuniform frame");
    let nest_fid = cframes
        .iter()
        .find(|f| !f.is_main && f.url.ends_with("/scaled-nested-inner"))
        .map(|f| f.frame_id.clone())
        .expect("nested inner frame");

    let nu_entry = csnap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("NonUniformScaledInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("NonUniformScaledInput ref");
    let nu_bounds = nu_entry.1.bounds.expect("NonUniformScaledInput bounds");
    assert!(
        (nu_bounds.width - 112.0).abs() <= 2.0 && (nu_bounds.height - 19.2).abs() <= 2.0,
        "{engine}: non-uniform scale(1.4, 0.8) bounds expected ~112x19.2, got {nu_bounds:?}"
    );
    input
        .click(
            &InputTarget::Ref(nu_entry.0),
            Some(&cstate.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click NonUniformScaledInput");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&nu_fid),
            )
            .await
            .expect("nonuniform activeElement"),
        "scaled-nonuniform-inp",
        "{engine}: click(@eN) must focus #scaled-nonuniform-inp inside scale(1.4, 0.8)"
    );

    let nest_entry = csnap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("NestedScaledInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("NestedScaledInput ref");
    let nest_bounds = nest_entry.1.bounds.expect("NestedScaledInput bounds");
    assert!(
        (nest_bounds.width - 120.0).abs() <= 2.5 && (nest_bounds.height - 36.0).abs() <= 2.0,
        "{engine}: nested scale(1.5)->scale(0.8) bounds expected ~120x36, got {nest_bounds:?}"
    );
    input
        .click(
            &InputTarget::Ref(nest_entry.0),
            Some(&cstate.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click NestedScaledInput");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&nest_fid),
            )
            .await
            .expect("nested activeElement"),
        "scaled-nested-inp",
        "{engine}: click(@eN) must focus #scaled-nested-inp inside nested scale(1.5)->scale(0.8)"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_scaled_shadow_and_regular_iframe_click_chromium() {
    assert_scaled_shadow_and_regular_iframe_click(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_scaled_shadow_and_regular_iframe_click_firefox() {
    assert_scaled_shadow_and_regular_iframe_click(EngineKind::Firefox).await;
}

async fn assert_scaled_cross_origin_iframe_click(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let extra_args = if engine == EngineKind::Chromium {
        vec!["--site-per-process".to_string()]
    } else {
        vec![]
    };
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            extra_args,
            ..Default::default()
        })
        .await
        .expect("launch browser");
    let pid = session.info().pid.expect("pid");
    let input = ChokepointInput::new(session.clone());

    let xo_url = server2.localhost_url("/xo-scaled-child");
    session
        .navigate(&server.url(&format!("/scaled-xo-host?xo={xo_url}")))
        .await
        .expect("navigate /scaled-xo-host");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut state = SnapshotState::default();
    let snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot /scaled-xo-host");

    let frames = session.list_frames().await.expect("list_frames xo scale");
    let xo_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-scaled-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-scaled-child frame");

    let xo_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("XOScaledInput"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("XOScaledInput ref");
    let xo_bounds = xo_entry.1.bounds.expect("XOScaledInput snap bounds");
    let xo_live = session
        .resolve_node_box(
            xo_entry.1.backend_node_id.expect("xo bid"),
            xo_entry.1.frame_id.as_deref(),
        )
        .await
        .expect("XOScaledInput live box");
    assert!(
        (xo_bounds.x - 372.8).abs() <= 2.0
            && (xo_bounds.y - 102.8).abs() <= 2.0
            && (xo_bounds.width - 96.0).abs() <= 2.0
            && (xo_bounds.height - 38.4).abs() <= 2.0,
        "{engine}: cross-origin scale(1.6) snap bounds expected ~(372.8, 102.8, 96.0, 38.4), got {xo_bounds:?}"
    );
    assert!(
        (xo_bounds.x - xo_live.x).abs() <= 2.0
            && (xo_bounds.y - xo_live.y).abs() <= 2.0
            && (xo_bounds.width - xo_live.width).abs() <= 2.0
            && (xo_bounds.height - xo_live.height).abs() <= 2.0,
        "{engine}: cross-origin snap bounds {xo_bounds:?} != live {xo_live:?}"
    );

    input
        .click(
            &InputTarget::Ref(xo_entry.0),
            Some(&state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click XOScaledInput");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.activeElement && document.activeElement.id",
                Some(&xo_fid),
            )
            .await
            .expect("xo-scaled-child activeElement"),
        "xo-scaled-inp",
        "{engine}: click(@eN) must focus #xo-scaled-inp inside cross-origin scale(1.6) OOPIF"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_scaled_cross_origin_iframe_click_chromium() {
    assert_scaled_cross_origin_iframe_click(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_scaled_cross_origin_iframe_click_firefox() {
    assert_scaled_cross_origin_iframe_click(EngineKind::Firefox).await;
}
