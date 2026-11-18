mod support;

use std::time::Duration;

use mimikyu_core::driver::{
    driver_for, ContextOptions, DriverSession, EngineKind, EnvProfile, LaunchOptions, ProfileMode,
    ProxyConfig, WebRtcPolicy,
};
use mimikyu_core::env_profile::{
    lookup_geoip, validate_or_err, validate_profile, EnvProfileBuilder, PresetKind,
};
use mimikyu_core::isolation::VirtualContextPool;
use support::{
    assert_pgid_gone, KeepAliveTestServer, LocalHttpProxy, LocalSocks5Proxy,
    StrictKeepAliveHttpProxy, TestServer,
};

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
        probe["glVendor"].as_str().is_some_and(|s| !s.trim().is_empty()),
        "{engine}: gl.getParameter(gl.VENDOR) must be non-empty: {probe:?}"
    );
    assert!(
        probe["glRenderer"].as_str().is_some_and(|s| !s.trim().is_empty()),
        "{engine}: gl.getParameter(gl.RENDERER) must be non-empty: {probe:?}"
    );
    assert!(
        probe["glVersion"].as_str().is_some_and(|s| !s.trim().is_empty()),
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
    assert_eq!(worker["userAgent"], profile.user_agent, "{engine} worker UA");
    assert_eq!(worker["platform"], "Linux x86_64", "{engine} worker platform");
    assert_eq!(worker["language"], "de-DE", "{engine} worker language");
    assert_eq!(worker["languages"][0], "de-DE", "{engine} worker languages[0]");
    assert_eq!(worker["hardwareConcurrency"], 8, "{engine} worker cores");
    assert_eq!(worker["timeZone"], "Europe/Berlin", "{engine} worker timeZone");
    assert_eq!(
        worker["tzOffset"], probe["tzOffset"],
        "{engine} worker getTimezoneOffset must match main frame"
    );
    if engine == EngineKind::Chromium {
        assert_eq!(worker["deviceMemory"], 8);
    }

    // ES module Worker assertions
    let mworker = &probe["mworker"];
    assert_eq!(mworker["userAgent"], profile.user_agent, "{engine} mworker UA");
    assert_eq!(mworker["platform"], "Linux x86_64", "{engine} mworker platform");
    assert_eq!(mworker["language"], "de-DE", "{engine} mworker language");
    assert_eq!(mworker["languages"][0], "de-DE", "{engine} mworker languages[0]");
    assert_eq!(mworker["hardwareConcurrency"], 8, "{engine} mworker cores");
    assert_eq!(mworker["timeZone"], "Europe/Berlin", "{engine} mworker timeZone");
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
    assert_eq!(probe["mworker"]["platform"], "MacIntel", "{engine} VC mworker platform");
    assert_eq!(
        probe["mworker"]["hardwareConcurrency"], jp_profile.hardware_concurrency,
        "{engine} VC mworker cores"
    );
    assert_eq!(probe["mworker"]["language"], "ja-JP", "{engine} VC mworker language");
    assert_eq!(probe["mworker"]["languages"][0], "ja-JP", "{engine} VC mworker languages[0]");
    assert_eq!(probe["mworker"]["timeZone"], "Asia/Tokyo", "{engine} VC mworker timeZone");
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
        let p = vc_win.session().eval_isolated(PROBE_ISOLATED_JS).await.unwrap();
        if !p.is_null() {
            win_probe = p;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!win_probe["mworker"].is_null(), "{engine}: win_probe mworker timed out");
    assert_eq!(win_probe["mworker"]["platform"], "Win32", "{engine} VC win32 mworker platform");
    assert_eq!(win_probe["mworker"]["hardwareConcurrency"], 8, "{engine} VC win32 mworker cores");
    assert_eq!(win_probe["mworker"]["language"], "de-DE", "{engine} VC win32 mworker language");
    assert_eq!(win_probe["mworker"]["languages"][0], "de-DE", "{engine} VC win32 mworker languages[0]");
    assert_eq!(win_probe["mworker"]["timeZone"], "Europe/Berlin", "{engine} VC win32 mworker timeZone");
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
    let strict_http_proxy = StrictKeepAliveHttpProxy::start(("ka_user".into(), "ka_pass".into())).await;
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
    assert_eq!(http_out["s1"], 200, "{engine} HTTP keepalive s1: {http_out:?}");
    assert_eq!(http_out["b1"], "ka:n=1");
    assert_eq!(http_out["s2"], 200, "{engine} HTTP keepalive s2: {http_out:?}");
    assert_eq!(http_out["b2"], "ka:n=2");
    assert_eq!(http_out["s3"], 200, "{engine} HTTP keepalive s3: {http_out:?}");
    assert_eq!(http_out["b3"], "ka:n=3");
    assert_eq!(http_out["s4"], 200, "{engine} HTTP keepalive chunked s4: {http_out:?}");
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
    assert_eq!(socks_out["s1"], 200, "{engine} SOCKS5 keepalive s1: {socks_out:?}");
    assert_eq!(socks_out["b1"], "ka:n=1");
    assert_eq!(socks_out["s2"], 200, "{engine} SOCKS5 keepalive s2: {socks_out:?}");
    assert_eq!(socks_out["b2"], "ka:n=2");
    assert_eq!(socks_out["s3"], 200, "{engine} SOCKS5 keepalive s3: {socks_out:?}");
    assert_eq!(socks_out["b3"], "ka:n=3");
    assert_eq!(socks_out["s4"], 200, "{engine} SOCKS5 keepalive chunked s4: {socks_out:?}");
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
    assert_eq!(nav.status, Some(200), "{engine}: expected HTTP 200 via socks5h");
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
        assert_eq!(shared1["workerMetaUrl"], server.url("/shared-echo-worker.mjs"));
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
