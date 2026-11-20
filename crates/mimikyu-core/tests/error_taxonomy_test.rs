use std::collections::BTreeSet;

use mimikyu_core::daemon::protocol::{DaemonError, DaemonErrorKind, DaemonResponse};
use mimikyu_core::error::{ErrorCategory, ErrorClassification, MimikyuError};
use serde::Deserialize;
use serde_json::{Value, json};

/// Mirrors `crates/mimikyu-cli/tests/cli_contract_test.rs` (`r3_forbidden_keywords`)
/// using `concat!` so prohibited terms never appear literally in source files.
fn r3_forbidden_keywords() -> Vec<&'static str> {
    vec![
        concat!("disable_", "coop"),
        concat!("cap", "tcha"),
        concat!("recap", "tcha"),
        concat!("hcap", "tcha"),
        concat!("turn", "stile"),
        concat!("sun", "dial"),
        concat!("cf_", "clearance"),
        concat!("waf_", "bypass"),
        concat!("waf_", "evasion"),
        concat!("waf ", "evasion"),
        concat!("gate_", "bypass"),
        concat!("gate ", "bypass"),
        concat!("challenge_", "bypass"),
        concat!("challenge_", "solver"),
        concat!("anti", "bot"),
        concat!("anti-", "bot"),
        concat!("human_", "verification"),
        concat!("human-", "verification"),
    ]
}

fn assert_r3_clean(text: &str, context: &str) {
    let lower = text.to_ascii_lowercase();
    for kw in r3_forbidden_keywords() {
        assert!(
            !lower.contains(kw),
            "R3 violation in {context}: found forbidden term '{kw}' in '{text}'"
        );
    }
}

#[test]
fn all_18_mimikyu_error_variants_classify_deterministically() {
    let json_err = serde_json::from_str::<Value>("{bad").unwrap_err();
    let url_err = url::Url::parse("://not-a-url").unwrap_err();
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");

    let cases: Vec<(MimikyuError, ErrorClassification)> = vec![
        (
            MimikyuError::BrowserNotFound("chromium not installed".into()),
            ErrorClassification::new("browser_not_found", ErrorCategory::Session, false),
        ),
        (
            MimikyuError::LaunchFailed("exec format error".into()),
            ErrorClassification::new("launch_failed", ErrorCategory::Session, true),
        ),
        (
            MimikyuError::Transport("CDP websocket closed".into()),
            ErrorClassification::new("transport_closed", ErrorCategory::Protocol, true),
        ),
        (
            MimikyuError::Protocol {
                code: -32601,
                message: "'Bogus.method' wasn't found".into(),
            },
            ErrorClassification {
                error_code: "protocol_error",
                category: ErrorCategory::Protocol,
                retryable: false,
                protocol_code: Some(-32601),
                timeout_ms: None,
            },
        ),
        (
            MimikyuError::Timeout {
                timeout_ms: 5000,
                operation: "wait_selector('#ready', Visible)".into(),
            },
            ErrorClassification {
                error_code: "wait_timeout",
                category: ErrorCategory::Timeout,
                retryable: true,
                protocol_code: None,
                timeout_ms: Some(5000),
            },
        ),
        (
            MimikyuError::InvalidConfig("timeout_ms must be > 0".into()),
            ErrorClassification::new("invalid_config", ErrorCategory::Config, false),
        ),
        (
            MimikyuError::ProfileLocked("/tmp/profile.lock".into()),
            ErrorClassification::new("profile_locked", ErrorCategory::Session, true),
        ),
        (
            MimikyuError::NotFound("session s-404".into()),
            ErrorClassification::new("session_not_found", ErrorCategory::Session, false),
        ),
        (
            MimikyuError::ElementNotFound("@e99".into()),
            ErrorClassification::new("element_not_found", ErrorCategory::Element, false),
        ),
        (
            MimikyuError::NavigationFailed("empty navigation history".into()),
            ErrorClassification::new("navigation_failed", ErrorCategory::Network, false),
        ),
        (
            MimikyuError::EvalFailed("ReferenceError: x is not defined".into()),
            ErrorClassification::new("eval_failed", ErrorCategory::Eval, false),
        ),
        (
            MimikyuError::MainWorldDisabled,
            ErrorClassification::new("main_world_disabled", ErrorCategory::Config, false),
        ),
        (
            MimikyuError::Unsupported {
                engine: "firefox".into(),
                feature: "cdp-only feature".into(),
            },
            ErrorClassification::new("unsupported_feature", ErrorCategory::Unsupported, false),
        ),
        (
            MimikyuError::PausedForHuman {
                session_id: "s1".into(),
                reason: Some("complete sign-in".into()),
            },
            ErrorClassification::new("paused_for_human", ErrorCategory::Session, false),
        ),
        (
            MimikyuError::Disposed,
            ErrorClassification::new("session_disposed", ErrorCategory::Session, false),
        ),
        (
            MimikyuError::Io(io_err),
            ErrorClassification::new("io_error", ErrorCategory::Io, false),
        ),
        (
            MimikyuError::Json(json_err),
            ErrorClassification::new("invalid_config", ErrorCategory::Config, false),
        ),
        (
            MimikyuError::Url(url_err),
            ErrorClassification::new("invalid_config", ErrorCategory::Config, false),
        ),
    ];

    assert_eq!(
        cases.len(),
        18,
        "all 18 MimikyuError variants must be tested"
    );
    for (err, expected) in cases {
        assert_eq!(err.classify(), expected, "mismatch for error: {err:?}");
    }
}

