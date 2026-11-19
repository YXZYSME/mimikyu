pub mod pool;
pub mod protocol;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use crate::driver::EngineKind;
use crate::error::{MimikyuError, Result};
use crate::session::{SessionManager, SessionManagerConfig};

pub use pool::DaemonPool;
pub use protocol::{
    normalize_batch_ops_value, parse_batch_actions, DaemonError, DaemonErrorKind, DaemonOp,
    DaemonRequest, DaemonResponse, DaemonStatusInfo, SessionListEntry,
};

const MAX_SUN_PATH_BYTES: usize = 100;
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_DAEMON_IDLE_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub idle_timeout: Option<Duration>,
    pub default_engine: EngineKind,
    pub default_headless: bool,
}

impl DaemonConfig {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            idle_timeout: Some(Duration::from_secs(DEFAULT_DAEMON_IDLE_TIMEOUT_SECS)),
            default_engine: EngineKind::Chromium,
            default_headless: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub state_dir: PathBuf,
    pub canon_state_dir: PathBuf,
    pub sock_link: PathBuf,
    pub bound_sock: PathBuf,
    pub pid_file: PathBuf,
    pub lock_file: PathBuf,
}

pub fn resolve_socket_paths(state_dir: &Path) -> Result<DaemonPaths> {
    let paths = resolve_socket_paths_inner(state_dir, true)?;
    Ok(paths)
}

fn resolve_socket_paths_readonly(state_dir: &Path) -> Result<DaemonPaths> {
    resolve_socket_paths_inner(state_dir, false)
}

fn resolve_socket_paths_inner(state_dir: &Path, create_dir: bool) -> Result<DaemonPaths> {
    if state_dir.as_os_str().is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "state_dir cannot be empty".to_string(),
        ));
    }
    if state_dir.exists() && !state_dir.is_dir() {
        return Err(MimikyuError::InvalidConfig(format!(
            "state_dir '{}' exists and is not a directory",
            state_dir.display()
        )));
    }
    if create_dir {
        fs::create_dir_all(state_dir)?;
    }

    let canon_state_dir = fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    let sock_link = state_dir.join("daemon.sock");
    let canon_sock = canon_state_dir.join("daemon.sock");
    let pid_file = state_dir.join("daemon.pid");
    let lock_file = state_dir.join("daemon.lock");

    let needs_short_sock = sock_link.as_os_str().as_encoded_bytes().len() >= MAX_SUN_PATH_BYTES
        || canon_sock.as_os_str().as_encoded_bytes().len() >= MAX_SUN_PATH_BYTES;

    let bound_sock = if needs_short_sock {
        let mut hasher = Sha256::new();
        hasher.update(canon_state_dir.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        let mut hex16 = String::with_capacity(16);
        for byte in &digest[..8] {
            use std::fmt::Write as _;
            let _ = write!(&mut hex16, "{byte:02x}");
        }
        let base = if Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        base.join(format!("mkd-{hex16}.sock"))
    } else {
        sock_link.clone()
    };

    Ok(DaemonPaths {
        state_dir: state_dir.to_path_buf(),
        canon_state_dir,
        sock_link,
        bound_sock,
        pid_file,
        lock_file,
    })
}

pub fn resolve_connect_target(sock_link: &Path) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(sock_link) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                let target = fs::read_link(sock_link)?;
                if target.is_relative() {
                    let parent = sock_link.parent().unwrap_or_else(|| Path::new("."));
                    Ok(Some(parent.join(target)))
                } else {
                    Ok(Some(target))
                }
            } else {
                Ok(Some(sock_link.to_path_buf()))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MimikyuError::Io(e)),
    }
}

pub fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn cleanup_stale_files(paths: &DaemonPaths) {
    if let Ok(meta) = fs::symlink_metadata(&paths.sock_link) {
        if meta.file_type().is_symlink()
            && let Ok(target) = fs::read_link(&paths.sock_link)
        {
            let resolved = if target.is_relative() {
                paths.state_dir.join(target)
            } else {
                target
            };
            let _ = fs::remove_file(resolved);
        }
        let _ = fs::remove_file(&paths.sock_link);
    }
    if paths.bound_sock != paths.sock_link {
        let _ = fs::remove_file(&paths.bound_sock);
    }
    let _ = fs::remove_file(&paths.pid_file);
}

