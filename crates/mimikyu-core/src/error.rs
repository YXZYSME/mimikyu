use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, MimikyuError>;

/// Neutral, machine-readable error category for automated caller routing
/// (proxy rotation, retry with backoff, engine fallback, or permanent failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Proxy,
    Network,
    Timeout,
    Protocol,
    Config,
    Session,
    Element,
    Eval,
    Io,
    Unsupported,
    Internal,
}

impl ErrorCategory {
    /// Every defined [`ErrorCategory`] variant in canonical order.
    pub const ALL: &'static [Self] = &[
        Self::Proxy,
        Self::Network,
        Self::Timeout,
        Self::Protocol,
        Self::Config,
        Self::Session,
        Self::Element,
        Self::Eval,
        Self::Io,
        Self::Unsupported,
        Self::Internal,
    ];

    /// Canonical `snake_case` string matching serde serialization.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
            Self::Config => "config",
            Self::Session => "session",
            Self::Element => "element",
            Self::Eval => "eval",
            Self::Io => "io",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Deterministic structured classification of a [`MimikyuError`] (or runtime
/// error) for wire envelopes across CLI `--json`, Daemon NDJSON, Batch, and MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ErrorClassification {
    pub error_code: &'static str,
    pub category: ErrorCategory,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl ErrorClassification {
    /// Canonical list of every `error_code` defined by the structured taxonomy.
    pub const ALL_ERROR_CODES: &'static [&'static str] = &[
        "proxy_tunnel_failed",
        "proxy_connection_failed",
        "proxy_auth_failed",
        "dns_not_resolved",
        "empty_response",
        "connection_reset",
        "connection_refused",
        "network_timeout",
        "tls_error",
        "navigation_aborted",
        "navigation_failed",
        "navigation_timeout",
        "wait_timeout",
        "rpc_timeout",
        "operation_timeout",
        "protocol_context_destroyed",
        "protocol_error",
        "transport_closed",
        "invalid_config",
        "cli_usage_error",
        "main_world_disabled",
        "session_not_found",
        "session_disposed",
        "profile_locked",
        "paused_for_human",
        "browser_not_found",
        "launch_failed",
        "element_not_found",
        "eval_failed",
        "io_error",
        "unsupported_feature",
        "internal_error",
    ];

    pub const fn new(error_code: &'static str, category: ErrorCategory, retryable: bool) -> Self {
        Self {
            error_code,
            category,
            retryable,
            protocol_code: None,
            timeout_ms: None,
        }
    }

    /// Classification for CLI argument / usage parse failures (`clap::Error`).
    pub const fn cli_usage() -> Self {
        Self::new("cli_usage_error", ErrorCategory::Config, false)
    }

    /// Classification for fallback / unexpected runtime errors.
    pub const fn internal() -> Self {
        Self::new("internal_error", ErrorCategory::Internal, false)
    }
}

#[derive(Debug, Error)]
pub enum MimikyuError {
    #[error("browser binary not found: {0}")]
    BrowserNotFound(String),

    #[error("failed to launch browser process: {0}")]
    LaunchFailed(String),

    #[error("protocol transport error: {0}")]
    Transport(String),

    #[error("protocol RPC error ({code}): {message}")]
    Protocol { code: i64, message: String },

    #[error("operation timed out after {timeout_ms}ms: {operation}")]
    Timeout { timeout_ms: u64, operation: String },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("profile is locked by another process: {0}")]
    ProfileLocked(String),

    #[error("session or target not found: {0}")]
    NotFound(String),

    #[error("element or node not found: {0}")]
    ElementNotFound(String),

    #[error("navigation failed: {0}")]
    NavigationFailed(String),

    #[error("JavaScript evaluation failed: {0}")]
    EvalFailed(String),

    #[error(
        "main-world evaluation is disabled (opt-in required via main_world_eval / --main-world)"
    )]
    MainWorldDisabled,

    #[error("unsupported by {engine} driver: {feature}")]
    Unsupported { engine: String, feature: String },

    #[error("session '{session_id}' is paused for human takeover{}; resume with `mimikyu session resume` or `mimikyu_resume_from_human`", .reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    PausedForHuman {
        session_id: String,
        reason: Option<String>,
    },

    #[error("session already disposed")]
    Disposed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL parse error: {0}")]
    Url(#[from] url::ParseError),
}