#[test]
fn dual_engine_navigation_errors_normalize_chromium_and_firefox() {
    let cases: &[(&str, &str, ErrorCategory, bool)] = &[
        // Proxy tunnel failures
        (
            "net::ERR_TUNNEL_CONNECTION_FAILED",
            "proxy_tunnel_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            "protocol RPC error (-1): unknown error: Error: NS_ERROR_PROXY_CONNECTION_REFUSED",
            "proxy_tunnel_failed",
            ErrorCategory::Proxy,
            true,
        ),
        // Proxy connection failures
        (
            "net::ERR_PROXY_CONNECTION_FAILED",
            "proxy_connection_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            "net::ERR_NO_SUPPORTED_PROXIES",
            "proxy_connection_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            "net::ERR_SOCKS_CONNECTION_FAILED",
            "proxy_connection_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            "unknown error: Error: NS_ERROR_PROXY_BAD_GATEWAY",
            "proxy_connection_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            "unknown error: Error: NS_ERROR_PROXY_GATEWAY_TIMEOUT",
            "proxy_connection_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            "unknown error: Error: NS_ERROR_UNKNOWN_PROXY_HOST",
            "proxy_connection_failed",
            ErrorCategory::Proxy,
            true,
        ),
        // Proxy authentication / certificate failures (non-retryable)
        (
            "net::ERR_PROXY_AUTH_UNSUPPORTED",
            "proxy_auth_failed",
            ErrorCategory::Proxy,
            false,
        ),
        (
            "net::ERR_PROXY_CERTIFICATE_INVALID",
            "proxy_auth_failed",
            ErrorCategory::Proxy,
            false,
        ),
        (
            "unknown error: Error: NS_ERROR_PROXY_AUTHENTICATION_FAILED",
            "proxy_auth_failed",
            ErrorCategory::Proxy,
            false,
        ),
        // DNS resolution failures
        (
            "net::ERR_NAME_NOT_RESOLVED",
            "dns_not_resolved",
            ErrorCategory::Network,
            false,
        ),
        (
            "unknown error: Error: NS_ERROR_UNKNOWN_HOST",
            "dns_not_resolved",
            ErrorCategory::Network,
            false,
        ),
        // Empty response
        (
            "net::ERR_EMPTY_RESPONSE",
            "empty_response",
            ErrorCategory::Network,
            true,
        ),
        // Connection reset / interrupted
        (
            "net::ERR_CONNECTION_RESET",
            "connection_reset",
            ErrorCategory::Network,
            true,
        ),
        (
            "net::ERR_CONNECTION_CLOSED",
            "connection_reset",
            ErrorCategory::Network,
            true,
        ),
        (
            "net::ERR_CONNECTION_ABORTED",
            "connection_reset",
            ErrorCategory::Network,
            true,
        ),
        (
            "NS_ERROR_NET_RESET",
            "connection_reset",
            ErrorCategory::Network,
            true,
        ),
        (
            "NS_ERROR_NET_INTERRUPT",
            "connection_reset",
            ErrorCategory::Network,
            true,
        ),
        (
            "NS_ERROR_NET_PARTIAL_TRANSFER",
            "connection_reset",
            ErrorCategory::Network,
            true,
        ),
        // Connection refused / unreachable
        (
            "net::ERR_CONNECTION_REFUSED",
            "connection_refused",
            ErrorCategory::Network,
            true,
        ),
        (
            "net::ERR_ADDRESS_UNREACHABLE",
            "connection_refused",
            ErrorCategory::Network,
            true,
        ),
        (
            "unknown error: Error: NS_ERROR_CONNECTION_REFUSED",
            "connection_refused",
            ErrorCategory::Network,
            true,
        ),
        // Network timeout
        (
            "net::ERR_CONNECTION_TIMED_OUT",
            "network_timeout",
            ErrorCategory::Network,
            true,
        ),
        (
            "net::ERR_TIMED_OUT",
            "network_timeout",
            ErrorCategory::Network,
            true,
        ),
        (
            "unknown error: Error: NS_ERROR_NET_TIMEOUT",
            "network_timeout",
            ErrorCategory::Network,
            true,
        ),
        // TLS / certificate errors
        (
            "net::ERR_CERT_AUTHORITY_INVALID",
            "tls_error",
            ErrorCategory::Network,
            false,
        ),
        (
            "net::ERR_CERT_COMMON_NAME_INVALID",
            "tls_error",
            ErrorCategory::Network,
            false,
        ),
        (
            "net::ERR_SSL_PROTOCOL_ERROR",
            "tls_error",
            ErrorCategory::Network,
            false,
        ),
        (
            "unknown error: Error: SEC_ERROR_UNKNOWN_ISSUER",
            "tls_error",
            ErrorCategory::Network,
            false,
        ),
        (
            "unknown error: Error: SSL_ERROR_BAD_CERT_DOMAIN",
            "tls_error",
            ErrorCategory::Network,
            false,
        ),
        (
            "unknown error: Error: MOZILLA_PKIX_ERROR_SELF_SIGNED_CERT",
            "tls_error",
            ErrorCategory::Network,
            false,
        ),
        // Navigation aborted
        (
            "net::ERR_ABORTED",
            "navigation_aborted",
            ErrorCategory::Network,
            true,
        ),
        (
            "unknown error: Error: NS_BINDING_ABORTED",
            "navigation_aborted",
            ErrorCategory::Network,
            true,
        ),
        // Case-insensitive token matching
        (
            "navigation failed: net::err_tunnel_connection_failed",
            "proxy_tunnel_failed",
            ErrorCategory::Proxy,
            true,
        ),
        // Fallback generic navigation error
        (
            "net::ERR_TOO_MANY_REDIRECTS",
            "navigation_failed",
            ErrorCategory::Network,
            false,
        ),
        (
            "empty navigation history",
            "navigation_failed",
            ErrorCategory::Network,
            false,
        ),
    ];

    for &(msg, expected_code, expected_cat, expected_retryable) in cases {
        let err = MimikyuError::NavigationFailed(msg.to_string());
        let got = err.classify();
        assert_eq!(
            got,
            ErrorClassification::new(expected_code, expected_cat, expected_retryable),
            "navigation classification mismatch for '{msg}'"
        );
    }
}

