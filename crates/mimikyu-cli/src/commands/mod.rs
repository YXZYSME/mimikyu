pub mod page;
pub mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mimikyu_core::bench::{format_bench_human, run_benchmark, BenchOptions};
use mimikyu_core::daemon::{
    daemon_status, daemon_stop, ensure_daemon, run_daemon_foreground, DaemonConfig,
    DEFAULT_DAEMON_IDLE_TIMEOUT_SECS,
};
use mimikyu_core::discovery::run_doctor;
use mimikyu_core::driver::EngineKind;
use mimikyu_core::error::MimikyuError;
use mimikyu_core::mcp::serve_stdio;
use mimikyu_core::session::{SessionManager, SessionManagerConfig};

use crate::cli::{BenchArgs, Cli, Commands, DaemonArgs, DaemonSubcommand};
use crate::client::ExecClient;
use crate::output::{print_doctor_human, print_json_ok};

pub fn resolve_state_dir(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| SessionManagerConfig::default().state_dir)
}

pub fn absolutize_cli_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let json_mode = cli.json;
    let global_session = cli.session;
    let global_engine = cli.engine;
    let no_daemon = cli.no_daemon;

    match cli.command {
        Commands::Doctor => {
            let report = run_doctor();
            if json_mode {
                print_json_ok(&report)?;
            } else {
                print_doctor_human(&report);
            }
            Ok(())
        }
        Commands::Daemon(args) => {
            let state_dir = resolve_state_dir(cli.state_dir);
            handle_daemon(args, state_dir, global_engine, json_mode).await
        }
        Commands::Mcp => {
            let state_dir = resolve_state_dir(cli.state_dir);
            let mgr = Arc::new(SessionManager::with_config(SessionManagerConfig {
                state_dir,
                default_engine: global_engine.unwrap_or(EngineKind::Chromium),
                ..Default::default()
            })?);
            serve_stdio(mgr).await?;
            Ok(())
        }
        Commands::Bench(args) => handle_bench(args, global_engine, json_mode).await,
        cmd => {
            let state_dir = resolve_state_dir(cli.state_dir);
            let client = ExecClient::connect(state_dir, global_engine, no_daemon).await?;
            let res = dispatch_client_command(&client, cmd, &global_session, global_engine, json_mode).await;
            client.finish().await;
            res
        }
    }
}

async fn dispatch_client_command(
    client: &ExecClient,
    cmd: Commands,
    global_session: &str,
    global_engine: Option<EngineKind>,
    json_mode: bool,
) -> anyhow::Result<()> {
    match cmd {
        Commands::Session(args) => {
            session::handle_session(client, args, global_session, global_engine, json_mode).await
        }
        Commands::Close { session_id } => {
            let target = session_id.as_deref().unwrap_or(global_session);
            session::run_session_close(client, target, json_mode).await
        }
        Commands::Profile(args) => {
            session::handle_profile(client, args, global_engine, json_mode).await
        }
        Commands::Context(args) => {
            session::handle_context(client, args, global_session, json_mode).await
        }
        Commands::Open { url } => page::run_open(client, global_session, url, json_mode).await,
        Commands::Back => page::run_back(client, global_session, json_mode).await,
        Commands::Forward => page::run_forward(client, global_session, json_mode).await,
        Commands::Reload => page::run_reload(client, global_session, json_mode).await,
        Commands::Snapshot(args) => {
            page::run_snapshot(client, global_session, args, json_mode).await
        }
        Commands::Click { target, humanize } => {
            page::run_click(client, global_session, target, humanize, json_mode).await
        }
        Commands::Fill {
            target,
            value,
            humanize,
        } => page::run_fill(client, global_session, target, value, humanize, json_mode).await,
        Commands::Type {
            target,
            text,
            humanize,
        } => page::run_type(client, global_session, target, text, humanize, json_mode).await,
        Commands::Select { target, value } => {
            page::run_select(client, global_session, target, value, json_mode).await
        }
        Commands::Upload { target, path } => {
            page::run_upload(client, global_session, target, path, json_mode).await
        }
        Commands::Scroll { dx, dy, humanize } => {
            page::run_scroll(client, global_session, dx, dy, humanize, json_mode).await
        }
        Commands::Press { key } => page::run_press(client, global_session, key, json_mode).await,
        Commands::Wait(args) => page::run_wait(client, global_session, args, json_mode).await,
        Commands::Batch(args) => page::run_batch(client, global_session, args, json_mode).await,
        Commands::Extract { mode } => {
            page::run_extract(client, global_session, mode, json_mode).await
        }
        Commands::Eval { js, main_world } => {
            page::run_eval(client, global_session, js, main_world, json_mode).await
        }
        Commands::Screenshot {
            out,
            target,
            full_page,
        } => page::run_screenshot(client, global_session, out, target, full_page, json_mode).await,
        Commands::Pdf { out } => page::run_pdf(client, global_session, out, json_mode).await,
        Commands::Logs { kind } => page::run_logs(client, global_session, kind, json_mode).await,
        Commands::Doctor | Commands::Daemon(_) | Commands::Mcp | Commands::Bench(_) => {
            unreachable!("handled directly without client")
        }
    }
}

