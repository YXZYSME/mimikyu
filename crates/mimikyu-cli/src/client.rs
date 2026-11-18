use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mimikyu_core::daemon::{
    ensure_daemon, parse_batch_actions, DaemonClient, DaemonConfig, DaemonOp, SessionListEntry,
    DEFAULT_DAEMON_IDLE_TIMEOUT_SECS,
};
use mimikyu_core::driver::EngineKind;
use mimikyu_core::error::Result;
use mimikyu_core::session::{SessionManager, SessionManagerConfig};
use serde_json::{json, Value};

pub enum ExecClient {
    Daemon(Box<DaemonClient>),
    Direct(Arc<SessionManager>),
}

impl ExecClient {
    pub async fn connect(
        state_dir: PathBuf,
        engine: Option<EngineKind>,
        no_daemon: bool,
    ) -> Result<Self> {
        let default_engine = engine.unwrap_or(EngineKind::Chromium);
        if no_daemon {
            let mgr = SessionManager::with_config(SessionManagerConfig {
                state_dir,
                default_engine,
                ..Default::default()
            })?;
            Ok(Self::Direct(Arc::new(mgr)))
        } else {
            let config = DaemonConfig {
                state_dir,
                idle_timeout: Some(Duration::from_secs(DEFAULT_DAEMON_IDLE_TIMEOUT_SECS)),
                default_engine,
                default_headless: true,
            };
            let client = ensure_daemon(config).await?;
            Ok(Self::Daemon(Box::new(client)))
        }
    }

    pub async fn call(&self, op: DaemonOp) -> Result<Value> {
        match self {
            Self::Daemon(client) => client.call(op).await,
            Self::Direct(manager) => dispatch_direct(manager, op).await,
        }
    }

    pub async fn finish(self) {
        if let Self::Direct(manager) = self {
            let _ = manager.shutdown_all().await;
        }
    }
}

async fn dispatch_direct(manager: &SessionManager, op: DaemonOp) -> Result<Value> {
    match op {
        DaemonOp::Ping => Ok(json!({ "pong": true, "pid": std::process::id() })),
        DaemonOp::DaemonStatus | DaemonOp::DaemonStop => Ok(json!({ "running": false })),
        DaemonOp::SessionNew(opts) => {
            let summary = manager.create_session(opts).await?;
            Ok(serde_json::to_value(SessionListEntry::from(summary))?)
        }
        DaemonOp::SessionList => {
            let entries: Vec<SessionListEntry> = manager
                .list_sessions()
                .into_iter()
                .map(SessionListEntry::from)
                .collect();
            Ok(serde_json::to_value(entries)?)
        }
        DaemonOp::SessionClose { session_id } => {
            manager.close_session(&session_id).await?;
            Ok(json!({ "closed": session_id }))
        }
        DaemonOp::SessionExport {
            session_id,
            out_path,
        } => {
            manager.export_session(&session_id, &out_path).await?;
            Ok(json!({ "session_id": session_id, "path": out_path }))
        }
        DaemonOp::SessionImport {
            session_id,
            in_path,
        } => {
            manager.import_session(&session_id, &in_path).await?;
            Ok(json!({ "session_id": session_id, "path": in_path }))
        }
        DaemonOp::ProfileList => Ok(serde_json::to_value(manager.profile_list()?)?),
        DaemonOp::ProfileDelete { name } => {
            manager.profile_delete(&name)?;
            Ok(json!({ "deleted": name }))
        }
        DaemonOp::Open { session_id, url } => {
            Ok(serde_json::to_value(manager.open(&session_id, &url).await?)?)
        }
        DaemonOp::Back { session_id } => {
            Ok(serde_json::to_value(manager.back(&session_id).await?)?)
        }
        DaemonOp::Forward { session_id } => {
            Ok(serde_json::to_value(manager.forward(&session_id).await?)?)
        }
        DaemonOp::Reload { session_id } => {
            Ok(serde_json::to_value(manager.reload(&session_id).await?)?)
        }
        DaemonOp::Snapshot {
            session_id,
            options,
        } => Ok(serde_json::to_value(
            manager.snapshot(&session_id, options).await?,
        )?),
        DaemonOp::Click {
            session_id,
            target,
            humanize,
        } => {
            manager.click(&session_id, &target, humanize).await?;
            Ok(json!({ "clicked": target }))
        }
        DaemonOp::Fill {
            session_id,
            target,
            value,
            humanize,
        } => {
            manager.fill(&session_id, &target, &value, humanize).await?;
            Ok(json!({ "filled": target }))
        }
        DaemonOp::TypeText {
            session_id,
            target,
            text,
            humanize,
        } => {
            manager
                .type_text(&session_id, &target, &text, humanize)
                .await?;
            Ok(json!({ "typed": target }))
        }
        DaemonOp::Select {
            session_id,
            target,
            value,
        } => {
            let selected = manager.select(&session_id, &target, &value).await?;
            Ok(json!({ "selected": selected }))
        }
        DaemonOp::Upload {
            session_id,
            target,
            path,
        } => {
            manager.upload(&session_id, &target, path).await?;
            Ok(json!({ "uploaded": target }))
        }
        DaemonOp::Scroll {
            session_id,
            dx,
            dy,
            humanize,
        } => {
            manager.scroll(&session_id, dx, dy, humanize).await?;
            Ok(json!({ "dx": dx, "dy": dy }))
        }
        DaemonOp::Press { session_id, key } => {
            manager.press(&session_id, &key).await?;
            Ok(json!({ "pressed": key }))
        }
        DaemonOp::Wait {
            session_id,
            request,
        } => {
            manager.wait(&session_id, request).await?;
            Ok(json!({ "waited": true }))
        }
        DaemonOp::Batch {
            session_id,
            actions,
        } => {
            let parsed = parse_batch_actions(actions)?;
            let report = manager.batch(&session_id, parsed).await?;
            Ok(serde_json::to_value(report)?)
        }
        DaemonOp::Extract { session_id, mode } => manager.extract(&session_id, mode).await,
        DaemonOp::Eval {
            session_id,
            js,
            main_world,
        } => {
            let val = manager.eval(&session_id, &js, main_world).await?;
            Ok(json!({ "value": val, "main_world": main_world }))
        }
        DaemonOp::Screenshot {
            session_id,
            out_path,
            target,
            full_page,
        } => Ok(serde_json::to_value(
            manager
                .screenshot(&session_id, &out_path, target.as_deref(), full_page)
                .await?,
        )?),
        DaemonOp::Pdf {
            session_id,
            out_path,
        } => Ok(serde_json::to_value(
            manager.pdf(&session_id, &out_path).await?,
        )?),
        DaemonOp::Logs { session_id, kind } => manager.logs(&session_id, kind).await,
        DaemonOp::EnvReport { session_id } => manager.env_report(&session_id).await,
    }
}