#[test]
fn timeout_operations_classify_into_four_canonical_codes() {
    let cases: &[(&str, u64, &str)] = &[
        // wait_timeout (including wait_idle whose message mentions readyState and wait_url with navigate)
        ("wait_text('NOPE')", 250, "wait_timeout"),
        (
            "wait_url('https://example.com/navigate')",
            1000,
            "wait_timeout",
        ),
        ("wait_selector('#submit', Visible)", 2000, "wait_timeout"),
        (
            "wait_idle (readyState=loading, inflight=2)",
            5000,
            "wait_timeout",
        ),
        // navigation_timeout
        (
            "waiting for document.readyState == 'complete'",
            30_000,
            "navigation_timeout",
        ),
        (
            "waiting for document.readyState == 'interactive'",
            15_000,
            "navigation_timeout",
        ),
        ("CDP Page.navigate", 30_000, "navigation_timeout"),
        ("CDP Page.reload", 30_000, "navigation_timeout"),
        (
            "CDP Page.navigateToHistoryEntry",
            30_000,
            "navigation_timeout",
        ),
        (
            "BiDi browsingContext.navigate",
            30_000,
            "navigation_timeout",
        ),
        ("BiDi browsingContext.reload", 30_000, "navigation_timeout"),
        (
            "BiDi browsingContext.traverseHistory",
            30_000,
            "navigation_timeout",
        ),
        ("waiting for DOMContentLoaded", 10_000, "navigation_timeout"),
        // rpc_timeout
        ("CDP Runtime.evaluate", 30_000, "rpc_timeout"),
        ("CDP DOM.getDocument", 30_000, "rpc_timeout"),
        ("BiDi script.evaluate", 30_000, "rpc_timeout"),
        (
            "BiDi browsingContext.captureScreenshot",
            30_000,
            "rpc_timeout",
        ),
        ("custom rpc call", 3_000, "rpc_timeout"),
        // operation_timeout
        (
            "waiting for Chromium DevTools WS endpoint (timeout)",
            10_000,
            "operation_timeout",
        ),
        (
            "waiting for Firefox WebDriver BiDi endpoint (timeout)",
            10_000,
            "operation_timeout",
        ),
        (
            "waiting for background daemon startup",
            10_000,
            "operation_timeout",
        ),
        ("stopping daemon pid 4242", 8_000, "operation_timeout"),
        ("click", 2_000, "operation_timeout"),
    ];

    for &(op, ms, expected_code) in cases {
        let err = MimikyuError::Timeout {
            timeout_ms: ms,
            operation: op.to_string(),
        };
        assert_eq!(
            err.classify(),
            ErrorClassification {
                error_code: expected_code,
                category: ErrorCategory::Timeout,
                retryable: true,
                protocol_code: None,
                timeout_ms: Some(ms),
            },
            "timeout classification mismatch for op '{op}'"
        );
    }
}

