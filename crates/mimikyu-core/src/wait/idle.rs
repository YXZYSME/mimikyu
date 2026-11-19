use std::time::Duration;

use tokio::time::Instant;

use crate::driver::{BoundingBox, DriverSession};
use crate::error::{MimikyuError, Result};
use crate::snapshot::{RefMap, TargetLocator};
use crate::wait::{compute_wait_deadline, WaitIdleOptions, MAX_WAIT_TIMEOUT_MS};

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
    let (deadline, poll) = compute_wait_deadline(opts.timeout_ms, opts.poll_interval_ms)?;
    if opts.network_quiet_ms > MAX_WAIT_TIMEOUT_MS {
        return Err(MimikyuError::InvalidConfig(format!(
            "wait network_quiet_ms {} exceeds maximum {MAX_WAIT_TIMEOUT_MS}ms",
            opts.network_quiet_ms
        )));
    }
    let quiet_dur = Duration::from_millis(opts.network_quiet_ms);

    if let Some(ref target) = opts.stable_target {
        match TargetLocator::parse(target) {
            TargetLocator::Ref(rid) => {
                let map = ref_map.ok_or_else(|| {
                    MimikyuError::ElementNotFound(format!(
                        "cannot resolve '{rid}' without active RefMap"
                    ))
                })?;
                if map.get(rid).is_none() {
                    return Err(MimikyuError::ElementNotFound(format!(
                        "unknown ref '{rid}'"
                    )));
                }
            }
            TargetLocator::Selector(sel) => {
                super::conditions::prevalidate_css_selector(session, sel).await?;
            }
        }
    }

    let mut last_seq: Option<u64> = None;
    let mut quiet_since: Option<Instant> = None;
    let mut prev_box: Option<BoundingBox> = None;
    let mut stable_count: u32 = 0;
    let mut last_ready = String::from("unknown");
    let mut last_inflight = 0usize;

    let timeout_err = |ready: &str, inflight: usize| MimikyuError::Timeout {
        timeout_ms: opts.timeout_ms,
        operation: format!("wait_idle (readyState={ready}, inflight={inflight})"),
    };

    loop {
        // Phase 1: Document load state (bounded by remaining deadline)
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(timeout_err(&last_ready, last_inflight));
        }
        match tokio::time::timeout(rem, session.document_ready_state()).await {
            Ok(Ok(state)) => {
                last_ready = state;
                if last_ready != "complete" {
                    quiet_since = None;
                    prev_box = None;
                    stable_count = 0;
                    if Instant::now() >= deadline {
                        return Err(timeout_err(&last_ready, last_inflight));
                    }
                    tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now())))
                        .await;
                    continue;
                }
            }
            Ok(Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_)))) => return Err(e),
            Ok(Err(_)) => {
                quiet_since = None;
                prev_box = None;
                stable_count = 0;
                if Instant::now() >= deadline {
                    return Err(timeout_err(&last_ready, last_inflight));
                }
                tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now())))
                    .await;
                continue;
            }
            Err(_) => return Err(timeout_err(&last_ready, last_inflight)),
        }

        // Phase 2: Inflight network + activity_seq quiet window
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(timeout_err(&last_ready, last_inflight));
        }
        let (inflight, seq) = match tokio::time::timeout(
            rem,
            session.inflight_activity(opts.ignore_long_lived_sockets),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => return Err(timeout_err(&last_ready, last_inflight)),
        };
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
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        return Err(timeout_err(&last_ready, last_inflight));
                    }
                    match tokio::time::timeout(rem, resolve_target_box(session, ref_map, target))
                        .await
                    {
                        Ok(Ok(curr)) if curr.is_visible() => {
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
                        Ok(Ok(_) | Err(_)) => {
                            prev_box = None;
                            stable_count = 0;
                            false
                        }
                        Err(_) => return Err(timeout_err(&last_ready, last_inflight)),
                    }
                } else {
                    true
                };

                if box_ok {
                    // Phase 4: Two-rAF paint yield (bounded by remaining deadline) + re-verify zero new network transitions
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        return Err(timeout_err(&last_ready, last_inflight));
                    }
                    match tokio::time::timeout(
                        rem,
                        session.eval_isolated(
                            r#"new Promise((resolve) => {
                                let settled = false;
                                const finish = () => {
                                    if (!settled) {
                                        settled = true;
                                        resolve(true);
                                    }
                                };
                                if (document.hidden || document.visibilityState === "hidden" || typeof requestAnimationFrame !== "function") {
                                    setTimeout(() => setTimeout(finish, 16), 16);
                                    return;
                                }
                                const fallback = setTimeout(finish, 50);
                                requestAnimationFrame(() => {
                                    requestAnimationFrame(() => {
                                        clearTimeout(fallback);
                                        finish();
                                    });
                                });
                            })"#,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(e @ (MimikyuError::Disposed | MimikyuError::Transport(_)))) => {
                            return Err(e);
                        }
                        Ok(Err(_)) => {
                            quiet_since = None;
                            prev_box = None;
                            stable_count = 0;
                            if Instant::now() >= deadline {
                                return Err(timeout_err(&last_ready, last_inflight));
                            }
                            let rem = deadline.saturating_duration_since(Instant::now());
                            tokio::time::sleep(poll.min(rem)).await;
                            continue;
                        }
                        Err(_) => return Err(timeout_err(&last_ready, last_inflight)),
                    }
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        return Err(timeout_err(&last_ready, last_inflight));
                    }
                    let (post_inflight, post_seq) = match tokio::time::timeout(
                        rem,
                        session.inflight_activity(opts.ignore_long_lived_sockets),
                    )
                    .await
                    {
                        Ok(res) => res?,
                        Err(_) => return Err(timeout_err(&last_ready, last_inflight)),
                    };
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
            return Err(timeout_err(&last_ready, last_inflight));
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(poll.min(rem)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::Value;

    use crate::driver::{
        AxNode, ConsoleEntry, ContextOptions, Cookie, DriverCapabilities, EngineKind, EnvProfile,
        FrameInfo, KeyEvent, MouseEvent, NavInfo, NetworkEvent, PageError, PermissionGrant,
        RouteMock, ScreenshotOpts, SelectBy, SessionInfo, ShadowMatch, StorageState, TabInfo,
        WheelEvent,
    };
    use crate::network::{BlockPolicy, RouteRule};

    struct StallingSession {
        info: SessionInfo,
        stall_ready_state: bool,
        stall_eval: bool,
    }

    impl StallingSession {
        fn new(stall_ready_state: bool, stall_eval: bool) -> Self {
            Self {
                info: SessionInfo {
                    session_id: "stub".to_string(),
                    engine: EngineKind::Chromium,
                    browser_version: "0".to_string(),
                    context_id: None,
                    target_id: "stub".to_string(),
                    profile_dir: None,
                    ws_endpoint: String::new(),
                    pid: None,
                },
                stall_ready_state,
                stall_eval,
            }
        }
    }

    #[async_trait]
    impl DriverSession for StallingSession {
        fn info(&self) -> &SessionInfo {
            &self.info
        }
        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities {
                engine: EngineKind::Chromium,
                protocol: "cdp".to_string(),
                supports_virtual_context: true,
                supports_per_context_proxy: true,
                supports_native_ax_tree: true,
                supports_isolated_world: true,
                supports_closed_shadow_dom: true,
                supports_pdf: true,
                supports_network_intercept: true,
            }
        }
        async fn new_context(&self, _opts: ContextOptions) -> Result<Arc<dyn DriverSession>> {
            Err(MimikyuError::Disposed)
        }
        async fn new_tab(&self, _url: Option<&str>) -> Result<Arc<dyn DriverSession>> {
            Err(MimikyuError::Disposed)
        }
        async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
            Ok(Vec::new())
        }
        async fn activate_tab(&self, _target_id: &str) -> Result<()> {
            Ok(())
        }
        async fn navigate(&self, url: &str) -> Result<NavInfo> {
            Ok(NavInfo {
                url: url.to_string(),
                status: Some(200),
                title: String::new(),
                frame_id: None,
            })
        }
        async fn reload(&self) -> Result<NavInfo> {
            self.navigate("about:blank").await
        }
        async fn go_back(&self) -> Result<NavInfo> {
            self.navigate("about:blank").await
        }
        async fn go_forward(&self) -> Result<NavInfo> {
            self.navigate("about:blank").await
        }
        async fn current_url(&self) -> Result<String> {
            Ok("about:blank".to_string())
        }
        async fn title(&self) -> Result<String> {
            Ok(String::new())
        }
        async fn eval_isolated(&self, _expr: &str) -> Result<Value> {
            if self.stall_eval {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Ok(Value::Bool(true))
        }
        async fn eval_main_world(&self, _expr: &str) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn list_frames(&self) -> Result<Vec<FrameInfo>> {
            Ok(Vec::new())
        }
        async fn eval_isolated_in_frame(
            &self,
            expr: &str,
            _frame_id: Option<&str>,
        ) -> Result<Value> {
            self.eval_isolated(expr).await
        }
        async fn eval_main_world_in_frame(
            &self,
            _expr: &str,
            _frame_id: Option<&str>,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn call_function_on_node(
            &self,
            _backend_node_id: i64,
            _frame_id: Option<&str>,
            _function_declaration: &str,
            _args: Vec<Value>,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn query_shadow(
            &self,
            _selector_chain: &[String],
            _frame_id: Option<&str>,
        ) -> Result<Vec<ShadowMatch>> {
            Ok(Vec::new())
        }
        async fn closed_shadow_hosts(&self, _frame_id: Option<&str>) -> Result<Vec<i64>> {
            Ok(Vec::new())
        }
        async fn add_init_script(&self, _script: &str) -> Result<String> {
            Ok(String::new())
        }
        async fn remove_init_script(&self, _script_id: &str) -> Result<()> {
            Ok(())
        }
        async fn accessibility_tree(&self) -> Result<Vec<AxNode>> {
            Ok(Vec::new())
        }
        async fn resolve_node_box(
            &self,
            _backend_node_id: i64,
            _frame_id: Option<&str>,
        ) -> Result<BoundingBox> {
            Err(MimikyuError::ElementNotFound("stub".to_string()))
        }
        async fn resolve_selector_box(&self, _selector: &str) -> Result<BoundingBox> {
            Err(MimikyuError::ElementNotFound("stub".to_string()))
        }
        async fn viewport_size(&self) -> Result<(f64, f64)> {
            Ok((1280.0, 720.0))
        }
        async fn scroll_into_view(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn clear_node_value(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn select_option(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
            _by: SelectBy,
        ) -> Result<String> {
            Ok(String::new())
        }
        async fn focus_node(
            &self,
            _backend_node_id: Option<i64>,
            _selector: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn set_file_input_files(
            &self,
            _backend_node_id: Option<i64>,
            _frame_id: Option<&str>,
            _selector: Option<&str>,
            _files: Vec<PathBuf>,
        ) -> Result<()> {
            Ok(())
        }
        async fn css_selector_for_node(
            &self,
            _backend_node_id: i64,
            _frame_id: Option<&str>,
        ) -> Result<Option<String>> {
            Ok(None)
        }
        async fn dispatch_mouse(&self, _ev: MouseEvent) -> Result<()> {
            Ok(())
        }
        async fn dispatch_key(&self, _ev: KeyEvent) -> Result<()> {
            Ok(())
        }
        async fn dispatch_wheel(&self, _ev: WheelEvent) -> Result<()> {
            Ok(())
        }
        async fn insert_text(&self, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _opts: ScreenshotOpts) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn pdf(&self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn set_instant_animations(&self, _enabled: bool) -> Result<()> {
            Ok(())
        }
        async fn set_env_overrides(&self, _profile: EnvProfile) -> Result<()> {
            Ok(())
        }
        async fn set_permissions(&self, _permissions: Vec<PermissionGrant>) -> Result<()> {
            Ok(())
        }
        async fn reset_permissions(&self) -> Result<()> {
            Ok(())
        }
        async fn set_block_policy(&self, _policy: BlockPolicy) -> Result<()> {
            Ok(())
        }
        async fn set_blocked_urls(&self, _patterns: Vec<String>) -> Result<()> {
            Ok(())
        }
        async fn set_route_mock(&self, _mock: RouteMock) -> Result<()> {
            Ok(())
        }
        async fn set_route_rule(&self, _rule: RouteRule) -> Result<()> {
            Ok(())
        }
        async fn clear_route_mocks(&self) -> Result<()> {
            Ok(())
        }
        async fn cookies(&self) -> Result<Vec<Cookie>> {
            Ok(Vec::new())
        }
        async fn set_cookies(&self, _cookies: Vec<Cookie>) -> Result<()> {
            Ok(())
        }
        async fn clear_cookies(&self) -> Result<()> {
            Ok(())
        }
        async fn storage_state(&self) -> Result<StorageState> {
            Ok(StorageState::default())
        }
        async fn set_storage_state(&self, _state: StorageState) -> Result<()> {
            Ok(())
        }
        async fn console_logs(&self) -> Result<Vec<ConsoleEntry>> {
            Ok(Vec::new())
        }
        async fn page_errors(&self) -> Result<Vec<PageError>> {
            Ok(Vec::new())
        }
        async fn network_events(&self) -> Result<Vec<NetworkEvent>> {
            Ok(Vec::new())
        }
        async fn drain_console_logs(&self) -> Result<Vec<ConsoleEntry>> {
            Ok(Vec::new())
        }
        async fn drain_page_errors(&self) -> Result<Vec<PageError>> {
            Ok(Vec::new())
        }
        async fn drain_network_events(&self) -> Result<Vec<NetworkEvent>> {
            Ok(Vec::new())
        }
        async fn clear_telemetry(&self) -> Result<()> {
            Ok(())
        }
        async fn inflight_requests(&self) -> Result<usize> {
            Ok(0)
        }
        async fn inflight_activity(&self, _ignore_long_lived: bool) -> Result<(usize, u64)> {
            Ok((0, 0))
        }
        async fn document_ready_state(&self) -> Result<String> {
            if self.stall_ready_state {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Ok("complete".to_string())
        }
        async fn dispose(&self) -> Result<()> {
            Ok(())
        }
    }

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

    #[tokio::test]
    async fn wait_idle_bounds_stalled_ready_state_and_raf_by_deadline() {
        let opts = WaitIdleOptions {
            timeout_ms: 80,
            poll_interval_ms: 15,
            network_quiet_ms: 0,
            ..Default::default()
        };

        let s_ready = StallingSession::new(true, false);
        let t0 = Instant::now();
        let err1 = wait_idle(&s_ready, None, opts.clone())
            .await
            .expect_err("stalled document_ready_state must time out at deadline");
        assert!(
            matches!(err1, MimikyuError::Timeout { timeout_ms: 80, .. }),
            "expected Timeout(80), got {err1:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "wait_idle overshot timeout_ms on stalled readyState: {:?}",
            t0.elapsed()
        );

        let s_raf = StallingSession::new(false, true);
        let t1 = Instant::now();
        let err2 = wait_idle(&s_raf, None, opts)
            .await
            .expect_err("stalled 2-rAF eval_isolated must time out at deadline");
        assert!(
            matches!(err2, MimikyuError::Timeout { timeout_ms: 80, .. }),
            "expected Timeout(80), got {err2:?}"
        );
        assert!(
            t1.elapsed() < Duration::from_millis(500),
            "wait_idle overshot timeout_ms on stalled 2-rAF eval: {:?}",
            t1.elapsed()
        );
    }
}
