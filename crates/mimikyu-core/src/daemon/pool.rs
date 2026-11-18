use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::watch;

use crate::driver::now_ms;
use crate::error::Result;
use crate::session::SessionManager;

use super::protocol::{
    parse_batch_actions, DaemonOp, DaemonRequest, DaemonResponse, DaemonStatusInfo,
    SessionListEntry,
};
use super::DaemonPaths;

pub struct RequestGuard<'a> {
    pool: &'a DaemonPool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.pool.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.pool.touch_activity();
    }
}

pub struct DaemonPool {
    manager: Arc<SessionManager>,
    paths: DaemonPaths,
    started_at: Instant,
    last_activity_ms: AtomicU64,
    in_flight: AtomicUsize,
    idle_timeout: Option<Duration>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    shutting_down: AtomicBool,
}

impl DaemonPool {
    pub fn new(
        manager: Arc<SessionManager>,
        paths: DaemonPaths,
        idle_timeout: Option<Duration>,
    ) -> Arc<Self> {
        let now = now_ms();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Arc::new(Self {
            manager,
            paths,
            started_at: Instant::now(),
            last_activity_ms: AtomicU64::new(now),
            in_flight: AtomicUsize::new(0),
            idle_timeout,
            shutdown_tx,
            shutdown_rx,
            shutting_down: AtomicBool::new(false),
        })
    }

    pub fn manager(&self) -> &Arc<SessionManager> {
        &self.manager
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    pub fn touch_activity(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Release);
    }

