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
        .set_file_input_files(None, None, Some("#file-inp"), vec![tmp_file.path().to_path_buf()])
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

async fn wait_for_download_file(path: &std::path::Path, expected_len: usize) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(bytes) = std::fs::read(path)
            && bytes.len() == expected_len
        {
            return bytes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for downloaded file {} ({expected_len} bytes)",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn wait_for_server_hit_count(server: &TestServer, needle: &str, min_count: usize) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let count = server
            .hits()
            .iter()
            .filter(|h| h.contains(needle))
            .count();
        if count >= min_count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {min_count} server hit(s) matching '{needle}', got {count}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn assert_pool_download_dir_empty(pool_profile_dl: &std::path::Path) {
    if !pool_profile_dl.exists() {
        return;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let pool_entries: Vec<_> = std::fs::read_dir(pool_profile_dl)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if pool_entries.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pool_download_dir must be empty after relocation/cleanup, got {pool_entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn live_chromium_download_behavior_ephemeral_and_virtual() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl_ephem = tmp.path().join("ephem-dl");
    let dl_ctx = tmp.path().join("ctx-dl");
    let expected_payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();

    let driver = driver_for(EngineKind::Chromium);
    let session = driver
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            download_dir: Some(dl_ephem.clone()),
            ..Default::default()
        })
        .await
        .expect("launch chromium with download_dir");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/download-page"))
        .await
        .expect("navigate /download-page");
    let link_box = session
        .resolve_selector_box("#dl-bin")
        .await
        .expect("resolve #dl-bin");
    let (lx, ly) = link_box.center();
    session
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: lx,
            y: ly,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-bin");

    let ephem_file = dl_ephem.join("sample-rust-test.bin");
    let got_ephem = wait_for_download_file(&ephem_file, expected_payload.len()).await;
    assert_eq!(got_ephem, expected_payload);
    assert!(
        server
            .hits()
            .iter()
            .any(|h| h.contains("/download/sample-rust-test.bin")),
        "expected /download/sample-rust-test.bin server hit"
    );

    // Virtual context download isolation into dl_ctx (not dl_ephem)
    let ctx = session
        .new_context(ContextOptions {
            download_dir: Some(dl_ctx.clone()),
            ..Default::default()
        })
        .await
        .expect("new_context with download_dir");
    ctx.navigate(&server.url("/download-page"))
        .await
        .expect("ctx navigate /download-page");
    let ctx_box = ctx
        .resolve_selector_box("#dl-bin")
        .await
        .expect("ctx resolve #dl-bin");
    let (cx, cy) = ctx_box.center();
    ctx.dispatch_mouse(MouseEvent {
        event_type: MouseEventType::Click,
        x: cx,
        y: cy,
        button: MouseButton::Left,
        click_count: 1,
        modifiers: 0,
    })
    .await
    .expect("ctx click #dl-bin");

    let ctx_file = dl_ctx.join("sample-rust-test.bin");
    let got_ctx = wait_for_download_file(&ctx_file, expected_payload.len()).await;
    assert_eq!(got_ctx, expected_payload);
    assert!(
        !dl_ephem.join("sample-rust-test.bin (1)").exists(),
        "virtual context must not download into root ephemeral dir"
    );

    if let Some(home_dl) = dirs::download_dir() {
        assert!(
            !home_dl.join("sample-rust-test.bin").exists(),
            "must not write into user Downloads directory"
        );
    }

    ctx.dispose().await.expect("dispose ctx");
    session.dispose().await.expect("dispose session");
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
    assert_eq!(rc, -1, "browser process group should be gone after dispose");
}

#[tokio::test]
async fn live_firefox_download_isolation_ephemeral() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl_ff = tmp.path().join("ff-dl");
    let expected_payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();

    let driver = driver_for(EngineKind::Firefox);
    let session = driver
        .launch(LaunchOptions {
            engine: EngineKind::Firefox,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            download_dir: Some(dl_ff.clone()),
            ..Default::default()
        })
        .await
        .expect("launch firefox with download_dir");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/download-page"))
        .await
        .expect("navigate /download-page");
    let link_box = session
        .resolve_selector_box("#dl-bin")
        .await
        .expect("resolve #dl-bin");
    let (lx, ly) = link_box.center();
    session
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: lx,
            y: ly,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-bin");

    let ff_file = dl_ff.join("sample-rust-test.bin");
    let got_ff = wait_for_download_file(&ff_file, expected_payload.len()).await;
    assert_eq!(got_ff, expected_payload);
    assert!(
        server
            .hits()
            .iter()
            .any(|h| h.contains("/download/sample-rust-test.bin")),
        "expected /download/sample-rust-test.bin server hit"
    );

    if let Some(home_dl) = dirs::download_dir() {
        assert!(
            !home_dl.join("sample-rust-test.bin").exists(),
            "Firefox must not write into user Downloads directory"
        );
    }

    session.dispose().await.expect("dispose firefox session");
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
    assert_eq!(rc, -1, "firefox process group should be gone after dispose");
}

