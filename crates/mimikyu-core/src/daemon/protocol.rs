use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{MimikyuError, Result};
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
            error: Some(DaemonError {
                kind,
                message: message.into(),
                code: None,
                timeout_ms: None,
            }),
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
}

impl DaemonError {
    fn simple(kind: DaemonErrorKind, message: String) -> Self {
        Self {
            kind,
            message,
            code: None,
            timeout_ms: None,
        }
    }

    pub fn from_mimikyu(err: &MimikyuError) -> Self {
        match err {
            MimikyuError::BrowserNotFound(msg) => {
                Self::simple(DaemonErrorKind::BrowserNotFound, msg.clone())
            }
            MimikyuError::LaunchFailed(msg) => {
                Self::simple(DaemonErrorKind::LaunchFailed, msg.clone())
            }
            MimikyuError::Transport(msg) => Self::simple(DaemonErrorKind::Transport, msg.clone()),
            MimikyuError::Protocol { code, message } => Self {
                kind: DaemonErrorKind::Protocol,
                message: message.clone(),
                code: Some(*code),
                timeout_ms: None,
            },
            MimikyuError::Timeout {
                timeout_ms,
                operation: _,
            } => Self {
                kind: DaemonErrorKind::Timeout,
                message: err.to_string(),
                code: None,
                timeout_ms: Some(*timeout_ms),
            },
            MimikyuError::InvalidConfig(msg) => {
                Self::simple(DaemonErrorKind::InvalidConfig, msg.clone())
            }
            MimikyuError::ProfileLocked(msg) => {
                Self::simple(DaemonErrorKind::ProfileLocked, msg.clone())
            }
            MimikyuError::NotFound(msg) => Self::simple(DaemonErrorKind::NotFound, msg.clone()),
            MimikyuError::ElementNotFound(msg) => {
                Self::simple(DaemonErrorKind::ElementNotFound, msg.clone())
            }
            MimikyuError::NavigationFailed(msg) => {
                Self::simple(DaemonErrorKind::NavigationFailed, msg.clone())
            }
            MimikyuError::EvalFailed(msg) => {
                Self::simple(DaemonErrorKind::EvalFailed, msg.clone())
            }
            MimikyuError::MainWorldDisabled => {
                Self::simple(DaemonErrorKind::MainWorldDisabled, err.to_string())
            }
            MimikyuError::Unsupported { .. } => {
                Self::simple(DaemonErrorKind::Unsupported, err.to_string())
            }
            MimikyuError::Disposed => Self::simple(DaemonErrorKind::Disposed, err.to_string()),
            MimikyuError::Io(e) => Self::simple(DaemonErrorKind::Io, e.to_string()),
            MimikyuError::Json(e) => Self::simple(DaemonErrorKind::InvalidConfig, e.to_string()),
            MimikyuError::Url(e) => Self::simple(DaemonErrorKind::InvalidConfig, e.to_string()),
        }
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
            DaemonErrorKind::Disposed => MimikyuError::Disposed,
            DaemonErrorKind::Io => MimikyuError::Io(std::io::Error::other(self.message)),
            DaemonErrorKind::Internal => MimikyuError::Transport(self.message),
        }
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
        assert_eq!(wire.message, "operation timed out after 250ms: wait_text('NOPE')");

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
}
