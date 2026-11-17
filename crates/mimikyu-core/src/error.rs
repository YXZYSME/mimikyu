use thiserror::Error;

pub type Result<T> = std::result::Result<T, MimikyuError>;

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

    #[error("session already disposed")]
    Disposed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL parse error: {0}")]
    Url(#[from] url::ParseError),
}
