use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use mimikyu_core::MimikyuError;
use mimikyu_core::driver::ProxyConfig;
use mimikyu_core::env_profile::ProxyRelay;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

async fn read_http_head(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = buf[..pos].to_vec();
            let leftover = buf[pos + 4..].to_vec();
            return (head, leftover);
        }
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut tmp))
            .await
            .expect("timed out reading HTTP head")
            .expect("read HTTP head");
        assert!(n > 0, "unexpected EOF reading HTTP head");
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn assert_closed_promptly(stream: &mut TcpStream, label: &str) {
    let mut buf = [0u8; 64];
    let res = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("{label}: timed out waiting for socket close"));
    match res {
        Ok(0) | Err(_) => {}
        Ok(n) => panic!(
            "{label}: expected EOF/close, got {n} unexpected bytes: {:?}",
            &buf[..n]
        ),
    }
}

struct MockHttpProxy {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    seen_heads: Arc<Mutex<Vec<String>>>,
    tunnel_closed_rx: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl MockHttpProxy {
    async fn start(label: &'static str, expected_auth: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen_heads = Arc::new(Mutex::new(Vec::new()));
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();

        let hits_c = Arc::clone(&hits);
        let heads_c = Arc::clone(&seen_heads);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let hits_inner = Arc::clone(&hits_c);
                let heads_inner = Arc::clone(&heads_c);
                let closed_inner = closed_tx.clone();
                let auth_inner = expected_auth.clone();
                tokio::spawn(async move {
                    hits_inner.fetch_add(1, Ordering::SeqCst);
                    let (head, _) = read_http_head(&mut s).await;
                    let head_str = String::from_utf8_lossy(&head).into_owned();
                    heads_inner.lock().push(head_str.clone());

                    if let Some(ref expected) = auth_inner {
                        let expected_line = format!("Proxy-Authorization: {expected}");
                        assert!(
                            head_str.contains(&expected_line),
                            "{label}: missing expected auth header '{expected_line}' in:\n{head_str}"
                        );
                    }

                    if head_str.starts_with("CONNECT ") {
                        let _ = s
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await;
                        let mut buf = [0u8; 256];
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    let reply =
                                        format!("{label}:{}", String::from_utf8_lossy(&buf[..n]));
                                    if s.write_all(reply.as_bytes()).await.is_err() {
                                        break;
                                    }
                                    let _ = s.flush().await;
                                }
                            }
                        }
                        let _ = closed_inner.send(());
                    } else if head_str.to_ascii_lowercase().contains("upgrade: websocket") {
                        let _ = s
                            .write_all(
                                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
                            )
                            .await;
                        let mut buf = [0u8; 256];
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    let reply = format!(
                                        "{label}-ws:{}",
                                        String::from_utf8_lossy(&buf[..n])
                                    );
                                    if s.write_all(reply.as_bytes()).await.is_err() {
                                        break;
                                    }
                                    let _ = s.flush().await;
                                }
                            }
                        }
                        let _ = closed_inner.send(());
                    } else {
                        let body = format!("proxy={label}");
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = s.write_all(resp.as_bytes()).await;
                        let _ = s.shutdown().await;
                    }
                });
            }
        });

        Self {
            addr,
            hits,
            seen_heads,
            tunnel_closed_rx: closed_rx,
            task,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    async fn wait_tunnel_closed(&mut self, label: &str) {
        tokio::time::timeout(Duration::from_secs(2), self.tunnel_closed_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for upstream tunnel close"))
            .expect("tunnel_closed channel closed unexpectedly");
    }
}

impl Drop for MockHttpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct MockSocks5Proxy {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    tunnel_closed_rx: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl MockSocks5Proxy {
    async fn start(
        label: &'static str,
        expected_user: &'static str,
        expected_pass: &'static str,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();

        let hits_c = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let hits_inner = Arc::clone(&hits_c);
                let closed_inner = closed_tx.clone();
                tokio::spawn(async move {
                    hits_inner.fetch_add(1, Ordering::SeqCst);

                    // 1. RFC 1928 Greeting
                    let mut ver_nmethods = [0u8; 2];
                    if s.read_exact(&mut ver_nmethods).await.is_err() {
                        return;
                    }
                    assert_eq!(ver_nmethods[0], 0x05);
                    let mut methods = vec![0u8; ver_nmethods[1] as usize];
                    s.read_exact(&mut methods).await.unwrap();
                    assert!(
                        methods.contains(&0x02),
                        "client must offer username/password auth"
                    );
                    s.write_all(&[0x05, 0x02]).await.unwrap();

                    // 2. RFC 1929 Username/Password subnegotiation
                    let mut auth_hdr = [0u8; 2];
                    s.read_exact(&mut auth_hdr).await.unwrap();
                    assert_eq!(auth_hdr[0], 0x01);
                    let mut user = vec![0u8; auth_hdr[1] as usize];
                    s.read_exact(&mut user).await.unwrap();
                    let mut plen = [0u8; 1];
                    s.read_exact(&mut plen).await.unwrap();
                    let mut pass = vec![0u8; plen[0] as usize];
                    s.read_exact(&mut pass).await.unwrap();
                    assert_eq!(std::str::from_utf8(&user).unwrap(), expected_user);
                    assert_eq!(std::str::from_utf8(&pass).unwrap(), expected_pass);
                    s.write_all(&[0x01, 0x00]).await.unwrap();

                    // 3. CONNECT request
                    let mut req_hdr = [0u8; 4];
                    s.read_exact(&mut req_hdr).await.unwrap();
                    assert_eq!(&req_hdr[..3], &[0x05, 0x01, 0x00]);
                    match req_hdr[3] {
                        0x01 => {
                            let mut rest = [0u8; 6];
                            s.read_exact(&mut rest).await.unwrap();
                        }
                        0x03 => {
                            let mut len = [0u8; 1];
                            s.read_exact(&mut len).await.unwrap();
                            let mut rest = vec![0u8; len[0] as usize + 2];
                            s.read_exact(&mut rest).await.unwrap();
                        }
                        0x04 => {
                            let mut rest = [0u8; 18];
                            s.read_exact(&mut rest).await.unwrap();
                        }
                        other => panic!("unexpected SOCKS5 ATYP {other}"),
                    }
                    s.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                        .await
                        .unwrap();

                    // 4. Handle either origin-form HTTP or raw tunnel echo
                    let mut peek = [0u8; 4];
                    let n = match s.read(&mut peek).await {
                        Ok(0) | Err(_) => {
                            let _ = closed_inner.send(());
                            return;
                        }
                        Ok(n) => n,
                    };
                    if peek[..n].starts_with(b"GET ") || peek[..n].starts_with(b"POST") {
                        let mut buf = peek[..n].to_vec();
                        let mut tmp = [0u8; 512];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            let m = s.read(&mut tmp).await.unwrap();
                            assert!(m > 0);
                            buf.extend_from_slice(&tmp[..m]);
                        }
                        let body = format!("socks5={label}");
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = s.write_all(resp.as_bytes()).await;
                        let _ = s.shutdown().await;
                    } else {
                        let first_reply =
                            format!("{label}:{}", String::from_utf8_lossy(&peek[..n]));
                        let _ = s.write_all(first_reply.as_bytes()).await;
                        let mut buf = [0u8; 256];
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(m) => {
                                    let reply =
                                        format!("{label}:{}", String::from_utf8_lossy(&buf[..m]));
                                    if s.write_all(reply.as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        let _ = closed_inner.send(());
                    }
                });
            }
        });

        Self {
            addr,
            hits,
            tunnel_closed_rx: closed_rx,
            task,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    async fn wait_tunnel_closed(&mut self, label: &str) {
        tokio::time::timeout(Duration::from_secs(2), self.tunnel_closed_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for SOCKS5 tunnel close"))
            .expect("tunnel_closed channel closed unexpectedly");
    }
}

impl Drop for MockSocks5Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct MockDirectOrigin {
    addr: SocketAddr,
    http_hits: Arc<AtomicUsize>,
    tunnel_hits: Arc<AtomicUsize>,
    seen_http_heads: Arc<Mutex<Vec<String>>>,
    seen_post_bodies: Arc<Mutex<Vec<String>>>,
    tunnel_closed_rx: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl MockDirectOrigin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let http_hits = Arc::new(AtomicUsize::new(0));
        let tunnel_hits = Arc::new(AtomicUsize::new(0));
        let seen_http_heads = Arc::new(Mutex::new(Vec::new()));
        let seen_post_bodies = Arc::new(Mutex::new(Vec::new()));
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();

        let http_hits_c = Arc::clone(&http_hits);
        let tunnel_hits_c = Arc::clone(&tunnel_hits);
        let heads_c = Arc::clone(&seen_http_heads);
        let bodies_c = Arc::clone(&seen_post_bodies);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let http_hits_i = Arc::clone(&http_hits_c);
                let tunnel_hits_i = Arc::clone(&tunnel_hits_c);
                let heads_i = Arc::clone(&heads_c);
                let bodies_i = Arc::clone(&bodies_c);
                let closed_i = closed_tx.clone();
                tokio::spawn(async move {
                    let mut first = [0u8; 4];
                    let n = match s.read(&mut first).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    if first[..n].starts_with(b"GET ") || first[..n].starts_with(b"POST") {
                        http_hits_i.fetch_add(1, Ordering::SeqCst);
                        let mut buf = first[..n].to_vec();
                        let mut tmp = [0u8; 512];
                        let head_end = loop {
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break pos;
                            }
                            let m = s.read(&mut tmp).await.unwrap();
                            assert!(m > 0);
                            buf.extend_from_slice(&tmp[..m]);
                        };
                        let head_str = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                        heads_i.lock().push(head_str.clone());

                        let content_len = head_str
                            .split("\r\n")
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                if k.trim().eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        let mut body = buf[head_end + 4..].to_vec();
                        while body.len() < content_len {
                            let m = s.read(&mut tmp).await.unwrap();
                            assert!(m > 0);
                            body.extend_from_slice(&tmp[..m]);
                        }
                        if content_len > 0 {
                            bodies_i
                                .lock()
                                .push(String::from_utf8_lossy(&body[..content_len]).into_owned());
                        }
                        let resp_body = if content_len > 0 {
                            format!(
                                "direct-post:{}",
                                String::from_utf8_lossy(&body[..content_len])
                            )
                        } else {
                            "direct-get-ok".to_string()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_body.len(),
                            resp_body
                        );
                        let _ = s.write_all(resp.as_bytes()).await;
                        let _ = s.shutdown().await;
                    } else {
                        tunnel_hits_i.fetch_add(1, Ordering::SeqCst);
                        let first_reply =
                            format!("direct-tunnel:{}", String::from_utf8_lossy(&first[..n]));
                        let _ = s.write_all(first_reply.as_bytes()).await;
                        let mut buf = [0u8; 256];
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(m) => {
                                    let reply = format!(
                                        "direct-tunnel:{}",
                                        String::from_utf8_lossy(&buf[..m])
                                    );
                                    if s.write_all(reply.as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        let _ = closed_i.send(());
                    }
                });
            }
        });

        Self {
            addr,
            http_hits,
            tunnel_hits,
            seen_http_heads,
            seen_post_bodies,
            tunnel_closed_rx: closed_rx,
            task,
        }
    }

    async fn wait_tunnel_closed(&mut self, label: &str) {
        tokio::time::timeout(Duration::from_secs(2), self.tunnel_closed_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for direct tunnel close"))
            .expect("tunnel_closed channel closed unexpectedly");
    }
}

impl Drop for MockDirectOrigin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn http_get_via_relay(relay_addr: SocketAddr, url: &str, host: &str) -> String {
    let mut client = TcpStream::connect(relay_addr).await.unwrap();
    let req = format!(
        "GET {url} HTTP/1.1\r\nHost: {host}\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic c3RhbGU6c3RhbGU=\r\n\r\n"
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut resp))
        .await
        .expect("HTTP GET through relay timed out")
        .unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

async fn open_connect_tunnel(relay_addr: SocketAddr, authority: &str) -> TcpStream {
    let mut client = TcpStream::connect(relay_addr).await.unwrap();
    let req = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n\r\n"
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let (head, _) = read_http_head(&mut client).await;
    let head_str = String::from_utf8_lossy(&head);
    assert!(
        head_str.starts_with("HTTP/1.1 200"),
        "expected 200 Connection Established, got: {head_str}"
    );
    client
}

async fn exchange_tunnel_msg(stream: &mut TcpStream, msg: &[u8], expected: &[u8]) {
    stream.write_all(msg).await.unwrap();
    stream.flush().await.unwrap();
    let mut got = vec![0u8; expected.len()];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut got))
        .await
        .expect("timed out reading tunnel echo")
        .unwrap();
    assert_eq!(
        got,
        expected,
        "unexpected tunnel payload: got {:?}, expected {:?}",
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(expected)
    );
}