impl MimikyuError {
    /// Classify this error into a deterministic, machine-readable
    /// [`ErrorClassification`] normalizing Chromium (`net::ERR_*`) and Firefox
    /// (`NS_ERROR_*`, `SEC_ERROR_*`, `SSL_ERROR_*`) error strings as well as
    /// CDP `-32000` and WebDriver BiDi transient protocol errors.
    pub fn classify(&self) -> ErrorClassification {
        match self {
            Self::BrowserNotFound(_) => {
                ErrorClassification::new("browser_not_found", ErrorCategory::Session, false)
            }
            Self::LaunchFailed(_) => {
                ErrorClassification::new("launch_failed", ErrorCategory::Session, true)
            }
            Self::Transport(_) => {
                ErrorClassification::new("transport_closed", ErrorCategory::Protocol, true)
            }
            Self::Protocol { code, message } => classify_protocol_error(Some(*code), message),
            Self::Timeout {
                timeout_ms,
                operation,
            } => classify_timeout_operation(Some(*timeout_ms), operation),
            Self::InvalidConfig(_) | Self::Json(_) | Self::Url(_) => {
                ErrorClassification::new("invalid_config", ErrorCategory::Config, false)
            }
            Self::ProfileLocked(_) => {
                ErrorClassification::new("profile_locked", ErrorCategory::Session, true)
            }
            Self::NotFound(_) => {
                ErrorClassification::new("session_not_found", ErrorCategory::Session, false)
            }
            Self::ElementNotFound(_) => {
                ErrorClassification::new("element_not_found", ErrorCategory::Element, false)
            }
            Self::NavigationFailed(msg) => classify_navigation_message(msg),
            Self::EvalFailed(_) => {
                ErrorClassification::new("eval_failed", ErrorCategory::Eval, false)
            }
            Self::MainWorldDisabled => {
                ErrorClassification::new("main_world_disabled", ErrorCategory::Config, false)
            }
            Self::Unsupported { .. } => {
                ErrorClassification::new("unsupported_feature", ErrorCategory::Unsupported, false)
            }
            Self::PausedForHuman { .. } => {
                ErrorClassification::new("paused_for_human", ErrorCategory::Session, false)
            }
            Self::Disposed => {
                ErrorClassification::new("session_disposed", ErrorCategory::Session, false)
            }
            Self::Io(_) => ErrorClassification::new("io_error", ErrorCategory::Io, false),
        }
    }
}

pub(crate) fn classify_navigation_message(msg: &str) -> ErrorClassification {
    let upper = msg.to_ascii_uppercase();

    // Proxy errors are checked first so specific Firefox NS_ERROR_PROXY_* tokens
    // take precedence over broader proxy or network rules.
    if upper.contains("ERR_PROXY_AUTH_UNSUPPORTED")
        || upper.contains("ERR_PROXY_AUTH_REQUESTED")
        || upper.contains("ERR_UNEXPECTED_PROXY_AUTH")
        || upper.contains("ERR_PROXY_CERTIFICATE_INVALID")
        || upper.contains("NS_ERROR_PROXY_AUTHENTICATION_FAILED")
    {
        return ErrorClassification::new("proxy_auth_failed", ErrorCategory::Proxy, false);
    }

    if upper.contains("ERR_TUNNEL_CONNECTION_FAILED")
        || upper.contains("NS_ERROR_PROXY_CONNECTION_REFUSED")
    {
        return ErrorClassification::new("proxy_tunnel_failed", ErrorCategory::Proxy, true);
    }

    if upper.contains("ERR_PROXY_CONNECTION_FAILED")
        || upper.contains("ERR_NO_SUPPORTED_PROXIES")
        || upper.contains("ERR_SOCKS_CONNECTION_FAILED")
        || upper.contains("ERR_SOCKS_CONNECTION_HOST_UNREACHABLE")
        || upper.contains("NS_ERROR_PROXY_")
        || upper.contains("NS_ERROR_UNKNOWN_PROXY_HOST")
    {
        return ErrorClassification::new("proxy_connection_failed", ErrorCategory::Proxy, true);
    }

    if upper.contains("ERR_NAME_NOT_RESOLVED")
        || upper.contains("ERR_NAME_RESOLUTION_FAILED")
        || upper.contains("NS_ERROR_UNKNOWN_HOST")
    {
        return ErrorClassification::new("dns_not_resolved", ErrorCategory::Network, false);
    }

    if upper.contains("ERR_EMPTY_RESPONSE") {
        return ErrorClassification::new("empty_response", ErrorCategory::Network, true);
    }

    if upper.contains("ERR_CONNECTION_RESET")
        || upper.contains("ERR_CONNECTION_CLOSED")
        || upper.contains("ERR_CONNECTION_ABORTED")
        || upper.contains("NS_ERROR_NET_RESET")
        || upper.contains("NS_ERROR_NET_INTERRUPT")
        || upper.contains("NS_ERROR_NET_PARTIAL_TRANSFER")
    {
        return ErrorClassification::new("connection_reset", ErrorCategory::Network, true);
    }

    if upper.contains("ERR_CONNECTION_REFUSED")
        || upper.contains("ERR_ADDRESS_UNREACHABLE")
        || upper.contains("NS_ERROR_CONNECTION_REFUSED")
    {
        return ErrorClassification::new("connection_refused", ErrorCategory::Network, true);
    }

    if upper.contains("ERR_CONNECTION_TIMED_OUT")
        || upper.contains("ERR_TIMED_OUT")
        || upper.contains("NS_ERROR_NET_TIMEOUT")
    {
        return ErrorClassification::new("network_timeout", ErrorCategory::Network, true);
    }

    if upper.contains("ERR_CERT_")
        || upper.contains("ERR_SSL_")
        || upper.contains("SEC_ERROR_")
        || upper.contains("SSL_ERROR_")
        || upper.contains("MOZILLA_PKIX_ERROR_")
    {
        return ErrorClassification::new("tls_error", ErrorCategory::Network, false);
    }

    if upper.contains("ERR_ABORTED") || upper.contains("NS_BINDING_ABORTED") {
        return ErrorClassification::new("navigation_aborted", ErrorCategory::Network, true);
    }

    ErrorClassification::new("navigation_failed", ErrorCategory::Network, false)
}

