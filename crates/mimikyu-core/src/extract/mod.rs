pub mod markdown;
pub mod media;
pub mod tables;

use serde::{Deserialize, Serialize};

use crate::driver::DriverSession;
use crate::error::Result;
use crate::extract::markdown::truncate_chars;

pub use markdown::{extract_markdown, extract_text, DocBlock, MarkdownResult, TextResult};
pub use media::{
    capture_screenshot, export_pdf, set_instant_animations, CaptureOpts, MediaCapture, MediaTarget,
};
pub use tables::{extract_tables, ExtractedTable, TableExtractResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub frame_id: Option<String>,
    pub selector: Option<String>,
    pub include_shadow_dom: bool,
    pub include_frames: bool,
    pub main_only: bool,
    pub max_chars: Option<usize>,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            frame_id: None,
            selector: None,
            include_shadow_dom: true,
            include_frames: false,
            main_only: false,
            max_chars: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedLink {
    pub text: String,
    pub href: String,
    pub raw_href: String,
    pub title: Option<String>,
    pub rel: Option<String>,
    pub target: Option<String>,
    pub is_external: bool,
    pub frame_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkExtractResult {
    pub url: String,
    pub links: Vec<ExtractedLink>,
}

const JS_COLLECT_LINKS: &str = r#"
const isVisible = (el) => {
    if (!el || el.nodeType !== 1) return false;
    if (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true') return false;
    try {
        const win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
        const cs = win.getComputedStyle(el);
        if (cs.display === 'none' || cs.visibility === 'hidden') return false;
    } catch {}
    return true;
};
const isSubtreeVisible = (startEl) => {
    let cur = startEl;
    while (cur) {
        if (cur.nodeType === 1) {
            if (!isVisible(cur)) return false;
            cur = cur.parentElement || (cur.getRootNode ? cur.getRootNode().host : null);
        } else if (cur.nodeType === 11) {
            cur = cur.host || null;
        } else {
            cur = null;
        }
    }
    return true;
};
const isInsideScope = (startEl, scopeRoot) => {
    if (!scopeRoot) return false;
    let cur = startEl;
    while (cur) {
        if (cur === scopeRoot) return true;
        if (cur.nodeType === 1) {
            cur = cur.parentElement || (cur.getRootNode ? cur.getRootNode().host : null);
        } else if (cur.nodeType === 11) {
            cur = cur.host || null;
        } else {
            cur = null;
        }
    }
    return false;
};
const collectLinksFromRoot = (root, includeShadow, frameId) => {
    const out = [];
    const seen = new Set();
    const origin = window.location.origin;
    const visit = (node) => {
        if (!node) return;
        if (node.nodeType === 1) {
            if (!isVisible(node)) return;
            if (node.tagName === 'A' && node.hasAttribute('href') && !seen.has(node)) {
                seen.add(node);
                const rawHref = node.getAttribute('href') || '';
                const absHref = node.href || rawHref;
                let text = (node.innerText || node.textContent || '').replace(/\s+/g, ' ').trim();
                if (!text) text = (node.getAttribute('aria-label') || '').trim();
                if (!text) {
                    const img = node.querySelector('img[alt]');
                    if (img && img.alt) text = img.alt.trim();
                }
                if (!text && node.title) text = node.title.trim();
                let isExt = false;
                try {
                    const u = new URL(absHref, window.location.href);
                    isExt = (u.protocol === 'http:' || u.protocol === 'https:') && u.origin !== origin;
                } catch {}
                out.push({
                    text,
                    href: absHref,
                    raw_href: rawHref,
                    title: node.title ? node.title.trim() : null,
                    rel: node.getAttribute('rel') || null,
                    target: node.getAttribute('target') || null,
                    is_external: isExt,
                    frame_id: frameId || null,
                });
            }
            if (includeShadow && node.shadowRoot) visit(node.shadowRoot);
        }
        if (node.childNodes) {
            for (const ch of node.childNodes) visit(ch);
        }
    };
    visit(root);
    return out;
};
"#;

async fn extract_links_in_frame(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
    frame_id: Option<&str>,
) -> Result<Vec<ExtractedLink>> {
    let sel_json = serde_json::to_string(&opts.selector)?;
    let fid_json = serde_json::to_string(&frame_id)?;
    let include_shadow = opts.include_shadow_dom;
    let js = format!(
        r#"(() => {{
            {JS_COLLECT_LINKS}
            const sel = {sel_json};
            const root = sel ? document.querySelector(sel) : (document.body || document.documentElement);
            if (!root) return [];
            return collectLinksFromRoot(root, {include_shadow}, {fid_json});
        }})()"#
    );
    let val = session.eval_isolated_in_frame(&js, frame_id).await?;
    let mut links: Vec<ExtractedLink> = serde_json::from_value(val).unwrap_or_default();

    if opts.include_shadow_dom && session.capabilities().supports_closed_shadow_dom {
        let owned_main_fid;
        let shadow_fid = match frame_id {
            Some(f) => Some(f),
            None => {
                owned_main_fid = session
                    .list_frames()
                    .await
                    .ok()
                    .and_then(|fs| fs.into_iter().find(|f| f.is_main).map(|f| f.frame_id));
                owned_main_fid.as_deref()
            }
        };
        let hosts = session
            .closed_shadow_hosts(shadow_fid)
            .await
            .unwrap_or_default();
        let fn_decl = format!(
            r#"function(fid, sel) {{
                {JS_COLLECT_LINKS}
                if (!this || !this.host || !isSubtreeVisible(this.host)) return [];
                const scopeRoot = sel ? document.querySelector(sel) : (document.body || document.documentElement);
                if (!isInsideScope(this.host, scopeRoot)) return [];
                return collectLinksFromRoot(this, true, fid);
            }}"#
        );
        for bid in hosts {
            if let Ok(sr_val) = session
                .call_function_on_node(
                    bid,
                    frame_id,
                    &fn_decl,
                    vec![
                        serde_json::json!(frame_id),
                        serde_json::json!(opts.selector),
                    ],
                )
                .await
                && let Ok(sr_links) = serde_json::from_value::<Vec<ExtractedLink>>(sr_val)
            {
                links.extend(sr_links);
            }
        }
    }

    if let Some(max) = opts.max_chars {
        for l in &mut links {
            let (t, _) = truncate_chars(&l.text, Some(max));
            l.text = t;
        }
    }

    Ok(links)
}

pub async fn extract_links(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
) -> Result<LinkExtractResult> {
    let url = session.current_url().await.unwrap_or_default();
    let mut links = extract_links_in_frame(session, opts, opts.frame_id.as_deref()).await?;
    if opts.include_frames && opts.frame_id.is_none() {
        let frames = session.list_frames().await.unwrap_or_default();
        for f in frames {
            if f.is_main {
                continue;
            }
            if let Ok(sub) = extract_links_in_frame(session, opts, Some(&f.frame_id)).await {
                links.extend(sub);
            }
        }
    }
    Ok(LinkExtractResult { url, links })
}
