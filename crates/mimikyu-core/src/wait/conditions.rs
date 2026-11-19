use serde_json::Value;
use tokio::time::Instant;

use crate::driver::DriverSession;
use crate::error::{MimikyuError, Result};
use crate::wait::{compute_wait_deadline, WaitOptions, WaitSelectorState};

pub(crate) async fn prevalidate_css_selector(
    session: &dyn DriverSession,
    selector: &str,
) -> Result<()> {
    if selector.trim().is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "wait selector cannot be empty".to_string(),
        ));
    }
    let sel_json = serde_json::to_string(selector)?;
    let js = format!(
        "(() => {{ try {{ document.createDocumentFragment().querySelector({sel_json}); return null; }} catch (e) {{ return String(e && e.message ? e.message : e); }} }})()"
    );
    match session.eval_isolated(&js).await {
        Ok(Value::String(err_msg)) => Err(MimikyuError::InvalidConfig(format!(
            "invalid CSS selector '{selector}': {err_msg}"
        ))),
        Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => Err(e),
        _ => Ok(()),
    }
}

pub async fn wait_text(
    session: &dyn DriverSession,
    text: &str,
    opts: WaitOptions,
) -> Result<()> {
    if text.is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "wait_text requires a non-empty text substring".to_string(),
        ));
    }
    let needle_json = serde_json::to_string(text)?;
    let js = format!(
        r#"(() => {{
            const needle = {needle_json};
            const seenDocs = new Set();
            const seenShadow = new Set();
            const isVisible = (el) => {{
                if (!el || el.nodeType !== 1) return false;
                const tag = el.tagName;
                if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT' || tag === 'TEMPLATE') return false;
                if (el.hasAttribute && (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true')) return false;
                try {{
                    const win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
                    const cs = win.getComputedStyle(el);
                    if (cs.display === 'none' || cs.visibility === 'hidden') return false;
                }} catch {{}}
                return true;
            }};
            const collectShadowText = (root, depth, out) => {{
                if (!root || depth > 64) return;
                if (root.nodeType === 3) {{
                    out.push(root.nodeValue || '');
                    return;
                }}
                if (root.nodeType === 1) {{
                    if (!isVisible(root)) return;
                    if (root.tagName === 'SLOT' && typeof root.assignedNodes === 'function') {{
                        const assigned = root.assignedNodes({{ flatten: true }});
                        if (assigned && assigned.length > 0) return;
                    }}
                }}
                if (root.childNodes) {{
                    for (const ch of root.childNodes) collectShadowText(ch, depth + 1, out);
                }}
            }};
            const scanShadowsAndIframes = (root, depth) => {{
                if (!root || depth > 64) return false;
                if (root.nodeType === 1) {{
                    if (!isVisible(root)) return false;
                    if (root.tagName === 'IFRAME') {{
                        try {{
                            if (root.contentDocument && checkDoc(root.contentDocument, depth + 1)) return true;
                        }} catch {{}}
                    }}
                    if (root.shadowRoot && !seenShadow.has(root.shadowRoot)) {{
                        seenShadow.add(root.shadowRoot);
                        const parts = [];
                        collectShadowText(root.shadowRoot, depth + 1, parts);
                        if (parts.join('').includes(needle)) return true;
                        if (scanShadowsAndIframes(root.shadowRoot, depth + 1)) return true;
                    }}
                }}
                if (root.childNodes) {{
                    for (const ch of root.childNodes) {{
                        if (scanShadowsAndIframes(ch, depth + 1)) return true;
                    }}
                }}
                return false;
            }};
            const checkDoc = (doc, depth) => {{
                if (!doc || !doc.body || depth > 64 || seenDocs.has(doc)) return false;
                seenDocs.add(doc);
                const txt = typeof doc.body.innerText === 'string' ? doc.body.innerText : (doc.body.textContent || '');
                if (txt.includes(needle)) return true;
                if (scanShadowsAndIframes(doc.body, depth)) return true;
                for (const fr of doc.querySelectorAll('iframe')) {{
                    try {{
                        if (fr.contentDocument && checkDoc(fr.contentDocument, depth + 1)) return true;
                    }} catch {{}}
                }}
                return false;
            }};
            return checkDoc(document, 0);
        }})()"#
    );

    let (deadline, poll) = compute_wait_deadline(opts.timeout_ms, opts.poll_interval_ms)?;

    loop {
        match session.eval_isolated(&js).await {
            Ok(v) if v.as_bool() == Some(true) => return Ok(()),
            Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => return Err(e),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(MimikyuError::Timeout {
                timeout_ms: opts.timeout_ms,
                operation: format!("wait_text('{text}')"),
            });
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(poll.min(rem)).await;
    }
}

pub async fn wait_url(
    session: &dyn DriverSession,
    substr: &str,
    opts: WaitOptions,
) -> Result<()> {
    if substr.is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "wait_url requires a non-empty substring".to_string(),
        ));
    }
    let (deadline, poll) = compute_wait_deadline(opts.timeout_ms, opts.poll_interval_ms)?;

    loop {
        match session.current_url().await {
            Ok(u) if u.contains(substr) => return Ok(()),
            Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => return Err(e),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(MimikyuError::Timeout {
                timeout_ms: opts.timeout_ms,
                operation: format!("wait_url('{substr}')"),
            });
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(poll.min(rem)).await;
    }
}

pub async fn wait_selector(
    session: &dyn DriverSession,
    selector: &str,
    state: WaitSelectorState,
    opts: WaitOptions,
) -> Result<()> {
    if selector.trim().is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "wait_selector requires a non-empty selector".to_string(),
        ));
    }
    let (deadline, poll) = compute_wait_deadline(opts.timeout_ms, opts.poll_interval_ms)?;
    prevalidate_css_selector(session, selector).await?;

    let sel_json = serde_json::to_string(selector)?;
    let state_str = match state {
        WaitSelectorState::Attached => "attached",
        WaitSelectorState::Visible => "visible",
        WaitSelectorState::Hidden => "hidden",
        WaitSelectorState::Detached => "detached",
    };
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({sel_json});
            const mode = "{state_str}";
            if (!el) return mode === "detached" || mode === "hidden";
            if (mode === "attached") return true;
            if (mode === "detached") return false;
            const cs = window.getComputedStyle(el);
            const r = el.getBoundingClientRect();
            const isVis = cs.display !== "none" && cs.visibility !== "hidden" && cs.opacity !== "0" && r.width > 0 && r.height > 0;
            return mode === "visible" ? isVis : !isVis;
        }})()"#
    );

    loop {
        match session.eval_isolated(&js).await {
            Ok(v) if v.as_bool() == Some(true) => return Ok(()),
            Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => return Err(e),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(MimikyuError::Timeout {
                timeout_ms: opts.timeout_ms,
                operation: format!("wait_selector('{selector}', {state:?})"),
            });
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(poll.min(rem)).await;
    }
}