fn read_pid_file(pid_file: &Path) -> Option<i32> {
    let raw = fs::read_to_string(pid_file).ok()?;
    raw.trim().parse::<i32>().ok().filter(|&p| p > 0)
}

pub(crate) enum BoundedLine {
    Eof,
    Ok(Vec<u8>),
    TooLong,
}

pub(crate) async fn read_bounded_line_bytes<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut buf = Vec::new();
    {
        let mut limited = reader.take((max_bytes + 1) as u64);
        let n = limited.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Ok(BoundedLine::Eof);
        }
    }
    if buf.len() > max_bytes {
        if !buf.ends_with(b"\n") {
            loop {
                let avail = reader.fill_buf().await?;
                if avail.is_empty() {
                    break;
                }
                if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                    reader.consume(pos + 1);
                    break;
                }
                let len = avail.len();
                reader.consume(len);
            }
        }
        return Ok(BoundedLine::TooLong);
    }
    Ok(BoundedLine::Ok(buf))
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> Result<usize> {
    buf.clear();
    match read_bounded_line_bytes(reader, MAX_LINE_BYTES).await? {
        BoundedLine::Eof => Ok(0),
        BoundedLine::TooLong => Err(MimikyuError::Transport(format!(
            "daemon RPC frame exceeded {MAX_LINE_BYTES} bytes"
        ))),
        BoundedLine::Ok(bytes) => {
            let s = std::str::from_utf8(&bytes).map_err(|e| {
                MimikyuError::Transport(format!("invalid UTF-8 in daemon RPC frame: {e}"))
            })?;
            let len = s.len();
            buf.push_str(s);
            Ok(len)
        }
    }
}

enum RpcSendOutcome {
    PreSendIo(std::io::Error),
    PostSend(MimikyuError),
}

async fn send_rpc_on_stream(
    stream: &mut UnixStream,
    req: &DaemonRequest,
) -> std::result::Result<DaemonResponse, RpcSendOutcome> {
    let mut payload = serde_json::to_string(req).map_err(|e| RpcSendOutcome::PostSend(e.into()))?;
    payload.push('\n');
    if let Err(e) = stream.write_all(payload.as_bytes()).await {
        return Err(RpcSendOutcome::PreSendIo(e));
    }
    if let Err(e) = stream.flush().await {
        return Err(RpcSendOutcome::PreSendIo(e));
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = read_line_bounded(&mut reader, &mut line)
        .await
        .map_err(RpcSendOutcome::PostSend)?;
    if n == 0 {
        return Err(RpcSendOutcome::PostSend(MimikyuError::Transport(
            "daemon closed connection before responding".to_string(),
        )));
    }
    let resp: DaemonResponse = serde_json::from_str(line.trim_end_matches(['\r', '\n']))
        .map_err(|e| RpcSendOutcome::PostSend(e.into()))?;
    Ok(resp)
}

async fn try_connect_and_ping_with_pid(paths: &DaemonPaths) -> Option<(UnixStream, u64)> {
    let target = resolve_connect_target(&paths.sock_link).ok()??;
    if target.as_os_str().as_encoded_bytes().len() >= 104 {
        return None;
    }
    let mut stream = tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(&target))
        .await
        .ok()?
        .ok()?;
    let ping_req = DaemonRequest {
        id: 0,
        op: DaemonOp::Ping,
    };
    let resp = tokio::time::timeout(
        Duration::from_millis(500),
        send_rpc_on_stream(&mut stream, &ping_req),
    )
    .await
    .ok()?
    .ok()?;
    if resp.ok {
        let pid = resp
            .data
            .as_ref()
            .and_then(|d| d.get("pid"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Some((stream, pid))
    } else {
        None
    }
}

async fn try_connect_and_ping(paths: &DaemonPaths) -> Option<UnixStream> {
    try_connect_and_ping_with_pid(paths)
        .await
        .map(|(stream, _)| stream)
}

fn is_active_lock_held(state_dir: &Path) -> bool {
    let lock_path = state_dir.join("daemon-active.lock");
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            true
        }
        Err(_) => false,
    }
}

