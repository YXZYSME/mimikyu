use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tracing::debug;

use crate::driver::ProxyConfig;
use crate::error::{MimikyuError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpstreamKind {
    Direct,
    Http {
        host: String,
        port: u16,
        auth_header: Option<String>,
    },
    Socks5 {
        host: String,
        port: u16,
        credentials: Option<(String, String)>,
    },
}

#[derive(Debug, Clone)]
struct UpstreamState {
    generation: u64,
    upstream: Arc<UpstreamKind>,
}

/// Local loopback HTTP/CONNECT relay supporting direct forwarding and live-swappable upstream
/// HTTP or SOCKS5 proxies.
pub struct ProxyRelay {
    local_addr: SocketAddr,
    state_tx: watch::Sender<UpstreamState>,
    task: JoinHandle<()>,
}

impl ProxyRelay {
    /// Start a loopback HTTP/CONNECT relay forwarding to `proxy`.
    ///
    /// The relay's upstream target can later be rotated live via [`Self::swap_upstream`].
    pub async fn start(proxy: &ProxyConfig) -> Result<Self> {
        let upstream = resolve_upstream(Some(proxy))?;
        Self::start_with_upstream(upstream).await
    }

    /// Start a loopback HTTP/CONNECT relay initialized in direct-forwarding mode.
    ///
    /// Plain HTTP and HTTPS `CONNECT` traffic is forwarded directly to the destination `host:port`
    /// until rotated via [`Self::swap_upstream`].
    pub async fn start_direct() -> Result<Self> {
        Self::start_with_upstream(UpstreamKind::Direct).await
    }

    /// Start a loopback HTTP/CONNECT relay initialized with `initial` (or direct mode when `None`),
    /// ready for live upstream rotation via [`Self::swap_upstream`].
    pub async fn start_rotatable(initial: Option<&ProxyConfig>) -> Result<Self> {
        let upstream = resolve_upstream(initial)?;
        Self::start_with_upstream(upstream).await
    }

    async fn start_with_upstream(initial: UpstreamKind) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        debug_assert!(local_addr.ip().is_loopback());

        let (state_tx, mut accept_rx) = watch::channel(UpstreamState {
            generation: 0,
            upstream: Arc::new(initial),
        });

        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut accept_rx) => {
                        break;
                    }
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else {
                            break;
                        };
                        while conns.try_join_next().is_some() {}
                        let conn_rx = accept_rx.clone();
                        conns.spawn(async move {
                            if let Err(e) = handle_client(client, conn_rx).await {
                                debug!(target: "mimikyu::proxy_relay", "relay connection ended: {e}");
                            }
                        });
                    }
                }
            }
            conns.abort_all();
            while conns.join_next().await.is_some() {}
        });

        Ok(Self {
            local_addr,
            state_tx,
            task,
        })
    }

    /// Atomically swap the active upstream proxy (or switch to direct forwarding when `None`).
    ///
    /// Validates and parses `new_proxy` before mutating state: invalid configs or unsupported
    /// schemes return an error without modifying the active upstream or dropping active tunnels.
    /// On success, increments the upstream generation so active `CONNECT` tunnels bound to the
    /// previous upstream abort immediately.
    pub fn swap_upstream(&self, new_proxy: Option<&ProxyConfig>) -> Result<()> {
        let next = resolve_upstream(new_proxy)?;
        self.state_tx.send_modify(|state| {
            state.generation = state.generation.wrapping_add(1);
            state.upstream = Arc::new(next);
        });
        Ok(())
    }

    /// Returns `true` when the relay is currently forwarding directly without an upstream proxy.
    pub fn is_direct(&self) -> bool {
        matches!(*self.state_tx.borrow().upstream, UpstreamKind::Direct)
    }

    /// Returns the monotonic upstream generation counter (incremented on every successful swap).
    pub fn generation(&self) -> u64 {
        self.state_tx.borrow().generation
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn browser_proxy_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    pub fn browser_proxy_config(&self, bypass: Vec<String>) -> ProxyConfig {
        ProxyConfig {
            server: self.browser_proxy_url(),
            username: None,
            password: None,
            bypass,
        }
    }
}

impl Drop for ProxyRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn strip_ipv6_brackets(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
        .to_string()
}

fn parse_upstream(proxy: &ProxyConfig) -> Result<UpstreamKind> {
    let parsed = crate::driver::parse_proxy_url(&proxy.server)?;

    let host = parsed
        .host_str()
        .map(strip_ipv6_brackets)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            MimikyuError::InvalidConfig(format!("proxy missing host: {}", proxy.server))
        })?;

    let scheme = parsed.scheme().to_ascii_lowercase();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(match scheme.as_str() {
            "socks" | "socks4" | "socks5" | "socks5h" => 1080,
            _ => 8080,
        });

    let username = proxy
        .username
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let u = parsed.username();
            if u.is_empty() {
                None
            } else {
                Some(percent_decode(u))
            }
        });
    let password = proxy
        .password
        .clone()
        .or_else(|| parsed.password().map(percent_decode));

    match scheme.as_str() {
        "http" => {
            let auth_header = username.map(|u| {
                let pass = password.unwrap_or_default();
                let token = BASE64.encode(format!("{u}:{pass}"));
                format!("Basic {token}")
            });
            Ok(UpstreamKind::Http {
                host,
                port,
                auth_header,
            })
        }
        "https" => Err(MimikyuError::Unsupported {
            engine: "proxy_relay".to_string(),
            feature: "TLS-encrypted upstream proxy hop (https://); use http:// or socks5:// proxy"
                .to_string(),
        }),
        "socks" | "socks5" | "socks5h" => {
            let credentials = username.map(|u| (u, password.unwrap_or_default()));
            Ok(UpstreamKind::Socks5 {
                host,
                port,
                credentials,
            })
        }
        "socks4" => Err(MimikyuError::Unsupported {
            engine: "proxy_relay".to_string(),
            feature: "SOCKS4 authenticated relay".to_string(),
        }),
        other => Err(MimikyuError::InvalidConfig(format!(
            "unsupported proxy scheme '{other}'"
        ))),
    }
}

