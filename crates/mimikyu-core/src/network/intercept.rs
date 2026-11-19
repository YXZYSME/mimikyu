use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::driver::{RouteAction, RouteMock};
use crate::error::Result;
use crate::network::blocker::{BlockPolicy, ResourceBlocker, ResourceType, UrlPattern};

/// Extended declarative route rule supporting optional `ResourceType` filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    pub id: String,
    pub url_pattern: String,
    #[serde(default)]
    pub resource_types: Vec<ResourceType>,
    pub action: RouteAction,
}

impl From<RouteMock> for RouteRule {
    fn from(mock: RouteMock) -> Self {
        Self {
            id: mock.id,
            url_pattern: mock.url_pattern,
            resource_types: Vec::new(),
            action: mock.action,
        }
    }
}

#[derive(Debug, Clone)]
struct CompiledRouteRule {
    rule: RouteRule,
    matcher: UrlPattern,
}

pub struct InterceptRequest<'a> {
    pub url: &'a str,
    pub resource_type: ResourceType,
    pub headers: &'a BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterceptDecision {
    Continue,
    ModifyHeaders {
        headers: BTreeMap<String, String>,
    },
    Fulfill {
        status: u16,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    },
    Block {
        reason: &'static str,
    },
}

/// Shared URL/category blocking + declarative route interception engine for CDP and BiDi.
#[derive(Debug, Clone, Default)]
pub struct NetworkInterceptor {
    blocker: ResourceBlocker,
    rules: Vec<CompiledRouteRule>,
}

impl NetworkInterceptor {
    pub fn new(policy: BlockPolicy) -> Result<Self> {
        Ok(Self {
            blocker: ResourceBlocker::new(policy)?,
            rules: Vec::new(),
        })
    }

    pub fn blocker(&self) -> &ResourceBlocker {
        &self.blocker
    }

    pub fn blocker_mut(&mut self) -> &mut ResourceBlocker {
        &mut self.blocker
    }

    pub fn set_block_policy(&mut self, policy: BlockPolicy) -> Result<()> {
        self.blocker = ResourceBlocker::new(policy)?;
        Ok(())
    }

    pub fn set_blocked_urls(&mut self, patterns: Vec<String>) -> Result<()> {
        self.blocker.set_blocked_urls(patterns)
    }

    pub fn set_categories(&mut self, block_images: bool, block_media: bool, block_fonts: bool) {
        self.blocker
            .set_categories(block_images, block_media, block_fonts);
    }

    pub fn add_rule(&mut self, rule: RouteRule) -> Result<()> {
        let matcher = UrlPattern::parse(&rule.url_pattern)?;
        self.rules.retain(|r| r.rule.id != rule.id);
        self.rules.push(CompiledRouteRule { rule, matcher });
        Ok(())
    }

    pub fn add_mock(&mut self, mock: RouteMock) -> Result<()> {
        self.add_rule(RouteRule::from(mock))
    }