async fn probe_live_socket_or_cleanup_stale(paths: &DaemonPaths) -> Option<UnixStream> {
    if let Some(stream) = try_connect_and_ping(paths).await {
        return Some(stream);
    }

    if let Some(pid) = read_pid_file(&paths.pid_file)
        && is_pid_alive(pid)
    {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if let Some(stream) = try_connect_and_ping(paths).await {
                return Some(stream);
            }
        }
    }

    cleanup_stale_files(paths);
    None
}

async fn wait_for_live_or_exiting_daemon(
    paths: &DaemonPaths,
) -> Result<Option<(UnixStream, u64)>> {
    if let Some(res) = try_connect_and_ping_with_pid(paths).await {
        return Ok(Some(res));
    }

    if is_active_lock_held(&paths.state_dir) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(res) = try_connect_and_ping_with_pid(paths).await {
                return Ok(Some(res));
            }
            if !is_active_lock_held(&paths.state_dir) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(MimikyuError::Timeout {
                    timeout_ms: 20_000,
                    operation: "waiting for existing daemon startup or shutdown".to_string(),
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    } else if let Some(pid) = read_pid_file(&paths.pid_file)
        && is_pid_alive(pid)
    {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if let Some(res) = try_connect_and_ping_with_pid(paths).await {
                return Ok(Some(res));
            }
        }
    }

    cleanup_stale_files(paths);
    Ok(None)
}

async fn acquire_daemon_lock(lock_path: &Path) -> Result<fs::File> {
    let lock_path = lock_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<fs::File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        file.lock_exclusive()?;
        Ok(file)
    })
    .await
    .map_err(|e| MimikyuError::Transport(format!("daemon lock task failed: {e}")))?
}

fn release_daemon_lock(file: fs::File) {
    let _ = FileExt::unlock(&file);
}

pub struct DaemonServer {
    paths: DaemonPaths,
    listener: Option<UnixListener>,
    pool: Arc<DaemonPool>,
    _active_lock: Option<fs::File>,
    owns_files: bool,
}

impl DaemonServer {
    pub async fn bind(config: DaemonConfig) -> Result<Self> {
        let paths = resolve_socket_paths(&config.state_dir)?;
        let bind_lock_path = paths.state_dir.join("daemon-bind.lock");
        let active_lock_path = paths.state_dir.join("daemon-active.lock");
        let bind_lock = acquire_daemon_lock(&bind_lock_path).await?;

        let bind_res = async {
            let active_lock = {
                let path = active_lock_path.clone();
                tokio::task::spawn_blocking(move || -> Result<fs::File> {
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&path)?;
                    file.try_lock_exclusive()?;
                    Ok(file)
                })
                .await
                .map_err(|e| MimikyuError::Transport(format!("daemon active lock task failed: {e}")))?
            };
            let active_lock = match active_lock {
                Ok(f) => f,
                Err(_) => {
                    return Err(MimikyuError::InvalidConfig(format!(
                        "daemon already running for state_dir '{}'",
                        paths.state_dir.display()
                    )));
                }
            };

            if probe_live_socket_or_cleanup_stale(&paths).await.is_some() {
                release_daemon_lock(active_lock);
                return Err(MimikyuError::InvalidConfig(format!(
                    "daemon already running for state_dir '{}'",
                    paths.state_dir.display()
                )));
            }

            if paths.bound_sock.exists() {
                let _ = fs::remove_file(&paths.bound_sock);
            }

            let listener = UnixListener::bind(&paths.bound_sock)?;

            if paths.bound_sock != paths.sock_link {
                let _ = fs::remove_file(&paths.sock_link);
                symlink(&paths.bound_sock, &paths.sock_link)?;
            }

            write_pid_file(&paths.pid_file, std::process::id())?;

            let manager = Arc::new(SessionManager::with_config(SessionManagerConfig {
                state_dir: paths.state_dir.clone(),
                default_engine: config.default_engine,
                default_headless: config.default_headless,
                ..Default::default()
            })?);

            let pool = DaemonPool::new(manager, paths.clone(), config.idle_timeout);

            Ok(Self {
                paths: paths.clone(),
                listener: Some(listener),
                pool,
                _active_lock: Some(active_lock),
                owns_files: true,
            })
        }
        .await;