fn resolve_upstream(proxy: Option<&ProxyConfig>) -> Result<UpstreamKind> {
    let Some(proxy) = proxy else {
        return Ok(UpstreamKind::Direct);
    };
    let trimmed = proxy.server.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(MimikyuError::InvalidConfig(
            "proxy URL cannot be empty or contain whitespace/control characters".to_string(),
        ));
    }
    let upstream = parse_upstream(proxy)?;
    match &upstream {
        UpstreamKind::Http { port: 0, .. } | UpstreamKind::Socks5 { port: 0, .. } => {
            return Err(MimikyuError::InvalidConfig(
                "proxy port cannot be 0".to_string(),
            ));
        }
        UpstreamKind::Socks5 {
            credentials: Some((u, p)),
            ..
        } if u.len() > 255 || p.len() > 255 => {
            return Err(MimikyuError::InvalidConfig(
                "SOCKS5 username/password exceeds 255 bytes".to_string(),
            ));
        }
        UpstreamKind::Direct | UpstreamKind::Http { .. } | UpstreamKind::Socks5 { .. } => {}
    }
    Ok(upstream)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hop_by_hop_or_proxy_auth_header(line: &str, preserve_upgrade_conn: bool) -> bool {
    let Some((name, _)) = line.split_once(':') else {
        return false;
    };
    let lower = name.trim().to_ascii_lowercase();
    if preserve_upgrade_conn && lower == "connection" {
        return false;
    }
    matches!(
        lower.as_str(),
        "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
    )
}

fn is_connection_header(line: &str) -> bool {
    let Some((name, _)) = line.split_once(':') else {
        return false;
    };
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "connection" | "proxy-connection" | "keep-alive"
    )
}

async fn read_http_head<R: AsyncRead + Unpin>(stream: &mut R) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(2048);
    let head_end = read_head_into_buf(stream, &mut buf).await?;
    let head = buf[..head_end].to_vec();
    let leftover = buf[head_end + 4..].to_vec();
    Ok((head, leftover))
}

async fn read_head_into_buf<R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut Vec<u8>,
) -> Result<usize> {
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Ok(pos);
    }
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(MimikyuError::Transport(
                "unexpected EOF reading proxy HTTP head".to_string(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(pos);
        }
        if buf.len() > 65536 {
            return Err(MimikyuError::Transport(
                "proxy HTTP head exceeded 64 KiB".to_string(),
            ));
        }
    }
}

async fn read_crlf_line<R: AsyncRead + Unpin>(
    buf: &mut Vec<u8>,
    stream: &mut R,
) -> Result<Vec<u8>> {
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line: Vec<u8> = buf.drain(..pos + 2).collect();
            return Ok(line);
        }
        if buf.len() > 65536 {
            return Err(MimikyuError::Transport(
                "HTTP chunked line exceeded 64 KiB".to_string(),
            ));
        }
        let mut tmp = [0u8; 1024];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(MimikyuError::Transport(
                "unexpected EOF reading chunked HTTP body".to_string(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn forward_exact_bytes<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    buf: &mut Vec<u8>,
    stream: &mut R,
    writer: &mut W,
    mut remaining: usize,
) -> Result<()> {
    while remaining > 0 {
        if !buf.is_empty() {
            let take = buf.len().min(remaining);
            writer.write_all(&buf[..take]).await?;
            buf.drain(..take);
            remaining -= take;
        } else {
            let mut tmp = [0u8; 8192];
            let max_read = tmp.len().min(remaining);
            let n = stream.read(&mut tmp[..max_read]).await?;
            if n == 0 {
                return Err(MimikyuError::Transport(
                    "unexpected EOF reading Content-Length HTTP body".to_string(),
                ));
            }
            writer.write_all(&tmp[..n]).await?;
            remaining -= n;
        }
    }
    Ok(())
}

async fn forward_chunked_body<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    buf: &mut Vec<u8>,
    stream: &mut R,
    writer: &mut W,
) -> Result<()> {
    loop {
        let line = read_crlf_line(buf, stream).await?;
        writer.write_all(&line).await?;
        let line_str = std::str::from_utf8(&line[..line.len() - 2]).map_err(|_| {
            MimikyuError::Transport("non-UTF8 chunked size line in HTTP response".to_string())
        })?;
        let size_hex = line_str.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(size_hex, 16).map_err(|_| {
            MimikyuError::Transport(format!("invalid chunk size '{size_hex}' in HTTP response"))
        })?;
        if chunk_size == 0 {
            loop {
                let trailer = read_crlf_line(buf, stream).await?;
                writer.write_all(&trailer).await?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }
        let to_copy = chunk_size
            .checked_add(2)
            .ok_or_else(|| MimikyuError::Transport("chunk size overflow".to_string()))?;
        forward_exact_bytes(buf, stream, writer, to_copy).await?;
    }
}

enum RelayOutcome {
    Completed,
    Upgraded,
}

async fn relay_http_response<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    req_method: &str,
    up_read: &mut R,
    client_write: &mut W,
) -> Result<RelayOutcome> {
    let mut buf = Vec::with_capacity(4096);
    for _ in 0..8 {
        let head_end = read_head_into_buf(up_read, &mut buf).await?;
        let head_str = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head_str.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok());

        if status_code == Some(101) {
            client_write.write_all(&buf[..head_end + 4]).await?;
            if buf.len() > head_end + 4 {
                client_write.write_all(&buf[head_end + 4..]).await?;
                buf.clear();
            }
            client_write.flush().await?;
            let _ = tokio::io::copy(up_read, client_write).await;
            return Ok(RelayOutcome::Upgraded);
        }

        if matches!(status_code, Some(100..=199)) {
            client_write.write_all(&buf[..head_end + 4]).await?;
            buf.drain(..head_end + 4);
            continue;
        }

        let mut is_chunked = false;
        let mut content_length: Option<usize> = None;
        let mut out = String::with_capacity(head_str.len() + 32);
        out.push_str(status_line);
        out.push_str("\r\n");

        for h in lines {
            if h.is_empty() {
                continue;
            }
            if is_connection_header(h) {
                continue;
            }
            if let Some((name, val)) = h.split_once(':') {
                let lower_name = name.trim().to_ascii_lowercase();
                if lower_name == "transfer-encoding"
                    && val
                        .split(',')
                        .any(|tok| tok.trim().eq_ignore_ascii_case("chunked"))
                {
                    is_chunked = true;
                } else if lower_name == "content-length"
                    && let Ok(cl) = val.trim().parse::<usize>()
                {
                    content_length = Some(cl);
                }
            }
            out.push_str(h);
            out.push_str("\r\n");
        }
        out.push_str("Connection: close\r\n\r\n");

        client_write.write_all(out.as_bytes()).await?;
        buf.drain(..head_end + 4);

        let no_body = req_method.eq_ignore_ascii_case("HEAD")
            || matches!(status_code, Some(204 | 304))
            || matches!(status_code, Some(100..=199));

        if !no_body {
            if is_chunked {
                forward_chunked_body(&mut buf, up_read, client_write).await?;
            } else if let Some(cl) = content_length {
                forward_exact_bytes(&mut buf, up_read, client_write, cl).await?;
            } else {
                if !buf.is_empty() {
                    client_write.write_all(&buf).await?;
                    buf.clear();
                }
                let _ = tokio::io::copy(up_read, client_write).await;
            }
        }

        let _ = client_write.flush().await;
        return Ok(RelayOutcome::Completed);
    }

    Err(MimikyuError::Transport(
        "too many 1xx informational responses from upstream".to_string(),
    ))
}