#[test]
fn protocol_errors_distinguish_transient_context_destruction_from_permanent() {
    let transient_cases: &[(i64, &str)] = &[
        (-32000, "Cannot find context with specified id"),
        (-32000, "Cannot find default execution context"),
        (-32000, "Execution context was destroyed."),
        (-32000, "Execution context is not available"),
        (-32000, "Could not find object with given id"),
        (-32000, "No frame for given id found"),
        (-32000, "Inspected target navigated or closed"),
        (-1, "no such frame: Browsing context abc not found"),
        (-1, "no such context: Context 123 was closed"),
        (-1, "unknown context: context not registered"),
        (-1, "no such node: SharedId 123 is stale"),
        (-1, "no such handle: Remote object handle not found"),
        (-1, "no such realm: Realm was destroyed"),
        (-1, "unknown error: browsing context has been discarded"),
    ];

    for &(code, msg) in transient_cases {
        let err = MimikyuError::Protocol {
            code,
            message: msg.to_string(),
        };
        assert_eq!(
            err.classify(),
            ErrorClassification {
                error_code: "protocol_context_destroyed",
                category: ErrorCategory::Protocol,
                retryable: true,
                protocol_code: Some(code),
                timeout_ms: None,
            },
            "expected transient protocol classification for ({code}) '{msg}'"
        );
    }

    let permanent_cases: &[(i64, &str)] = &[
        (-32000, "Invalid timezone ID: Mars/Olympus"),
        (-32601, "'Bogus.method' wasn't found"),
        (
            -32602,
            "Invalid parameters: Failed to deserialize params.url",
        ),
        (-1, "invalid argument: Expected \"url\" to be a string"),
        (-1, "unsupported operation: Not implemented"),
    ];

    for &(code, msg) in permanent_cases {
        let err = MimikyuError::Protocol {
            code,
            message: msg.to_string(),
        };
        assert_eq!(
            err.classify(),
            ErrorClassification {
                error_code: "protocol_error",
                category: ErrorCategory::Protocol,
                retryable: false,
                protocol_code: Some(code),
                timeout_ms: None,
            },
            "expected non-retryable protocol classification for ({code}) '{msg}'"
        );
    }
}