#[tokio::test]
async fn live_firefox_virtual_context_download_isolation_concurrent() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl1 = tmp.path().join("v1-dl");
    let dl2 = tmp.path().join("v2-dl");
    let expected_payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();

    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;
    let pool_profile_dl = pool
        .root_session()
        .info()
        .profile_dir
        .clone()
        .unwrap()
        .join("downloads");

    let vc1 = pool
        .create_labeled_context(
            "v1",
            ContextOptions {
                download_dir: Some(dl1.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc1");
    let vc2 = pool
        .create_labeled_context(
            "v2",
            ContextOptions {
                download_dir: Some(dl2.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc2");

    vc1.session()
        .navigate(&server.url("/download-page"))
        .await
        .expect("nav v1");
    vc2.session()
        .navigate(&server.url("/download-page"))
        .await
        .expect("nav v2");

    let b1 = vc1.session().resolve_selector_box("#dl-bin").await.unwrap();
    let b2 = vc2.session().resolve_selector_box("#dl-bin").await.unwrap();
    let (x1, y1) = b1.center();
    let (x2, y2) = b2.center();

    let (r1, r2) = tokio::join!(
        vc1.session().dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x1,
            y: y1,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        }),
        vc2.session().dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x2,
            y: y2,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
    );
    r1.expect("click v1");
    r2.expect("click v2");

    let got1 = wait_for_download_file(&dl1.join("sample-rust-test.bin"), expected_payload.len()).await;
    let got2 = wait_for_download_file(&dl2.join("sample-rust-test.bin"), expected_payload.len()).await;
    assert_eq!(got1, expected_payload);
    assert_eq!(got2, expected_payload);
    assert!(
        !pool_profile_dl.join("sample-rust-test.bin").exists()
            && !pool_profile_dl.join("sample-rust-test(1).bin").exists(),
        "pool shared downloads dir must not retain colliding files"
    );

    vc1.dispose().await.expect("dispose vc1");
    vc2.dispose().await.expect("dispose vc2");
    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

#[tokio::test]
async fn live_firefox_virtual_context_slow_and_aborted_download_isolation() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl1 = tmp.path().join("v1-dl");
    let dl2 = tmp.path().join("v2-dl");
    let slow_payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let fast_payload: Vec<u8> = (0..512).map(|i| ((i + 7) % 251) as u8).collect();

    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;
    let pool_profile_dl = pool
        .root_session()
        .info()
        .profile_dir
        .clone()
        .unwrap()
        .join("downloads");

    let vc1 = pool
        .create_labeled_context(
            "slow-v1",
            ContextOptions {
                download_dir: Some(dl1.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc1");
    let vc2 = pool
        .create_labeled_context(
            "fast-v2",
            ContextOptions {
                download_dir: Some(dl2.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc2");

    vc1.session()
        .navigate(&server.url("/download-slow-page"))
        .await
        .expect("nav v1");
    vc2.session()
        .navigate(&server.url("/download-slow-page"))
        .await
        .expect("nav v2");

    let b_slow = vc1.session().resolve_selector_box("#dl-slow").await.unwrap();
    let b_fast = vc2.session().resolve_selector_box("#dl-fast").await.unwrap();
    let (sx, sy) = b_slow.center();
    let (fx, fy) = b_fast.center();

    // Start slow (>300ms) download in vc1, confirm server received it, then trigger fast same-name download in vc2
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: sx,
            y: sy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-slow");
    wait_for_server_hit_count(&server, "/download/slow-same.bin", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    vc2.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: fx,
            y: fy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-fast");

    let got_slow = wait_for_download_file(&dl1.join("same.bin"), slow_payload.len()).await;
    let got_fast = wait_for_download_file(&dl2.join("same.bin"), fast_payload.len()).await;
    assert_eq!(got_slow, slow_payload, "slow download must not truncate to 0 bytes");
    assert_eq!(got_fast, fast_payload, "fast concurrent download must preserve exact bytes");

    // Aborted transfer in vc1 must NOT relocate a 0-byte placeholder into dl1
    let b_abort = vc1.session().resolve_selector_box("#dl-abort").await.unwrap();
    let (ax, ay) = b_abort.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: ax,
            y: ay,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-abort");

    // Genuine Content-Length: 0 download in vc2 must still relocate cleanly
    let b_empty = vc2.session().resolve_selector_box("#dl-empty").await.unwrap();
    let (ex, ey) = b_empty.center();
    vc2.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: ex,
            y: ey,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-empty");

    let got_empty = wait_for_download_file(&dl2.join("empty.bin"), 0).await;
    assert!(got_empty.is_empty());

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert!(
        !dl1.join("abort.bin").exists(),
        "aborted transfer must not relocate 0-byte placeholder into dl1"
    );
    let dl1_entries: Vec<_> = std::fs::read_dir(&dl1)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        dl1_entries,
        vec!["same.bin".to_string()],
        "dl1 should contain only same.bin, got {dl1_entries:?}"
    );
    assert_pool_download_dir_empty(&pool_profile_dl).await;

    vc1.dispose().await.expect("dispose vc1");
    vc2.dispose().await.expect("dispose vc2");
    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

#[tokio::test]
async fn live_firefox_virtual_context_popup_download_isolation() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl1 = tmp.path().join("v1-popup-dl");
    let dl2 = tmp.path().join("v2-popup-dl");
    let popup_slow_payload: Vec<u8> = (0..4096).map(|i| ((i * 3 + 11) % 251) as u8).collect();
    let popup_direct_payload: Vec<u8> = (0..1024).map(|i| ((i * 7 + 1) % 251) as u8).collect();

    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;
    let pool_profile_dl = pool
        .root_session()
        .info()
        .profile_dir
        .clone()
        .unwrap()
        .join("downloads");

    let vc1 = pool
        .create_labeled_context(
            "popup-v1",
            ContextOptions {
                download_dir: Some(dl1.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc1");
    let vc2 = pool
        .create_labeled_context(
            "popup-v2",
            ContextOptions {
                download_dir: Some(dl2.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc2");

    vc1.session()
        .navigate(&server.url("/popup-download-host"))
        .await
        .expect("nav vc1 /popup-download-host");

    let b_popup = vc1
        .session()
        .resolve_selector_box("#popup-dl")
        .await
        .expect("resolve #popup-dl");
    let (px, py) = b_popup.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: px,
            y: py,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #popup-dl");

    let got_popup_slow =
        wait_for_download_file(&dl1.join("popup-slow.bin"), popup_slow_payload.len()).await;
    assert_eq!(got_popup_slow, popup_slow_payload);

    let b_blank = vc1
        .session()
        .resolve_selector_box("#blank-dl")
        .await
        .expect("resolve #blank-dl");
    let (bx, by) = b_blank.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: bx,
            y: by,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #blank-dl");

    let got_popup_direct =
        wait_for_download_file(&dl1.join("popup-direct.bin"), popup_direct_payload.len()).await;
    assert_eq!(got_popup_direct, popup_direct_payload);

    let dl2_entries: Vec<_> = std::fs::read_dir(&dl2)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        dl2_entries.is_empty(),
        "vc2 download_dir must not receive vc1 popup downloads: {dl2_entries:?}"
    );
    assert_pool_download_dir_empty(&pool_profile_dl).await;

    vc1.dispose().await.expect("dispose vc1");
    vc2.dispose().await.expect("dispose vc2");
    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

#[tokio::test]
async fn live_firefox_virtual_context_same_tab_same_url_concurrent_download_isolation() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl1 = tmp.path().join("v1-same-url-dl");
    let dl2 = tmp.path().join("v2-zero-dl");
    let ok_payload: Vec<u8> = (0..4096).map(|i| ((i * 5 + 3) % 251) as u8).collect();
    let hold_payload: Vec<u8> = (0..4096).map(|i| ((i + 19) % 251) as u8).collect();

    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;
    let pool_profile_dl = pool
        .root_session()
        .info()
        .profile_dir
        .clone()
        .unwrap()
        .join("downloads");

    let vc1 = pool
        .create_labeled_context(
            "same-url-v1",
            ContextOptions {
                download_dir: Some(dl1.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc1");
    let vc2 = pool
        .create_labeled_context(
            "zero-byte-v2",
            ContextOptions {
                download_dir: Some(dl2.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc2");

    vc1.session()
        .navigate(&server.url("/download-same-url-concurrent-page"))
        .await
        .expect("nav vc1 /download-same-url-concurrent-page");
    vc2.session()
        .navigate(&server.url("/download-same-url-concurrent-page"))
        .await
        .expect("nav vc2 /download-same-url-concurrent-page");

    let b1 = vc1.session().resolve_selector_box("#d1").await.unwrap();
    let b2 = vc1.session().resolve_selector_box("#d2").await.unwrap();
    let (x1, y1) = b1.center();
    let (x2, y2) = b2.center();

    // Start slow 4096B transfer (#d1), wait until server receives seq #1, then start second same-URL transfer (#d2) that aborts
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x1,
            y: y1,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #d1");
    wait_for_server_hit_count(&server, "/download/flaky-same.bin", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x2,
            y: y2,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #d2");

    let got_ok = wait_for_download_file(&dl1.join("ok.bin"), ok_payload.len()).await;
    assert_eq!(got_ok, ok_payload, "first same-URL transfer ok.bin must succeed with 4096B");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        !dl1.join("fail.bin").exists(),
        "aborted second same-URL transfer must not produce fail.bin"
    );

    // Also verify 0-byte download in vc2 relocates promptly even while vc1 has a slow .part download active
    let b_hold = vc1.session().resolve_selector_box("#d-hold").await.unwrap();
    let b_empty = vc2.session().resolve_selector_box("#d-empty").await.unwrap();
    let (hx, hy) = b_hold.center();
    let (ex, ey) = b_empty.center();

    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: hx,
            y: hy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #d-hold");
    wait_for_server_hit_count(&server, "/download/slow-hold.bin", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let empty_t0 = tokio::time::Instant::now();
    vc2.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: ex,
            y: ey,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #d-empty");

    let got_empty = tokio::time::timeout(
        std::time::Duration::from_millis(1800),
        wait_for_download_file(&dl2.join("empty.bin"), 0),
    )
    .await
    .expect("0-byte download must relocate promptly without stalling on unrelated .part file");
    assert!(got_empty.is_empty());
    let empty_elapsed = empty_t0.elapsed();
    assert!(
        !dl1.join("slow-hold.bin").exists(),
        "0-byte download ({empty_elapsed:?}) must finish before 2200ms slow-hold.bin completes"
    );

    let got_hold = wait_for_download_file(&dl1.join("slow-hold.bin"), hold_payload.len()).await;
    assert_eq!(got_hold, hold_payload);

    let mut dl1_entries: Vec<_> = std::fs::read_dir(&dl1)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    dl1_entries.sort();
    assert_eq!(
        dl1_entries,
        vec!["ok.bin".to_string(), "slow-hold.bin".to_string()],
        "dl1 should contain only ok.bin and slow-hold.bin, got {dl1_entries:?}"
    );
    assert_pool_download_dir_empty(&pool_profile_dl).await;

    vc1.dispose().await.expect("dispose vc1");
    vc2.dispose().await.expect("dispose vc2");
    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

async fn assert_display_contents_snapshot_and_click(engine: EngineKind) {
    let driver = driver_for(engine);
    let session = driver
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid") as libc::pid_t;

    let html = r##"<!doctype html><html><body>
        <button id="dc-btn" style="display: contents" onclick="document.title='dc-clicked'">DC Button</button>
        <div><button id="wrap-btn">Wrapped Button</button></div>
        <a id="dc-link" href="#ok" style="display: contents">DC Link</a>
        <span id="page-indicator">Page 1 of 2</span>
    </body></html>"##;
    let url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session.navigate(&url).await.expect("navigate");

    let mut snap_state = mimikyu_core::snapshot::SnapshotState::default();
    let snap = mimikyu_core::snapshot::SnapshotEngine::capture(
        session.as_ref(),
        &mut snap_state,
        mimikyu_core::snapshot::SnapshotOptions::default(),
    )
    .await
    .expect("snapshot");

    assert!(
        snap.text.contains("DC Button"),
        "{engine}: missing display:contents button in snapshot:\n{}",
        snap.text
    );
    assert!(
        snap.text.contains("DC Link"),
        "{engine}: missing display:contents link in snapshot:\n{}",
        snap.text
    );
    assert!(
        snap.text.contains("Page 1 of 2"),
        "{engine}: missing inline span text in snapshot:\n{}",
        snap.text
    );

    let dc_ref = snap
        .ref_map
        .iter()
        .find(|(_, e)| e.name.as_deref() == Some("DC Button"))
        .map(|(k, _)| k.clone())
        .expect("DC Button ref");
    let chokepoint = mimikyu_core::input::ChokepointInput::new(session.clone());
    chokepoint
        .click(
            &mimikyu_core::input::InputTarget::Ref(dc_ref),
            Some(&snap_state.active_ref_map),
            &mimikyu_core::input::ClickOptions::default(),
        )
        .await
        .expect("click display:contents button");
    assert_eq!(session.title().await.unwrap(), "dc-clicked");

    session.dispose().await.expect("dispose");
    support::assert_pgid_gone(pid);
}

#[tokio::test]
async fn live_display_contents_and_inline_text_parity_chromium() {
    assert_display_contents_snapshot_and_click(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_display_contents_and_inline_text_parity_firefox() {
    assert_display_contents_snapshot_and_click(EngineKind::Firefox).await;
}

#[tokio::test]
async fn live_firefox_scrollbar_width_thin_does_not_falsely_occlude() {
    let driver = driver_for(EngineKind::Firefox);

    // 1. Force overlay scrollbars (`ui.useOverlayScrollbars = 1`) so this test deterministically
    // exercises Firefox's overlay scrollbar gutter detection even on headless macOS VMs where no
    // trackpad is attached (which otherwise default to classic layout scrollbars with `cw < iw`).
    let session = driver
        .launch(LaunchOptions {
            engine: EngineKind::Firefox,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            extra_prefs: BTreeMap::from([(
                "ui.useOverlayScrollbars".to_string(),
                serde_json::json!(1),
            )]),
            ..Default::default()
        })
        .await
        .expect("launch firefox with overlay scrollbars");
    let pid = session.info().pid.expect("pid") as libc::pid_t;

    let html = r#"<!doctype html><html style="scrollbar-width: thin;"><body style="margin:0;height:3000px;">
        <button id="thin-btn" style="position:fixed;top:120px;right:10px;width:6px;height:24px;padding:0;border:0;" onclick="document.title='thin-ok'">T</button>
        <button id="b-gutter" style="position:fixed;top:160px;right:2px;width:4px;height:24px;padding:0;border:0;" onclick="document.title='gutter-clicked'">G</button>
    </body></html>"#;
    let url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session.navigate(&url).await.expect("navigate thin page");

    let (thin_gutter_r, _) = session
        .scrollbar_gutters()
        .await
        .expect("thin scrollbar_gutters");
    assert!(
        (6.0..=12.0).contains(&thin_gutter_r),
        "expected thin overlay scrollbar gutter in 6..=12px, got {thin_gutter_r}"
    );
    // Position `#b-gutter` right at the inner edge of the measured `[iw - thin_gutter_r, iw)` gutter.
    let gutter_right_px = (thin_gutter_r - 4.0).max(0.0);
    session
        .eval_isolated(&format!(
            "(() => {{ document.getElementById('b-gutter').style.right = '{gutter_right_px}px'; return true; }})()"
        ))
        .await
        .expect("position #b-gutter inside measured thin scrollbar gutter");

    let chokepoint = mimikyu_core::input::ChokepointInput::new(session.clone());
    chokepoint
        .click(
            &mimikyu_core::input::InputTarget::Selector("#thin-btn".into()),
            None,
            &mimikyu_core::input::ClickOptions::default(),
        )
        .await
        .expect("click near thin scrollbar must not falsely fail occlusion");
    assert_eq!(session.title().await.unwrap(), "thin-ok");

    let gutter_err = chokepoint
        .click(
            &mimikyu_core::input::InputTarget::Selector("#b-gutter".into()),
            None,
            &mimikyu_core::input::ClickOptions::default(),
        )
        .await
        .expect_err("element inside measured thin scrollbar gutter must be rejected as occluded");
    assert!(
        matches!(gutter_err, MimikyuError::ElementNotFound(_)),
        "expected ElementNotFound for occluded gutter button, got {gutter_err:?}"
    );

    // scrollbar-width: thin on <body> does NOT shrink Firefox's 16px viewport root scrollbar
    let body_thin_html = r#"<!doctype html><html><body style="margin:0;height:3000px;scrollbar-width:thin;">
        <button id="body-thin-btn" style="position:fixed;top:120px;right:10px;width:6px;height:24px;padding:0;border:0;" onclick="document.title='body-thin-clicked'">B</button>
    </body></html>"#;
    let body_thin_url = format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(body_thin_html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    session
        .navigate(&body_thin_url)
        .await
        .expect("navigate body thin page");
    let body_thin_err = chokepoint
        .click(
            &mimikyu_core::input::InputTarget::Selector("#body-thin-btn".into()),
            None,
            &mimikyu_core::input::ClickOptions::default(),
        )
        .await
        .expect_err("body { scrollbar-width: thin } must preserve 16px root gutter and reject right:10px,width:6px");
    assert!(
        matches!(body_thin_err, MimikyuError::ElementNotFound(_)),
        "expected ElementNotFound for body-thin occluded button, got {body_thin_err:?}"
    );

    session.dispose().await.expect("dispose");
    support::assert_pgid_gone(pid);

    // 2. Also verify classic layout scrollbars (`ui.useOverlayScrollbars = 0`, matching headless
    // macOS CI VMs with no trackpad): `clientWidth < innerWidth`, `scrollbar_gutters()` returns 0,
    // and fixed `right: 8px` elements sit left of the classic scrollbar and click cleanly.
    let classic_session = driver
        .launch(LaunchOptions {
            engine: EngineKind::Firefox,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            extra_prefs: BTreeMap::from([(
                "ui.useOverlayScrollbars".to_string(),
                serde_json::json!(0),
            )]),
            ..Default::default()
        })
        .await
        .expect("launch firefox with classic scrollbars");
    let classic_pid = classic_session.info().pid.expect("classic pid") as libc::pid_t;
    classic_session
        .navigate(&url)
        .await
        .expect("navigate classic thin page");
    let (classic_gutter_r, _) = classic_session
        .scrollbar_gutters()
        .await
        .expect("classic scrollbar_gutters");
    assert_eq!(classic_gutter_r, 0.0);
    let classic_cp = mimikyu_core::input::ChokepointInput::new(classic_session.clone());
    classic_cp
        .click(
            &mimikyu_core::input::InputTarget::Selector("#b-gutter".into()),
            None,
            &mimikyu_core::input::ClickOptions::default(),
        )
        .await
        .expect("classic layout scrollbar already shrinks clientWidth; right-aligned button must click cleanly");
    assert_eq!(classic_session.title().await.unwrap(), "gutter-clicked");

    classic_session.dispose().await.expect("dispose classic");
    support::assert_pgid_gone(classic_pid);
}

async fn assert_new_context_rollback_on_invalid_timezone(engine: EngineKind) {
    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;

    let mut bad_prof = mimikyu_core::env_profile::PresetKind::Win11.to_env_profile(engine);
    bad_prof.timezone = "Invalid/DoesNotExist".to_string();

    let err = pool
        .create_labeled_context(
            "bad-tz",
            ContextOptions {
                env_profile: Some(bad_prof),
                ..Default::default()
            },
        )
        .await
        .err()
        .expect("invalid timezone must fail new_context");
    assert!(!err.to_string().is_empty());

    // After failed context creation, no leaked extra tab/window or active context should remain
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.root_session().list_tabs().await.unwrap().len(), 1);

    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

#[tokio::test]
async fn live_context_creation_rollback_on_invalid_timezone_chromium() {
    assert_new_context_rollback_on_invalid_timezone(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_context_creation_rollback_on_invalid_timezone_firefox() {
    assert_new_context_rollback_on_invalid_timezone(EngineKind::Firefox).await;
}

#[tokio::test]
async fn live_firefox_virtual_context_download_after_aborted_and_completed_fetch() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl1 = tmp.path().join("v1-fetch-poison-dl");
    let expected_payload: Vec<u8> = (0..4096).map(|i| ((i * 9 + 5) % 251) as u8).collect();

    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;
    let pool_profile_dl = pool
        .root_session()
        .info()
        .profile_dir
        .clone()
        .unwrap()
        .join("downloads");

    let vc1 = pool
        .create_labeled_context(
            "fetch-poison-v1",
            ContextOptions {
                download_dir: Some(dl1.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc1");

    vc1.session()
        .navigate(&server.url("/download-after-fetch-page"))
        .await
        .expect("nav /download-after-fetch-page");

    // 1. Aborted fetch('/download/asset-poison.bin') before <a download="ok-after-abort.bin">
    vc1.session()
        .eval_isolated(
            r#"(async () => {
                const ac = new AbortController();
                setTimeout(() => ac.abort(), 30);
                try { await fetch('/download/asset-poison.bin', { signal: ac.signal }); } catch {}
                return true;
            })()"#,
        )
        .await
        .expect("aborted fetch");
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let b_abort = vc1
        .session()
        .resolve_selector_box("#dl-after-abort")
        .await
        .unwrap();
    let (ax, ay) = b_abort.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: ax,
            y: ay,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-after-abort");

    let got_after_abort =
        wait_for_download_file(&dl1.join("ok-after-abort.bin"), expected_payload.len()).await;
    assert_eq!(got_after_abort, expected_payload);

    // 2. Completed fetch() + completed XHR to same URL before <a download="ok-after-fetch.bin">
    vc1.session()
        .eval_isolated(
            r#"(async () => {
                const r = await fetch('/download/asset-poison.bin');
                await r.arrayBuffer();
                await new Promise((resolve, reject) => {
                    const x = new XMLHttpRequest();
                    x.open('GET', '/download/asset-poison.bin');
                    x.responseType = 'arraybuffer';
                    x.onload = () => resolve(true);
                    x.onerror = () => reject(new Error('xhr error'));
                    x.send();
                });
                return true;
            })()"#,
        )
        .await
        .expect("completed fetch + xhr");

    let b_ok = vc1
        .session()
        .resolve_selector_box("#dl-after-ok")
        .await
        .unwrap();
    let (ox, oy) = b_ok.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: ox,
            y: oy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-after-ok");

    let got_after_ok =
        wait_for_download_file(&dl1.join("ok-after-fetch.bin"), expected_payload.len()).await;
    assert_eq!(got_after_ok, expected_payload);

    // 3. Completed fetch() to 0-byte URL before <a download="zero-after-fetch.bin">
    vc1.session()
        .eval_isolated(
            "fetch('/download/zero-poison.bin').then(r => r.arrayBuffer()).then(() => true)",
        )
        .await
        .expect("completed zero-byte fetch");
    let b_zero = vc1
        .session()
        .resolve_selector_box("#dl-zero-after-ok")
        .await
        .unwrap();
    let (zx, zy) = b_zero.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: zx,
            y: zy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-zero-after-ok");

    let got_zero = wait_for_download_file(&dl1.join("zero-after-fetch.bin"), 0).await;
    assert!(got_zero.is_empty());

    assert_pool_download_dir_empty(&pool_profile_dl).await;

    vc1.dispose().await.expect("dispose vc1");
    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

async fn assert_unpaired_utf16_surrogates_no_hang(engine: EngineKind) {
    let driver = driver_for(engine);
    let session = driver
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid") as libc::pid_t;

    session
        .navigate("data:text/html,<div id='d'>ok</div>")
        .await
        .expect("nav");

    let res = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let lone_hi = session.eval_isolated("'\\uD800'").await?;
        let lone_lo = session.eval_isolated("'\\uDFFF'").await?;
        let pair = session.eval_isolated("'\\uD83D\\uDE00'").await?;
        let escaped = session.eval_isolated("'\\\\ud800'").await?;
        session
            .eval_isolated("document.title = 'hi\\uD800end'; document.getElementById('d').textContent = 'dom\\uDC00text'; true")
            .await?;
        let title = session.title().await?;
        let dom = session
            .eval_isolated("document.getElementById('d').textContent")
            .await?;
        Ok::<_, MimikyuError>((lone_hi, lone_lo, pair, escaped, title, dom))
    })
    .await
    .expect("lone surrogate eval/title must not hang 30s")
    .expect("lone surrogate evals must succeed");

    assert_eq!(res.0, "\u{FFFD}");
    assert_eq!(res.1, "\u{FFFD}");
    assert_eq!(res.2, "😀");
    assert_eq!(res.3, "\\ud800");
    assert_eq!(res.4, "hi\u{FFFD}end");
    assert_eq!(res.5, "dom\u{FFFD}text");

    session.dispose().await.expect("dispose");
    support::assert_pgid_gone(pid);
}

#[tokio::test]
async fn live_unpaired_utf16_surrogates_do_not_hang_chromium() {
    assert_unpaired_utf16_surrogates_no_hang(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_unpaired_utf16_surrogates_do_not_hang_firefox() {
    assert_unpaired_utf16_surrogates_no_hang(EngineKind::Firefox).await;
}

async fn assert_iframe_selector_actions_parity(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            extra_args: if engine == EngineKind::Chromium {
                vec!["--site-per-process".to_string()]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid") as libc::pid_t;

    let xo_url = server2.localhost_url("/iframe-selector-sub");
    session
        .navigate(&server.url(&format!("/iframe-selector-host?xo={xo_url}")))
        .await
        .expect("navigate /iframe-selector-host");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let frames = session.list_frames().await.expect("list_frames");
    let so_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.starts_with(&server.url("/iframe-selector-sub")))
        .map(|f| f.frame_id.clone())
        .expect("same-origin child frame");
    let xo_fid = frames
        .iter()
        .find(|f| !f.is_main && f.url.starts_with(&xo_url))
        .map(|f| f.frame_id.clone())
        .expect("cross-origin child frame");

    for (label, fid) in [("same-origin", &so_fid), ("cross-origin", &xo_fid)] {
        let selected = session
            .select_option(
                None,
                Some(fid),
                Some("#sub-sel"),
                mimikyu_core::driver::SelectBy::Value("b".to_string()),
            )
            .await
            .unwrap_or_else(|e| panic!("{engine} ({label}) select_option failed: {e}"));
        assert_eq!(selected, "b");
        assert_eq!(
            session
                .eval_isolated_in_frame("document.getElementById('sub-sel').value", Some(fid))
                .await
                .unwrap(),
            "b"
        );

        session
            .clear_node_value(None, Some(fid), Some("#sub-inp"))
            .await
            .unwrap_or_else(|e| panic!("{engine} ({label}) clear_node_value failed: {e}"));
        assert_eq!(
            session
                .eval_isolated_in_frame("document.getElementById('sub-inp').value", Some(fid))
                .await
                .unwrap(),
            ""
        );

        session
            .scroll_into_view(None, Some(fid), Some("#sub-bottom"))
            .await
            .unwrap_or_else(|e| panic!("{engine} ({label}) scroll_into_view failed: {e}"));
        let sy = session
            .eval_isolated_in_frame("window.scrollY", Some(fid))
            .await
            .unwrap()
            .as_f64()
            .unwrap_or(0.0);
        assert!(
            sy > 0.0,
            "{engine} ({label}) scroll_into_view must scroll iframe (scrollY={sy})"
        );
    }

    session.dispose().await.expect("dispose");
    support::assert_pgid_gone(pid);
}

#[tokio::test]
async fn live_iframe_selector_actions_parity_chromium() {
    assert_iframe_selector_actions_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_iframe_selector_actions_parity_firefox() {
    assert_iframe_selector_actions_parity(EngineKind::Firefox).await;
}

#[tokio::test]
async fn live_firefox_virtual_context_content_disposition_does_not_leak_to_subsequent_download() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dl1 = tmp.path().join("vc-cd-reset");

    let pool = mimikyu_core::isolation::VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid") as libc::pid_t;

    let vc1 = pool
        .create_labeled_context(
            "cd-reset-vc",
            ContextOptions {
                download_dir: Some(dl1.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("vc1");

    vc1.session()
        .navigate(&server.url("/download-cd-reset-page"))
        .await
        .expect("nav /download-cd-reset-page");

    // 1. Response 1 serves Content-Disposition: attachment; filename="first.bin"
    let b1 = vc1
        .session()
        .resolve_selector_box("#dl-cd")
        .await
        .expect("box #dl-cd");
    let (x1, y1) = b1.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x1,
            y: y1,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-cd");
    let got1 = wait_for_download_file(&dl1.join("first.bin"), 512).await;
    assert_eq!(got1, vec![1u8; 512]);

    // 2. Response 2 from same URL omits Content-Disposition; <a download="client-named.bin"> must win
    let b2 = vc1
        .session()
        .resolve_selector_box("#dl-attr")
        .await
        .expect("box #dl-attr");
    let (x2, y2) = b2.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x2,
            y: y2,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-attr");
    let got2 = wait_for_download_file(&dl1.join("client-named.bin"), 512).await;
    assert_eq!(got2, vec![2u8; 512]);

    // 3. Response 3 from same URL serves Content-Disposition: attachment; filename="third.bin"
    let b3 = vc1
        .session()
        .resolve_selector_box("#dl-cd-3")
        .await
        .expect("box #dl-cd-3");
    let (x3, y3) = b3.center();
    vc1.session()
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Click,
            x: x3,
            y: y3,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .expect("click #dl-cd-3");
    let got3 = wait_for_download_file(&dl1.join("third.bin"), 512).await;
    assert_eq!(got3, vec![3u8; 512]);

    let mut entries: Vec<String> = std::fs::read_dir(&dl1)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["client-named.bin", "first.bin", "third.bin"],
        "unexpected downloaded files in vc1: {entries:?}"
    );

    vc1.dispose().await.expect("dispose vc1");
    pool.shutdown().await.expect("shutdown pool");
    support::assert_pgid_gone(pool_pid);
}

async fn assert_activate_tab_across_target_blank_popups(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid") as libc::pid_t;
    let root_tid = session.info().target_id.clone();

    session
        .navigate(&server.url("/target-blank-host"))
        .await
        .expect("navigate /target-blank-host");

    for sel in ["#lnk-blank", "#lnk-noopener"] {
        let b = session
            .resolve_selector_box(sel)
            .await
            .unwrap_or_else(|e| panic!("{engine} resolve {sel}: {e}"));
        let (x, y) = b.center();
        session
            .dispatch_mouse(MouseEvent {
                event_type: MouseEventType::Click,
                x,
                y,
                button: MouseButton::Left,
                click_count: 1,
                modifiers: 0,
            })
            .await
            .unwrap_or_else(|e| panic!("{engine} click {sel}: {e}"));
    }

    let mut blank_tid = None;
    let mut noopener_tid = None;
    for _ in 0..120 {
        let tabs = session.list_tabs().await.expect("list_tabs");
        blank_tid = tabs
            .iter()
            .find(|t| t.url.contains("/target-blank-child?tag=blank"))
            .map(|t| t.target_id.clone());
        noopener_tid = tabs
            .iter()
            .find(|t| t.url.contains("/target-blank-child?tag=noopener"))
            .map(|t| t.target_id.clone());
        if blank_tid.is_some() && noopener_tid.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let blank_tid = blank_tid.expect("blank popup tab present");
    let noopener_tid = noopener_tid.expect("noopener popup tab present");

    for target_tid in [&blank_tid, &noopener_tid, &root_tid, &blank_tid] {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.activate_tab(target_tid),
        )
        .await
        .unwrap_or_else(|_| panic!("{engine}: activate_tab({target_tid}) timed out"))
        .unwrap_or_else(|e| panic!("{engine}: activate_tab({target_tid}) failed: {e}"));

        let tabs = session.list_tabs().await.expect("list_tabs after activate");
        let active_ids: Vec<&str> = tabs
            .iter()
            .filter(|t| t.active)
            .map(|t| t.target_id.as_str())
            .collect();
        assert_eq!(
            active_ids,
            vec![target_tid.as_str()],
            "{engine}: expected only {target_tid} active, got {tabs:?}"
        );
    }

    session.dispose().await.expect("dispose");
    support::assert_pgid_gone(pid);
}

#[tokio::test]
async fn live_activate_tab_across_target_blank_popups_chromium() {
    assert_activate_tab_across_target_blank_popups(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_activate_tab_across_target_blank_popups_firefox() {
    assert_activate_tab_across_target_blank_popups(EngineKind::Firefox).await;
}

async fn assert_instant_animations_popups_child_frames_and_oopif_toggle(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let xo_url = server2.localhost_url("/anim-xo-child");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            extra_args: if engine == EngineKind::Chromium {
                vec!["--site-per-process".to_string()]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid") as libc::pid_t;

    session
        .navigate(&server.url(&format!("/anim-host?xo={xo_url}")))
        .await
        .expect("navigate /anim-host");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Open popup containing a child iframe
    session
        .eval_isolated("document.getElementById('open-pop').click()")
        .await
        .expect("click #open-pop");

    for _ in 0..120 {
        let loads = session
            .eval_main_world("window.__popLoads")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if loads >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Allow popup/OOPIF target setup to settle
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let read_main_and_oopif = || async {
        let main_style = session
            .eval_isolated(
                "({ animDur: getComputedStyle(document.getElementById('box')).animationDuration, transDur: getComputedStyle(document.getElementById('box')).transitionDuration, scrollBeh: getComputedStyle(document.documentElement).scrollBehavior })",
            )
            .await
            .expect("read main anim");
        let frames = session.list_frames().await.expect("list_frames");
        let xo_fid = frames
            .iter()
            .find(|f| !f.is_main && f.url.contains("/anim-xo-child"))
            .map(|f| f.frame_id.clone())
            .expect("xo child frame present");
        let oopif_style = session
            .eval_isolated_in_frame(
                "({ animDur: getComputedStyle(document.getElementById('box')).animationDuration, transDur: getComputedStyle(document.getElementById('box')).transitionDuration, scrollBeh: getComputedStyle(document.documentElement).scrollBehavior })",
                Some(&xo_fid),
            )
            .await
            .expect("read oopif anim");
        (main_style, oopif_style)
    };

    let ask_popup = |tag: &str| {
        let session = &session;
        let tag_owned = tag.to_string();
        async move {
            session
                .eval_main_world(&format!(
                    "window.__askPop({})",
                    serde_json::to_string(&tag_owned).unwrap()
                ))
                .await
                .expect("askPop");
            for _ in 0..100 {
                let val = session
                    .eval_main_world(&format!(
                        "window.__animReplies[{}] || null",
                        serde_json::to_string(&tag_owned).unwrap()
                    ))
                    .await
                    .unwrap_or(serde_json::Value::Null);
                if !val.is_null() {
                    return val;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            panic!("{engine}: timed out waiting for popup probe reply '{tag_owned}'");
        }
    };

    let assert_durations = |label: &str, expected_anim: &str, expected_trans: &str, expected_scroll: &str, main_v: &serde_json::Value, oopif_v: &serde_json::Value, pop_reply: &serde_json::Value| {
        for (scope, v) in [
            ("main", main_v),
            ("oopif", oopif_v),
            ("popup", &pop_reply["pop"]),
            ("popup_child", &pop_reply["child"]),
        ] {
            assert_eq!(
                v["animDur"].as_str(),
                Some(expected_anim),
                "{engine} [{label}] {scope} animDur mismatch: {v:?}"
            );
            assert_eq!(
                v["transDur"].as_str(),
                Some(expected_trans),
                "{engine} [{label}] {scope} transDur mismatch: {v:?}"
            );
            assert_eq!(
                v["scrollBeh"].as_str(),
                Some(expected_scroll),
                "{engine} [{label}] {scope} scrollBeh mismatch: {v:?}"
            );
        }
    };

    // 1. Baseline (disabled)
    let (m0, o0) = read_main_and_oopif().await;
    let p0 = ask_popup("baseline").await;
    assert_durations("baseline", "10s", "5s", "smooth", &m0, &o0, &p0);

    // 2. Enable instant animations while main, OOPIF, popup, and popup child are already open
    session
        .set_instant_animations(true)
        .await
        .expect("enable instant animations");
    let (m1, o1) = read_main_and_oopif().await;
    let p1 = ask_popup("after_enable_live").await;
    assert_durations("after_enable_live", "0s", "0s", "auto", &m1, &o1, &p1);

    // 3. Reload main+OOPIF and reload popup+child while enabled
    let prev_loads = session
        .eval_main_world("window.__popLoads")
        .await
        .unwrap()
        .as_u64()
        .unwrap_or(1);
    session
        .eval_main_world("window.__reloadPop()")
        .await
        .expect("reload popup");
    for _ in 0..120 {
        let loads = session
            .eval_main_world("window.__popLoads")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if loads > prev_loads {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    session.reload().await.expect("reload main");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (m2, o2) = read_main_and_oopif().await;
    let p2 = ask_popup("after_enable_reload").await;
    assert_durations("after_enable_reload", "0s", "0s", "auto", &m2, &o2, &p2);

    // 3b. Open a brand-new popup WHILE instant_animations is already enabled (popup document + child iframe must both be 0s)
    let prev_loads_new_pop = session
        .eval_main_world("window.__popLoads")
        .await
        .unwrap()
        .as_u64()
        .unwrap_or(2);
    session
        .eval_main_world(
            "if (window.__pop && !window.__pop.closed) window.__pop.close(); window.__pop = window.open('/anim-popup', 'animpop2', 'width=400,height=300'); true",
        )
        .await
        .expect("open fresh popup while instant_animations=true");
    for _ in 0..120 {
        let loads = session
            .eval_main_world("window.__popLoads")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if loads > prev_loads_new_pop {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (m2b, o2b) = read_main_and_oopif().await;
    let p2b = ask_popup("new_popup_while_enabled").await;
    assert_durations("new_popup_while_enabled", "0s", "0s", "auto", &m2b, &o2b, &p2b);

    let prev_loads_new_pop_reload = session
        .eval_main_world("window.__popLoads")
        .await
        .unwrap()
        .as_u64()
        .unwrap_or(3);
    session
        .eval_main_world("window.__reloadPop()")
        .await
        .expect("reload newly opened popup while instant_animations=true");
    for _ in 0..120 {
        let loads = session
            .eval_main_world("window.__popLoads")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if loads > prev_loads_new_pop_reload {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let p2c = ask_popup("new_popup_while_enabled_reload").await;
    assert_durations("new_popup_while_enabled_reload", "0s", "0s", "auto", &m2b, &o2b, &p2c);

    // 4. Disable instant animations (live check across main, OOPIF, popup, popup child)
    session
        .set_instant_animations(false)
        .await
        .expect("disable instant animations");
    let (m3, o3) = read_main_and_oopif().await;
    let p3 = ask_popup("after_disable_live").await;
    assert_durations("after_disable_live", "10s", "5s", "smooth", &m3, &o3, &p3);

    // 5. Reload main+OOPIF and reload popup+child while disabled (preload scripts must be removed)
    let prev_loads2 = session
        .eval_main_world("window.__popLoads")
        .await
        .unwrap()
        .as_u64()
        .unwrap_or(2);
    session
        .eval_main_world("window.__reloadPop()")
        .await
        .expect("reload popup after disable");
    for _ in 0..120 {
        let loads = session
            .eval_main_world("window.__popLoads")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if loads > prev_loads2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    session.reload().await.expect("reload main after disable");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (m4, o4) = read_main_and_oopif().await;
    let p4 = ask_popup("after_disable_reload").await;
    assert_durations("after_disable_reload", "10s", "5s", "smooth", &m4, &o4, &p4);

    // 6. Open another fresh popup AFTER disable to ensure no stale popup/userContext preload script remains
    let prev_loads3 = session
        .eval_main_world("window.__popLoads")
        .await
        .unwrap()
        .as_u64()
        .unwrap_or(4);
    session
        .eval_main_world(
            "if (window.__pop && !window.__pop.closed) window.__pop.close(); window.__pop = window.open('/anim-popup', 'animpop3', 'width=400,height=300'); true",
        )
        .await
        .expect("open fresh popup after disable");
    for _ in 0..120 {
        let loads = session
            .eval_main_world("window.__popLoads")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if loads > prev_loads3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let p5 = ask_popup("new_popup_after_disable").await;
    assert_durations("new_popup_after_disable", "10s", "5s", "smooth", &m4, &o4, &p5);

    session.dispose().await.expect("dispose");
    support::assert_pgid_gone(pid);
}

#[tokio::test]
async fn live_instant_animations_popups_child_frames_and_oopif_toggle_chromium() {
    assert_instant_animations_popups_child_frames_and_oopif_toggle(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_instant_animations_popups_child_frames_and_oopif_toggle_firefox() {
    assert_instant_animations_popups_child_frames_and_oopif_toggle(EngineKind::Firefox).await;
}

async fn assert_instant_animations_virtual_context_popup_isolation(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let xo_url = server2.localhost_url("/anim-xo-child");

    let root = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::VirtualContext,
            main_world_eval: true,
            extra_args: if engine == EngineKind::Chromium {
                vec!["--site-per-process".to_string()]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
        .await
        .expect("launch virtual pool");
    let pid = root.info().pid.expect("pid") as libc::pid_t;

    let ctx_a = root
        .new_context(ContextOptions {
            main_world_eval: true,
            instant_animations: true,
            ..Default::default()
        })
        .await
        .expect("new_context A");
    let ctx_b = root
        .new_context(ContextOptions {
            main_world_eval: true,
            instant_animations: false,
            ..Default::default()
        })
        .await
        .expect("new_context B");

    for (label, s) in [("A", &ctx_a), ("B", &ctx_b)] {
        s.navigate(&server.url(&format!("/anim-host?xo={xo_url}")))
            .await
            .unwrap_or_else(|e| panic!("{engine} ctx_{label} navigate: {e}"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        s.eval_isolated("document.getElementById('open-pop').click()")
            .await
            .unwrap_or_else(|e| panic!("{engine} ctx_{label} open-pop: {e}"));
        for _ in 0..120 {
            let loads = s
                .eval_main_world("window.__popLoads")
                .await
                .ok()
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if loads >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    let read_ctx = |s: &std::sync::Arc<dyn mimikyu_core::driver::DriverSession>, tag: &str| {
        let s = std::sync::Arc::clone(s);
        let tag = tag.to_string();
        async move {
            let main_style = s
                .eval_isolated(
                    "({ animDur: getComputedStyle(document.getElementById('box')).animationDuration, transDur: getComputedStyle(document.getElementById('box')).transitionDuration, scrollBeh: getComputedStyle(document.documentElement).scrollBehavior })",
                )
                .await
                .expect("read main anim");
            s.eval_main_world(&format!(
                "window.__askPop({})",
                serde_json::to_string(&tag).unwrap()
            ))
            .await
            .expect("askPop");
            let mut pop_reply = serde_json::Value::Null;
            for _ in 0..100 {
                let val = s
                    .eval_main_world(&format!(
                        "window.__animReplies[{}] || null",
                        serde_json::to_string(&tag).unwrap()
                    ))
                    .await
                    .unwrap_or(serde_json::Value::Null);
                if !val.is_null() {
                    pop_reply = val;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            assert!(!pop_reply.is_null(), "{engine}: timed out waiting for ctx popup reply");
            (main_style, pop_reply)
        }
    };

    let (ma, pa) = read_ctx(&ctx_a, "vc_a").await;
    let (mb, pb) = read_ctx(&ctx_b, "vc_b").await;
    for (scope, v) in [
        ("ctx_a.main", &ma),
        ("ctx_a.pop", &pa["pop"]),
        ("ctx_a.child", &pa["child"]),
    ] {
        assert_eq!(v["animDur"].as_str(), Some("0s"), "{engine} {scope}: {v:?}");
        assert_eq!(v["transDur"].as_str(), Some("0s"), "{engine} {scope}: {v:?}");
        assert_eq!(v["scrollBeh"].as_str(), Some("auto"), "{engine} {scope}: {v:?}");
    }
    for (scope, v) in [
        ("ctx_b.main", &mb),
        ("ctx_b.pop", &pb["pop"]),
        ("ctx_b.child", &pb["child"]),
    ] {
        assert_eq!(v["animDur"].as_str(), Some("10s"), "{engine} {scope}: {v:?}");
        assert_eq!(v["transDur"].as_str(), Some("5s"), "{engine} {scope}: {v:?}");
        assert_eq!(v["scrollBeh"].as_str(), Some("smooth"), "{engine} {scope}: {v:?}");
    }

    // Toggle ctx_a off and verify ctx_a reverts + reload stays disabled while ctx_b remains unaffected
    ctx_a
        .set_instant_animations(false)
        .await
        .expect("ctx_a disable");
    let (ma_off, pa_off) = read_ctx(&ctx_a, "vc_a_off").await;
    for (scope, v) in [
        ("ctx_a_off.main", &ma_off),
        ("ctx_a_off.pop", &pa_off["pop"]),
        ("ctx_a_off.child", &pa_off["child"]),
    ] {
        assert_eq!(v["animDur"].as_str(), Some("10s"), "{engine} {scope}: {v:?}");
    }

    ctx_a.dispose().await.expect("dispose ctx_a");
    ctx_b.dispose().await.expect("dispose ctx_b");
    root.dispose().await.expect("dispose root");
    support::assert_pgid_gone(pid);
}

#[tokio::test]
async fn live_instant_animations_virtual_context_popup_isolation_chromium() {
    assert_instant_animations_virtual_context_popup_isolation(EngineKind::Chromium).await;
}

#[tokio::test]
async fn live_instant_animations_virtual_context_popup_isolation_firefox() {
    assert_instant_animations_virtual_context_popup_isolation(EngineKind::Firefox).await;
}
