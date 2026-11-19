use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver::{DriverSession, MouseButton, SelectBy};
use crate::error::{MimikyuError, Result};
use crate::extract::{extract_links, extract_markdown, extract_tables, extract_text, ExtractOptions};
use crate::input::chokepoint::{
    ChokepointInput, ClickOptions, FillOptions, InputOptions, InputTarget, ScrollOptions,
    TypeOptions,
};
use crate::input::humanize::HumanizeConfig;
use crate::snapshot::{SnapshotEngine, SnapshotOptions, SnapshotOutput, SnapshotState};
use crate::wait::{WaitEngine, WaitIdleOptions, WaitOptions, WaitSelectorState};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BatchAction {
    Open {
        url: String,
    },
    Click {
        target: String,
        #[serde(default)]
        humanize: Option<bool>,
        #[serde(default)]
        button: Option<MouseButton>,
        #[serde(default)]
        click_count: Option<u32>,
    },
    Fill {
        target: String,
        value: String,
        #[serde(default)]
        humanize: Option<bool>,
    },
    Type {
        target: String,
        text: String,
        #[serde(default)]
        humanize: Option<bool>,
    },
    Select {
        target: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        values: Vec<String>,
    },
    Upload {
        target: String,
        files: Vec<PathBuf>,
    },
    Scroll {
        dx: f64,
        dy: f64,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        humanize: Option<bool>,
    },
    Press {
        key: String,
    },
    WaitIdle {
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        network_quiet_ms: Option<u64>,
        #[serde(default, alias = "stable_target")]
        target: Option<String>,
    },
    WaitSelector {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        state: Option<WaitSelectorState>,
    },
    WaitText {
        text: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    WaitUrl {
        substr: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Snapshot {
        #[serde(default)]
        max_depth: Option<usize>,
        #[serde(default)]
        interactive_only: bool,
        #[serde(default)]
        diff: bool,
    },
    Extract {
        mode: String,
    },
}

impl BatchAction {
    pub fn name(&self) -> &'static str {
        match self {
            BatchAction::Open { .. } => "open",
            BatchAction::Click { .. } => "click",
            BatchAction::Fill { .. } => "fill",
            BatchAction::Type { .. } => "type",
            BatchAction::Select { .. } => "select",
            BatchAction::Upload { .. } => "upload",
            BatchAction::Scroll { .. } => "scroll",
            BatchAction::Press { .. } => "press",
            BatchAction::WaitIdle { .. } => "wait_idle",
            BatchAction::WaitSelector { .. } => "wait_selector",
            BatchAction::WaitText { .. } => "wait_text",
            BatchAction::WaitUrl { .. } => "wait_url",
            BatchAction::Snapshot { .. } => "snapshot",
            BatchAction::Extract { .. } => "extract",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchStepResult {
    pub step_index: usize,
    pub action: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchError {
    pub step_index: usize,
    pub action: String,
    pub message: String,
    pub recovery_snapshot: Option<SnapshotOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchReport {
    pub ok: bool,
    pub completed_steps: usize,
    pub total_steps: usize,
    pub results: Vec<BatchStepResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchError>,
}

pub struct ActionBatcher;

impl ActionBatcher {
    pub async fn execute(
        session: &dyn DriverSession,
        chokepoint: &ChokepointInput,
        snapshot_state: &mut SnapshotState,
        actions: &[BatchAction],
    ) -> Result<BatchReport> {
        let total_steps = actions.len();
        let mut results = Vec::with_capacity(total_steps);

        for (idx, action) in actions.iter().enumerate() {
            let started = Instant::now();
            let step_res = Self::run_step(session, chokepoint, snapshot_state, action).await;
            let duration_ms = started.elapsed().as_millis() as u64;

            match step_res {
                Ok(output) => {
                    results.push(BatchStepResult {
                        step_index: idx,
                        action: action.name().to_string(),
                        duration_ms,
                        output,
                    });
                }
                Err(err) => {
                    let recovery_snapshot = if !matches!(
                        err,
                        MimikyuError::Disposed | MimikyuError::Transport(_)
                    ) {
                        SnapshotEngine::capture(
                            session,
                            snapshot_state,
                            SnapshotOptions::default(),
                        )
                        .await
                        .ok()
                    } else {
                        None
                    };

                    return Ok(BatchReport {
                        ok: false,
                        completed_steps: idx,
                        total_steps,
                        results,
                        error: Some(BatchError {
                            step_index: idx,
                            action: action.name().to_string(),
                            message: err.to_string(),
                            recovery_snapshot,
                        }),
                    });
                }
            }
        }

        Ok(BatchReport {
            ok: true,
            completed_steps: total_steps,
            total_steps,
            results,
            error: None,
        })
    }

    async fn run_step(
        session: &dyn DriverSession,
        chokepoint: &ChokepointInput,
        snapshot_state: &mut SnapshotState,
        action: &BatchAction,
    ) -> Result<Option<Value>> {
        match action {
            BatchAction::Open { url } => {
                let valid_url = crate::session::validate_nav_url(url)?;
                snapshot_state.active_ref_map.clear();
                let nav = session.navigate(&valid_url).await?;
                Ok(Some(serde_json::to_value(nav)?))
            }
            BatchAction::Click {
                target,
                humanize,
                button,
                click_count,
            } => {
                let tgt = InputTarget::parse(target);
                let opts = ClickOptions {
                    button: button.unwrap_or_default(),
                    click_count: click_count.unwrap_or(1).max(1),
                    humanize: HumanizeConfig {
                        enabled: humanize.unwrap_or(false),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                chokepoint
                    .click(&tgt, Some(&snapshot_state.active_ref_map), &opts)
                    .await?;
                Ok(None)
            }
            BatchAction::Fill {
                target,
                value,
                humanize,
            } => {
                let tgt = InputTarget::parse(target);
                let opts = FillOptions {
                    humanize: HumanizeConfig {
                        enabled: humanize.unwrap_or(false),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                chokepoint
                    .fill(&tgt, Some(&snapshot_state.active_ref_map), value, &opts)
                    .await?;
                Ok(None)
            }
            BatchAction::Type {
                target,
                text,
                humanize,
            } => {
                let tgt = InputTarget::parse(target);
                let opts = TypeOptions {
                    humanize: HumanizeConfig {
                        enabled: humanize.unwrap_or(false),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                chokepoint
                    .type_text(&tgt, Some(&snapshot_state.active_ref_map), text, &opts)
                    .await?;
                Ok(None)
            }
            BatchAction::Select {
                target,
                value,
                values,
            } => {
                let tgt = InputTarget::parse(target);
                let chosen = value
                    .clone()
                    .or_else(|| values.first().cloned())
                    .ok_or_else(|| {
                        MimikyuError::InvalidConfig(
                            "batch select action requires 'value' or non-empty 'values'".to_string(),
                        )
                    })?;
                let selected = chokepoint
                    .select_option(
                        &tgt,
                        Some(&snapshot_state.active_ref_map),
                        SelectBy::Auto(chosen),
                        0,
                    )
                    .await?;
                Ok(Some(serde_json::json!({ "selected": selected })))
            }
            BatchAction::Upload { target, files } => {
                if files.is_empty() {
                    return Err(MimikyuError::InvalidConfig(
                        "batch upload action requires at least one file".to_string(),
                    ));
                }
                for path in files {
                    let meta = std::fs::metadata(path).map_err(|e| {
                        MimikyuError::InvalidConfig(format!(
                            "upload file '{}' is not accessible: {e}",
                            path.display()
                        ))
                    })?;
                    if !meta.is_file() {
                        return Err(MimikyuError::InvalidConfig(format!(
                            "upload path '{}' is not a regular file",
                            path.display()
                        )));
                    }
                }
                let tgt = InputTarget::parse(target);
                chokepoint
                    .set_input_files(&tgt, Some(&snapshot_state.active_ref_map), files.clone(), 0)
                    .await?;
                Ok(None)
            }
            BatchAction::Scroll {
                dx,
                dy,
                target,
                humanize,
            } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(MimikyuError::InvalidConfig(
                        "scroll deltas must be finite numbers".to_string(),
                    ));
                }
                let tgt = target.as_deref().map(InputTarget::parse);
                let opts = ScrollOptions {
                    humanize: HumanizeConfig {
                        enabled: humanize.unwrap_or(false),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                chokepoint
                    .scroll(
                        *dx,
                        *dy,
                        tgt.as_ref(),
                        Some(&snapshot_state.active_ref_map),
                        &opts,
                    )
                    .await?;
                Ok(None)
            }
            BatchAction::Press { key } => {
                chokepoint.press(key, &InputOptions::default()).await?;
                Ok(None)
            }
            BatchAction::WaitIdle {
                timeout_ms,
                network_quiet_ms,
                target,
            } => {
                let mut opts = WaitIdleOptions::default();
                if let Some(t) = timeout_ms {
                    opts.timeout_ms = *t;
                }
                if let Some(q) = network_quiet_ms {
                    opts.network_quiet_ms = *q;
                }
                opts.stable_target = target.clone();
                WaitEngine::wait_idle(session, Some(&snapshot_state.active_ref_map), opts).await?;
                Ok(None)
            }
            BatchAction::WaitSelector {
                selector,
                timeout_ms,
                state,
            } => {
                let mut opts = WaitOptions::default();
                if let Some(t) = timeout_ms {
                    opts.timeout_ms = *t;
                }
                WaitEngine::wait_selector(session, selector, state.unwrap_or_default(), opts)
                    .await?;
                Ok(None)
            }
            BatchAction::WaitText { text, timeout_ms } => {
                let mut opts = WaitOptions::default();
                if let Some(t) = timeout_ms {
                    opts.timeout_ms = *t;
                }
                WaitEngine::wait_text(session, text, opts).await?;
                Ok(None)
            }
            BatchAction::WaitUrl { substr, timeout_ms } => {
                let mut opts = WaitOptions::default();
                if let Some(t) = timeout_ms {
                    opts.timeout_ms = *t;
                }
                WaitEngine::wait_url(session, substr, opts).await?;
                Ok(None)
            }
            BatchAction::Snapshot {
                max_depth,
                interactive_only,
                diff,
            } => {
                let snap = SnapshotEngine::capture(
                    session,
                    snapshot_state,
                    SnapshotOptions {
                        max_depth: *max_depth,
                        interactive_only: *interactive_only,
                        diff: *diff,
                        include_bounds: false,
                    },
                )
                .await?;
                Ok(Some(serde_json::to_value(snap)?))
            }
            BatchAction::Extract { mode } => {
                let parsed_mode: crate::session::ExtractMode = mode.parse()?;
                let opts = ExtractOptions::default();
                let val = match parsed_mode {
                    crate::session::ExtractMode::Markdown => {
                        serde_json::to_value(extract_markdown(session, &opts).await?)?
                    }
                    crate::session::ExtractMode::Text => {
                        serde_json::to_value(extract_text(session, &opts).await?)?
                    }
                    crate::session::ExtractMode::Links => {
                        serde_json::to_value(extract_links(session, &opts).await?)?
                    }
                    crate::session::ExtractMode::Tables => {
                        serde_json::to_value(extract_tables(session, &opts).await?)?
                    }
                };
                Ok(Some(val))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_action_json_round_trip() {
        let json_str = r#"[
            { "action": "open", "url": "http://127.0.0.1/form" },
            { "action": "snapshot", "interactive_only": true },
            { "action": "fill", "target": "@e1", "value": "alice" },
            { "action": "select", "target": "@e2", "value": "de" },
            { "action": "click", "target": "@e3", "humanize": true },
            { "action": "wait_idle", "timeout_ms": 5000, "network_quiet_ms": 200 },
            { "action": "extract", "mode": "markdown" }
        ]"#;
        let parsed: Vec<BatchAction> = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed.len(), 7);
        assert_eq!(parsed[0].name(), "open");
        assert_eq!(parsed[6].name(), "extract");
        let re = serde_json::to_string(&parsed).unwrap();
        let reparsed: Vec<BatchAction> = serde_json::from_str(&re).unwrap();
        assert_eq!(reparsed.len(), 7);
    }
}
