use std::path::PathBuf;

use mimikyu_core::daemon::DaemonOp;
use mimikyu_core::driver::{ScreencastFormat, ScreencastOptions};
use mimikyu_core::error::MimikyuError;
use mimikyu_core::input::BatchReport;
use mimikyu_core::session::{ExtractMode, LogKind, WaitRequest};
use mimikyu_core::snapshot::SnapshotOptions;
use mimikyu_core::wait::WaitSelectorState;
use serde_json::Value;

use crate::batch::load_normalized_batch_actions;
use crate::cli::{BatchArgs, RecordArgs, RecordSubcommand, SnapshotArgs, WaitArgs};
use crate::client::ExecClient;
use crate::commands::absolutize_cli_path;
use crate::output::{format_batch_failure, print_json_ok, CliError};

fn emit_value(json_mode: bool, data: &Value, human: impl FnOnce()) -> anyhow::Result<()> {
    if json_mode {
        print_json_ok(data)?;
    } else {
        human();
    }
    Ok(())
}

pub async fn run_open(
    client: &ExecClient,
    session_id: &str,
    url: String,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Open {
            session_id: session_id.to_string(),
            url,
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!(
            "{} ({})",
            data["url"].as_str().unwrap_or(""),
            data["title"].as_str().unwrap_or("")
        );
    })
}

pub async fn run_back(client: &ExecClient, session_id: &str, json_mode: bool) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Back {
            session_id: session_id.to_string(),
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!("{}", data["url"].as_str().unwrap_or(""));
    })
}

pub async fn run_forward(
    client: &ExecClient,
    session_id: &str,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Forward {
            session_id: session_id.to_string(),
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!("{}", data["url"].as_str().unwrap_or(""));
    })
}

pub async fn run_reload(
    client: &ExecClient,
    session_id: &str,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Reload {
            session_id: session_id.to_string(),
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!("{}", data["url"].as_str().unwrap_or(""));
    })
}

pub async fn run_snapshot(
    client: &ExecClient,
    session_id: &str,
    args: SnapshotArgs,
    json_mode: bool,
) -> anyhow::Result<()> {
    let max_depth = match args.max_depth {
        Some(d) if d < 0 => {
            return Err(MimikyuError::InvalidConfig(format!(
                "--max-depth must be non-negative, got {d}"
            ))
            .into());
        }
        Some(d) => Some(d as usize),
        None => None,
    };

    let options = SnapshotOptions {
        max_depth,
        interactive_only: args.interactive_only,
        diff: args.diff,
        include_bounds: args.include_bounds,
    };

    let data = client
        .call(DaemonOp::Snapshot {
            session_id: session_id.to_string(),
            options,
        })
        .await?;
    emit_value(json_mode, &data, || {
        if args.diff
            && let Some(diff_text) = data["diff"]["text"].as_str()
        {
            println!("{diff_text}");
            return;
        }
        println!("{}", data["text"].as_str().unwrap_or(""));
    })
}

pub async fn run_click(
    client: &ExecClient,
    session_id: &str,
    target: String,
    humanize: bool,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Click {
            session_id: session_id.to_string(),
            target: target.clone(),
            humanize: humanize.then_some(true),
        })
        .await?;
    emit_value(json_mode, &data, || println!("clicked {target}"))
}

pub async fn run_fill(
    client: &ExecClient,
    session_id: &str,
    target: String,
    value: String,
    humanize: bool,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Fill {
            session_id: session_id.to_string(),
            target: target.clone(),
            value,
            humanize: humanize.then_some(true),
        })
        .await?;
    emit_value(json_mode, &data, || println!("filled {target}"))
}

pub async fn run_type(
    client: &ExecClient,
    session_id: &str,
    target: String,
    text: String,
    humanize: bool,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::TypeText {
            session_id: session_id.to_string(),
            target: target.clone(),
            text,
            humanize: humanize.then_some(true),
        })
        .await?;
    emit_value(json_mode, &data, || println!("typed into {target}"))
}

pub async fn run_select(
    client: &ExecClient,
    session_id: &str,
    target: String,
    value: String,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Select {
            session_id: session_id.to_string(),
            target,
            value,
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!("selected {}", data["selected"].as_str().unwrap_or(""));
    })
}

