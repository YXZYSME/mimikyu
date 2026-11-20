mod support;

use std::time::Duration;

use mimikyu_core::driver::{
    ContextOptions, DriverSession, EngineKind, EnvProfile, LaunchOptions, ProfileMode, ProxyConfig,
    WebRtcPolicy, driver_for,
};
use mimikyu_core::env_profile::{
    EnvProfileBuilder, PresetKind, lookup_geoip, validate_or_err, validate_profile,
};
use mimikyu_core::isolation::VirtualContextPool;
use support::{
    KeepAliveTestServer, LocalHttpProxy, LocalSocks5Proxy, StrictKeepAliveHttpProxy, TestServer,
    assert_pgid_gone,
};

async fn poll_non_null(session: &dyn DriverSession, expr: &str, label: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let val = session.eval_isolated(expr).await.expect(label);
        if !val.is_null() {
            return val;
        }
        if tokio::time::Instant::now() >= deadline {
            return serde_json::Value::Null;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn coherence_validator_accepts_presets_and_rejects_mismatches() {
    for preset in [PresetKind::Win11, PresetKind::MacOs, PresetKind::Linux] {
        for engine in [EngineKind::Chromium, EngineKind::Firefox] {
            let prof = EnvProfileBuilder::from_preset(preset, engine)
                .geoip("DE")
                .expect("apply geoip DE")
                .build()
                .expect("coherent preset + geoip DE");
            assert_eq!(prof.timezone, "Europe/Berlin");
            assert_eq!(prof.locale, "de-DE");
            assert_eq!(prof.languages[0], "de-DE");
            assert!(prof.geolocation.is_some());
        }
    }

    // Mismatch 1: Windows preset with MacIntel platform
    let mut bad_os = PresetKind::Win11.to_env_profile(EngineKind::Chromium);
    bad_os.platform = "MacIntel".to_string();
    assert!(validate_or_err(&bad_os).is_err());

    // Mismatch 2: Viewport exceeds screen
    let mut bad_vp = PresetKind::Linux.to_env_profile(EngineKind::Firefox);
    bad_vp.viewport_width = 2560;
    assert!(validate_or_err(&bad_vp).is_err());

    // Mismatch 3: Firefox UA with Chromium Client Hints metadata
    let mut bad_ffx = PresetKind::MacOs.to_env_profile(EngineKind::Firefox);
    bad_ffx.user_agent_metadata = PresetKind::MacOs
        .to_env_profile(EngineKind::Chromium)
        .user_agent_metadata;
    assert!(validate_or_err(&bad_ffx).is_err());

    // Mismatch 4: Locale vs languages mismatch
    let mut bad_loc = PresetKind::Win11.to_env_profile(EngineKind::Chromium);
    bad_loc.locale = "ja-JP".to_string();
    assert!(validate_or_err(&bad_loc).is_err());

    // Mismatch 5: Non-standard DPR warning on macOS
    let mut warn_mac = PresetKind::MacOs.to_env_profile(EngineKind::Chromium);
    warn_mac.device_scale_factor = 1.0;
    let rep = validate_profile(&warn_mac);
    assert!(rep.valid);
    assert!(rep.warnings.iter().any(|w| w.code == "MACOS_DPR_ONE"));
}

const PROBE_ISOLATED_JS: &str = r#"(() => {
  const mTxt = document.getElementById('main-out')?.textContent || '';
  const fTxt = document.getElementById('frame-out')?.textContent || '';
  const wTxt = document.getElementById('worker-out')?.textContent || '';
  const mwTxt = document.getElementById('mworker-out')?.textContent || '';
  if (!mTxt || !fTxt || !wTxt || !mwTxt) return null;
  const main = JSON.parse(mTxt);
  main.frame = JSON.parse(fTxt);
  main.worker = JSON.parse(wTxt);
  main.mworker = JSON.parse(mwTxt);
  return main;
})()"#;

