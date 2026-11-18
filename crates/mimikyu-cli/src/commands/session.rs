use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use mimikyu_core::daemon::DaemonOp;
use mimikyu_core::driver::EngineKind;
use mimikyu_core::env_profile::PresetKind;
use mimikyu_core::error::{MimikyuError, Result as CoreResult};
use mimikyu_core::session::SessionCreateOpts;
use serde_json::Value;

use crate::cli::{
    ContextArgs, ContextSubcommand, ProfileArgs, ProfileSubcommand, SessionArgs, SessionNewArgs,
    SessionSubcommand,
};
use crate::client::ExecClient;
use crate::output::print_json_ok;

pub fn parse_viewport(raw: &str) -> CoreResult<(u32, u32)> {
    let trimmed = raw.trim();
    let (ws, hs) = trimmed
        .split_once('x')
        .or_else(|| trimmed.split_once('X'))
        .ok_or_else(|| {
            MimikyuError::InvalidConfig(format!("invalid viewport '{raw}' (expected WxH)"))
        })?;
    let w: u32 = ws.trim().parse().map_err(|_| {
        MimikyuError::InvalidConfig(format!("invalid viewport width in '{raw}'"))
    })?;
    let h: u32 = hs.trim().parse().map_err(|_| {
        MimikyuError::InvalidConfig(format!("invalid viewport height in '{raw}'"))
    })?;
    if w == 0 || h == 0 || w > 16_384 || h > 16_384 {
        return Err(MimikyuError::InvalidConfig(format!(
            "viewport {w}x{h} out of valid bounds (1..=16384)"
        )));
    }
    Ok((w, h))
}

fn transient_session_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}", std::process::id())
}

pub async fn handle_session(
    client: &ExecClient,
    args: SessionArgs,
    global_session: &str,
    global_engine: Option<EngineKind>,
    json_mode: bool,
) -> anyhow::Result<()> {
    match args.action {
        SessionSubcommand::New(new_args) => {
            run_session_new(client, new_args, global_session, global_engine, json_mode).await
        }
        SessionSubcommand::List => run_session_list(client, json_mode).await,
        SessionSubcommand::Close { session_id } => {
            let target = session_id.as_deref().unwrap_or(global_session);
            run_session_close(client, target, json_mode).await
        }
        SessionSubcommand::Export { file } => {
            let data = client
                .call(DaemonOp::SessionExport {
                    session_id: global_session.to_string(),
                    out_path: file,
                })
                .await?;
            emit_value(json_mode, &data, || {
                println!(
                    "exported session '{}' to {}",
                    global_session,
                    data["path"].as_str().unwrap_or("")
                );
            })
        }
        SessionSubcommand::Import { file } => {
            let data = client
                .call(DaemonOp::SessionImport {
                    session_id: global_session.to_string(),
                    in_path: file,
                })
                .await?;
            emit_value(json_mode, &data, || {
                println!(
                    "imported storage state into session '{}' from {}",
                    global_session,
                    data["path"].as_str().unwrap_or("")
                );
            })
        }
    }
}

async fn run_session_new(
    client: &ExecClient,
    args: SessionNewArgs,
    global_session: &str,
    global_engine: Option<EngineKind>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let preset = args
        .preset
        .as_deref()
        .map(str::parse::<PresetKind>)
        .transpose()?;
    let viewport = args.viewport.as_deref().map(parse_viewport).transpose()?;

    let opts = SessionCreateOpts {
        session_id: Some(global_session.to_string()),
        engine: global_engine,
        profile: args.profile,
        ephemeral: args.ephemeral,
        virtual_ctx: args.r#virtual,
        preset,
        locale: args.locale,
        timezone: args.timezone,
        proxy: args.proxy,
        geoip: args.geoip,
        viewport,
        block_images: args.block_images,
        block_media: args.block_media,
        block_fonts: args.block_fonts,
        instant_animations: args.instant_animations,
        humanize: args.humanize,
        main_world_eval: true,
        headless: !args.headed,
    };

    let data = client.call(DaemonOp::SessionNew(opts)).await?;
    emit_value(json_mode, &data, || {
        println!(
            "session '{}' created (engine={}, mode={})",
            data["session_id"].as_str().unwrap_or(global_session),
            data["engine"].as_str().unwrap_or("unknown"),
            data["mode"].as_str().unwrap_or("unknown"),
        );
    })
}