    pub fn clear_rules(&mut self) {
        self.rules.clear();
    }

    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }

    pub fn needs_bidi_intercept(&self) -> bool {
        !self.rules.is_empty() || self.blocker.has_any_blocks()
    }

    /// Returns `Fetch.enable` patterns needed on CDP, or `None` when `Fetch` should be disabled.
    pub fn cdp_fetch_patterns(&self) -> Option<Vec<Value>> {
        if !self.rules.is_empty() || self.blocker.has_non_cdp_url_patterns() {
            return Some(vec![json!({
                "urlPattern": "*",
                "requestStage": "Request",
            })]);
        }
        let types = self.blocker.blocked_cdp_resource_types();
        if !types.is_empty() {
            return Some(
                types
                    .into_iter()
                    .map(|rt| {
                        json!({
                            "urlPattern": "*",
                            "resourceType": rt,
                            "requestStage": "Request",
                        })
                    })
                    .collect(),
            );
        }
        None
    }

    /// Evaluate request with deterministic precedence:
    /// 1. Explicit `blocked_urls` -> `Block("BlockedByClient")`
    /// 2. First matching `RouteRule` -> `Abort`/`Fulfill`/`ContinueWithHeaders`/`Continue`
    /// 3. Category blocker (`block_images`/`block_media`/`block_fonts`) -> `Block("BlockedByClient")`
    /// 4. Default -> `Continue`
    pub fn decide(&self, req: &InterceptRequest<'_>) -> InterceptDecision {
        if self.blocker.should_block_url(req.url) {
            return InterceptDecision::Block {
                reason: "BlockedByClient",
            };
        }

        if let Some(compiled) = self.rules.iter().find(|r| {
            r.matcher.matches(req.url)
                && (r.rule.resource_types.is_empty()
                    || r.rule.resource_types.iter().any(|rt| {
                        *rt == req.resource_type
                            || matches!(
                                (*rt, req.resource_type),
                                (ResourceType::Fetch, ResourceType::Xhr)
                                    | (ResourceType::Xhr, ResourceType::Fetch)
                            )
                    }))
        }) {
            return match &compiled.rule.action {
                RouteAction::Abort => InterceptDecision::Block { reason: "Aborted" },
                RouteAction::Fulfill {
                    status,
                    headers,
                    body,
                } => InterceptDecision::Fulfill {
                    status: *status,
                    headers: headers.clone(),
                    body: body.clone(),
                },
                RouteAction::ContinueWithHeaders { headers } => InterceptDecision::ModifyHeaders {
                    headers: merge_headers_case_insensitive(req.headers, headers),
                },
                RouteAction::Continue => InterceptDecision::Continue,
            };
        }

        if self
            .blocker
            .should_block_resource_type(req.resource_type, req.url)
        {
            return InterceptDecision::Block {
                reason: "BlockedByClient",
            };
        }

        InterceptDecision::Continue
    }

    pub fn to_shared_worker_policy_json(&self) -> Value {
        let pol = self.blocker.policy();
        let rules_json: Vec<Value> = self
            .rules
            .iter()
            .map(|cr| {
                let rtypes: Vec<&str> = cr
                    .rule
                    .resource_types
                    .iter()
                    .map(|rt| match rt {
                        ResourceType::Document => "document",
                        ResourceType::Stylesheet => "stylesheet",
                        ResourceType::Image => "image",
                        ResourceType::Media => "media",
                        ResourceType::Font => "font",
                        ResourceType::Script => "script",
                        ResourceType::TextTrack => "texttrack",
                        ResourceType::Xhr => "xhr",
                        ResourceType::Fetch => "fetch",
                        ResourceType::EventSource => "eventsource",
                        ResourceType::WebSocket => "websocket",
                        ResourceType::Manifest => "manifest",
                        ResourceType::Other => "other",
                    })
                    .collect();
                let action = match &cr.rule.action {
                    RouteAction::Abort => json!({ "kind": "abort" }),
                    RouteAction::Fulfill {
                        status,
                        headers,
                        body,
                    } => json!({
                        "kind": "fulfill",
                        "status": status,
                        "statusText": http_reason_phrase(*status),
                        "headers": headers,
                        "bodyB64": BASE64.encode(body),
                    }),
                    RouteAction::ContinueWithHeaders { headers } => json!({
                        "kind": "modify_headers",
                        "headers": headers,
                    }),
                    RouteAction::Continue => json!({ "kind": "continue" }),
                };
                json!({
                    "id": cr.rule.id,
                    "matcher": cr.matcher.to_js_matcher(),
                    "resourceTypes": rtypes,
                    "action": action,
                })
            })
            .collect();
        json!({
            "active": self.needs_bidi_intercept(),
            "blockImages": pol.block_images,
            "blockMedia": pol.block_media,
            "blockFonts": pol.block_fonts,
            "blockedUrls": self.blocker.compiled_url_matchers_json(),
            "rules": rules_json,
        })
    }
}

/// Merge HTTP header overrides into `base` case-insensitively (`overrides` wins).
pub fn merge_headers_case_insensitive(
    base: &BTreeMap<String, String>,
    overrides: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = base.clone();
    for (ok, ov) in overrides {
        let existing_keys: Vec<String> = merged
            .keys()
            .filter(|k| k.eq_ignore_ascii_case(ok))
            .cloned()
            .collect();
        for ek in existing_keys {
            merged.remove(&ek);
        }
        merged.insert(ok.clone(), ov.clone());
    }
    merged
}