async fn relay_single_http_exchange(
    req_method: &str,
    client: &mut TcpStream,
    up: &mut TcpStream,
) -> Result<()> {
    let (mut client_read, mut client_write) = client.split();
    let (mut up_read, mut up_write) = up.split();

    let c2u = async {
        let _ = tokio::io::copy(&mut client_read, &mut up_write).await;
        let _ = up_write.shutdown().await;
    };
    let u2c = async {
        let res = relay_http_response(req_method, &mut up_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
        res
    };

    tokio::pin!(c2u);
    tokio::pin!(u2c);

    let mut c2u_done = false;
    loop {
        tokio::select! {
            () = &mut c2u, if !c2u_done => {
                c2u_done = true;
            }
            res = &mut u2c => {
                return match res {
                    Ok(RelayOutcome::Upgraded) => {
                        if !c2u_done {
                            c2u.await;
                        }
                        Ok(())
                    }
                    Ok(RelayOutcome::Completed) => Ok(()),
                    Err(e) => Err(e),
                };
            }
        }
    }
}

struct ParsedRequest<'a> {
    method: &'a str,
    target: &'a str,
    version: &'a str,
    headers: Vec<&'a str>,
    leftover: &'a [u8],
    is_connect: bool,
    has_upgrade: bool,
}

async fn wait_for_shutdown(state_rx: &mut watch::Receiver<UpstreamState>) {
    loop {
        if state_rx.changed().await.is_err() {
            return;
        }
        let _ = state_rx.borrow_and_update();
    }
}

async fn wait_for_generation_change_or_shutdown(
    state_rx: &mut watch::Receiver<UpstreamState>,
    captured_gen: u64,
) {
    loop {
        if state_rx.borrow().generation != captured_gen {
            return;
        }
        if state_rx.changed().await.is_err() {
            return;
        }
    }
}

fn resolve_direct_target(
    is_connect: bool,
    target: &str,
    rest_headers: &[&str],
) -> Result<(String, u16, Option<String>)> {
    if is_connect {
        let (h, p) = parse_host_port(target, 443)?;
        Ok((h, p, None))
    } else if let Ok(u) = url::Url::parse(target) {
        let h = u
            .host_str()
            .map(strip_ipv6_brackets)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MimikyuError::InvalidConfig("missing target host".into()))?;
        let p = u.port_or_known_default().unwrap_or(80);
        let mut pq = u.path().to_string();
        if pq.is_empty() {
            pq.push('/');
        }
        if let Some(q) = u.query() {
            pq.push('?');
            pq.push_str(q);
        }
        Ok((h, p, Some(pq)))
    } else {
        let host_hdr = rest_headers
            .iter()
            .find_map(|h| {
                let (k, v) = h.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("host") {
                    Some(v.trim())
                } else {
                    None
                }
            })
            .unwrap_or("");
        let (h, p) = parse_host_port(host_hdr, 80)?;
        Ok((h, p, Some(target.to_string())))
    }
}

