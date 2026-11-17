mod support;

use std::collections::BTreeMap;

use mimikyu_core::driver::{
    driver_for, ContextOptions, Cookie, EngineKind, EnvProfile, KeyEvent, KeyEventType,
    LaunchOptions, MouseButton, MouseEvent, MouseEventType, ProfileMode, RouteAction, RouteMock,
    ScreenshotFormat, ScreenshotOpts, WheelEvent,
};
use mimikyu_core::MimikyuError;
use support::TestServer;

async fn exercise_engine_full(engine: EngineKind) {
    let server = TestServer::start().await;
    let driver = driver_for(engine);

    // 1. Default session has main_world_eval = false
    let session = driver
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: false,
            ..Default::default()
        })
        .await
        .expect("launch browser");

    let pid = session.info().pid.expect("pid present");

    // 2. MainWorldDisabled check
    let mw_err = session.eval_main_world("1 + 1").await.unwrap_err();
    assert!(
        matches!(mw_err, MimikyuError::MainWorldDisabled),
        "expected MainWorldDisabled, got {mw_err:?}"
    );

    // 3. Navigation + Redirects
    let nav = session
        .navigate(&server.url("/redirect"))
        .await
        .expect("navigate redirect");
    assert_eq!(nav.url, server.url("/target"));
    assert_eq!(nav.title, "Redirect Target");

    // Navigate back to home
    let home = session.navigate(&server.url("/")).await.expect("navigate /");
    assert_eq!(home.title, "Mimikyu M1 Home");
    assert_eq!(
        session.document_ready_state().await.expect("readyState"),
        "complete"
    );

    // 3b. History navigation (go_back -> /target, go_forward -> /) and reload
    let back_nav = session.go_back().await.expect("go_back");
    assert_eq!(back_nav.url, server.url("/target"));
    let fwd_nav = session.go_forward().await.expect("go_forward");
    assert_eq!(fwd_nav.url, server.url("/"));
    let reloaded = session.reload().await.expect("reload");
    assert_eq!(reloaded.title, "Mimikyu M1 Home");

    // 3c. Init scripts (add_init_script / remove_init_script)
    let init_id = session
        .add_init_script(
            "document.addEventListener('DOMContentLoaded', () => { document.documentElement.setAttribute('data-init-ok', 'yes'); });",
        )
        .await
        .expect("add_init_script");
    session.reload().await.expect("reload after init script");
    let marker = session
        .eval_isolated("document.documentElement.getAttribute('data-init-ok')")
        .await
        .expect("check init marker");
    assert_eq!(marker, "yes");
    session
        .remove_init_script(&init_id)
        .await
        .expect("remove_init_script");

    // 4. Isolated eval vs main-world isolation
    let iso_res = session
        .eval_isolated(
            r#"(() => {
                window.__isoProbe = 'iso-secret';
                return {
                    h1: document.querySelector('h1')?.textContent,
                    seesMainSecret: typeof window.__mainSecret
                };
            })()"#,
        )
        .await
        .expect("eval_isolated");
    assert_eq!(iso_res["h1"], "Ready");
    assert_eq!(iso_res["seesMainSecret"], "undefined");

    // 5. Accessibility tree & bounding box
    let ax = session.accessibility_tree().await.expect("ax tree");
    assert!(!ax.is_empty(), "AX tree should be non-empty");
    assert!(
        ax.iter().any(|n| n.role.eq_ignore_ascii_case("button")
            && n.name.as_deref() == Some("Go")),
        "AX tree should contain button 'Go'"
    );

    let btn_box = session
        .resolve_selector_box("#btn")
        .await
        .expect("resolve #btn box");
    assert!(btn_box.is_visible());
    let (bx, by) = btn_box.center();

    // 6. Chokepoint input: click button + focus input + insert text
    session
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: bx,
            y: by,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #btn");

    let status_text = session
        .eval_isolated("document.getElementById('status').textContent")
        .await
        .expect("read status");
    assert_eq!(status_text, "clicked:1");

    session
        .focus_node(None, Some("#inp"))
        .await
        .expect("focus #inp");
    session
        .insert_text("mimikyu")
        .await
        .expect("insert_text");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "!".to_string(),
            code: None,
            text: Some("!".to_string()),
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key");
    session
        .dispatch_wheel(WheelEvent {
            x: bx,
            y: by,
            delta_x: 0.0,
            delta_y: 40.0,
            modifiers: 0,
        })
        .await
        .expect("dispatch_wheel");
    let inp_val = session
        .eval_isolated("document.getElementById('inp').value")
        .await
        .expect("read #inp value");
    assert_eq!(inp_val, "mimikyu!");

    // 6a. Editing key parity: KeyEventType::Char("Backspace") deletes trailing '!' on both engines
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "Backspace".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Backspace");
    let inp_after_bs = session
        .eval_isolated("document.getElementById('inp').value")
        .await
        .expect("read #inp value after backspace");
    assert_eq!(inp_after_bs, "mimikyu");

    // 6a-2. KeyEventType::Char Enter (form submit + textarea newline), Space, and printable without text
    session
        .navigate(&server.url("/form-keys"))
        .await
        .expect("navigate /form-keys");
    session
        .focus_node(None, Some("#form-inp"))
        .await
        .expect("focus #form-inp");
    session
        .insert_text("hello")
        .await
        .expect("insert_text #form-inp");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "Enter".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Enter on form");
    let form_status = session
        .eval_isolated("document.getElementById('form-status').textContent")
        .await
        .expect("check form submission");
    assert_eq!(
        form_status, "submitted",
        "Enter with KeyEventType::Char and text: None must submit <form>"
    );

    session
        .focus_node(None, Some("#ta"))
        .await
        .expect("focus #ta");
    session
        .insert_text("a")
        .await
        .expect("insert_text a");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "Enter".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Enter on textarea");
    session
        .insert_text("b")
        .await
        .expect("insert_text b");
    let ta_val = session
        .eval_isolated("document.getElementById('ta').value")
        .await
        .expect("read #ta value");
    assert_eq!(
        ta_val, "a\nb",
        "Enter with KeyEventType::Char and text: None must insert newline in <textarea>"
    );

    session
        .focus_node(None, Some("#inp2"))
        .await
        .expect("focus #inp2");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "Z".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Z without text");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "Space".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Space without text");
    session
        .insert_text("ok")
        .await
        .expect("insert_text ok");
    let inp2_val = session
        .eval_isolated("document.getElementById('inp2').value")
        .await
        .expect("read #inp2 value");
    assert_eq!(
        inp2_val, "Z ok",
        "printable key without text and Space must insert 'Z ok'"
    );

    // 6a-3. Function keys (F1, F12) & Shift+a Char parity on both engines
    session
        .eval_isolated(
            "window.__keyEvents = []; document.addEventListener('keydown', (e) => { window.__keyEvents.push(e.key + ':' + e.shiftKey); });",
        )
        .await
        .expect("attach keydown listener");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "F1".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key F1");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "F12".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key F12");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Down,
            key: "Shift".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Shift down");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Char,
            key: "a".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 8,
        })
        .await
        .expect("dispatch_key Shift+a Char");
    session
        .dispatch_key(KeyEvent {
            event_type: KeyEventType::Up,
            key: "Shift".to_string(),
            code: None,
            text: None,
            windows_virtual_key_code: None,
            modifiers: 0,
        })
        .await
        .expect("dispatch_key Shift up");

    let key_events = session
        .eval_isolated("window.__keyEvents")
        .await
        .expect("read __keyEvents");
    let key_events_arr = key_events.as_array().expect("__keyEvents array");
    assert!(
        key_events_arr.iter().any(|v| v.as_str() == Some("F1:false")),
        "expected F1 keydown event on {engine}: {key_events_arr:?}"
    );
    assert!(
        key_events_arr.iter().any(|v| v.as_str() == Some("F12:false")),
        "expected F12 keydown event on {engine}: {key_events_arr:?}"
    );
    assert!(
        key_events_arr.iter().any(|v| v.as_str() == Some("A:true")),
        "expected Shift+a keydown 'A:true' on {engine}: {key_events_arr:?}"
    );
    let inp2_after_shift = session
        .eval_isolated("document.getElementById('inp2').value")
        .await
        .expect("read #inp2 after Shift+a");
    assert_eq!(
        inp2_after_shift, "Z okA",
        "Shift+a Char must insert 'A' on {engine}"
    );

    // Navigate back to / for subsequent checks (#file-inp, screenshot, etc.)
    session
        .navigate(&server.url("/"))
        .await
        .expect("navigate back to /");

    // 6b. File input upload
    let tmp_file = tempfile::NamedTempFile::new().expect("temp file");
    std::fs::write(tmp_file.path(), b"hello-upload").expect("write temp file");
    session
        .set_file_input_files(None, Some("#file-inp"), vec![tmp_file.path().to_path_buf()])
        .await
        .expect("set_file_input_files");
    let file_count = session
        .eval_isolated("document.getElementById('file-inp').files.length")
        .await
        .expect("files length");
    assert_eq!(file_count, 1);

    // 6c. Env overrides & instant animations + invalid timezone error propagation
    let bad_tz_err = session
        .set_env_overrides(EnvProfile {
            timezone: "Invalid/DoesNotExist".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("invalid timezone should return Err");
    assert!(
        matches!(bad_tz_err, MimikyuError::Protocol { .. }),
        "expected Protocol error for invalid timezone, got {bad_tz_err:?}"
    );

    let env_prof = EnvProfile {
        viewport_width: 1024,
        viewport_height: 768,
        timezone: "Europe/London".to_string(),
        locale: "en-GB".to_string(),
        ..Default::default()
    };
    session
        .set_env_overrides(env_prof)
        .await
        .expect("set_env_overrides");
    let tz = session
        .eval_isolated("Intl.DateTimeFormat().resolvedOptions().timeZone")
        .await
        .expect("resolved tz");
    assert_eq!(tz, "Europe/London");
    session
        .set_instant_animations(true)
        .await
        .expect("set_instant_animations");

    // 6d. Tabs: new_tab, list_tabs (title + active tracking), activate_tab
    let tab2 = session
        .new_tab(Some(&server.url("/target")))
        .await
        .expect("new_tab");
    assert_eq!(tab2.title().await.expect("tab2 title"), "Redirect Target");
    let tabs = session.list_tabs().await.expect("list_tabs");
    assert!(tabs.len() >= 2, "expected at least 2 tabs, got {tabs:?}");
    let t2_info = tabs
        .iter()
        .find(|t| t.target_id == tab2.info().target_id)
        .expect("tab2 present in list_tabs");
    assert!(t2_info.active, "newly created tab2 should be active: {tabs:?}");
    assert_eq!(t2_info.title, "Redirect Target");

    session
        .activate_tab(&session.info().target_id)
        .await
        .expect("activate_tab back to root");
    let tabs_after_act = session.list_tabs().await.expect("list_tabs after activate");
    let root_info = tabs_after_act
        .iter()
        .find(|t| t.target_id == session.info().target_id)
        .expect("root tab present");
    assert!(
        root_info.active,
        "root tab should be active after activate_tab: {tabs_after_act:?}"
    );
    assert!(
        !tabs_after_act
            .iter()
            .find(|t| t.target_id == tab2.info().target_id)
            .expect("tab2 present")
            .active,
        "tab2 should no longer be active"
    );
    tab2.dispose().await.expect("dispose tab2");

    // 6d-2. Disposing an active non-root tab immediately removes it from list_tabs() and marks remaining tab active
    let tab3 = session
        .new_tab(Some(&server.url("/target")))
        .await
        .expect("new_tab tab3");
    let tab3_id = tab3.info().target_id.clone();
    let tabs_with_t3 = session.list_tabs().await.expect("list_tabs with tab3");
    assert!(
        tabs_with_t3
            .iter()
            .find(|t| t.target_id == tab3_id)
            .expect("tab3 present")
            .active,
        "tab3 should be active before dispose: {tabs_with_t3:?}"
    );
    tab3.dispose().await.expect("dispose active tab3");
    let tabs_after_t3_dispose = session
        .list_tabs()
        .await
        .expect("list_tabs immediately after tab3.dispose()");
    assert!(
        !tabs_after_t3_dispose.iter().any(|t| t.target_id == tab3_id),
        "disposed tab3 must be immediately absent from list_tabs(): {tabs_after_t3_dispose:?}"
    );
    let root_after_dispose = tabs_after_t3_dispose
        .iter()
        .find(|t| t.target_id == session.info().target_id)
        .expect("root tab present after tab3 dispose");
    assert!(
        root_after_dispose.active,
        "remaining root tab should be active immediately after active tab3.dispose(): {tabs_after_t3_dispose:?}"
    );

    // 7. Screenshot & PDF
    let png = session
        .screenshot(ScreenshotOpts {
            format: ScreenshotFormat::Png,
            ..Default::default()
        })
        .await
        .expect("screenshot png");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "valid PNG header");

    let pdf = session.pdf().await.expect("pdf");
    assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");

    // 8. Virtual context isolation (new_context) + main_world_eval opt-in
    let ctx_a = session
        .new_context(ContextOptions {
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("new_context A");
    let ctx_b = session
        .new_context(ContextOptions {
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("new_context B");

    // Verify main_world_eval works when opted in & doesn't see isolated global
    ctx_a
        .navigate(&server.url("/isolation"))
        .await
        .expect("ctx_a navigate");
    ctx_a
        .eval_isolated("window.__ctxAIso = 'hidden'; localStorage.setItem('k', 'A');")
        .await
        .expect("ctx_a isolated");
    let mw_val = ctx_a
        .eval_main_world("typeof window.__ctxAIso")
        .await
        .expect("ctx_a main world");
    assert_eq!(mw_val, "undefined");

    ctx_a
        .set_cookies(vec![Cookie {
            name: "ctx_cookie".to_string(),
            value: "valA".to_string(),
            domain: "127.0.0.1".to_string(),
            path: "/".to_string(),
            expires: None,
            http_only: false,
            secure: false,
            same_site: None,
        }])
        .await
        .expect("ctx_a set_cookies");

    ctx_b
        .navigate(&server.url("/isolation"))
        .await
        .expect("ctx_b navigate");
    let b_ls = ctx_b
        .eval_isolated("localStorage.getItem('k')")
        .await
        .expect("ctx_b ls");
    assert!(b_ls.is_null(), "ctx_b localStorage should be isolated, got {b_ls:?}");
    let b_cookies = ctx_b.cookies().await.expect("ctx_b cookies");
    assert!(
        !b_cookies.iter().any(|c| c.name == "ctx_cookie"),
        "ctx_b should not see ctx_a's cookie"
    );

    // Verify list_tabs() isolation between root and VirtualContext sessions
    let root_tabs = session.list_tabs().await.expect("root list_tabs");
    assert!(
        !root_tabs.iter().any(|t| t.target_id == ctx_a.info().target_id || t.target_id == ctx_b.info().target_id),
        "root list_tabs() must not leak virtual context tabs: {root_tabs:?}"
    );
    let ctx_a_tabs = ctx_a.list_tabs().await.expect("ctx_a list_tabs");
    assert_eq!(ctx_a_tabs.len(), 1, "ctx_a should see only its own tab: {ctx_a_tabs:?}");
    assert_eq!(ctx_a_tabs[0].target_id, ctx_a.info().target_id);
    assert!(ctx_a_tabs[0].active);

    ctx_a.dispose().await.expect("dispose ctx_a");
    ctx_b.dispose().await.expect("dispose ctx_b");

    // 9. Console logs & network events
    session
        .navigate(&server.url("/console"))
        .await
        .expect("navigate /console");
    // Brief yield to ensure console event arrives on reader task
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let logs = session.console_logs().await.expect("console_logs");
    assert!(
        logs.iter().any(|l| l.text.contains("m1-log")),
        "expected m1-log in console logs, got {logs:?}"
    );

    let net = session.network_events().await.expect("network_events");
    assert!(
        net.iter().any(|e| e.url.ends_with("/console") && e.status == Some(200)),
        "expected /console 200 in network_events"
    );

    // 10. Cookies + storage_state / set_storage_state + clear_cookies
    session
        .navigate(&server.url("/cookie"))
        .await
        .expect("navigate /cookie");
    session
        .eval_isolated("localStorage.setItem('persist_k', 'persist_v')")
        .await
        .expect("set localStorage");
    let st = session.storage_state().await.expect("storage_state");
    assert!(
        st.cookies
            .iter()
            .any(|c| c.name == "mimikyu_test" && c.value == "1"),
        "expected mimikyu_test=1 cookie in storage_state, got {:?}",
        st.cookies
    );
    assert!(
        st.origins
            .iter()
            .any(|o| o.local_storage.get("persist_k").map(String::as_str) == Some("persist_v")),
        "expected persist_k in storage_state origins"
    );
    session.clear_cookies().await.expect("clear_cookies");
    let after_clear = session.cookies().await.expect("cookies after clear");
    assert!(after_clear.is_empty());
    session
        .set_storage_state(st)
        .await
        .expect("set_storage_state");
    let restored = session.cookies().await.expect("cookies after restore");
    assert!(restored.iter().any(|c| c.name == "mimikyu_test"));
    let _inflight = session
        .inflight_requests()
        .await
        .expect("inflight_requests");

    // 11. Blocked URLs & Route Mock
    session
        .navigate(&server.url("/network"))
        .await
        .expect("navigate /network");
    session
        .set_blocked_urls(vec!["*blocked.png*".to_string()])
        .await
        .expect("set_blocked_urls");
    let mut mock_headers = BTreeMap::new();
    mock_headers.insert("content-type".to_string(), "application/json".to_string());
    session
        .set_route_mock(RouteMock {
            id: "mock1".to_string(),
            url_pattern: "*/api/mock*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_headers,
                body: br#"{"mocked":true}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    let fetch_res = session
        .eval_isolated(
            r#"(async () => {
                let blockedOk = false;
                try {
                    const r = await fetch('/blocked.png');
                    blockedOk = r.ok;
                } catch {
                    blockedOk = false;
                }
                const mocked = await (await fetch('/api/mock')).json();
                const real = await (await fetch('/api/real')).json();
                return { blockedOk, mocked, real };
            })()"#,
        )
        .await
        .expect("fetch test");

    assert_eq!(fetch_res["blockedOk"], false);
    assert_eq!(fetch_res["mocked"]["mocked"], true);
    assert_eq!(fetch_res["real"]["real"], true);

    let hits = server.hits();
    assert!(
        !hits.iter().any(|h| h.contains("/blocked.png")),
        "blocked.png must not hit server; hits={hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.contains("/api/mock")),
        "/api/mock must be fulfilled locally; hits={hits:?}"
    );
    assert!(
        hits.iter().any(|h| h.contains("/api/real")),
        "/api/real should hit server; hits={hits:?}"
    );

    // 11b. Blocked URLs + Route Mock with <iframe> subframe navigation (must not hang in BiDi or CDP)
    let iframe_nav = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session.navigate(&server.url("/iframe-page")),
    )
    .await
    .expect("iframe navigation with active intercept must not hang")
    .expect("navigate /iframe-page");
    assert_eq!(iframe_nav.title, "Iframe Host");

    let sub_text = session
        .eval_isolated("document.getElementById('subframe').contentDocument.getElementById('sub-ok').textContent")
        .await
        .expect("read subframe DOM");
    assert_eq!(sub_text, "subframe-loaded");

    let hits_after_iframe = server.hits();
    assert!(
        hits_after_iframe.iter().any(|h| h.contains("/subframe")),
        "subframe request should reach server; hits={hits_after_iframe:?}"
    );
    assert!(
        !hits_after_iframe.iter().any(|h| h.contains("/blocked.png")),
        "blocked.png inside iframe must still be blocked; hits={hits_after_iframe:?}"
    );

    // 12. Dispose & verify zero orphan process group
    session.dispose().await.expect("dispose root");
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
    assert_eq!(rc, -1, "browser process group should be gone after dispose");
}

#[tokio::test]
async fn live_chromium_cdp_integration() {
    exercise_engine_full(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_firefox_bidi_integration() {
    exercise_engine_full(EngineKind::Firefox).await;
}