pub fn http_reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        418 => "I'm a teapot",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_headers_replaces_case_insensitively() {
        let mut base = BTreeMap::new();
        base.insert("User-Agent".to_string(), "OldUA".to_string());
        base.insert("Accept".to_string(), "*/*".to_string());

        let mut overrides = BTreeMap::new();
        overrides.insert("user-agent".to_string(), "NewUA".to_string());
        overrides.insert("X-Custom".to_string(), "1".to_string());

        let merged = merge_headers_case_insensitive(&base, &overrides);
        assert_eq!(merged.get("user-agent").map(String::as_str), Some("NewUA"));
        assert!(!merged.contains_key("User-Agent"));
        assert_eq!(merged.get("Accept").map(String::as_str), Some("*/*"));
        assert_eq!(merged.get("X-Custom").map(String::as_str), Some("1"));
    }

    #[test]
    fn interceptor_precedence_and_resource_type_filter() {
        let mut ic = NetworkInterceptor::new(BlockPolicy {
            block_images: true,
            block_media: false,
            block_fonts: false,
            blocked_urls: vec!["exact:http://127.0.0.1/deny".to_string()],
        })
        .unwrap();

        ic.add_rule(RouteRule {
            id: "r1".to_string(),
            url_pattern: "prefix:http://127.0.0.1/api/".to_string(),
            resource_types: vec![ResourceType::Fetch, ResourceType::Xhr],
            action: RouteAction::Fulfill {
                status: 201,
                headers: BTreeMap::new(),
                body: b"{}".to_vec(),
            },
        })
        .unwrap();

        let empty_hdrs = BTreeMap::new();

        // Explicit URL block wins
        assert_eq!(
            ic.decide(&InterceptRequest {
                url: "http://127.0.0.1/deny",
                resource_type: ResourceType::Document,
                headers: &empty_hdrs,
            }),
            InterceptDecision::Block {
                reason: "BlockedByClient"
            }
        );

        // RouteRule matches Fetch but not Document
        assert!(matches!(
            ic.decide(&InterceptRequest {
                url: "http://127.0.0.1/api/items",
                resource_type: ResourceType::Fetch,
                headers: &empty_hdrs,
            }),
            InterceptDecision::Fulfill { status: 201, .. }
        ));
        assert_eq!(
            ic.decide(&InterceptRequest {
                url: "http://127.0.0.1/api/items",
                resource_type: ResourceType::Document,
                headers: &empty_hdrs,
            }),
            InterceptDecision::Continue
        );

        // Category blocker catches extensionless image
        assert_eq!(
            ic.decide(&InterceptRequest {
                url: "http://127.0.0.1/avatar?id=1",
                resource_type: ResourceType::Image,
                headers: &empty_hdrs,
            }),
            InterceptDecision::Block {
                reason: "BlockedByClient"
            }
        );
    }

    #[test]
    fn cdp_fetch_patterns_tracks_rules_and_categories() {
        let mut ic = NetworkInterceptor::default();
        assert_eq!(ic.cdp_fetch_patterns(), None);

        ic.set_categories(true, false, true);
        let pats = ic.cdp_fetch_patterns().unwrap();
        assert_eq!(pats.len(), 2);
        assert_eq!(pats[0]["resourceType"], "Image");
        assert_eq!(pats[1]["resourceType"], "Font");

        ic.add_mock(RouteMock {
            id: "m".to_string(),
            url_pattern: "*/api/*".to_string(),
            action: RouteAction::Abort,
        })
        .unwrap();
        let pats = ic.cdp_fetch_patterns().unwrap();
        assert_eq!(pats.len(), 1);
        assert!(pats[0].get("resourceType").is_none());

        ic.clear_rules();
        let pats = ic.cdp_fetch_patterns().unwrap();
        assert_eq!(pats.len(), 2);
    }
}
