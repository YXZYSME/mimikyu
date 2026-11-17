mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use mimikyu_core::driver::{
    driver_for, EngineKind, LaunchOptions, ProfileMode, RouteAction, RouteMock,
};
use mimikyu_core::network::{BlockPolicy, ResourceType, RouteRule};
use support::{assert_pgid_gone, TestServer};

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

    session.clear_route_mocks().await.expect("clear_route_mocks");
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
async fn resource_blocker_filters_images_media_fonts_including_extensionless_and_iframes_chromium() {
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

    assert!(logs.iter().any(|c| c.text.contains("net-log-1") && c.level == "log"));
    assert!(logs.iter().any(|c| c.text.contains("net-info-2") && c.level == "info"));
    assert!(logs.iter().any(|c| c.text.contains("net-warn-3") && (c.level == "warning" || c.level == "warn")));
    assert!(logs.iter().any(|c| c.text.contains("net-err-4") && c.level == "error"));
    assert!(
        !logs.iter().any(|c| c.text.contains("uncaught-boom-42")),
        "{engine}: uncaught page error must not be mixed into console_logs"
    );
    assert!(
        errs.iter().any(|e| e.message.contains("uncaught-boom-42")),
        "{engine}: uncaught page error missing from page_errors: {errs:?}"
    );
    assert!(
        nets.iter().any(|n| n.url.contains("/net/console-and-errors") && n.status == Some(200)),
        "{engine}: network_events missing 200 entry"
    );
    assert_eq!(session.inflight_requests().await.unwrap(), 0);

    // Verify drain_* empties buffers
    let drained_c = session.drain_console_logs().await.expect("drain console");
    assert!(!drained_c.is_empty());
    assert!(session.console_logs().await.unwrap().is_empty());

    let drained_e = session.drain_page_errors().await.expect("drain page_errors");
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
