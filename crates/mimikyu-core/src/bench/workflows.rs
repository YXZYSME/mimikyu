use std::time::{Duration, Instant};

use serde_json::Value;

use crate::bench::BenchTaskResult;
use crate::driver::EngineKind;
use crate::error::{MimikyuError, Result};
use crate::extract::TableExtractResult;
use crate::harness::HarnessServer;
use crate::input::BatchAction;
use crate::session::{ExtractMode, SessionCreateOpts, SessionManager, WaitRequest};
use crate::snapshot::{SnapshotOptions, SnapshotOutput};

pub fn find_ref(snap: &SnapshotOutput, roles: &[&str], name_query: &str) -> Result<String> {
    let q_lower = name_query.trim().to_ascii_lowercase();
    for (ref_id, entry) in snap.ref_map.iter() {
        let role_ok = roles.iter().any(|r| entry.role.eq_ignore_ascii_case(r));
        if !role_ok {
            continue;
        }
        let name = entry.name.as_deref().unwrap_or("").trim();
        if name.eq_ignore_ascii_case(name_query.trim()) {
            return Ok(ref_id.clone());
        }
    }
    for (ref_id, entry) in snap.ref_map.iter() {
        let role_ok = roles.iter().any(|r| entry.role.eq_ignore_ascii_case(r));
        if !role_ok {
            continue;
        }
        let name_lower = entry.name.as_deref().unwrap_or("").trim().to_ascii_lowercase();
        if name_lower.contains(&q_lower) {
            return Ok(ref_id.clone());
        }
    }
    Err(MimikyuError::ElementNotFound(format!(
        "no snapshot ref found for roles {roles:?} matching '{name_query}' in:\n{}",
        snap.text
    )))
}

async fn run_task_1_wizard(
    mgr: &SessionManager,
    sess_id: &str,
    harness: &HarnessServer,
) -> Result<(usize, String)> {
    let mut steps = 0;
    mgr.open(sess_id, &harness.url("/wizard")).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let snap1 = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;
    let name_ref = find_ref(&snap1, &["textbox"], "Full Name")?;
    let role_ref = find_ref(&snap1, &["combobox", "listbox"], "Workspace Role")?;
    let next_ref = find_ref(&snap1, &["button"], "Continue to Step 2")?;

    mgr.fill(sess_id, &name_ref, "Alice Hopper", Some(false))
        .await?;
    steps += 1;
    mgr.select(sess_id, &role_ref, "editor").await?;
    steps += 1;
    mgr.click(sess_id, &next_ref, Some(false)).await?;
    steps += 1;

    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            url: Some("/wizard/step2".to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let snap2 = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;
    let region_ref = find_ref(&snap2, &["textbox"], "Region")?;
    let agree_ref = find_ref(&snap2, &["checkbox"], "Confirm Settings")?;
    let done_ref = find_ref(&snap2, &["button"], "Complete Setup")?;

    mgr.fill(sess_id, &region_ref, "eu-central", Some(false))
        .await?;
    steps += 1;
    mgr.click(sess_id, &agree_ref, Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &done_ref, Some(false)).await?;
    steps += 1;

    let expected = "WIZARD_DONE:Alice Hopper:editor:eu-central:yes";
    mgr.wait(
        sess_id,
        WaitRequest {
            text: Some(expected.to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let extracted = mgr.extract(sess_id, ExtractMode::Text).await?;
    steps += 1;
    let text = extracted["text"].as_str().unwrap_or("");
    if !text.contains(expected) {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("wizard text missing '{expected}', got: {text}"),
        });
    }
    Ok((steps, expected.to_string()))
}

async fn run_task_2_form_echo(
    mgr: &SessionManager,
    sess_id: &str,
    harness: &HarnessServer,
) -> Result<(usize, String)> {
    let mut steps = 0;
    mgr.open(sess_id, &harness.url("/form")).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let snap = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;

    let name_ref = find_ref(&snap, &["textbox"], "Applicant Name")?;
    let notes_ref = find_ref(&snap, &["textbox"], "Notes")?;
    let country_ref = find_ref(&snap, &["combobox", "listbox"], "Country")?;
    let tier_ref = find_ref(&snap, &["radio"], "Pro")?;
    let terms_ref = find_ref(&snap, &["checkbox"], "Accept Terms")?;
    let submit_ref = find_ref(&snap, &["button"], "Submit Application")?;

    mgr.fill(sess_id, &name_ref, "Grace Lovelace", Some(false))
        .await?;
    steps += 1;
    mgr.fill(sess_id, &notes_ref, "Form workflow check", Some(false))
        .await?;
    steps += 1;
    mgr.select(sess_id, &country_ref, "de").await?;
    steps += 1;
    mgr.click(sess_id, &tier_ref, Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &terms_ref, Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &submit_ref, Some(false)).await?;
    steps += 1;

    let expected = "SUBMITTED:Grace Lovelace:de:pro:yes";
    mgr.wait(
        sess_id,
        WaitRequest {
            text: Some(expected.to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let extracted = mgr.extract(sess_id, ExtractMode::Text).await?;
    steps += 1;
    let text = extracted["text"].as_str().unwrap_or("");
    if !text.contains(expected) {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("form echo missing '{expected}', got: {text}"),
        });
    }
    Ok((steps, expected.to_string()))
}

async fn run_task_3_catalog_filter_tables(
    mgr: &SessionManager,
    sess_id: &str,
    harness: &HarnessServer,
) -> Result<(usize, String)> {
    let mut steps = 0;
    mgr.open(sess_id, &harness.url("/catalog")).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let base_snap = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: false,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;
    let search_ref = find_ref(&base_snap, &["searchbox", "textbox"], "Filter Items")?;

    mgr.fill(sess_id, &search_ref, "Alpha", Some(false))
        .await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            text: Some("Showing 1 of 5 items".to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let diff_snap = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: false,
                diff: true,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;
    let diff = diff_snap.diff.as_ref().ok_or_else(|| MimikyuError::Protocol {
        code: -1,
        message: "expected snapshot diff for catalog filter".to_string(),
    })?;
    if !diff.has_previous || (diff.added + diff.removed + diff.changed) == 0 {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("expected non-empty snapshot diff after filter, got {diff:?}"),
        });
    }

    let tables_val = mgr.extract(sess_id, ExtractMode::Tables).await?;
    steps += 1;
    let tables: TableExtractResult = serde_json::from_value(tables_val)?;
    let first_table = tables.tables.first().ok_or_else(|| MimikyuError::Protocol {
        code: -1,
        message: "catalog extract(tables) returned zero tables".to_string(),
    })?;
    if first_table.row_count != 1
        || !first_table
            .rows
            .first()
            .is_some_and(|r| r.iter().any(|c| c.contains("Alpha Compute")))
    {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("unexpected filtered table rows: {:?}", first_table.rows),
        });
    }

    Ok((steps, format!("rows={}", first_table.row_count)))
}