#[test]
fn error_category_and_classification_serialization_contract() {
    for &cat in ErrorCategory::ALL {
        let serialized = serde_json::to_string(&cat).unwrap();
        let expected_quoted = format!("\"{}\"", cat.as_str());
        assert_eq!(serialized, expected_quoted);
        assert_eq!(cat.to_string(), cat.as_str());
        let decoded: ErrorCategory = serde_json::from_str(&serialized).unwrap();
        assert_eq!(decoded, cat);
    }

    // Optional fields omitted when None
    let simple = ErrorClassification::new("dns_not_resolved", ErrorCategory::Network, false);
    let simple_val = serde_json::to_value(simple).unwrap();
    assert_eq!(
        simple_val,
        json!({
            "error_code": "dns_not_resolved",
            "category": "network",
            "retryable": false
        })
    );

    // protocol_code included when present
    let proto = MimikyuError::Protocol {
        code: -32000,
        message: "Cannot find context with specified id".into(),
    }
    .classify();
    let proto_val = serde_json::to_value(proto).unwrap();
    assert_eq!(
        proto_val,
        json!({
            "error_code": "protocol_context_destroyed",
            "category": "protocol",
            "retryable": true,
            "protocol_code": -32000
        })
    );

    // timeout_ms included when present
    let timeout = MimikyuError::Timeout {
        timeout_ms: 12_000,
        operation: "CDP Page.navigate".into(),
    }
    .classify();
    let timeout_val = serde_json::to_value(timeout).unwrap();
    assert_eq!(
        timeout_val,
        json!({
            "error_code": "navigation_timeout",
            "category": "timeout",
            "retryable": true,
            "timeout_ms": 12000
        })
    );

    // Helper constructors for CLI usage and internal errors
    assert_eq!(
        ErrorClassification::cli_usage(),
        ErrorClassification::new("cli_usage_error", ErrorCategory::Config, false)
    );
    assert_eq!(
        ErrorClassification::internal(),
        ErrorClassification::new("internal_error", ErrorCategory::Internal, false)
    );
}

