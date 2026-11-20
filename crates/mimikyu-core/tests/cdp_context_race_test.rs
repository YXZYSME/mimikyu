use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mimikyu_core::MimikyuError;
use mimikyu_core::driver::{EngineKind, LaunchOptions, ProfileMode, driver_for};
use mimikyu_core::extract::{ExtractOptions, extract_links, extract_markdown};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

fn assert_pgid_gone(pgid: libc::pid_t) {
    let rc = unsafe { libc::kill(-pgid, 0) };
    assert_eq!(rc, -1, "process group {pgid} still alive");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

struct RaceTestServer {
    addr: SocketAddr,
    side_effect_hits: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl RaceTestServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let side_effect_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&side_effect_hits);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits = Arc::clone(&hits_clone);
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 2048];
                    let header_end = loop {
                        match stream.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                    break pos;
                                }
                                if buf.len() > 65536 {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    };

                    let header_text = String::from_utf8_lossy(&buf[..header_end]);
                    let req_line = header_text.lines().next().unwrap_or("GET / HTTP/1.1");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("GET");
                    let raw_target = parts.next().unwrap_or("/");
                    let (path, query) = match raw_target.split_once('?') {
                        Some((p, q)) => (p, q),
                        None => (raw_target, ""),
                    };

                    let owned_body: Option<String>;
                    let (status, content_type, body): (&str, &str, &str) = match (method, path) {
                        ("GET", "/churn") => {
                            let step: usize = query
                                .strip_prefix("n=")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                            let next_script = if step < 5 {
                                format!(
                                    r#"setTimeout(() => location.replace("/churn?n={}"), 20);"#,
                                    step + 1
                                )
                            } else {
                                String::new()
                            };
                            owned_body = Some(format!(
                                r#"<!doctype html>
<html><head><title>Churn {step}</title></head><body>
  <h1 id="churn-h1">Churn Step {step}</h1>
  <a href="/step/{step}">step-{step}</a>
  <div id="churn-host"></div>
  <script>
    const sr = document.getElementById('churn-host').attachShadow({{ mode: 'open' }});
    sr.innerHTML = '<button id="churn-btn">StepBtn {step}</button>';
    {next_script}
  </script>
</body></html>"#
                            ));
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                owned_body.as_deref().unwrap_or_default(),
                            )
                        }
                        ("GET", "/redirect-race") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head>
  <title>Redirect Race</title>
  <meta http-equiv="refresh" content="0;url=/redirect-target">
</head><body>
  <a href="/early">early</a>
  <script>setTimeout(() => location.replace("/redirect-target"), 5);</script>
</body></html>"#,
                        ),
                        ("GET", "/redirect-chain-1") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Chain 1</title></head><body>
  <a href="/hop1">hop1</a>
  <script>setTimeout(() => location.replace("/redirect-chain-2"), 15);</script>
</body></html>"#,
                        ),
                        ("GET", "/redirect-chain-2") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Chain 2</title></head><body>
  <a href="/hop2">hop2</a>
  <script>setTimeout(() => location.replace("/redirect-target"), 15);</script>
</body></html>"#,
                        ),
                        ("GET", "/redirect-target") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Redirect Target</title></head><body>
  <h1>Settled Target</h1>
  <a href="/final">final</a>
  <div id="ok">ready</div>
  <div id="open-host"></div>
  <div id="closed-host"></div>
  <script>
    const openRoot = document.getElementById('open-host').attachShadow({ mode: 'open' });
    openRoot.innerHTML = '<button id="open-btn">OpenSettled</button>';
    const closedRoot = document.getElementById('closed-host').attachShadow({ mode: 'closed' });
    closedRoot.innerHTML = '<button id="closed-btn">ClosedSettled</button>';
  </script>
</body></html>"#,
                        ),
                        ("GET", "/side-effect-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Side Effect Host</title></head><body>
  <div id="host">ready</div>
</body></html>"#,
                        ),
                        ("POST", "/side-effect") => {
                            hits.fetch_add(1, Ordering::SeqCst);
                            ("200 OK", "application/json", r#"{"recorded":true}"#)
                        }
                        ("GET", "/next") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Next Settled</title></head><body>
  <div id="next-ok">arrived</div>
</body></html>"#,
                        ),
                        ("GET", "/iframe-redirect-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Iframe Race Host</title></head><body>
  <h1>Host</h1>
  <iframe id="race-frame" src="/iframe-redirect-src"></iframe>
</body></html>"#,
                        ),
                        ("GET", "/iframe-redirect-src") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Iframe Src</title></head><body>
  <p id="frame-state">early-frame</p>
  <script>
    window.__frameMarker = 'early';
    setTimeout(() => location.replace("/iframe-redirect-dst"), 20);
  </script>