async fn run_task_4_batch_hydration(
    mgr: &SessionManager,
    sess_id: &str,
    harness: &HarnessServer,
) -> Result<(usize, String)> {
    let mut steps = 0;
    mgr.open(sess_id, &harness.url("/dynamic")).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            text: Some("DYNAMIC_HYDRATED".to_string()),
            selector: Some("#dyn-action".to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let snap = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;
    let note_ref = find_ref(&snap, &["textbox"], "Hydration Note")?;
    let confirm_ref = find_ref(&snap, &["button"], "Confirm Hydrated")?;

    let actions = vec![
        BatchAction::Fill {
            target: note_ref,
            value: "batch-ok".to_string(),
            humanize: Some(false),
        },
        BatchAction::Click {
            target: confirm_ref,
            humanize: Some(false),
            button: None,
            click_count: None,
        },
        BatchAction::WaitText {
            text: "CONFIRMED:batch-ok".to_string(),
            timeout_ms: Some(5000),
        },
        BatchAction::Extract {
            mode: "text".to_string(),
        },
    ];
    let report = mgr.batch(sess_id, actions).await?;
    steps += report.completed_steps;
    if !report.ok {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("batch failed: {:?}", report.error),
        });
    }
    let last_text = report
        .results
        .last()
        .and_then(|r| r.output.as_ref())
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !last_text.contains("CONFIRMED:batch-ok") {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("batch extract missing CONFIRMED:batch-ok: {last_text}"),
        });
    }

    Ok((steps, "CONFIRMED:batch-ok".to_string()))
}