#[test]
fn daemon_error_populates_additive_fields_and_round_trips() {
    // 1. Proxy navigation failure via DaemonResponse::err
    let nav_err = MimikyuError::NavigationFailed("net::ERR_TUNNEL_CONNECTION_FAILED".into());
    let resp = DaemonResponse::err(42, &nav_err);
    assert!(!resp.ok);
    let wire_json = serde_json::to_value(&resp).unwrap();
    assert_eq!(
        wire_json,
        json!({
            "id": 42,
            "ok": false,
            "error": {
                "kind": "navigation_failed",
                "message": "net::ERR_TUNNEL_CONNECTION_FAILED",
                "error_code": "proxy_tunnel_failed",
                "category": "proxy",
                "retryable": true
            }
        })
    );

    let decoded_resp: DaemonResponse = serde_json::from_value(wire_json).unwrap();
    let decoded_err = decoded_resp.error.unwrap();
    assert_eq!(
        decoded_err.error_code.as_deref(),
        Some("proxy_tunnel_failed")
    );
    assert_eq!(decoded_err.category, Some(ErrorCategory::Proxy));
    assert_eq!(decoded_err.retryable, Some(true));
    assert_eq!(decoded_err.into_mimikyu().classify(), nav_err.classify());

    // 2. Protocol error preserves numeric `code` alongside string `error_code`
    let proto_err = MimikyuError::Protocol {
        code: -32000,
        message: "Cannot find context with specified id".into(),
    };
    let proto_wire = DaemonError::from_mimikyu(&proto_err);
    let proto_json = serde_json::to_value(&proto_wire).unwrap();
    assert_eq!(
        proto_json,
        json!({
            "kind": "protocol",
            "message": "Cannot find context with specified id",
            "code": -32000,
            "error_code": "protocol_context_destroyed",
            "category": "protocol",
            "retryable": true
        })
    );
    let proto_rt: DaemonError = serde_json::from_value(proto_json).unwrap();
    assert_eq!(proto_rt.code, Some(-32000));
    assert_eq!(proto_rt.into_mimikyu().classify(), proto_err.classify());

    // 3. Timeout error preserves timeout_ms and structured classification
    let timeout_err = MimikyuError::Timeout {
        timeout_ms: 250,
        operation: "wait_text('NOPE')".into(),
    };
    let timeout_wire = DaemonError::from_mimikyu(&timeout_err);
    let timeout_json = serde_json::to_value(&timeout_wire).unwrap();
    assert_eq!(
        timeout_json,
        json!({
            "kind": "timeout",
            "message": "operation timed out after 250ms: wait_text('NOPE')",
            "timeout_ms": 250,
            "error_code": "wait_timeout",
            "category": "timeout",
            "retryable": true
        })
    );
    let timeout_rt: DaemonError = serde_json::from_value(timeout_json).unwrap();
    assert_eq!(timeout_rt.into_mimikyu().classify(), timeout_err.classify());
}

#[test]
fn daemon_error_err_message_populates_taxonomy_for_all_kinds() {
    let cases: &[(DaemonErrorKind, &str, &str, ErrorCategory, bool)] = &[
        (
            DaemonErrorKind::BrowserNotFound,
            "no browser",
            "browser_not_found",
            ErrorCategory::Session,
            false,
        ),
        (
            DaemonErrorKind::LaunchFailed,
            "spawn failed",
            "launch_failed",
            ErrorCategory::Session,
            true,
        ),
        (
            DaemonErrorKind::Transport,
            "eof",
            "transport_closed",
            ErrorCategory::Protocol,
            true,
        ),
        (
            DaemonErrorKind::Protocol,
            "Cannot find context with specified id",
            "protocol_context_destroyed",
            ErrorCategory::Protocol,
            true,
        ),
        (
            DaemonErrorKind::Protocol,
            "Invalid parameters",
            "protocol_error",
            ErrorCategory::Protocol,
            false,
        ),
        (
            DaemonErrorKind::Timeout,
            "operation timed out after 150ms: wait_idle (readyState=loading, inflight=1)",
            "wait_timeout",
            ErrorCategory::Timeout,
            true,
        ),
        (
            DaemonErrorKind::InvalidConfig,
            "malformed json",
            "invalid_config",
            ErrorCategory::Config,
            false,
        ),
        (
            DaemonErrorKind::ProfileLocked,
            "lock held",
            "profile_locked",
            ErrorCategory::Session,
            true,
        ),
        (
            DaemonErrorKind::NotFound,
            "unknown session",
            "session_not_found",
            ErrorCategory::Session,
            false,
        ),
        (
            DaemonErrorKind::ElementNotFound,
            "missing button",
            "element_not_found",
            ErrorCategory::Element,
            false,
        ),
        (
            DaemonErrorKind::NavigationFailed,
            "net::ERR_TUNNEL_CONNECTION_FAILED",
            "proxy_tunnel_failed",
            ErrorCategory::Proxy,
            true,
        ),
        (
            DaemonErrorKind::EvalFailed,
            "TypeError",
            "eval_failed",
            ErrorCategory::Eval,
            false,
        ),
        (
            DaemonErrorKind::MainWorldDisabled,
            "disabled",
            "main_world_disabled",
            ErrorCategory::Config,
            false,
        ),
        (
            DaemonErrorKind::Unsupported,
            "not supported",
            "unsupported_feature",
            ErrorCategory::Unsupported,
            false,
        ),
        (
            DaemonErrorKind::PausedForHuman,
            "paused",
            "paused_for_human",
            ErrorCategory::Session,
            false,
        ),
        (
            DaemonErrorKind::Disposed,
            "daemon stopping",
            "session_disposed",
            ErrorCategory::Session,
            false,
        ),
        (
            DaemonErrorKind::Io,
            "broken pipe",
            "io_error",
            ErrorCategory::Io,
            false,
        ),
        (
            DaemonErrorKind::Internal,
            "unexpected worker panic",
            "internal_error",
            ErrorCategory::Internal,
            false,
        ),
    ];

    for &(kind, msg, expected_code, expected_cat, expected_retryable) in cases {
        let resp = DaemonResponse::err_message(9, kind, msg);
        assert!(!resp.ok);
        let err = resp.error.expect("error payload present");
        assert_eq!(err.kind, kind);
        assert_eq!(err.message, msg);
        assert_eq!(err.error_code.as_deref(), Some(expected_code));
        assert_eq!(err.category, Some(expected_cat));
        assert_eq!(err.retryable, Some(expected_retryable));
    }
}

