use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub struct TestServer {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_clone = Arc::clone(&hits);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits_req = Arc::clone(&hits_clone);
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 2048];
                    let header_end = loop {
                        match stream.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
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
                    let mut lines = header_text.lines();
                    let req_line = lines.next().unwrap_or("GET / HTTP/1.1");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("GET");
                    let raw_target = parts.next().unwrap_or("/");
                    let path = raw_target.split('?').next().unwrap_or("/");

                    let mut headers = BTreeMap::new();
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }

                    hits_req.lock().push(format!("{method} {path}"));

                    let (status, content_type, extra_headers, body): (
                        &str,
                        &str,
                        Vec<(&str, &str)>,
                        Vec<u8>,
                    ) = match (method, path) {
                        ("GET", "/") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Mimikyu M1 Home</title></head>
<body>
  <h1>Ready</h1>
  <button id="btn" onclick="window.__clicked = (window.__clicked || 0) + 1; document.getElementById('status').textContent = 'clicked:' + window.__clicked;">Go</button>
  <input id="inp" type="text" placeholder="Type here" />
  <input id="file-inp" type="file" />
  <div id="status">idle</div>
  <a id="next-link" href="/target">Next</a>
  <script>
    window.__mainSecret = 'main-only-value';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/redirect") => (
                            "302 Found",
                            "text/plain",
                            vec![("Location", "/target")],
                            Vec::new(),
                        ),
                        ("GET", "/target") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Redirect Target</title></head><body><p id="ok">arrived</p></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/echo-headers") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            serde_json::to_vec(&headers).unwrap_or_default(),
                        ),
                        ("GET", "/console") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Console</title></head><body><script>console.log("m1-log", 42);</script><p>logged</p></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/cookie") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![("Set-Cookie", "mimikyu_test=1; Path=/")],
                            br#"<!doctype html><html><head><title>Cookie</title></head><body>cookie set</body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/network") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Network</title></head><body><div id="net">network</div></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/blocked.png") => (
                            "200 image/png",
                            "image/png",
                            vec![],
                            vec![137, 80, 78, 71, 13, 10, 26, 10],
                        ),
                        ("GET", "/api/real") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"real":true}"#.to_vec(),
                        ),
                        ("GET", "/api/mock") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"real":true}"#.to_vec(),
                        ),
                        ("GET", "/isolation") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Isolation</title></head><body><div id="iso">isolated</div></body></html>"#
                                .to_vec(),
                        ),
                        _ => (
                            "404 Not Found",
                            "text/plain",
                            vec![],
                            b"not found".to_vec(),
                        ),
                    };

                    let mut resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in extra_headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");

                    let _ = stream.write_all(resp.as_bytes()).await;
                    if !body.is_empty() {
                        let _ = stream.write_all(&body).await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });

        Self { addr, hits, task }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