pub(crate) fn classify_timeout_operation(
    timeout_ms: Option<u64>,
    operation: &str,
) -> ErrorClassification {
    let op = operation.trim().to_ascii_lowercase();
    let error_code = if op.starts_with("wait_") {
        "wait_timeout"
    } else if op.contains("document.readystate")
        || op.contains("page.navigate")
        || op.contains("page.reload")
        || op.contains("browsingcontext.navigate")
        || op.contains("browsingcontext.reload")
        || op.contains("browsingcontext.traversehistory")
        || op.contains("navigate")
        || op.contains("navigation")
        || op.contains("domcontentloaded")
    {
        "navigation_timeout"
    } else if op.starts_with("cdp ") || op.starts_with("bidi ") || op.contains("rpc") {
        "rpc_timeout"
    } else {
        "operation_timeout"
    };

    ErrorClassification {
        error_code,
        category: ErrorCategory::Timeout,
        retryable: true,
        protocol_code: None,
        timeout_ms,
    }
}

pub(crate) fn classify_protocol_error(
    protocol_code: Option<i64>,
    message: &str,
) -> ErrorClassification {
    let transient = is_transient_protocol_context_error(message);
    ErrorClassification {
        error_code: if transient {
            "protocol_context_destroyed"
        } else {
            "protocol_error"
        },
        category: ErrorCategory::Protocol,
        retryable: transient,
        protocol_code,
        timeout_ms: None,
    }
}

fn is_transient_protocol_context_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // Chromium CDP transient execution-context / object-handle / frame destruction
    if lower.contains("cannot find context")
        || lower.contains("cannot find default execution context")
        || lower.contains("execution context was destroyed")
        || lower.contains("execution context is not available")
        || lower.contains("could not find object with given id")
        || lower.contains("no frame for given id found")
        || lower.contains("inspected target navigated or closed")
    {
        return true;
    }

    // Firefox WebDriver BiDi transient context / frame / handle / realm destruction
    if lower.contains("no such frame")
        || lower.contains("no such context")
        || lower.contains("unknown context")
        || lower.contains("no such node")
        || lower.contains("no such handle")
        || lower.contains("no such realm")
        || lower.contains("no such script")
        || (lower.contains("browsing context")
            && (lower.contains("not found")
                || lower.contains("discarded")
                || lower.contains("destroyed")
                || lower.contains("closed")))
        || (lower.contains("execution context") && lower.contains("destroyed"))
    {
        return true;
    }

    false
}