pub async fn run_upload(
    client: &ExecClient,
    session_id: &str,
    target: String,
    path: PathBuf,
    json_mode: bool,
) -> anyhow::Result<()> {
    let path = absolutize_cli_path(path)?;
    let data = client
        .call(DaemonOp::Upload {
            session_id: session_id.to_string(),
            target: target.clone(),
            path,
        })
        .await?;
    emit_value(json_mode, &data, || println!("uploaded to {target}"))
}

pub async fn run_scroll(
    client: &ExecClient,
    session_id: &str,
    dx: f64,
    dy: f64,
    humanize: bool,
    json_mode: bool,
) -> anyhow::Result<()> {
    if !dx.is_finite() || !dy.is_finite() {
        return Err(MimikyuError::InvalidConfig(
            "scroll deltas must be finite numbers".to_string(),
        )
        .into());
    }
    let data = client
        .call(DaemonOp::Scroll {
            session_id: session_id.to_string(),
            dx,
            dy,
            humanize: humanize.then_some(true),
        })
        .await?;
    emit_value(json_mode, &data, || println!("scrolled ({dx}, {dy})"))
}

pub async fn run_press(
    client: &ExecClient,
    session_id: &str,
    key: String,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Press {
            session_id: session_id.to_string(),
            key: key.clone(),
        })
        .await?;
    emit_value(json_mode, &data, || println!("pressed {key}"))
}

pub async fn run_wait(
    client: &ExecClient,
    session_id: &str,
    args: WaitArgs,
    json_mode: bool,
) -> anyhow::Result<()> {
    let state = match args.state.trim().to_ascii_lowercase().as_str() {
        "attached" => WaitSelectorState::Attached,
        "visible" => WaitSelectorState::Visible,
        "hidden" => WaitSelectorState::Hidden,
        "detached" => WaitSelectorState::Detached,
        other => {
            return Err(MimikyuError::InvalidConfig(format!(
                "unknown wait state '{other}' (expected attached|visible|hidden|detached)"
            ))
            .into());
        }
    };
    let request = WaitRequest {
        idle: args.idle,
        text: args.text,
        url: args.url,
        selector: args.selector,
        state,
        timeout_ms: args.timeout_ms,
        poll_interval_ms: args.poll_interval_ms,
        network_quiet_ms: args.network_quiet_ms,
        stable_target: args.stable_target,
    };
    if !request.idle
        && request.text.is_none()
        && request.url.is_none()
        && request.selector.is_none()
    {
        return Err(MimikyuError::InvalidConfig(
            "wait requires at least one condition (--idle, --text, --url, or --selector)"
                .to_string(),
        )
        .into());
    }

    let data = client
        .call(DaemonOp::Wait {
            session_id: session_id.to_string(),
            request,
        })
        .await?;
    emit_value(json_mode, &data, || println!("wait satisfied"))
}

pub async fn run_batch(
    client: &ExecClient,
    session_id: &str,
    args: BatchArgs,
    json_mode: bool,
) -> anyhow::Result<()> {
    let normalized = load_normalized_batch_actions(&args)?;
    let data = client
        .call(DaemonOp::Batch {
            session_id: session_id.to_string(),
            actions: Value::Array(normalized),
        })
        .await?;

    let report: BatchReport = serde_json::from_value(data.clone())?;
    if !report.ok {
        let msg = format_batch_failure(&report);
        return Err(CliError::with_data(msg, data).into());
    }

    emit_value(json_mode, &data, || {
        println!(
            "batch completed {}/{} steps",
            report.completed_steps, report.total_steps
        );
    })
}

pub async fn run_extract(
    client: &ExecClient,
    session_id: &str,
    mode_str: String,
    json_mode: bool,
) -> anyhow::Result<()> {
    let mode = mode_str.parse::<ExtractMode>()?;
    let data = client
        .call(DaemonOp::Extract {
            session_id: session_id.to_string(),
            mode,
        })
        .await?;
    emit_value(json_mode, &data, || {
        match mode {
            ExtractMode::Markdown => println!("{}", data["markdown"].as_str().unwrap_or("")),
            ExtractMode::Text => println!("{}", data["text"].as_str().unwrap_or("")),
            ExtractMode::Links | ExtractMode::Tables => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_default()
                );
            }
        }
    })
}

