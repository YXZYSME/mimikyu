use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::driver::ScreencastOptions;
use crate::error::{
    ErrorCategory, ErrorClassification, MimikyuError, Result, classify_navigation_message,
    classify_protocol_error, classify_timeout_operation,
};
use crate::input::BatchAction;
use crate::session::{ExtractMode, LogKind, SessionCreateOpts, SessionSummary, WaitRequest};
use crate::snapshot::SnapshotOptions;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub id: u64,
    #[serde(flatten)]
    pub op: DaemonOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum DaemonOp {
    Ping,
    DaemonStatus,
    DaemonStop,
    SessionNew(SessionCreateOpts),
    SessionList,
    SessionClose {
        session_id: String,
    },
    SessionExport {
        session_id: String,
        out_path: PathBuf,
    },
    SessionImport {
        session_id: String,
        in_path: PathBuf,
    },
    #[serde(rename = "session.pause", alias = "session_pause")]
    SessionPause {
        session_id: String,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(rename = "session.resume", alias = "session_resume")]
    SessionResume {
        session_id: String,
    },
    #[serde(rename = "session.takeover", alias = "session_takeover")]
    SessionTakeover {
        session_id: String,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(rename = "record.start", alias = "record_start")]
    RecordStart {
        session_id: String,
        options: ScreencastOptions,
    },
    #[serde(rename = "record.stop", alias = "record_stop")]
    RecordStop {
        session_id: String,
    },
    ProfileList,
    ProfileDelete {
        name: String,
    },
    Open {
        session_id: String,
        url: String,
    },
    Back {
        session_id: String,
    },
    Forward {
        session_id: String,
    },
    Reload {
        session_id: String,
    },
    Snapshot {
        session_id: String,
        options: SnapshotOptions,
    },
    Click {
        session_id: String,
        target: String,
        #[serde(default)]
        humanize: Option<bool>,
    },
    Fill {
        session_id: String,
        target: String,
        value: String,
        #[serde(default)]
        humanize: Option<bool>,
    },
    TypeText {
        session_id: String,
        target: String,
        text: String,
        #[serde(default)]
        humanize: Option<bool>,
    },
    Select {
        session_id: String,
        target: String,
        value: String,
    },
    Upload {
        session_id: String,
        target: String,
        path: PathBuf,
    },
    Scroll {
        session_id: String,
        dx: f64,
        dy: f64,
        #[serde(default)]
        humanize: Option<bool>,
    },
    Press {
        session_id: String,
        key: String,
    },
    Wait {
        session_id: String,
        request: WaitRequest,
    },
    Batch {
        session_id: String,
        actions: Value,
    },
    Extract {
        session_id: String,
        mode: ExtractMode,
    },
    Eval {
        session_id: String,
        js: String,
        #[serde(default)]
        main_world: bool,
    },
    Screenshot {
        session_id: String,
        out_path: PathBuf,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        full_page: bool,
    },
    Pdf {
        session_id: String,
        out_path: PathBuf,
    },
    Logs {
        session_id: String,
        kind: LogKind,
    },
    EnvReport {
        session_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DaemonError>,
}

impl DaemonResponse {
    pub fn ok(id: u64, data: Value) -> Self {
        Self {
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(id: u64, err: &MimikyuError) -> Self {
        Self {
            id,
            ok: false,
            data: None,
            error: Some(DaemonError::from_mimikyu(err)),
        }
    }

    pub fn err_message(id: u64, kind: DaemonErrorKind, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            data: None,
            error: Some(DaemonError::from_kind_and_message(kind, message)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonErrorKind {
    BrowserNotFound,
    LaunchFailed,
    Transport,
    Protocol,
    Timeout,
    InvalidConfig,
    ProfileLocked,
    NotFound,
    ElementNotFound,
    NavigationFailed,
    EvalFailed,
    MainWorldDisabled,
    Unsupported,
    PausedForHuman,
    Disposed,
    Io,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonError {
    pub kind: DaemonErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<ErrorCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl DaemonError {
    fn from_classification(
        kind: DaemonErrorKind,
        message: String,
        class: ErrorClassification,
    ) -> Self {
        Self {
            kind,
            message,
            code: class.protocol_code,
            timeout_ms: class.timeout_ms,
            error_code: Some(class.error_code.to_string()),
            category: Some(class.category),
            retryable: Some(class.retryable),
        }
    }

    pub fn from_kind_and_message(kind: DaemonErrorKind, message: impl Into<String>) -> Self {
        let message = message.into();
        let class = classify_daemon_kind(kind, &message);
        Self::from_classification(kind, message, class)
    }

    pub fn from_mimikyu(err: &MimikyuError) -> Self {
        let class = err.classify();
        let (kind, message) = match err {
            MimikyuError::BrowserNotFound(msg) => (DaemonErrorKind::BrowserNotFound, msg.clone()),
            MimikyuError::LaunchFailed(msg) => (DaemonErrorKind::LaunchFailed, msg.clone()),
            MimikyuError::Transport(msg) => (DaemonErrorKind::Transport, msg.clone()),
            MimikyuError::Protocol { message, .. } => (DaemonErrorKind::Protocol, message.clone()),
            MimikyuError::Timeout { .. } => (DaemonErrorKind::Timeout, err.to_string()),
            MimikyuError::InvalidConfig(msg) => (DaemonErrorKind::InvalidConfig, msg.clone()),
            MimikyuError::ProfileLocked(msg) => (DaemonErrorKind::ProfileLocked, msg.clone()),
            MimikyuError::NotFound(msg) => (DaemonErrorKind::NotFound, msg.clone()),
            MimikyuError::ElementNotFound(msg) => (DaemonErrorKind::ElementNotFound, msg.clone()),
            MimikyuError::NavigationFailed(msg) => (DaemonErrorKind::NavigationFailed, msg.clone()),
            MimikyuError::EvalFailed(msg) => (DaemonErrorKind::EvalFailed, msg.clone()),
            MimikyuError::MainWorldDisabled => {
                (DaemonErrorKind::MainWorldDisabled, err.to_string())
            }
            MimikyuError::Unsupported { .. } => (DaemonErrorKind::Unsupported, err.to_string()),
            MimikyuError::PausedForHuman { .. } => {
                (DaemonErrorKind::PausedForHuman, err.to_string())
            }
            MimikyuError::Disposed => (DaemonErrorKind::Disposed, err.to_string()),
            MimikyuError::Io(e) => (DaemonErrorKind::Io, e.to_string()),
            MimikyuError::Json(e) => (DaemonErrorKind::InvalidConfig, e.to_string()),
            MimikyuError::Url(e) => (DaemonErrorKind::InvalidConfig, e.to_string()),
        };
        Self::from_classification(kind, message, class)
    }

    pub fn into_mimikyu(self) -> MimikyuError {
        match self.kind {
            DaemonErrorKind::BrowserNotFound => MimikyuError::BrowserNotFound(self.message),
            DaemonErrorKind::LaunchFailed => MimikyuError::LaunchFailed(self.message),
            DaemonErrorKind::Transport => MimikyuError::Transport(self.message),
            DaemonErrorKind::Protocol => MimikyuError::Protocol {
                code: self.code.unwrap_or(-1),
                message: self.message,
            },
            DaemonErrorKind::Timeout => {
                if let Some(ms) = self.timeout_ms {
                    let prefix = format!("operation timed out after {ms}ms: ");
                    let operation = self
                        .message
                        .strip_prefix(&prefix)
                        .map(ToString::to_string)
                        .unwrap_or(self.message);
                    MimikyuError::Timeout {
                        timeout_ms: ms,
                        operation,
                    }
                } else if let Some(rest) = self.message.strip_prefix("operation timed out after ")
                    && let Some((num, op)) = rest.split_once("ms: ")
                    && let Ok(ms) = num.parse::<u64>()
                {
                    MimikyuError::Timeout {
                        timeout_ms: ms,
                        operation: op.to_string(),
                    }
                } else {
                    MimikyuError::Timeout {
                        timeout_ms: 0,
                        operation: self.message,
                    }
                }
            }
            DaemonErrorKind::InvalidConfig => MimikyuError::InvalidConfig(self.message),
            DaemonErrorKind::ProfileLocked => MimikyuError::ProfileLocked(self.message),
            DaemonErrorKind::NotFound => MimikyuError::NotFound(self.message),
            DaemonErrorKind::ElementNotFound => MimikyuError::ElementNotFound(self.message),
            DaemonErrorKind::NavigationFailed => MimikyuError::NavigationFailed(self.message),
            DaemonErrorKind::EvalFailed => MimikyuError::EvalFailed(self.message),
            DaemonErrorKind::MainWorldDisabled => MimikyuError::MainWorldDisabled,
            DaemonErrorKind::Unsupported => MimikyuError::Unsupported {
                engine: "daemon".to_string(),
                feature: self.message,
            },
            DaemonErrorKind::PausedForHuman => {
                if let Some(rest) = self.message.strip_prefix("session '")
                    && let Some((sid, after_sid)) =
                        rest.split_once("' is paused for human takeover")
                {
                    let reason = after_sid
                        .strip_prefix(" (")
                        .and_then(|r| r.split_once(");"))
                        .map(|(inner, _)| inner.to_string());
                    MimikyuError::PausedForHuman {
                        session_id: sid.to_string(),
                        reason,
                    }
                } else {
                    MimikyuError::PausedForHuman {
                        session_id: "unknown".to_string(),
                        reason: Some(self.message),
                    }
                }
            }
            DaemonErrorKind::Disposed => MimikyuError::Disposed,
            DaemonErrorKind::Io => MimikyuError::Io(std::io::Error::other(self.message)),
            DaemonErrorKind::Internal => MimikyuError::Transport(self.message),
        }
    }
}

fn classify_daemon_kind(kind: DaemonErrorKind, message: &str) -> ErrorClassification {
    match kind {
        DaemonErrorKind::BrowserNotFound => {
            ErrorClassification::new("browser_not_found", ErrorCategory::Session, false)
        }
        DaemonErrorKind::LaunchFailed => {
            ErrorClassification::new("launch_failed", ErrorCategory::Session, true)
        }
        DaemonErrorKind::Transport => {
            ErrorClassification::new("transport_closed", ErrorCategory::Protocol, true)
        }
        DaemonErrorKind::Protocol => classify_protocol_error(None, message),
        DaemonErrorKind::Timeout => {
            let op = message
                .strip_prefix("operation timed out after ")
                .and_then(|rest| rest.split_once("ms: "))
                .map(|(_, op)| op)
                .unwrap_or(message);
            classify_timeout_operation(None, op)
        }
        DaemonErrorKind::InvalidConfig => {
            ErrorClassification::new("invalid_config", ErrorCategory::Config, false)
        }
        DaemonErrorKind::ProfileLocked => {
            ErrorClassification::new("profile_locked", ErrorCategory::Session, true)
        }
        DaemonErrorKind::NotFound => {
            ErrorClassification::new("session_not_found", ErrorCategory::Session, false)
        }
        DaemonErrorKind::ElementNotFound => {
            ErrorClassification::new("element_not_found", ErrorCategory::Element, false)
        }
        DaemonErrorKind::NavigationFailed => classify_navigation_message(message),
        DaemonErrorKind::EvalFailed => {
            ErrorClassification::new("eval_failed", ErrorCategory::Eval, false)
        }
        DaemonErrorKind::MainWorldDisabled => {
            ErrorClassification::new("main_world_disabled", ErrorCategory::Config, false)
        }
        DaemonErrorKind::Unsupported => {
            ErrorClassification::new("unsupported_feature", ErrorCategory::Unsupported, false)
        }
        DaemonErrorKind::PausedForHuman => {
            ErrorClassification::new("paused_for_human", ErrorCategory::Session, false)
        }
        DaemonErrorKind::Disposed => {
            ErrorClassification::new("session_disposed", ErrorCategory::Session, false)
        }
        DaemonErrorKind::Io => ErrorClassification::new("io_error", ErrorCategory::Io, false),
        DaemonErrorKind::Internal => ErrorClassification::internal(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatusInfo {
    pub running: bool,
    pub pid: u32,
    pub state_dir: PathBuf,
    pub socket_path: PathBuf,
    pub bound_socket_path: PathBuf,
    pub uptime_ms: u64,
    pub idle_timeout_secs: Option<u64>,
    pub active_sessions: usize,
    pub last_activity_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub id: String,
    #[serde(flatten)]
    pub summary: SessionSummary,
}

impl From<SessionSummary> for SessionListEntry {
    fn from(summary: SessionSummary) -> Self {
        Self {
            id: summary.session_id.clone(),
            summary,
        }
    }
}

/// Normalize batch input JSON (`[...]`, `{"ops": [...]}`, `{"actions": [...]}`)
/// with `"op"` -> `"action"`, `upload.path` -> `files`, `wait_url.url` -> `substr`,
/// and `"op": "wait"` predicate inference.
pub fn normalize_batch_ops_value(raw: Value) -> Result<Vec<Value>> {
    let items = match raw {
        Value::Array(arr) => arr,
        Value::Object(mut obj) => {
            if let Some(ops) = obj.remove("ops") {
                match ops {
                    Value::Array(arr) => arr,
                    _ => {
                        return Err(MimikyuError::InvalidConfig(
                            "batch 'ops' field must be an array".to_string(),
                        ));
                    }
                }
            } else if let Some(actions) = obj.remove("actions") {
                match actions {
                    Value::Array(arr) => arr,
                    _ => {
                        return Err(MimikyuError::InvalidConfig(
                            "batch 'actions' field must be an array".to_string(),
                        ));
                    }
                }
            } else {
                return Err(MimikyuError::InvalidConfig(
                    "batch object must contain an 'ops' or 'actions' array".to_string(),
                ));
            }
        }
        _ => {
            return Err(MimikyuError::InvalidConfig(
                "batch input must be a JSON array or an object with 'ops'".to_string(),
            ));
        }
    };

    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.into_iter().enumerate() {
        let mut map: Map<String, Value> = match item {
            Value::Object(m) => m,
            _ => {
                return Err(MimikyuError::InvalidConfig(format!(
                    "batch step {idx} must be a JSON object"
                )));
            }
        };

        if !map.contains_key("action")
            && let Some(op_val) = map.remove("op")
        {
            map.insert("action".to_string(), op_val);
        }

        let action_name = map
            .get("action")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase());

        if let Some(ref name) = action_name {
            match name.as_str() {
                "wait" => {
                    if map.contains_key("selector") {
                        map.insert("action".to_string(), Value::String("wait_selector".into()));
                    } else if map.contains_key("text") {
                        map.insert("action".to_string(), Value::String("wait_text".into()));
                    } else if map.contains_key("url") {
                        map.insert("action".to_string(), Value::String("wait_url".into()));
                        if !map.contains_key("substr")
                            && let Some(u) = map.remove("url")
                        {
                            map.insert("substr".to_string(), u);
                        }
                    } else if map
                        .remove("idle")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || map.contains_key("network_quiet_ms")
                        || map.contains_key("target")
                        || map.contains_key("stable_target")
                    {
                        map.insert("action".to_string(), Value::String("wait_idle".into()));
                    }
                }
                "upload" => {
                    if !map.contains_key("files")
                        && let Some(path_val) = map.remove("path")
                    {
                        map.insert("files".to_string(), Value::Array(vec![path_val]));
                    }
                }
                "wait_url" => {
                    if !map.contains_key("substr")
                        && let Some(url_val) = map.remove("url")
                    {
                        map.insert("substr".to_string(), url_val);
                    }
                }
                "wait_idle" => {
                    map.remove("idle");
                }
                _ => {}
            }
        }

        out.push(Value::Object(map));
    }

    Ok(out)
}

pub fn parse_batch_actions(raw: Value) -> Result<Vec<BatchAction>> {
    let normalized = normalize_batch_ops_value(raw)?;
    let mut actions = Vec::with_capacity(normalized.len());
    for (idx, item) in normalized.into_iter().enumerate() {
        let action: BatchAction = serde_json::from_value(item).map_err(|e| {
            MimikyuError::InvalidConfig(format!("invalid batch action at step {idx}: {e}"))
        })?;
        actions.push(action);
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_error_timeout_round_trips_without_double_prefix() {
        let orig = MimikyuError::Timeout {
            timeout_ms: 250,
            operation: "wait_text('NOPE')".to_string(),
        };
        let wire = DaemonError::from_mimikyu(&orig);
        assert_eq!(wire.kind, DaemonErrorKind::Timeout);
        assert_eq!(wire.timeout_ms, Some(250));
        assert_eq!(
            wire.message,
            "operation timed out after 250ms: wait_text('NOPE')"
        );

        let json_str = serde_json::to_string(&wire).unwrap();
        let decoded: DaemonError = serde_json::from_str(&json_str).unwrap();
        let roundtrip = decoded.into_mimikyu();
        match roundtrip {
            MimikyuError::Timeout {
                timeout_ms,
                operation,
            } => {
                assert_eq!(timeout_ms, 250);
                assert_eq!(operation, "wait_text('NOPE')");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert_eq!(
            DaemonError::from_mimikyu(&orig).into_mimikyu().to_string(),
            "operation timed out after 250ms: wait_text('NOPE')"
        );

        // Legacy payload without timeout_ms still unwraps cleanly
        let legacy: DaemonError = serde_json::from_value(serde_json::json!({
            "kind": "timeout",
            "message": "operation timed out after 150ms: wait_idle"
        }))
        .unwrap();
        assert_eq!(
            legacy.into_mimikyu().to_string(),
            "operation timed out after 150ms: wait_idle"
        );
    }

    #[test]
    fn hitl_and_record_daemon_ops_and_paused_error_round_trip() {
        let pause_req = DaemonRequest {
            id: 7,
            op: DaemonOp::SessionPause {
                session_id: "s1".to_string(),
                reason: Some("complete 2FA".to_string()),
            },
        };
        let encoded = serde_json::to_string(&pause_req).unwrap();
        assert!(
            encoded.contains("\"method\":\"session.pause\""),
            "{encoded}"
        );
        let decoded: DaemonRequest = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded.op, DaemonOp::SessionPause { .. }));

        // Also accept snake_case alias
        let alias_dec: DaemonRequest = serde_json::from_str(
            r#"{"id":8,"method":"session_resume","params":{"session_id":"s1"}}"#,
        )
        .unwrap();
        assert!(matches!(alias_dec.op, DaemonOp::SessionResume { .. }));

        let takeover_dec: DaemonRequest = serde_json::from_str(
            r#"{"id":9,"method":"session.takeover","params":{"session_id":"s1","reason":"login"}}"#,
        )
        .unwrap();
        assert!(matches!(takeover_dec.op, DaemonOp::SessionTakeover { .. }));

        let rec_stop: DaemonRequest = serde_json::from_str(
            r#"{"id":10,"method":"record.stop","params":{"session_id":"s1"}}"#,
        )
        .unwrap();
        assert!(matches!(rec_stop.op, DaemonOp::RecordStop { .. }));

        let orig = MimikyuError::PausedForHuman {
            session_id: "vault-1".to_string(),
            reason: Some("confirm sign-in".to_string()),
        };
        let wire = DaemonError::from_mimikyu(&orig);
        assert_eq!(wire.kind, DaemonErrorKind::PausedForHuman);
        let roundtrip = wire.into_mimikyu();
        match roundtrip {
            MimikyuError::PausedForHuman { session_id, reason } => {
                assert_eq!(session_id, "vault-1");
                assert_eq!(reason.as_deref(), Some("confirm sign-in"));
            }
            other => panic!("expected PausedForHuman, got {other:?}"),
        }
    }
}