    pub fn begin_request(&self) -> RequestGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch_activity();
        RequestGuard { pool: self }
    }

    pub fn is_idle_expired(&self) -> bool {
        let Some(timeout) = self.idle_timeout else {
            return false;
        };
        if self.in_flight.load(Ordering::Acquire) > 0 {
            return false;
        }
        let elapsed_ms = now_ms().saturating_sub(self.last_activity_ms.load(Ordering::Acquire));
        elapsed_ms >= timeout.as_millis() as u64
    }

    pub fn request_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.shutdown_tx.send(true);
    }

    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_rx.clone()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn status_info(&self) -> DaemonStatusInfo {
        DaemonStatusInfo {
            running: !self.is_shutting_down(),
            pid: std::process::id(),
            state_dir: self.paths.state_dir.clone(),
            socket_path: self.paths.sock_link.clone(),
            bound_socket_path: self.paths.bound_sock.clone(),
            uptime_ms: self.started_at.elapsed().as_millis() as u64,
            idle_timeout_secs: self.idle_timeout.map(|d| d.as_secs()),
            active_sessions: self.manager.list_sessions().len(),
            last_activity_ms: self.last_activity_ms.load(Ordering::Acquire),
        }
    }

    /// Execute a request and return `(response, should_shutdown_after_flush)`.
    pub async fn dispatch_with_control(&self, req: DaemonRequest) -> (DaemonResponse, bool) {
        let _guard = self.begin_request();
        let id = req.id;

        if matches!(req.op, DaemonOp::DaemonStop) {
            self.shutting_down.store(true, Ordering::SeqCst);
            return (DaemonResponse::ok(id, json!({ "stopping": true })), true);
        }

        let resp = match self.dispatch_op(req.op).await {
            Ok(data) => DaemonResponse::ok(id, data),
            Err(err) => DaemonResponse::err(id, &err),
        };
        (resp, false)
    }

    pub async fn dispatch(&self, req: DaemonRequest) -> DaemonResponse {
        let (resp, stop) = self.dispatch_with_control(req).await;
        if stop {
            self.request_shutdown();
        }
        resp
    }

    async fn dispatch_op(&self, op: DaemonOp) -> Result<serde_json::Value> {
        match op {
            DaemonOp::Ping => Ok(json!({ "pong": true, "pid": std::process::id() })),
            DaemonOp::DaemonStatus => Ok(serde_json::to_value(self.status_info())?),
            DaemonOp::DaemonStop => Ok(json!({ "stopping": true })),
            DaemonOp::SessionNew(opts) => {
                let summary = self.manager.create_session(opts).await?;
                Ok(serde_json::to_value(SessionListEntry::from(summary))?)
            }
            DaemonOp::SessionList => {
                let entries: Vec<SessionListEntry> = self
                    .manager
                    .list_sessions()
                    .into_iter()
                    .map(SessionListEntry::from)
                    .collect();
                Ok(serde_json::to_value(entries)?)
            }
            DaemonOp::SessionClose { session_id } => {
                self.manager.close_session(&session_id).await?;
                Ok(json!({ "closed": session_id }))
            }
            DaemonOp::SessionExport {
                session_id,
                out_path,
            } => {
                self.manager.export_session(&session_id, &out_path).await?;
                Ok(json!({ "session_id": session_id, "path": out_path }))
            }
            DaemonOp::SessionImport {
                session_id,
                in_path,
            } => {
                self.manager.import_session(&session_id, &in_path).await?;
                Ok(json!({ "session_id": session_id, "path": in_path }))
            }
            DaemonOp::ProfileList => {
                let profiles = self.manager.profile_list()?;
                Ok(serde_json::to_value(profiles)?)
            }
            DaemonOp::ProfileDelete { name } => {
                self.manager.profile_delete(&name)?;
                Ok(json!({ "deleted": name }))
            }
            DaemonOp::Open { session_id, url } => {
                let nav = self.manager.open(&session_id, &url).await?;
                Ok(serde_json::to_value(nav)?)
            }
            DaemonOp::Back { session_id } => {
                let nav = self.manager.back(&session_id).await?;
                Ok(serde_json::to_value(nav)?)
            }
            DaemonOp::Forward { session_id } => {
                let nav = self.manager.forward(&session_id).await?;
                Ok(serde_json::to_value(nav)?)
            }
            DaemonOp::Reload { session_id } => {
                let nav = self.manager.reload(&session_id).await?;
                Ok(serde_json::to_value(nav)?)
            }
            DaemonOp::Snapshot {
                session_id,
                options,
            } => {
                let snap = self.manager.snapshot(&session_id, options).await?;
                Ok(serde_json::to_value(snap)?)
            }
            DaemonOp::Click {
                session_id,
                target,
                humanize,
            } => {
                self.manager.click(&session_id, &target, humanize).await?;
                Ok(json!({ "clicked": target }))
            }
            DaemonOp::Fill {
                session_id,
                target,
                value,
                humanize,
            } => {
                self.manager
                    .fill(&session_id, &target, &value, humanize)
                    .await?;
                Ok(json!({ "filled": target }))
            }
            DaemonOp::TypeText {
                session_id,
                target,
                text,
                humanize,
            } => {
                self.manager
                    .type_text(&session_id, &target, &text, humanize)
                    .await?;
                Ok(json!({ "typed": target }))
            }
            DaemonOp::Select {
                session_id,
                target,
                value,
            } => {
                let selected = self.manager.select(&session_id, &target, &value).await?;
                Ok(json!({ "selected": selected }))
            }
            DaemonOp::Upload {
                session_id,
                target,
                path,
            } => {
                self.manager.upload(&session_id, &target, path).await?;
                Ok(json!({ "uploaded": target }))
            }
            DaemonOp::Scroll {
                session_id,
                dx,
                dy,
                humanize,
            } => {
                self.manager.scroll(&session_id, dx, dy, humanize).await?;
                Ok(json!({ "dx": dx, "dy": dy }))
            }
            DaemonOp::Press { session_id, key } => {
                self.manager.press(&session_id, &key).await?;
                Ok(json!({ "pressed": key }))
            }
            DaemonOp::Wait {
                session_id,
                request,
            } => {
                self.manager.wait(&session_id, request).await?;
                Ok(json!({ "waited": true }))
            }
            DaemonOp::Batch {
                session_id,
                actions,
            } => {
                let parsed = parse_batch_actions(actions)?;
                let report = self.manager.batch(&session_id, parsed).await?;
                Ok(serde_json::to_value(report)?)
            }
            DaemonOp::Extract { session_id, mode } => self.manager.extract(&session_id, mode).await,
            DaemonOp::Eval {
                session_id,
                js,
                main_world,
            } => {
                let val = self.manager.eval(&session_id, &js, main_world).await?;
                Ok(json!({ "value": val, "main_world": main_world }))
            }
            DaemonOp::Screenshot {
                session_id,
                out_path,
                target,
                full_page,
            } => {
                let res = self
                    .manager
                    .screenshot(&session_id, &out_path, target.as_deref(), full_page)
                    .await?;
                Ok(serde_json::to_value(res)?)
            }
            DaemonOp::Pdf {
                session_id,
                out_path,
            } => {
                let res = self.manager.pdf(&session_id, &out_path).await?;
                Ok(serde_json::to_value(res)?)
            }
            DaemonOp::Logs { session_id, kind } => self.manager.logs(&session_id, kind).await,
            DaemonOp::EnvReport { session_id } => self.manager.env_report(&session_id).await,
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.request_shutdown();
        self.manager.shutdown_all().await
    }
}