pub async fn run_eval(
    client: &ExecClient,
    session_id: &str,
    js: String,
    main_world: bool,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::Eval {
            session_id: session_id.to_string(),
            js,
            main_world,
        })
        .await?;
    emit_value(json_mode, &data, || {
        let val = &data["value"];
        if let Some(s) = val.as_str() {
            println!("{s}");
        } else {
            println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
        }
    })
}

pub async fn run_screenshot(
    client: &ExecClient,
    session_id: &str,
    out_path: PathBuf,
    target: Option<String>,
    full_page: bool,
    json_mode: bool,
) -> anyhow::Result<()> {
    let out_path = absolutize_cli_path(out_path)?;
    let data = client
        .call(DaemonOp::Screenshot {
            session_id: session_id.to_string(),
            out_path,
            target,
            full_page,
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!(
            "saved screenshot {} ({}x{}, {} bytes)",
            data["path"].as_str().unwrap_or(""),
            data["width"].as_u64().unwrap_or(0),
            data["height"].as_u64().unwrap_or(0),
            data["bytes"].as_u64().unwrap_or(0),
        );
    })
}

pub async fn run_pdf(
    client: &ExecClient,
    session_id: &str,
    out_path: PathBuf,
    json_mode: bool,
) -> anyhow::Result<()> {
    let out_path = absolutize_cli_path(out_path)?;
    let data = client
        .call(DaemonOp::Pdf {
            session_id: session_id.to_string(),
            out_path,
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!(
            "saved pdf {} ({} bytes)",
            data["path"].as_str().unwrap_or(""),
            data["bytes"].as_u64().unwrap_or(0),
        );
    })
}

pub async fn handle_record(
    client: &ExecClient,
    session_id: &str,
    args: RecordArgs,
    json_mode: bool,
) -> anyhow::Result<()> {
    match args.action {
        RecordSubcommand::Start {
            out_dir,
            format,
            quality,
            max_width,
            max_height,
            every_nth_frame,
        } => {
            let parsed_format = format.parse::<ScreencastFormat>()?;
            if let Some(q) = quality
                && (q == 0 || q > 100)
            {
                return Err(MimikyuError::InvalidConfig(format!(
                    "--quality must be 1..=100, got {q}"
                ))
                .into());
            }
            if every_nth_frame == Some(0) {
                return Err(MimikyuError::InvalidConfig(
                    "--every-nth-frame must be >= 1".to_string(),
                )
                .into());
            }
            let out_dir = absolutize_cli_path(out_dir)?;
            let options = ScreencastOptions {
                out_dir,
                format: parsed_format,
                quality,
                max_width,
                max_height,
                every_nth_frame,
            };
            let data = client
                .call(DaemonOp::RecordStart {
                    session_id: session_id.to_string(),
                    options,
                })
                .await?;
            emit_value(json_mode, &data, || {
                println!(
                    "recording screencast for session '{}' -> {} (format={})",
                    session_id,
                    data["out_dir"].as_str().unwrap_or(""),
                    data["format"].as_str().unwrap_or("png"),
                );
            })
        }
        RecordSubcommand::Stop => {
            let data = client
                .call(DaemonOp::RecordStop {
                    session_id: session_id.to_string(),
                })
                .await?;
            emit_value(json_mode, &data, || {
                println!(
                    "stopped screencast for session '{}' ({} frames in {})",
                    session_id,
                    data["frame_count"].as_u64().unwrap_or(0),
                    data["out_dir"].as_str().unwrap_or(""),
                );
            })
        }
    }
}

pub async fn run_logs(
    client: &ExecClient,
    session_id: &str,
    kind_str: String,
    json_mode: bool,
) -> anyhow::Result<()> {
    let kind = kind_str.parse::<LogKind>()?;
    let data = client
        .call(DaemonOp::Logs {
            session_id: session_id.to_string(),
            kind,
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!(
            "{}",
            serde_json::to_string_pretty(&data).unwrap_or_default()
        );
    })
}
