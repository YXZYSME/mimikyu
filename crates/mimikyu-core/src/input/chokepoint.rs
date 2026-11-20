use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::driver::{
    BoundingBox, DriverSession, KeyEvent, KeyEventType, MOD_ALT, MOD_CONTROL, MOD_META, MOD_SHIFT,
    MouseButton, MouseEvent, MouseEventType, SelectBy, WheelEvent,
};
use crate::error::{MimikyuError, Result};
use crate::input::humanize::{
    HumanizeConfig, MAX_WHEEL_DELTA_PX, clamp_point_to_viewport, generate_trajectory, make_rng,
    sample_key_delay_ms, split_wheel_notches,
};
use crate::snapshot::{RefMap, TargetLocator};

pub const DEFAULT_INPUT_ACK_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputTarget {
    Ref(String),
    Selector(String),
}

impl InputTarget {
    pub fn parse(raw: &str) -> Self {
        match TargetLocator::parse(raw) {
            TargetLocator::Ref(r) => Self::Ref(r.to_string()),
            TargetLocator::Selector(s) => Self::Selector(s.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub backend_node_id: Option<i64>,
    pub frame_id: Option<String>,
    pub css_fallback: Option<String>,
    pub cached_bounds: Option<BoundingBox>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputOptions {
    #[serde(default)]
    pub humanize: HumanizeConfig,
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
}

fn default_ack_timeout_ms() -> u64 {
    DEFAULT_INPUT_ACK_TIMEOUT_MS
}

impl Default for InputOptions {
    fn default() -> Self {
        Self {
            humanize: HumanizeConfig::default(),
            ack_timeout_ms: DEFAULT_INPUT_ACK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickOptions {
    #[serde(default)]
    pub button: MouseButton,
    #[serde(default = "default_click_count")]
    pub click_count: u32,
    #[serde(default)]
    pub modifiers: u32,
    #[serde(default)]
    pub humanize: HumanizeConfig,
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
}

fn default_click_count() -> u32 {
    1
}

impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            click_count: 1,
            modifiers: 0,
            humanize: HumanizeConfig::default(),
            ack_timeout_ms: DEFAULT_INPUT_ACK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillOptions {
    #[serde(default)]
    pub humanize: HumanizeConfig,
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
}

impl Default for FillOptions {
    fn default() -> Self {
        Self {
            humanize: HumanizeConfig::default(),
            ack_timeout_ms: DEFAULT_INPUT_ACK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeOptions {
    #[serde(default)]
    pub humanize: HumanizeConfig,
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
}

impl Default for TypeOptions {
    fn default() -> Self {
        Self {
            humanize: HumanizeConfig::default(),
            ack_timeout_ms: DEFAULT_INPUT_ACK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollOptions {
    #[serde(default)]
    pub modifiers: u32,
    #[serde(default)]
    pub humanize: HumanizeConfig,
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
}

impl Default for ScrollOptions {
    fn default() -> Self {
        Self {
            modifiers: 0,
            humanize: HumanizeConfig::default(),
            ack_timeout_ms: DEFAULT_INPUT_ACK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub modifiers: u32,
    pub mod_keys: Vec<String>,
    pub key: String,
    pub code: Option<String>,
    pub windows_virtual_key_code: Option<u32>,
}

impl KeyChord {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "key specification cannot be empty".to_string(),
            ));
        }

        // Special-case literal "+" key
        if trimmed == "+" {
            return Ok(Self {
                modifiers: 0,
                mod_keys: Vec::new(),
                key: "+".to_string(),
                code: None,
                windows_virtual_key_code: None,
            });
        }

        let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        if parts.iter().any(|p| p.is_empty()) && !trimmed.ends_with("++") {
            return Err(MimikyuError::InvalidConfig(format!(
                "invalid key chord '{raw}'"
            )));
        }

        let mut modifiers = 0u32;
        let mut mod_keys = Vec::new();
        let (mod_slice, base_raw) = if trimmed.ends_with("++") {
            (&parts[..parts.len().saturating_sub(2)], "+")
        } else {
            (&parts[..parts.len() - 1], parts[parts.len() - 1])
        };

        for m in mod_slice {
            match m.to_ascii_lowercase().as_str() {
                "control" | "ctrl" => {
                    if modifiers & MOD_CONTROL == 0 {
                        modifiers |= MOD_CONTROL;
                        mod_keys.push("Control".to_string());
                    }
                }
                "alt" | "option" => {
                    if modifiers & MOD_ALT == 0 {
                        modifiers |= MOD_ALT;
                        mod_keys.push("Alt".to_string());
                    }
                }
                "shift" => {
                    if modifiers & MOD_SHIFT == 0 {
                        modifiers |= MOD_SHIFT;
                        mod_keys.push("Shift".to_string());
                    }
                }
                "meta" | "cmd" | "command" | "super" | "win" => {
                    if modifiers & MOD_META == 0 {
                        modifiers |= MOD_META;
                        mod_keys.push("Meta".to_string());
                    }
                }
                other => {
                    return Err(MimikyuError::InvalidConfig(format!(
                        "unknown modifier '{other}' in key chord '{raw}'"
                    )));
                }
            }
        }

        let key = normalize_base_key(base_raw);
        let (code, vk) = key_code_and_vk(&key);
        Ok(Self {
            modifiers,
            mod_keys,
            key,
            code,
            windows_virtual_key_code: vk,
        })
    }
}

fn normalize_base_key(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "plus" => "+".to_string(),
        "enter" => "Enter".to_string(),
        "return" => "Return".to_string(),
        "tab" => "Tab".to_string(),
        "escape" | "esc" => "Escape".to_string(),
        "space" => "Space".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "insert" => "Insert".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        "arrowleft" | "left" => "ArrowLeft".to_string(),
        "arrowup" | "up" => "ArrowUp".to_string(),
        "arrowright" | "right" => "ArrowRight".to_string(),
        "arrowdown" | "down" => "ArrowDown".to_string(),
        "f1" => "F1".to_string(),
        "f2" => "F2".to_string(),
        "f3" => "F3".to_string(),
        "f4" => "F4".to_string(),
        "f5" => "F5".to_string(),
        "f6" => "F6".to_string(),
        "f7" => "F7".to_string(),
        "f8" => "F8".to_string(),
        "f9" => "F9".to_string(),
        "f10" => "F10".to_string(),
        "f11" => "F11".to_string(),
        "f12" => "F12".to_string(),
        _ => raw.to_string(),
    }
}

fn key_code_and_vk(key: &str) -> (Option<String>, Option<u32>) {
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let upper = ch.to_ascii_uppercase();
        if upper.is_ascii_alphabetic() {
            return (Some(format!("Key{upper}")), Some(upper as u32));
        }
        if ch.is_ascii_digit() {
            return (Some(format!("Digit{ch}")), Some(ch as u32));
        }
    }
    (None, None)
}

pub fn target_click_point(bbox: BoundingBox, vp_w: f64, vp_h: f64) -> (f64, f64) {
    let ix0 = bbox.x.max(0.0);
    let iy0 = bbox.y.max(0.0);
    let ix1 = (bbox.x + bbox.width).min(vp_w);
    let iy1 = (bbox.y + bbox.height).min(vp_h);

    let (raw_x, raw_y) = if ix1 > ix0 && iy1 > iy0 {
        ((ix0 + ix1) * 0.5, (iy0 + iy1) * 0.5)
    } else {
        bbox.center()
    };
    clamp_point_to_viewport(raw_x, raw_y, vp_w, vp_h)
}

pub(crate) fn resolve_safe_click_point(
    bbox: BoundingBox,
    vp_w: f64,
    vp_h: f64,
    gutter_r: f64,
    gutter_b: f64,
) -> Result<(f64, f64)> {
    let safe_w = (vp_w - gutter_r.max(0.0)).max(1.0);
    let safe_h = (vp_h - gutter_b.max(0.0)).max(1.0);
    let in_raw_vp = bbox.x.max(0.0) < vp_w
        && (bbox.x + bbox.width).min(vp_w) > 0.0
        && bbox.y.max(0.0) < vp_h
        && (bbox.y + bbox.height).min(vp_h) > 0.0;
    let in_safe_vp = bbox.x.max(0.0) < safe_w
        && (bbox.x + bbox.width).min(safe_w) > 0.0
        && bbox.y.max(0.0) < safe_h
        && (bbox.y + bbox.height).min(safe_h) > 0.0;
    if in_raw_vp && !in_safe_vp {
        return Err(MimikyuError::ElementNotFound(
            "target element is occluded by viewport scrollbar".to_string(),
        ));
    }
    Ok(target_click_point(bbox, safe_w, safe_h))
}

#[derive(Debug, Default)]
struct InputState {
    cursor: Option<(f64, f64)>,
}

pub struct ChokepointInput {
    session: Arc<dyn DriverSession>,
    gate: Mutex<InputState>,
    default_ack_timeout: Duration,
}

impl ChokepointInput {
    pub fn new(session: Arc<dyn DriverSession>) -> Self {
        Self {
            session,
            gate: Mutex::new(InputState::default()),
            default_ack_timeout: Duration::from_millis(DEFAULT_INPUT_ACK_TIMEOUT_MS),
        }
    }

    pub fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.default_ack_timeout = timeout;
        self
    }

    fn ack_duration(&self, ms: u64) -> Duration {
        if ms == 0 {
            self.default_ack_timeout
        } else {
            Duration::from_millis(ms)
        }
    }

    async fn run_with_ack<T, F>(&self, ack_timeout: Duration, op: &str, fut: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        match tokio::time::timeout(ack_timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(MimikyuError::Timeout {
                timeout_ms: ack_timeout.as_millis() as u64,
                operation: op.to_string(),
            }),
        }
    }

    fn resolve_target(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
    ) -> Result<ResolvedTarget> {
        match target {
            InputTarget::Selector(sel) => Ok(ResolvedTarget {
                backend_node_id: None,
                frame_id: None,
                css_fallback: Some(sel.clone()),
                cached_bounds: None,
            }),
            InputTarget::Ref(ref_id) => {
                let map = refs.ok_or_else(|| {
                    MimikyuError::ElementNotFound(format!(
                        "cannot resolve '{ref_id}' without an active snapshot"
                    ))
                })?;
                let entry = map.get(ref_id).ok_or_else(|| {
                    MimikyuError::ElementNotFound(format!(
                        "stale or unknown snapshot ref '{ref_id}' (take a fresh snapshot)"
                    ))
                })?;
                Ok(ResolvedTarget {
                    backend_node_id: entry.backend_node_id,
                    frame_id: entry.frame_id.clone(),
                    css_fallback: entry.css_fallback.clone(),
                    cached_bounds: entry.bounds,
                })
            }
        }
    }

    async fn resolve_box_for_target(
        &self,
        resolved: &ResolvedTarget,
        ack: Duration,
    ) -> Result<BoundingBox> {
        if let Some(bid) = resolved.backend_node_id {
            match self
                .run_with_ack(
                    ack,
                    "resolve_node_box",
                    self.session
                        .resolve_node_box(bid, resolved.frame_id.as_deref()),
                )
                .await
            {
                Ok(b) => return Ok(b),
                Err(e) => {
                    if let Some(ref css) = resolved.css_fallback
                        && let Ok(b) = self
                            .run_with_ack(
                                ack,
                                "resolve_selector_box",
                                self.session.resolve_selector_box(css),
                            )
                            .await
                    {
                        return Ok(b);
                    }
                    return Err(e);
                }
            }
        }
        if let Some(ref css) = resolved.css_fallback {
            return self
                .run_with_ack(
                    ack,
                    "resolve_selector_box",
                    self.session.resolve_selector_box(css),
                )
                .await;
        }
        resolved.cached_bounds.ok_or_else(|| {
            MimikyuError::ElementNotFound("target has no resolvable geometry".to_string())
        })
    }

    async fn resolve_target_in_viewport(
        &self,
        resolved: &ResolvedTarget,
        ack: Duration,
    ) -> Result<(BoundingBox, f64, f64, (f64, f64))> {
        let (raw_vp_w, raw_vp_h, gutter_r, gutter_b) = self
            .run_with_ack(
                ack,
                "viewport_and_gutters",
                self.session.viewport_and_gutters(),
            )
            .await?;
        let safe_w = (raw_vp_w - gutter_r.max(0.0)).max(1.0);
        let safe_h = (raw_vp_h - gutter_b.max(0.0)).max(1.0);
        let mut bbox = self.resolve_box_for_target(resolved, ack).await?;
        if !bbox.is_visible() {
            return Err(MimikyuError::ElementNotFound(
                "target element has zero width or height".to_string(),
            ));
        }

        const MAX_SCROLL_ATTEMPTS: usize = 3;
        let is_offscreen = |b: BoundingBox| -> bool {
            let (cx, cy) = b.center();
            cx < 0.0
                || cx >= safe_w
                || cy < 0.0
                || cy >= safe_h
                || b.x + b.width <= 0.0
                || b.y + b.height <= 0.0
                || b.x >= safe_w
                || b.y >= safe_h
        };

        let mut offscreen = is_offscreen(bbox);
        let mut attempts = 0usize;
        while offscreen && attempts < MAX_SCROLL_ATTEMPTS {
            attempts += 1;
            let prev_bbox = bbox;
            let before_main = self
                .read_scroll_offset(safe_w * 0.5, safe_h * 0.5, None, ack)
                .await;
            let before_frame = if let Some(fid) = resolved.frame_id.as_deref() {
                self.read_scroll_offset(safe_w * 0.5, safe_h * 0.5, Some(fid), ack)
                    .await
            } else {
                None
            };
            let _ = self
                .run_with_ack(
                    ack,
                    "scroll_into_view",
                    self.session.scroll_into_view(
                        resolved.backend_node_id,
                        resolved.frame_id.as_deref(),
                        resolved.css_fallback.as_deref(),
                    ),
                )
                .await;
            let mut scroll_moved = false;
            if let (Some(fid), Some(bf)) = (resolved.frame_id.as_deref(), before_frame) {
                scroll_moved |= self
                    .wait_for_scroll_settle(bf, safe_w * 0.5, safe_h * 0.5, Some(fid), ack)
                    .await;
            }
            if let Some(bm) = before_main {
                scroll_moved |= self
                    .wait_for_scroll_settle(bm, safe_w * 0.5, safe_h * 0.5, None, ack)
                    .await;
            }
            bbox = self.resolve_box_for_target(resolved, ack).await?;
            if !bbox.is_visible() {
                return Err(MimikyuError::ElementNotFound(
                    "target element became non-visible after scrolling".to_string(),
                ));
            }
            offscreen = is_offscreen(bbox);
            let bbox_unchanged =
                (bbox.x - prev_bbox.x).abs() < 0.5 && (bbox.y - prev_bbox.y).abs() < 0.5;
            if !offscreen || (bbox_unchanged && !scroll_moved) {
                break;
            }
        }

        let point = resolve_safe_click_point(bbox, raw_vp_w, raw_vp_h, gutter_r, gutter_b)?;
        Ok((bbox, safe_w, safe_h, point))
    }

    async fn move_cursor_to(
        &self,
        state: &mut InputState,
        target: (f64, f64),
        target_size: Option<(f64, f64)>,
        viewport: (f64, f64),
        humanize: &HumanizeConfig,
        ack: Duration,
    ) -> Result<()> {
        let (vp_w, vp_h) = viewport;
        let (tx, ty) = clamp_point_to_viewport(target.0, target.1, vp_w, vp_h);
        if humanize.enabled {
            let pts =
                generate_trajectory(state.cursor, (tx, ty), target_size, vp_w, vp_h, humanize);
            for pt in pts {
                if pt.delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(pt.delay_ms)).await;
                }
                self.run_with_ack(
                    ack,
                    "dispatch_mouse(Move)",
                    self.session.dispatch_mouse(MouseEvent {
                        event_type: MouseEventType::Move,
                        x: pt.x,
                        y: pt.y,
                        button: MouseButton::None,
                        click_count: 1,
                        modifiers: 0,
                    }),
                )
                .await?;
                state.cursor = Some((pt.x, pt.y));
            }
        } else {
            self.run_with_ack(
                ack,
                "dispatch_mouse(Move)",
                self.session.dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Move,
                    x: tx,
                    y: ty,
                    button: MouseButton::None,
                    click_count: 1,
                    modifiers: 0,
                }),
            )
            .await?;
            state.cursor = Some((tx, ty));
        }
        Ok(())
    }

    async fn send_mod_keys(&self, mask: u32, down: bool, ack: Duration) -> Result<()> {
        let mut keys = Vec::new();
        if mask & MOD_CONTROL != 0 {
            keys.push("Control");
        }
        if mask & MOD_ALT != 0 {
            keys.push("Alt");
        }
        if mask & MOD_SHIFT != 0 {
            keys.push("Shift");
        }
        if mask & MOD_META != 0 {
            keys.push("Meta");
        }
        if !down {
            keys.reverse();
        }
        let event_type = if down {
            KeyEventType::Down
        } else {
            KeyEventType::Up
        };
        for k in keys {
            self.run_with_ack(
                ack,
                "dispatch_key(mod)",
                self.session.dispatch_key(KeyEvent {
                    event_type,
                    key: k.to_string(),
                    code: None,
                    text: None,
                    windows_virtual_key_code: None,
                    modifiers: 0,
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn dispatch_typed_chars(
        &self,
        text: &str,
        humanize: Option<&HumanizeConfig>,
        ack: Duration,
    ) -> Result<()> {
        let mut state = humanize.map(|cfg| (cfg, make_rng(cfg.seed), None::<char>));
        for ch in text.chars() {
            if ch == '\r' {
                continue;
            }
            if let Some((cfg, rng, prev)) = state.as_mut() {
                let delay = sample_key_delay_ms(*prev, cfg, rng);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                *prev = Some(ch);
            }
            if ch == '\n' {
                self.run_with_ack(
                    ack,
                    "dispatch_key(Enter)",
                    self.session.dispatch_key(KeyEvent {
                        event_type: KeyEventType::Char,
                        key: "Enter".to_string(),
                        code: None,
                        text: None,
                        windows_virtual_key_code: None,
                        modifiers: 0,
                    }),
                )
                .await?;
            } else if ch == '\t' {
                self.run_with_ack(
                    ack,
                    "dispatch_key(Tab)",
                    self.session.dispatch_key(KeyEvent {
                        event_type: KeyEventType::Char,
                        key: "Tab".to_string(),
                        code: None,
                        text: None,
                        windows_virtual_key_code: None,
                        modifiers: 0,
                    }),
                )
                .await?;
            } else {
                let s = ch.to_string();
                self.run_with_ack(
                    ack,
                    "dispatch_key(Char)",
                    self.session.dispatch_key(KeyEvent {
                        event_type: KeyEventType::Char,
                        key: s.clone(),
                        code: None,
                        text: Some(s),
                        windows_virtual_key_code: None,
                        modifiers: 0,
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn focus_resolved(&self, resolved: &ResolvedTarget, ack: Duration) -> Result<()> {
        if let Some(bid) = resolved.backend_node_id
            && self
                .run_with_ack(
                    ack,
                    "call_function_on_node(focus)",
                    self.session.call_function_on_node(
                        bid,
                        resolved.frame_id.as_deref(),
                        "function() { const win = this.ownerDocument && this.ownerDocument.defaultView; if (win && win !== window && typeof win.focus === 'function') { try { win.focus(); } catch {} } if (typeof this.focus === 'function') this.focus(); return true; }",
                        vec![],
                    ),
                )
                .await
                .is_ok()
        {
            return Ok(());
        }
        self.run_with_ack(
            ack,
            "focus_node",
            self.session
                .focus_node(resolved.backend_node_id, resolved.css_fallback.as_deref()),
        )
        .await
    }

    async fn dispatch_change_event(&self, resolved: &ResolvedTarget, ack: Duration) {
        if let Some(bid) = resolved.backend_node_id {
            let _ = self
                .run_with_ack(
                    ack,
                    "dispatch_change",
                    self.session.call_function_on_node(
                        bid,
                        resolved.frame_id.as_deref(),
                        "function() { this.dispatchEvent(new Event('change', { bubbles: true })); return true; }",
                        vec![],
                    ),
                )
                .await;
        } else if let Some(ref sel) = resolved.css_fallback
            && let Ok(sel_json) = serde_json::to_string(sel)
        {
            let js = format!(
                "(() => {{ const el = document.querySelector({sel_json}); if (el) el.dispatchEvent(new Event('change', {{ bubbles: true }})); }})()"
            );
            let _ = self
                .run_with_ack(ack, "dispatch_change", self.session.eval_isolated(&js))
                .await;
        }
    }

    async fn prepare_firefox_click_focus(
        &self,
        target_frame_id: Option<&str>,
        tx: f64,
        ty: f64,
        ack: Duration,
    ) {
        if self.session.info().engine != crate::driver::EngineKind::Firefox {
            return;
        }
        let frames = self
            .run_with_ack(ack, "list_frames", self.session.list_frames())
            .await
            .unwrap_or_default();
        if frames.len() <= 1 {
            let blur_top_iframe_js = if tx.is_finite() && ty.is_finite() {
                format!(
                    "(() => {{ let cur = document.activeElement; while (cur && cur.shadowRoot && cur.shadowRoot.activeElement) cur = cur.shadowRoot.activeElement; if (cur && cur.tagName === 'IFRAME') {{ const r = cur.getBoundingClientRect(); if ({tx} < r.left || {tx} > r.right || {ty} < r.top || {ty} > r.bottom) cur.blur(); }} }})()"
                )
            } else if target_frame_id.is_none() {
                "(() => { let cur = document.activeElement; while (cur && cur.shadowRoot && cur.shadowRoot.activeElement) cur = cur.shadowRoot.activeElement; if (cur && cur.tagName === 'IFRAME') cur.blur(); })()".to_string()
            } else {
                return;
            };
            let _ = self
                .run_with_ack(
                    ack,
                    "blur_other_iframe",
                    self.session.eval_isolated(&blur_top_iframe_js),
                )
                .await;
            return;
        }

        let main_fid = frames
            .iter()
            .find(|f| f.is_main)
            .or_else(|| frames.first())
            .map(|f| f.frame_id.as_str());
        let effective_target_fid = target_frame_id
            .filter(|tfid| frames.iter().any(|f| f.frame_id == *tfid))
            .or(main_fid);

        let parent_map: std::collections::HashMap<&str, &str> = frames
            .iter()
            .filter_map(|f| {
                f.parent_frame_id
                    .as_deref()
                    .map(|p| (f.frame_id.as_str(), p))
            })
            .collect();
        let mut ancestors: std::collections::HashSet<&str> = std::collections::HashSet::new();
        if let Some(mut cur) = effective_target_fid {
            for _ in 0..32 {
                if !ancestors.insert(cur) {
                    break;
                }
                let Some(&p) = parent_map.get(cur) else {
                    break;
                };
                cur = p;
            }
        }

        let target_check_js = r#"(() => {
            let cur = document.activeElement;
            while (cur && cur.shadowRoot && cur.shadowRoot.activeElement) {
                cur = cur.shadowRoot.activeElement;
            }
            return {
                hasFocus: typeof document.hasFocus === 'function' ? document.hasFocus() : false,
                hasActive: Boolean(cur && cur !== document.body && cur !== document.documentElement),
                isIframe: Boolean(cur && cur.tagName === 'IFRAME')
            };
        })()"#;
        let target_state = if let Some(tfid) = effective_target_fid {
            self.run_with_ack(
                ack,
                "check_target_focus",
                self.session
                    .eval_isolated_in_frame(target_check_js, Some(tfid)),
            )
            .await
            .ok()
        } else {
            None
        };
        let target_has_focus = target_state
            .as_ref()
            .and_then(|v| v["hasFocus"].as_bool())
            .unwrap_or(false);
        let target_active_is_iframe = target_state
            .as_ref()
            .and_then(|v| v["isIframe"].as_bool())
            .unwrap_or(false);

        let mut other_frame_has_focus = false;
        if target_has_focus && !target_active_is_iframe {
            for f in &frames {
                let fid = f.frame_id.as_str();
                if Some(fid) == effective_target_fid {
                    continue;
                }
                if let Ok(st) = self
                    .run_with_ack(
                        ack,
                        "check_other_frame_focus",
                        self.session
                            .eval_isolated_in_frame(target_check_js, Some(fid)),
                    )
                    .await
                {
                    let is_other_active = if ancestors.contains(fid) {
                        st["hasActive"].as_bool() == Some(true)
                            && st["isIframe"].as_bool() != Some(true)
                    } else {
                        st["hasFocus"].as_bool() == Some(true)
                            || st["hasActive"].as_bool() == Some(true)
                    };
                    if is_other_active {
                        other_frame_has_focus = true;
                        break;
                    }
                }
            }
            if !other_frame_has_focus {
                return;
            }
        }

        let blur_any_js = r#"(() => {
            let cur = document.activeElement;
            while (cur && cur.shadowRoot && cur.shadowRoot.activeElement) {
                cur = cur.shadowRoot.activeElement;
            }
            if (cur && cur !== document.body && cur !== document.documentElement && typeof cur.blur === 'function') {
                cur.blur();
            }
            try {
                if (window.frameElement && typeof window.frameElement.blur === 'function') {
                    window.frameElement.blur();
                }
            } catch {}
            if (window !== window.top && typeof window.blur === 'function') {
                window.blur();
            }
        })()"#;
        let blur_non_iframe_active_js = r#"(() => {
            let cur = document.activeElement;
            while (cur && cur.shadowRoot && cur.shadowRoot.activeElement) {
                cur = cur.shadowRoot.activeElement;
            }
            if (cur && cur !== document.body && cur !== document.documentElement && cur.tagName !== 'IFRAME' && typeof cur.blur === 'function') {
                cur.blur();
            }
        })()"#;
        let blur_iframe_only_js = r#"(() => {
            let cur = document.activeElement;
            while (cur && cur.shadowRoot && cur.shadowRoot.activeElement) {
                cur = cur.shadowRoot.activeElement;
            }
            if (cur && cur.tagName === 'IFRAME' && typeof cur.blur === 'function') {
                cur.blur();
            }
        })()"#;
        let focus_target_window_js = r#"(() => {
            if (typeof window.focus === 'function') {
                window.focus();
            }
        })()"#;

        for f in frames.iter().rev() {
            let fid = f.frame_id.as_str();
            if Some(fid) == effective_target_fid {
                continue;
            }
            if !ancestors.contains(fid) {
                let _ = self
                    .run_with_ack(
                        ack,
                        "blur_other_frame",
                        self.session.eval_isolated_in_frame(blur_any_js, Some(fid)),
                    )
                    .await;
            } else {
                let _ = self
                    .run_with_ack(
                        ack,
                        "blur_ancestor_active",
                        self.session
                            .eval_isolated_in_frame(blur_non_iframe_active_js, Some(fid)),
                    )
                    .await;
            }
        }

        if let Some(tfid) = effective_target_fid {
            if target_active_is_iframe {
                let _ = self
                    .run_with_ack(
                        ack,
                        "blur_child_iframe",
                        self.session
                            .eval_isolated_in_frame(blur_iframe_only_js, Some(tfid)),
                    )
                    .await;
            } else if other_frame_has_focus {
                let _ = self
                    .run_with_ack(
                        ack,
                        "blur_shadow_iframe_host",
                        self.session
                            .eval_isolated_in_frame(blur_non_iframe_active_js, Some(tfid)),
                    )
                    .await;
            }
            if target_active_is_iframe || other_frame_has_focus || !target_has_focus {
                let _ = self
                    .run_with_ack(
                        ack,
                        "focus_target_window",
                        self.session
                            .eval_isolated_in_frame(focus_target_window_js, Some(tfid)),
                    )
                    .await;
            }
        }
    }

    pub async fn click(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
        opts: &ClickOptions,
    ) -> Result<()> {
        let mut state = self.gate.lock().await;
        let ack = self.ack_duration(opts.ack_timeout_ms);
        let resolved = self.resolve_target(target, refs)?;
        let (bbox, vp_w, vp_h, (tx, ty)) = self.resolve_target_in_viewport(&resolved, ack).await?;

        self.move_cursor_to(
            &mut state,
            (tx, ty),
            Some((bbox.width, bbox.height)),
            (vp_w, vp_h),
            &opts.humanize,
            ack,
        )
        .await?;

        self.prepare_firefox_click_focus(resolved.frame_id.as_deref(), tx, ty, ack)
            .await;

        if opts.modifiers != 0 {
            self.send_mod_keys(opts.modifiers, true, ack).await?;
        }
        let click_res = self
            .run_with_ack(
                ack,
                "dispatch_mouse(Click)",
                self.session.dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Click,
                    x: tx,
                    y: ty,
                    button: opts.button,
                    click_count: opts.click_count.max(1),
                    modifiers: opts.modifiers,
                }),
            )
            .await;
        if opts.modifiers != 0 {
            let _ = self.send_mod_keys(opts.modifiers, false, ack).await;
        }
        click_res
    }

    pub async fn hover(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
        opts: &InputOptions,
    ) -> Result<()> {
        let mut state = self.gate.lock().await;
        let ack = self.ack_duration(opts.ack_timeout_ms);
        let resolved = self.resolve_target(target, refs)?;
        let (bbox, vp_w, vp_h, (tx, ty)) = self.resolve_target_in_viewport(&resolved, ack).await?;
        self.move_cursor_to(
            &mut state,
            (tx, ty),
            Some((bbox.width, bbox.height)),
            (vp_w, vp_h),
            &opts.humanize,
            ack,
        )
        .await
    }

    pub async fn fill(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
        value: &str,
        opts: &FillOptions,
    ) -> Result<()> {
        let mut state = self.gate.lock().await;
        let ack = self.ack_duration(opts.ack_timeout_ms);
        let resolved = self.resolve_target(target, refs)?;

        if opts.humanize.enabled {
            let (bbox, vp_w, vp_h, (tx, ty)) =
                self.resolve_target_in_viewport(&resolved, ack).await?;
            self.move_cursor_to(
                &mut state,
                (tx, ty),
                Some((bbox.width, bbox.height)),
                (vp_w, vp_h),
                &opts.humanize,
                ack,
            )
            .await?;
            self.prepare_firefox_click_focus(resolved.frame_id.as_deref(), tx, ty, ack)
                .await;
            self.run_with_ack(
                ack,
                "dispatch_mouse(Click)",
                self.session.dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Click,
                    x: tx,
                    y: ty,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: 0,
                }),
            )
            .await?;
        } else {
            self.prepare_firefox_click_focus(resolved.frame_id.as_deref(), f64::NAN, f64::NAN, ack)
                .await;
            let _ = self
                .run_with_ack(
                    ack,
                    "scroll_into_view",
                    self.session.scroll_into_view(
                        resolved.backend_node_id,
                        resolved.frame_id.as_deref(),
                        resolved.css_fallback.as_deref(),
                    ),
                )
                .await;
        }

        self.run_with_ack(
            ack,
            "clear_node_value",
            self.session.clear_node_value(
                resolved.backend_node_id,
                resolved.frame_id.as_deref(),
                resolved.css_fallback.as_deref(),
            ),
        )
        .await?;

        self.focus_resolved(&resolved, ack).await?;

        if !value.is_empty() {
            if opts.humanize.enabled {
                self.dispatch_typed_chars(value, Some(&opts.humanize), ack)
                    .await?;
            } else {
                self.run_with_ack(ack, "insert_text", self.session.insert_text(value))
                    .await?;
            }
        }
        self.dispatch_change_event(&resolved, ack).await;
        Ok(())
    }

    pub async fn type_text(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
        text: &str,
        opts: &TypeOptions,
    ) -> Result<()> {
        let mut state = self.gate.lock().await;
        let ack = self.ack_duration(opts.ack_timeout_ms);
        let resolved = self.resolve_target(target, refs)?;

        if opts.humanize.enabled {
            let (bbox, vp_w, vp_h, (tx, ty)) =
                self.resolve_target_in_viewport(&resolved, ack).await?;
            self.move_cursor_to(
                &mut state,
                (tx, ty),
                Some((bbox.width, bbox.height)),
                (vp_w, vp_h),
                &opts.humanize,
                ack,
            )
            .await?;
            self.prepare_firefox_click_focus(resolved.frame_id.as_deref(), tx, ty, ack)
                .await;
            self.run_with_ack(
                ack,
                "dispatch_mouse(Click)",
                self.session.dispatch_mouse(MouseEvent {
                    event_type: MouseEventType::Click,
                    x: tx,
                    y: ty,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: 0,
                }),
            )
            .await?;
        } else {
            self.prepare_firefox_click_focus(resolved.frame_id.as_deref(), f64::NAN, f64::NAN, ack)
                .await;
            self.focus_resolved(&resolved, ack).await?;
        }

        if !text.is_empty() {
            self.dispatch_typed_chars(text, opts.humanize.enabled.then_some(&opts.humanize), ack)
                .await?;
        }
        Ok(())
    }

    pub async fn select_option(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
        by: SelectBy,
        ack_timeout_ms: u64,
    ) -> Result<String> {
        let _state = self.gate.lock().await;
        let ack = self.ack_duration(ack_timeout_ms);
        let resolved = self.resolve_target(target, refs)?;
        self.run_with_ack(
            ack,
            "select_option",
            self.session.select_option(
                resolved.backend_node_id,
                resolved.frame_id.as_deref(),
                resolved.css_fallback.as_deref(),
                by,
            ),
        )
        .await
    }

    pub async fn set_input_files(
        &self,
        target: &InputTarget,
        refs: Option<&RefMap>,
        files: Vec<PathBuf>,
        ack_timeout_ms: u64,
    ) -> Result<()> {
        let _state = self.gate.lock().await;
        let ack = self.ack_duration(ack_timeout_ms);
        let resolved = self.resolve_target(target, refs)?;
        let mut canonical = Vec::with_capacity(files.len());
        for p in files {
            let canon = std::fs::canonicalize(&p).map_err(|e| {
                MimikyuError::InvalidConfig(format!(
                    "upload file '{}' not accessible: {e}",
                    p.display()
                ))
            })?;
            canonical.push(canon);
        }
        self.run_with_ack(
            ack,
            "set_file_input_files",
            self.session.set_file_input_files(
                resolved.backend_node_id,
                resolved.frame_id.as_deref(),
                resolved.css_fallback.as_deref(),
                canonical,
            ),
        )
        .await
    }

    async fn read_scroll_offset(
        &self,
        ax: f64,
        ay: f64,
        frame_id: Option<&str>,
        ack: Duration,
    ) -> Option<(f64, f64)> {
        let script = if frame_id.is_some() {
            "(() => [window.scrollX || 0, window.scrollY || 0])()".to_string()
        } else {
            format!(
                r#"(() => {{
                    let sx = window.scrollX || 0;
                    let sy = window.scrollY || 0;
                    try {{
                        const se = document.scrollingElement || document.documentElement;
                        for (const el of document.elementsFromPoint({ax}, {ay})) {{
                            if (el === se || el === document.documentElement) continue;
                            sx += el.scrollLeft || 0;
                            sy += el.scrollTop || 0;
                        }}
                    }} catch {{}}
                    return [sx, sy];
                }})()"#,
            )
        };
        let val = self
            .run_with_ack(
                ack,
                "read_scroll_offset",
                self.session.eval_isolated_in_frame(&script, frame_id),
            )
            .await
            .ok()?;
        let arr = val.as_array()?;
        let x = arr.first()?.as_f64().filter(|v| v.is_finite())?;
        let y = arr.get(1)?.as_f64().filter(|v| v.is_finite())?;
        Some((x, y))
    }

    async fn wait_for_scroll_settle(
        &self,
        before: (f64, f64),
        ax: f64,
        ay: f64,
        frame_id: Option<&str>,
        ack: Duration,
    ) -> bool {
        let cap_ms = (ack.as_millis() as u64).clamp(50, 700);
        let read_body = if frame_id.is_some() {
            "return [window.scrollX || 0, window.scrollY || 0];".to_string()
        } else {
            format!(
                r#"let sx = window.scrollX || 0;
                let sy = window.scrollY || 0;
                try {{
                    const se = document.scrollingElement || document.documentElement;
                    for (const el of document.elementsFromPoint({ax}, {ay})) {{
                        if (el === se || el === document.documentElement) continue;
                        sx += el.scrollLeft || 0;
                        sy += el.scrollTop || 0;
                    }}
                }} catch {{}}
                return [sx, sy];"#
            )
        };
        let script = format!(
            r#"(() => new Promise((resolve) => {{
                const bx = {bx};
                const by = {by};
                const capMs = {cap_ms};
                const read = () => {{ {read_body} }};
                let done = false;
                let prev = read();
                let moved = Math.abs(prev[0] - bx) > 0.5 || Math.abs(prev[1] - by) > 0.5;
                let stable = 0;
                let frames = 0;
                const start = performance.now();
                let timer = 0;
                const finish = () => {{
                    if (done) return;
                    done = true;
                    clearTimeout(timer);
                    resolve(moved);
                }};
                timer = setTimeout(finish, capMs);
                const step = () => {{
                    if (done) return;
                    frames += 1;
                    const cur = read();
                    const elapsed = performance.now() - start;
                    if (Math.abs(cur[0] - bx) > 0.5 || Math.abs(cur[1] - by) > 0.5) {{
                        moved = true;
                    }}
                    if (Math.abs(cur[0] - prev[0]) <= 0.5 && Math.abs(cur[1] - prev[1]) <= 0.5) {{
                        stable += 1;
                    }} else {{
                        stable = 0;
                    }}
                    prev = cur;
                    if (moved && stable >= 2) {{
                        finish();
                        return;
                    }}
                    if (!moved && elapsed >= Math.min(80, capMs) && frames >= 3) {{
                        finish();
                        return;
                    }}
                    if (elapsed >= capMs) {{
                        finish();
                        return;
                    }}
                    requestAnimationFrame(step);
                }};
                requestAnimationFrame(step);
            }}))()"#,
            bx = before.0,
            by = before.1,
        );
        self.run_with_ack(
            ack,
            "wait_for_scroll_settle",
            self.session.eval_isolated_in_frame(&script, frame_id),
        )
        .await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    }

    pub async fn scroll(
        &self,
        dx: f64,
        dy: f64,
        target: Option<&InputTarget>,
        refs: Option<&RefMap>,
        opts: &ScrollOptions,
    ) -> Result<()> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(MimikyuError::InvalidConfig(
                "scroll deltas must be finite numbers".to_string(),
            ));
        }
        let dx = dx.clamp(-MAX_WHEEL_DELTA_PX, MAX_WHEEL_DELTA_PX);
        let dy = dy.clamp(-MAX_WHEEL_DELTA_PX, MAX_WHEEL_DELTA_PX);
        let mut state = self.gate.lock().await;
        let ack = self.ack_duration(opts.ack_timeout_ms);

        let (ax, ay, target_frame_id) = if let Some(tgt) = target {
            let resolved = self.resolve_target(tgt, refs)?;
            let (_, _, _, pt) = self.resolve_target_in_viewport(&resolved, ack).await?;
            (pt.0, pt.1, resolved.frame_id)
        } else {
            let (vp_w, vp_h) = self
                .run_with_ack(ack, "viewport_size", self.session.viewport_size())
                .await?;
            let pt = clamp_point_to_viewport(vp_w * 0.5, vp_h * 0.5, vp_w, vp_h);
            (pt.0, pt.1, None)
        };
        state.cursor = Some((ax, ay));

        let wants_scroll = dx.abs() > f64::EPSILON || dy.abs() > f64::EPSILON;
        let before_main = if wants_scroll {
            self.read_scroll_offset(ax, ay, None, ack).await
        } else {
            None
        };
        let before_frame = if wants_scroll && target_frame_id.is_some() {
            self.read_scroll_offset(ax, ay, target_frame_id.as_deref(), ack)
                .await
        } else {
            None
        };

        if opts.modifiers != 0 {
            self.send_mod_keys(opts.modifiers, true, ack).await?;
        }

        let res = if opts.humanize.enabled {
            let mut rng = make_rng(opts.humanize.seed);
            let notches = split_wheel_notches(dx, dy, &mut rng);
            let mut last_res = Ok(());
            for (nx, ny, delay_ms) in notches {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                if let Err(e) = self
                    .run_with_ack(
                        ack,
                        "dispatch_wheel",
                        self.session.dispatch_wheel(WheelEvent {
                            x: ax,
                            y: ay,
                            delta_x: nx,
                            delta_y: ny,
                            modifiers: opts.modifiers,
                        }),
                    )
                    .await
                {
                    last_res = Err(e);
                    break;
                }
            }
            last_res
        } else {
            self.run_with_ack(
                ack,
                "dispatch_wheel",
                self.session.dispatch_wheel(WheelEvent {
                    x: ax,
                    y: ay,
                    delta_x: dx,
                    delta_y: dy,
                    modifiers: opts.modifiers,
                }),
            )
            .await
        };

        if opts.modifiers != 0 {
            let _ = self.send_mod_keys(opts.modifiers, false, ack).await;
        }
        if res.is_ok() && wants_scroll {
            if let (Some(fid), Some(bf)) = (target_frame_id.as_deref(), before_frame) {
                self.wait_for_scroll_settle(bf, ax, ay, Some(fid), ack)
                    .await;
            }
            if let Some(bm) = before_main {
                self.wait_for_scroll_settle(bm, ax, ay, None, ack).await;
            }
        }
        res
    }

    pub async fn press(&self, key_spec: &str, opts: &InputOptions) -> Result<()> {
        let _state = self.gate.lock().await;
        let ack = self.ack_duration(opts.ack_timeout_ms);
        let chord = KeyChord::parse(key_spec)?;

        for mk in &chord.mod_keys {
            self.run_with_ack(
                ack,
                "dispatch_key(mod down)",
                self.session.dispatch_key(KeyEvent {
                    event_type: KeyEventType::Down,
                    key: mk.clone(),
                    code: None,
                    text: None,
                    windows_virtual_key_code: None,
                    modifiers: 0,
                }),
            )
            .await?;
        }

        let base_res = self
            .run_with_ack(
                ack,
                "dispatch_key(chord base)",
                self.session.dispatch_key(KeyEvent {
                    event_type: KeyEventType::Char,
                    key: chord.key.clone(),
                    code: chord.code.clone(),
                    text: None,
                    windows_virtual_key_code: chord.windows_virtual_key_code,
                    modifiers: chord.modifiers,
                }),
            )
            .await;

        for mk in chord.mod_keys.iter().rev() {
            let _ = self
                .run_with_ack(
                    ack,
                    "dispatch_key(mod up)",
                    self.session.dispatch_key(KeyEvent {
                        event_type: KeyEventType::Up,
                        key: mk.clone(),
                        code: None,
                        text: None,
                        windows_virtual_key_code: None,
                        modifiers: 0,
                    }),
                )
                .await;
        }

        base_res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chord_parses_modifiers_aliases_and_special_keys() {
        let c1 = KeyChord::parse("Control+a").unwrap();
        assert_eq!(c1.modifiers, MOD_CONTROL);
        assert_eq!(c1.mod_keys, vec!["Control"]);
        assert_eq!(c1.key, "a");
        assert_eq!(c1.code.as_deref(), Some("KeyA"));
        assert_eq!(c1.windows_virtual_key_code, Some(65));

        let c2 = KeyChord::parse("Ctrl+Shift+Z").unwrap();
        assert_eq!(c2.modifiers, MOD_CONTROL | MOD_SHIFT);
        assert_eq!(c2.mod_keys, vec!["Control", "Shift"]);
        assert_eq!(c2.key, "Z");

        let c3 = KeyChord::parse("Cmd+k").unwrap();
        assert_eq!(c3.modifiers, MOD_META);
        assert_eq!(c3.mod_keys, vec!["Meta"]);

        let c4 = KeyChord::parse("F12").unwrap();
        assert_eq!(c4.modifiers, 0);
        assert_eq!(c4.key, "F12");

        let c5 = KeyChord::parse("+").unwrap();
        assert_eq!(c5.key, "+");
        assert!(KeyChord::parse("").is_err());
        assert!(KeyChord::parse("Bogus+a").is_err());
    }

    #[test]
    fn target_click_point_clamps_off_edge_boxes_away_from_negative_one() {
        let top_edge = BoundingBox {
            x: 200.0,
            y: -10.0,
            width: 80.0,
            height: 8.0,
        };
        assert_eq!(target_click_point(top_edge, 1280.0, 800.0), (240.0, 0.0));

        let bottom_right = BoundingBox {
            x: 1272.0,
            y: 792.0,
            width: 20.0,
            height: 20.0,
        };
        assert_eq!(
            target_click_point(bottom_right, 1280.0, 800.0),
            (1276.0, 796.0)
        );

        let way_beyond = BoundingBox {
            x: 1285.0,
            y: 810.0,
            width: 20.0,
            height: 20.0,
        };
        assert_eq!(
            target_click_point(way_beyond, 1280.0, 800.0),
            (1279.0, 799.0)
        );
    }

    #[test]
    fn resolve_safe_click_point_avoids_and_detects_scrollbar_gutter_occlusion() {
        // Partially overlapping right-edge element (770..810 on 800x600 with 16px right scrollbar gutter):
        // aims at midpoint of [770, 784] = 777.0, safely left of scrollbar!
        let partial = BoundingBox {
            x: 770.0,
            y: 200.0,
            width: 40.0,
            height: 30.0,
        };
        assert_eq!(
            resolve_safe_click_point(partial, 800.0, 600.0, 16.0, 0.0).unwrap(),
            (777.0, 215.0)
        );

        // Completely occluded right-edge element (790..830 on 800x600 with 16px right scrollbar gutter):
        // fails explicitly instead of silently clicking the scrollbar.
        let occluded = BoundingBox {
            x: 790.0,
            y: 200.0,
            width: 40.0,
            height: 30.0,
        };
        let err = resolve_safe_click_point(occluded, 800.0, 600.0, 16.0, 0.0)
            .expect_err("fully scrollbar-occluded target must error");
        assert!(matches!(err, MimikyuError::ElementNotFound(_)));

        // Without scrollbar gutter (short page or Chromium), 790..830 clicks cleanly at 795.0.
        assert_eq!(
            resolve_safe_click_point(occluded, 800.0, 600.0, 0.0, 0.0).unwrap(),
            (795.0, 215.0)
        );

        // Classic layout scrollbar (`clientWidth = 785 < innerWidth = 800`, 15px scrollbar in [785, 800)):
        // partially overlapping [770, 810] aims at midpoint of [770, 785] = 777.5, while [790, 830]
        // inside [785, 800) fails explicitly instead of clamping to x=784 and clicking empty body!
        assert_eq!(
            resolve_safe_click_point(partial, 800.0, 600.0, 15.0, 0.0).unwrap(),
            (777.5, 215.0)
        );
        let classic_err = resolve_safe_click_point(occluded, 800.0, 600.0, 15.0, 0.0)
            .expect_err("classic scrollbar-occluded target must error");
        assert!(matches!(classic_err, MimikyuError::ElementNotFound(_)));
    }
}