async fn assert_cross_frame_and_worker_consistency(engine: EngineKind) {
    let server = TestServer::start().await;

    let profile = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .viewport(1366, 768)
        .screen(1920, 1080)
        .webrtc(WebRtcPolicy::DisableAll)
        .extra_header("X-Custom-Env", "mimikyu-m2")
        .build()
        .expect("valid profile");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            env_profile: Some(profile.clone()),
            ..Default::default()
        })
        .await
        .expect("launch with env_profile");
    let pid = session.info().pid.expect("pid");

    // 1. Verify HTTP request headers
    session
        .navigate(&server.url("/echo-env"))
        .await
        .expect("navigate /echo-env");
    let hdr_body = session
        .eval_isolated("document.body.innerText")
        .await
        .expect("echo-env body");
    let hdrs: serde_json::Value =
        serde_json::from_str(hdr_body.as_str().unwrap_or("{}")).expect("parse headers");
    assert_eq!(
        hdrs["user-agent"].as_str().unwrap_or(""),
        profile.user_agent,
        "{engine}: HTTP User-Agent must match EnvProfile"
    );
    let accept_lang = hdrs["accept-language"].as_str().unwrap_or("");
    assert!(
        accept_lang.starts_with("de-DE"),
        "{engine}: HTTP Accept-Language must start with de-DE, got {accept_lang}"
    );
    assert_eq!(
        hdrs["x-custom-env"].as_str().unwrap_or(""),
        "mimikyu-m2",
        "{engine}: custom extra_header missing"
    );
    if engine == EngineKind::Chromium {
        assert_eq!(
            hdrs["sec-ch-ua-platform"].as_str().unwrap_or(""),
            "\"Linux\"",
            "Chromium Sec-CH-UA-Platform must match Linux"
        );
    }

    // 2. Verify main frame + iframe + Web Worker JS consistency
    session
        .navigate(&server.url("/env-probe"))
        .await
        .expect("navigate /env-probe");

    let mut probe = serde_json::Value::Null;
    for _ in 0..80 {
        let p = session
            .eval_isolated(PROBE_ISOLATED_JS)
            .await
            .expect("probe eval");
        if !p.is_null() {
            probe = p;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !probe["frame"].is_null() && !probe["worker"].is_null() && !probe["mworker"].is_null(),
        "{engine}: timed out waiting for frame/worker/mworker probe: {probe:?}"
    );

    // Main frame assertions
    assert_eq!(probe["userAgent"], profile.user_agent);
    assert_eq!(probe["platform"], "Linux x86_64");
    assert_eq!(probe["language"], "de-DE");
    assert_eq!(probe["languages"][0], "de-DE");
    assert_eq!(probe["hardwareConcurrency"], 8);
    assert_eq!(probe["webdriver"], false);
    assert_eq!(probe["screenWidth"], 1920);
    assert_eq!(probe["screenHeight"], 1080);
    assert_eq!(probe["innerWidth"], 1366);
    assert_eq!(probe["innerHeight"], 768);
    assert_eq!(probe["timeZone"], "Europe/Berlin");
    assert_eq!(probe["rtcBlocked"], true);
    assert_eq!(probe["noMimikyuGlobal"], true);
    assert_eq!(probe["audioSampleRate"], 48000);
    assert_eq!(
        probe["webglVendor"],
        profile.webgl_vendor.as_deref().unwrap()
    );
    assert_eq!(
        probe["webglRenderer"],
        profile.webgl_renderer.as_deref().unwrap()
    );
    assert!(
        probe["glVendor"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "{engine}: gl.getParameter(gl.VENDOR) must be non-empty: {probe:?}"
    );
    assert!(
        probe["glRenderer"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "{engine}: gl.getParameter(gl.RENDERER) must be non-empty: {probe:?}"
    );
    assert!(
        probe["glVersion"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "{engine}: gl.getParameter(gl.VERSION) must be non-empty: {probe:?}"
    );
    if engine == EngineKind::Chromium {
        assert_eq!(probe["deviceMemory"], 8);
        assert_eq!(probe["userAgentData"]["platform"], "Linux");
    } else {
        assert!(probe["deviceMemory"].is_null());
        assert!(probe["userAgentData"].is_null());
    }

    // Iframe assertions
    let frame = &probe["frame"];
    assert_eq!(frame["userAgent"], profile.user_agent);
    assert_eq!(frame["platform"], "Linux x86_64");
    assert_eq!(frame["language"], "de-DE");
    assert_eq!(frame["languages"][0], "de-DE");
    assert_eq!(frame["timeZone"], "Europe/Berlin");
    assert_eq!(frame["screenWidth"], 1920);
    assert_eq!(frame["webdriver"], false);
    assert_eq!(frame["noMimikyuGlobal"], true);

    // Worker assertions
    let worker = &probe["worker"];
    assert_eq!(
        worker["userAgent"], profile.user_agent,
        "{engine} worker UA"
    );
    assert_eq!(
        worker["platform"], "Linux x86_64",
        "{engine} worker platform"
    );
    assert_eq!(worker["language"], "de-DE", "{engine} worker language");
    assert_eq!(
        worker["languages"][0], "de-DE",
        "{engine} worker languages[0]"
    );
    assert_eq!(worker["hardwareConcurrency"], 8, "{engine} worker cores");
    assert_eq!(
        worker["timeZone"], "Europe/Berlin",
        "{engine} worker timeZone"
    );
    assert_eq!(
        worker["tzOffset"], probe["tzOffset"],
        "{engine} worker getTimezoneOffset must match main frame"
    );
    if engine == EngineKind::Chromium {
        assert_eq!(worker["deviceMemory"], 8);
    }

    // ES module Worker assertions
    let mworker = &probe["mworker"];
    assert_eq!(
        mworker["userAgent"], profile.user_agent,
        "{engine} mworker UA"
    );
    assert_eq!(
        mworker["platform"], "Linux x86_64",
        "{engine} mworker platform"
    );
    assert_eq!(mworker["language"], "de-DE", "{engine} mworker language");
    assert_eq!(
        mworker["languages"][0], "de-DE",
        "{engine} mworker languages[0]"
    );
    assert_eq!(mworker["hardwareConcurrency"], 8, "{engine} mworker cores");
    assert_eq!(
        mworker["timeZone"], "Europe/Berlin",
        "{engine} mworker timeZone"
    );
    assert_eq!(
        mworker["tzOffset"], probe["tzOffset"],
        "{engine} mworker getTimezoneOffset must match main frame"
    );
    assert_eq!(
        mworker["helperTag"], "relative-helper-ok",
        "{engine} mworker relative ./helper.mjs import"
    );
    assert_eq!(
        mworker["workerMetaUrl"],
        server.url("/env-worker.mjs"),
        "{engine} mworker import.meta.url"
    );
    assert_eq!(
        mworker["helperMeta"]["helperUrl"],
        server.url("/helper.mjs"),
        "{engine} helper import.meta.url"
    );
    if engine == EngineKind::Chromium {
        assert_eq!(mworker["deviceMemory"], 8);
    }

    // 3. Test Geolocation API with GeoIP alignment
    session
        .navigate(&server.url("/geo-page"))
        .await
        .expect("navigate /geo-page");
    let coords = session
        .eval_isolated(
            r#"new Promise((resolve, reject) => {
                navigator.geolocation.getCurrentPosition(
                    (pos) => resolve({ lat: pos.coords.latitude, lon: pos.coords.longitude }),
                    (err) => reject(err.message),
                    { timeout: 5000 }
                );
            })"#,
        )
        .await
        .expect("getCurrentPosition");
    let de = lookup_geoip("DE").unwrap();
    assert!((coords["lat"].as_f64().unwrap() - de.latitude).abs() < 0.01);
    assert!((coords["lon"].as_f64().unwrap() - de.longitude).abs() < 0.01);

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn env_profile_cross_frame_and_worker_consistency_chromium() {
    assert_cross_frame_and_worker_consistency(EngineKind::Chromium).await;
}

#[tokio::test]
async fn env_profile_cross_frame_and_worker_consistency_firefox() {
    assert_cross_frame_and_worker_consistency(EngineKind::Firefox).await;
}

async fn assert_virtual_context_env_and_auth_proxies(engine: EngineKind) {
    let server = TestServer::start().await;
    let http_proxy = LocalHttpProxy::start(Some(("alice".into(), "s3cr3t".into()))).await;
    let socks_proxy = LocalSocks5Proxy::start(Some(("bob".into(), "p@ss".into()))).await;

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        proxy: Some(ProxyConfig {
            server: http_proxy.proxy_url(),
            username: Some("alice".to_string()),
            password: Some("s3cr3t".to_string()),
            bypass: Vec::new(),
        }),
        ..Default::default()
    })
    .await
    .expect("launch pool with auth http proxy");
    let pid = pool.root_session().info().pid.expect("pid");

    // Root session routes through authenticated HTTP proxy via ProxyRelay
    pool.root_session()
        .navigate(&server.url("/echo-env"))
        .await
        .expect("root navigate via auth http proxy");
    assert!(
        !http_proxy.hits().is_empty(),
        "{engine}: root session must route via authenticated HTTP proxy"
    );

    // VirtualContext with JP GeoIP + macOS preset + authenticated SOCKS5 proxy
    let jp_profile = EnvProfileBuilder::from_preset(PresetKind::MacOs, engine)
        .geoip("203.0.113.5")
        .expect("geoip 203.0.113.5 -> JP")
        .build()
        .expect("valid jp profile");

    let vc = pool
        .create_labeled_context(
            "jp-socks",
            ContextOptions {
                proxy: Some(ProxyConfig {
                    server: socks_proxy.proxy_url(),
                    username: Some("bob".to_string()),
                    password: Some("p@ss".to_string()),
                    bypass: Vec::new(),
                }),
                env_profile: Some(jp_profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc with socks5 + jp profile");

    vc.session()
        .navigate(&server.url("/echo-env"))
        .await
        .expect("vc navigate /echo-env via socks5");
    assert!(
        !socks_proxy.hits().is_empty(),
        "{engine}: VirtualContext must route via authenticated SOCKS5 proxy"
    );

    let vc_hdr_text = vc
        .session()
        .eval_isolated("document.body.innerText")
        .await
        .unwrap();
    let vc_hdrs: serde_json::Value =
        serde_json::from_str(vc_hdr_text.as_str().unwrap_or("{}")).unwrap();
    assert!(
        vc_hdrs["accept-language"]
            .as_str()
            .unwrap_or("")
            .starts_with("ja-JP"),
        "{engine}: VC Accept-Language must start with ja-JP, got {:?}",
        vc_hdrs["accept-language"]
    );

    vc.session()
        .navigate(&server.url("/env-probe"))
        .await
        .expect("vc /env-probe");
    let mut probe = serde_json::Value::Null;
    for _ in 0..80 {
        let p = vc.session().eval_isolated(PROBE_ISOLATED_JS).await.unwrap();
        if !p.is_null() {
            probe = p;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(probe["timeZone"], "Asia/Tokyo");
    assert_eq!(probe["language"], "ja-JP");
    assert_eq!(probe["platform"], "MacIntel");
    assert_eq!(probe["frame"]["timeZone"], "Asia/Tokyo");
    assert_eq!(probe["worker"]["timeZone"], "Asia/Tokyo");
    assert_eq!(probe["worker"]["language"], "ja-JP");
    assert_eq!(
        probe["mworker"]["platform"], "MacIntel",
        "{engine} VC mworker platform"
    );
    assert_eq!(
        probe["mworker"]["hardwareConcurrency"], jp_profile.hardware_concurrency,
        "{engine} VC mworker cores"
    );
    assert_eq!(
        probe["mworker"]["language"], "ja-JP",
        "{engine} VC mworker language"
    );
    assert_eq!(
        probe["mworker"]["languages"][0], "ja-JP",
        "{engine} VC mworker languages[0]"
    );
    assert_eq!(
        probe["mworker"]["timeZone"], "Asia/Tokyo",
        "{engine} VC mworker timeZone"
    );
    assert_eq!(
        probe["mworker"]["tzOffset"], probe["tzOffset"],
        "{engine} VC mworker tzOffset"
    );

    // Second VirtualContext with non-host Win11 platform/cores + DE GeoIP to prove ES module Worker overrides platform & hardwareConcurrency in VirtualContext
    let win_de = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("valid win11 DE profile");
    let vc_win = pool
        .create_labeled_context(
            "win-de",
            ContextOptions {
                env_profile: Some(win_de),
                ..Default::default()
            },
        )
        .await
        .expect("create win-de vc");
    vc_win
        .session()
        .navigate(&server.url("/env-probe"))
        .await
        .expect("vc_win /env-probe");
    let mut win_probe = serde_json::Value::Null;
    for _ in 0..80 {
        let p = vc_win
            .session()
            .eval_isolated(PROBE_ISOLATED_JS)
            .await
            .unwrap();
        if !p.is_null() {
            win_probe = p;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !win_probe["mworker"].is_null(),
        "{engine}: win_probe mworker timed out"
    );
    assert_eq!(
        win_probe["mworker"]["platform"], "Win32",
        "{engine} VC win32 mworker platform"
    );
    assert_eq!(
        win_probe["mworker"]["hardwareConcurrency"], 8,
        "{engine} VC win32 mworker cores"
    );
    assert_eq!(
        win_probe["mworker"]["language"], "de-DE",
        "{engine} VC win32 mworker language"
    );
    assert_eq!(
        win_probe["mworker"]["languages"][0], "de-DE",
        "{engine} VC win32 mworker languages[0]"
    );
    assert_eq!(
        win_probe["mworker"]["timeZone"], "Europe/Berlin",
        "{engine} VC win32 mworker timeZone"
    );
    assert_eq!(
        win_probe["mworker"]["tzOffset"], win_probe["tzOffset"],
        "{engine} VC win32 mworker tzOffset"
    );

    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn proxy_relay_and_vc_geoip_alignment_chromium() {
    assert_virtual_context_env_and_auth_proxies(EngineKind::Chromium).await;
}

#[tokio::test]
async fn proxy_relay_and_vc_geoip_alignment_firefox() {
    assert_virtual_context_env_and_auth_proxies(EngineKind::Firefox).await;
}

async fn assert_keepalive_auth_proxies(engine: EngineKind) {
    let server = KeepAliveTestServer::start().await;
    let strict_http_proxy =
        StrictKeepAliveHttpProxy::start(("ka_user".into(), "ka_pass".into())).await;
    let socks_proxy = LocalSocks5Proxy::start(Some(("socks_u".into(), "socks_p".into()))).await;

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        proxy: Some(ProxyConfig {
            server: strict_http_proxy.proxy_url(),
            username: Some("ka_user".to_string()),
            password: Some("ka_pass".to_string()),
            bypass: Vec::new(),
        }),
        ..Default::default()
    })
    .await
    .expect("launch pool with strict keep-alive auth http proxy");
    let pid = pool.root_session().info().pid.expect("pid");

    // 1. Root session: sequential fetch() over HTTP/1.1 Keep-Alive via strict per-request auth HTTP proxy
    pool.root_session()
        .navigate(&server.url("/ka-page"))
        .await
        .expect("root navigate /ka-page");

    let mut http_out = serde_json::Value::Null;
    for _ in 0..80 {
        let val = pool
            .root_session()
            .eval_isolated("document.getElementById('ka-out')?.textContent || ''")
            .await
            .unwrap();
        let txt = val.as_str().unwrap_or("");
        if !txt.is_empty() {
            http_out = serde_json::from_str(txt).expect("parse ka-out json");
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        http_out["s1"], 200,
        "{engine} HTTP keepalive s1: {http_out:?}"
    );
    assert_eq!(http_out["b1"], "ka:n=1");
    assert_eq!(
        http_out["s2"], 200,
        "{engine} HTTP keepalive s2: {http_out:?}"
    );
    assert_eq!(http_out["b2"], "ka:n=2");
    assert_eq!(
        http_out["s3"], 200,
        "{engine} HTTP keepalive s3: {http_out:?}"
    );
    assert_eq!(http_out["b3"], "ka:n=3");
    assert_eq!(
        http_out["s4"], 200,
        "{engine} HTTP keepalive chunked s4: {http_out:?}"
    );
    assert_eq!(http_out["b4"], "chunked-ok");
    assert_eq!(
        strict_http_proxy.auth_rejects(),
        0,
        "{engine}: strict HTTP proxy must never reject with 407"
    );
    assert!(
        strict_http_proxy.hits().len() >= 5,
        "{engine}: expected >=5 requests through strict HTTP proxy, got {:?}",
        strict_http_proxy.hits()
    );

    // 2. VirtualContext: sequential fetch() over HTTP/1.1 Keep-Alive via authenticated SOCKS5 proxy
    let vc = pool
        .create_labeled_context(
            "ka-socks",
            ContextOptions {
                proxy: Some(ProxyConfig {
                    server: socks_proxy.proxy_url(),
                    username: Some("socks_u".to_string()),
                    password: Some("socks_p".to_string()),
                    bypass: Vec::new(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("create vc with auth socks5 proxy");

    vc.session()
        .navigate(&server.url("/ka-page"))
        .await
        .expect("vc navigate /ka-page via socks5");

    let mut socks_out = serde_json::Value::Null;
    for _ in 0..80 {
        let val = vc
            .session()
            .eval_isolated("document.getElementById('ka-out')?.textContent || ''")
            .await
            .unwrap();
        let txt = val.as_str().unwrap_or("");
        if !txt.is_empty() {
            socks_out = serde_json::from_str(txt).expect("parse socks ka-out json");
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        socks_out["s1"], 200,
        "{engine} SOCKS5 keepalive s1: {socks_out:?}"
    );
    assert_eq!(socks_out["b1"], "ka:n=1");
    assert_eq!(
        socks_out["s2"], 200,
        "{engine} SOCKS5 keepalive s2: {socks_out:?}"
    );
    assert_eq!(socks_out["b2"], "ka:n=2");
    assert_eq!(
        socks_out["s3"], 200,
        "{engine} SOCKS5 keepalive s3: {socks_out:?}"
    );
    assert_eq!(socks_out["b3"], "ka:n=3");
    assert_eq!(
        socks_out["s4"], 200,
        "{engine} SOCKS5 keepalive chunked s4: {socks_out:?}"
    );
    assert_eq!(socks_out["b4"], "chunked-ok");
    assert!(
        socks_proxy.hits().len() >= 5,
        "{engine}: expected >=5 connections through SOCKS5 proxy, got {:?}",
        socks_proxy.hits()
    );

    // Every request reaching KeepAliveTestServer must use origin-form path (never proxy-form http://...)
    let server_hits = server.hits();
    for hit in &server_hits {
        assert!(
            !hit.contains("http://"),
            "{engine}: origin server received raw proxy-form URI: {hit}"
        );
    }
    assert!(server_hits.iter().any(|h| h == "GET /api/ka?n=1"));
    assert!(server_hits.iter().any(|h| h == "GET /api/ka?n=2"));
    assert!(server_hits.iter().any(|h| h == "GET /api/ka?n=3"));
    assert!(server_hits.iter().any(|h| h == "GET /api/ka-chunked"));

    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn proxy_relay_keepalive_auth_proxies_chromium() {
    assert_keepalive_auth_proxies(EngineKind::Chromium).await;
}

#[tokio::test]
async fn proxy_relay_keepalive_auth_proxies_firefox() {
    assert_keepalive_auth_proxies(EngineKind::Firefox).await;
}

async fn assert_no_auth_socks5h_virtual_context_launch(engine: EngineKind) {
    let server = TestServer::start().await;
    let socks = LocalSocks5Proxy::start(None).await;

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::VirtualContext,
            proxy: Some(ProxyConfig {
                server: socks.socks5h_url(),
                username: None,
                password: None,
                bypass: Vec::new(),
            }),
            ..Default::default()
        })
        .await
        .expect("launch VirtualContext with unauthenticated socks5h proxy");
    let pid = session.info().pid.expect("pid");

    let nav = session
        .navigate(&server.url("/echo-env"))
        .await
        .expect("navigate /echo-env via unauthenticated socks5h proxy");
    assert_eq!(
        nav.status,
        Some(200),
        "{engine}: expected HTTP 200 via socks5h"
    );
    assert!(
        !socks.hits().is_empty(),
        "{engine}: expected LocalSocks5Proxy hits for unauthenticated socks5h://"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn no_auth_socks5h_virtual_context_launch_chromium() {
    assert_no_auth_socks5h_virtual_context_launch(EngineKind::Chromium).await;
}

#[tokio::test]
async fn no_auth_socks5h_virtual_context_launch_firefox() {
    assert_no_auth_socks5h_virtual_context_launch(EngineKind::Firefox).await;
}

const MODULE_WORKER_PROBE_JS: &str = r#"(() => {
  const get = (id) => document.getElementById(id)?.textContent || '';
  const m = get('main-out');
  const d = get('delay-out');
  const p = get('port-out');
  const f = get('fast-out');
  const e = get('err404-out');
  const s1 = get('shared1-out');
  const s2 = get('shared2-out');
  if (!m || !d || !p || !f || !e || !s1 || !s2) return null;
  return {
    main: JSON.parse(m),
    delay: JSON.parse(d),
    port: JSON.parse(p),
    fast: JSON.parse(f),
    err404: JSON.parse(e),
    shared1: JSON.parse(s1),
    shared2: JSON.parse(s2),
  };
})()"#;

async fn assert_module_worker_probe(
    session: &dyn DriverSession,
    server: &TestServer,
    profile: &EnvProfile,
    engine: EngineKind,
    label: &str,
) {
    session
        .navigate(&server.url("/module-worker-probe"))
        .await
        .expect("navigate /module-worker-probe");

    let mut out = serde_json::Value::Null;
    for _ in 0..160 {
        let val = session
            .eval_isolated(MODULE_WORKER_PROBE_JS)
            .await
            .expect("eval module-worker-probe");
        if !val.is_null() {
            out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !out.is_null(),
        "{engine} ({label}): timed out waiting for /module-worker-probe outputs"
    );

    let main = &out["main"];

    // 1. Delayed (150ms) ES module Worker: immediate postMessage + MessageChannel port + both handlers
    let delay = &out["delay"];
    assert!(
        delay["error"].is_null(),
        "{engine} ({label}): delay worker error: {delay:?}"
    );
    let expected_delay_msgs = serde_json::json!([
        {"ping": "immediate-delayed", "seq": 1},
        {"ping": "immediate-delayed-port", "seq": 2}
    ]);
    assert_eq!(
        delay["onmessageMsgs"], expected_delay_msgs,
        "{engine} ({label}): delay self.onmessage missed/reordered immediate messages: {delay:?}"
    );
    assert_eq!(
        delay["listenerMsgs"], expected_delay_msgs,
        "{engine} ({label}): delay addEventListener('message') missed/reordered immediate messages: {delay:?}"
    );
    assert_eq!(
        delay["portsLen"], 1,
        "{engine} ({label}): delay MessagePort not preserved: {delay:?}"
    );
    assert_eq!(
        out["port"]["portMsg"], "port-pong:2",
        "{engine} ({label}): transferred MessagePort reply failed: {:?}",
        out["port"]
    );
    assert_eq!(delay["helperTag"], "relative-helper-ok");
    assert_eq!(delay["workerMetaUrl"], server.url("/echo-worker.mjs"));
    assert_eq!(delay["helperMeta"]["helperUrl"], server.url("/helper.mjs"));
    assert_eq!(delay["helperMeta"]["platformFromHelper"], profile.platform);
    assert_eq!(delay["helperMeta"]["uaFromHelper"], profile.user_agent);
    assert_eq!(delay["helperMeta"]["tzFromHelper"], profile.timezone);
    assert_eq!(delay["userAgent"], profile.user_agent);
    assert_eq!(delay["platform"], profile.platform);
    assert_eq!(delay["language"], profile.locale);
    assert_eq!(delay["languages"][0], profile.locale);
    assert_eq!(delay["hardwareConcurrency"], profile.hardware_concurrency);
    assert_eq!(delay["timeZone"], profile.timezone);
    assert_eq!(delay["tzOffset"], main["tzOffset"]);

    // 2. Fast (0ms) ES module Worker: immediate postMessage + both handlers
    let fast = &out["fast"];
    assert!(
        fast["error"].is_null(),
        "{engine} ({label}): fast worker error: {fast:?}"
    );
    let expected_fast_msgs = serde_json::json!([
        {"ping": "immediate-fast", "seq": 10},
        {"ping": "immediate-fast", "seq": 11}
    ]);
    assert_eq!(
        fast["onmessageMsgs"], expected_fast_msgs,
        "{engine} ({label}): fast self.onmessage missed/reordered immediate messages: {fast:?}"
    );
    assert_eq!(
        fast["listenerMsgs"], expected_fast_msgs,
        "{engine} ({label}): fast addEventListener('message') missed/reordered immediate messages: {fast:?}"
    );
    assert_eq!(fast["helperTag"], "relative-helper-ok");
    assert_eq!(fast["workerMetaUrl"], server.url("/echo-fast-worker.mjs"));
    assert_eq!(fast["helperMeta"]["helperUrl"], server.url("/helper.mjs"));
    assert_eq!(fast["userAgent"], profile.user_agent);
    assert_eq!(fast["platform"], profile.platform);
    assert_eq!(fast["language"], profile.locale);
    assert_eq!(fast["hardwareConcurrency"], profile.hardware_concurrency);
    assert_eq!(fast["timeZone"], profile.timezone);
    assert_eq!(fast["tzOffset"], main["tzOffset"]);

    // 3. 404 ES module Worker onerror propagation
    assert_eq!(
        out["err404"]["fired"], true,
        "{engine} ({label}): 404 module worker onerror did not fire: {:?}",
        out["err404"]
    );

    // 4. SharedWorker module probe (connect buffering + shared instance via cached blobUrl)
    let shared1 = &out["shared1"];
    let shared2 = &out["shared2"];
    if shared1["unsupported"] != true {
        assert!(
            shared1["error"].is_null(),
            "{engine} ({label}): shared1 error: {shared1:?}"
        );
        assert!(
            shared2["error"].is_null(),
            "{engine} ({label}): shared2 error: {shared2:?}"
        );
        assert_eq!(shared1["echoed"]["seq"], 101);
        assert_eq!(shared2["echoed"]["seq"], 102);
        assert_eq!(
            shared1["connIndex"], 1,
            "{engine} ({label}): shared1 connIndex: {shared1:?}"
        );
        assert_eq!(
            shared2["connIndex"], 2,
            "{engine} ({label}): shared2 must reuse same SharedWorker instance: {shared2:?}"
        );
        assert_eq!(shared1["helperTag"], "relative-helper-ok");
        assert_eq!(
            shared1["workerMetaUrl"],
            server.url("/shared-echo-worker.mjs")
        );
        if engine == EngineKind::Firefox {
            assert_eq!(shared1["platform"], profile.platform);
            assert_eq!(shared1["userAgent"], profile.user_agent);
            assert_eq!(shared1["timeZone"], profile.timezone);
            assert_eq!(shared1["tzOffset"], main["tzOffset"]);
        }
    }
}

async fn assert_module_worker_immediate_postmessage(engine: EngineKind) {
    let server = TestServer::start().await;

    // 1. Root Launch with non-host Linux + DE profile
    let root_profile = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("valid linux DE profile");

    let root_session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            env_profile: Some(root_profile.clone()),
            ..Default::default()
        })
        .await
        .expect("launch root session");
    let root_pid = root_session.info().pid.expect("root pid");

    assert_module_worker_probe(&*root_session, &server, &root_profile, engine, "root").await;

    root_session.dispose().await.expect("dispose root session");
    assert_pgid_gone(root_pid as libc::pid_t);

    // 2. VirtualContext with non-host Win11 + JP profile
    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc_profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("valid win11 JP profile");

    let vc = pool
        .create_labeled_context(
            "mod-worker-vc",
            ContextOptions {
                env_profile: Some(vc_profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    assert_module_worker_probe(
        vc.session().as_ref(),
        &server,
        &vc_profile,
        engine,
        "vc-win11-jp",
    )
    .await;

    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn module_worker_immediate_postmessage_chromium() {
    assert_module_worker_immediate_postmessage(EngineKind::Chromium).await;
}

#[tokio::test]
async fn module_worker_immediate_postmessage_firefox() {
    assert_module_worker_immediate_postmessage(EngineKind::Firefox).await;
}

async fn assert_isolated_world_and_webgl_parity(engine: EngineKind) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        extra_args: if engine == EngineKind::Chromium {
            vec!["--site-per-process".to_string()]
        } else {
            Vec::new()
        },
        ..Default::default()
    })
    .await
    .expect("launch virtual pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    // 1. VirtualContext WITH PresetKind::Win11 + JP GeoIP: isolated world, main world, OOPIF timezone/locale, and gl.VENDOR/RENDERER
    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("win11 JP profile");
    let vc = pool
        .create_labeled_context(
            "iso-parity",
            ContextOptions {
                env_profile: Some(profile.clone()),
                main_world_eval: true,
                ..Default::default()
            },
        )
        .await
        .expect("create vc");
    let sess = vc.session();

    let xo_url = server2.localhost_url("/env-probe-frame");
    sess.navigate(&server.url(&format!("/mixed-xo-host?xo={xo_url}")))
        .await
        .expect("navigate");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let probe_expr = r#"(() => {
        let gl7936 = null;
        let gl7937 = null;
        try {
            const c = document.createElement('canvas');
            const gl = c.getContext('webgl2') || c.getContext('webgl') || c.getContext('experimental-webgl');
            if (gl) {
                gl7936 = gl.getParameter(7936);
                gl7937 = gl.getParameter(7937);
            }
        } catch {}
        return {
            platform: navigator.platform,
            webdriver: navigator.webdriver,
            screenW: screen.width,
            languages: Array.from(navigator.languages || []),
            timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
            tzOffset: new Date().getTimezoneOffset(),
            gl7936,
            gl7937
        };
    })()"#;

    let main_val = sess
        .eval_main_world(probe_expr)
        .await
        .expect("eval_main_world");
    let iso_val = sess.eval_isolated(probe_expr).await.expect("eval_isolated");

    assert_eq!(main_val["platform"], profile.platform);
    assert_eq!(iso_val["platform"], profile.platform);
    assert_eq!(main_val["webdriver"], false);
    assert_eq!(iso_val["webdriver"], false);
    assert_eq!(main_val["screenW"], profile.screen_width);
    assert_eq!(iso_val["screenW"], profile.screen_width);
    assert_eq!(main_val["languages"], serde_json::json!(profile.languages));
    assert_eq!(iso_val["languages"], serde_json::json!(profile.languages));
    assert_eq!(main_val["timeZone"], "Asia/Tokyo");
    assert_eq!(iso_val["timeZone"], "Asia/Tokyo");
    if let Some(ref expected_v) = profile.webgl_vendor {
        assert_eq!(main_val["gl7936"].as_str(), Some(expected_v.as_str()));
        assert_eq!(iso_val["gl7936"].as_str(), Some(expected_v.as_str()));
    }
    if let Some(ref expected_r) = profile.webgl_renderer {
        assert_eq!(main_val["gl7937"].as_str(), Some(expected_r.as_str()));
        assert_eq!(iso_val["gl7937"].as_str(), Some(expected_r.as_str()));
    }

    // Check OOPIF child frame isolated vs main world parity (including timeZone & tzOffset)
    let mut xo_frame_id = None;
    let mut last_frames = Vec::new();
    for _ in 0..300 {
        let frames = sess.list_frames().await.expect("list_frames");
        if let Some(f) = frames
            .iter()
            .find(|f| !f.is_main && f.url.contains("/env-probe-frame"))
        {
            xo_frame_id = Some(f.frame_id.clone());
            break;
        }
        last_frames = frames;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let xo_frame_id = xo_frame_id
        .unwrap_or_else(|| panic!("must find OOPIF child frame, frames={last_frames:?}"));
    let frame_probe = "({ platform: navigator.platform, webdriver: navigator.webdriver, screenW: screen.width, timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone, tzOffset: new Date().getTimezoneOffset() })";
    let f_main = sess
        .eval_main_world_in_frame(frame_probe, Some(&xo_frame_id))
        .await
        .expect("eval_main_world_in_frame");
    let f_iso = sess
        .eval_isolated_in_frame(frame_probe, Some(&xo_frame_id))
        .await
        .expect("eval_isolated_in_frame");
    assert_eq!(f_main["platform"], profile.platform);
    assert_eq!(f_iso["platform"], profile.platform);
    assert_eq!(f_main["webdriver"], false);
    assert_eq!(f_iso["webdriver"], false);
    assert_eq!(f_main["screenW"], profile.screen_width);
    assert_eq!(f_iso["screenW"], profile.screen_width);
    assert_eq!(
        f_main["timeZone"], "Asia/Tokyo",
        "{engine}: OOPIF main-world timeZone mismatch"
    );
    assert_eq!(
        f_iso["timeZone"], "Asia/Tokyo",
        "{engine}: OOPIF isolated-world timeZone mismatch"
    );
    assert_eq!(
        f_main["tzOffset"], main_val["tzOffset"],
        "{engine}: OOPIF tzOffset mismatch"
    );
    assert_eq!(
        f_iso["tzOffset"], main_val["tzOffset"],
        "{engine}: OOPIF isolated tzOffset mismatch"
    );

    // Dynamic set_env_overrides while OOPIF is attached must also update OOPIF timezone
    let de_profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("win11 DE profile");
    sess.set_env_overrides(de_profile)
        .await
        .expect("update env_overrides to DE");
    let f_tz_after = sess
        .eval_isolated_in_frame(
            "Intl.DateTimeFormat().resolvedOptions().timeZone",
            Some(&xo_frame_id),
        )
        .await
        .expect("eval OOPIF timeZone after set_env_overrides");
    assert_eq!(
        f_tz_after, "Europe/Berlin",
        "{engine}: OOPIF timeZone must update after set_env_overrides"
    );

    // 2. VirtualContext WITHOUT env_profile: Chromium must keep navigator.webdriver == false in both worlds
    if engine == EngineKind::Chromium {
        let vc_plain = pool
            .create_labeled_context(
                "plain-vc",
                ContextOptions {
                    main_world_eval: true,
                    ..Default::default()
                },
            )
            .await
            .expect("create plain vc");
        vc_plain
            .session()
            .navigate(&server.url("/isolation"))
            .await
            .expect("nav plain");
        assert_eq!(
            vc_plain
                .session()
                .eval_main_world("navigator.webdriver")
                .await
                .unwrap(),
            false
        );
        assert_eq!(
            vc_plain
                .session()
                .eval_isolated("navigator.webdriver")
                .await
                .unwrap(),
            false
        );
        vc_plain.dispose().await.expect("dispose plain vc");
    }

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn isolated_world_and_oopif_env_profile_and_webdriver_parity_chromium() {
    assert_isolated_world_and_webgl_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn isolated_world_and_oopif_env_profile_and_webdriver_parity_firefox() {
    assert_isolated_world_and_webgl_parity(EngineKind::Firefox).await;
}

async fn assert_no_auth_ipv6_ephemeral_proxy(engine: EngineKind) {
    let server = TestServer::start().await;
    let http_proxy = LocalHttpProxy::start_ipv6(None).await;
    let socks_proxy = LocalSocks5Proxy::start_ipv6(None).await;

    let http_sess = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            proxy: Some(ProxyConfig {
                server: http_proxy.proxy_url(),
                username: None,
                password: None,
                bypass: Vec::new(),
            }),
            ..Default::default()
        })
        .await
        .expect("launch EphemeralDir with unauthenticated IPv6 HTTP proxy");
    let http_pid = http_sess.info().pid.expect("pid");
    let nav_http = http_sess
        .navigate(&server.url("/echo-env"))
        .await
        .expect("navigate via IPv6 HTTP proxy in EphemeralDir");
    assert_eq!(nav_http.status, Some(200));
    assert!(
        !http_proxy.hits().is_empty(),
        "{engine}: expected IPv6 LocalHttpProxy hits in EphemeralDir"
    );
    http_sess.dispose().await.expect("dispose http_sess");
    assert_pgid_gone(http_pid as libc::pid_t);

    let socks_sess = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            proxy: Some(ProxyConfig {
                server: socks_proxy.socks5h_url(),
                username: None,
                password: None,
                bypass: Vec::new(),
            }),
            ..Default::default()
        })
        .await
        .expect("launch EphemeralDir with unauthenticated IPv6 socks5h proxy");
    let socks_pid = socks_sess.info().pid.expect("pid");
    let nav_socks = socks_sess
        .navigate(&server.url("/echo-env"))
        .await
        .expect("navigate via IPv6 socks5h proxy in EphemeralDir");
    assert_eq!(nav_socks.status, Some(200));
    assert!(
        !socks_proxy.hits().is_empty(),
        "{engine}: expected IPv6 LocalSocks5Proxy hits in EphemeralDir"
    );
    socks_sess.dispose().await.expect("dispose socks_sess");
    assert_pgid_gone(socks_pid as libc::pid_t);
}

#[tokio::test]
async fn no_auth_ipv6_ephemeral_proxy_chromium() {
    assert_no_auth_ipv6_ephemeral_proxy(EngineKind::Chromium).await;
}

#[tokio::test]
async fn no_auth_ipv6_ephemeral_proxy_firefox() {
    assert_no_auth_ipv6_ephemeral_proxy(EngineKind::Firefox).await;
}

async fn assert_worker_extra_headers_and_relative_urls_with_env_profile(engine: EngineKind) {
    let server = TestServer::start().await;

    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "wk-999")
        .build()
        .expect("valid win11 JP profile with extra header");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            env_profile: Some(profile),
            ..Default::default()
        })
        .await
        .expect("launch with env_profile + extra_header");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/worker-rel-page"))
        .await
        .expect("navigate /worker-rel-page");

    let mut out = serde_json::Value::Null;
    for _ in 0..100 {
        let val = session
            .eval_isolated(
                r#"(() => {
                    const c = document.getElementById('classic-out')?.textContent || '';
                    const m = document.getElementById('module-out')?.textContent || '';
                    const s = document.getElementById('shared-out')?.textContent || '';
                    if (!c || !m || !s) return null;
                    return { classic: JSON.parse(c), module: JSON.parse(m), shared: JSON.parse(s) };
                })()"#,
            )
            .await
            .expect("poll worker-rel outputs");
        if !val.is_null() {
            out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !out.is_null(),
        "{engine}: timed out waiting for /worker-rel-page outputs"
    );

    let classic = &out["classic"];
    assert!(
        classic["error"].is_null(),
        "{engine}: classic worker relative URL/fetch failed: {classic:?}"
    );
    assert_eq!(classic["helperLoaded"], "classic-rel-helper-ok");
    assert_eq!(classic["locHref"], server.url("/sub/worker-rel-test.js"));
    assert_eq!(classic["locPathname"], "/sub/worker-rel-test.js");
    assert_eq!(classic["relResolved"], server.url("/sub/sibling.json"));
    assert_eq!(
        classic["relFromLocObj"],
        server.url("/sub/sibling.json"),
        "{engine}: new URL('./sibling.json', self.location) mismatch: {classic:?}"
    );
    assert_eq!(
        classic["relFetch"]["x-worker-extra"], "wk-999",
        "{engine}: classic worker relative fetch() missing X-Worker-Extra: {classic:?}"
    );
    assert_eq!(
        classic["reqFetch"]["x-worker-extra"], "wk-999",
        "{engine}: classic worker fetch(new Request('../echo-env')) missing X-Worker-Extra: {classic:?}"
    );
    assert_eq!(
        classic["xhrEcho"]["x-worker-extra"], "wk-999",
        "{engine}: classic worker relative XHR missing X-Worker-Extra: {classic:?}"
    );

    let module = &out["module"];
    assert!(
        module["error"].is_null(),
        "{engine}: module worker relative URL/fetch failed: {module:?}"
    );
    assert_eq!(module["locHref"], server.url("/sub/worker-rel-test.mjs"));
    assert_eq!(module["metaUrl"], server.url("/sub/worker-rel-test.mjs"));
    assert_eq!(module["relResolved"], server.url("/sub/sibling.json"));
    assert_eq!(
        module["relFetch"]["x-worker-extra"], "wk-999",
        "{engine}: module worker relative fetch() missing X-Worker-Extra: {module:?}"
    );
    assert_eq!(
        module["reqFetch"]["x-worker-extra"], "wk-999",
        "{engine}: module worker fetch(new Request('../echo-env')) missing X-Worker-Extra: {module:?}"
    );

    let shared = &out["shared"];
    if shared["unsupported"] != true {
        assert!(
            shared["error"].is_null(),
            "{engine}: shared worker error: {shared:?}"
        );
        assert_eq!(shared["locHref"], server.url("/sub/worker-shared-rel.js"));
        assert_eq!(shared["relFromLocObj"], server.url("/sub/sibling.json"));
    }

    // Dynamic set_env_overrides while worker is running must update X-Worker-Extra
    let updated_profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "wk-updated-42")
        .build()
        .expect("valid updated profile");
    session
        .set_env_overrides(updated_profile)
        .await
        .expect("set_env_overrides on running worker");
    session
        .eval_isolated("document.getElementById('ping-worker').click()")
        .await
        .expect("click ping-worker");
    let mut dyn_out = serde_json::Value::Null;
    for _ in 0..80 {
        let val = session
            .eval_isolated(
                "(() => { const t = document.getElementById('dyn-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
            )
            .await
            .expect("poll dyn-out");
        if !val.is_null() {
            dyn_out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !dyn_out.is_null(),
        "{engine}: timed out waiting for dyn-out"
    );
    assert_eq!(
        dyn_out["dynFetch"]["x-worker-extra"], "wk-updated-42",
        "{engine}: running worker did not pick up updated X-Worker-Extra after set_env_overrides: {dyn_out:?}"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn worker_extra_headers_and_relative_urls_with_env_profile_chromium() {
    assert_worker_extra_headers_and_relative_urls_with_env_profile(EngineKind::Chromium).await;
}

#[tokio::test]
async fn worker_extra_headers_and_relative_urls_with_env_profile_firefox() {
    assert_worker_extra_headers_and_relative_urls_with_env_profile(EngineKind::Firefox).await;
}

async fn assert_nested_worker_and_srcdoc_iframe_worker(engine: EngineKind) {
    let server = TestServer::start().await;

    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "wk-nested-77")
        .build()
        .expect("valid win11 JP profile with extra header");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            env_profile: Some(profile.clone()),
            ..Default::default()
        })
        .await
        .expect("launch with env_profile + extra_header");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/nested-worker-page"))
        .await
        .expect("navigate /nested-worker-page");

    let mut out = serde_json::Value::Null;
    for _ in 0..120 {
        let val = session
            .eval_isolated(
                r#"(() => {
                    const n = document.getElementById('nested-out')?.textContent || '';
                    const s = document.getElementById('srcdoc-out')?.textContent || '';
                    if (!n || !s) return null;
                    return { nested: JSON.parse(n), srcdoc: JSON.parse(s) };
                })()"#,
            )
            .await
            .expect("poll nested-worker-page outputs");
        if !val.is_null() {
            out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !out.is_null(),
        "{engine}: timed out waiting for /nested-worker-page outputs"
    );

    let nested = &out["nested"];
    assert_eq!(nested["outerHref"], server.url("/sub/outer-worker.js"));
    assert_eq!(nested["outerHc"], profile.hardware_concurrency);

    for key in ["rel", "rootRel", "abs"] {
        let item = &nested[key];
        assert_eq!(item["ok"], true, "{engine} nested[{key}] failed: {item:?}");
        let d = &item["data"];
        assert_eq!(
            d["href"],
            server.url("/sub/inner-worker.js"),
            "{engine} nested[{key}]: {d:?}"
        );
        assert_eq!(
            d["hc"], profile.hardware_concurrency,
            "{engine} nested[{key}]: {d:?}"
        );
        assert_eq!(
            d["platform"], profile.platform,
            "{engine} nested[{key}]: {d:?}"
        );
        assert_eq!(d["ua"], profile.user_agent, "{engine} nested[{key}]: {d:?}");
        assert_eq!(d["tz"], "Asia/Tokyo", "{engine} nested[{key}]: {d:?}");
        assert_eq!(d["xExtra"], "wk-nested-77", "{engine} nested[{key}]: {d:?}");
    }

    let mod_item = &nested["mod"];
    assert_eq!(
        mod_item["ok"], true,
        "{engine} nested[mod] failed: {mod_item:?}"
    );
    let md = &mod_item["data"];
    assert_eq!(
        md["href"],
        server.url("/sub/inner-mod.mjs"),
        "{engine} nested[mod]: {md:?}"
    );
    assert_eq!(
        md["metaUrl"],
        server.url("/sub/inner-mod.mjs"),
        "{engine} nested[mod]: {md:?}"
    );
    assert_eq!(
        md["hc"], profile.hardware_concurrency,
        "{engine} nested[mod]: {md:?}"
    );
    assert_eq!(
        md["platform"], profile.platform,
        "{engine} nested[mod]: {md:?}"
    );
    assert_eq!(md["ua"], profile.user_agent, "{engine} nested[mod]: {md:?}");
    assert_eq!(md["tz"], "Asia/Tokyo", "{engine} nested[mod]: {md:?}");
    assert_eq!(md["xExtra"], "wk-nested-77", "{engine} nested[mod]: {md:?}");

    let l2 = &nested["l2"];
    assert_eq!(l2["ok"], true, "{engine} nested[l2] failed: {l2:?}");
    assert_eq!(l2["data"]["l2Href"], server.url("/sub/inner-worker-l2.js"));
    assert_eq!(l2["data"]["l2Hc"], profile.hardware_concurrency);
    let l2_inner = &l2["data"]["inner"];
    assert_eq!(
        l2_inner["href"],
        server.url("/sub/inner-worker.js"),
        "{engine} l2 inner: {l2_inner:?}"
    );
    assert_eq!(
        l2_inner["hc"], profile.hardware_concurrency,
        "{engine} l2 inner: {l2_inner:?}"
    );
    assert_eq!(
        l2_inner["platform"], profile.platform,
        "{engine} l2 inner: {l2_inner:?}"
    );
    assert_eq!(
        l2_inner["ua"], profile.user_agent,
        "{engine} l2 inner: {l2_inner:?}"
    );
    assert_eq!(
        l2_inner["tz"], "Asia/Tokyo",
        "{engine} l2 inner: {l2_inner:?}"
    );
    assert_eq!(
        l2_inner["xExtra"], "wk-nested-77",
        "{engine} l2 inner: {l2_inner:?}"
    );

    let srcdoc = &out["srcdoc"];
    assert!(
        srcdoc["error"].is_null(),
        "{engine} srcdoc worker error: {srcdoc:?}"
    );
    assert_eq!(
        srcdoc["href"],
        server.url("/sub/inner-worker.js"),
        "{engine} srcdoc: {srcdoc:?}"
    );
    assert_eq!(
        srcdoc["hc"], profile.hardware_concurrency,
        "{engine} srcdoc: {srcdoc:?}"
    );
    assert_eq!(
        srcdoc["platform"], profile.platform,
        "{engine} srcdoc: {srcdoc:?}"
    );
    assert_eq!(
        srcdoc["ua"], profile.user_agent,
        "{engine} srcdoc: {srcdoc:?}"
    );
    assert_eq!(srcdoc["tz"], "Asia/Tokyo", "{engine} srcdoc: {srcdoc:?}");
    assert_eq!(
        srcdoc["xExtra"], "wk-nested-77",
        "{engine} srcdoc: {srcdoc:?}"
    );

    // Dynamic set_env_overrides must also update running L2 nested worker
    let updated_profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "wk-nested-updated")
        .build()
        .expect("valid updated profile");
    session
        .set_env_overrides(updated_profile)
        .await
        .expect("set_env_overrides on running nested workers");
    session
        .eval_isolated("document.getElementById('ping-nested').click()")
        .await
        .expect("click ping-nested");

    let mut dyn_out = serde_json::Value::Null;
    for _ in 0..80 {
        let val = session
            .eval_isolated(
                "(() => { const t = document.getElementById('nested-dyn-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
            )
            .await
            .expect("poll nested-dyn-out");
        if !val.is_null() {
            dyn_out = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !dyn_out.is_null(),
        "{engine}: timed out waiting for nested-dyn-out"
    );
    assert_eq!(
        dyn_out["dynInner"]["xExtra"], "wk-nested-updated",
        "{engine}: nested L2 worker did not pick up updated X-Worker-Extra: {dyn_out:?}"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn nested_worker_and_srcdoc_iframe_worker_with_env_profile_chromium() {
    assert_nested_worker_and_srcdoc_iframe_worker(EngineKind::Chromium).await;
}

#[tokio::test]
async fn nested_worker_and_srcdoc_iframe_worker_with_env_profile_firefox() {
    assert_nested_worker_and_srcdoc_iframe_worker(EngineKind::Firefox).await;
}

async fn assert_window_open_popup_preserves_env_and_network(engine: EngineKind) {
    use mimikyu_core::driver::{ContextOptions, RouteAction, RouteMock};
    use mimikyu_core::isolation::VirtualContextPool;
    use std::collections::BTreeMap;

    let server = TestServer::start().await;

    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Probe-Hdr", "pop1")
        .build()
        .expect("valid win11 JP profile with extra header");

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc = pool
        .create_labeled_context(
            "popup-env-vc",
            ContextOptions {
                env_profile: Some(profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    vc.session()
        .set_blocked_urls(vec!["*blocked-pop*".to_string()])
        .await
        .expect("set_blocked_urls");

    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    mock_hdrs.insert("access-control-allow-origin".to_string(), "*".to_string());
    vc.session()
        .set_route_mock(RouteMock {
            id: "popup-mock-1".to_string(),
            url_pattern: "*mock-pop-pre*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs.clone(),
                body: br#"{"mocked":true,"phase":"pre"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    vc.session()
        .navigate(&server.url("/popup-probe-host"))
        .await
        .expect("navigate /popup-probe-host");

    // 1. Synchronous window.open('about:blank')
    vc.session()
        .eval_isolated("document.getElementById('open-blank-popup').click()")
        .await
        .expect("click open-blank-popup");
    let blank_out = vc
        .session()
        .eval_isolated("JSON.parse(document.getElementById('blank-popup-out').textContent)")
        .await
        .expect("read blank-popup-out");
    assert_eq!(
        blank_out["platform"], profile.platform,
        "{engine}: window.open('about:blank') platform mismatch: {blank_out:?}"
    );
    assert_eq!(
        blank_out["hc"], profile.hardware_concurrency,
        "{engine}: window.open('about:blank') hc mismatch: {blank_out:?}"
    );
    assert_eq!(
        blank_out["ua"], profile.user_agent,
        "{engine}: window.open('about:blank') ua mismatch: {blank_out:?}"
    );
    assert_eq!(
        blank_out["tz"], "Asia/Tokyo",
        "{engine}: window.open('about:blank') tz mismatch: {blank_out:?}"
    );

    // 2. window.open('/popup-probe-child')
    server.reset_hits();
    vc.session()
        .eval_isolated("document.getElementById('open-popup').click()")
        .await
        .expect("click open-popup");

    let child_out = poll_non_null(
        vc.session().as_ref(),
        "(() => { const t = document.getElementById('popup-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
        "poll popup-out",
    )
    .await;
    assert!(
        !child_out.is_null(),
        "{engine}: timed out waiting for popup-out"
    );
    assert_eq!(
        child_out["syncNav"]["platform"], profile.platform,
        "{engine}: popup platform mismatch: {child_out:?}"
    );
    assert_eq!(
        child_out["syncNav"]["hc"], profile.hardware_concurrency,
        "{engine}: popup hc mismatch: {child_out:?}"
    );
    assert_eq!(
        child_out["syncNav"]["ua"], profile.user_agent,
        "{engine}: popup ua mismatch: {child_out:?}"
    );
    assert_eq!(
        child_out["syncNav"]["tz"], "Asia/Tokyo",
        "{engine}: popup tz mismatch: {child_out:?}"
    );
    assert_eq!(
        child_out["xHdr"], "pop1",
        "{engine}: popup extra header missing: {child_out:?}"
    );
    assert_eq!(
        child_out["reqUa"], profile.user_agent,
        "{engine}: popup HTTP User-Agent mismatch: {child_out:?}"
    );
    assert_eq!(
        child_out["mocked"]["mocked"], true,
        "{engine}: popup RouteMock missed: {child_out:?}"
    );
    assert_eq!(
        child_out["blockedOk"], false,
        "{engine}: popup blocked URL succeeded: {child_out:?}"
    );
    let hits = server.hits();
    assert!(
        !hits.iter().any(|h| h.contains("/blocked-pop.js")),
        "{engine}: popup /blocked-pop.js hit server: {hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.contains("/api/mock-pop-pre")),
        "{engine}: popup /api/mock-pop-pre hit server: {hits:?}"
    );

    // 3. Dynamic update while popup is open (extra_headers + RouteMock)
    let updated_profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Probe-Hdr", "pop2-dyn")
        .build()
        .expect("valid updated profile");
    vc.session()
        .set_env_overrides(updated_profile)
        .await
        .expect("dynamic set_env_overrides");
    vc.session()
        .set_route_mock(RouteMock {
            id: "popup-mock-dyn".to_string(),
            url_pattern: "*mock-pop-dyn*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs,
                body: br#"{"mocked":true,"phase":"dyn"}"#.to_vec(),
            },
        })
        .await
        .expect("dynamic set_route_mock");
    vc.session()
        .eval_isolated("document.getElementById('ping-popup').click()")
        .await
        .expect("click ping-popup");
    let dyn_out = poll_non_null(
        vc.session().as_ref(),
        "(() => { const t = document.getElementById('popup-dyn-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
        "poll popup-dyn-out",
    )
    .await;
    assert!(
        !dyn_out.is_null(),
        "{engine}: timed out waiting for popup-dyn-out"
    );
    assert_eq!(
        dyn_out["xHdr"], "pop2-dyn",
        "{engine}: popup did not receive dynamic X-Probe-Hdr: {dyn_out:?}"
    );
    assert_eq!(
        dyn_out["dynMock"]["phase"], "dyn",
        "{engine}: popup did not receive dynamic RouteMock: {dyn_out:?}"
    );

    // 4. Nested window.open() from inside the popup
    vc.session()
        .eval_isolated("document.getElementById('trigger-grandchild').click()")
        .await
        .expect("click trigger-grandchild");
    let gc_out = poll_non_null(
        vc.session().as_ref(),
        "(() => { const t = document.getElementById('grandchild-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
        "poll grandchild-out",
    )
    .await;
    assert!(
        !gc_out.is_null(),
        "{engine}: timed out waiting for grandchild-out"
    );
    assert_eq!(
        gc_out["platform"], profile.platform,
        "{engine}: grandchild popup platform mismatch: {gc_out:?}"
    );
    assert_eq!(
        gc_out["hc"], profile.hardware_concurrency,
        "{engine}: grandchild popup hc mismatch: {gc_out:?}"
    );
    assert_eq!(
        gc_out["ua"], profile.user_agent,
        "{engine}: grandchild popup ua mismatch: {gc_out:?}"
    );
    assert_eq!(
        gc_out["tz"], "Asia/Tokyo",
        "{engine}: grandchild popup tz mismatch: {gc_out:?}"
    );
    assert_eq!(
        gc_out["xHdr"], "pop2-dyn",
        "{engine}: grandchild popup xHdr mismatch: {gc_out:?}"
    );
    assert_eq!(
        gc_out["mocked"]["mocked"], true,
        "{engine}: grandchild popup RouteMock missed: {gc_out:?}"
    );

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn window_open_popup_preserves_env_and_network_chromium() {
    assert_window_open_popup_preserves_env_and_network(EngineKind::Chromium).await;
}

#[tokio::test]
async fn window_open_popup_preserves_env_and_network_firefox() {
    assert_window_open_popup_preserves_env_and_network(EngineKind::Firefox).await;
}

async fn assert_sync_about_blank_iframe_env_and_worker_parity(engine: EngineKind) {
    use mimikyu_core::driver::ContextOptions;
    use mimikyu_core::isolation::VirtualContextPool;

    let server = TestServer::start().await;
    let server2 = TestServer::start().await;

    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "sync-iframe-1")
        .build()
        .expect("valid win11 JP profile with extra header");

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc = pool
        .create_labeled_context(
            "sync-blank-iframe-vc",
            ContextOptions {
                env_profile: Some(profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    let xo_url = server2.localhost_url("/target");
    vc.session()
        .navigate(&server.url(&format!("/sync-blank-iframe-host?xo={xo_url}")))
        .await
        .expect("navigate /sync-blank-iframe-host");

    let iframe_out = vc
        .session()
        .eval_isolated("JSON.parse(document.getElementById('sync-iframe-out').textContent)")
        .await
        .expect("read sync-iframe-out");
    assert_eq!(iframe_out["xoAccessible"], true);

    for (label, node) in [
        ("about:blank", &iframe_out["syncNav"]),
        ("nested about:blank", &iframe_out["nestedNav"]),
    ] {
        assert_eq!(
            node["platform"], profile.platform,
            "{engine} ({label}): navigator.platform leaked: {node:?}"
        );
        assert_eq!(
            node["hc"], profile.hardware_concurrency,
            "{engine} ({label}): navigator.hardwareConcurrency leaked: {node:?}"
        );
        assert_eq!(
            node["ua"], profile.user_agent,
            "{engine} ({label}): navigator.userAgent leaked: {node:?}"
        );
        assert_eq!(
            node["tz"], "Asia/Tokyo",
            "{engine} ({label}): timezone leaked: {node:?}"
        );
    }

    let worker_out = poll_non_null(
        vc.session().as_ref(),
        r#"(() => {
            const a = document.getElementById('sync-worker-out')?.textContent || '';
            const b = document.getElementById('nested-worker-out')?.textContent || '';
            if (!a || !b) return null;
            return { a: JSON.parse(a), b: JSON.parse(b) };
        })()"#,
        "poll sync-worker-out",
    )
    .await;
    assert!(
        !worker_out.is_null(),
        "{engine}: timed out waiting for sync-worker-out"
    );

    for (label, w) in [
        ("about:blank", &worker_out["a"]),
        ("nested about:blank", &worker_out["b"]),
    ] {
        assert!(
            w["error"].is_null(),
            "{engine} ({label}) worker error: {w:?}"
        );
        assert_eq!(
            w["platform"], profile.platform,
            "{engine} ({label}) worker platform leaked: {w:?}"
        );
        assert_eq!(
            w["hc"], profile.hardware_concurrency,
            "{engine} ({label}) worker hc leaked: {w:?}"
        );
        assert_eq!(
            w["ua"], profile.user_agent,
            "{engine} ({label}) worker ua leaked: {w:?}"
        );
        assert_eq!(
            w["tz"], "Asia/Tokyo",
            "{engine} ({label}) worker tz leaked: {w:?}"
        );
        assert_eq!(
            w["xExtra"], "sync-iframe-1",
            "{engine} ({label}) worker extra header missing: {w:?}"
        );
    }

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn sync_about_blank_iframe_env_and_worker_parity_chromium() {
    assert_sync_about_blank_iframe_env_and_worker_parity(EngineKind::Chromium).await;
}

#[tokio::test]
async fn sync_about_blank_iframe_env_and_worker_parity_firefox() {
    assert_sync_about_blank_iframe_env_and_worker_parity(EngineKind::Firefox).await;
}

async fn click_element_real_mouse(session: &dyn DriverSession, selector: &str) {
    use mimikyu_core::driver::{MouseButton, MouseEvent, MouseEventType};

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

async fn assert_target_blank_links_preserve_env_and_network_virtual(engine: EngineKind) {
    use mimikyu_core::driver::{RouteAction, RouteMock};
    use std::collections::BTreeMap;

    let server = TestServer::start().await;
    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Probe-Hdr", "tb-vc")
        .build()
        .expect("valid profile");

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc = pool
        .create_labeled_context(
            "target-blank-vc",
            ContextOptions {
                env_profile: Some(profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    vc.session()
        .set_blocked_urls(vec!["*blocked-pop*".to_string()])
        .await
        .expect("set_blocked_urls");
    let mut mock_hdrs = BTreeMap::new();
    mock_hdrs.insert("content-type".to_string(), "application/json".to_string());
    mock_hdrs.insert("access-control-allow-origin".to_string(), "*".to_string());
    vc.session()
        .set_route_mock(RouteMock {
            id: "tb-vc-mock".to_string(),
            url_pattern: "*mock-pop-pre*".to_string(),
            action: RouteAction::Fulfill {
                status: 200,
                headers: mock_hdrs,
                body: br#"{"mocked":true,"phase":"tb-vc"}"#.to_vec(),
            },
        })
        .await
        .expect("set_route_mock");

    let host_tid = vc.session().info().target_id.clone();
    vc.session()
        .navigate(&server.url("/target-blank-host"))
        .await
        .expect("navigate /target-blank-host");

    click_element_real_mouse(vc.session().as_ref(), "#lnk-blank").await;

    let blank_out = poll_non_null(
        vc.session().as_ref(),
        r#"(() => {
            const a = document.getElementById('tb-blank')?.textContent || localStorage.getItem('tb_blank') || '';
            return a ? JSON.parse(a) : null;
        })()"#,
        "poll tb-blank",
    )
    .await;
    assert!(
        !blank_out.is_null(),
        "{engine}: timed out waiting for <a target=_blank>"
    );

    // Bring the opener tab back to the foreground and settle a layout frame before clicking `#lnk-noopener`.
    vc.session()
        .activate_tab(&host_tid)
        .await
        .unwrap_or_else(|e| panic!("{engine}: activate_tab(host_tid) before noopener failed: {e}"));
    let _ = vc
        .session()
        .eval_isolated(
            "new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))",
        )
        .await;

    click_element_real_mouse(vc.session().as_ref(), "#lnk-noopener").await;

    let noopener_out = poll_non_null(
        vc.session().as_ref(),
        r#"(() => {
            const b = document.getElementById('tb-noopener')?.textContent || localStorage.getItem('tb_noopener') || '';
            return b ? JSON.parse(b) : null;
        })()"#,
        "poll tb-noopener",
    )
    .await;
    if noopener_out.is_null() {
        let tabs = vc.session().list_tabs().await.unwrap_or_default();
        let hits = server.hits();
        panic!(
            "{engine}: timed out waiting for <a target=_blank rel=noopener>; tabs={tabs:?}, hits={hits:?}"
        );
    }
    let out = serde_json::json!({ "blank": blank_out, "noopener": noopener_out });

    let tabs = vc.session().list_tabs().await.expect("list_tabs");
    assert!(
        tabs.iter()
            .any(|t| t.url.contains("/target-blank-child?tag=blank")),
        "{engine}: missing navigated target=_blank tab in {tabs:?}"
    );
    assert!(
        tabs.iter()
            .any(|t| t.url.contains("/target-blank-child?tag=noopener")),
        "{engine}: missing navigated rel=noopener tab in {tabs:?}"
    );
    assert!(
        !tabs.iter().any(|t| t.url.is_empty()),
        "{engine}: popup tab frozen at empty url: {tabs:?}"
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
        assert_eq!(node["xHdr"], "tb-vc", "{engine} {tag}: {node:?}");
        assert_eq!(
            node["reqUa"], profile.user_agent,
            "{engine} {tag}: {node:?}"
        );
        assert_eq!(node["mocked"]["phase"], "tb-vc", "{engine} {tag}: {node:?}");
        assert_eq!(node["blockedOk"], false, "{engine} {tag}: {node:?}");
    }
    assert_eq!(out["noopener"]["syncNav"]["hasOpener"], false);

    let req_hdrs = server.request_headers();
    for path_frag in [
        "GET /target-blank-child?tag=blank",
        "GET /target-blank-child?tag=noopener",
    ] {
        let (_, hdrs) = req_hdrs
            .iter()
            .find(|(req, _)| req == path_frag)
            .unwrap_or_else(|| panic!("{engine}: missing initial server request {path_frag}"));
        assert_eq!(
            hdrs.get("x-probe-hdr").map(String::as_str),
            Some("tb-vc"),
            "{engine}: {path_frag} missing X-Probe-Hdr: {hdrs:?}"
        );
        assert_eq!(
            hdrs.get("user-agent").map(String::as_str),
            Some(profile.user_agent.as_str()),
            "{engine}: {path_frag} User-Agent mismatch: {hdrs:?}"
        );
    }

    let hits = server.hits();
    assert!(!hits.iter().any(|h| h.contains("/blocked-pop.js")));
    assert!(!hits.iter().any(|h| h.contains("/api/mock-pop-pre")));

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn target_blank_links_preserve_env_and_network_virtual_chromium() {
    assert_target_blank_links_preserve_env_and_network_virtual(EngineKind::Chromium).await;
}

#[tokio::test]
async fn target_blank_links_preserve_env_and_network_virtual_firefox() {
    assert_target_blank_links_preserve_env_and_network_virtual(EngineKind::Firefox).await;
}

async fn assert_frames_index_and_named_sync_about_blank_iframe_worker(engine: EngineKind) {
    let server = TestServer::start().await;
    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "frames-idx-1")
        .build()
        .expect("valid profile");

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc = pool
        .create_labeled_context(
            "frames-index-vc",
            ContextOptions {
                env_profile: Some(profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    vc.session()
        .navigate(&server.url("/frames-index-host"))
        .await
        .expect("navigate /frames-index-host");

    let out = vc
        .session()
        .eval_isolated("JSON.parse(document.getElementById('frames-index-out').textContent)")
        .await
        .expect("read frames-index-out");

    for label in ["wf", "wi", "wn", "adj", "nestedDiv"] {
        let node = &out[label];
        assert_eq!(
            node["platform"], profile.platform,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["hc"], profile.hardware_concurrency,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["ua"], profile.user_agent,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(node["tz"], "Asia/Tokyo", "{engine} ({label}): {node:?}");
    }

    let worker_out = poll_non_null(
        vc.session().as_ref(),
        "(() => { const t = document.getElementById('frames-worker-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
        "poll frames-worker-out",
    )
    .await;
    assert!(
        !worker_out.is_null(),
        "{engine}: timed out waiting for frames-worker-out"
    );
    assert!(
        worker_out["error"].is_null(),
        "{engine}: worker error: {worker_out:?}"
    );
    assert_eq!(
        worker_out["platform"], profile.platform,
        "{engine}: {worker_out:?}"
    );
    assert_eq!(
        worker_out["hc"], profile.hardware_concurrency,
        "{engine}: {worker_out:?}"
    );
    assert_eq!(
        worker_out["ua"], profile.user_agent,
        "{engine}: {worker_out:?}"
    );
    assert_eq!(worker_out["tz"], "Asia/Tokyo", "{engine}: {worker_out:?}");
    assert_eq!(
        worker_out["xExtra"], "frames-idx-1",
        "{engine}: {worker_out:?}"
    );

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn frames_index_and_named_sync_about_blank_iframe_worker_chromium() {
    assert_frames_index_and_named_sync_about_blank_iframe_worker(EngineKind::Chromium).await;
}

#[tokio::test]
async fn frames_index_and_named_sync_about_blank_iframe_worker_firefox() {
    assert_frames_index_and_named_sync_about_blank_iframe_worker(EngineKind::Firefox).await;
}

async fn assert_dynamic_set_env_overrides_updates_loaded_page_and_iframes(engine: EngineKind) {
    let server = TestServer::start().await;
    let win_jp = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "dyn-phase-1")
        .build()
        .expect("valid win11 JP profile");
    let linux_de = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .extra_header("X-Worker-Extra", "dyn-phase-2")
        .build()
        .expect("valid linux DE profile");

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc = pool
        .create_labeled_context(
            "dyn-override-vc",
            ContextOptions {
                env_profile: Some(win_jp.clone()),
                main_world_eval: true,
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    vc.session()
        .navigate(&server.url("/blank"))
        .await
        .expect("navigate /blank");

    let snap_expr = r#"(() => ({
        platform: navigator.platform,
        hc: navigator.hardwareConcurrency,
        ua: navigator.userAgent,
        tz: Intl.DateTimeFormat().resolvedOptions().timeZone
    }))()"#;

    let main_before = vc
        .session()
        .eval_main_world(snap_expr)
        .await
        .expect("main before");
    let iso_before = vc
        .session()
        .eval_isolated(snap_expr)
        .await
        .expect("iso before");
    assert_eq!(main_before["platform"], win_jp.platform);
    assert_eq!(iso_before["platform"], win_jp.platform);
    assert_eq!(main_before["tz"], "Asia/Tokyo");
    assert_eq!(iso_before["tz"], "Asia/Tokyo");

    // Append an about:blank iframe BEFORE set_env_overrides
    let pre_frame = vc
        .session()
        .eval_main_world(
            r#"(() => {
                const f = document.createElement('iframe');
                f.id = 'pre-frame';
                document.body.appendChild(f);
                const w = f.contentWindow;
                return {
                    platform: w.navigator.platform,
                    ua: w.navigator.userAgent,
                    tz: new w.Intl.DateTimeFormat().resolvedOptions().timeZone
                };
            })()"#,
        )
        .await
        .expect("pre_frame");
    assert_eq!(pre_frame["platform"], win_jp.platform);
    assert_eq!(pre_frame["tz"], "Asia/Tokyo");

    // Dynamically switch Win11/JP -> Linux/DE on the already-loaded document
    vc.session()
        .set_env_overrides(linux_de.clone())
        .await
        .expect("set_env_overrides linux_de");

    let main_after = vc
        .session()
        .eval_main_world(snap_expr)
        .await
        .expect("main after");
    let iso_after = vc
        .session()
        .eval_isolated(snap_expr)
        .await
        .expect("iso after");
    for (world, val) in [("main", &main_after), ("isolated", &iso_after)] {
        assert_eq!(
            val["platform"], linux_de.platform,
            "{engine} ({world}): {val:?}"
        );
        assert_eq!(
            val["hc"], linux_de.hardware_concurrency,
            "{engine} ({world}): {val:?}"
        );
        assert_eq!(
            val["ua"], linux_de.user_agent,
            "{engine} ({world}): {val:?}"
        );
        assert_eq!(val["tz"], "Europe/Berlin", "{engine} ({world}): {val:?}");
    }

    // Existing pre-override iframe + newly appended about:blank iframes (via frames[i] and contentWindow)
    let iframes_after = vc
        .session()
        .eval_main_world(
            r#"(() => {
                const snap = (w) => ({
                    platform: w.navigator.platform,
                    hc: w.navigator.hardwareConcurrency,
                    ua: w.navigator.userAgent,
                    tz: new w.Intl.DateTimeFormat().resolvedOptions().timeZone
                });
                const pre = document.getElementById('pre-frame').contentWindow;
                const fIdx = document.createElement('iframe');
                document.body.appendChild(fIdx);
                const viaFrames = snap(window.frames[window.frames.length - 1]);
                const fCw = document.createElement('iframe');
                document.body.appendChild(fCw);
                const viaCw = snap(fCw.contentWindow);
                window.__dynFrameWin = fCw.contentWindow;
                return { pre: snap(pre), viaFrames, viaCw };
            })()"#,
        )
        .await
        .expect("iframes_after");

    for (label, node) in [
        ("pre_frame", &iframes_after["pre"]),
        ("via_frames_index", &iframes_after["viaFrames"]),
        ("via_contentWindow", &iframes_after["viaCw"]),
    ] {
        assert_eq!(
            node["platform"], linux_de.platform,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["hc"], linux_de.hardware_concurrency,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["ua"], linux_de.user_agent,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(node["tz"], "Europe/Berlin", "{engine} ({label}): {node:?}");
    }

    // Worker spawned from newly appended about:blank iframe after dynamic override
    let wrk_after = vc
        .session()
        .eval_main_world(
            r#"new Promise((resolve, reject) => {
                const w = new window.__dynFrameWin.Worker('/sub/inner-worker.js');
                w.onmessage = (ev) => resolve(ev.data);
                w.onerror = (err) => reject(new Error(String(err.message || err)));
            })"#,
        )
        .await
        .expect("worker after dynamic override");
    assert_eq!(
        wrk_after["platform"], linux_de.platform,
        "{engine}: {wrk_after:?}"
    );
    assert_eq!(
        wrk_after["hc"], linux_de.hardware_concurrency,
        "{engine}: {wrk_after:?}"
    );
    assert_eq!(
        wrk_after["ua"], linux_de.user_agent,
        "{engine}: {wrk_after:?}"
    );
    assert_eq!(wrk_after["tz"], "Europe/Berlin", "{engine}: {wrk_after:?}");
    assert_eq!(
        wrk_after["xExtra"], "dyn-phase-2",
        "{engine}: {wrk_after:?}"
    );

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_loaded_page_and_iframes_chromium() {
    assert_dynamic_set_env_overrides_updates_loaded_page_and_iframes(EngineKind::Chromium).await;
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_loaded_page_and_iframes_firefox() {
    assert_dynamic_set_env_overrides_updates_loaded_page_and_iframes(EngineKind::Firefox).await;
}

async fn assert_static_iframe_and_dom_insertion_apis(engine: EngineKind) {
    let server = TestServer::start().await;
    let profile = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .extra_header("X-Worker-Extra", "static-dom-1")
        .build()
        .expect("valid profile");

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pool_pid = pool.root_session().info().pid.expect("pool pid");

    let vc = pool
        .create_labeled_context(
            "static-dom-vc",
            ContextOptions {
                env_profile: Some(profile.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create vc");

    vc.session()
        .navigate(&server.url("/static-and-dom-frames-host"))
        .await
        .expect("navigate /static-and-dom-frames-host");

    let static_out = vc
        .session()
        .eval_isolated("JSON.parse(document.getElementById('static-frames-out').textContent)")
        .await
        .expect("read static-frames-out");

    for label in ["wf", "wi", "wn"] {
        let node = &static_out[label];
        assert_eq!(
            node["platform"], profile.platform,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["hc"], profile.hardware_concurrency,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["ua"], profile.user_agent,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(node["tz"], "Asia/Tokyo", "{engine} ({label}): {node:?}");
    }

    let worker_out = poll_non_null(
        vc.session().as_ref(),
        "(() => { const t = document.getElementById('static-worker-out')?.textContent || ''; return t ? JSON.parse(t) : null; })()",
        "poll static-worker-out",
    )
    .await;
    assert!(
        !worker_out.is_null(),
        "{engine}: timed out waiting for static-worker-out"
    );
    assert!(
        worker_out["error"].is_null(),
        "{engine}: worker error: {worker_out:?}"
    );
    assert_eq!(
        worker_out["platform"], profile.platform,
        "{engine}: {worker_out:?}"
    );
    assert_eq!(
        worker_out["hc"], profile.hardware_concurrency,
        "{engine}: {worker_out:?}"
    );
    assert_eq!(
        worker_out["ua"], profile.user_agent,
        "{engine}: {worker_out:?}"
    );
    assert_eq!(worker_out["tz"], "Asia/Tokyo", "{engine}: {worker_out:?}");
    assert_eq!(
        worker_out["xExtra"], "static-dom-1",
        "{engine}: {worker_out:?}"
    );

    let dom_out = vc
        .session()
        .eval_isolated("JSON.parse(document.getElementById('dom-frames-out').textContent)")
        .await
        .expect("read dom-frames-out");

    for label in [
        "rangeSnap",
        "repSnap",
        "srSnap",
        "unsafeSnap",
        "srUnsafeSnap",
    ] {
        let node = &dom_out[label];
        assert_eq!(
            node["platform"], profile.platform,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["hc"], profile.hardware_concurrency,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(
            node["ua"], profile.user_agent,
            "{engine} ({label}): {node:?}"
        );
        assert_eq!(node["tz"], "Asia/Tokyo", "{engine} ({label}): {node:?}");
    }

    vc.dispose().await.expect("dispose vc");
    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pool_pid as libc::pid_t);
}

#[tokio::test]
async fn static_iframe_and_dom_insertion_apis_chromium() {
    assert_static_iframe_and_dom_insertion_apis(EngineKind::Chromium).await;
}

#[tokio::test]
async fn static_iframe_and_dom_insertion_apis_firefox() {
    assert_static_iframe_and_dom_insertion_apis(EngineKind::Firefox).await;
}

async fn assert_dynamic_set_env_overrides_updates_cross_origin_iframe_default_launch(
    engine: EngineKind,
) {
    let server = TestServer::start().await;
    let server2 = TestServer::start().await;
    let win_jp = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("valid win11 JP");
    let linux_de = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("valid linux DE");

    // Intentionally use default LaunchOptions (no `--site-per-process` on Chromium)
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            env_profile: Some(win_jp.clone()),
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch session");
    let pid = session.info().pid.expect("pid");

    let xo_url = server2.localhost_url("/xo-child");
    session
        .navigate(&server.url(&format!("/mixed-xo-host?xo={xo_url}")))
        .await
        .expect("navigate /mixed-xo-host");

    let mut xo_fid = None;
    for _ in 0..300 {
        let frames = session.list_frames().await.expect("list_frames");
        if let Some(f) = frames
            .iter()
            .find(|f| !f.is_main && f.url.starts_with(&xo_url))
        {
            xo_fid = Some(f.frame_id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let xo_fid = xo_fid.expect("xo-child frame present");

    let snap_expr = r#"(() => ({
        platform: navigator.platform,
        langs: Array.from(navigator.languages || []),
        ua: navigator.userAgent,
        tz: Intl.DateTimeFormat().resolvedOptions().timeZone,
        uadPlat: navigator.userAgentData ? navigator.userAgentData.platform : null
    }))()"#;

    let main_before = session
        .eval_main_world_in_frame(snap_expr, Some(&xo_fid))
        .await
        .expect("xo main before");
    let iso_before = session
        .eval_isolated_in_frame(snap_expr, Some(&xo_fid))
        .await
        .expect("xo iso before");
    for (world, val) in [("main", &main_before), ("isolated", &iso_before)] {
        assert_eq!(
            val["platform"], win_jp.platform,
            "{engine} before ({world}): {val:?}"
        );
        assert_eq!(
            val["langs"][0], "ja-JP",
            "{engine} before ({world}): {val:?}"
        );
        assert_eq!(
            val["ua"], win_jp.user_agent,
            "{engine} before ({world}): {val:?}"
        );
        assert_eq!(
            val["tz"], "Asia/Tokyo",
            "{engine} before ({world}): {val:?}"
        );
        if engine == EngineKind::Chromium {
            assert_eq!(
                val["uadPlat"], "Windows",
                "{engine} before ({world}): {val:?}"
            );
        }
    }

    session
        .set_env_overrides(linux_de.clone())
        .await
        .expect("set_env_overrides linux_de");

    let main_after = session
        .eval_main_world_in_frame(snap_expr, Some(&xo_fid))
        .await
        .expect("xo main after");
    let iso_after = session
        .eval_isolated_in_frame(snap_expr, Some(&xo_fid))
        .await
        .expect("xo iso after");
    for (world, val) in [("main", &main_after), ("isolated", &iso_after)] {
        assert_eq!(
            val["platform"], linux_de.platform,
            "{engine} after ({world}): {val:?}"
        );
        assert_eq!(
            val["langs"][0], "de-DE",
            "{engine} after ({world}): {val:?}"
        );
        assert_eq!(
            val["ua"], linux_de.user_agent,
            "{engine} after ({world}): {val:?}"
        );
        assert_eq!(
            val["tz"], "Europe/Berlin",
            "{engine} after ({world}): {val:?}"
        );
        if engine == EngineKind::Chromium {
            assert_eq!(val["uadPlat"], "Linux", "{engine} after ({world}): {val:?}");
        }
    }

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_cross_origin_iframe_default_launch_chromium() {
    assert_dynamic_set_env_overrides_updates_cross_origin_iframe_default_launch(
        EngineKind::Chromium,
    )
    .await;
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_cross_origin_iframe_default_launch_firefox() {
    assert_dynamic_set_env_overrides_updates_cross_origin_iframe_default_launch(
        EngineKind::Firefox,
    )
    .await;
}

async fn assert_dynamic_set_env_overrides_updates_nested_cross_origin_in_oopif(engine: EngineKind) {
    let server1 = TestServer::start().await;
    let server2 = TestServer::start().await;
    let server3 = TestServer::start().await;
    let win_jp = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("valid win11 JP");
    let linux_de = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("valid linux DE");

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
            env_profile: Some(win_jp.clone()),
            main_world_eval: true,
            extra_args,
            ..Default::default()
        })
        .await
        .expect("launch session");
    let pid = session.info().pid.expect("pid");

    // 127.0.0.1:P1 -> localhost:P2 (OOPIF under --site-per-process)
    //              -> localhost:P3 (same-site cross-origin, shares OOPIF process)
    let nested_url = server3.localhost_url("/xo-child");
    let mid_url = server2.localhost_url(&format!("/nested-so-xo-middle?xo={nested_url}"));
    session
        .navigate(&server1.url(&format!("/mixed-xo-host?xo={mid_url}")))
        .await
        .expect("navigate /mixed-xo-host -> /nested-so-xo-middle -> /xo-child");

    let mut mid_fid = None;
    let mut nested_fid = None;
    for _ in 0..300 {
        let frames = session.list_frames().await.expect("list_frames");
        if mid_fid.is_none() {
            mid_fid = frames
                .iter()
                .find(|f| !f.is_main && f.url.starts_with(&mid_url))
                .map(|f| f.frame_id.clone());
        }
        if nested_fid.is_none() {
            nested_fid = frames
                .iter()
                .find(|f| !f.is_main && f.url.starts_with(&nested_url))
                .map(|f| f.frame_id.clone());
        }
        if mid_fid.is_some() && nested_fid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mid_fid = mid_fid.expect("OOPIF root frame present");
    let nested_fid = nested_fid.expect("nested cross-origin frame inside OOPIF present");

    let snap_expr = r#"(() => ({
        platform: navigator.platform,
        langs: Array.from(navigator.languages || []),
        ua: navigator.userAgent,
        tz: Intl.DateTimeFormat().resolvedOptions().timeZone,
        uadPlat: navigator.userAgentData ? navigator.userAgentData.platform : null
    }))()"#;

    for (label, fid) in [
        ("oopif-root", &mid_fid),
        ("nested-xo-in-oopif", &nested_fid),
    ] {
        let main_before = session
            .eval_main_world_in_frame(snap_expr, Some(fid))
            .await
            .expect("before main");
        let iso_before = session
            .eval_isolated_in_frame(snap_expr, Some(fid))
            .await
            .expect("before iso");
        for (world, val) in [("main", &main_before), ("isolated", &iso_before)] {
            assert_eq!(
                val["platform"], win_jp.platform,
                "{engine} {label} before ({world}): {val:?}"
            );
            assert_eq!(
                val["langs"][0], "ja-JP",
                "{engine} {label} before ({world}): {val:?}"
            );
            assert_eq!(
                val["ua"], win_jp.user_agent,
                "{engine} {label} before ({world}): {val:?}"
            );
            assert_eq!(
                val["tz"], "Asia/Tokyo",
                "{engine} {label} before ({world}): {val:?}"
            );
            if engine == EngineKind::Chromium {
                assert_eq!(
                    val["uadPlat"], "Windows",
                    "{engine} {label} before ({world}): {val:?}"
                );
            }
        }
    }

    session
        .set_env_overrides(linux_de.clone())
        .await
        .expect("set_env_overrides linux_de");

    for (label, fid) in [
        ("oopif-root", &mid_fid),
        ("nested-xo-in-oopif", &nested_fid),
    ] {
        let main_after = session
            .eval_main_world_in_frame(snap_expr, Some(fid))
            .await
            .expect("after main");
        let iso_after = session
            .eval_isolated_in_frame(snap_expr, Some(fid))
            .await
            .expect("after iso");
        for (world, val) in [("main", &main_after), ("isolated", &iso_after)] {
            assert_eq!(
                val["platform"], linux_de.platform,
                "{engine} {label} after ({world}): {val:?}"
            );
            assert_eq!(
                val["langs"][0], "de-DE",
                "{engine} {label} after ({world}): {val:?}"
            );
            assert_eq!(
                val["ua"], linux_de.user_agent,
                "{engine} {label} after ({world}): {val:?}"
            );
            assert_eq!(
                val["tz"], "Europe/Berlin",
                "{engine} {label} after ({world}): {val:?}"
            );
            if engine == EngineKind::Chromium {
                assert_eq!(
                    val["uadPlat"], "Linux",
                    "{engine} {label} after ({world}): {val:?}"
                );
            }
        }
    }

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_nested_cross_origin_in_oopif_chromium() {
    assert_dynamic_set_env_overrides_updates_nested_cross_origin_in_oopif(EngineKind::Chromium)
        .await;
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_nested_cross_origin_in_oopif_firefox() {
    assert_dynamic_set_env_overrides_updates_nested_cross_origin_in_oopif(EngineKind::Firefox)
        .await;
}

async fn assert_dynamic_set_env_overrides_updates_popup_cross_origin_iframe(
    engine: EngineKind,
    use_virtual: bool,
) {
    let server1 = TestServer::start().await;
    let server2 = TestServer::start().await;
    let win_jp = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("valid win11 JP");
    let linux_de = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("valid linux DE");

    let mut vc_opt = None;
    let mut pool_opt = None;
    let session = if use_virtual {
        let pool = VirtualContextPool::launch(LaunchOptions {
            engine,
            headless: true,
            ..Default::default()
        })
        .await
        .expect("launch pool");
        let vc = pool
            .create_labeled_context(
                "popup-xo-vc",
                ContextOptions {
                    env_profile: Some(win_jp.clone()),
                    main_world_eval: true,
                    ..Default::default()
                },
            )
            .await
            .expect("create virtual context");
        let sess = vc.session().clone();
        vc_opt = Some(vc);
        pool_opt = Some(pool);
        sess
    } else {
        driver_for(engine)
            .launch(LaunchOptions {
                engine,
                headless: true,
                profile_mode: ProfileMode::EphemeralDir,
                env_profile: Some(win_jp.clone()),
                main_world_eval: true,
                ..Default::default()
            })
            .await
            .expect("launch ephemeral")
    };
    let pid = if let Some(ref pool) = pool_opt {
        pool.root_session().info().pid.expect("pool pid")
    } else {
        session.info().pid.expect("pid")
    };

    session
        .navigate(&server1.url("/"))
        .await
        .expect("navigate opener /");

    let setup_js = r#"(() => {
        window.__xoReady = false;
        window.__xoReports = {};
        window.addEventListener('message', (ev) => {
            if (!ev.data) return;
            if (ev.data.kind === 'popup-xo-ready') window.__xoReady = true;
            else if (ev.data.kind === 'xo-env-report') window.__xoReports[ev.data.tag] = ev.data.payload;
        });
    })()"#;
    session
        .eval_main_world(setup_js)
        .await
        .expect("setup opener listener");

    let xo_url = server2.localhost_url("/xo-env-probe");
    let popup_url = server1.url(&format!("/popup-xo-env?xo={xo_url}"));
    let open_js = format!(
        "window.__pop = window.open({}, '_blank'); Boolean(window.__pop)",
        serde_json::to_string(&popup_url).unwrap()
    );
    let opened = session
        .eval_main_world(&open_js)
        .await
        .expect("window.open popup");
    assert_eq!(
        opened,
        serde_json::json!(true),
        "{engine}: window.open failed"
    );

    let mut ready = false;
    for _ in 0..300 {
        if session
            .eval_main_world("Boolean(window.__xoReady)")
            .await
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        ready,
        "{engine} vc={use_virtual}: timed out waiting for popup-xo-ready"
    );

    session
        .eval_main_world("window.__pop.postMessage({ kind: 'collect-all', tag: 'before' }, '*')")
        .await
        .expect("request before report");
    let mut before = serde_json::Value::Null;
    for _ in 0..300 {
        let val = session
            .eval_main_world("window.__xoReports.before || null")
            .await
            .expect("poll before report");
        if !val.is_null() {
            before = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !before.is_null(),
        "{engine} vc={use_virtual}: missing before report"
    );

    for label in ["pop", "so", "xo"] {
        let node = &before[label];
        assert_eq!(
            node["platform"], win_jp.platform,
            "{engine} vc={use_virtual} {label} before: {node:?}"
        );
        assert_eq!(
            node["langs"][0], "ja-JP",
            "{engine} vc={use_virtual} {label} before: {node:?}"
        );
        assert_eq!(
            node["ua"], win_jp.user_agent,
            "{engine} vc={use_virtual} {label} before: {node:?}"
        );
        assert_eq!(
            node["tz"], "Asia/Tokyo",
            "{engine} vc={use_virtual} {label} before: {node:?}"
        );
        assert_eq!(
            node["screenW"], win_jp.screen_width,
            "{engine} vc={use_virtual} {label} before: {node:?}"
        );
        if engine == EngineKind::Chromium {
            assert_eq!(
                node["uadPlat"], "Windows",
                "{engine} vc={use_virtual} {label} before: {node:?}"
            );
        }
    }

    // Opener tab remains the active session context while popup + its cross-origin iframe are open.
    session
        .set_env_overrides(linux_de.clone())
        .await
        .expect("set_env_overrides linux_de");

    session
        .eval_main_world("window.__pop.postMessage({ kind: 'collect-all', tag: 'after' }, '*')")
        .await
        .expect("request after report");
    let mut after = serde_json::Value::Null;
    for _ in 0..300 {
        let val = session
            .eval_main_world("window.__xoReports.after || null")
            .await
            .expect("poll after report");
        if !val.is_null() {
            after = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !after.is_null(),
        "{engine} vc={use_virtual}: missing after report"
    );

    for label in ["pop", "so", "xo"] {
        let node = &after[label];
        assert_eq!(
            node["platform"], linux_de.platform,
            "{engine} vc={use_virtual} {label} after: {node:?}"
        );
        assert_eq!(
            node["langs"][0], "de-DE",
            "{engine} vc={use_virtual} {label} after: {node:?}"
        );
        assert_eq!(
            node["ua"], linux_de.user_agent,
            "{engine} vc={use_virtual} {label} after: {node:?}"
        );
        assert_eq!(
            node["tz"], "Europe/Berlin",
            "{engine} vc={use_virtual} {label} after: {node:?}"
        );
        assert_eq!(
            node["screenW"], linux_de.screen_width,
            "{engine} vc={use_virtual} {label} after: {node:?}"
        );
        assert_eq!(
            node["echo"]["user-agent"], linux_de.user_agent,
            "{engine} vc={use_virtual} {label} echo ua after: {node:?}"
        );
        assert!(
            node["echo"]["accept-language"]
                .as_str()
                .unwrap_or("")
                .starts_with("de-DE"),
            "{engine} vc={use_virtual} {label} echo accept-language after: {node:?}"
        );
        if engine == EngineKind::Chromium {
            assert_eq!(
                node["uadPlat"], "Linux",
                "{engine} vc={use_virtual} {label} after: {node:?}"
            );
        }
    }

    if engine == EngineKind::Firefox {
        let tabs = session.list_tabs().await.expect("list_tabs");
        let popup_tid = tabs
            .iter()
            .find(|t| t.url.contains("/popup-xo-env"))
            .expect("popup tab in list_tabs")
            .target_id
            .clone();
        let snap_expr = r#"(() => ({
            platform: navigator.platform,
            ua: navigator.userAgent,
            tz: Intl.DateTimeFormat().resolvedOptions().timeZone
        }))()"#;
        let pop_main = session
            .eval_main_world_in_frame(snap_expr, Some(&popup_tid))
            .await
            .expect("firefox popup eval_main_world_in_frame");
        let pop_iso = session
            .eval_isolated_in_frame(snap_expr, Some(&popup_tid))
            .await
            .expect("firefox popup eval_isolated_in_frame");
        for (world, val) in [("main", &pop_main), ("isolated", &pop_iso)] {
            assert_eq!(
                val["platform"], linux_de.platform,
                "firefox {world}: {val:?}"
            );
            assert_eq!(val["ua"], linux_de.user_agent, "firefox {world}: {val:?}");
            assert_eq!(val["tz"], "Europe/Berlin", "firefox {world}: {val:?}");
        }
    }

    if let Some(vc) = vc_opt {
        vc.dispose().await.expect("dispose vc");
    } else {
        session.dispose().await.expect("dispose session");
    }
    if let Some(pool) = pool_opt {
        pool.shutdown().await.expect("shutdown pool");
    }
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_popup_cross_origin_iframe_chromium() {
    assert_dynamic_set_env_overrides_updates_popup_cross_origin_iframe(EngineKind::Chromium, false)
        .await;
    assert_dynamic_set_env_overrides_updates_popup_cross_origin_iframe(EngineKind::Chromium, true)
        .await;
}

#[tokio::test]
async fn dynamic_set_env_overrides_updates_popup_cross_origin_iframe_firefox() {
    assert_dynamic_set_env_overrides_updates_popup_cross_origin_iframe(EngineKind::Firefox, false)
        .await;
    assert_dynamic_set_env_overrides_updates_popup_cross_origin_iframe(EngineKind::Firefox, true)
        .await;
}

async fn assert_blob_worker_and_shared_worker_immediate_revoke(engine: EngineKind) {
    let server = TestServer::start().await;
    let win_jp = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("valid win11 JP");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            env_profile: Some(win_jp.clone()),
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch session");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/"))
        .await
        .expect("navigate /");

    let probe_js = r#"(async () => {
        const withTimeout = (p, label) => Promise.race([
            p,
            new Promise((_, rej) => setTimeout(() => rej(new Error(label + ' timed out')), 15000))
        ]);

        const runClassic = () => withTimeout(new Promise((resolve) => {
            const b = new Blob([
                '"use strict"; function strictCheck() { return this === undefined; } postMessage({ ok: true, strict: strictCheck(), platform: navigator.platform, ua: navigator.userAgent, tz: Intl.DateTimeFormat().resolvedOptions().timeZone });'
            ], { type: 'application/javascript' });
            const u = URL.createObjectURL(b);
            const w = new Worker(u);
            URL.revokeObjectURL(u);
            w.onmessage = (e) => { w.terminate(); resolve(e.data); };
            w.onerror = (e) => { w.terminate(); resolve({ error: e.message || 'classic error' }); };
        }), 'classic');

        const runModule = () => withTimeout(new Promise((resolve) => {
            const b = new Blob([
                'export const x = 1; postMessage({ ok: true, platform: navigator.platform, ua: navigator.userAgent, tz: Intl.DateTimeFormat().resolvedOptions().timeZone });'
            ], { type: 'application/javascript' });
            const u = URL.createObjectURL(b);
            const w = new Worker(u, { type: 'module' });
            URL.revokeObjectURL(u);
            w.onmessage = (e) => { w.terminate(); resolve(e.data); };
            w.onerror = (e) => { w.terminate(); resolve({ error: e.message || 'module error' }); };
        }), 'module');

        const runShared = () => withTimeout(new Promise((resolve) => {
            const b = new Blob([
                'let count = 0; onconnect = (ev) => { count++; const p = ev.ports[0]; p.postMessage({ ok: true, count, platform: navigator.platform, ua: navigator.userAgent, tz: Intl.DateTimeFormat().resolvedOptions().timeZone }); };'
            ], { type: 'application/javascript' });
            const u = URL.createObjectURL(b);
            const sw1 = new SharedWorker(u, 'rev-shared-worker');
            const sw2 = new SharedWorker(u, 'rev-shared-worker');
            URL.revokeObjectURL(u);
            const msgs = [];
            const onErr = (e) => resolve({ error: e.message || 'shared error' });
            sw1.onerror = onErr;
            sw2.onerror = onErr;
            const onMsg = (ev) => {
                msgs.push(ev.data);
                if (msgs.length === 2) resolve({ msgs });
            };
            sw1.port.onmessage = onMsg;
            sw2.port.onmessage = onMsg;
            sw1.port.start();
            sw2.port.start();
        }), 'shared');

        const runNested = () => withTimeout(new Promise((resolve) => {
            const outerSrc = `
                const innerBlob = new Blob([
                    'postMessage({ ok: true, platform: navigator.platform, tz: Intl.DateTimeFormat().resolvedOptions().timeZone });'
                ], { type: 'application/javascript' });
                const u2 = URL.createObjectURL(innerBlob);
                const w2 = new Worker(u2);
                URL.revokeObjectURL(u2);
                w2.onmessage = (e) => { w2.terminate(); postMessage({ ok: true, outerPlatform: navigator.platform, inner: e.data }); };
                w2.onerror = (e) => { w2.terminate(); postMessage({ error: e.message || 'nested inner error' }); };
            `;
            const u1 = URL.createObjectURL(new Blob([outerSrc], { type: 'application/javascript' }));
            const w1 = new Worker(u1);
            URL.revokeObjectURL(u1);
            w1.onmessage = (e) => { w1.terminate(); resolve(e.data); };
            w1.onerror = (e) => { w1.terminate(); resolve({ error: e.message || 'nested outer error' }); };
        }), 'nested');

        return {
            noGlobalLeak: !('__mimikyu' in globalThis),
            classic: await runClassic(),
            module: await runModule(),
            shared: await runShared(),
            nested: await runNested(),
        };
    })()"#;

    let res = session
        .eval_main_world(probe_js)
        .await
        .expect("eval blob worker revoke probe");

    assert_eq!(
        res["noGlobalLeak"], true,
        "{engine}: leaked global: {res:?}"
    );

    assert_eq!(res["classic"]["ok"], true, "{engine} classic: {res:?}");
    assert_eq!(
        res["classic"]["strict"], true,
        "{engine} classic strict: {res:?}"
    );
    assert_eq!(
        res["classic"]["platform"], win_jp.platform,
        "{engine} classic: {res:?}"
    );
    assert_eq!(
        res["classic"]["ua"], win_jp.user_agent,
        "{engine} classic: {res:?}"
    );
    assert_eq!(
        res["classic"]["tz"], "Asia/Tokyo",
        "{engine} classic: {res:?}"
    );

    assert_eq!(res["module"]["ok"], true, "{engine} module: {res:?}");
    assert_eq!(
        res["module"]["platform"], win_jp.platform,
        "{engine} module: {res:?}"
    );
    assert_eq!(
        res["module"]["ua"], win_jp.user_agent,
        "{engine} module: {res:?}"
    );
    assert_eq!(
        res["module"]["tz"], "Asia/Tokyo",
        "{engine} module: {res:?}"
    );

    let shared_msgs = res["shared"]["msgs"]
        .as_array()
        .unwrap_or_else(|| panic!("{engine} shared missing msgs: {res:?}"));
    assert_eq!(shared_msgs.len(), 2, "{engine} shared: {res:?}");
    let mut counts: Vec<i64> = shared_msgs
        .iter()
        .map(|m| m["count"].as_i64().unwrap_or(0))
        .collect();
    counts.sort();
    assert_eq!(
        counts,
        vec![1, 2],
        "{engine} SharedWorker must deduplicate: {res:?}"
    );
    for m in shared_msgs {
        assert_eq!(m["ok"], true, "{engine} shared msg: {res:?}");
        assert_eq!(
            m["platform"], win_jp.platform,
            "{engine} shared msg: {res:?}"
        );
        assert_eq!(m["tz"], "Asia/Tokyo", "{engine} shared msg: {res:?}");
    }

    assert_eq!(res["nested"]["ok"], true, "{engine} nested: {res:?}");
    assert_eq!(
        res["nested"]["outerPlatform"], win_jp.platform,
        "{engine} nested: {res:?}"
    );
    assert_eq!(
        res["nested"]["inner"]["ok"], true,
        "{engine} nested inner: {res:?}"
    );
    assert_eq!(
        res["nested"]["inner"]["platform"], win_jp.platform,
        "{engine} nested inner: {res:?}"
    );
    assert_eq!(
        res["nested"]["inner"]["tz"], "Asia/Tokyo",
        "{engine} nested inner: {res:?}"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn blob_worker_and_shared_worker_immediate_revoke_chromium() {
    assert_blob_worker_and_shared_worker_immediate_revoke(EngineKind::Chromium).await;
}

#[tokio::test]
async fn blob_worker_and_shared_worker_immediate_revoke_firefox() {
    assert_blob_worker_and_shared_worker_immediate_revoke(EngineKind::Firefox).await;
}

async fn assert_ephemeral_popup_close_then_immediate_set_env_overrides(engine: EngineKind) {
    let server = TestServer::start().await;
    let p1 = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("DE")
        .expect("geoip DE")
        .build()
        .expect("valid win11 DE");
    let p2 = EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
        .geoip("JP")
        .expect("geoip JP")
        .build()
        .expect("valid win11 JP");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            env_profile: Some(p1),
            ..Default::default()
        })
        .await
        .expect("launch ephemeral");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/target"))
        .await
        .expect("navigate /target");

    for iter in 0..3 {
        let open_res = session
            .eval_main_world(&format!(
                r#"(async () => {{
                    const pop = window.open('/target', 'close_race_{iter}', 'width=320,height=240');
                    if (!pop) return {{ opened: false }};
                    await new Promise((r) => setTimeout(r, 80));
                    pop.close();
                    return {{ opened: true }};
                }})()"#
            ))
            .await
            .expect("open and close popup");
        assert_eq!(
            open_res["opened"], true,
            "{engine} iter {iter}: {open_res:?}"
        );

        let target_prof = if iter % 2 == 0 {
            p2.clone()
        } else {
            EnvProfileBuilder::from_preset(PresetKind::Win11, engine)
                .geoip("DE")
                .unwrap()
                .build()
                .unwrap()
        };
        let expected_tz = target_prof.timezone.clone();
        session
            .set_env_overrides(target_prof)
            .await
            .unwrap_or_else(|e| {
                panic!("{engine} iter {iter}: set_env_overrides after pop.close() failed: {e}")
            });
        let tz = session
            .eval_isolated("Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await
            .expect("check tz after override");
        assert_eq!(tz, expected_tz, "{engine} iter {iter}: tz mismatch");
    }

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn ephemeral_popup_close_then_immediate_set_env_overrides_chromium() {
    assert_ephemeral_popup_close_then_immediate_set_env_overrides(EngineKind::Chromium).await;
}

#[tokio::test]
async fn ephemeral_popup_close_then_immediate_set_env_overrides_firefox() {
    assert_ephemeral_popup_close_then_immediate_set_env_overrides(EngineKind::Firefox).await;
}

async fn assert_webrtc_host_candidate_suppression(engine: EngineKind) {
    let server = TestServer::start().await;
    let profile = EnvProfileBuilder::from_preset(PresetKind::MacOs, engine)
        .webrtc(WebRtcPolicy::DisableNonProxiedUdp)
        .build()
        .expect("valid macos DisableNonProxiedUdp profile");

    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            env_profile: Some(profile),
            ..Default::default()
        })
        .await
        .expect("launch with DisableNonProxiedUdp");
    let pid = session.info().pid.expect("pid");

    session
        .navigate(&server.url("/target"))
        .await
        .expect("navigate /target");

    let probe_js = r#"(() => new Promise(async (resolve) => {
        const PC = window.RTCPeerConnection || window.webkitRTCPeerConnection || window.mozRTCPeerConnection;
        if (!PC) {
            return resolve({ status: "unavailable", hasHostCandidate: false });
        }
        const pc = new PC({ iceServers: [] });
        const onIceSeen = [];
        const addElSeen = [];
        let onceSeen = null;

        const fn = (ev) => {
            onIceSeen.push(ev && ev.candidate ? ev.candidate.candidate : null);
        };
        pc.onicecandidate = fn;
        const getterMatches = pc.onicecandidate === fn;

        pc.addEventListener("icecandidate", (ev) => {
            addElSeen.push(ev && ev.candidate ? ev.candidate.candidate : null);
        });
        pc.addEventListener("icecandidate", (ev) => {
            onceSeen = ev && ev.candidate ? ev.candidate.candidate : "EOF";
        }, { once: true });

        // Simulate raw-IP typ host ICE event (as emitted on Linux without mDNS) followed by srflx.
        const hostEv = Object.assign(new Event("icecandidate"), {
            candidate: {
                candidate: "candidate:1 1 tcp 1518280447 10.1.0.5 9 typ host tcptype active",
                type: "host",
            },
        });
        pc.dispatchEvent(hostEv);

        const srflxEv = Object.assign(new Event("icecandidate"), {
            candidate: {
                candidate: "candidate:2 1 udp 1686052607 203.0.113.9 54321 typ srflx raddr 0.0.0.0 rport 0",
                type: "srflx",
            },
        });
        pc.dispatchEvent(srflxEv);

        // Also run real ICE gathering via createOffer + setLocalDescription
        const realCands = [];
        const pc2 = new PC({ iceServers: [] });
        pc2.createDataChannel("probe");
        const realDone = new Promise((r) => {
            const t = setTimeout(() => r("timeout"), 1200);
            pc2.onicecandidate = (ev) => {
                if (ev && ev.candidate && ev.candidate.candidate) {
                    realCands.push(ev.candidate.candidate);
                } else if (!ev.candidate) {
                    clearTimeout(t);
                    r("ok");
                }
            };
        });
        const offer = await pc2.createOffer();
        await pc2.setLocalDescription(offer);
        const gatherStatus = await realDone;
        const sdp = pc2.localDescription ? pc2.localDescription.sdp : "";
        pc.close();
        pc2.close();

        const anyHostInReal = realCands.some((c) => /\btyp\s+host\b/i.test(c));
        const anyHostInSdp = sdp.split(/\r?\n/).some((l) => /^a=candidate:/i.test(l) && /\btyp\s+host\b/i.test(l));
        resolve({
            status: gatherStatus,
            getterMatches,
            onIceSeen,
            addElSeen,
            onceSeen,
            realCands,
            hasHostCandidate: anyHostInReal || anyHostInSdp,
        });
    }))()"#;

    let res = session
        .eval_main_world(probe_js)
        .await
        .expect("eval WebRTC host-candidate probe");
    assert_eq!(res["getterMatches"], true, "{engine}: {res:?}");
    assert_eq!(
        res["onIceSeen"],
        serde_json::json!([
            "candidate:2 1 udp 1686052607 203.0.113.9 54321 typ srflx raddr 0.0.0.0 rport 0"
        ]),
        "{engine}: synthetic typ host must be filtered from onicecandidate while srflx passes: {res:?}"
    );
    assert_eq!(
        res["addElSeen"],
        serde_json::json!([
            "candidate:2 1 udp 1686052607 203.0.113.9 54321 typ srflx raddr 0.0.0.0 rport 0"
        ]),
        "{engine}: synthetic typ host must be filtered from addEventListener while srflx passes: {res:?}"
    );
    assert_eq!(
        res["onceSeen"],
        "candidate:2 1 udp 1686052607 203.0.113.9 54321 typ srflx raddr 0.0.0.0 rport 0",
        "{engine}: once listener must not be consumed by suppressed typ host event: {res:?}"
    );
    assert_eq!(
        res["hasHostCandidate"], false,
        "{engine}: real ICE gathering or SDP leaked typ host candidate: {res:?}"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn webrtc_host_candidate_suppression_chromium() {
    assert_webrtc_host_candidate_suppression(EngineKind::Chromium).await;
}

#[tokio::test]
async fn webrtc_host_candidate_suppression_firefox() {
    assert_webrtc_host_candidate_suppression(EngineKind::Firefox).await;
}
