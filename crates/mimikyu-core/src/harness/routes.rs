use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};
use url::Url;

use super::{pages, HarnessOrigin, HarnessRequest, HarnessResponse, HarnessState};

pub async fn dispatch(req: &HarnessRequest, state: &HarnessState) -> HarnessResponse {
    if req.method.eq_ignore_ascii_case("OPTIONS") {
        return HarnessResponse {
            status: 204,
            reason: "No Content",
            content_type: "text/plain",
            extra_headers: vec![
                ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
                (
                    "Access-Control-Allow-Methods".to_string(),
                    "GET, POST, OPTIONS".to_string(),
                ),
                (
                    "Access-Control-Allow-Headers".to_string(),
                    "Content-Type, Accept".to_string(),
                ),
            ],
            body: Vec::new(),
            force_close: false,
        };
    }

    let method = req.method.to_ascii_uppercase();
    let effective_method = if method == "HEAD" {
        "GET"
    } else {
        method.as_str()
    };

    match (req.origin, effective_method, req.path.as_str()) {
        (_, "GET", "/favicon.ico") => empty(204, "No Content", "image/x-icon"),
        (HarnessOrigin::CrossOrigin, "GET", "/xo-child")
        | (HarnessOrigin::Primary, "GET", "/xo-child") => html(pages::xo_child_html()),
        (HarnessOrigin::CrossOrigin, "GET", "/xo-env-frame")
        | (HarnessOrigin::Primary, "GET", "/xo-env-frame") => {
            html(pages::env_probe_frame_html("xo-frame"))
        }
        (HarnessOrigin::CrossOrigin, "GET", "/") => html(pages::xo_child_html()),
        (HarnessOrigin::Primary, "GET", "/") => html(pages::index_html()),
        (HarnessOrigin::Primary, "GET", "/redirect") => {
            let to = req
                .query
                .get("to")
                .map(String::as_str)
                .filter(|t| {
                    t.starts_with('/')
                        && !t.starts_with("//")
                        && !t.contains('\r')
                        && !t.contains('\n')
                        && !t.chars().any(|c| c.is_control())
                })
                .unwrap_or("/target");
            HarnessResponse {
                status: 302,
                reason: "Found",
                content_type: "text/plain; charset=utf-8",
                extra_headers: vec![("Location".to_string(), to.to_string())],
                body: Vec::new(),
                force_close: false,
            }
        }
        (HarnessOrigin::Primary, "GET", "/target") => html(pages::target_html()),
        (HarnessOrigin::Primary, "GET", "/wizard") => html(pages::wizard_step1_html()),
        (HarnessOrigin::Primary, "GET", "/wizard/step2") => {
            let name = req.query.get("name").map(String::as_str).unwrap_or("");
            let role = req.query.get("role").map(String::as_str).unwrap_or("viewer");
            html(pages::wizard_step2_html(name, role))
        }
        (HarnessOrigin::Primary, "POST", "/wizard/step2") => {
            let params = parse_body_params(req);
            let name = params.get("name").map(String::as_str).unwrap_or("");
            let role = params.get("role").map(String::as_str).unwrap_or("viewer");
            html(pages::wizard_step2_html(name, role))
        }
        (HarnessOrigin::Primary, "POST", "/wizard/done") => {
            let params = parse_body_params(req);
            let name = params.get("name").map(String::as_str).unwrap_or("");
            let role = params.get("role").map(String::as_str).unwrap_or("");
            let region = params.get("region").map(String::as_str).unwrap_or("");
            let agree = params.get("agree").map(String::as_str).unwrap_or("no");
            html(pages::wizard_done_html(name, role, region, agree))
        }
        (HarnessOrigin::Primary, "GET", "/wizard/done") => {
            let name = req.query.get("name").map(String::as_str).unwrap_or("");
            let role = req.query.get("role").map(String::as_str).unwrap_or("");
            let region = req.query.get("region").map(String::as_str).unwrap_or("");
            let agree = req.query.get("agree").map(String::as_str).unwrap_or("no");
            html(pages::wizard_done_html(name, role, region, agree))
        }
        (HarnessOrigin::Primary, "GET", "/form") => html(pages::form_html()),
        (HarnessOrigin::Primary, "POST", "/submit") => {
            let seq = state.submit_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let mut params = parse_body_params(req);
            params.insert("_seq".to_string(), seq.to_string());
            let name = params.get("name").cloned().unwrap_or_default();
            let country = params.get("country").cloned().unwrap_or_default();
            let tier = params.get("tier").cloned().unwrap_or_default();
            let terms = params
                .get("terms")
                .cloned()
                .unwrap_or_else(|| "no".to_string());

            let wants_json = req.query.get("format").map(String::as_str) == Some("json")
                || req
                    .headers
                    .get("accept")
                    .is_some_and(|a| a.contains("application/json"));

            let payload = json!({
                "seq": seq,
                "fields": params,
            });
            if wants_json {
                json_response(200, "OK", &payload)
            } else {
                let pretty = serde_json::to_string_pretty(&payload).unwrap_or_default();
                html(pages::submit_echo_html(
                    &pretty, &name, &country, &tier, &terms,
                ))
            }
        }
        (HarnessOrigin::Primary, "GET", "/table") => html(pages::table_html()),
        (HarnessOrigin::Primary, "GET", "/catalog") => html(pages::catalog_html()),
        (HarnessOrigin::Primary, "GET", "/dynamic") => html(pages::dynamic_html()),
        (HarnessOrigin::Primary, "GET", "/api/items") => {
            let delay_ms = req
                .query
                .get("delay")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(80)
                .min(2000);
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let category_filter = req.query.get("category").map(|s| s.to_ascii_lowercase());
            let mut items = vec![
                json!({"id": "alpha", "label": "Alpha Node", "category": "compute", "price": 120}),
                json!({"id": "beta", "label": "Beta Store", "category": "storage", "price": 80}),
            ];
            if let Some(ref cat) = category_filter
                && cat != "all"
            {
                items.retain(|it| it["category"].as_str() == Some(cat.as_str()));
            }
            json_response(200, "OK", &json!({ "items": items }))
        }
        (HarnessOrigin::Primary, "GET", "/shadow") => html(pages::shadow_html()),
        (HarnessOrigin::Primary, "GET", "/frames") => {
            let default_xo = state
                .cross_origin_base_url
                .as_deref()
                .map(|b| format!("{b}/xo-child"))
                .unwrap_or_else(|| "/xo-child".to_string());
            let xo_url = req
                .query
                .get("xo")
                .filter(|raw| is_loopback_http_url(raw))
                .cloned()
                .unwrap_or(default_xo);
            html(pages::frames_html(&xo_url))
        }
        (HarnessOrigin::Primary, "GET", "/frames/same-child") => {
            html(pages::frames_same_child_html())
        }
        (HarnessOrigin::Primary, "GET", "/frames/nested-inner") => {
            html(pages::frames_nested_inner_html())
        }
        (HarnessOrigin::Primary, "GET", "/env-probe") => {
            let xo_url = state
                .cross_origin_base_url
                .as_deref()
                .map(|b| format!("{b}/xo-env-frame"));
            html(pages::env_probe_html(xo_url.as_deref()))
        }
        (HarnessOrigin::Primary, "GET", "/env-probe-frame") => {
            html(pages::env_probe_frame_html("frame"))
        }
        (HarnessOrigin::Primary, "GET", "/env-worker.js") => {
            js(pages::env_worker_classic_js().as_bytes().to_vec())
        }
        (HarnessOrigin::Primary, "GET", "/env-worker.mjs") => {
            js(pages::env_worker_module_js().as_bytes().to_vec())
        }
        (HarnessOrigin::Primary, "GET", "/helper.mjs") => {
            js(pages::helper_mjs().as_bytes().to_vec())
        }
        (_, "GET", "/echo-headers") | (_, "GET", "/echo-env") => {
            json_response(200, "OK", &json!(req.headers))
        }
        (HarnessOrigin::Primary, "GET", "/geo-page") => html(pages::geo_page_html()),
        (_, "GET", "/assets/pixel.png") => HarnessResponse {
            status: 200,
            reason: "OK",
            content_type: "image/png",
            extra_headers: Vec::new(),
            body: vec![137, 80, 78, 71, 13, 10, 26, 10],
            force_close: false,
        },
        (_, "GET", "/assets/font.woff2") => HarnessResponse {
            status: 200,
            reason: "OK",
            content_type: "font/woff2",
            extra_headers: Vec::new(),
            body: b"wOF2\x00\x01\x00\x00".to_vec(),
            force_close: false,
        },
        (_, "GET", "/assets/clip.mp4") => HarnessResponse {
            status: 200,
            reason: "OK",
            content_type: "video/mp4",
            extra_headers: Vec::new(),
            body: b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom".to_vec(),
            force_close: false,
        },
        _ => HarnessResponse {
            status: 404,
            reason: "Not Found",
            content_type: "text/plain; charset=utf-8",
            extra_headers: Vec::new(),
            body: b"not found".to_vec(),
            force_close: false,
        },
    }
}

