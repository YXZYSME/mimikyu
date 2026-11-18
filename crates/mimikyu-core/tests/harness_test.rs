use std::net::TcpListener;
use std::time::Duration;

use mimikyu_core::driver::{driver_for, EngineKind, LaunchOptions, WebRtcPolicy};
use mimikyu_core::env_profile::{EnvProfileBuilder, PresetKind};
use mimikyu_core::extract::TableExtractResult;
use mimikyu_core::harness::{HarnessOrigin, HarnessServer};
use mimikyu_core::input::BatchAction;
use mimikyu_core::session::{
    ExtractMode, SessionCreateOpts, SessionManager, SessionManagerConfig, WaitRequest,
};
use mimikyu_core::snapshot::SnapshotOptions;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn r3_forbidden_keywords() -> Vec<&'static str> {
    vec![
        concat!("disable_", "coop"),
        concat!("cap", "tcha"),
        concat!("recap", "tcha"),
        concat!("hcap", "tcha"),
        concat!("turn", "stile"),
        concat!("sun", "dial"),
        concat!("cf_", "clearance"),
        concat!("waf_", "bypass"),
        concat!("waf_", "evasion"),
        concat!("waf ", "evasion"),
        concat!("gate_", "bypass"),
        concat!("gate ", "bypass"),
        concat!("challenge_", "bypass"),
        concat!("challenge_", "solver"),
        concat!("anti", "bot"),
        concat!("anti-", "bot"),
        concat!("human_", "verification"),
        concat!("human-", "verification"),
    ]
}

fn assert_pgid_gone(pgid: i32) {
    let rc = unsafe { libc::kill(-pgid, 0) };
    assert_eq!(rc, -1, "process group {pgid} still alive");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

fn find_ref(
    snap: &mimikyu_core::snapshot::SnapshotOutput,
    roles: &[&str],
    name_query: &str,
) -> String {
    let q_lower = name_query.trim().to_ascii_lowercase();
    for (ref_id, entry) in snap.ref_map.iter() {
        if roles.iter().any(|r| entry.role.eq_ignore_ascii_case(r))
            && entry
                .name
                .as_deref()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case(name_query.trim())
        {
            return ref_id.clone();
        }
    }
    for (ref_id, entry) in snap.ref_map.iter() {
        if roles.iter().any(|r| entry.role.eq_ignore_ascii_case(r))
            && entry
                .name
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
                .contains(&q_lower)
        {
            return ref_id.clone();
        }
    }
    panic!(
        "ref not found for roles {roles:?} name '{name_query}' in:\n{}",
        snap.text
    );
}

async fn read_http_response(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut tmp))
            .await
            .expect("read header timeout")
            .expect("read header");
        assert!(n > 0, "unexpected EOF reading HTTP response header");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut content_len = 0usize;
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_len = v.trim().parse::<usize>().expect("valid content-length");
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut tmp))
            .await
            .expect("read body timeout")
            .expect("read body");
        assert!(n > 0, "unexpected EOF reading HTTP response body");
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);
    (head, body)
}

