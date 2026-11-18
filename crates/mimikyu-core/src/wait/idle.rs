use std::time::Duration;

use tokio::time::Instant;

use crate::driver::{BoundingBox, DriverSession};
use crate::error::{MimikyuError, Result};
use crate::snapshot::{RefMap, TargetLocator};
use crate::wait::WaitIdleOptions;

pub(crate) fn boxes_within_epsilon(a: BoundingBox, b: BoundingBox, eps: f64) -> bool {
    (a.x - b.x).abs() <= eps
        && (a.y - b.y).abs() <= eps
        && (a.width - b.width).abs() <= eps
        && (a.height - b.height).abs() <= eps
}

async fn resolve_target_box(
    session: &dyn DriverSession,
    ref_map: Option<&RefMap>,
    target: &str,
) -> Result<BoundingBox> {
    match TargetLocator::parse(target) {
        TargetLocator::Ref(rid) => {
            let map = ref_map.ok_or_else(|| {
                MimikyuError::ElementNotFound(format!(
                    "cannot resolve '{rid}' without active RefMap"
                ))
            })?;
            let entry = map.get(rid).ok_or_else(|| {
                MimikyuError::ElementNotFound(format!("unknown ref '{rid}'"))
            })?;
            if let Some(bid) = entry.backend_node_id
                && let Ok(b) = session
                    .resolve_node_box(bid, entry.frame_id.as_deref())
                    .await
            {
                return Ok(b);
            }
            if let Some(ref css) = entry.css_fallback {
                return session.resolve_selector_box(css).await;
            }
            Err(MimikyuError::ElementNotFound(format!(
                "unable to resolve box for '{rid}'"
            )))
        }
        TargetLocator::Selector(sel) => session.resolve_selector_box(sel).await,
    }
}

pub async fn wait_idle(
    session: &dyn DriverSession,
    ref_map: Option<&RefMap>,
    opts: WaitIdleOptions,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(opts.timeout_ms);
    let poll = Duration::from_millis(opts.poll_interval_ms.max(5));
    let quiet_dur = Duration::from_millis(opts.network_quiet_ms);

    let mut last_seq: Option<u64> = None;
    let mut quiet_since: Option<Instant> = None;
    let mut prev_box: Option<BoundingBox> = None;
    let mut stable_count: u32 = 0;
    let mut last_ready = String::from("unknown");
    let mut last_inflight = 0usize;

    loop {
        // Phase 1: Document load state
        match session.document_ready_state().await {
            Ok(state) => {
                last_ready = state;
                if last_ready != "complete" {
                    quiet_since = None;
                    prev_box = None;
                    stable_count = 0;
                    if Instant::now() >= deadline {
                        return Err(MimikyuError::Timeout {
                            timeout_ms: opts.timeout_ms,
                            operation: format!(
                                "wait_idle (readyState={last_ready}, inflight={last_inflight})"
                            ),
                        });
                    }
                    tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now())))
                        .await;
                    continue;
                }
            }
            Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => return Err(e),
            Err(_) => {
                quiet_since = None;
                prev_box = None;
                stable_count = 0;
                if Instant::now() >= deadline {
                    return Err(MimikyuError::Timeout {
                        timeout_ms: opts.timeout_ms,
                        operation: format!(
                            "wait_idle (readyState={last_ready}, inflight={last_inflight})"
                        ),
                    });
                }
                tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now())))
                    .await;
                continue;
            }
        }

        // Phase 2: Inflight network + activity_seq quiet window
        let (inflight, seq) = session
            .inflight_activity(opts.ignore_long_lived_sockets)
            .await?;
        last_inflight = inflight;
        let seq_changed = last_seq.is_some_and(|prev| prev != seq);
        last_seq = Some(seq);

        if inflight > 0 || seq_changed {
            quiet_since = None;
            prev_box = None;
            stable_count = 0;
        } else {
            if quiet_since.is_none() {
                quiet_since = Some(Instant::now());
            }
            let quiet_elapsed = quiet_since.map(|t| t.elapsed()).unwrap_or_default();
            if quiet_elapsed >= quiet_dur {
                // Phase 3: Optional target bounding-box stability
                let box_ok = if let Some(ref target) = opts.stable_target {
                    let needed = opts.bbox_stable_frames.max(2);
                    match resolve_target_box(session, ref_map, target).await {
                        Ok(curr) if curr.is_visible() => {
                            if let Some(prev) = prev_box
                                && boxes_within_epsilon(prev, curr, 0.5)
                            {
                                stable_count += 1;
                            } else {
                                prev_box = Some(curr);
                                stable_count = 1;
                            }
                            stable_count >= needed
                        }
                        Ok(_) | Err(_) => {
                            prev_box = None;
                            stable_count = 0;
                            false
                        }
                    }
                } else {
                    true
                };

                if box_ok {
                    // Phase 4: Two-rAF paint yield + re-verify zero new network transitions
                    match session
                        .eval_isolated(
                            "new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))",
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_))) => {
                            return Err(e);
                        }
                        Err(_) => {
                            quiet_since = None;
                            prev_box = None;
                            stable_count = 0;
                            if Instant::now() >= deadline {
                                return Err(MimikyuError::Timeout {
                                    timeout_ms: opts.timeout_ms,
                                    operation: format!(
                                        "wait_idle (readyState={last_ready}, inflight={last_inflight})"
                                    ),
                                });
                            }
                            let rem = deadline.saturating_duration_since(Instant::now());
                            tokio::time::sleep(poll.min(rem)).await;
                            continue;
                        }
                    }
                    let (post_inflight, post_seq) = session
                        .inflight_activity(opts.ignore_long_lived_sockets)
                        .await?;
                    if post_inflight == 0 && post_seq == seq {
                        return Ok(());
                    }
                    last_seq = Some(post_seq);
                    last_inflight = post_inflight;
                    quiet_since = None;
                    prev_box = None;
                    stable_count = 0;
                }
            }
        }

        if Instant::now() >= deadline {
            return Err(MimikyuError::Timeout {
                timeout_ms: opts.timeout_ms,
                operation: format!(
                    "wait_idle (readyState={last_ready}, inflight={last_inflight})"
                ),
            });
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(poll.min(rem)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_within_epsilon_checks_all_dimensions() {
        let a = BoundingBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 40.0,
        };
        let b = BoundingBox {
            x: 10.4,
            y: 19.6,
            width: 100.2,
            height: 40.0,
        };
        let c = BoundingBox {
            x: 12.0,
            y: 20.0,
            width: 100.0,
            height: 40.0,
        };
        assert!(boxes_within_epsilon(a, b, 0.5));
        assert!(!boxes_within_epsilon(a, c, 0.5));
    }
}
