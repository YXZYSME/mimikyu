use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mimikyu_core::discovery::{run_doctor, DoctorReport};
use mimikyu_core::driver::EngineKind;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "mimikyu",
    version,
    about = "Fast browser automation, session/profile isolation, and environment consistency control plane for AI agents"
)]
struct Cli {
    /// Emit machine-readable JSON envelope: {"ok": true, "data": ...} or {"ok": false, "error": "..."}
    #[arg(long, global = true)]
    json: bool,

    /// Target session identifier
    #[arg(long, global = true, default_value = "default")]
    session: String,

    /// Browser engine (chromium | firefox)
    #[arg(long, global = true, value_parser = parse_engine)]
    engine: Option<EngineKind>,

    /// State directory for persistent profiles and daemon sockets
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Discover local Chromium & Firefox binaries and check host readiness
    Doctor,
}

#[derive(Serialize)]
struct JsonOk<T: Serialize> {
    ok: bool,
    data: T,
}

#[derive(Serialize)]
struct JsonErr<'a> {
    ok: bool,
    error: &'a str,
}

fn parse_engine(s: &str) -> Result<EngineKind, String> {
    s.parse::<EngineKind>().map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_mode = cli.json;

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if json_mode {
                let msg = err.to_string();
                let payload = JsonErr {
                    ok: false,
                    error: &msg,
                };
                if let Ok(s) = serde_json::to_string(&payload) {
                    println!("{s}");
                } else {
                    println!("{{\"ok\":false,\"error\":\"internal serialization error\"}}");
                }
            } else {
                eprintln!("error: {err}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Validate global context options are well-formed; M4 will route session/state_dir/engine to daemon.
    let _ = (&cli.session, &cli.engine, &cli.state_dir);

    match cli.command {
        Commands::Doctor => {
            let report = run_doctor();
            if cli.json {
                let envelope = JsonOk {
                    ok: true,
                    data: report,
                };
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            } else {
                print_doctor_human(&report);
            }
            Ok(())
        }
    }
}

fn print_doctor_human(report: &DoctorReport) {
    println!("mimikyu doctor ({})", report.platform);
    println!(
        "  default chromium: {}",
        report
            .default_chromium
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!(
        "  default firefox:  {}",
        report
            .default_firefox
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    if !report.chromium.is_empty() {
        println!("\nChromium binaries ({}):", report.chromium.len());
        for b in &report.chromium {
            println!(
                "  - [{:?} / {:?}] {} (headed={}) -> {}",
                b.kind,
                b.source,
                b.version,
                b.supports_headed,
                b.path.display()
            );
        }
    }
    if !report.firefox.is_empty() {
        println!("\nFirefox binaries ({}):", report.firefox.len());
        for b in &report.firefox {
            println!(
                "  - [{:?} / {:?}] {} (headed={}) -> {}",
                b.kind,
                b.source,
                b.version,
                b.supports_headed,
                b.path.display()
            );
        }
    }
    if !report.warnings.is_empty() {
        println!("\nWarnings:");
        for w in &report.warnings {
            println!("  ! {w}");
        }
    }
}
