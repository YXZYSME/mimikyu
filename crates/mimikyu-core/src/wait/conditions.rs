use std::time::Duration;

use tokio::time::Instant;

use crate::driver::DriverSession;
use crate::error::{MimikyuError, Result};
use crate::wait::{WaitOptions, WaitSelectorState};

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
            const checkDoc = (doc) => {{
                if (!doc || !doc.body) return false;
                const txt = doc.body.innerText || doc.body.textContent || '';
                if (txt.includes(needle)) return true;
                for (const fr of doc.querySelectorAll('iframe')) {{
                    try {{
                        if (fr.contentDocument && checkDoc(fr.contentDocument)) return true;
                    }} catch {{}}
                }}
                return false;
            }};
            return checkDoc(document);
        }})()"#
    );

    let deadline = Instant::now() + Duration::from_millis(opts.timeout_ms);
    let poll = Duration::from_millis(opts.poll_interval_ms.max(5));

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
    let deadline = Instant::now() + Duration::from_millis(opts.timeout_ms);
    let poll = Duration::from_millis(opts.poll_interval_ms.max(5));

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

    let deadline = Instant::now() + Duration::from_millis(opts.timeout_ms);
    let poll = Duration::from_millis(opts.poll_interval_ms.max(5));

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