#[tokio::test]
async fn plain_http_rotates_from_upstream_a_to_upstream_b() {
    let auth_a = format!("Basic {}", BASE64.encode("alice:secret-a"));
    let auth_b = format!("Basic {}", BASE64.encode("bob:secret-b"));
    let proxy_a = MockHttpProxy::start("up-a", Some(auth_a)).await;
    let proxy_b = MockHttpProxy::start("up-b", Some(auth_b)).await;

    let cfg_a = ProxyConfig {
        server: format!("http://alice:secret-a@{}", proxy_a.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };
    let cfg_b = ProxyConfig {
        server: format!("http://{}", proxy_b.addr),
        username: Some("bob".to_string()),
        password: Some("secret-b".to_string()),
        bypass: Vec::new(),
    };

    let relay = ProxyRelay::start(&cfg_a).await.unwrap();
    assert!(relay.local_addr().ip().is_loopback());
    assert!(!relay.is_direct());
    assert_eq!(relay.generation(), 0);

    let r1 = http_get_via_relay(
        relay.local_addr(),
        "http://example.test/one",
        "example.test",
    )
    .await;
    assert!(r1.ends_with("\r\n\r\nproxy=up-a"), "unexpected r1: {r1}");
    assert_eq!(proxy_a.hits(), 1);
    assert_eq!(proxy_b.hits(), 0);

    relay.swap_upstream(Some(&cfg_b)).unwrap();
    assert_eq!(relay.generation(), 1);
    assert!(!relay.is_direct());

    let r2 = http_get_via_relay(
        relay.local_addr(),
        "http://example.test/two",
        "example.test",
    )
    .await;
    assert!(r2.ends_with("\r\n\r\nproxy=up-b"), "unexpected r2: {r2}");
    assert_eq!(proxy_a.hits(), 1);
    assert_eq!(proxy_b.hits(), 1);

    // Swap back to Upstream A to verify repeatable rotation.
    relay.swap_upstream(Some(&cfg_a)).unwrap();
    assert_eq!(relay.generation(), 2);

    let r3 = http_get_via_relay(
        relay.local_addr(),
        "http://example.test/three",
        "example.test",
    )
    .await;
    assert!(r3.ends_with("\r\n\r\nproxy=up-a"), "unexpected r3: {r3}");
    assert_eq!(proxy_a.hits(), 2);
    assert_eq!(proxy_b.hits(), 1);

    // Verify client's stale Proxy-Authorization/Proxy-Connection were stripped and replaced.
    let heads_a = proxy_a.seen_heads.lock().clone();
    let heads_b = proxy_b.seen_heads.lock().clone();
    assert_eq!(heads_a.len(), 2);
    assert_eq!(heads_b.len(), 1);
    for head in heads_a.iter().chain(heads_b.iter()) {
        assert!(
            !head.contains("c3RhbGU6c3RhbGU="),
            "stale client Proxy-Authorization leaked to upstream: {head}"
        );
        let lower = head.to_ascii_lowercase();
        assert!(!lower.contains("proxy-connection:"));
        assert!(lower.contains("connection: close"));
    }
}

#[tokio::test]
async fn active_connect_tunnel_flushes_on_swap_and_fresh_connect_routes_to_upstream_b() {
    let auth_a = format!("Basic {}", BASE64.encode("ua:pa"));
    let auth_b = format!("Basic {}", BASE64.encode("ub:pb"));
    let mut proxy_a = MockHttpProxy::start("up-a", Some(auth_a)).await;
    let mut proxy_b = MockHttpProxy::start("up-b", Some(auth_b)).await;

    let cfg_a = ProxyConfig {
        server: format!("http://ua:pa@{}", proxy_a.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };
    let cfg_b = ProxyConfig {
        server: format!("http://ub:pb@{}", proxy_b.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };

    let relay = ProxyRelay::start(&cfg_a).await.unwrap();

    // Open active CONNECT tunnel through Upstream A and confirm live bidirectional traffic.
    let mut tunnel_a = open_connect_tunnel(relay.local_addr(), "secure.test:443").await;
    exchange_tunnel_msg(&mut tunnel_a, b"ping", b"up-a:ping").await;
    assert_eq!(proxy_a.hits(), 1);
    assert_eq!(proxy_b.hits(), 0);

    // Swap upstream while CONNECT tunnel on A is still active.
    relay.swap_upstream(Some(&cfg_b)).unwrap();
    assert_eq!(relay.generation(), 1);

    // Both client and Upstream A must observe immediate tunnel termination.
    assert_closed_promptly(&mut tunnel_a, "client tunnel on Upstream A").await;
    proxy_a.wait_tunnel_closed("Upstream A tunnel").await;

    // A fresh CONNECT must now route exclusively through Upstream B.
    let mut tunnel_b = open_connect_tunnel(relay.local_addr(), "secure.test:443").await;
    exchange_tunnel_msg(&mut tunnel_b, b"next", b"up-b:next").await;
    assert_eq!(
        proxy_a.hits(),
        1,
        "Upstream A must not receive post-swap traffic"
    );
    assert_eq!(proxy_b.hits(), 1);

    drop(tunnel_b);
    proxy_b.wait_tunnel_closed("Upstream B tunnel").await;
}

#[tokio::test]
async fn multiple_concurrent_connect_tunnels_flush_simultaneously_on_swap() {
    let mut proxy_a = MockHttpProxy::start("up-a", None).await;
    let proxy_b = MockHttpProxy::start("up-b", None).await;

    let cfg_a = ProxyConfig {
        server: format!("http://{}", proxy_a.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };
    let cfg_b = ProxyConfig {
        server: format!("http://{}", proxy_b.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };

    let relay = ProxyRelay::start(&cfg_a).await.unwrap();

    let mut tunnels = Vec::new();
    for i in 0..3 {
        let mut t = open_connect_tunnel(relay.local_addr(), "pool.test:443").await;
        let msg = format!("t{i}");
        let expected = format!("up-a:t{i}");
        exchange_tunnel_msg(&mut t, msg.as_bytes(), expected.as_bytes()).await;
        tunnels.push(t);
    }
    assert_eq!(proxy_a.hits(), 3);

    relay.swap_upstream(Some(&cfg_b)).unwrap();

    for (i, t) in tunnels.iter_mut().enumerate() {
        assert_closed_promptly(t, &format!("concurrent tunnel #{i}")).await;
    }
    for i in 0..3 {
        proxy_a
            .wait_tunnel_closed(&format!("upstream concurrent tunnel #{i}"))
            .await;
    }

    let mut fresh = open_connect_tunnel(relay.local_addr(), "pool.test:443").await;
    exchange_tunnel_msg(&mut fresh, b"ok", b"up-b:ok").await;
    assert_eq!(proxy_a.hits(), 3);
    assert_eq!(proxy_b.hits(), 1);
}

#[tokio::test]
async fn direct_http_and_socks5_full_rotation_cycle() {
    let mut origin = MockDirectOrigin::start().await;
    let http_auth = format!("Basic {}", BASE64.encode("huser:hpass"));
    let mut http_proxy = MockHttpProxy::start("http-up", Some(http_auth)).await;
    let mut socks5_proxy = MockSocks5Proxy::start("s5-up", "suser", "spass").await;

    let http_cfg = ProxyConfig {
        server: format!("http://huser:hpass@{}", http_proxy.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };
    let socks5_cfg = ProxyConfig {
        server: format!("socks5h://suser:spass@{}", socks5_proxy.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };

    // Phase 1: Start in Direct mode
    let relay = ProxyRelay::start_direct().await.unwrap();
    assert!(relay.is_direct());
    assert_eq!(relay.generation(), 0);

    let origin_url = format!("http://{}/items?limit=5", origin.addr);
    let get_resp =
        http_get_via_relay(relay.local_addr(), &origin_url, &origin.addr.to_string()).await;
    assert!(
        get_resp.ends_with("\r\n\r\ndirect-get-ok"),
        "direct GET failed: {get_resp}"
    );
    {
        let heads = origin.seen_http_heads.lock();
        assert_eq!(heads.len(), 1);
        assert!(
            heads[0].starts_with("GET /items?limit=5 HTTP/1.1"),
            "origin must receive origin-form path+query: {}",
            heads[0]
        );
        let lower = heads[0].to_ascii_lowercase();
        assert!(
            !lower.contains("proxy-authorization:") && !lower.contains("proxy-connection:"),
            "proxy headers must be stripped in Direct mode: {}",
            heads[0]
        );
        assert!(lower.contains("connection: close"));
    }

    // Verify Direct POST with body
    let mut post_client = TcpStream::connect(relay.local_addr()).await.unwrap();
    let post_req = format!(
        "POST http://{}/submit HTTP/1.1\r\nHost: {}\r\nContent-Length: 11\r\n\r\npayload-123",
        origin.addr, origin.addr
    );
    post_client.write_all(post_req.as_bytes()).await.unwrap();
    let mut post_resp = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        post_client.read_to_end(&mut post_resp),
    )
    .await
    .expect("direct POST timed out")
    .unwrap();
    assert!(
        String::from_utf8_lossy(&post_resp).ends_with("\r\n\r\ndirect-post:payload-123"),
        "unexpected direct POST response: {}",
        String::from_utf8_lossy(&post_resp)
    );
    assert_eq!(origin.seen_post_bodies.lock().as_slice(), &["payload-123"]);

    // Verify Direct CONNECT tunnel directly to destination
    let mut direct_tunnel = open_connect_tunnel(relay.local_addr(), &origin.addr.to_string()).await;
    exchange_tunnel_msg(&mut direct_tunnel, b"hi", b"direct-tunnel:hi").await;
    assert_eq!(origin.tunnel_hits.load(Ordering::SeqCst), 1);

    // Phase 2: Rotate Direct -> Authenticated HTTP proxy
    relay.swap_upstream(Some(&http_cfg)).unwrap();
    assert!(!relay.is_direct());
    assert_eq!(relay.generation(), 1);

    assert_closed_promptly(
        &mut direct_tunnel,
        "direct CONNECT tunnel after swap to HTTP",
    )
    .await;
    origin.wait_tunnel_closed("direct origin tunnel").await;

    let http_resp =
        http_get_via_relay(relay.local_addr(), "http://target.test/a", "target.test").await;
    assert!(http_resp.ends_with("\r\n\r\nproxy=http-up"));
    let mut http_tunnel = open_connect_tunnel(relay.local_addr(), "target.test:443").await;
    exchange_tunnel_msg(&mut http_tunnel, b"h1", b"http-up:h1").await;
    assert_eq!(http_proxy.hits(), 2);

    // Phase 3: Rotate Authenticated HTTP proxy -> Authenticated SOCKS5 proxy
    relay.swap_upstream(Some(&socks5_cfg)).unwrap();
    assert!(!relay.is_direct());
    assert_eq!(relay.generation(), 2);

    assert_closed_promptly(&mut http_tunnel, "HTTP CONNECT tunnel after swap to SOCKS5").await;
    http_proxy.wait_tunnel_closed("HTTP proxy tunnel").await;

    let s5_http =
        http_get_via_relay(relay.local_addr(), "http://target.test/b", "target.test").await;
    assert!(
        s5_http.ends_with("\r\n\r\nsocks5=s5-up"),
        "unexpected SOCKS5 HTTP: {s5_http}"
    );
    let mut s5_tunnel = open_connect_tunnel(relay.local_addr(), "target.test:443").await;
    exchange_tunnel_msg(&mut s5_tunnel, b"s1", b"s5-up:s1").await;
    assert_eq!(socks5_proxy.hits(), 2);

    // Phase 4: Rotate Authenticated SOCKS5 proxy -> Direct
    relay.swap_upstream(None).unwrap();
    assert!(relay.is_direct());
    assert_eq!(relay.generation(), 3);

    assert_closed_promptly(&mut s5_tunnel, "SOCKS5 CONNECT tunnel after swap to Direct").await;
    socks5_proxy.wait_tunnel_closed("SOCKS5 proxy tunnel").await;

    let back_direct =
        http_get_via_relay(relay.local_addr(), &origin_url, &origin.addr.to_string()).await;
    assert!(back_direct.ends_with("\r\n\r\ndirect-get-ok"));
    let mut direct_tunnel_2 =
        open_connect_tunnel(relay.local_addr(), &origin.addr.to_string()).await;
    exchange_tunnel_msg(&mut direct_tunnel_2, b"ok", b"direct-tunnel:ok").await;

    assert_eq!(origin.http_hits.load(Ordering::SeqCst), 3);
    assert_eq!(origin.tunnel_hits.load(Ordering::SeqCst), 2);
    assert_eq!(http_proxy.hits(), 2);
    assert_eq!(socks5_proxy.hits(), 2);
}

#[tokio::test]
async fn invalid_swap_rejects_atomically_without_mutating_upstream_or_dropping_active_tunnel() {
    let auth_a = format!("Basic {}", BASE64.encode("keep:alive"));
    let proxy_a = MockHttpProxy::start("stable-a", Some(auth_a)).await;
    let cfg_a = ProxyConfig {
        server: format!("http://keep:alive@{}", proxy_a.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };

    let relay = ProxyRelay::start_rotatable(Some(&cfg_a)).await.unwrap();
    assert!(!relay.is_direct());
    assert_eq!(relay.generation(), 0);

    // Open active CONNECT tunnel on Upstream A before attempting invalid swaps.
    let mut tunnel = open_connect_tunnel(relay.local_addr(), "stable.test:443").await;
    exchange_tunnel_msg(&mut tunnel, b"pre", b"stable-a:pre").await;

    let invalid_cases: &[(&str, &str)] = &[
        ("https upstream", "https://127.0.0.1:8443"),
        ("socks4 upstream", "socks4://127.0.0.1:1080"),
        ("ftp scheme", "ftp://127.0.0.1:21"),
        ("empty url", ""),
        ("whitespace only", "   "),
        ("missing host", "http://"),
        ("port zero", "http://127.0.0.1:0"),
        ("embedded whitespace", "http://bad host:8080"),
        ("malformed ipv6", "http://[::1"),
        ("out-of-range port", "http://127.0.0.1:99999"),
    ];

    for (name, bad_server) in invalid_cases {
        let bad = ProxyConfig {
            server: (*bad_server).to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        let err = relay
            .swap_upstream(Some(&bad))
            .expect_err(&format!("expected error for {name} ({bad_server})"));
        if *bad_server == "https://127.0.0.1:8443" || *bad_server == "socks4://127.0.0.1:1080" {
            assert!(
                matches!(err, MimikyuError::Unsupported { .. }),
                "expected Unsupported for {name}, got {err:?}"
            );
        }
        assert_eq!(
            relay.generation(),
            0,
            "generation must not change on rejected swap ({name})"
        );
        assert!(
            !relay.is_direct(),
            "upstream must remain non-direct after rejected swap ({name})"
        );
    }

    // Active CONNECT tunnel must STILL be alive and passing bytes uninterrupted!
    exchange_tunnel_msg(&mut tunnel, b"post", b"stable-a:post").await;

    // Subsequent HTTP requests must STILL route through Upstream A.
    let resp = http_get_via_relay(relay.local_addr(), "http://stable.test/", "stable.test").await;
    assert!(resp.ends_with("\r\n\r\nproxy=stable-a"));
    assert_eq!(proxy_a.hits(), 2);
}

#[tokio::test]
async fn upgraded_101_websocket_stream_flushes_on_swap() {
    let mut proxy_a = MockHttpProxy::start("ws-a", None).await;
    let cfg_a = ProxyConfig {
        server: format!("http://{}", proxy_a.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };

    let relay = ProxyRelay::start(&cfg_a).await.unwrap();
    let mut client = TcpStream::connect(relay.local_addr()).await.unwrap();
    client
        .write_all(
            b"GET http://ws.test/socket HTTP/1.1\r\nHost: ws.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        )
        .await
        .unwrap();
    let (head, _) = read_http_head(&mut client).await;
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"),
        "expected 101 Switching Protocols"
    );

    exchange_tunnel_msg(&mut client, b"hey", b"ws-a-ws:hey").await;

    // Swapping to Direct must flush the active upgraded 101 stream immediately.
    relay.swap_upstream(None).unwrap();
    assert_closed_promptly(&mut client, "upgraded 101 client").await;
    proxy_a.wait_tunnel_closed("upgraded 101 upstream").await;
}

#[tokio::test]
async fn start_rotatable_none_synthesizes_host_header_in_direct_mode() {
    let origin = MockDirectOrigin::start().await;
    let relay = ProxyRelay::start_rotatable(None).await.unwrap();
    assert!(relay.is_direct());
    assert!(relay.local_addr().ip().is_loopback());

    let mut client = TcpStream::connect(relay.local_addr()).await.unwrap();
    let req = format!("GET http://{}/no-host HTTP/1.1\r\n\r\n", origin.addr);
    client.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut resp))
        .await
        .expect("direct request without Host header timed out")
        .unwrap();
    assert!(String::from_utf8_lossy(&resp).ends_with("\r\n\r\ndirect-get-ok"));

    let heads = origin.seen_http_heads.lock();
    assert_eq!(heads.len(), 1);
    assert!(
        heads[0].contains(&format!("Host: {}", origin.addr)),
        "relay should synthesize Host header when missing: {}",
        heads[0]
    );
}

#[tokio::test]
async fn concurrent_readers_and_swaps_never_deadlock_or_panic() {
    let proxy_a = MockHttpProxy::start("race-a", None).await;
    let proxy_b = MockHttpProxy::start("race-b", None).await;
    let cfg_a = ProxyConfig {
        server: format!("http://{}", proxy_a.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };
    let cfg_b = ProxyConfig {
        server: format!("http://{}", proxy_b.addr),
        username: None,
        password: None,
        bypass: Vec::new(),
    };

    let relay = Arc::new(ProxyRelay::start(&cfg_a).await.unwrap());
    let done = Arc::new(Notify::new());

    let relay_swap = Arc::clone(&relay);
    let done_swap = Arc::clone(&done);
    let swapper = tokio::spawn(async move {
        for i in 0..20 {
            let target = if i % 2 == 0 {
                Some(&cfg_b)
            } else {
                Some(&cfg_a)
            };
            relay_swap.swap_upstream(target).unwrap();
            tokio::task::yield_now().await;
        }
        done_swap.notify_one();
    });

    let relay_req = Arc::clone(&relay);
    let requester = tokio::spawn(async move {
        for _ in 0..20 {
            if let Ok(mut c) = TcpStream::connect(relay_req.local_addr()).await {
                let _ = c
                    .write_all(b"GET http://race.test/ HTTP/1.1\r\nHost: race.test\r\n\r\n")
                    .await;
                let mut sink = Vec::new();
                let _ = tokio::time::timeout(Duration::from_millis(500), c.read_to_end(&mut sink))
                    .await;
            }
        }
    });

    swapper.await.unwrap();
    requester.await.unwrap();
    assert_eq!(relay.generation(), 20);

    // After rapid swaps settle on Upstream A (last i = 19, odd => cfg_a), traffic works cleanly.
    let final_resp =
        http_get_via_relay(relay.local_addr(), "http://race.test/final", "race.test").await;
    assert!(final_resp.ends_with("\r\n\r\nproxy=race-a"));
    let _ = done;
}