fn parse_body_params(req: &HarnessRequest) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let content_type = req
        .headers
        .get("content-type")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if content_type.contains("application/json") {
        if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&req.body) {
            for (k, v) in map {
                let s = match v {
                    Value::String(s) => s,
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                out.insert(k, s);
            }
        }
        return out;
    }

    for (k, v) in url::form_urlencoded::parse(&req.body) {
        out.insert(k.into_owned(), v.into_owned());
    }
    out
}

fn is_loopback_http_url(raw: &str) -> bool {
    if let Ok(parsed) = Url::parse(raw) {
        if !matches!(parsed.scheme(), "http" | "https") {
            return false;
        }
        return matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost"));
    }
    false
}

fn html(body: String) -> HarnessResponse {
    HarnessResponse {
        status: 200,
        reason: "OK",
        content_type: "text/html; charset=utf-8",
        extra_headers: vec![("Cache-Control".to_string(), "no-store".to_string())],
        body: body.into_bytes(),
        force_close: false,
    }
}

fn js(body: Vec<u8>) -> HarnessResponse {
    HarnessResponse {
        status: 200,
        reason: "OK",
        content_type: "application/javascript; charset=utf-8",
        extra_headers: vec![("Cache-Control".to_string(), "no-store".to_string())],
        body,
        force_close: false,
    }
}

fn json_response(status: u16, reason: &'static str, value: &Value) -> HarnessResponse {
    HarnessResponse {
        status,
        reason,
        content_type: "application/json; charset=utf-8",
        extra_headers: vec![
            ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        body: serde_json::to_vec(value).unwrap_or_default(),
        force_close: false,
    }
}

fn empty(status: u16, reason: &'static str, content_type: &'static str) -> HarnessResponse {
    HarnessResponse {
        status,
        reason,
        content_type,
        extra_headers: Vec::new(),
        body: Vec::new(),
        force_close: false,
    }
}
