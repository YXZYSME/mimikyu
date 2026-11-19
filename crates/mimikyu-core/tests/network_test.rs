mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use mimikyu_core::driver::{
    EngineKind, LaunchOptions, ProfileMode, RouteAction, RouteMock, driver_for,
};
use mimikyu_core::network::{BlockPolicy, ResourceType, RouteRule};
use support::{TestServer, assert_pgid_gone};

async fn assert_resource_blocker_filters_categories_and_iframes(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            block_images: true,
            block_media: true,
            block_fonts: true,
            ..Default::default()
        })
        .await
        .expect("launch with category blockers");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/net/page"))
        .await
        .expect("navigate /net/page");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let sub_text = session
        .eval_isolated(
            "document.getElementById('net-subframe').contentDocument.getElementById('sub-ok').textContent",
        )
        .await
        .expect("read subframe");
    assert_eq!(sub_text, "sub-loaded");

    let hits = server.hits();
    assert!(hits.iter().any(|h| h.contains("/net/page")));
    assert!(hits.iter().any(|h| h.contains("/net/subframe")));
    assert!(
        !hits.iter().any(|h| h.contains("/assets/")),
        "{engine}: all image/media/font assets (including extensionless and iframe) must be blocked; hits={hits:?}"
    );

    // Verify set_route_mock + clear_route_mocks preserves category blocking
    session
        .set_route_mock(RouteMock {
            id: "tmp-mock".to_string(),
            url_pattern: "*/api/data*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: BTreeMap::new(),
                body: br#"{"data":"mocked"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");
    let mocked = session
        .eval_isolated("fetch('/api/data').then(r => r.json())")
        .await
        .expect("fetch mocked");
    assert_eq!(mocked["data"], "mocked");

    session
        .clear_route_mocks()
        .await
        .expect("clear_route_mocks");
    server.reset_hits();

    session
        .navigate(&server.url("/net/page"))
        .await
        .expect("re-navigate /net/page after clear_route_mocks");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let hits_after = server.hits();
    assert!(
        !hits_after.iter().any(|h| h.contains("/assets/")),
        "{engine}: category blocking must remain active after clear_route_mocks; hits={hits_after:?}"
    );

    let live_data = session
        .eval_isolated("fetch('/api/data').then(r => r.json())")
        .await
        .expect("fetch live after clear");
    assert_eq!(live_data["data"], "live");

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn resource_blocker_filters_images_media_fonts_including_extensionless_and_iframes_chromium()
{
    assert_resource_blocker_filters_categories_and_iframes(EngineKind::Chromium).await;
}

#[tokio::test]
async fn resource_blocker_filters_images_media_fonts_including_extensionless_and_iframes_firefox() {
    assert_resource_blocker_filters_categories_and_iframes(EngineKind::Firefox).await;
}

async fn assert_route_rules_and_patterns(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid");

    session
        .set_block_policy(BlockPolicy {
            block_images: false,
            block_media: false,
            block_fonts: false,
            blocked_urls: vec![format!("exact:{}", server.url("/api/abort-me"))],
        })
        .await
        .expect("set_block_policy");

    let mut fulfill_hdrs = BTreeMap::new();
    fulfill_hdrs.insert("content-type".to_string(), "application/json".to_string());
    fulfill_hdrs.insert("x-mocked-by".to_string(), "mimikyu".to_string());
    session
        .set_route_rule(RouteRule {
            id: "regex-sub".to_string(),
            url_pattern: r"regex:/api/sub-mock(\?.*)?$".to_string(),
            resource_types: vec![ResourceType::Fetch, ResourceType::Xhr],
            action: RouteAction::Fulfill {
                status: 201,
                headers: fulfill_hdrs,
                body: br#"{"sub":"mocked"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_rule fulfill");

    let mut mod_hdrs = BTreeMap::new();
    mod_hdrs.insert("X-Injected-Route".to_string(), "yes-42".to_string());
    session
        .set_route_rule(RouteRule {
            id: "mod-hdr".to_string(),
            url_pattern: "prefix:http://".to_string(),
            resource_types: vec![ResourceType::Fetch],
            action: RouteAction::ContinueWithHeaders { headers: mod_hdrs },
        })
        .await
        .expect("set_route_rule modify headers");

    session
        .navigate(&server.url("/net/page"))
        .await
        .expect("navigate /net/page");

    let res = session
        .eval_isolated(
            r#"(async () => {
                let exactBlocked = false;
                try {
                    const r = await fetch('/api/abort-me');
                    exactBlocked = !r.ok;
                } catch {
                    exactBlocked = true;
                }
                const subResp = await fetch('/api/sub-mock?x=1');
                const subBody = await subResp.json();
                const modResp = await fetch('/api/modify-headers', {
                    headers: { 'X-Client-Header': 'kept' }
                });
                const modEcho = await modResp.json();
                return {
                    exactBlocked,
                    subStatus: subResp.status,
                    subHdr: subResp.headers.get('x-mocked-by'),
                    subBody,
                    modEcho
                };
            })()"#,
        )
        .await
        .expect("route eval");

    assert_eq!(res["exactBlocked"], true);
    assert_eq!(res["subStatus"], 201);
    assert_eq!(res["subHdr"], "mimikyu");
    assert_eq!(res["subBody"]["sub"], "mocked");
    assert_eq!(
        res["modEcho"]["x-injected-route"], "yes-42",
        "{engine}: injected header missing: {:?}",
        res["modEcho"]
    );
    assert_eq!(
        res["modEcho"]["x-client-header"], "kept",
        "{engine}: original request header must be preserved: {:?}",
        res["modEcho"]
    );

    let hits = server.hits();
    assert!(!hits.iter().any(|h| h.contains("/api/abort-me")));
    assert!(!hits.iter().any(|h| h.contains("/api/sub-mock")));

    assert_eq!(
        session.inflight_requests().await.expect("inflight"),
        0,
        "{engine}: inflight_requests must be 0 after mocked/blocked fetches"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn route_mock_fulfill_modify_headers_abort_and_patterns_chromium() {
    assert_route_rules_and_patterns(EngineKind::Chromium).await;
}

#[tokio::test]
async fn route_mock_fulfill_modify_headers_abort_and_patterns_firefox() {
    assert_route_rules_and_patterns(EngineKind::Firefox).await;
}

async fn assert_telemetry_ring_buffers_and_eviction(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            telemetry_capacity: Some(1000),
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/net/console-and-errors"))
        .await
        .expect("navigate /net/console-and-errors");
    tokio::time::sleep(Duration::from_millis(120)).await;

    let logs = session.console_logs().await.expect("console_logs");
    let errs = session.page_errors().await.expect("page_errors");
    let nets = session.network_events().await.expect("network_events");

    assert!(
        logs.iter()
            .any(|c| c.text.contains("net-log-1") && c.level == "log")
    );
    assert!(
        logs.iter()
            .any(|c| c.text.contains("net-info-2") && c.level == "info")
    );
    assert!(
        logs.iter()
            .any(|c| c.text.contains("net-warn-3") && (c.level == "warning" || c.level == "warn"))
    );
    assert!(
        logs.iter()
            .any(|c| c.text.contains("net-err-4") && c.level == "error")
    );
    assert!(
        !logs.iter().any(|c| c.text.contains("uncaught-boom-42")),
        "{engine}: uncaught page error must not be mixed into console_logs"
    );
    assert!(
        errs.iter().any(|e| e.message.contains("uncaught-boom-42")),
        "{engine}: uncaught page error missing from page_errors: {errs:?}"
    );
    assert!(
        nets.iter()
            .any(|n| n.url.contains("/net/console-and-errors") && n.status == Some(200)),
        "{engine}: network_events missing 200 entry"
    );
    assert_eq!(session.inflight_requests().await.unwrap(), 0);

    // Verify drain_* empties buffers
    let drained_c = session.drain_console_logs().await.expect("drain console");
    assert!(!drained_c.is_empty());
    assert!(session.console_logs().await.unwrap().is_empty());

    let drained_e = session
        .drain_page_errors()
        .await
        .expect("drain page_errors");
    assert!(!drained_e.is_empty());
    assert!(session.page_errors().await.unwrap().is_empty());

    let drained_n = session.drain_network_events().await.expect("drain network");
    assert!(!drained_n.is_empty());

    session.clear_telemetry().await.expect("clear_telemetry");

    // Flood 1200 console logs against capacity 1000
    session
        .navigate(&server.url("/net/console-flood?n=1200"))
        .await
        .expect("navigate /net/console-flood");
    for _ in 0..40 {
        if session
            .eval_isolated("Boolean(window.__floodDone)")
            .await
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let flood_logs = session.console_logs().await.expect("flood logs");
    assert_eq!(
        flood_logs.len(),
        1000,
        "{engine}: ring buffer must cap at 1000 entries"
    );
    assert!(
        !flood_logs.iter().any(|l| l.text == "flood-0"),
        "{engine}: oldest flood-0 must be evicted"
    );
    assert_eq!(
        flood_logs.last().map(|l| l.text.as_str()),
        Some("flood-1199"),
        "{engine}: newest flood-1199 must be retained"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn telemetry_ring_buffers_console_page_errors_network_and_eviction_chromium() {
    assert_telemetry_ring_buffers_and_eviction(EngineKind::Chromium).await;
}

#[tokio::test]
async fn telemetry_ring_buffers_console_page_errors_network_and_eviction_firefox() {
    assert_telemetry_ring_buffers_and_eviction(EngineKind::Firefox).await;
}

async fn assert_oopif_network_interception_and_block_policy(engine: EngineKind) {
    let host_server = TestServer::start().await;
    let sub_server = TestServer::start().await;

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            block_images: true,
            extra_args: if engine == EngineKind::Chromium {
                vec!["--site-per-process".to_string()]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid");

    // 1. Pre-attachment policy & route mock before OOPIF attaches
    session
        .set_blocked_urls(vec!["*blocked.png*".to_string()])
        .await
        .expect("set_blocked_urls");
    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    session
        .set_route_mock(RouteMock {
            id: "oopif-mock-1".to_string(),
            url_pattern: "*/api/mock*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs.clone(),
                body: br#"{"mocked":true,"phase":"pre"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    let xo_url = sub_server.localhost_url("/oopif-net-sub");
    session
        .navigate(&host_server.url(&format!("/oopif-net-host?xo={xo_url}")))
        .await
        .expect("navigate /oopif-net-host");

    let mut xo_fid = None;
    for _ in 0..40 {
        let frames = session.list_frames().await.expect("list_frames");
        if let Some(f) = frames
            .iter()
            .find(|f| !f.is_main && f.url.contains("/oopif-net-sub"))
        {
            xo_fid = Some(f.frame_id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let xo_fid = xo_fid.expect("OOPIF frame_id");

    // Wait for OOPIF document to finish loading
    for _ in 0..40 {
        if let Ok(rs) = session
            .eval_isolated_in_frame("document.readyState", Some(&xo_fid))
            .await
            && rs == "complete"
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let pre_res = session
        .eval_isolated_in_frame(
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
                const img = document.getElementById('oopif-img');
                return {
                    blockedOk,
                    mocked,
                    real,
                    imgNaturalWidth: img ? img.naturalWidth : -1
                };
            })()"#,
            Some(&xo_fid),
        )
        .await
        .expect("eval pre in OOPIF");

    assert_eq!(
        pre_res["blockedOk"], false,
        "{engine}: /blocked.png in OOPIF must be blocked"
    );
    assert_eq!(
        pre_res["mocked"]["mocked"], true,
        "{engine}: /api/mock in OOPIF must be mocked"
    );
    assert_eq!(pre_res["mocked"]["phase"], "pre");
    assert_eq!(
        pre_res["real"]["real"], true,
        "{engine}: /api/real in OOPIF must pass through"
    );
    assert_eq!(
        pre_res["imgNaturalWidth"], 0,
        "{engine}: OOPIF <img> must be blocked by block_images: {pre_res:?}"
    );

    let sub_hits = sub_server.hits();
    assert!(
        !sub_hits.iter().any(|h| h.contains("/assets/sub-avatar")),
        "{engine}: OOPIF <img> hit sub_server despite block_images: {sub_hits:?}"
    );
    assert!(
        !sub_hits.iter().any(|h| h.contains("/blocked.png")),
        "{engine}: OOPIF /blocked.png hit sub_server: {sub_hits:?}"
    );
    assert!(
        !sub_hits.iter().any(|h| h.contains("/api/mock")),
        "{engine}: OOPIF /api/mock hit sub_server: {sub_hits:?}"
    );
    assert!(
        sub_hits.iter().any(|h| h.contains("/api/real")),
        "{engine}: OOPIF /api/real missing from sub_server: {sub_hits:?}"
    );

    // 2. Runtime updates WHILE OOPIF is already attached: set_block_policy, set_route_mock, clear_route_mocks
    sub_server.reset_hits();
    session
        .set_block_policy(BlockPolicy {
            block_images: false,
            block_media: false,
            block_fonts: false,
            blocked_urls: vec!["*/api/abort-me*".to_string()],
        })
        .await
        .expect("set_block_policy dynamically");
    session
        .set_route_mock(RouteMock {
            id: "oopif-mock-dyn".to_string(),
            url_pattern: "*/api/data*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs,
                body: br#"{"data":"dyn-oopif-mock"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock dynamically");

    let dyn_res = session
        .eval_isolated_in_frame(
            r#"(async () => {
                let abortBlocked = false;
                try {
                    const r = await fetch('/api/abort-me');
                    abortBlocked = !r.ok;
                } catch {
                    abortBlocked = true;
                }
                const dynMocked = await (await fetch('/api/data')).json();
                return { abortBlocked, dynMocked };
            })()"#,
            Some(&xo_fid),
        )
        .await
        .expect("eval dyn in OOPIF");
    assert_eq!(
        dyn_res["abortBlocked"], true,
        "{engine}: dynamic blocked_urls in OOPIF"
    );
    assert_eq!(
        dyn_res["dynMocked"]["data"], "dyn-oopif-mock",
        "{engine}: dynamic set_route_mock in OOPIF"
    );

    let sub_hits_dyn = sub_server.hits();
    assert!(!sub_hits_dyn.iter().any(|h| h.contains("/api/abort-me")));
    assert!(!sub_hits_dyn.iter().any(|h| h.contains("/api/data")));

    // Worker inside OOPIF must also honor blocked_urls and set_route_mock
    let worker_net_res = session
        .eval_isolated_in_frame(
            r#"new Promise((resolve, reject) => {
                const w = new Worker('/oopif-net-worker.js');
                w.onmessage = (ev) => resolve(ev.data);
                w.onerror = (err) => reject(new Error(String(err.message || err)));
                w.postMessage({ blockUrl: '/api/abort-me', mockUrl: '/api/data' });
            })"#,
            Some(&xo_fid),
        )
        .await
        .expect("worker in OOPIF");
    assert_eq!(
        worker_net_res["blockedOk"], false,
        "{engine}: worker inside OOPIF must honor blocked_urls: {worker_net_res:?}"
    );
    assert_eq!(
        worker_net_res["mocked"]["data"], "dyn-oopif-mock",
        "{engine}: worker inside OOPIF must honor set_route_mock: {worker_net_res:?}"
    );
    let sub_hits_wrk = sub_server.hits();
    assert!(
        !sub_hits_wrk.iter().any(|h| h.contains("/api/abort-me")),
        "{engine}: worker inside OOPIF /api/abort-me hit sub_server: {sub_hits_wrk:?}"
    );
    assert!(
        !sub_hits_wrk.iter().any(|h| h.contains("/api/data")),
        "{engine}: worker inside OOPIF /api/data hit sub_server: {sub_hits_wrk:?}"
    );

    // Clear route mocks while OOPIF is attached -> /api/data must reach sub_server live
    session
        .clear_route_mocks()
        .await
        .expect("clear_route_mocks");
    let after_clear = session
        .eval_isolated_in_frame("fetch('/api/data').then(r => r.json())", Some(&xo_fid))
        .await
        .expect("fetch /api/data after clear");
    assert_eq!(
        after_clear["data"], "live",
        "{engine}: OOPIF /api/data must reach server after clear_route_mocks"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn oopif_network_interception_and_block_policy_parity_chromium() {
    assert_oopif_network_interception_and_block_policy(EngineKind::Chromium).await;
}

#[tokio::test]
async fn oopif_network_interception_and_block_policy_parity_firefox() {
    assert_oopif_network_interception_and_block_policy(EngineKind::Firefox).await;
}

async fn assert_popup_network_interception_and_env_ephemeral(engine: EngineKind) {
    use mimikyu_core::env_profile::{EnvProfileBuilder, PresetKind};

    let server = TestServer::start().await;
    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Probe-Hdr", "eph-pop")
        .build()
        .expect("valid profile");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            env_profile: Some(profile.clone()),
            ..Default::default()
        })
        .await
        .expect("launch ephemeral session");
    let pid = session.info().pid.expect("pid");

    session
        .set_blocked_urls(vec!["*blocked-pop*".to_string()])
        .await
        .expect("set_blocked_urls");
    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    mock_hdrs.insert("access-control-allow-origin".to_string(), "*".to_string());
    session
        .set_route_mock(RouteMock {
            id: "eph-pop-mock".to_string(),
            url_pattern: "*mock-pop-pre*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs,
                body: br#"{"mocked":true,"phase":"eph"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    session
        .navigate(&server.url("/popup-probe-host"))
        .await
        .expect("navigate /popup-probe-host");

    session
        .eval_isolated("document.getElementById('open-popup').click()")
        .await
        .expect("click open-popup");

    let mut out = serde_json::Value::Null;
    for _ in 0..120 {
        let val = session
            .eval_isolated(
                "(() => { const t = document.getElementById('popup-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
            )
            .await
            .expect("poll popup-out");
        if !val.is_null() {
            out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!out.is_null(), "{engine}: timed out waiting for popup-out");
    assert_eq!(out["syncNav"]["platform"], profile.platform);
    assert_eq!(out["syncNav"]["hc"], profile.hardware_concurrency);
    assert_eq!(out["syncNav"]["ua"], profile.user_agent);
    assert_eq!(out["xHdr"], "eph-pop");
    assert_eq!(out["reqUa"], profile.user_agent);
    assert_eq!(out["mocked"]["phase"], "eph");
    assert_eq!(out["blockedOk"], false);

    let hits = server.hits();
    assert!(!hits.iter().any(|h| h.contains("/blocked-pop.js")));
    assert!(!hits.iter().any(|h| h.contains("/api/mock-pop-pre")));

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn popup_network_interception_and_env_ephemeral_parity_chromium() {
    assert_popup_network_interception_and_env_ephemeral(EngineKind::Chromium).await;
}

#[tokio::test]
async fn popup_network_interception_and_env_ephemeral_parity_firefox() {
    assert_popup_network_interception_and_env_ephemeral(EngineKind::Firefox).await;
}

async fn assert_target_blank_links_network_and_env_ephemeral(engine: EngineKind) {
    use mimikyu_core::driver::{MouseButton, MouseEvent, MouseEventType};
    use mimikyu_core::env_profile::{EnvProfileBuilder, PresetKind};

    let server = TestServer::start().await;
    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Probe-Hdr", "eph-tb")
        .build()
        .expect("valid profile");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            env_profile: Some(profile.clone()),
            ..Default::default()
        })
        .await
        .expect("launch ephemeral session");
    let pid = session.info().pid.expect("pid");

    session
        .set_blocked_urls(vec!["*blocked-pop*".to_string()])
        .await
        .expect("set_blocked_urls");
    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    mock_hdrs.insert("access-control-allow-origin".to_string(), "*".to_string());
    session
        .set_route_mock(RouteMock {
            id: "eph-tb-mock".to_string(),
            url_pattern: "*mock-pop-pre*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs,
                body: br#"{"mocked":true,"phase":"eph-tb"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    session
        .navigate(&server.url("/target-blank-host"))
        .await
        .expect("navigate /target-blank-host");

    let click_sel = |selector: &'static str| {
        let session = &session;
        async move {
            let box_rect = session
                .resolve_selector_box(selector)
                .await
                .expect("resolve_selector_box");
            let (cx, cy) = box_rect.center();
            session
                .dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Move,
                    x: cx,
                    y: cy,
                    button: MouseButton::None,
                    click_count: 0,
                    modifiers: 0,
                })
                .await
                .expect("mouse move");
            session
                .dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Down,
                    x: cx,
                    y: cy,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: 0,
                })
                .await
                .expect("mouse down");
            session
                .dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Up,
                    x: cx,
                    y: cy,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: 0,
                })
                .await
                .expect("mouse up");
        }
    };

    click_sel("#lnk-blank").await;
    let mut blank_out = serde_json::Value::Null;
    for _ in 0..300 {
        let val = session
            .eval_isolated(
                r#"(() => {
                    const a = document.getElementById('tb-blank')?.textContent || localStorage.getItem('tb_blank') || '';
                    return a ? JSON.parse(a) : null;
                })()"#,
            )
            .await
            .expect("poll tb-blank");
        if !val.is_null() {
            blank_out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !blank_out.is_null(),
        "{engine}: timed out waiting for target=_blank"
    );

    click_sel("#lnk-noopener").await;
    let mut noopener_out = serde_json::Value::Null;
    for _ in 0..300 {
        let val = session
            .eval_isolated(
                r#"(() => {
                    const b = document.getElementById('tb-noopener')?.textContent || localStorage.getItem('tb_noopener') || '';
                    return b ? JSON.parse(b) : null;
                })()"#,
            )
            .await
            .expect("poll tb-noopener");
        if !val.is_null() {
            noopener_out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !noopener_out.is_null(),
        "{engine}: timed out waiting for target=_blank rel=noopener"
    );
    let out = serde_json::json!({ "blank": blank_out, "noopener": noopener_out });

    let tabs = session.list_tabs().await.expect("list_tabs");
    assert!(
        tabs.iter()
            .any(|t| t.url.contains("/target-blank-child?tag=blank"))
    );
    assert!(
        tabs.iter()
            .any(|t| t.url.contains("/target-blank-child?tag=noopener"))
    );
    assert!(
        !tabs.iter().any(|t| t.url.is_empty()),
        "{engine}: empty url tab in {tabs:?}"
    );

    for (tag, node) in [("blank", &out["blank"]), ("noopener", &out["noopener"])] {
        assert_eq!(
            node["syncNav"]["platform"], profile.platform,
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(
            node["syncNav"]["hc"], profile.hardware_concurrency,
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(
            node["syncNav"]["ua"], profile.user_agent,
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(
            node["syncNav"]["tz"], "Asia/Tokyo",
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(node["xHdr"], "eph-tb", "{engine} {tag}: {node:?}");
        assert_eq!(
            node["reqUa"], profile.user_agent,
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(
            node["mocked"]["phase"], "eph-tb",
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(node["blockedOk"], false, "{engine} {tag}: {node:?}");
    }

    let req_hdrs = server.request_headers();
    for path_frag in [
        "GET /target-blank-child?tag=blank",
        "GET /target-blank-child?tag=noopener",
    ] {
        let (_, hdrs) = req_hdrs
            .iter()
            .find(|(req, _)| req == path_frag)
            .unwrap_or_else(|| panic!("{engine}: missing initial server request {path_frag}"));
        assert_eq!(hdrs.get("x-probe-hdr").map(String::as_str), Some("eph-tb"));
        assert_eq!(
            hdrs.get("user-agent").map(String::as_str),
            Some(profile.user_agent.as_str())
        );
    }

    let hits = server.hits();
    assert!(!hits.iter().any(|h| h.contains("/blocked-pop.js")));
    assert!(!hits.iter().any(|h| h.contains("/api/mock-pop-pre")));

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn target_blank_links_network_and_env_ephemeral_parity_chromium() {
    assert_target_blank_links_network_and_env_ephemeral(EngineKind::Chromium).await;
}

#[tokio::test]
async fn target_blank_links_network_and_env_ephemeral_parity_firefox() {
    assert_target_blank_links_network_and_env_ephemeral(EngineKind::Firefox).await;
}

async fn assert_shared_worker_network_blocking_and_route_rules(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid");

    session
        .set_blocked_urls(vec!["*blocked-sw*".to_string()])
        .await
        .expect("set_blocked_urls");

    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    mock_hdrs.insert("x-sw-mock".to_string(), "from-mock".to_string());
    session
        .set_route_mock(RouteMock {
            id: "sw-fetch-mock".to_string(),
            url_pattern: "*/api/sw-mock*".to_string(),
            action: RouteAction::Fulfill {
                status: 201,
                headers: mock_hdrs,
                body: br#"{"live":false,"mocked":true}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    let mut xhr_hdrs = BTreeMap::new();
    xhr_hdrs.insert("content-type".to_string(), "application/json".to_string());
    session
        .set_route_rule(RouteRule {
            id: "sw-xhr-rule".to_string(),
            url_pattern: "*/api/sw-xhr-mock*".to_string(),
            resource_types: vec![ResourceType::Xhr],
            action: RouteAction::Fulfill {
                status: 200,
                headers: xhr_hdrs,
                body: br#"{"xhrMocked":true}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_rule");

    session
        .navigate(&server.url("/sw-net-host"))
        .await
        .expect("navigate /sw-net-host");

    let res = session
        .eval_main_world("window.__runSwProbe()")
        .await
        .expect("run SharedWorker probe");

    assert_eq!(
        res["blockedFailed"], true,
        "{engine}: SharedWorker blocked URL fetch must fail: {res:?}"
    );
    assert_eq!(res["mockStatus"], 201, "{engine}: {res:?}");
    assert_eq!(res["mockHdr"], "from-mock", "{engine}: {res:?}");
    assert_eq!(res["mockJson"]["mocked"], true, "{engine}: {res:?}");
    assert_eq!(res["liveJson"]["live"], true, "{engine}: {res:?}");
    assert_eq!(res["xhrRes"]["ok"], true, "{engine}: {res:?}");
    assert_eq!(
        res["xhrRes"]["body"]["xhrMocked"], true,
        "{engine}: {res:?}"
    );

    let hits = server.hits();
    assert!(
        !hits.iter().any(|h| h.contains("/blocked-sw.png")),
        "{engine}: /blocked-sw.png hit server despite set_blocked_urls! hits={hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.contains("/api/sw-mock")),
        "{engine}: /api/sw-mock hit server despite set_route_mock! hits={hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.contains("/api/sw-xhr-mock")),
        "{engine}: /api/sw-xhr-mock hit server despite set_route_rule! hits={hits:?}"
    );
    assert!(
        hits.iter().any(|h| h.contains("/api/sw-live")),
        "{engine}: /api/sw-live should hit server normally; hits={hits:?}"
    );

    // Clear route mocks and update blocked URLs while SharedWorker is already running
    session
        .clear_route_mocks()
        .await
        .expect("clear_route_mocks");
    server.reset_hits();
    let res2 = session
        .eval_main_world("window.__runSwProbe()")
        .await
        .expect("run SharedWorker probe after clear");
    assert_eq!(res2["blockedFailed"], true, "{engine}: {res2:?}");
    assert_eq!(res2["mockJson"]["live"], true, "{engine}: {res2:?}");
    assert_eq!(res2["mockJson"]["mocked"], false, "{engine}: {res2:?}");
    assert_eq!(
        res2["xhrRes"]["body"]["xhrLive"], true,
        "{engine}: {res2:?}"
    );

    let hits2 = server.hits();
    assert!(!hits2.iter().any(|h| h.contains("/blocked-sw.png")));
    assert!(hits2.iter().any(|h| h.contains("/api/sw-mock")));

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn shared_worker_network_blocking_and_route_rules_chromium() {
    assert_shared_worker_network_blocking_and_route_rules(EngineKind::Chromium).await;
}

#[tokio::test]
async fn shared_worker_network_blocking_and_route_rules_firefox() {
    assert_shared_worker_network_blocking_and_route_rules(EngineKind::Firefox).await;
}

async fn assert_virtual_context_network_intercept_isolation(engine: EngineKind) {
    use mimikyu_core::driver::{ContextOptions, MouseButton, MouseEvent, MouseEventType};
    use mimikyu_core::env_profile::{EnvProfileBuilder, PresetKind};

    let server = TestServer::start().await;
    let profile_a = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .extra_header("X-Probe-Hdr", "ctx-a")
        .build()
        .expect("valid profile");

    let root = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::VirtualContext,
            ..Default::default()
        })
        .await
        .expect("launch virtual pool");
    let pid = root.info().pid.expect("pid");

    let ctx_a = root
        .new_context(ContextOptions {
            env_profile: Some(profile_a),
            ..Default::default()
        })
        .await
        .expect("new_context A");
    ctx_a
        .set_blocked_urls(vec!["*blocked-pop*".to_string()])
        .await
        .expect("ctx_a set_blocked_urls");
    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    mock_hdrs.insert("access-control-allow-origin".to_string(), "*".to_string());
    ctx_a
        .set_route_mock(RouteMock {
            id: "ctx-a-mock".to_string(),
            url_pattern: "*mock-pop-pre*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs,
                body: br#"{"mocked":true,"phase":"ctx-a"}"#.to_vec(),
            },
        })
        .await
        .expect("ctx_a set_route_mock");

    let ctx_b = root
        .new_context(ContextOptions::default())
        .await
        .expect("new_context B");

    // 1. ctx_a opens target=_blank popup and receives its own headers, mock, and block
    ctx_a
        .navigate(&server.url("/target-blank-host"))
        .await
        .expect("ctx_a navigate");
    let rect = ctx_a
        .resolve_selector_box("#lnk-blank")
        .await
        .expect("resolve #lnk-blank");
    let (cx, cy) = rect.center();
    ctx_a
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Move,
            x: cx,
            y: cy,
            button: MouseButton::None,
            click_count: 0,
            modifiers: 0,
        })
        .await
        .unwrap();
    ctx_a
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Down,
            x: cx,
            y: cy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .unwrap();
    ctx_a
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Up,
            x: cx,
            y: cy,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .unwrap();

    let mut a_out = serde_json::Value::Null;
    for _ in 0..120 {
        let val = ctx_a
            .eval_isolated(
                r#"(() => {
                    const a = document.getElementById('tb-blank')?.textContent || localStorage.getItem('tb_blank') || '';
                    return a ? JSON.parse(a) : null;
                })()"#,
            )
            .await
            .expect("poll ctx_a tb-blank");
        if !val.is_null() {
            a_out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !a_out.is_null(),
        "{engine}: timed out waiting for ctx_a popup"
    );
    assert_eq!(a_out["xHdr"], "ctx-a");
    assert_eq!(a_out["mocked"]["phase"], "ctx-a");
    assert_eq!(a_out["blockedOk"], false);
    let a_hits = server.hits();
    assert!(!a_hits.iter().any(|h| h.contains("/blocked-pop.js")));
    assert!(!a_hits.iter().any(|h| h.contains("/api/mock-pop-pre")));

    // 2. Sibling ctx_b on the same browser process must NOT inherit ctx_a's headers, mocks, or blocks
    server.reset_hits();
    ctx_b
        .navigate(&server.url("/target-blank-host"))
        .await
        .expect("ctx_b navigate");
    let rect_b = ctx_b
        .resolve_selector_box("#lnk-blank")
        .await
        .expect("ctx_b resolve #lnk-blank");
    let (bx, by) = rect_b.center();
    ctx_b
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Move,
            x: bx,
            y: by,
            button: MouseButton::None,
            click_count: 0,
            modifiers: 0,
        })
        .await
        .unwrap();
    ctx_b
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Down,
            x: bx,
            y: by,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .unwrap();
    ctx_b
        .dispatch_mouse(MouseEvent {
            event_type: MouseEventType::Up,
            x: bx,
            y: by,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        })
        .await
        .unwrap();

    let mut b_out = serde_json::Value::Null;
    for _ in 0..120 {
        let val = ctx_b
            .eval_isolated(
                r#"(() => {
                    const a = document.getElementById('tb-blank')?.textContent || localStorage.getItem('tb_blank') || '';
                    return a ? JSON.parse(a) : null;
                })()"#,
            )
            .await
            .expect("poll ctx_b tb-blank");
        if !val.is_null() {
            b_out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !b_out.is_null(),
        "{engine}: timed out waiting for ctx_b popup"
    );
    assert!(
        b_out["xHdr"].is_null(),
        "{engine}: ctx_b leaked xHdr: {b_out:?}"
    );
    assert_ne!(
        b_out["mocked"]["phase"], "ctx-a",
        "{engine}: ctx_b leaked ctx_a mock: {b_out:?}"
    );
    let b_hits = server.hits();
    assert!(
        b_hits.iter().any(|h| h.contains("/blocked-pop.js")),
        "{engine}: ctx_b /blocked-pop.js did not reach server (leaked ctx_a block?): hits={b_hits:?}"
    );
    assert!(
        b_hits.iter().any(|h| h.contains("/api/mock-pop-pre")),
        "{engine}: ctx_b /api/mock-pop-pre did not reach server (leaked ctx_a mock?): hits={b_hits:?}"
    );

    let b_req_hdrs = server.request_headers();
    let (_, child_hdrs) = b_req_hdrs
        .iter()
        .find(|(req, _)| req == "GET /target-blank-child?tag=blank")
        .expect("ctx_b initial popup request recorded");
    assert_eq!(
        child_hdrs.get("x-probe-hdr"),
        None,
        "{engine}: ctx_b popup initial request leaked x-probe-hdr"
    );

    // 3. Disposing ctx_a cleans up intercept without stalling ctx_b
    ctx_a.dispose().await.expect("dispose ctx_a");
    ctx_b
        .navigate(&server.url("/net/page"))
        .await
        .expect("ctx_b navigate after ctx_a dispose");

    ctx_b.dispose().await.expect("dispose ctx_b");
    root.dispose().await.expect("dispose root");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn virtual_context_network_intercept_isolation_chromium() {
    assert_virtual_context_network_intercept_isolation(EngineKind::Chromium).await;
}

#[tokio::test]
async fn virtual_context_network_intercept_isolation_firefox() {
    assert_virtual_context_network_intercept_isolation(EngineKind::Firefox).await;
}

async fn assert_shared_worker_blob_immediate_revoke_with_route_mock_no_env(engine: EngineKind) {
    let server = TestServer::start().await;
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            env_profile: None,
            ..Default::default()
        })
        .await
        .expect("launch");
    let pid = session.info().pid.expect("pid");

    let mut hdrs = BTreeMap::new();
    hdrs.insert("content-type".to_string(), "application/json".to_string());
    session
        .set_route_mock(RouteMock {
            id: "sw-blob-revoke-mock".to_string(),
            url_pattern: "*/api/sw-mock*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: hdrs,
                body: br#"{"mocked":true,"source":"blob-sw"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    session
        .navigate(&server.url("/target"))
        .await
        .expect("navigate /target");

    let sw_url = server.url("/api/sw-mock");
    let probe_js = format!(
        r#"(async () => {{
                return await new Promise((resolve) => {{
                    const timer = setTimeout(() => resolve({{ ok: false, err: 'timeout' }}), 5000);
                    const code = `
                        onconnect = async (ev) => {{
                            const port = ev.ports[0];
                            try {{
                                const r = await fetch({:?});
                                const data = await r.json();
                                port.postMessage({{ ok: true, data }});
                            }} catch (e) {{
                                port.postMessage({{ ok: false, err: String(e) }});
                            }}
                        }};
                    `;
                    const blob = new Blob([code], {{ type: 'application/javascript' }});
                    const u = URL.createObjectURL(blob);
                    const sw = new SharedWorker(u);
                    URL.revokeObjectURL(u);
                    sw.onerror = (e) => {{
                        clearTimeout(timer);
                        resolve({{ ok: false, err: (e && e.message) ? e.message : 'SharedWorker error' }});
                    }};
                    sw.port.onmessage = (ev) => {{
                        clearTimeout(timer);
                        resolve(ev.data);
                    }};
                    sw.port.start();
                }});
            }})()"#,
        sw_url
    );
    let res = session
        .eval_main_world(&probe_js)
        .await
        .expect("eval SharedWorker blob revoke probe");

    assert_eq!(
        res["ok"], true,
        "{engine} SharedWorker blob revoke failed: {res:?}"
    );
    assert_eq!(
        res["data"]["mocked"], true,
        "{engine} mock mismatch: {res:?}"
    );
    assert_eq!(
        res["data"]["source"], "blob-sw",
        "{engine} source mismatch: {res:?}"
    );
    assert!(!server.hits().iter().any(|h| h.contains("/api/sw-mock")));

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn shared_worker_blob_immediate_revoke_with_route_mock_no_env_chromium() {
    assert_shared_worker_blob_immediate_revoke_with_route_mock_no_env(EngineKind::Chromium).await;
}

#[tokio::test]
async fn shared_worker_blob_immediate_revoke_with_route_mock_no_env_firefox() {
    assert_shared_worker_blob_immediate_revoke_with_route_mock_no_env(EngineKind::Firefox).await;
}