        release_daemon_lock(bind_lock);
        bind_res
    }

    pub fn pool(&self) -> &Arc<DaemonPool> {
        &self.pool
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    pub async fn run(mut self) -> Result<()> {
        let listener = self.listener.take().ok_or(MimikyuError::Disposed)?;
        let pool = Arc::clone(&self.pool);
        let mut shutdown_rx = pool.subscribe_shutdown();

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut idle_tick = tokio::time::interval(Duration::from_millis(200));
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut connections = tokio::task::JoinSet::new();

        loop {
            while connections.try_join_next().is_some() {}

            tokio::select! {
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, _addr)) => {
                            let conn_pool = Arc::clone(&pool);
                            connections.spawn(async move {
                                handle_connection(stream, conn_pool).await;
                            });
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "daemon accept error");
                        }
                    }
                }
                _ = sigterm.recv() => {
                    tracing::info!("daemon received SIGTERM");
                    pool.request_shutdown();
                    break;
                }
                _ = sigint.recv() => {
                    tracing::info!("daemon received SIGINT");
                    pool.request_shutdown();
                    break;
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        pool.request_shutdown();
                        break;
                    }
                }
                _ = idle_tick.tick() => {
                    if pool.try_expire_idle()
                        || !self.paths.state_dir.exists()
                        || fs::symlink_metadata(&self.paths.bound_sock).is_err()
                        || fs::symlink_metadata(&self.paths.sock_link).is_err()
                    {
                        tracing::info!("daemon idle timeout or state_dir/socket removed; shutting down");
                        pool.request_shutdown();
                        break;
                    }
                }
            }
        }

        // Accept any already-queued backlog connections after setting shutting_down=true
        // so clients receive a clean Disposed RPC response rather than a mid-frame EOF/reset.
        for _ in 0..64 {
            let accepted = std::future::poll_fn(|cx| match listener.poll_accept(cx) {
                std::task::Poll::Ready(res) => std::task::Poll::Ready(Some(res)),
                std::task::Poll::Pending => std::task::Poll::Ready(None),
            })
            .await;
            match accepted {
                Some(Ok((stream, _addr))) => {
                    let conn_pool = Arc::clone(&pool);
                    connections.spawn(async move {
                        handle_connection(stream, conn_pool).await;
                    });
                }
                _ => break,
            }
        }

        drop(listener);
        let mut interrupted = false;
        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while !connections.is_empty() {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                res = tokio::time::timeout(remaining, connections.join_next()) => {
                    match res {
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
                _ = sigterm.recv() => {
                    tracing::warn!("daemon received SIGTERM during connection drain");
                    interrupted = true;
                    break;
                }
                _ = sigint.recv() => {
                    tracing::warn!("daemon received SIGINT during connection drain");
                    interrupted = true;
                    break;
                }
            }
        }
        connections.abort_all();
        let abort_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while !connections.is_empty() {
            let remaining = abort_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                res = tokio::time::timeout(remaining, connections.join_next()) => {
                    match res {
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
                _ = sigterm.recv() => {
                    tracing::warn!("daemon received SIGTERM while aborting connections");
                    interrupted = true;
                    break;
                }
                _ = sigint.recv() => {
                    tracing::warn!("daemon received SIGINT while aborting connections");
                    interrupted = true;
                    break;
                }
            }
        }

        let shutdown_res = if interrupted {
            Ok(())
        } else {
            tokio::select! {
                res = pool.shutdown() => res,
                _ = sigterm.recv() => {
                    tracing::warn!("daemon received SIGTERM during pool shutdown; aborting graceful teardown");
                    Ok(())
                }
                _ = sigint.recv() => {
                    tracing::warn!("daemon received SIGINT during pool shutdown; aborting graceful teardown");
                    Ok(())
                }
            }
        };
        cleanup_stale_files(&self.paths);
        self.owns_files = false;
        shutdown_res
    }
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        if self.owns_files {
            cleanup_stale_files(&self.paths);
        }
    }
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    let tmp_path = path.with_extension("pid.tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        writeln!(f, "{pid}")?;
        f.flush()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

async fn handle_connection(stream: UnixStream, pool: Arc<DaemonPool>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        match read_bounded_line_bytes(&mut reader, MAX_LINE_BYTES).await {
            Ok(BoundedLine::Eof) => break,
            Ok(BoundedLine::TooLong) => {
                let resp = DaemonResponse::err_message(
                    0,
                    DaemonErrorKind::InvalidConfig,
                    format!("request exceeded {MAX_LINE_BYTES} bytes"),
                );
                if write_response(&mut write_half, &resp).await.is_err() {
                    break;
                }
                continue;
            }
            Ok(BoundedLine::Ok(bytes)) => {
                let line = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        let resp = DaemonResponse::err_message(
                            0,
                            DaemonErrorKind::InvalidConfig,
                            format!("invalid UTF-8 in daemon RPC request: {e}"),
                        );
                        if write_response(&mut write_half, &resp).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let req: DaemonRequest = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        let req_id = serde_json::from_str::<Value>(trimmed)
                            .ok()
                            .and_then(|v| v.get("id").and_then(Value::as_u64))
                            .unwrap_or(0);
                        let resp = DaemonResponse::err_message(
                            req_id,
                            DaemonErrorKind::InvalidConfig,
                            format!("invalid daemon RPC request: {e}"),
                        );
                        if write_response(&mut write_half, &resp).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };

                let (resp, stop_after_flush) = pool.dispatch_with_control(req).await;
                if write_response(&mut write_half, &resp).await.is_err() {
                    if stop_after_flush {
                        pool.request_shutdown();
                    }
                    break;
                }
                if stop_after_flush {
                    pool.request_shutdown();
                    break;
                }
                if pool.is_shutting_down() {
                    break;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "daemon client read error");
                break;
            }
        }
    }
}

