mod support;

use mimikyu_core::driver::{driver_for, EngineKind, LaunchOptions, ProfileMode, SelectBy};
use mimikyu_core::input::{
    ChokepointInput, ClickOptions, FillOptions, InputTarget,
};
use mimikyu_core::snapshot::{SnapshotEngine, SnapshotOptions, SnapshotState};
use support::{assert_pgid_gone, TestServer};

async fn assert_snapshot_and_diff_parity(engine: EngineKind) {
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
        .navigate(&server.url("/forms-rich"))
        .await
        .expect("navigate /forms-rich");

    let mut snap_state = SnapshotState::default();
    let full = SnapshotEngine::capture(
        session.as_ref(),
        &mut snap_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("capture full snapshot");

    assert!(full.ref_count >= 5, "expected >=5 refs, got {}", full.ref_count);
    assert!(
        !full.text.contains("HiddenCss") && !full.text.contains("AriaHidden"),
        "{engine}: display:none and aria-hidden nodes must be pruned:\n{}",
        full.text
    );
    assert!(
        !full.text.contains("ZeroRect"),
        "{engine}: 0x0 node must be pruned:\n{}",
        full.text
    );
    assert!(
        full.text.contains("ContentsBtn"),
        "{engine}: display:contents child must be preserved:\n{}",
        full.text
    );
    assert!(
        full.text.contains("Rich Form"),
        "{engine}: heading should appear in full snapshot:\n{}",
        full.text
    );

    // interactive_only & max_depth
    let mut inter_state = SnapshotState::default();
    let inter = SnapshotEngine::capture(
        session.as_ref(),
        &mut inter_state,
        SnapshotOptions {
            interactive_only: true,
            ..Default::default()
        },
    )
    .await
    .expect("interactive_only snapshot");
    assert!(
        !inter.text.contains("Rich Form"),
        "interactive_only should exclude static heading:\n{}",
        inter.text
    );
    assert!(inter.text.starts_with("@e1 "), "must number from @e1");

    let mut depth_state = SnapshotState::default();
    let depth0 = SnapshotEngine::capture(
        session.as_ref(),
        &mut depth_state,
        SnapshotOptions {
            max_depth: Some(0),
            ..Default::default()
        },
    )
    .await
    .expect("max_depth=0 snapshot");
    assert!(
        depth0.tree.iter().all(|n| n.children.is_empty()),
        "max_depth=0 must have no nested children"
    );

    // Locate @eN refs for prefilled input, country select, checkbox, upload, and offscreen submit button
    let prefilled_ref = full
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("Username"))
        .map(|(k, _)| k.clone())
        .expect("Username textbox ref");
    let select_ref = full
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "combobox" && e.name.as_deref() == Some("Country"))
        .map(|(k, _)| k.clone())
        .expect("Country select ref");
    let agree_ref = full
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "checkbox")
        .map(|(k, _)| k.clone())
        .expect("checkbox ref");
    let upload_ref = full
        .ref_map
        .iter()
        .find(|(_, e)| {
            e.role == "button"
                && e.name
                    .as_deref()
                    .is_some_and(|n| n.contains("Upload Files") || n.contains("Choose File"))
        })
        .map(|(k, _)| k.clone())
        .expect("file input ref");
    let submit_ref = full
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("Submit Form"))
        .map(|(k, _)| k.clone())
        .expect("Submit Form ref");

    let input = ChokepointInput::new(session.clone());
    input
        .fill(
            &InputTarget::Ref(prefilled_ref),
            Some(&snap_state.active_ref_map),
            "alice",
            &FillOptions::default(),
        )
        .await
        .expect("fill @eN");
    let selected = input
        .select_option(
            &InputTarget::Ref(select_ref),
            Some(&snap_state.active_ref_map),
            SelectBy::Value("de".into()),
            0,
        )
        .await
        .expect("select @eN");
    assert_eq!(selected, "de");

    input
        .click(
            &InputTarget::Ref(agree_ref),
            Some(&snap_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click checkbox @eN");

    let tmp_file = tempfile::NamedTempFile::new().expect("temp file");
    std::fs::write(tmp_file.path(), b"snap-upload").expect("write temp");
    input
        .set_input_files(
            &InputTarget::Ref(upload_ref),
            Some(&snap_state.active_ref_map),
            vec![tmp_file.path().to_path_buf()],
            0,
        )
        .await
        .expect("upload @eN");

    let upload_len = session
        .eval_isolated("document.getElementById('upload').files.length")
        .await
        .expect("upload files length");
    assert_eq!(upload_len, 1);

    // Offscreen click via @eN auto-scrolls into view
    input
        .click(
            &InputTarget::Ref(submit_ref),
            Some(&snap_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click offscreen @eN");
    let form_result = session
        .eval_isolated("document.getElementById('form-result').textContent")
        .await
        .expect("form-result");
    assert_eq!(form_result, "submitted:alice:de");

    // Mutate DOM (insert banner + disable button) and verify snapshot --diff
    session
        .eval_isolated(
            r#"(() => {
                const b = document.createElement('div');
                b.setAttribute('role', 'alert');
                b.textContent = 'Form Saved';
                document.getElementById('banner-slot').appendChild(b);
                document.getElementById('submit-bottom').disabled = true;
            })()"#,
        )
        .await
        .expect("mutate DOM for diff");

    let diff_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut snap_state,
        SnapshotOptions {
            diff: true,
            ..Default::default()
        },
    )
    .await
    .expect("diff snapshot");
    let diff = diff_snap.diff.expect("diff present");
    assert!(diff.has_previous);
    assert!(diff.added >= 1, "expected added alert in diff: {}", diff.text);
    assert!(diff.changed >= 2, "expected value/checked/disabled changes: {}", diff.text);
    assert!(
        diff.text.contains("Form Saved"),
        "expected 'Form Saved' in diff:\n{}",
        diff.text
    );
    assert!(
        diff.text.contains("disabled: false -> true"),
        "expected disabled transition in diff:\n{}",
        diff.text
    );

    // Nested iframes & @eN click inside deep iframe
    session
        .navigate(&server.url("/nested-frames-shadow"))
        .await
        .expect("navigate /nested-frames-shadow");
    let mut frame_state = SnapshotState::default();
    let frame_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut frame_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("capture nested frames snapshot");

    assert!(
        frame_snap.text.contains("OuterBtn"),
        "{engine}: expected OuterBtn from outer iframe:\n{}",
        frame_snap.text
    );
    assert!(
        frame_snap.text.contains("DeepBtn"),
        "{engine}: expected DeepBtn from inner iframe:\n{}",
        frame_snap.text
    );
    if engine == EngineKind::Chromium {
        assert!(
            frame_snap.text.contains("ClosedShadowBtn"),
            "Chromium AXTree should include ClosedShadowBtn:\n{}",
            frame_snap.text
        );
    }

    let deep_inp_ref = frame_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("DeepInput"))
        .map(|(k, _)| k.clone())
        .expect("DeepInput ref inside nested iframe");
    let deep_btn_ref = frame_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("DeepBtn"))
        .map(|(k, _)| k.clone())
        .expect("DeepBtn ref inside nested iframe");

    input
        .fill(
            &InputTarget::Ref(deep_inp_ref),
            Some(&frame_state.active_ref_map),
            "nested-ok",
            &FillOptions::default(),
        )
        .await
        .expect("fill DeepInput in nested iframe");
    input
        .click(
            &InputTarget::Ref(deep_btn_ref),
            Some(&frame_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click DeepBtn in nested iframe");

    let deep_status = session
        .eval_isolated(
            "document.getElementById('outer-frame').contentDocument.getElementById('inner-frame').contentDocument.getElementById('deep-status').textContent",
        )
        .await
        .expect("deep-status");
    assert_eq!(deep_status, "deep-clicked:nested-ok");

    // 0x0 <input type="file"> preserved while display:none / visibility:hidden file inputs are pruned
    session
        .navigate(&server.url("/forms-zero-file"))
        .await
        .expect("navigate /forms-zero-file");
    let mut zf_state = SnapshotState::default();
    let zf_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut zf_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("capture /forms-zero-file");
    assert!(
        !zf_snap.text.contains("Hidden Display File") && !zf_snap.text.contains("Hidden Vis File"),
        "{engine}: display:none/visibility:hidden file inputs must stay pruned:\n{}",
        zf_snap.text
    );
    let zero_file_ref = zf_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("Zero File"))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| panic!("{engine}: 0x0 <input type=file> was pruned:\n{}", zf_snap.text));
    let zf_tmp = tempfile::NamedTempFile::new().expect("zf temp");
    std::fs::write(zf_tmp.path(), b"zero-rect-upload").expect("write zf temp");
    input
        .set_input_files(
            &InputTarget::Ref(zero_file_ref),
            Some(&zf_state.active_ref_map),
            vec![zf_tmp.path().to_path_buf()],
            0,
        )
        .await
        .expect("upload to 0x0 file input");
    assert_eq!(
        session
            .eval_isolated("document.getElementById('file-zero').files.length")
            .await
            .expect("file-zero files length"),
        1
    );

    // Offset same-origin iframe bounds parity (SnapshotOutput bounds == resolve_node_box)
    session
        .navigate(&server.url("/offset-iframe-host"))
        .await
        .expect("navigate /offset-iframe-host");
    let mut off_state = SnapshotState::default();
    let off_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut off_state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("capture /offset-iframe-host");
    let off_entry = off_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("OffsetAction"))
        .map(|(_, e)| e.clone())
        .expect("OffsetAction ref");
    let off_snap_bounds = off_entry.bounds.expect("OffsetAction snap bounds");
    let off_live_box = session
        .resolve_node_box(
            off_entry.backend_node_id.expect("offset bid"),
            off_entry.frame_id.as_deref(),
        )
        .await
        .expect("resolve_node_box OffsetAction");
    assert!(
        (off_snap_bounds.x - off_live_box.x).abs() <= 2.0
            && (off_snap_bounds.y - off_live_box.y).abs() <= 2.0,
        "{engine}: offset iframe snap bounds {off_snap_bounds:?} != live {off_live_box:?}"
    );

    // Mixed same-origin + cross-origin iframe page: both walked, bounds match, @eN click/fill/select/upload work
    let server2 = TestServer::start().await;
    let xo_url = server2.url("/xo-child");
    session
        .navigate(&server.url(&format!("/mixed-xo-host?xo={xo_url}")))
        .await
        .expect("navigate /mixed-xo-host");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut xo_state = SnapshotState::default();
    let xo_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut xo_state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("capture /mixed-xo-host");
    assert_eq!(
        xo_snap.text.matches("OffsetAction").count(),
        1,
        "{engine}: same-origin iframe must appear once:\n{}",
        xo_snap.text
    );
    assert_eq!(
        xo_snap.text.matches("XOClick").count(),
        1,
        "{engine}: cross-origin iframe must not be skipped on mixed page:\n{}",
        xo_snap.text
    );

    let xo_btn_entry = xo_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("XOClick"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("XOClick ref");
    let xo_snap_bounds = xo_btn_entry.1.bounds.expect("XOClick snap bounds");
    let xo_live_box = session
        .resolve_node_box(
            xo_btn_entry.1.backend_node_id.expect("xo bid"),
            xo_btn_entry.1.frame_id.as_deref(),
        )
        .await
        .expect("resolve_node_box XOClick");
    assert!(
        (xo_snap_bounds.x - xo_live_box.x).abs() <= 2.0
            && (xo_snap_bounds.y - xo_live_box.y).abs() <= 2.0,
        "{engine}: cross-origin iframe snap bounds {xo_snap_bounds:?} != live {xo_live_box:?}"
    );

    let xo_inp_ref = xo_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("XOInput"))
        .map(|(k, _)| k.clone())
        .expect("XOInput ref");
    let xo_sel_ref = xo_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "combobox" && e.name.as_deref() == Some("XOSelect"))
        .map(|(k, _)| k.clone())
        .expect("XOSelect ref");
    let xo_file_ref = xo_snap
        .ref_map
        .iter()
        .find(|(_, e)| {
            e.role == "button"
                && e.name
                    .as_deref()
                    .is_some_and(|n| n.contains("XOFile") || n.contains("Choose File"))
        })
        .map(|(k, _)| k.clone())
        .expect("XOFile ref");

    input
        .fill(
            &InputTarget::Ref(xo_inp_ref),
            Some(&xo_state.active_ref_map),
            "xo-typed",
            &FillOptions::default(),
        )
        .await
        .expect("fill @eN inside cross-origin iframe");
    let xo_selected = input
        .select_option(
            &InputTarget::Ref(xo_sel_ref),
            Some(&xo_state.active_ref_map),
            SelectBy::Value("b".into()),
            0,
        )
        .await
        .expect("select @eN inside cross-origin iframe");
    assert_eq!(xo_selected, "b");
    input
        .click(
            &InputTarget::Ref(xo_btn_entry.0),
            Some(&xo_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click @eN inside cross-origin iframe");

    let xo_tmp = tempfile::NamedTempFile::new().expect("xo temp");
    std::fs::write(xo_tmp.path(), b"xo-upload-data").expect("write xo temp");
    input
        .set_input_files(
            &InputTarget::Ref(xo_file_ref),
            Some(&xo_state.active_ref_map),
            vec![xo_tmp.path().to_path_buf()],
            0,
        )
        .await
        .expect("upload @eN inside cross-origin iframe");

    let xo_frames = session.list_frames().await.expect("list_frames mixed");
    let xo_fid = xo_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-child frame");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('xo-status').textContent",
                Some(&xo_fid),
            )
            .await
            .expect("xo-status"),
        "xo-clicked:xo-typed:b"
    );
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('xo-file').files.length",
                Some(&xo_fid),
            )
            .await
            .expect("xo-file length"),
        1
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_snapshot_and_diff_chromium() {
    assert_snapshot_and_diff_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_snapshot_and_diff_firefox() {
    assert_snapshot_and_diff_parity(EngineKind::Firefox).await;
}

#[tokio::test]
async fn live_oopif_site_isolation_chromium() {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let xo_url = server2.localhost_url("/xo-child");

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

    session
        .navigate(&server.url(&format!("/mixed-xo-host?xo={xo_url}")))
        .await
        .expect("navigate /mixed-xo-host with OOPIF");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let frames = session.list_frames().await.expect("list_frames OOPIF");
    let main_frame = frames.iter().find(|f| f.is_main).expect("main frame");
    let so_frame = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/offset-iframe-child"))
        .expect("same-origin child frame");
    let xo_frame = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-child"))
        .expect("cross-site OOPIF frame");
    assert_eq!(
        xo_frame.parent_frame_id.as_deref(),
        Some(main_frame.frame_id.as_str()),
        "OOPIF must link to true parent_frame_id, frames={frames:?}"
    );
    assert_eq!(
        so_frame.parent_frame_id.as_deref(),
        Some(main_frame.frame_id.as_str())
    );
    assert_eq!(
        frames.iter().filter(|f| f.frame_id == xo_frame.frame_id).count(),
        1,
        "OOPIF frame_id must be deduplicated"
    );

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
    .expect("capture OOPIF snapshot with bounds");

    assert_eq!(
        snap.text.matches("OffsetAction").count(),
        1,
        "same-origin iframe node must appear once:\n{}",
        snap.text
    );
    assert_eq!(
        snap.text.matches("XOClick").count(),
        1,
        "OOPIF XOClick must appear in snapshot:\n{}",
        snap.text
    );

    let so_btn_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("OffsetAction"))
        .map(|(_, e)| e.clone())
        .expect("OffsetAction ref");
    let so_snap_bounds = so_btn_entry.bounds.expect("OffsetAction snap bounds");
    let so_live_box = session
        .resolve_node_box(
            so_btn_entry.backend_node_id.expect("so bid"),
            so_btn_entry.frame_id.as_deref(),
        )
        .await
        .expect("resolve_node_box OffsetAction");
    assert!(
        (so_snap_bounds.x - so_live_box.x).abs() <= 2.0
            && (so_snap_bounds.y - so_live_box.y).abs() <= 2.0,
        "same-origin snap bounds {so_snap_bounds:?} != live {so_live_box:?} (possible backendNodeId collision)"
    );

    let xo_btn_entry = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("XOClick"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("XOClick ref");
    let xo_snap_bounds = xo_btn_entry.1.bounds.expect("XOClick snap bounds");
    let xo_live_box = session
        .resolve_node_box(
            xo_btn_entry.1.backend_node_id.expect("xo bid"),
            xo_btn_entry.1.frame_id.as_deref(),
        )
        .await
        .expect("resolve_node_box XOClick in OOPIF");
    assert!(
        (xo_snap_bounds.x - xo_live_box.x).abs() <= 2.0
            && (xo_snap_bounds.y - xo_live_box.y).abs() <= 2.0,
        "OOPIF snap bounds {xo_snap_bounds:?} != live {xo_live_box:?}"
    );
    assert!(
        xo_snap_bounds.y > so_snap_bounds.y + 50.0,
        "OOPIF bounds {xo_snap_bounds:?} must include iframe offset below same-origin {so_snap_bounds:?}"
    );

    let xo_inp_ref = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "textbox" && e.name.as_deref() == Some("XOInput"))
        .map(|(k, _)| k.clone())
        .expect("XOInput ref in OOPIF");
    let xo_sel_ref = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "combobox" && e.name.as_deref() == Some("XOSelect"))
        .map(|(k, _)| k.clone())
        .expect("XOSelect ref in OOPIF");
    let xo_file_ref = snap
        .ref_map
        .iter()
        .find(|(_, e)| {
            e.role == "button"
                && e.name
                    .as_deref()
                    .is_some_and(|n| n.contains("XOFile") || n.contains("Choose File"))
        })
        .map(|(k, _)| k.clone())
        .expect("XOFile ref in OOPIF");

    let input = ChokepointInput::new(session.clone());
    input
        .fill(
            &InputTarget::Ref(xo_inp_ref),
            Some(&state.active_ref_map),
            "oopif-typed",
            &FillOptions::default(),
        )
        .await
        .expect("fill @eN in OOPIF");
    let selected = input
        .select_option(
            &InputTarget::Ref(xo_sel_ref),
            Some(&state.active_ref_map),
            SelectBy::Value("b".into()),
            0,
        )
        .await
        .expect("select @eN in OOPIF");
    assert_eq!(selected, "b");
    input
        .click(
            &InputTarget::Ref(xo_btn_entry.0),
            Some(&state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click @eN in OOPIF");

    let tmp_file = tempfile::NamedTempFile::new().expect("temp file");
    std::fs::write(tmp_file.path(), b"oopif-upload").expect("write temp");
    input
        .set_input_files(
            &InputTarget::Ref(xo_file_ref),
            Some(&state.active_ref_map),
            vec![tmp_file.path().to_path_buf()],
            0,
        )
        .await
        .expect("upload @eN in OOPIF");

    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('xo-status').textContent",
                Some(&xo_frame.frame_id),
            )
            .await
            .expect("xo-status in OOPIF"),
        "xo-clicked:oopif-typed:b"
    );
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('xo-file').files.length",
                Some(&xo_frame.frame_id),
            )
            .await
            .expect("xo-file length in OOPIF"),
        1
    );

    // Nested XO-in-Same OOPIF: main -> same-origin middle -> cross-site OOPIF
    session
        .navigate(&server.url(&format!("/nested-so-xo-host?xo={xo_url}")))
        .await
        .expect("navigate /nested-so-xo-host");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let n_frames = session.list_frames().await.expect("list_frames nested OOPIF");
    let n_mid = n_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/nested-so-xo-middle"))
        .expect("middle same-origin frame");
    let n_xo = n_frames
        .iter()
        .find(|f| !f.is_main && f.url == xo_url)
        .expect("nested OOPIF frame");
    assert_eq!(
        n_xo.parent_frame_id.as_deref(),
        Some(n_mid.frame_id.as_str()),
        "nested OOPIF parent_frame_id must be the middle same-origin frame, frames={n_frames:?}"
    );

    let mut n_state = SnapshotState::default();
    let n_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut n_state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("capture nested OOPIF snapshot");
    let n_xo_btn = n_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("XOClick"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("nested OOPIF XOClick ref");
    let n_snap_box = n_xo_btn.1.bounds.expect("nested OOPIF snap bounds");
    let n_live_box = session
        .resolve_node_box(
            n_xo_btn.1.backend_node_id.expect("nested xo bid"),
            n_xo_btn.1.frame_id.as_deref(),
        )
        .await
        .expect("resolve_node_box nested OOPIF");
    assert!(
        (n_snap_box.x - n_live_box.x).abs() <= 2.0
            && (n_snap_box.y - n_live_box.y).abs() <= 2.0,
        "nested OOPIF snap bounds {n_snap_box:?} != live {n_live_box:?}"
    );
    input
        .click(
            &InputTarget::Ref(n_xo_btn.0),
            Some(&n_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click nested OOPIF XOClick");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('xo-status').textContent",
                Some(&n_xo.frame_id),
            )
            .await
            .expect("nested xo-status"),
        "xo-clicked::a"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_nested_oopif_and_subframe_site_isolation_chromium() {
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

    // (a) True 3-level nested OOPIF: 127.0.0.1 -> localhost -> 127.0.0.1 (deadlock & detach regression)
    let leaf_url = server.url("/nested-oopif-leaf");
    let mid_url = server2.localhost_url(&format!("/nested-oopif-mid?leaf={leaf_url}"));
    let root_url = server.url(&format!("/nested-oopif-root?mid={mid_url}"));

    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        session.navigate(&root_url).await.expect("navigate nested OOPIF");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let frames = session.list_frames().await.expect("list_frames 3-level OOPIF");
        let main_f = frames.iter().find(|f| f.is_main).expect("main frame");
        let mid_f = frames
            .iter()
            .find(|f| !f.is_main && f.name.as_deref() == Some("nested-mid"))
            .expect("mid OOPIF frame");
        let leaf_f = frames
            .iter()
            .find(|f| !f.is_main && f.name.as_deref() == Some("nested-leaf"))
            .expect("leaf OOPIF frame");
        assert!(mid_f.url.contains("/nested-oopif-mid"));
        assert!(leaf_f.url.ends_with("/nested-oopif-leaf"));
        assert_eq!(mid_f.parent_frame_id.as_deref(), Some(main_f.frame_id.as_str()));
        assert_eq!(leaf_f.parent_frame_id.as_deref(), Some(mid_f.frame_id.as_str()));

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
        .expect("snapshot 3-level OOPIF");
        assert_eq!(snap.text.matches("NLeafBtn").count(), 1, "{}", snap.text);

        let leaf_btn = snap
            .ref_map
            .iter()
            .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("NLeafBtn"))
            .map(|(k, e)| (k.clone(), e.clone()))
            .expect("NLeafBtn ref");
        let snap_box = leaf_btn.1.bounds.expect("NLeafBtn bounds");
        let live_box = session
            .resolve_node_box(
                leaf_btn.1.backend_node_id.expect("NLeafBtn bid"),
                leaf_btn.1.frame_id.as_deref(),
            )
            .await
            .expect("resolve_node_box NLeafBtn");
        assert!(
            (snap_box.x - live_box.x).abs() <= 2.0 && (snap_box.y - live_box.y).abs() <= 2.0,
            "3-level OOPIF snap {snap_box:?} != live {live_box:?}"
        );

        let input = ChokepointInput::new(session.clone());
        input
            .click(
                &InputTarget::Ref(leaf_btn.0),
                Some(&state.active_ref_map),
                &ClickOptions::default(),
            )
            .await
            .expect("click NLeafBtn");
        assert_eq!(
            session
                .eval_isolated_in_frame(
                    "document.getElementById('nleaf-status').textContent",
                    Some(&leaf_f.frame_id),
                )
                .await
                .expect("nleaf-status"),
            "nleaf-clicked"
        );

        // Detach outer OOPIF (cascades nested child OOPIF detach) and confirm zero deadlock/stale frames
        session
            .eval_isolated("document.getElementById('nmid-frame').remove()")
            .await
            .expect("remove outer OOPIF iframe");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let after_frames = session.list_frames().await.expect("list_frames after detach");
        assert_eq!(after_frames.len(), 1, "detached OOPIFs must be pruned: {after_frames:?}");
    })
    .await
    .expect("3-level nested OOPIF test must not deadlock");

    // (b) Same-origin subframe inside OOPIF: 127.0.0.1 -> localhost/oopif-outer -> localhost/oopif-inner
    let outer_url = server2.localhost_url("/oopif-outer");
    let sub_host_url = server.url(&format!("/oopif-same-sub-host?outer={outer_url}"));
    session
        .navigate(&sub_host_url)
        .await
        .expect("navigate /oopif-same-sub-host");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let s_frames = session.list_frames().await.expect("list_frames same-sub-in-oopif");
    let s_main = s_frames.iter().find(|f| f.is_main).expect("main frame");
    let s_outer = s_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/oopif-outer"))
        .expect("oopif-outer frame");
    let s_inner = s_frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/oopif-inner"))
        .expect("oopif-inner frame");
    assert_eq!(s_outer.parent_frame_id.as_deref(), Some(s_main.frame_id.as_str()));
    assert_eq!(s_inner.parent_frame_id.as_deref(), Some(s_outer.frame_id.as_str()));

    let mut s_state = SnapshotState::default();
    let s_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut s_state,
        SnapshotOptions {
            include_bounds: true,
            ..Default::default()
        },
    )
    .await
    .expect("snapshot same-sub-in-oopif");
    assert_eq!(
        s_snap.text.matches("OuterOOPIFPara").count(),
        1,
        "oopif-outer must not be duplicated:\n{}",
        s_snap.text
    );
    assert_eq!(
        s_snap.text.matches("OOPIFSameSubBtn").count(),
        1,
        "oopif-inner OOPIFSameSubBtn must be present:\n{}",
        s_snap.text
    );

    let inner_btn = s_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("OOPIFSameSubBtn"))
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("OOPIFSameSubBtn ref");
    let inner_snap_box = inner_btn.1.bounds.expect("OOPIFSameSubBtn snap bounds");
    let inner_live_box = session
        .resolve_node_box(
            inner_btn.1.backend_node_id.expect("inner btn bid"),
            inner_btn.1.frame_id.as_deref(),
        )
        .await
        .expect("resolve_node_box OOPIFSameSubBtn");
    assert!(
        (inner_snap_box.x - inner_live_box.x).abs() <= 2.0
            && (inner_snap_box.y - inner_live_box.y).abs() <= 2.0,
        "oopif-inner snap {inner_snap_box:?} != live {inner_live_box:?} (possible double-offset)"
    );

    // Cross-check against DOM getBoundingClientRect sum (must not double-add oopif-inner's margin/border/padding)
    let outer_off = session
        .eval_isolated(
            "(() => { const f = document.getElementById('oopif-outer-frame'); const r = f.getBoundingClientRect(); const cs = getComputedStyle(f); return { x: r.x + parseFloat(cs.borderLeftWidth) + parseFloat(cs.paddingLeft), y: r.y + parseFloat(cs.borderTopWidth) + parseFloat(cs.paddingTop) }; })()",
        )
        .await
        .expect("outer offset");
    let inner_off = session
        .eval_isolated_in_frame(
            "(() => { const f = document.getElementById('oopif-inner-frame'); const r = f.getBoundingClientRect(); const cs = getComputedStyle(f); return { x: r.x + parseFloat(cs.borderLeftWidth) + parseFloat(cs.paddingLeft), y: r.y + parseFloat(cs.borderTopWidth) + parseFloat(cs.paddingTop) }; })()",
            Some(&s_outer.frame_id),
        )
        .await
        .expect("inner offset");
    let btn_local = session
        .eval_isolated_in_frame(
            "(() => { const r = document.getElementById('inner-sub-btn').getBoundingClientRect(); return { x: r.x, y: r.y }; })()",
            Some(&s_inner.frame_id),
        )
        .await
        .expect("btn local");
    let expected_x = outer_off["x"].as_f64().unwrap()
        + inner_off["x"].as_f64().unwrap()
        + btn_local["x"].as_f64().unwrap();
    let expected_y = outer_off["y"].as_f64().unwrap()
        + inner_off["y"].as_f64().unwrap()
        + btn_local["y"].as_f64().unwrap();
    assert!(
        (inner_live_box.x - expected_x).abs() <= 2.0
            && (inner_live_box.y - expected_y).abs() <= 2.0,
        "oopif-inner live {inner_live_box:?} != expected ({expected_x}, {expected_y})"
    );

    let input = ChokepointInput::new(session.clone());
    input
        .click(
            &InputTarget::Ref(inner_btn.0),
            Some(&s_state.active_ref_map),
            &ClickOptions::default(),
        )
        .await
        .expect("click OOPIFSameSubBtn");
    assert_eq!(
        session
            .eval_isolated_in_frame(
                "document.getElementById('inner-sub-status').textContent",
                Some(&s_inner.frame_id),
            )
            .await
            .expect("inner-sub-status"),
        "inner-sub-clicked"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_oopif_exec_context_isolation_chromium() {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let xo_url_1 = server2.localhost_url("/xo-ctx-child");
    let xo_url_2 = server2.localhost_url("/xo-ctx-child-2");

    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            extra_args: vec!["--site-per-process".to_string()],
            ..Default::default()
        })
        .await
        .expect("launch chromium with main_world_eval + --site-per-process");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url(&format!("/iframe-exec-ctx-host?xo={xo_url_1}")))
        .await
        .expect("navigate /iframe-exec-ctx-host");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let frames = session.list_frames().await.expect("list_frames exec ctx");
    let so_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/so-ctx-child"))
        .map(|f| f.frame_id.clone())
        .expect("so-ctx-child frame");
    let xo_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-ctx-child"))
        .map(|f| f.frame_id.clone())
        .expect("xo-ctx-child frame");

    assert_eq!(
        session
            .eval_main_world_in_frame("window.__soMarker", Some(&so_fid))
            .await
            .expect("so marker before"),
        "so-alive-42"
    );
    assert_eq!(
        session
            .eval_main_world_in_frame("window.__xoMarker", Some(&xo_fid))
            .await
            .expect("xo marker 1"),
        "xo-1"
    );

    // Navigate OOPIF to a second URL on the same OOPIF origin (triggers executionContextsCleared / executionContextDestroyed with overlapping ctx_id=1..N)
    session
        .eval_isolated(&format!(
            "document.getElementById('xo-ctx-frame').src = '{}'",
            xo_url_2
        ))
        .await
        .expect("navigate OOPIF iframe");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        session
            .eval_main_world_in_frame("window.__soMarker", Some(&so_fid))
            .await
            .expect("so marker must survive OOPIF navigation"),
        "so-alive-42"
    );
    assert_eq!(
        session
            .eval_main_world_in_frame("window.__xoMarker", Some(&xo_fid))
            .await
            .expect("xo marker 2"),
        "xo-2"
    );

    // Destroy OOPIF iframe completely (emits executionContextDestroyed + Target.detachedFromTarget)
    session
        .eval_isolated("document.getElementById('xo-ctx-frame').remove()")
        .await
        .expect("remove OOPIF iframe");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert_eq!(
        session
            .eval_main_world_in_frame("window.__soMarker", Some(&so_fid))
            .await
            .expect("so marker must survive OOPIF destruction"),
        "so-alive-42"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}