async fn run_task_5_shadow_iframe(
    mgr: &SessionManager,
    sess_id: &str,
    harness: &HarnessServer,
) -> Result<(usize, String)> {
    let mut steps = 0;

    // Part A: Open Shadow DOM interaction
    mgr.open(sess_id, &harness.url("/shadow")).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    let sh_snap = mgr
        .snapshot(
            sess_id,
            SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .await?;
    steps += 1;
    let sh_input = find_ref(&sh_snap, &["textbox"], "Open Shadow Input")?;
    let sh_btn = find_ref(&sh_snap, &["button"], "Open Shadow Action")?;
    mgr.fill(sess_id, &sh_input, "sh-val", Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &sh_btn, Some(false)).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            text: Some("open-clicked:sh-val".to_string()),
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    // Part B: Same-origin, nested same-origin, and cross-origin iframe @eN interactions
    mgr.open(sess_id, &harness.url("/frames")).await?;
    steps += 1;
    mgr.wait(
        sess_id,
        WaitRequest {
            idle: true,
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
    .await?;
    steps += 1;

    // Poll briefly until all 3 iframe children are reflected in the snapshot
    let mut frame_snap = None;
    for _ in 0..20 {
        let s = mgr
            .snapshot(
                sess_id,
                SnapshotOptions {
                    interactive_only: true,
                    ..Default::default()
                },
            )
            .await?;
        if find_ref(&s, &["textbox"], "Same Origin Input").is_ok()
            && find_ref(&s, &["textbox"], "Nested Inner Input").is_ok()
            && find_ref(&s, &["textbox"], "Cross Origin Input").is_ok()
        {
            frame_snap = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let frame_snap = frame_snap.ok_or_else(|| MimikyuError::ElementNotFound(
        "nested or cross-origin iframe inputs did not appear in snapshot".to_string(),
    ))?;
    steps += 1;

    let so_in = find_ref(&frame_snap, &["textbox"], "Same Origin Input")?;
    let so_btn = find_ref(&frame_snap, &["button"], "Same Origin Action")?;
    let inner_in = find_ref(&frame_snap, &["textbox"], "Nested Inner Input")?;
    let inner_btn = find_ref(&frame_snap, &["button"], "Nested Inner Action")?;
    let xo_in = find_ref(&frame_snap, &["textbox"], "Cross Origin Input")?;
    let xo_btn = find_ref(&frame_snap, &["button"], "Cross Origin Action")?;

    mgr.fill(sess_id, &so_in, "so-1", Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &so_btn, Some(false)).await?;
    steps += 1;

    mgr.fill(sess_id, &inner_in, "in-2", Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &inner_btn, Some(false)).await?;
    steps += 1;

    mgr.fill(sess_id, &xo_in, "xo-3", Some(false)).await?;
    steps += 1;
    mgr.click(sess_id, &xo_btn, Some(false)).await?;
    steps += 1;

    let mut verified = false;
    let mut last_text = String::new();
    for _ in 0..20 {
        let verify_snap = mgr
            .snapshot(
                sess_id,
                SnapshotOptions {
                    interactive_only: false,
                    ..Default::default()
                },
            )
            .await?;
        last_text = verify_snap.text;
        if last_text.contains("so-clicked:so-1")
            && last_text.contains("inner-clicked:in-2")
            && last_text.contains("xo-clicked:xo-3")
        {
            verified = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    steps += 1;
    if !verified {
        return Err(MimikyuError::Protocol {
            code: -1,
            message: format!("iframe status markers missing from snapshot:\n{last_text}"),
        });
    }

    Ok((steps, "shadow+same+nested+xo-ok".to_string()))
}

pub async fn run_all_workflows(
    mgr: &SessionManager,
    harness: &HarnessServer,
    engine: EngineKind,
    headless: bool,
) -> Result<Vec<BenchTaskResult>> {
    let specs: [(&str, &str); 5] = [
        ("task_1_wizard", "Multi-Step Wizard Onboarding"),
        ("task_2_form_echo", "Form Input, Select & Submit Echo"),
        (
            "task_3_catalog_filter_tables",
            "Catalog Filter, Diff & Table Extraction",
        ),
        (
            "task_4_batch_hydration",
            "Dynamic DOM Hydration & Atomic Batch",
        ),
        (
            "task_5_shadow_iframe",
            "Shadow DOM & Nested Cross-Origin Iframes",
        ),
    ];

    let mut results = Vec::with_capacity(5);
    for (idx, (id, name)) in specs.iter().enumerate() {
        let sess_id = format!("bench-wf-{}", idx + 1);
        let started = Instant::now();
        let create_res = mgr
            .create_session(SessionCreateOpts {
                session_id: Some(sess_id.clone()),
                engine: Some(engine),
                virtual_ctx: true,
                headless,
                main_world_eval: false,
                ..Default::default()
            })
            .await;

        let task_res = match create_res {
            Ok(_) => {
                let outcome = match idx {
                    0 => run_task_1_wizard(mgr, &sess_id, harness).await,
                    1 => run_task_2_form_echo(mgr, &sess_id, harness).await,
                    2 => run_task_3_catalog_filter_tables(mgr, &sess_id, harness).await,
                    3 => run_task_4_batch_hydration(mgr, &sess_id, harness).await,
                    4 => run_task_5_shadow_iframe(mgr, &sess_id, harness).await,
                    _ => unreachable!(),
                };
                let _ = mgr.close_session(&sess_id).await;
                outcome
            }
            Err(e) => Err(e),
        };

        let latency_ms = (started.elapsed().as_secs_f64() * 1000.0).max(0.001);
        match task_res {
            Ok((steps, detail)) => results.push(BenchTaskResult {
                id: (*id).to_string(),
                name: (*name).to_string(),
                ok: true,
                passed: true,
                steps,
                latency_ms,
                detail,
            }),
            Err(err) => results.push(BenchTaskResult {
                id: (*id).to_string(),
                name: (*name).to_string(),
                ok: false,
                passed: false,
                steps: 0,
                latency_ms,
                detail: err.to_string(),
            }),
        }
    }

    Ok(results)
}