async fn handle_daemon(
    args: DaemonArgs,
    state_dir: PathBuf,
    engine: Option<EngineKind>,
    json_mode: bool,
) -> anyhow::Result<()> {
    match args.action {
        Some(DaemonSubcommand::Status) => {
            if args.foreground || args.idle_timeout.is_some() {
                return Err(MimikyuError::InvalidConfig(
                    "--foreground and --idle-timeout cannot be used with `daemon status`"
                        .to_string(),
                )
                .into());
            }
            let status = daemon_status(&state_dir).await?;
            if json_mode {
                print_json_ok(&status)?;
            } else if status.running {
                println!(
                    "daemon running (pid={}, sessions={}, socket={})",
                    status.pid,
                    status.active_sessions,
                    status.socket_path.display()
                );
            } else {
                println!("daemon not running (state_dir={})", status.state_dir.display());
            }
            Ok(())
        }
        Some(DaemonSubcommand::Stop) => {
            if args.foreground || args.idle_timeout.is_some() {
                return Err(MimikyuError::InvalidConfig(
                    "--foreground and --idle-timeout cannot be used with `daemon stop`"
                        .to_string(),
                )
                .into());
            }
            let stopped = daemon_stop(&state_dir).await?;
            let payload = serde_json::json!({ "stopped": stopped });
            if json_mode {
                print_json_ok(&payload)?;
            } else if stopped {
                println!("daemon stopped");
            } else {
                println!("daemon was not running");
            }
            Ok(())
        }
        None => {
            let idle_timeout = match args.idle_timeout {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => Some(Duration::from_secs(DEFAULT_DAEMON_IDLE_TIMEOUT_SECS)),
            };
            let config = DaemonConfig {
                state_dir: state_dir.clone(),
                idle_timeout,
                default_engine: engine.unwrap_or(EngineKind::Chromium),
                default_headless: true,
            };
            if args.foreground {
                run_daemon_foreground(config).await?;
                Ok(())
            } else {
                let _client = ensure_daemon(config).await?;
                let status = daemon_status(&state_dir).await?;
                if json_mode {
                    print_json_ok(&status)?;
                } else {
                    println!(
                        "daemon ready (pid={}, socket={})",
                        status.pid,
                        status.socket_path.display()
                    );
                }
                Ok(())
            }
        }
    }
}

async fn handle_bench(
    args: BenchArgs,
    engine: Option<EngineKind>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let opts = BenchOptions::from_cli(
        args.iterations,
        args.warmup,
        engine.unwrap_or(EngineKind::Chromium),
        &args.suite,
    )?;
    let report = run_benchmark(opts).await?;
    if json_mode {
        print_json_ok(&report)?;
    } else {
        println!("{}", format_bench_human(&report));
    }
    Ok(())
}
