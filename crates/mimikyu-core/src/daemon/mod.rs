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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    fs::create_dir_all(state_dir)?;

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

async fn read_line_bounded<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> Result<usize> {
    buf.clear();
    let n = reader.read_line(buf).await?;
    if buf.len() > MAX_LINE_BYTES {
        return Err(MimikyuError::Transport(format!(
            "daemon RPC frame exceeded {MAX_LINE_BYTES} bytes"
        )));
    }
    Ok(n)
}

async fn send_rpc_on_stream(stream: &mut UnixStream, req: &DaemonRequest) -> Result<DaemonResponse> {
    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = read_line_bounded(&mut reader, &mut line).await?;
    if n == 0 {
        return Err(MimikyuError::Transport(
            "daemon closed connection before responding".to_string(),
        ));
    }
    let resp: DaemonResponse = serde_json::from_str(line.trim_end_matches(['\r', '\n']))?;
    Ok(resp)
}

async fn try_connect_and_ping(paths: &DaemonPaths) -> Option<UnixStream> {
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
        Some(stream)
    } else {
        None
    }
}

async fn probe_live_or_cleanup_stale(paths: &DaemonPaths) -> Option<UnixStream> {
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
    owns_files: bool,
}

impl DaemonServer {
    pub async fn bind(config: DaemonConfig) -> Result<Self> {
        let paths = resolve_socket_paths(&config.state_dir)?;

        if probe_live_or_cleanup_stale(&paths).await.is_some() {
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
            paths,
            listener: Some(listener),
            pool,
            owns_files: true,
        })
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
                ctrl = tokio::signal::ctrl_c() => {
                    if ctrl.is_ok() {
                        tracing::info!("daemon received SIGINT");
                        pool.request_shutdown();
                        break;
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = idle_tick.tick() => {
                    if pool.is_idle_expired()
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

        drop(listener);
        connections.abort_all();
        while connections.join_next().await.is_some() {}

        let shutdown_res = pool.shutdown().await;
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
    let mut line = String::new();

    loop {
        line.clear();
        let read_res = reader.read_line(&mut line).await;
        match read_res {
            Ok(0) => break,
            Ok(_) => {
                if line.len() > MAX_LINE_BYTES {
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

pub struct DaemonClient {
    paths: DaemonPaths,
    stream: Mutex<UnixStream>,
    next_id: std::sync::atomic::AtomicU64,
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
        })
    }

    fn from_stream(paths: DaemonPaths, stream: UnixStream) -> Self {
        Self {
            paths,
            stream: Mutex::new(stream),
            next_id: std::sync::atomic::AtomicU64::new(1),
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

        let resp = match send_rpc_on_stream(&mut guard, &req).await {
            Ok(r) => r,
            Err(_) => {
                let target = resolve_connect_target(&self.paths.sock_link)?.ok_or_else(|| {
                    MimikyuError::Transport("daemon socket disappeared".to_string())
                })?;
                *guard = UnixStream::connect(&target).await?;
                send_rpc_on_stream(&mut guard, &req).await?
            }
        };

        if resp.ok {
            Ok(resp.data.unwrap_or(Value::Null))
        } else if let Some(err) = resp.error {
            Err(err.into_mimikyu())
        } else {
            Err(MimikyuError::Transport("unknown daemon error".to_string()))
        }
    }
}

pub fn spawn_daemon_background(config: &DaemonConfig) -> Result<std::process::Child> {
    let paths = resolve_socket_paths(&config.state_dir)?;
    let exe = std::env::current_exe()?;

    let mut cmd = Command::new(exe);
    cmd.arg("--state-dir")
        .arg(&paths.state_dir)
        .arg("--engine")
        .arg(config.default_engine.to_string())
        .arg("daemon")
        .arg("--foreground");
    if let Some(timeout) = config.idle_timeout {
        cmd.arg("--idle-timeout").arg(timeout.as_secs().to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 && libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    Ok(cmd.spawn()?)
}

pub async fn ensure_daemon(config: DaemonConfig) -> Result<DaemonClient> {
    let paths = resolve_socket_paths(&config.state_dir)?;

    if let Some(stream) = try_connect_and_ping(&paths).await {
        return Ok(DaemonClient::from_stream(paths, stream));
    }

    let lock_file = acquire_daemon_lock(&paths.lock_file).await?;

    let result = async {
        if let Some(stream) = probe_live_or_cleanup_stale(&paths).await {
            return Ok(DaemonClient::from_stream(paths.clone(), stream));
        }

        let mut child = spawn_daemon_background(&config)?;
        let deadline = Instant::now() + Duration::from_secs(10);

        loop {
            if let Some(stream) = try_connect_and_ping(&paths).await {
                return Ok(DaemonClient::from_stream(paths.clone(), stream));
            }
            if let Some(status) = child.try_wait()? {
                if let Some(stream) = try_connect_and_ping(&paths).await {
                    return Ok(DaemonClient::from_stream(paths.clone(), stream));
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
    let paths = resolve_socket_paths(state_dir)?;
    if let Some(stream) = try_connect_and_ping(&paths).await {
        let client = DaemonClient::from_stream(paths, stream);
        let data = client.call(DaemonOp::DaemonStatus).await?;
        let info: DaemonStatusInfo = serde_json::from_value(data)?;
        Ok(info)
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

pub async fn daemon_stop(state_dir: &Path) -> Result<bool> {
    let paths = resolve_socket_paths(state_dir)?;
    let Some(stream) = try_connect_and_ping(&paths).await else {
        cleanup_stale_files(&paths);
        return Ok(false);
    };

    let pid = read_pid_file(&paths.pid_file);
    let client = DaemonClient::from_stream(paths.clone(), stream);
    let _ = client.call(DaemonOp::DaemonStop).await?;
    drop(client);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let sock_gone = resolve_connect_target(&paths.sock_link)?.is_none();
        let pid_gone = pid.is_none_or(|p| !is_pid_alive(p));
        if sock_gone && pid_gone {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    if let Some(p) = pid
        && is_pid_alive(p)
    {
        unsafe {
            libc::kill(p as libc::pid_t, libc::SIGTERM);
        }
        for _ in 0..25 {
            if !is_pid_alive(p) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    cleanup_stale_files(&paths);
    Ok(true)
}

pub async fn run_daemon_foreground(config: DaemonConfig) -> Result<()> {
    let server = DaemonServer::bind(config).await?;
    server.run().await
}