</body></html>"#,
                        ),
                        ("GET", "/iframe-redirect-dst") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            r#"<!doctype html>
<html><head><title>Iframe Dst</title></head><body>
  <p id="frame-state">settled-frame</p>
  <div id="child-shadow-host"></div>
  <script>
    window.__frameMarker = 'settled';
    const r = document.getElementById('child-shadow-host').attachShadow({ mode: 'open' });
    r.innerHTML = '<button id="child-shadow-btn">ChildShadowOk</button>';
  </script>
</body></html>"#,
                        ),
                        ("GET", "/favicon.ico") => ("204 No Content", "text/plain", ""),
                        _ => ("404 Not Found", "text/plain", "not found"),
                    };

                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        Self {
            addr,
            side_effect_hits,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn side_effect_hits(&self) -> usize {
        self.side_effect_hits.load(Ordering::SeqCst)
    }
}

impl Drop for RaceTestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn assert_no_raw_stale_context_rpc(err: &MimikyuError) {
    let text = err.to_string();
    assert!(
        !text.contains("Cannot find context with specified id")
            || text.contains("exhausted")
            || text.contains("side-effect safety"),
        "raw unrecovered CDP -32000 context error leaked: {text}"
    );
    assert!(
        !text.contains("Could not find object with given id")
            || text.contains("exhausted")
            || text.contains("side-effect safety"),
        "raw unrecovered CDP -32000 object-id error leaked: {text}"
    );
}