#[tokio::test]
async fn harness_binds_loopback_ephemeral_ports_and_clean_drop_shutdown() {
    let server = HarnessServer::start().await.expect("start harness");
    let primary = server.addr();
    let xo = server.cross_origin_addr().expect("cross-origin addr");
    assert!(primary.ip().is_loopback());
    assert_ne!(primary.port(), 0);
    assert!(xo.ip().is_loopback());
    assert_ne!(xo.port(), 0);
    assert_ne!(primary.port(), xo.port());

    // Open a keep-alive TCP connection and leave it idle before dropping server
    let mut stream = TcpStream::connect(primary).await.expect("connect primary");
    stream
        .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n")
        .await
        .expect("write request");
    let (head, _) = read_http_response(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 204 No Content"), "head: {head}");

    drop(server);

    let mut refused = false;
    for _ in 0..20 {
        tokio::task::yield_now().await;
        match TcpStream::connect(primary).await {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                refused = true;
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "primary port should be refused after drop");

    // Explicit shutdown().await must close immediately before returning
    let server2 = HarnessServer::start().await.expect("start harness2");
    let addr2 = server2.addr();
    let xo2 = server2.cross_origin_addr().expect("xo2");
    server2.shutdown().await;
    assert!(TcpStream::connect(addr2).await.is_err());
    assert!(TcpStream::connect(xo2).await.is_err());
    let _ = TcpListener::bind(addr2);
}

#[tokio::test]
async fn harness_http11_keepalive_close_query_and_post_echo() {
    let server = HarnessServer::start().await.expect("start harness");
    let mut stream = TcpStream::connect(server.addr())
        .await
        .expect("connect primary");

    // 1. Redirect over keep-alive
    stream
        .write_all(b"GET /redirect HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();
    let (h1, _) = read_http_response(&mut stream).await;
    assert!(h1.starts_with("HTTP/1.1 302 Found"), "{h1}");
    assert!(h1.contains("Location: /target"), "{h1}");

    // 2. Query decoding + custom header echo over same keep-alive socket
    stream
        .write_all(
            b"GET /echo-headers?foo=bar%20baz&n=42 HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Test-Hdr: ok\r\nConnection: keep-alive\r\n\r\n",
        )
        .await
        .unwrap();
    let (h2, b2) = read_http_response(&mut stream).await;
    assert!(h2.starts_with("HTTP/1.1 200 OK"), "{h2}");
    let hdr_json: Value = serde_json::from_slice(&b2).expect("json headers");
    assert_eq!(hdr_json["x-test-hdr"], "ok");

    // 3. POST Content-Length + Connection: close
    let form_body = b"name=Alice+Smith&country=de&tier=pro&terms=yes";
    let post_req = format!(
        "POST /submit HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        form_body.len()
    );
    stream.write_all(post_req.as_bytes()).await.unwrap();
    stream.write_all(form_body).await.unwrap();
    let (h3, b3) = read_http_response(&mut stream).await;
    assert!(h3.starts_with("HTTP/1.1 200 OK"), "{h3}");
    let body_str = String::from_utf8_lossy(&b3);
    assert!(
        body_str.contains("SUBMITTED:Alice Smith:de:pro:yes"),
        "unexpected submit html: {body_str}"
    );

    // Server must close connection after Connection: close
    let mut eof_buf = [0u8; 16];
    let n = stream.read(&mut eof_buf).await.expect("eof read");
    assert_eq!(n, 0, "expected clean EOF after Connection: close");

    let recorded = server.requests();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].origin, HarnessOrigin::Primary);
    assert_eq!(recorded[1].query.get("foo").map(String::as_str), Some("bar baz"));
    assert_eq!(recorded[1].query.get("n").map(String::as_str), Some("42"));
    assert_eq!(recorded[2].method, "POST");
    assert_eq!(recorded[2].body_len, form_body.len());

    server.shutdown().await;
}

#[tokio::test]
async fn harness_r3_compliance_static_check() {
    let server = HarnessServer::start().await.expect("start harness");
    let forbidden = r3_forbidden_keywords();
    let primary_paths = [
        "/",
        "/target",
        "/wizard",
        "/wizard/step2?name=Ada&role=editor",
        "/wizard/done?name=Ada&role=editor&region=us&agree=yes",
        "/form",
        "/table",
        "/catalog",
        "/dynamic",
        "/shadow",
        "/frames",
        "/frames/same-child",
        "/frames/nested-inner",
        "/env-probe",
        "/env-probe-frame",
        "/env-worker.js",
        "/env-worker.mjs",
        "/helper.mjs",
        "/geo-page",
    ];

    for path in primary_paths {
        let mut stream = TcpStream::connect(server.addr()).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let (head, body) = read_http_response(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{path}: {head}");
        let combined = format!("{head}\n{}", String::from_utf8_lossy(&body)).to_ascii_lowercase();
        for kw in &forbidden {
            assert!(
                !combined.contains(kw),
                "R3 forbidden keyword '{kw}' found in route {path}"
            );
        }
    }

    // Also inspect cross-origin routes on secondary listener
    let xo_addr = server.cross_origin_addr().expect("xo addr");
    for path in ["/xo-child", "/xo-env-frame"] {
        let mut stream = TcpStream::connect(xo_addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let (head, body) = read_http_response(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{path}: {head}");
        let combined = format!("{head}\n{}", String::from_utf8_lossy(&body)).to_ascii_lowercase();
        for kw in &forbidden {
            assert!(
                !combined.contains(kw),
                "R3 forbidden keyword '{kw}' found in cross-origin route {path}"
            );
        }
    }

    server.shutdown().await;
}

async fn run_live_harness_suite(engine: EngineKind) {
    let server = HarnessServer::start().await.expect("start harness");
    let tmp = tempfile::tempdir().expect("tempdir");
    let mgr = SessionManager::with_config(SessionManagerConfig {
        state_dir: tmp.path().to_path_buf(),
        default_engine: engine,
        default_headless: true,
        ..Default::default()
    })
    .expect("session manager");

    // Part 1: Ephemeral session checking /table, /dynamic, /shadow (open + closed on Chromium), /frames
    let sess = "harness-wf";
    let summary = mgr
        .create_session(SessionCreateOpts {
            session_id: Some(sess.to_string()),
            engine: Some(engine),
            ephemeral: true,
            headless: true,
            main_world_eval: false,
            ..Default::default()
        })
        .await
        .expect("create ephemeral session");
    let pid = summary.pid.expect("ephemeral pid") as i32;

    // 1. /table extraction
    mgr.open(sess, &server.url("/table")).await.unwrap();
    mgr.wait(
        sess,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let tbl_val = mgr.extract(sess, ExtractMode::Tables).await.unwrap();
    let tbls: TableExtractResult = serde_json::from_value(tbl_val).unwrap();
    assert_eq!(tbls.tables.len(), 2, "{engine}: expected 2 tables");
    assert_eq!(tbls.tables[1].row_count, 4, "{engine}: metrics table rows");

    // 2. /dynamic wait_idle + batch
    mgr.open(sess, &server.url("/dynamic")).await.unwrap();
    mgr.wait(
        sess,
        WaitRequest {
            idle: true,
            text: Some("DYNAMIC_HYDRATED".to_string()),
            selector: Some("#dyn-action".to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let dyn_snap = mgr
        .snapshot(
            sess,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let note_ref = find_ref(&dyn_snap, &["textbox"], "Hydration Note");
    let btn_ref = find_ref(&dyn_snap, &["button"], "Confirm Hydrated");
    let batch_rep = mgr
        .batch(
            sess,
            vec![
                BatchAction::Fill {
                    target: note_ref,
                    value: "live-check".to_string(),
                    humanize: Some(false),
                },
                BatchAction::Click {
                    target: btn_ref,
                    humanize: Some(false),
                    button: None,
                    click_count: None,
                },
                BatchAction::WaitText {
                    text: "CONFIRMED:live-check".to_string(),
                    timeout_ms: Some(5000),
                },
            ],
        )
        .await
        .unwrap();
    assert!(batch_rep.ok, "{engine}: batch failed {:?}", batch_rep.error);

    // 3. /shadow (open on both engines, closed on Chromium)
    mgr.open(sess, &server.url("/shadow")).await.unwrap();
    mgr.wait(
        sess,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sh_snap = mgr
        .snapshot(
            sess,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let open_in = find_ref(&sh_snap, &["textbox"], "Open Shadow Input");
    let open_btn = find_ref(&sh_snap, &["button"], "Open Shadow Action");
    mgr.fill(sess, &open_in, "abc", Some(false)).await.unwrap();
    mgr.click(sess, &open_btn, Some(false)).await.unwrap();
    mgr.wait(
        sess,
        WaitRequest {
            text: Some("open-clicked:abc".to_string()),
            timeout_ms: Some(5000),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    if engine == EngineKind::Chromium {
        let closed_in = find_ref(&sh_snap, &["textbox"], "Closed Shadow Input");
        let closed_btn = find_ref(&sh_snap, &["button"], "Closed Shadow Action");
        mgr.fill(sess, &closed_in, "xyz", Some(false)).await.unwrap();
        mgr.click(sess, &closed_btn, Some(false)).await.unwrap();
        mgr.wait(
            sess,
            WaitRequest {
                text: Some("closed-clicked:xyz".to_string()),
                timeout_ms: Some(5000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    mgr.close_session(sess).await.expect("close session");
    assert_pgid_gone(pid);

    // Part 2: EnvProfile + /env-probe across main, same-origin frame, cross-origin frame, and workers
    let profile = EnvProfileBuilder::from_preset(PresetKind::Linux, engine)
        .geoip("DE")
        .expect("geoip DE")
        .webrtc(WebRtcPolicy::DisableAll)
        .extra_header("X-Harness-Env", "m4-3")
        .build()
        .expect("valid profile");

    let drv = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            env_profile: Some(profile.clone()),
            ..Default::default()
        })
        .await
        .expect("launch env driver");
    let env_pid = drv.info().pid.expect("env pid") as i32;

    drv.navigate(&server.url("/echo-headers"))
        .await
        .expect("navigate echo-headers");
    let hdr_raw = drv
        .eval_isolated("document.body.innerText")
        .await
        .expect("eval echo-headers");
    let hdrs: Value = serde_json::from_str(hdr_raw.as_str().unwrap_or("{}")).unwrap();
    assert_eq!(hdrs["x-harness-env"].as_str(), Some("m4-3"));
    assert!(
        hdrs["accept-language"]
            .as_str()
            .unwrap_or("")
            .starts_with("de-DE")
    );

    drv.navigate(&server.url("/env-probe"))
        .await
        .expect("navigate env-probe");
    let probe_js = r#"(() => {
        const m = document.getElementById('main-out')?.textContent || '';
        const f = document.getElementById('frame-out')?.textContent || '';
        const x = document.getElementById('xo-frame-out')?.textContent || '';
        const w = document.getElementById('worker-out')?.textContent || '';
        const mw = document.getElementById('mworker-out')?.textContent || '';
        if (!m || !f || !x || !w || !mw) return null;
        return {
            main: JSON.parse(m),
            frame: JSON.parse(f),
            xoFrame: JSON.parse(x),
            worker: JSON.parse(w),
            mworker: JSON.parse(mw),
        };
    })()"#;

    let mut probe = Value::Null;
    for _ in 0..80 {
        let val = drv.eval_isolated(probe_js).await.expect("probe eval");
        if !val.is_null() {
            probe = val;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !probe.is_null(),
        "{engine}: env-probe timed out waiting for frame/xoFrame/workers"
    );
    assert_eq!(probe["main"]["noMimikyuGlobal"], Value::Bool(true));
    assert_eq!(probe["frame"]["noMimikyuGlobal"], Value::Bool(true));
    assert_eq!(probe["xoFrame"]["noMimikyuGlobal"], Value::Bool(true));
    assert_eq!(probe["main"]["timeZone"], "Europe/Berlin");
    assert_eq!(probe["frame"]["timeZone"], "Europe/Berlin");
    assert_eq!(probe["xoFrame"]["timeZone"], "Europe/Berlin");
    assert_eq!(probe["worker"]["timeZone"], "Europe/Berlin");
    assert_eq!(probe["mworker"]["timeZone"], "Europe/Berlin");
    assert_eq!(probe["mworker"]["helperTag"], "relative-helper-ok");

    drv.dispose().await.expect("dispose env driver");
    assert_pgid_gone(env_pid);

    mgr.shutdown_all().await.expect("shutdown_all");
    server.shutdown().await;
}

#[tokio::test]
async fn harness_live_workflows_chromium() {
    run_live_harness_suite(EngineKind::Chromium).await;
}

#[tokio::test]
async fn harness_live_workflows_firefox() {
    run_live_harness_suite(EngineKind::Firefox).await;
}
