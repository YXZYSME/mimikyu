mod support;

use std::time::Instant;

use mimikyu_core::driver::{
    driver_for, ContextOptions, EngineKind, EvalOptions, LaunchOptions, ProfileMode,
    ScreenshotFormat,
};
use mimikyu_core::extract::{
    capture_screenshot, export_pdf, extract_links, extract_markdown, extract_tables, extract_text,
    set_instant_animations, CaptureOpts, ExtractOptions, MediaTarget,
};
use mimikyu_core::snapshot::{SnapshotEngine, SnapshotOptions, SnapshotState};
use mimikyu_core::MimikyuError;
use support::{assert_pgid_gone, TestServer};

async fn assert_eval_extract_media_parity(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: false,
            instant_animations: true,
            ..Default::default()
        })
        .await
        .expect("launch browser");
    let pid = session.info().pid.expect("pid");

    // 1. F9: Isolated vs opt-in main-world eval across frames & zero __mimikyu leakage
    session
        .navigate(&server.url("/nested-frames-shadow"))
        .await
        .expect("navigate /nested-frames-shadow");

    let sees_main = session
        .eval_isolated("typeof window.__mainSecret")
        .await
        .expect("isolated eval");
    assert_eq!(sees_main, "undefined");

    let no_mimikyu_main = session
        .eval_isolated("typeof window.__mimikyu")
        .await
        .expect("no __mimikyu");
    assert_eq!(no_mimikyu_main, "undefined");

    let mw_disabled = session
        .eval_main_world("window.__mainSecret")
        .await
        .expect_err("main_world_eval=false must reject");
    assert!(matches!(mw_disabled, MimikyuError::MainWorldDisabled));

    let frames = session.list_frames().await.expect("list_frames");
    assert!(
        frames.len() >= 3,
        "{engine}: expected main + outer + inner frames, got {frames:?}"
    );
    let inner_frame = frames
        .iter()
        .find(|f| f.url.contains("/inner-frame"))
        .expect("inner-frame present");

    let mw_sub_disabled = session
        .eval_with_options(
            "window.__innerSecret",
            &EvalOptions {
                main_world: true,
                frame_id: Some(inner_frame.frame_id.clone()),
            },
        )
        .await
        .expect_err("subframe main world must reject when main_world_eval=false");
    assert!(matches!(mw_sub_disabled, MimikyuError::MainWorldDisabled));

    let iso_sub_text = session
        .eval_isolated_in_frame(
            "document.getElementById('deep-btn').textContent",
            Some(&inner_frame.frame_id),
        )
        .await
        .expect("eval_isolated_in_frame");
    assert_eq!(iso_sub_text, "DeepBtn");

    // Opt-in main_world_eval in isolated context
    let mw_ctx = session
        .new_context(ContextOptions {
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("new_context with main_world_eval=true");
    mw_ctx
        .navigate(&server.url("/nested-frames-shadow"))
        .await
        .expect("mw_ctx navigate");
    let mw_frames = mw_ctx.list_frames().await.expect("mw_ctx list_frames");
    let mw_inner = mw_frames
        .iter()
        .find(|f| f.url.contains("/inner-frame"))
        .expect("mw inner frame");
    assert_eq!(
        mw_ctx.eval_main_world("window.__mainSecret").await.unwrap(),
        "top-secret"
    );
    assert_eq!(
        mw_ctx
            .eval_main_world_in_frame("window.__innerSecret", Some(&mw_inner.frame_id))
            .await
            .unwrap(),
        "inner-secret"
    );
    mw_ctx.dispose().await.expect("dispose mw_ctx");

    // 2. F9: Open & Closed Shadow DOM traversal
    let open_matches = session
        .query_shadow(&["#open-host".into(), "#open-btn".into()], None)
        .await
        .expect("query open shadow");
    assert_eq!(open_matches.len(), 1);
    assert_eq!(open_matches[0].text, "OpenShadowBtn");

    let closed_chain = vec![
        "#closed-host-1".into(),
        "#closed-host-2".into(),
        "#closed-btn".into(),
    ];
    if engine == EngineKind::Chromium {
        assert!(session.capabilities().supports_closed_shadow_dom);
        let closed_matches = session
            .query_shadow(&closed_chain, None)
            .await
            .expect("query nested closed shadow on Chromium");
        assert_eq!(closed_matches.len(), 1);
        assert_eq!(closed_matches[0].text, "ClosedShadowBtn");
        let bid = closed_matches[0].backend_node_id.expect("closed btn bid");
        let tag = session
            .call_function_on_node(bid, None, "function() { return this.tagName; }", vec![])
            .await
            .expect("call_function_on_node");
        assert_eq!(tag, "BUTTON");

        // Verify attachShadow was never monkey-patched
        let native_check = session
            .eval_isolated("Element.prototype.attachShadow.toString().includes('[native code]')")
            .await
            .expect("native attachShadow check");
        assert_eq!(native_check, true);
    } else {
        assert!(!session.capabilities().supports_closed_shadow_dom);
        let err = session
            .query_shadow(&closed_chain, None)
            .await
            .expect_err("Firefox BiDi must return Unsupported for closed shadow");
        assert!(matches!(err, MimikyuError::Unsupported { .. }));
    }

    // Also test extract_links/markdown/tables including closed shadow on Chromium without cross-frame duplication
    let shadow_md = extract_markdown(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_markdown on shadow page");
    let shadow_md_frames = extract_markdown(
        session.as_ref(),
        &ExtractOptions {
            include_frames: true,
            ..Default::default()
        },
    )
    .await
    .expect("extract_markdown(include_frames=true) on shadow page");
    if engine == EngineKind::Chromium {
        assert!(
            shadow_md.markdown.contains("ClosedHeading"),
            "Chromium should extract closed shadow markdown: {}",
            shadow_md.markdown
        );
        assert_eq!(
            shadow_md_frames.markdown.matches("ClosedHeading").count(),
            1,
            "Chromium extract_markdown(include_frames=true) must not duplicate closed shadow across frames:\n{}",
            shadow_md_frames.markdown
        );
        let shadow_links_frames = extract_links(
            session.as_ref(),
            &ExtractOptions {
                include_frames: true,
                ..Default::default()
            },
        )
        .await
        .expect("extract_links(include_frames=true) on shadow page");
        assert_eq!(
            shadow_links_frames
                .links
                .iter()
                .filter(|l| l.text == "ClosedLink")
                .count(),
            1,
            "Chromium extract_links(include_frames=true) must not duplicate closed shadow link"
        );
        let shadow_tables_frames = extract_tables(
            session.as_ref(),
            &ExtractOptions {
                include_frames: true,
                ..Default::default()
            },
        )
        .await
        .expect("extract_tables(include_frames=true) on shadow page");
        assert_eq!(
            shadow_tables_frames
                .tables
                .iter()
                .filter(|t| t.rows.iter().any(|r| r.iter().any(|c| c == "ClosedCell")))
                .count(),
            1,
            "Chromium extract_tables(include_frames=true) must not duplicate closed shadow table"
        );
        assert!(
            session
                .closed_shadow_hosts(Some(&inner_frame.frame_id))
                .await
                .expect("closed_shadow_hosts(inner)")
                .is_empty(),
            "inner frame has 0 closed shadow hosts and must not inherit main frame hosts"
        );

        // Closed shadow inside a same-origin iframe on CDP
        session
            .navigate(&server.url("/iframe-closed-shadow-host"))
            .await
            .expect("navigate /iframe-closed-shadow-host");
        let cs_frames = session.list_frames().await.expect("list_frames cs");
        let cs_main = cs_frames.iter().find(|f| f.is_main).expect("cs main");
        let cs_child = cs_frames
            .iter()
            .find(|f| f.url.contains("/iframe-closed-shadow-child"))
            .expect("cs child");
        assert!(
            session
                .closed_shadow_hosts(Some(&cs_main.frame_id))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            session
                .closed_shadow_hosts(Some(&cs_child.frame_id))
                .await
                .unwrap()
                .len(),
            1
        );
        let cs_no_frames = extract_markdown(session.as_ref(), &ExtractOptions::default())
            .await
            .expect("cs no frames");
        assert!(
            !cs_no_frames.markdown.contains("SubClosedHeading"),
            "include_frames=false must not leak iframe closed shadow:\n{}",
            cs_no_frames.markdown
        );
        let cs_with_frames = extract_markdown(
            session.as_ref(),
            &ExtractOptions {
                include_frames: true,
                ..Default::default()
            },
        )
        .await
        .expect("cs with frames");
        assert_eq!(
            cs_with_frames.markdown.matches("SubClosedHeading").count(),
            1,
            "include_frames=true must extract iframe closed shadow exactly once:\n{}",
            cs_with_frames.markdown
        );
    }

    // 3. F11: Content Extraction (markdown, text, links, tables)
    session
        .navigate(&server.url("/extract-article"))
        .await
        .expect("navigate /extract-article");

    let md = extract_markdown(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_markdown");
    assert!(md.markdown.contains("# Main Title"));
    assert!(md.markdown.contains("## Section One"));
    assert!(md.markdown.contains("- Item 1"));
    assert!(md.markdown.contains("  - Sub 1"));
    assert!(md.markdown.contains("```rust\nfn hello()"));
    assert!(md.markdown.contains("> Quoted wisdom"));
    assert!(md.markdown.contains("OpenShadowParagraph"));
    assert!(!md.markdown.contains("HiddenNav"));
    assert!(!md.markdown.contains("HiddenDisplayNone"));

    let trunc_md = extract_markdown(
        session.as_ref(),
        &ExtractOptions {
            max_chars: Some(25),
            ..Default::default()
        },
    )
    .await
    .expect("truncated markdown");
    assert!(trunc_md.truncated);
    assert_eq!(trunc_md.char_count, 25);

    let txt = extract_text(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_text");
    assert!(txt.text.contains("Main Title"));
    assert!(!txt.text.contains("HiddenDisplayNone"));

    let links = extract_links(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_links");
    assert!(links.links.iter().any(|l| l.text == "Relative Link" && !l.is_external));
    assert!(links.links.iter().any(|l| l.text == "External Link" && l.is_external));
    assert!(links.links.iter().any(|l| l.text == "Icon Alt"));
    assert!(links.links.iter().any(|l| l.text == "OpenShadowLink"));
    assert!(!links.links.iter().any(|l| l.text == "HiddenNav"));

    let tables = extract_tables(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_tables");
    let t1 = tables
        .tables
        .iter()
        .find(|t| t.id.as_deref() == Some("t1"))
        .expect("table t1");
    assert_eq!(t1.caption.as_deref(), Some("Plan Matrix"));
    assert_eq!(t1.headers, vec!["Tier", "Specs", "Specs"]);
    assert_eq!(
        t1.rows,
        vec![vec!["Pro", "CPU", "8"], vec!["Pro", "RAM", "32G"]]
    );
    let outer_tbl = tables
        .tables
        .iter()
        .find(|t| t.id.as_deref() == Some("outer-tbl"))
        .expect("outer-tbl");
    assert_eq!(outer_tbl.row_count, 1, "nested table rows must not leak into outer table");

    // 4. F11: instant_animations persistence across navigation + zero DOM leakage + runtime toggle
    let leak_id = session
        .eval_isolated("document.getElementById('__mimikyu_instant_anim') === null")
        .await
        .expect("no __mimikyu_instant_anim");
    assert_eq!(leak_id, true);

    let t0 = Instant::now();
    session
        .eval_isolated("document.getElementById('anim-box').classList.add('anim-active')")
        .await
        .expect("trigger anim");
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let ended = session
        .eval_isolated("document.getElementById('anim-box').dataset.animEnded")
        .await
        .expect("animEnded");
    assert_eq!(
        ended, "true",
        "{engine}: 2s animation must finish instantly (<100ms), elapsed={}ms",
        t0.elapsed().as_millis()
    );

    set_instant_animations(session.as_ref(), false)
        .await
        .expect("disable instant_animations");
    set_instant_animations(session.as_ref(), true)
        .await
        .expect("re-enable instant_animations");

    // 5. F11: Non-mutating offscreen element screenshot + max_width/max_height + PDF
    let scroll_before = session
        .eval_isolated("window.scrollY")
        .await
        .expect("scrollY before");
    assert_eq!(scroll_before, 0);

    let offscreen_shot = capture_screenshot(
        session.as_ref(),
        None,
        CaptureOpts {
            target: Some(MediaTarget::Selector("#offscreen-card".into())),
            format: ScreenshotFormat::Png,
            ..Default::default()
        },
    )
    .await
    .expect("offscreen element screenshot");
    assert!(offscreen_shot.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(offscreen_shot.width >= 200 && offscreen_shot.height >= 60);

    let scroll_after = session
        .eval_isolated("window.scrollY")
        .await
        .expect("scrollY after");
    assert_eq!(scroll_after, 0, "screenshot must not mutate window.scrollY");

    // Offscreen screenshot via MediaTarget::Ref("@eN")
    let mut media_snap_state = SnapshotState::default();
    let media_snap = SnapshotEngine::capture(
        session.as_ref(),
        &mut media_snap_state,
        SnapshotOptions::default(),
    )
    .await
    .expect("snapshot for MediaTarget::Ref");
    let offscreen_ref = media_snap
        .ref_map
        .iter()
        .find(|(_, e)| e.role == "button" && e.name.as_deref() == Some("Offscreen Ref Target"))
        .map(|(k, _)| k.clone())
        .expect("Offscreen Ref Target ref");
    let ref_shot = capture_screenshot(
        session.as_ref(),
        Some(&media_snap_state.active_ref_map),
        CaptureOpts {
            target: Some(MediaTarget::Ref(offscreen_ref)),
            format: ScreenshotFormat::Png,
            ..Default::default()
        },
    )
    .await
    .expect("offscreen MediaTarget::Ref screenshot");
    assert!(ref_shot.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(ref_shot.width >= 200 && ref_shot.height >= 60);
    let scroll_after_ref = session
        .eval_isolated("window.scrollY")
        .await
        .expect("scrollY after MediaTarget::Ref");
    assert_eq!(scroll_after_ref, 0, "MediaTarget::Ref screenshot must not mutate scrollY");

    let bounded_shot = capture_screenshot(
        session.as_ref(),
        None,
        CaptureOpts {
            target: Some(MediaTarget::FullPage),
            format: ScreenshotFormat::Jpeg { quality: 80 },
            max_width: Some(320),
            max_height: Some(320),
            ..Default::default()
        },
    )
    .await
    .expect("bounded full_page jpeg");
    assert!(bounded_shot.bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
    assert!(bounded_shot.width <= 320 && bounded_shot.height <= 320);

    let pdf = export_pdf(session.as_ref()).await.expect("export_pdf");
    assert!(pdf.starts_with(b"%PDF-"));

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_eval_extract_and_media_chromium() {
    assert_eval_extract_media_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_eval_extract_and_media_firefox() {
    assert_eval_extract_media_parity(EngineKind::Firefox).await;
}

async fn assert_eval_json_parity(engine: EngineKind) {
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch browser");
    let pid = session.info().pid.expect("pid");

    session
        .navigate("data:text/html,<!doctype html><html><body><p>eval</p></body></html>")
        .await
        .expect("navigate data html");

    let valid_cases = [
        ("undefined", serde_json::json!(null)),
        ("null", serde_json::json!(null)),
        ("-0", serde_json::json!(0)),
        ("1 + 1", serde_json::json!(2)),
        ("({a: [1, true, 'x']})", serde_json::json!({"a": [1, true, "x"]})),
        (
            "(() => { const x = {k: 1}; return {a: x, b: x}; })()",
            serde_json::json!({"a": {"k": 1}, "b": {"k": 1}}),
        ),
        ("({x: -0})", serde_json::json!({"x": 0})),
        ("({x: undefined, y: 1})", serde_json::json!({"y": 1})),
        ("[1, undefined, 2]", serde_json::json!([1, null, 2])),
        ("Promise.resolve({answer: 42})", serde_json::json!({"answer": 42})),
        ("new Response('ok')", serde_json::json!({})),
        ("new Request('http://127.0.0.1/')", serde_json::json!({})),
        ("new Headers({ 'x-a': '1' })", serde_json::json!({})),
        ("new URL('http://127.0.0.1/a')", serde_json::json!({})),
        ("new URLSearchParams('a=1')", serde_json::json!({})),
        (
            "({r: new Response('ok'), u: new URL('http://127.0.0.1/a')})",
            serde_json::json!({"r": {}, "u": {}}),
        ),
    ];

    let invalid_cases = [
        "BigInt(42)",
        "NaN",
        "Infinity",
        "-Infinity",
        "Symbol('s')",
        "(() => {})",
        "(function foo() {})",
        "(async () => {})",
        "(class Foo {})",
        "(() => { const a = {}; a.self = a; return a; })()",
        "(() => { const a = []; a.push(a); return a; })()",
        "({x: NaN})",
        "({x: Infinity})",
        "({x: BigInt(42)})",
        "document.body",
        "window",
        "new Map([['k', 'v']])",
        "new Set([1, 2])",
        "new Date()",
        "/abc/",
        "new Error('e')",
        "Promise.resolve(() => {})",
        "Promise.resolve(NaN)",
        "new WeakMap()",
        "new ArrayBuffer(8)",
        "new Uint8Array(2)",
        "new Blob(['ok'])",
        "new DOMException('e')",
        "({w: new WeakMap()})",
        "document.body.style",
    ];

    for main_world in [false, true] {
        for (expr, expected) in &valid_cases {
            let actual = if main_world {
                session.eval_main_world(expr).await
            } else {
                session.eval_isolated(expr).await
            }
            .unwrap_or_else(|e| panic!("{engine} main_world={main_world} expr={expr}: {e}"));
            assert_eq!(
                &actual, expected,
                "{engine} main_world={main_world} expr={expr}"
            );
        }
        for expr in &invalid_cases {
            let res = if main_world {
                session.eval_main_world(expr).await
            } else {
                session.eval_isolated(expr).await
            };
            assert!(
                matches!(res, Err(MimikyuError::EvalFailed(_))),
                "{engine} main_world={main_world} expr={expr}: expected EvalFailed, got {res:?}"
            );
        }
    }

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn live_eval_json_parity_chromium() {
    assert_eval_json_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_eval_json_parity_firefox() {
    assert_eval_json_parity(EngineKind::Firefox).await;
}

#[tokio::test]
async fn live_oopif_extract_and_closed_shadow_chromium() {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let xo_url = server2.localhost_url("/xo-closed-shadow-child");

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
        .navigate(&server.url(&format!("/xo-closed-shadow-host?xo={xo_url}")))
        .await
        .expect("navigate /xo-closed-shadow-host");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let frames = session.list_frames().await.expect("list_frames");
    let main_f = frames.iter().find(|f| f.is_main).expect("main frame");
    let xo_f = frames
        .iter()
        .find(|f| !f.is_main && f.url.contains("/xo-closed-shadow-child"))
        .expect("OOPIF closed-shadow frame");
    assert_eq!(
        xo_f.parent_frame_id.as_deref(),
        Some(main_f.frame_id.as_str())
    );

    assert!(
        session
            .closed_shadow_hosts(Some(&main_f.frame_id))
            .await
            .expect("closed_shadow_hosts(main)")
            .is_empty(),
        "main frame must not inherit OOPIF closed shadow hosts"
    );
    assert_eq!(
        session
            .closed_shadow_hosts(Some(&xo_f.frame_id))
            .await
            .expect("closed_shadow_hosts(xo)")
            .len(),
        1,
        "OOPIF closed_shadow_hosts must find the closed shadow host inside the OOPIF"
    );

    let md_no_frames = extract_markdown(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_markdown(include_frames=false)");
    assert!(!md_no_frames.markdown.contains("XOPlainHeading"));
    assert!(!md_no_frames.markdown.contains("XOClosedHeading"));

    let md_with_frames = extract_markdown(
        session.as_ref(),
        &ExtractOptions {
            include_frames: true,
            ..Default::default()
        },
    )
    .await
    .expect("extract_markdown(include_frames=true)");
    assert_eq!(
        md_with_frames.markdown.matches("XOMainHeading").count(),
        1,
        "main heading count:\n{}",
        md_with_frames.markdown
    );
    assert_eq!(
        md_with_frames.markdown.matches("XOPlainHeading").count(),
        1,
        "OOPIF plain heading must be extracted once:\n{}",
        md_with_frames.markdown
    );
    assert_eq!(
        md_with_frames.markdown.matches("XOClosedHeading").count(),
        1,
        "OOPIF closed shadow heading must be extracted once:\n{}",
        md_with_frames.markdown
    );

    let links_with_frames = extract_links(
        session.as_ref(),
        &ExtractOptions {
            include_frames: true,
            ..Default::default()
        },
    )
    .await
    .expect("extract_links(include_frames=true)");
    assert_eq!(
        links_with_frames
            .links
            .iter()
            .filter(|l| l.text == "XOClosedLink")
            .count(),
        1
    );

    let tables_with_frames = extract_tables(
        session.as_ref(),
        &ExtractOptions {
            include_frames: true,
            ..Default::default()
        },
    )
    .await
    .expect("extract_tables(include_frames=true)");
    assert_eq!(
        tables_with_frames
            .tables
            .iter()
            .filter(|t| t.rows.iter().any(|r| r.iter().any(|c| c == "XOClosedCell")))
            .count(),
        1
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

async fn assert_extract_tables_handles_non_integer_and_oversized_spans(engine: EngineKind) {
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
        <table id="t1">
          <thead><tr><th>ColA</th><th>ColB</th></tr></thead>
          <tbody><tr><td>A1</td><td>B1</td></tr></tbody>
        </table>
        <table id="t2">
          <tbody>
            <tr><td colspan="1.5">X1</td><td rowspan="1e400">Y1</td></tr>
            <tr><td>X2</td><td>Y2</td></tr>
          </tbody>
        </table>
    </body></html>"#;
    let data_url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session.navigate(&data_url).await.expect("navigate data");

    let res = extract_tables(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_tables");
    assert_eq!(
        res.tables.len(),
        2,
        "{engine}: malformed colspan/rowspan in #t2 must not wipe #t1 or #t2: {:?}",
        res.tables
    );
    assert_eq!(res.tables[0].id.as_deref(), Some("t1"));
    assert_eq!(res.tables[0].headers, vec!["ColA", "ColB"]);
    assert_eq!(res.tables[0].rows, vec![vec!["A1", "B1"]]);
    assert_eq!(res.tables[1].id.as_deref(), Some("t2"));
    assert_eq!(res.tables[1].rows[0], vec!["X1", "Y1"]);

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn extract_tables_handles_non_integer_and_oversized_spans_chromium() {
    assert_extract_tables_handles_non_integer_and_oversized_spans(EngineKind::Chromium).await;
}

#[tokio::test]
async fn extract_tables_handles_non_integer_and_oversized_spans_firefox() {
    assert_extract_tables_handles_non_integer_and_oversized_spans(EngineKind::Firefox).await;
}

#[tokio::test]
async fn extract_closed_shadow_respects_hidden_hosts_and_selector_chromium() {
    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch chromium");
    let pid = session.info().pid.expect("pid");

    let html = r#"<!doctype html><html><body>
        <div id="scoped-sec">
            <div id="scoped-closed-host"></div>
        </div>
        <div id="outside-sec">
            <div id="outside-closed-host"></div>
        </div>
        <div id="closed-disp-none" style="display:none"></div>
        <div id="closed-attr-hidden" hidden></div>
        <div id="closed-aria-hidden" aria-hidden="true"></div>
        <div id="hidden-ancestor" style="display:none"><div id="closed-nested-hidden"></div></div>
        <script>
            const populate = (id, tag) => {
                const sr = document.getElementById(id).attachShadow({ mode: 'closed' });
                sr.innerHTML = `<p>${tag}Para</p><a href="https://example.com/${tag}">${tag}Link</a><table><thead><tr><th>H</th></tr></thead><tbody><tr><td>${tag}Cell</td></tr></tbody></table>`;
            };
            populate('scoped-closed-host', 'ScopedClosed');
            populate('outside-closed-host', 'OutsideClosed');
            populate('closed-disp-none', 'HiddenDisp');
            populate('closed-attr-hidden', 'HiddenAttr');
            populate('closed-aria-hidden', 'HiddenAria');
            populate('closed-nested-hidden', 'HiddenNest');
        </script>
    </body></html>"#;
    let data_url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session.navigate(&data_url).await.expect("navigate data");

    // 1. Default extract_* must include visible closed shadow hosts and exclude all hidden closed shadow hosts
    let md = extract_markdown(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_markdown");
    let txt = extract_text(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_text");
    let links = extract_links(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_links");
    let tables = extract_tables(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_tables");

    assert!(md.markdown.contains("ScopedClosedPara"));
    assert!(txt.text.contains("ScopedClosedPara"));
    assert!(links.links.iter().any(|l| l.text == "ScopedClosedLink"));
    assert!(tables
        .tables
        .iter()
        .any(|t| t.rows.iter().any(|r| r.iter().any(|c| c == "ScopedClosedCell"))));

    for hidden_tag in ["HiddenDisp", "HiddenAttr", "HiddenAria", "HiddenNest"] {
        assert!(
            !md.markdown.contains(hidden_tag),
            "markdown leaked hidden closed shadow {hidden_tag}: {}",
            md.markdown
        );
        assert!(
            !txt.text.contains(hidden_tag),
            "text leaked hidden closed shadow {hidden_tag}: {}",
            txt.text
        );
        assert!(
            !links.links.iter().any(|l| l.text.contains(hidden_tag)),
            "links leaked hidden closed shadow {hidden_tag}: {:?}",
            links.links
        );
        assert!(
            !tables
                .tables
                .iter()
                .any(|t| t.rows.iter().any(|r| r.iter().any(|c| c.contains(hidden_tag)))),
            "tables leaked hidden closed shadow {hidden_tag}: {:?}",
            tables.tables
        );
    }

    // 2. ExtractOptions::selector must include closed shadow DOM inside selector and exclude outside
    let scoped_opts = ExtractOptions {
        selector: Some("#scoped-sec".to_string()),
        include_shadow_dom: true,
        ..Default::default()
    };
    let smd = extract_markdown(session.as_ref(), &scoped_opts)
        .await
        .expect("scoped markdown");
    let stxt = extract_text(session.as_ref(), &scoped_opts)
        .await
        .expect("scoped text");
    let slinks = extract_links(session.as_ref(), &scoped_opts)
        .await
        .expect("scoped links");
    let stables = extract_tables(session.as_ref(), &scoped_opts)
        .await
        .expect("scoped tables");

    assert!(smd.markdown.contains("ScopedClosedPara"), "{}", smd.markdown);
    assert!(!smd.markdown.contains("OutsideClosedPara"), "{}", smd.markdown);
    assert!(stxt.text.contains("ScopedClosedPara"), "{}", stxt.text);
    assert!(!stxt.text.contains("OutsideClosedPara"), "{}", stxt.text);
    assert!(slinks.links.iter().any(|l| l.text == "ScopedClosedLink"));
    assert!(!slinks.links.iter().any(|l| l.text == "OutsideClosedLink"));
    assert!(stables
        .tables
        .iter()
        .any(|t| t.rows.iter().any(|r| r.iter().any(|c| c == "ScopedClosedCell"))));
    assert!(!stables
        .tables
        .iter()
        .any(|t| t.rows.iter().any(|r| r.iter().any(|c| c == "OutsideClosedCell"))));

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

async fn assert_extract_tables_nested_aria_tables_do_not_leak(engine: EngineKind) {
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
        <div id="outer-aria" role="table" aria-label="OuterARIA">
            <div role="row">
                <div role="cell">OuterVal1</div>
                <div role="cell">
                    <div id="inner-grid" role="grid" aria-label="InnerGrid">
                        <div role="row"><div role="columnheader">InCol</div></div>
                        <div role="row"><div role="gridcell">InnerSecretCell</div></div>
                    </div>
                </div>
            </div>
        </div>
        <div id="aria-with-html" role="table" aria-label="AriaWithHtml">
            <div role="row">
                <div role="cell">AriaLeft</div>
                <div role="cell">
                    <table id="nested-html-in-aria">
                        <tr><td>HtmlInner1</td><td>HtmlInner2</td></tr>
                    </table>
                </div>
            </div>
        </div>
    </body></html>"#;
    let data_url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session.navigate(&data_url).await.expect("navigate data");

    let res = extract_tables(session.as_ref(), &ExtractOptions::default())
        .await
        .expect("extract_tables");

    let outer = res
        .tables
        .iter()
        .find(|t| t.id.as_deref() == Some("outer-aria"))
        .expect("outer-aria present");
    assert_eq!(
        outer.row_count, 1,
        "{engine}: nested ARIA grid leaked rows into outer-aria: {outer:?}"
    );
    assert_eq!(
        outer.rows[0].len(),
        2,
        "{engine}: nested ARIA grid leaked cells into outer-aria row: {outer:?}"
    );
    assert_eq!(outer.rows[0][0], "OuterVal1");

    let inner = res
        .tables
        .iter()
        .find(|t| t.id.as_deref() == Some("inner-grid"))
        .expect("inner-grid present");
    assert_eq!(inner.headers, vec!["InCol"]);
    assert_eq!(inner.rows, vec![vec!["InnerSecretCell"]]);

    let aria_with_html = res
        .tables
        .iter()
        .find(|t| t.id.as_deref() == Some("aria-with-html"))
        .expect("aria-with-html present");
    assert_eq!(aria_with_html.row_count, 1);
    assert_eq!(aria_with_html.rows[0].len(), 2);

    let nested_html = res
        .tables
        .iter()
        .find(|t| t.id.as_deref() == Some("nested-html-in-aria"))
        .expect("nested-html-in-aria present");
    assert_eq!(nested_html.rows, vec![vec!["HtmlInner1", "HtmlInner2"]]);

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn extract_tables_nested_aria_tables_do_not_leak_chromium() {
    assert_extract_tables_nested_aria_tables_do_not_leak(EngineKind::Chromium).await;
}

#[tokio::test]
async fn extract_tables_nested_aria_tables_do_not_leak_firefox() {
    assert_extract_tables_nested_aria_tables_do_not_leak(EngineKind::Firefox).await;
}