async fn forward_origin_http_request(
    client: &mut TcpStream,
    up: &mut TcpStream,
    req: &ParsedRequest<'_>,
    path: &str,
    target_host: &str,
    target_port: u16,
) -> Result<()> {
    let mut has_host_header = false;
    let mut out = format!("{} {path} {}\r\n", req.method, req.version);
    for h in &req.headers {
        if h.is_empty() || is_hop_by_hop_or_proxy_auth_header(h, req.has_upgrade) {
            continue;
        }
        if let Some((name, _)) = h.split_once(':')
            && name.trim().eq_ignore_ascii_case("host")
        {
            has_host_header = true;
        }
        out.push_str(h);
        out.push_str("\r\n");
    }
    if !has_host_header {
        let host_fmt = if target_host.contains(':') {
            format!("[{target_host}]")
        } else {
            target_host.to_string()
        };
        if target_port == 80 {
            out.push_str(&format!("Host: {host_fmt}\r\n"));
        } else {
            out.push_str(&format!("Host: {host_fmt}:{target_port}\r\n"));
        }
    }
    if !req.has_upgrade {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    up.write_all(out.as_bytes()).await?;
    if !req.leftover.is_empty() {
        up.write_all(req.leftover).await?;
    }
    relay_single_http_exchange(req.method, client, up).await
}

async fn dispatch_to_upstream(
    client: &mut TcpStream,
    upstream: &UpstreamKind,
    req: &ParsedRequest<'_>,
) -> Result<()> {
    match upstream {
        UpstreamKind::Direct => {
            let (target_host, target_port, origin_path) =
                resolve_direct_target(req.is_connect, req.target, &req.headers)?;
            let mut up = TcpStream::connect((target_host.as_str(), target_port)).await?;

            if req.is_connect {
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                if !req.leftover.is_empty() {
                    up.write_all(req.leftover).await?;
                }
                let _ = tokio::io::copy_bidirectional(client, &mut up).await;
            } else {
                let path = origin_path.as_deref().unwrap_or("/");
                forward_origin_http_request(client, &mut up, req, path, &target_host, target_port)
                    .await?;
            }
        }
        UpstreamKind::Http {
            host,
            port,
            auth_header,
        } => {
            let mut up = TcpStream::connect((host.as_str(), *port)).await?;
            let mut out = format!("{} {} {}\r\n", req.method, req.target, req.version);
            for h in &req.headers {
                if h.is_empty() {
                    continue;
                }
                if req.is_connect {
                    if let Some((name, _)) = h.split_once(':') {
                        let lower = name.trim().to_ascii_lowercase();
                        if lower == "proxy-authorization" || lower == "proxy-connection" {
                            continue;
                        }
                    }
                } else if is_hop_by_hop_or_proxy_auth_header(h, req.has_upgrade) {
                    continue;
                }
                out.push_str(h);
                out.push_str("\r\n");
            }
            if let Some(auth) = auth_header {
                out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
            }
            if !req.is_connect && !req.has_upgrade {
                out.push_str("Connection: close\r\n");
            }
            out.push_str("\r\n");

            up.write_all(out.as_bytes()).await?;
            if !req.leftover.is_empty() {
                up.write_all(req.leftover).await?;
            }

            if req.is_connect {
                let _ = tokio::io::copy_bidirectional(client, &mut up).await;
            } else {
                relay_single_http_exchange(req.method, client, &mut up).await?;
            }
        }
        UpstreamKind::Socks5 {
            host,
            port,
            credentials,
        } => {
            let (target_host, target_port, origin_path) =
                resolve_direct_target(req.is_connect, req.target, &req.headers)?;

            let mut up = TcpStream::connect((host.as_str(), *port)).await?;
            socks5_connect(&mut up, &target_host, target_port, credentials.as_ref()).await?;

            if req.is_connect {
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                if !req.leftover.is_empty() {
                    up.write_all(req.leftover).await?;
                }
                let _ = tokio::io::copy_bidirectional(client, &mut up).await;
            } else {
                let path = origin_path.as_deref().unwrap_or("/");
                forward_origin_http_request(client, &mut up, req, path, &target_host, target_port)
                    .await?;
            }
        }
    }

    Ok(())
}

async fn handle_client(
    mut client: TcpStream,
    mut state_rx: watch::Receiver<UpstreamState>,
) -> Result<()> {
    let (head_bytes, leftover) = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut state_rx) => {
            let _ = client.shutdown().await;
            return Ok(());
        }
        res = read_http_head(&mut client) => res?,
    };

    let head_str = String::from_utf8_lossy(&head_bytes);
    let mut lines = head_str.split("\r\n");
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let headers: Vec<&str> = lines.collect();

    if method.is_empty() || target.is_empty() {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await;
        return Ok(());
    }

    let is_connect = method.eq_ignore_ascii_case("CONNECT");
    let has_upgrade = !is_connect
        && headers.iter().any(|h| {
            h.split_once(':')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("upgrade"))
        });

    let req = ParsedRequest {
        method,
        target,
        version,
        headers,
        leftover: &leftover,
        is_connect,
        has_upgrade,
    };

    let (captured_gen, upstream) = {
        let state = state_rx.borrow_and_update();
        (state.generation, Arc::clone(&state.upstream))
    };

    let aborted = tokio::select! {
        biased;
        _ = wait_for_generation_change_or_shutdown(&mut state_rx, captured_gen) => {
            true
        }
        res = dispatch_to_upstream(&mut client, &upstream, &req) => {
            res?;
            false
        }
    };

    if aborted {
        debug!(
            target: "mimikyu::proxy_relay",
            captured_gen,
            is_connect,
            "flushing active relay connection after upstream swap or shutdown"
        );
        let _ = client.shutdown().await;
    }

    Ok(())
}

fn parse_host_port(raw: &str, default_port: u16) -> Result<(String, u16)> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Ok((host.to_string(), port));
    }
    if trimmed.parse::<IpAddr>().is_ok() {
        return Ok((trimmed.to_string(), default_port));
    }
    if let Some((host, p)) = trimmed.rsplit_once(':')
        && let Ok(port) = p.parse::<u16>()
    {
        return Ok((host.to_string(), port));
    }
    if trimmed.is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "empty target host in proxy request".to_string(),
        ));
    }
    Ok((trimmed.to_string(), default_port))
}