async fn write_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &DaemonResponse,
) -> std::io::Result<()> {
    let mut out = serde_json::to_string(resp).unwrap_or_else(|_| {
        "{\"id\":0,\"ok\":false,\"error\":{\"kind\":\"internal\",\"message\":\"serialization error\"}}"
            .to_string()
    });
    out.push('\n');
    writer.write_all(out.as_bytes()).await?;
    writer.flush().await
}

fn is_idempotent_read_op(op: &DaemonOp) -> bool {
    matches!(
        op,
        DaemonOp::Ping | DaemonOp::DaemonStatus | DaemonOp::SessionList | DaemonOp::ProfileList
    )
}

pub struct DaemonClient {
    paths: DaemonPaths,
    stream: Mutex<UnixStream>,
    next_id: std::sync::atomic::AtomicU64,
    daemon_pid: std::sync::atomic::AtomicU64,
    auto_spawn_config: Option<DaemonConfig>,
}

impl DaemonClient {
    pub async fn connect(state_dir: &Path) -> Result<Self> {
        let paths = resolve_socket_paths(state_dir)?;
        let target = resolve_connect_target(&paths.sock_link)?.ok_or_else(|| {
            MimikyuError::NotFound(format!(
                "daemon socket not found at '{}'",
                paths.sock_link.display()
            ))
        })?;
        let stream = UnixStream::connect(&target).await?;
        Ok(Self {
            paths,
            stream: Mutex::new(stream),
            next_id: std::sync::atomic::AtomicU64::new(1),
            daemon_pid: std::sync::atomic::AtomicU64::new(0),
            auto_spawn_config: None,
        })
    }

    fn from_stream(paths: DaemonPaths, stream: UnixStream) -> Self {
        Self {
            paths,
            stream: Mutex::new(stream),
            next_id: std::sync::atomic::AtomicU64::new(1),
            daemon_pid: std::sync::atomic::AtomicU64::new(0),
            auto_spawn_config: None,
        }
    }

