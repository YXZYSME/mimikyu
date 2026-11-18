pub mod pages;
pub mod routes;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessOrigin {
    Primary,
    CrossOrigin,
}

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub enable_cross_origin: bool,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub keep_alive_timeout_secs: u64,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            enable_cross_origin: true,
            max_header_bytes: 64 * 1024,
            max_body_bytes: 2 * 1024 * 1024,
            keep_alive_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub origin: HarnessOrigin,
    pub method: String,
    pub path: String,
    pub raw_target: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body_len: usize,
}

#[derive(Debug, Clone)]
pub struct HarnessRequest {
    pub origin: HarnessOrigin,
    pub method: String,
    pub path: String,
    pub raw_target: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct HarnessResponse {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub extra_headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub force_close: bool,
}

pub struct HarnessState {
    pub requests: Mutex<Vec<RecordedRequest>>,
    pub cache_hits: AtomicUsize,
    pub submit_seq: AtomicUsize,
    pub cross_origin_base_url: Option<String>,
}

pub struct HarnessServer {
    primary_addr: SocketAddr,
    cross_origin_addr: Option<SocketAddr>,
    state: Arc<HarnessState>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl HarnessServer {
    pub async fn start() -> Result<Self> {
        Self::start_with_config(HarnessConfig::default()).await
    }

    pub async fn start_with_config(cfg: HarnessConfig) -> Result<Self> {
        let primary_listener = TcpListener::bind("127.0.0.1:0").await?;
        let primary_addr = primary_listener.local_addr()?;

        let (cross_listener, cross_origin_addr) = if cfg.enable_cross_origin {
            let l = TcpListener::bind("127.0.0.1:0").await?;
            let a = l.local_addr()?;
            (Some(l), Some(a))
        } else {
            (None, None)
        };

        let cross_origin_base_url = cross_origin_addr.map(|a| format!("http://{a}"));
        let state = Arc::new(HarnessState {
            requests: Mutex::new(Vec::new()),
            cache_hits: AtomicUsize::new(0),
            submit_seq: AtomicUsize::new(0),
            cross_origin_base_url,
        });

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut tasks = Vec::with_capacity(2);

        tasks.push(spawn_accept_loop(
            primary_listener,
            HarnessOrigin::Primary,
            Arc::clone(&state),
            cfg.clone(),
            shutdown_rx.clone(),
        ));

        if let Some(listener) = cross_listener {
            tasks.push(spawn_accept_loop(
                listener,
                HarnessOrigin::CrossOrigin,
                Arc::clone(&state),
                cfg,
                shutdown_rx,
            ));
        }

        Ok(Self {
            primary_addr,
            cross_origin_addr,
            state,
            shutdown_tx,
            tasks,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.primary_addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.primary_addr)
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("http://{}{}", self.primary_addr, path)
        } else {
            format!("http://{}/{}", self.primary_addr, path)
        }
    }

    pub fn localhost_url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("http://localhost:{}{}", self.primary_addr.port(), path)
        } else {
            format!("http://localhost:{}/{}", self.primary_addr.port(), path)
        }
    }

    pub fn cross_origin_addr(&self) -> Option<SocketAddr> {
        self.cross_origin_addr
    }

    pub fn cross_origin_base_url(&self) -> Option<String> {
        self.cross_origin_addr.map(|a| format!("http://{a}"))
    }

    pub fn cross_origin_url(&self, path: &str) -> Option<String> {
        self.cross_origin_addr.map(|a| {
            if path.starts_with('/') {
                format!("http://{a}{path}")
            } else {
                format!("http://{a}/{path}")
            }
        })
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().clone()
    }

    pub fn clear_requests(&self) {
        self.state.requests.lock().clear();
    }

    pub fn reset_counters(&self) {
        self.state.cache_hits.store(0, Ordering::SeqCst);
        self.state.submit_seq.store(0, Ordering::SeqCst);
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        for handle in self.tasks.drain(..) {
            let mut h = handle;
            if tokio::time::timeout(Duration::from_millis(500), &mut h)
                .await
                .is_err()
            {
                h.abort();
                let _ = h.await;
            }
        }
    }
}

impl Drop for HarnessServer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        for handle in self.tasks.drain(..) {
            handle.abort();
        }
    }
}

use std::sync::atomic::Ordering;

fn spawn_accept_loop(
    listener: TcpListener,
    origin: HarnessOrigin,
    state: Arc<HarnessState>,
    cfg: HarnessConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, _)) => {
                            let st = Arc::clone(&state);
                            let c = cfg.clone();
                            let rx = shutdown_rx.clone();
                            tokio::spawn(async move {
                                serve_connection(stream, origin, st, c, rx).await;
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    })
}

enum HeadRead {
    Ok(usize),
    Eof,
    TooLarge,
    Shutdown,
}