async fn socks5_connect(
    up: &mut TcpStream,
    target_host: &str,
    target_port: u16,
    credentials: Option<&(String, String)>,
) -> Result<()> {
    // 1. Greeting (RFC 1928)
    let methods: &[u8] = if credentials.is_some() {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    up.write_all(methods).await?;

    let mut greeting_resp = [0u8; 2];
    up.read_exact(&mut greeting_resp).await?;
    if greeting_resp[0] != 0x05 {
        return Err(MimikyuError::Transport(
            "invalid SOCKS5 greeting version".to_string(),
        ));
    }

    // 2. Optional RFC 1929 Username/Password subnegotiation
    match greeting_resp[1] {
        0x00 => {}
        0x02 => {
            let (user, pass) = credentials.ok_or_else(|| {
                MimikyuError::Transport("SOCKS5 server requested auth but none configured".into())
            })?;
            let u_bytes = user.as_bytes();
            let p_bytes = pass.as_bytes();
            if u_bytes.len() > 255 || p_bytes.len() > 255 {
                return Err(MimikyuError::InvalidConfig(
                    "SOCKS5 username/password exceeds 255 bytes".to_string(),
                ));
            }
            let mut auth_msg = Vec::with_capacity(3 + u_bytes.len() + p_bytes.len());
            auth_msg.push(0x01);
            auth_msg.push(u_bytes.len() as u8);
            auth_msg.extend_from_slice(u_bytes);
            auth_msg.push(p_bytes.len() as u8);
            auth_msg.extend_from_slice(p_bytes);
            up.write_all(&auth_msg).await?;

            let mut auth_resp = [0u8; 2];
            up.read_exact(&mut auth_resp).await?;
            if auth_resp[1] != 0x00 {
                return Err(MimikyuError::Transport(
                    "SOCKS5 username/password authentication rejected".to_string(),
                ));
            }
        }
        other => {
            return Err(MimikyuError::Transport(format!(
                "SOCKS5 server rejected auth methods (code {other:#x})"
            )));
        }
    }

    // 3. CONNECT request (prefer ATYP=0x03 for hostnames to ensure remote DNS)
    let mut req = Vec::with_capacity(7 + target_host.len());
    req.push(0x05); // VER
    req.push(0x01); // CMD = CONNECT
    req.push(0x00); // RSV
    if let Ok(ip) = target_host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => {
                req.push(0x01);
                req.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                req.push(0x04);
                req.extend_from_slice(&v6.octets());
            }
        }
    } else {
        let host_bytes = target_host.as_bytes();
        if host_bytes.len() > 255 {
            return Err(MimikyuError::InvalidConfig(
                "SOCKS5 target hostname exceeds 255 bytes".to_string(),
            ));
        }
        req.push(0x03);
        req.push(host_bytes.len() as u8);
        req.extend_from_slice(host_bytes);
    }
    req.extend_from_slice(&target_port.to_be_bytes());
    up.write_all(&req).await?;

    let mut hdr = [0u8; 4];
    up.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 || hdr[1] != 0x00 {
        return Err(MimikyuError::Transport(format!(
            "SOCKS5 CONNECT failed with reply code {}",
            hdr[1]
        )));
    }
    match hdr[3] {
        0x01 => {
            let mut rest = [0u8; 6];
            up.read_exact(&mut rest).await?;
        }
        0x03 => {
            let mut len_buf = [0u8; 1];
            up.read_exact(&mut len_buf).await?;
            let mut rest = vec![0u8; len_buf[0] as usize + 2];
            up.read_exact(&mut rest).await?;
        }
        0x04 => {
            let mut rest = [0u8; 18];
            up.read_exact(&mut rest).await?;
        }
        other => {
            return Err(MimikyuError::Transport(format!(
                "SOCKS5 invalid reply ATYP {other}"
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_upstream_socks5h_and_percent_decoded_credentials() {
        let socks = ProxyConfig {
            server: "socks5h://alice%40corp:p%40ss%3A123%25@127.0.0.1:1080".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        match parse_upstream(&socks).expect("socks5h should parse") {
            UpstreamKind::Socks5 {
                host,
                port,
                credentials,
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 1080);
                assert_eq!(
                    credentials,
                    Some(("alice@corp".to_string(), "p@ss:123%".to_string()))
                );
            }
            other => panic!("expected Socks5, got {other:?}"),
        }

        let http = ProxyConfig {
            server: "http://bob%2B1:s3cr%23t@127.0.0.1:8080".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        match parse_upstream(&http).expect("http URL creds should parse") {
            UpstreamKind::Http { auth_header, .. } => {
                let expected = format!("Basic {}", BASE64.encode("bob+1:s3cr#t"));
                assert_eq!(auth_header, Some(expected));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn proxy_relay_enforces_connection_close_both_directions() {
        // Upstream HTTP proxy that intentionally IGNORES Connection: close and sends
        // Connection: keep-alive with both Content-Length and Transfer-Encoding: chunked bodies,
        // holding the TCP socket open until the relay drops it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = listener.local_addr().unwrap();
        let seen_reqs = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let seen_clone = std::sync::Arc::clone(&seen_reqs);

        let srv = tokio::spawn(async move {
            for idx in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let seen = std::sync::Arc::clone(&seen_clone);
                tokio::spawn(async move {
                    let (head, _) = read_http_head(&mut s).await.unwrap();
                    let head_str = String::from_utf8_lossy(&head).into_owned();
                    seen.lock().push(head_str.clone());
                    let resp = if idx == 0 {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: keep-alive\r\nKeep-Alive: timeout=60\r\n\r\nhello".to_vec()
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n5\r\nworld\r\n0\r\n\r\n".to_vec()
                    };
                    let _ = s.write_all(&resp).await;
                    // Hold socket open until peer closes (verifying ProxyRelay terminates via CL/chunked)
                    let mut sink = [0u8; 64];
                    let _ = s.read(&mut sink).await;
                });
            }
        });

        let relay = ProxyRelay::start(&ProxyConfig {
            server: format!("http://{up_addr}"),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            bypass: Vec::new(),
        })
        .await
        .unwrap();

        for path in ["/cl", "/chunked"] {
            let mut client = TcpStream::connect(relay.local_addr()).await.unwrap();
            let req = format!(
                "GET http://127.0.0.1:80{path} HTTP/1.1\r\nHost: 127.0.0.1:80\r\nConnection: keep-alive\r\nProxy-Connection: keep-alive\r\n\r\n"
            );
            client.write_all(req.as_bytes()).await.unwrap();
            let mut raw_resp = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.read_to_end(&mut raw_resp),
            )
            .await
            .expect("relay must close client socket promptly after body")
            .unwrap();
            let resp_str = String::from_utf8_lossy(&raw_resp);
            assert!(
                resp_str.contains("Connection: close\r\n"),
                "response must enforce Connection: close: {resp_str}"
            );
            assert!(
                !resp_str
                    .to_ascii_lowercase()
                    .contains("connection: keep-alive"),
                "response must strip keep-alive: {resp_str}"
            );
            if path == "/cl" {
                assert!(resp_str.ends_with("\r\n\r\nhello"));
            } else {
                assert!(resp_str.ends_with("5\r\nworld\r\n0\r\n\r\n"));
            }
        }

        let captured = seen_reqs.lock().clone();
        assert_eq!(captured.len(), 2);
        for req in &captured {
            let lower = req.to_ascii_lowercase();
            assert!(
                lower.contains("connection: close"),
                "missing Connection: close: {req}"
            );
            assert!(
                !lower.contains("proxy-connection:"),
                "Proxy-Connection leaked: {req}"
            );
            assert!(
                !lower.contains("connection: keep-alive"),
                "keep-alive leaked: {req}"
            );
            assert!(
                req.contains("Proxy-Authorization: Basic "),
                "missing Proxy-Authorization: {req}"
            );
        }

        srv.abort();
    }

    #[tokio::test]
    async fn proxy_relay_parses_schemeless_credentials_and_starts() {
        let cfg = ProxyConfig {
            server: "alice:secret@127.0.0.1:8080".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        assert!(cfg.needs_relay());
        match parse_upstream(&cfg).expect("schemeless credentials must parse") {
            UpstreamKind::Http {
                host,
                port,
                auth_header,
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8080);
                let expected = format!("Basic {}", BASE64.encode("alice:secret"));
                assert_eq!(auth_header, Some(expected));
            }
            other => panic!("expected Http, got {other:?}"),
        }

        let relay = ProxyRelay::start(&cfg)
            .await
            .expect("ProxyRelay::start must succeed with schemeless credentials");
        assert_ne!(relay.local_addr().port(), 0);
        drop(relay);
    }

    #[tokio::test]
    async fn proxy_relay_preserves_101_switching_protocols_bidirectional() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = listener.local_addr().unwrap();

        let srv = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let (head, _) = read_http_head(&mut s).await.unwrap();
            let head_str = String::from_utf8_lossy(&head);
            assert!(
                head_str.to_ascii_lowercase().contains("upgrade: websocket"),
                "upstream must see Upgrade header: {head_str}"
            );
            assert!(
                head_str
                    .to_ascii_lowercase()
                    .contains("connection: upgrade"),
                "upstream must see Connection: Upgrade: {head_str}"
            );
            s.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .unwrap();
            let mut buf = [0u8; 10];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping-frame");
            s.write_all(b"pong-frame").await.unwrap();
            s.flush().await.unwrap();
        });

        let relay = ProxyRelay::start(&ProxyConfig {
            server: format!("http://alice:secret@{up_addr}"),
            username: None,
            password: None,
            bypass: Vec::new(),
        })
        .await
        .unwrap();

        let mut client = TcpStream::connect(relay.local_addr()).await.unwrap();
        client
            .write_all(
                b"GET http://127.0.0.1:80/ws HTTP/1.1\r\nHost: 127.0.0.1:80\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .unwrap();

        let (resp_head, leftover) = read_http_head(&mut client).await.unwrap();
        let resp_str = String::from_utf8_lossy(&resp_head);
        assert!(
            resp_str.starts_with("HTTP/1.1 101 Switching Protocols"),
            "expected 101 response, got: {resp_str}"
        );
        assert!(!resp_str.to_ascii_lowercase().contains("connection: close"));

        client.write_all(b"ping-frame").await.unwrap();
        client.flush().await.unwrap();

        let mut got = leftover;
        while got.len() < 10 {
            let mut chunk = [0u8; 32];
            let n =
                tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut chunk))
                    .await
                    .expect("read upgraded response")
                    .unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(&got, b"pong-frame");
        drop(client);
        let _ = srv.await;
    }

    #[test]
    fn parse_upstream_strips_ipv6_brackets() {
        let http_cfg = ProxyConfig {
            server: "http://u:p@[::1]:8080".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        match parse_upstream(&http_cfg).expect("ipv6 http upstream should parse") {
            UpstreamKind::Http { host, port, .. } => {
                assert_eq!(host, "::1");
                assert_eq!(port, 8080);
            }
            other => panic!("expected Http, got {other:?}"),
        }

        let socks_cfg = ProxyConfig {
            server: "socks5h://u:p@[2001:db8::1]:1080".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        match parse_upstream(&socks_cfg).expect("ipv6 socks5 upstream should parse") {
            UpstreamKind::Socks5 { host, port, .. } => {
                assert_eq!(host, "2001:db8::1");
                assert_eq!(port, 1080);
            }
            other => panic!("expected Socks5, got {other:?}"),
        }

        assert_eq!(
            parse_host_port("[::1]:8443", 443).unwrap(),
            ("::1".to_string(), 8443)
        );
        assert_eq!(
            parse_host_port("::1", 443).unwrap(),
            ("::1".to_string(), 443)
        );
    }

    #[tokio::test]
    async fn proxy_relay_forwards_http_and_connect_via_ipv6_upstream() {
        let Ok(listener) = TcpListener::bind("[::1]:0").await else {
            return;
        };
        let up_addr = listener.local_addr().unwrap();
        let expected_auth = format!("Basic {}", BASE64.encode("u:p"));

        let srv = tokio::spawn(async move {
            // Connection 1: HTTP GET
            let (mut s1, _) = listener.accept().await.unwrap();
            let (head1, _) = read_http_head(&mut s1).await.unwrap();
            let h1 = String::from_utf8_lossy(&head1).into_owned();
            assert!(h1.contains(&format!("Proxy-Authorization: {expected_auth}")));
            s1.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nipv6-ok",
            )
            .await
            .unwrap();
            let _ = s1.shutdown().await;

            // Connection 2: CONNECT tunnel
            let (mut s2, _) = listener.accept().await.unwrap();
            let (head2, _) = read_http_head(&mut s2).await.unwrap();
            let h2 = String::from_utf8_lossy(&head2).into_owned();
            assert!(h2.starts_with("CONNECT [::1]:443 HTTP/1.1"));
            assert!(h2.contains(&format!("Proxy-Authorization: {expected_auth}")));
            s2.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut ping = [0u8; 4];
            s2.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            s2.write_all(b"pong").await.unwrap();
            let _ = s2.shutdown().await;
        });

        let relay = ProxyRelay::start(&ProxyConfig {
            server: format!("http://u:p@[::1]:{}", up_addr.port()),
            username: None,
            password: None,
            bypass: Vec::new(),
        })
        .await
        .expect("start relay with IPv6 HTTP upstream");

        let mut c1 = TcpStream::connect(relay.local_addr()).await.unwrap();
        c1.write_all(b"GET http://example.com/hello HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        let mut resp1 = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            c1.read_to_end(&mut resp1),
        )
        .await
        .expect("HTTP GET via IPv6 upstream")
        .unwrap();
        assert!(String::from_utf8_lossy(&resp1).ends_with("\r\n\r\nipv6-ok"));

        let mut c2 = TcpStream::connect(relay.local_addr()).await.unwrap();
        c2.write_all(b"CONNECT [::1]:443 HTTP/1.1\r\nHost: [::1]:443\r\n\r\n")
            .await
            .unwrap();
        let (conn_head, _) = read_http_head(&mut c2).await.unwrap();
        assert!(String::from_utf8_lossy(&conn_head).starts_with("HTTP/1.1 200"));
        c2.write_all(b"ping").await.unwrap();
        let mut pong = [0u8; 4];
        c2.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");

        drop(c2);
        let _ = srv.await;
    }

    #[tokio::test]
    async fn proxy_relay_forwards_socks5_ipv6_upstream_and_ipv6_atyp04_target() {
        let Ok(listener) = TcpListener::bind("[::1]:0").await else {
            return;
        };
        let socks_addr = listener.local_addr().unwrap();

        let srv = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // 1. Greeting: VER=5, NMETHODS=2, [0x00, 0x02]
            let mut greet = [0u8; 4];
            s.read_exact(&mut greet).await.unwrap();
            assert_eq!(greet[0], 0x05);
            s.write_all(&[0x05, 0x02]).await.unwrap();

            // 2. Username/Password auth
            let mut ulen_hdr = [0u8; 2];
            s.read_exact(&mut ulen_hdr).await.unwrap();
            let mut rest = vec![0u8; ulen_hdr[1] as usize];
            s.read_exact(&mut rest).await.unwrap();
            let mut plen = [0u8; 1];
            s.read_exact(&mut plen).await.unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            s.read_exact(&mut pass).await.unwrap();
            assert_eq!(&rest, b"u");
            assert_eq!(&pass, b"p");
            s.write_all(&[0x01, 0x00]).await.unwrap();

            // 3. CONNECT request: must use ATYP=0x04 for "::1" and port 8080
            let mut req_hdr = [0u8; 4];
            s.read_exact(&mut req_hdr).await.unwrap();
            assert_eq!(req_hdr, [0x05, 0x01, 0x00, 0x04]);
            let mut v6_and_port = [0u8; 18];
            s.read_exact(&mut v6_and_port).await.unwrap();
            assert_eq!(&v6_and_port[..16], &std::net::Ipv6Addr::LOCALHOST.octets());
            assert_eq!(u16::from_be_bytes([v6_and_port[16], v6_and_port[17]]), 8080);
            s.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();

            // 4. Forwarded origin-form HTTP request
            let (head, _) = read_http_head(&mut s).await.unwrap();
            let head_str = String::from_utf8_lossy(&head);
            assert!(head_str.starts_with("GET /v6path HTTP/1.1"));
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nsocks6-ok",
            )
            .await
            .unwrap();
            let _ = s.shutdown().await;
        });

        let relay = ProxyRelay::start(&ProxyConfig {
            server: format!("socks5h://u:p@[::1]:{}", socks_addr.port()),
            username: None,
            password: None,
            bypass: Vec::new(),
        })
        .await
        .expect("start relay with IPv6 SOCKS5 upstream");

        let mut client = TcpStream::connect(relay.local_addr()).await.unwrap();
        client
            .write_all(b"GET http://[::1]:8080/v6path HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_to_end(&mut resp),
        )
        .await
        .expect("SOCKS5 IPv6 target response")
        .unwrap();
        assert!(String::from_utf8_lossy(&resp).ends_with("\r\n\r\nsocks6-ok"));
        let _ = srv.await;
    }

    #[test]
    fn proxy_relay_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProxyRelay>();
    }

    #[test]
    fn resolve_upstream_handles_direct_and_rejects_invalid_configs() {
        assert_eq!(resolve_upstream(None).unwrap(), UpstreamKind::Direct);

        let port_zero = ProxyConfig {
            server: "http://127.0.0.1:0".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        assert!(matches!(
            resolve_upstream(Some(&port_zero)),
            Err(MimikyuError::InvalidConfig(_))
        ));

        let with_space = ProxyConfig {
            server: "http://127.0.0.1:8080/bad path".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        assert!(matches!(
            resolve_upstream(Some(&with_space)),
            Err(MimikyuError::InvalidConfig(_))
        ));

        let with_crlf = ProxyConfig {
            server: "http://127.0.0.1:8080\r\n".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        // Leading/trailing whitespace is trimmed, embedded control chars are rejected.
        assert!(resolve_upstream(Some(&with_crlf)).is_ok());

        let embedded_crlf = ProxyConfig {
            server: "http://127.0.0.1:8080\r\nX-Injected: 1".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        assert!(matches!(
            resolve_upstream(Some(&embedded_crlf)),
            Err(MimikyuError::InvalidConfig(_))
        ));

        let https_proxy = ProxyConfig {
            server: "https://127.0.0.1:8443".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        assert!(matches!(
            resolve_upstream(Some(&https_proxy)),
            Err(MimikyuError::Unsupported { .. })
        ));

        let socks4_proxy = ProxyConfig {
            server: "socks4://127.0.0.1:1080".to_string(),
            username: None,
            password: None,
            bypass: Vec::new(),
        };
        assert!(matches!(
            resolve_upstream(Some(&socks4_proxy)),
            Err(MimikyuError::Unsupported { .. })
        ));

        let oversized_socks5 = ProxyConfig {
            server: "socks5://127.0.0.1:1080".to_string(),
            username: Some("a".repeat(256)),
            password: Some("p".to_string()),
            bypass: Vec::new(),
        };
        assert!(matches!(
            resolve_upstream(Some(&oversized_socks5)),
            Err(MimikyuError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn preconnected_idle_socket_routes_to_swapped_upstream() {
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = listener_b.local_addr().unwrap();

        let srv_b = tokio::spawn(async move {
            let (mut s, _) = listener_b.accept().await.unwrap();
            let (_head, _) = read_http_head(&mut s).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nfrom-b")
                .await
                .unwrap();
            let _ = s.shutdown().await;
        });

        let relay = ProxyRelay::start(&ProxyConfig {
            server: format!("http://{addr_a}"),
            username: None,
            password: None,
            bypass: Vec::new(),
        })
        .await
        .unwrap();

        // Pre-connect TCP socket BEFORE swapping upstream (simulating browser socket preconnect).
        let mut client = TcpStream::connect(relay.local_addr()).await.unwrap();
        tokio::task::yield_now().await;

        relay
            .swap_upstream(Some(&ProxyConfig {
                server: format!("http://{addr_b}"),
                username: None,
                password: None,
                bypass: Vec::new(),
            }))
            .unwrap();
        assert_eq!(relay.generation(), 1);

        client
            .write_all(b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_to_end(&mut resp),
        )
        .await
        .expect("preconnected client should route via swapped upstream B")
        .unwrap();
        assert!(String::from_utf8_lossy(&resp).ends_with("\r\n\r\nfrom-b"));

        // Upstream A must never have received a connection.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener_a.accept())
                .await
                .is_err(),
            "upstream A should receive zero connections"
        );
        let _ = srv_b.await;
    }

    #[tokio::test]
    async fn drop_deterministically_closes_active_tunnels_and_idle_clients() {
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();

        let (up_closed_tx, up_closed_rx) = tokio::sync::oneshot::channel::<()>();
        let srv = tokio::spawn(async move {
            let (mut s, _) = up_listener.accept().await.unwrap();
            let (_head, _) = read_http_head(&mut s).await.unwrap();
            s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut ping = [0u8; 4];
            s.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"live");
            s.write_all(b"ack!").await.unwrap();
            // Wait for relay drop to close the upstream side of the tunnel.
            let mut sink = [0u8; 16];
            let _ = s.read(&mut sink).await;
            let _ = up_closed_tx.send(());
        });

        let relay = ProxyRelay::start(&ProxyConfig {
            server: format!("http://{up_addr}"),
            username: None,
            password: None,
            bypass: Vec::new(),
        })
        .await
        .unwrap();
        let relay_addr = relay.local_addr();
        assert!(relay_addr.ip().is_loopback());

        // 1. Open an active CONNECT tunnel
        let mut tunnel_client = TcpStream::connect(relay_addr).await.unwrap();
        tunnel_client
            .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
            .await
            .unwrap();
        let (head, _) = read_http_head(&mut tunnel_client).await.unwrap();
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));
        tunnel_client.write_all(b"live").await.unwrap();
        let mut ack = [0u8; 4];
        tunnel_client.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"ack!");

        // 2. Open an idle pre-connected client socket
        let mut idle_client = TcpStream::connect(relay_addr).await.unwrap();
        tokio::task::yield_now().await;

        // Drop relay: must abort accept loop AND close active tunnel + idle socket promptly.
        drop(relay);

        let mut buf = [0u8; 16];
        let tunnel_eof = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tunnel_client.read(&mut buf),
        )
        .await
        .expect("active tunnel client must close promptly on Drop");
        assert!(
            matches!(tunnel_eof, Ok(0) | Err(_)),
            "expected tunnel EOF/reset on drop, got {tunnel_eof:?}"
        );

        let idle_eof = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            idle_client.read(&mut buf),
        )
        .await
        .expect("idle client must close promptly on Drop");
        assert!(
            matches!(idle_eof, Ok(0) | Err(_)),
            "expected idle client EOF/reset on drop, got {idle_eof:?}"
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), up_closed_rx)
            .await
            .expect("upstream tunnel socket must close promptly on Drop")
            .unwrap();

        let _ = srv.await;
    }
}