    fn from_managed_stream(
        paths: DaemonPaths,
        stream: UnixStream,
        daemon_pid: u64,
        config: DaemonConfig,
    ) -> Self {
        Self {
            paths,
            stream: Mutex::new(stream),
            next_id: std::sync::atomic::AtomicU64::new(1),
            daemon_pid: std::sync::atomic::AtomicU64::new(daemon_pid),
            auto_spawn_config: Some(config),
        }
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    pub async fn call(&self, op: DaemonOp) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let req = DaemonRequest { id, op };
        let mut guard = self.stream.lock().await;
        let max_attempts =
            if self.auto_spawn_config.is_some() && !matches!(req.op, DaemonOp::DaemonStop) {
                3
            } else {
                1
            };

        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match send_rpc_on_stream(&mut guard, &req).await {
                Ok(resp) => {
                    if resp.ok {
                        return Ok(resp.data.unwrap_or(Value::Null));
                    }
                    let is_disposed = resp
                        .error
                        .as_ref()
                        .is_some_and(|e| e.kind == DaemonErrorKind::Disposed);
                    let is_explicit_shutdown_disposed = resp.error.as_ref().is_some_and(|e| {
                        e.kind == DaemonErrorKind::Disposed
                            && e.message == "daemon is shutting down"
                    });
                    let err = resp
                        .error
                        .map(DaemonError::into_mimikyu)
                        .unwrap_or_else(|| {
                            MimikyuError::Transport("unknown daemon error".to_string())
                        });
                    if attempt < max_attempts
                        && is_disposed
                        && let Some(ref cfg) = self.auto_spawn_config
                    {
                        let should_respawn = if is_explicit_shutdown_disposed {
                            true
                        } else {
                            let prev_pid = self
                                .daemon_pid
                                .load(std::sync::atomic::Ordering::Relaxed);
                            match try_connect_and_ping_with_pid(&self.paths).await {
                                None => true,
                                Some((_, live_pid)) => prev_pid != 0 && live_pid != prev_pid,
                            }
                        };
                        if should_respawn {
                            let fresh = ensure_daemon(cfg.clone()).await?;
                            self.daemon_pid.store(
                                fresh
                                    .daemon_pid
                                    .load(std::sync::atomic::Ordering::Relaxed),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            *guard = fresh.stream.into_inner();
                            continue;
                        }
                    }
                    return Err(err);
                }
                Err(RpcSendOutcome::PreSendIo(e)) => {
                    if matches!(req.op, DaemonOp::DaemonStop) {
                        return Err(e.into());
                    }
                    if let Some(ref cfg) = self.auto_spawn_config {
                        if attempt < max_attempts {
                            let fresh = ensure_daemon(cfg.clone()).await?;
                            self.daemon_pid.store(
                                fresh
                                    .daemon_pid
                                    .load(std::sync::atomic::Ordering::Relaxed),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            *guard = fresh.stream.into_inner();
                            continue;
                        }
                        return Err(e.into());
                    }
                    let target = resolve_connect_target(&self.paths.sock_link)?.ok_or_else(|| {
                        MimikyuError::Transport("daemon socket disappeared".to_string())
                    })?;
                    *guard = UnixStream::connect(&target).await?;
                    match send_rpc_on_stream(&mut guard, &req).await {
                        Ok(resp) => {
                            return if resp.ok {
                                Ok(resp.data.unwrap_or(Value::Null))
                            } else if let Some(err) = resp.error {
                                Err(err.into_mimikyu())
                            } else {
                                Err(MimikyuError::Transport("unknown daemon error".to_string()))
                            };
                        }
                        Err(RpcSendOutcome::PreSendIo(e2)) => return Err(e2.into()),
                        Err(RpcSendOutcome::PostSend(e2)) => return Err(e2),
                    }
                }
                Err(RpcSendOutcome::PostSend(e)) => {
                    if attempt < max_attempts
                        && let Some(ref cfg) = self.auto_spawn_config
                        && is_idempotent_read_op(&req.op)
                        && matches!(e, MimikyuError::Transport(_) | MimikyuError::Io(_))
                    {
                        let fresh = ensure_daemon(cfg.clone()).await?;
                        self.daemon_pid.store(
                            fresh
                                .daemon_pid
                                .load(std::sync::atomic::Ordering::Relaxed),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        *guard = fresh.stream.into_inner();
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}

pub fn spawn_daemon_background(config: &DaemonConfig) -> Result<std::process::Child> {
    let paths = resolve_socket_paths(&config.state_dir)?;
    let exe = std::env::current_exe()?;

    let mut state_arg = std::ffi::OsString::from("--state-dir=");
    state_arg.push(&paths.state_dir);

    let mut cmd = Command::new(exe);
    cmd.arg(state_arg)
        .arg("--engine")
        .arg(config.default_engine.to_string())
        .arg("daemon")
        .arg("--foreground");
    match config.idle_timeout {
        Some(timeout) => {
            cmd.arg("--idle-timeout").arg(timeout.as_secs().to_string());
        }
        None => {
            cmd.arg("--idle-timeout").arg("0");
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 && libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let max_fd = libc::sysconf(libc::_SC_OPEN_MAX);
            let limit = if max_fd > 3 {
                (max_fd as libc::c_int).min(4096)
            } else {
                1024
            };
            for fd in 3..limit {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && (flags & libc::FD_CLOEXEC) == 0 {
                    let _ = libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }

    Ok(cmd.spawn()?)
}

pub async fn ensure_daemon(config: DaemonConfig) -> Result<DaemonClient> {
    let paths = resolve_socket_paths(&config.state_dir)?;

    if let Some((stream, pid)) = try_connect_and_ping_with_pid(&paths).await {
        return Ok(DaemonClient::from_managed_stream(paths, stream, pid, config));
    }

    let lock_file = acquire_daemon_lock(&paths.lock_file).await?;

    let result = async {
        if let Some((stream, pid)) = wait_for_live_or_exiting_daemon(&paths).await? {
            return Ok(DaemonClient::from_managed_stream(
                paths.clone(),
                stream,
                pid,
                config.clone(),
            ));
        }

        let mut child = spawn_daemon_background(&config)?;
        let deadline = Instant::now() + Duration::from_secs(10);

        loop {
            if let Some((stream, pid)) = try_connect_and_ping_with_pid(&paths).await {
                return Ok(DaemonClient::from_managed_stream(
                    paths.clone(),
                    stream,
                    pid,
                    config.clone(),
                ));
            }
            if let Some(status) = child.try_wait()? {
                for _ in 0..10 {
                    if let Some((stream, pid)) = try_connect_and_ping_with_pid(&paths).await {
                        return Ok(DaemonClient::from_managed_stream(
                            paths.clone(),
                            stream,
                            pid,
                            config.clone(),
                        ));
                    }
                    if !is_active_lock_held(&paths.state_dir) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                return Err(MimikyuError::LaunchFailed(format!(
                    "daemon exited prematurely with status {status}"
                )));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(MimikyuError::Timeout {
                    timeout_ms: 10_000,
                    operation: "waiting for background daemon startup".to_string(),
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    .await;

    release_daemon_lock(lock_file);
    result
}

pub async fn daemon_status(state_dir: &Path) -> Result<DaemonStatusInfo> {
    let paths = resolve_socket_paths_readonly(state_dir)?;
    if !paths.state_dir.exists() {
        return Ok(DaemonStatusInfo {
            running: false,
            pid: 0,
            state_dir: paths.state_dir,
            socket_path: paths.sock_link,
            bound_socket_path: paths.bound_sock,
            uptime_ms: 0,
            idle_timeout_secs: None,
            active_sessions: 0,
            last_activity_ms: 0,
        });
    }
    if let Some(stream) = try_connect_and_ping(&paths).await {
        let client = DaemonClient::from_stream(paths.clone(), stream);
        match client.call(DaemonOp::DaemonStatus).await {
            Ok(data) => {
                let info: DaemonStatusInfo = serde_json::from_value(data)?;
                Ok(info)
            }
            Err(MimikyuError::Disposed | MimikyuError::Transport(_) | MimikyuError::Io(_)) => {
                Ok(DaemonStatusInfo {
                    running: false,
                    pid: 0,
                    state_dir: paths.state_dir,
                    socket_path: paths.sock_link,
                    bound_socket_path: paths.bound_sock,
                    uptime_ms: 0,
                    idle_timeout_secs: None,
                    active_sessions: 0,
                    last_activity_ms: 0,
                })
            }
            Err(e) => Err(e),
        }
    } else {
        Ok(DaemonStatusInfo {
            running: false,
            pid: 0,
            state_dir: paths.state_dir,
            socket_path: paths.sock_link,
            bound_socket_path: paths.bound_sock,
            uptime_ms: 0,
            idle_timeout_secs: None,
            active_sessions: 0,
            last_activity_ms: 0,
        })
    }
}

async fn terminate_pid_with_escalation(paths: &DaemonPaths, p: i32) -> Result<()> {
    if is_pid_alive(p) {
        unsafe {
            libc::kill(p as libc::pid_t, libc::SIGTERM);
        }
        for _ in 0..100 {
            if !is_pid_alive(p) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    if is_pid_alive(p) {
        tracing::warn!(pid = p, "daemon survived SIGTERM; escalating to SIGKILL");
        unsafe {
            libc::kill(p as libc::pid_t, libc::SIGKILL);
        }
        for _ in 0..50 {
            if !is_pid_alive(p) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    if is_pid_alive(p) {
        return Err(MimikyuError::Timeout {
            timeout_ms: 8_000,
            operation: format!(
                "stopping daemon pid {p}: process remained alive after SIGTERM and SIGKILL"
            ),
        });
    }
    if !is_active_lock_held(&paths.state_dir) {
        let current_pid = read_pid_file(&paths.pid_file);
        if current_pid.is_none() || current_pid == Some(p) {
            cleanup_stale_files(paths);
        }
    }
    Ok(())
}

pub async fn daemon_stop(state_dir: &Path) -> Result<bool> {
    let paths = resolve_socket_paths_readonly(state_dir)?;
    if !paths.state_dir.exists() {
        return Ok(false);
    }
    let Some((stream, ping_pid)) = try_connect_and_ping_with_pid(&paths).await else {
        if is_active_lock_held(&paths.state_dir) {
            if let Some(p) = read_pid_file(&paths.pid_file)
                && p != std::process::id() as i32
                && is_pid_alive(p)
            {
                for _ in 0..25 {
                    if !is_pid_alive(p) && !is_active_lock_held(&paths.state_dir) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if is_pid_alive(p) {
                    terminate_pid_with_escalation(&paths, p).await?;
                    return Ok(true);
                }
                if !is_active_lock_held(&paths.state_dir) {
                    cleanup_stale_files(&paths);
                }
                return Ok(true);
            }
            return Ok(false);
        }
        cleanup_stale_files(&paths);
        return Ok(false);
    };

    let pid = read_pid_file(&paths.pid_file)
        .or_else(|| (ping_pid > 0 && ping_pid <= i32::MAX as u64).then_some(ping_pid as i32));
    let client = DaemonClient::from_stream(paths.clone(), stream);
    let _ = client.call(DaemonOp::DaemonStop).await?;
    drop(client);

    let self_pid = std::process::id() as i32;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let current_pid = read_pid_file(&paths.pid_file);
        let sock_gone = resolve_connect_target(&paths.sock_link)?.is_none();
        let lock_free = !is_active_lock_held(&paths.state_dir);
        if let Some(p) = pid {
            if p == self_pid {
                if current_pid.is_none() && sock_gone && lock_free {
                    return Ok(true);
                }
            } else if !is_pid_alive(p)
                && (current_pid != Some(p) || (sock_gone && lock_free))
            {
                if lock_free {
                    cleanup_stale_files(&paths);
                }
                return Ok(true);
            }
        } else if sock_gone && lock_free {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    if let Some(p) = pid {
        if p == self_pid {
            if is_active_lock_held(&paths.state_dir) {
                return Err(MimikyuError::Timeout {
                    timeout_ms: 5_000,
                    operation: "waiting for in-process daemon shutdown".to_string(),
                });
            }
        } else {
            terminate_pid_with_escalation(&paths, p).await?;
            return Ok(true);
        }
    } else if is_active_lock_held(&paths.state_dir) {
        return Err(MimikyuError::Timeout {
            timeout_ms: 5_000,
            operation: "waiting for daemon shutdown (active lock still held)".to_string(),
        });
    }

    if !is_active_lock_held(&paths.state_dir) {
        let current_pid = read_pid_file(&paths.pid_file);
        if current_pid.is_none() || current_pid == pid {
            cleanup_stale_files(&paths);
        }
    }
    Ok(true)
}

pub async fn run_daemon_foreground(config: DaemonConfig) -> Result<()> {
    let server = DaemonServer::bind(config).await?;
    server.run().await
}