async fn read_http_head(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    max_header_bytes: usize,
    keep_alive_timeout: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> HeadRead {
    let mut tmp = [0u8; 2048];
    loop {
        if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
            return HeadRead::Ok(pos);
        }
        if buf.len() > max_header_bytes {
            return HeadRead::TooLarge;
        }
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return HeadRead::Shutdown;
                }
            }
            res = tokio::time::timeout(keep_alive_timeout, stream.read(&mut tmp)) => {
                match res {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return HeadRead::Eof,
                    Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                }
            }
        }
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    origin: HarnessOrigin,
    state: Arc<HarnessState>,
    cfg: HarnessConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut buf = Vec::with_capacity(4096);
    let keep_alive_timeout = Duration::from_secs(cfg.keep_alive_timeout_secs.max(1));

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let header_end = match read_http_head(
            &mut stream,
            &mut buf,
            cfg.max_header_bytes,
            keep_alive_timeout,
            &mut shutdown_rx,
        )
        .await
        {
            HeadRead::Ok(pos) => pos,
            HeadRead::TooLarge => {
                let _ = write_simple_error(&mut stream, 431, "Request Header Fields Too Large").await;
                break;
            }
            HeadRead::Eof | HeadRead::Shutdown => break,
        };

        let header_bytes = &buf[..header_end];
        let header_text = String::from_utf8_lossy(header_bytes).into_owned();
        buf.drain(..header_end + 4);

        let mut lines = header_text.split("\r\n");
        let req_line = lines.next().unwrap_or("");
        let mut parts = req_line.split_whitespace();
        let Some(method_raw) = parts.next() else {
            let _ = write_simple_error(&mut stream, 400, "Bad Request").await;
            break;
        };
        let Some(raw_target_raw) = parts.next() else {
            let _ = write_simple_error(&mut stream, 400, "Bad Request").await;
            break;
        };
        let version = parts.next().unwrap_or("HTTP/1.1").to_string();
        let method = method_raw.to_ascii_uppercase();
        let raw_target = raw_target_raw.to_string();

        let (path_raw, query_str) = match raw_target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (raw_target.as_str(), ""),
        };
        let path = path_raw.to_string();
        let mut query = BTreeMap::new();
        if !query_str.is_empty() {
            for (k, v) in url::form_urlencoded::parse(query_str.as_bytes()) {
                query.insert(k.into_owned(), v.into_owned());
            }
        }

        let mut headers = BTreeMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        if headers
            .get("expect")
            .is_some_and(|v| v.to_ascii_lowercase().contains("100-continue"))
            && stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.is_err()
        {
            break;
        }

        if headers
            .get("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            let _ = write_simple_error(&mut stream, 501, "Not Implemented").await;
            break;
        }

        let content_len = match headers.get("content-length") {
            Some(raw_cl) => match raw_cl.parse::<usize>() {
                Ok(n) => n,
                Err(_) => {
                    let _ = write_simple_error(&mut stream, 400, "Bad Request").await;
                    break;
                }
            },
            None => 0,
        };

        if content_len > cfg.max_body_bytes {
            let _ = write_simple_error(&mut stream, 413, "Payload Too Large").await;
            break;
        }

        let mut body = Vec::with_capacity(content_len);
        if content_len > 0 {
            let take = content_len.min(buf.len());
            body.extend_from_slice(&buf[..take]);
            buf.drain(..take);
            while body.len() < content_len {
                let remaining = content_len - body.len();
                let mut chunk = vec![0u8; remaining.min(16 * 1024)];
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return;
                        }
                    }
                    read_res = stream.read(&mut chunk) => {
                        match read_res {
                            Ok(0) | Err(_) => return,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                }
            }
        }

        state.requests.lock().push(RecordedRequest {
            origin,
            method: method.clone(),
            path: path.clone(),
            raw_target: raw_target.clone(),
            query: query.clone(),
            headers: headers.clone(),
            body_len: body.len(),
        });

        if origin == HarnessOrigin::Primary && method == "GET" && path == "/sse-stream" {
            let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\ndata: hello\n\n";
            if stream.write_all(hdr.as_bytes()).await.is_ok() {
                let _ = stream.flush().await;
                tokio::select! {
                    _ = shutdown_rx.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(3000)) => {
                        let _ = stream.write_all(b"data: done\n\n").await;
                        let _ = stream.flush().await;
                    }
                }
            }
            let _ = stream.shutdown().await;
            break;
        }

        let req = HarnessRequest {
            origin,
            method: method.clone(),
            path,
            raw_target,
            query,
            headers: headers.clone(),
            body,
        };
        let resp = routes::dispatch(&req, &state).await;

        let conn_hdr = headers
            .get("connection")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let keep_alive = if resp.force_close || conn_hdr.contains("close") {
            false
        } else if version == "HTTP/1.0" {
            conn_hdr.contains("keep-alive")
        } else {
            true
        };

        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            resp.status,
            resp.reason,
            resp.content_type,
            resp.body.len()
        );
        if keep_alive {
            head.push_str(&format!(
                "Connection: keep-alive\r\nKeep-Alive: timeout={}\r\n",
                cfg.keep_alive_timeout_secs
            ));
        } else {
            head.push_str("Connection: close\r\n");
        }
        for (k, v) in &resp.extra_headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");

        if stream.write_all(head.as_bytes()).await.is_err() {
            break;
        }
        if method != "HEAD"
            && !matches!(resp.status, 204 | 304)
            && !resp.body.is_empty()
            && stream.write_all(&resp.body).await.is_err()
        {
            break;
        }
        if stream.flush().await.is_err() {
            break;
        }

        if !keep_alive {
            let _ = stream.shutdown().await;
            break;
        }
    }
}

async fn write_simple_error(stream: &mut TcpStream, status: u16, reason: &str) -> std::io::Result<()> {
    let body = reason.as_bytes();
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(msg.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    stream.shutdown().await
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