#[test]
fn daemon_error_backwards_compatible_with_legacy_payloads_and_consumers() {
    // 1. New server deserializing a legacy payload lacking error_code / category / retryable
    let legacy_json = json!({
        "kind": "navigation_failed",
        "message": "net::ERR_NAME_NOT_RESOLVED"
    });
    let legacy: DaemonError = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(legacy.kind, DaemonErrorKind::NavigationFailed);
    assert_eq!(legacy.message, "net::ERR_NAME_NOT_RESOLVED");
    assert_eq!(legacy.code, None);
    assert_eq!(legacy.timeout_ms, None);
    assert_eq!(legacy.error_code, None);
    assert_eq!(legacy.category, None);
    assert_eq!(legacy.retryable, None);

    // Re-serializing legacy DaemonError omits None additive fields cleanly
    let re_serialized = serde_json::to_value(&legacy).unwrap();
    assert!(re_serialized.get("error_code").is_none());
    assert!(re_serialized.get("category").is_none());
    assert!(re_serialized.get("retryable").is_none());

    // Converting legacy DaemonError -> MimikyuError -> classify still yields exact taxonomy
    assert_eq!(
        legacy.into_mimikyu().classify(),
        ErrorClassification::new("dns_not_resolved", ErrorCategory::Network, false)
    );

    // 2. Legacy consumer that only knows about { kind, message, code, timeout_ms }
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct LegacyDaemonError {
        kind: DaemonErrorKind,
        message: String,
        #[serde(default)]
        code: Option<i64>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    }

    let modern = DaemonError::from_mimikyu(&MimikyuError::Protocol {
        code: -32000,
        message: "Execution context was destroyed.".into(),
    });
    let wire = serde_json::to_string(&modern).unwrap();
    let legacy_consumer: LegacyDaemonError = serde_json::from_str(&wire).unwrap();
    assert_eq!(
        legacy_consumer,
        LegacyDaemonError {
            kind: DaemonErrorKind::Protocol,
            message: "Execution context was destroyed.".into(),
            code: Some(-32000),
            timeout_ms: None,
        }
    );
}