pub async fn run_session_list(client: &ExecClient, json_mode: bool) -> anyhow::Result<()> {
    let data = client.call(DaemonOp::SessionList).await?;
    emit_value(json_mode, &data, || {
        if let Some(arr) = data.as_array() {
            if arr.is_empty() {
                println!("no active sessions");
            } else {
                for entry in arr {
                    println!(
                        "{}\t{}\t{}\t{}",
                        entry["session_id"].as_str().unwrap_or("-"),
                        entry["engine"].as_str().unwrap_or("-"),
                        entry["mode"].as_str().unwrap_or("-"),
                        entry["url"].as_str().unwrap_or("about:blank"),
                    );
                }
            }
        }
    })
}

pub async fn run_session_close(
    client: &ExecClient,
    session_id: &str,
    json_mode: bool,
) -> anyhow::Result<()> {
    let data = client
        .call(DaemonOp::SessionClose {
            session_id: session_id.to_string(),
        })
        .await?;
    emit_value(json_mode, &data, || {
        println!("closed session '{session_id}'");
    })
}

pub async fn handle_profile(
    client: &ExecClient,
    args: ProfileArgs,
    global_engine: Option<EngineKind>,
    json_mode: bool,
) -> anyhow::Result<()> {
    match args.action {
        ProfileSubcommand::List => {
            let data = client.call(DaemonOp::ProfileList).await?;
            emit_value(json_mode, &data, || {
                if let Some(arr) = data.as_array() {
                    if arr.is_empty() {
                        println!("no named profiles");
                    } else {
                        for item in arr {
                            println!(
                                "{}\tlocked={}\t{}",
                                item["name"].as_str().unwrap_or("-"),
                                item["locked"].as_bool().unwrap_or(false),
                                item["path"].as_str().unwrap_or(""),
                            );
                        }
                    }
                }
            })
        }
        ProfileSubcommand::Delete { name } => {
            let data = client.call(DaemonOp::ProfileDelete { name: name.clone() }).await?;
            emit_value(json_mode, &data, || {
                println!("deleted profile '{name}'");
            })
        }
        ProfileSubcommand::Export { name, file } => {
            validate_export_target(&file)?;
            let sid = transient_session_id("prof-exp");
            let opts = SessionCreateOpts {
                session_id: Some(sid.clone()),
                engine: global_engine,
                profile: Some(name),
                main_world_eval: true,
                headless: true,
                ..Default::default()
            };
            client.call(DaemonOp::SessionNew(opts)).await?;
            let res = client
                .call(DaemonOp::SessionExport {
                    session_id: sid.clone(),
                    out_path: file,
                })
                .await;
            let _ = client.call(DaemonOp::SessionClose { session_id: sid }).await;
            let data = res?;
            emit_value(json_mode, &data, || {
                println!("exported profile to {}", data["path"].as_str().unwrap_or(""));
            })
        }
        ProfileSubcommand::Import { name, file } => {
            if file.as_os_str().is_empty() || !file.is_file() {
                return Err(MimikyuError::InvalidConfig(format!(
                    "import file '{}' does not exist or is not a file",
                    file.display()
                ))
                .into());
            }
            let sid = transient_session_id("prof-imp");
            let opts = SessionCreateOpts {
                session_id: Some(sid.clone()),
                engine: global_engine,
                profile: Some(name),
                main_world_eval: true,
                headless: true,
                ..Default::default()
            };
            client.call(DaemonOp::SessionNew(opts)).await?;
            let res = client
                .call(DaemonOp::SessionImport {
                    session_id: sid.clone(),
                    in_path: file,
                })
                .await;
            let _ = client.call(DaemonOp::SessionClose { session_id: sid }).await;
            let data = res?;
            emit_value(json_mode, &data, || {
                println!("imported profile from {}", data["path"].as_str().unwrap_or(""));
            })
        }
    }
}

pub async fn handle_context(
    client: &ExecClient,
    args: ContextArgs,
    global_session: &str,
    json_mode: bool,
) -> anyhow::Result<()> {
    match args.action {
        ContextSubcommand::List => run_session_list(client, json_mode).await,
        ContextSubcommand::Close { session_id } => {
            let target = session_id.as_deref().unwrap_or(global_session);
            run_session_close(client, target, json_mode).await
        }
    }
}

fn validate_export_target(path: &Path) -> CoreResult<()> {
    if path.as_os_str().is_empty() || path.is_dir() {
        return Err(MimikyuError::InvalidConfig(format!(
            "invalid export path '{}'",
            path.display()
        )));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.is_dir()
    {
        return Err(MimikyuError::InvalidConfig(format!(
            "export parent directory '{}' does not exist",
            parent.display()
        )));
    }
    Ok(())
}

fn emit_value(json_mode: bool, data: &Value, human: impl FnOnce()) -> anyhow::Result<()> {
    if json_mode {
        print_json_ok(data)?;
    } else {
        human();
    }
    Ok(())
}