#[tokio::test]
async fn cdp_redirect_race_recovers_extract_eval_shadow_and_node_calls() {
    let server = RaceTestServer::start().await;
    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch chromium");
    let pid = session.info().pid.expect("chromium pid");

    // 1. Repeat Issue #30 /redirect-race and multi-hop /redirect-chain-1 churn
    // while immediately firing extract_links, extract_markdown, eval_isolated,
    // eval_main_world, query_shadow, and read-only metadata helpers.
    for round in 0..4 {
        let path = if round % 2 == 0 {
            "/redirect-race"
        } else {
            "/redirect-chain-1"
        };
        session
            .navigate(&server.url(path))
            .await
            .expect("navigate redirect race");

        let links = extract_links(session.as_ref(), &ExtractOptions::default())
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("round {round} extract_links failed: {e:?}");
            });
        assert!(
            !links.links.is_empty(),
            "round {round}: extract_links should return links from early or settled frame"
        );

        let md = extract_markdown(session.as_ref(), &ExtractOptions::default())
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("round {round} extract_markdown failed: {e:?}");
            });
        assert!(
            !md.markdown.is_empty(),
            "round {round}: extract_markdown should return non-empty markdown"
        );

        let iso = session
            .eval_isolated(
                "({ href: location.href, links: Array.from(document.querySelectorAll('a')).map(a => a.href) })",
            )
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("round {round} eval_isolated failed: {e:?}");
            });
        assert!(
            iso["href"]
                .as_str()
                .is_some_and(|u| u.starts_with("http://127.0.0.1:")),
            "round {round}: unexpected iso href: {iso:?}"
        );

        let mw = session
            .eval_main_world("({ href: location.href, ready: document.readyState })")
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("round {round} eval_main_world failed: {e:?}");
            });
        assert!(
            mw["href"].is_string(),
            "round {round}: unexpected mw: {mw:?}"
        );

        // Wait until settled on /redirect-target, then verify shadow + call_function_on_node
        let mut settled = false;
        for _ in 0..40 {
            if session
                .current_url()
                .await
                .is_ok_and(|u| u.ends_with("/redirect-target"))
            {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            settled,
            "round {round}: expected page to settle on /redirect-target"
        );
    }

    assert_eq!(session.title().await.expect("title"), "Redirect Target");
    assert_eq!(
        session.document_ready_state().await.expect("readyState"),
        "complete"
    );

    // 2. Shadow DOM (open + closed) and call_function_on_node recovery + ElementNotFound on stale bid
    let open = session
        .query_shadow(&["#open-host".into(), "#open-btn".into()], None)
        .await
        .expect("query open shadow");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].text, "OpenSettled");

    let closed = session
        .query_shadow(&["#closed-host".into(), "#closed-btn".into()], None)
        .await
        .expect("query closed shadow");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].text, "ClosedSettled");
    let closed_bid = closed[0].backend_node_id.expect("closed bid");

    let tag = session
        .call_function_on_node(
            closed_bid,
            None,
            "function() { return this.tagName; }",
            vec![],
        )
        .await
        .expect("call_function_on_node on settled closed shadow button");
    assert_eq!(tag, "BUTTON");

    // Bogus / gone backendNodeId MUST fail promptly with ElementNotFound (never raw -32000)
    let missing_err = session
        .call_function_on_node(
            999_999_999,
            None,
            "function() { return this.tagName; }",
            vec![],
        )
        .await
        .expect_err("missing backendNodeId must fail");
    assert!(
        matches!(missing_err, MimikyuError::ElementNotFound(_)),
        "expected ElementNotFound for missing backendNodeId, got {missing_err:?}"
    );

    // 3. Verify deepSerializedValue fast path rejects nested non-JSON platform objects
    for nested_bad in [
        "({ b: new Blob(['ok']) })",
        "({ d: new DOMException('e') })",
        "({ s: document.body.style })",
        "({ e: new Event('click') })",
    ] {
        let err = session
            .eval_isolated(nested_bad)
            .await
            .expect_err(&format!("expected EvalFailed for {nested_bad}"));
        assert!(
            matches!(err, MimikyuError::EvalFailed(_)),
            "expected EvalFailed for {nested_bad}, got {err:?}"
        );
    }

    // 4. Stale backendNodeId from prior document after navigation fails with ElementNotFound
    session
        .navigate(&server.url("/next"))
        .await
        .expect("navigate /next");
    let stale_node_err = session
        .call_function_on_node(
            closed_bid,
            None,
            "function() { return this.tagName; }",
            vec![],
        )
        .await
        .expect_err("stale backendNodeId after navigation must fail");
    assert!(
        matches!(stale_node_err, MimikyuError::ElementNotFound(_)),
        "expected ElementNotFound for stale backendNodeId after nav, got {stale_node_err:?}"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn cdp_side_effect_safety_never_reexecutes_user_eval_on_mid_eval_destruction() {
    let server = RaceTestServer::start().await;
    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch chromium");
    let pid = session.info().pid.expect("chromium pid");

    // 1. Isolated-world side-effecting eval that destroys its own execution context mid-flight
    session
        .navigate(&server.url("/side-effect-host"))
        .await
        .expect("navigate /side-effect-host");

    let iso_err = session
        .eval_isolated(
            r#"(() => {
                fetch('/side-effect', { method: 'POST', keepalive: true });
                return new Promise(() => {
                    setTimeout(() => location.replace('/next'), 15);
                });
            })()"#,
        )
        .await
        .expect_err("mid-eval navigation destruction must error without re-executing");
    match &iso_err {
        MimikyuError::Protocol { code, message } => {
            assert_eq!(*code, -32000);
            assert!(
                message.contains("Execution context was destroyed"),
                "unexpected isolated error message: {message}"
            );
            assert!(
                message.contains("side-effect safety"),
                "expected side-effect safety explanation, got: {message}"
            );
        }
        other => panic!("expected Protocol error, got {other:?}"),
    }

    // Allow keepalive fetch + navigation to finish and confirm hit count == 1 (never retried!)
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.side_effect_hits(),
        1,
        "isolated user eval must execute at most once on Tier B destruction"
    );

    // Session must remain healthy on the settled /next page
    assert!(
        session
            .current_url()
            .await
            .expect("current_url")
            .ends_with("/next")
    );
    assert_eq!(
        session.eval_isolated("1 + 1").await.expect("post-nav eval"),
        2
    );

    // 2. Main-world side-effecting eval must likewise NEVER re-execute on Tier B destruction
    session
        .navigate(&server.url("/side-effect-host"))
        .await
        .expect("navigate /side-effect-host again");

    let mw_err = session
        .eval_main_world(
            r#"(() => {
                fetch('/side-effect', { method: 'POST', keepalive: true });
                return new Promise(() => {
                    setTimeout(() => location.replace('/next'), 15);
                });
            })()"#,
        )
        .await
        .expect_err("mid-eval main-world navigation destruction must error without re-executing");
    match &mw_err {
        MimikyuError::Protocol { code, message } => {
            assert_eq!(*code, -32000);
            assert!(
                message.contains("Execution context was destroyed"),
                "unexpected main-world error message: {message}"
            );
            assert!(
                message.contains("side-effect safety"),
                "expected side-effect safety explanation, got: {message}"
            );
        }
        other => panic!("expected Protocol error, got {other:?}"),
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.side_effect_hits(),
        2,
        "main-world user eval must execute at most once on Tier B destruction"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn cdp_subframe_redirect_race_recovers_isolated_main_world_and_shadow_queries() {
    let server = RaceTestServer::start().await;
    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch chromium");
    let pid = session.info().pid.expect("chromium pid");

    session
        .navigate(&server.url("/iframe-redirect-host"))
        .await
        .expect("navigate /iframe-redirect-host");

    let frames = session.list_frames().await.expect("list_frames");
    let child_fid = frames
        .iter()
        .find(|f| !f.is_main)
        .expect("child iframe present")
        .frame_id
        .clone();

    // Immediately hammer isolated + main-world eval on the child frame while it
    // executes its 20ms `location.replace("/iframe-redirect-dst")`.
    let mut saw_settled = false;
    for _ in 0..20 {
        let iso = session
            .eval_isolated_in_frame(
                "({ href: location.href, state: document.getElementById('frame-state')?.textContent || '' })",
                Some(&child_fid),
            )
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("subframe eval_isolated_in_frame failed: {e:?}");
            });

        let mw = session
            .eval_main_world_in_frame("window.__frameMarker || ''", Some(&child_fid))
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("subframe eval_main_world_in_frame failed: {e:?}");
            });

        if iso["state"] == "settled-frame" && mw == "settled" {
            saw_settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        saw_settled,
        "child iframe should settle on /iframe-redirect-dst"
    );

    let child_shadow = session
        .query_shadow(
            &["#child-shadow-host".into(), "#child-shadow-btn".into()],
            Some(&child_fid),
        )
        .await
        .expect("query_shadow on settled child frame");
    assert_eq!(child_shadow.len(), 1);
    assert_eq!(child_shadow[0].text, "ChildShadowOk");

    // Nonexistent frame_id in eval_main_world_in_frame must preserve MimikyuError::NotFound
    let nf_err = session
        .eval_main_world_in_frame("1 + 1", Some("nonexistent-frame-id"))
        .await
        .expect_err("nonexistent frame must return NotFound");
    assert!(
        matches!(nf_err, MimikyuError::NotFound(_)),
        "expected NotFound for nonexistent frame, got {nf_err:?}"
    );

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn cdp_active_location_replace_churn_recovers_without_raw_32000() {
    let server = RaceTestServer::start().await;
    let session = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            main_world_eval: true,
            ..Default::default()
        })
        .await
        .expect("launch chromium");
    let pid = session.info().pid.expect("chromium pid");

    session
        .navigate(&server.url("/churn?n=0"))
        .await
        .expect("navigate /churn?n=0");

    // Rapidly hammer extract, eval_isolated, eval_main_world, query_shadow,
    // resolve_selector_box (query_backend_node_id), and metadata queries while
    // the page performs 5 consecutive 20ms `location.replace` hops.
    let mut reached_final = false;
    for iter in 0..30 {
        let links = extract_links(session.as_ref(), &ExtractOptions::default())
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("churn iter {iter} extract_links failed: {e:?}");
            });
        assert!(!links.links.is_empty());

        let md = extract_markdown(session.as_ref(), &ExtractOptions::default())
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("churn iter {iter} extract_markdown failed: {e:?}");
            });
        assert!(!md.markdown.is_empty());

        let iso = session
            .eval_isolated(
                "({ url: location.href, h1: document.getElementById('churn-h1')?.textContent || '' })",
            )
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("churn iter {iter} eval_isolated failed: {e:?}");
            });
        assert!(iso["url"].is_string());

        let mw = session
            .eval_main_world("({ url: location.href, ready: document.readyState })")
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("churn iter {iter} eval_main_world failed: {e:?}");
            });
        assert!(mw["url"].is_string());

        let shadow = session
            .query_shadow(&["#churn-host".into(), "#churn-btn".into()], None)
            .await
            .unwrap_or_else(|e| {
                assert_no_raw_stale_context_rpc(&e);
                panic!("churn iter {iter} query_shadow failed: {e:?}");
            });
        if !shadow.is_empty() {
            assert!(shadow[0].text.starts_with("StepBtn"));
        }

        match session.resolve_selector_box("#churn-h1").await {
            Ok(bbox) => assert!(bbox.width >= 0.0),
            Err(MimikyuError::ElementNotFound(_)) => {}
            Err(e) => {
                assert_no_raw_stale_context_rpc(&e);
                panic!("churn iter {iter} resolve_selector_box failed: {e:?}");
            }
        }

        let url = session.current_url().await.expect("current_url");
        if url.ends_with("/churn?n=5") {
            reached_final = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reached_final, "expected churn page to settle at /churn?n=5");
    assert_eq!(session.title().await.expect("final title"), "Churn 5");

    session.dispose().await.expect("dispose");
    assert_pgid_gone(pid as libc::pid_t);
}