#[test]
fn r3_keyword_scan_covers_every_category_and_error_code() {
    // 1. Scan every ErrorCategory serialized value
    for &cat in ErrorCategory::ALL {
        let json = serde_json::to_string(&cat).unwrap();
        assert_r3_clean(&json, "ErrorCategory");
        assert_r3_clean(cat.as_str(), "ErrorCategory::as_str");
    }

    // 2. Scan every canonical error_code in ALL_ERROR_CODES
    let canonical_codes: BTreeSet<&str> = ErrorClassification::ALL_ERROR_CODES
        .iter()
        .copied()
        .collect();
    assert_eq!(
        canonical_codes.len(),
        ErrorClassification::ALL_ERROR_CODES.len(),
        "ALL_ERROR_CODES must not contain duplicates"
    );
    for &code in &canonical_codes {
        assert!(!code.is_empty());
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "error_code must be snake_case ASCII: '{code}'"
        );
        assert_r3_clean(code, "ALL_ERROR_CODES");
    }

    // 3. Exercise every code path and confirm emitted codes match ALL_ERROR_CODES 1-to-1
    let sample_classifications = vec![
        MimikyuError::NavigationFailed("net::ERR_TUNNEL_CONNECTION_FAILED".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_PROXY_CONNECTION_FAILED".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_PROXY_AUTH_UNSUPPORTED".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_NAME_NOT_RESOLVED".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_EMPTY_RESPONSE".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_CONNECTION_RESET".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_CONNECTION_REFUSED".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_CONNECTION_TIMED_OUT".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_CERT_AUTHORITY_INVALID".into()).classify(),
        MimikyuError::NavigationFailed("net::ERR_ABORTED".into()).classify(),
        MimikyuError::NavigationFailed("other failure".into()).classify(),
        MimikyuError::Timeout {
            timeout_ms: 1000,
            operation: "CDP Page.navigate".into(),
        }
        .classify(),
        MimikyuError::Timeout {
            timeout_ms: 1000,
            operation: "wait_idle (readyState=loading, inflight=1)".into(),
        }
        .classify(),
        MimikyuError::Timeout {
            timeout_ms: 1000,
            operation: "CDP Runtime.evaluate".into(),
        }
        .classify(),
        MimikyuError::Timeout {
            timeout_ms: 1000,
            operation: "click".into(),
        }
        .classify(),
        MimikyuError::Protocol {
            code: -32000,
            message: "Cannot find context with specified id".into(),
        }
        .classify(),
        MimikyuError::Protocol {
            code: -32601,
            message: "method not found".into(),
        }
        .classify(),
        MimikyuError::Transport("closed".into()).classify(),
        MimikyuError::InvalidConfig("bad".into()).classify(),
        ErrorClassification::cli_usage(),
        MimikyuError::MainWorldDisabled.classify(),
        MimikyuError::NotFound("s1".into()).classify(),
        MimikyuError::Disposed.classify(),
        MimikyuError::ProfileLocked("lock".into()).classify(),
        MimikyuError::PausedForHuman {
            session_id: "s1".into(),
            reason: None,
        }
        .classify(),
        MimikyuError::BrowserNotFound("chrome".into()).classify(),
        MimikyuError::LaunchFailed("spawn".into()).classify(),
        MimikyuError::ElementNotFound("@e1".into()).classify(),
        MimikyuError::EvalFailed("ReferenceError".into()).classify(),
        MimikyuError::Io(std::io::Error::other("io")).classify(),
        MimikyuError::Unsupported {
            engine: "firefox".into(),
            feature: "x".into(),
        }
        .classify(),
        ErrorClassification::internal(),
    ];

    let mut observed_codes = BTreeSet::new();
    let mut observed_categories = BTreeSet::new();
    for class in sample_classifications {
        let serialized = serde_json::to_string(&class).unwrap();
        assert_r3_clean(&serialized, "ErrorClassification JSON");
        observed_codes.insert(class.error_code);
        observed_categories.insert(class.category.as_str());
    }

    assert_eq!(
        observed_codes, canonical_codes,
        "observed error codes must match ALL_ERROR_CODES 1-to-1"
    );
    let canonical_categories: BTreeSet<&str> =
        ErrorCategory::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(
        observed_categories, canonical_categories,
        "all ErrorCategory variants must be reachable"
    );
}
