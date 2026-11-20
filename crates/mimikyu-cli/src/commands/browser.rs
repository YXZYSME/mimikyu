use mimikyu_core::browser_install::{install_browser, BrowserInstallOptions};
use mimikyu_core::discovery::{discover_browser, managed_browsers_dir, run_doctor};
use serde_json::json;

use crate::cli::{BrowserArgs, BrowserSubcommand};
use crate::output::print_json_ok;

pub fn handle_browser(args: BrowserArgs, json_mode: bool) -> anyhow::Result<()> {
    match args.action {
        BrowserSubcommand::List => {
            let report = run_doctor();
            let managed_dir = managed_browsers_dir();
            if json_mode {
                let payload = json!({
                    "platform": report.platform,
                    "managed_dir": managed_dir,
                    "default_chromium": report.default_chromium,
                    "default_firefox": report.default_firefox,
                    "chromium": report.chromium,
                    "firefox": report.firefox,
                });
                print_json_ok(&payload)?;
            } else {
                println!("managed dir: {}", managed_dir.display());
                println!("Chromium ({}):", report.chromium.len());
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
                println!("Firefox ({}):", report.firefox.len());
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
            Ok(())
        }
        BrowserSubcommand::Path { engine, headed } => {
            let browser = discover_browser(engine, !headed, None)?;
            if json_mode {
                print_json_ok(&browser)?;
            } else {
                println!("{}", browser.path.display());
            }
            Ok(())
        }
        BrowserSubcommand::Install {
            engine,
            dry_run,
            version,
            sha256,
        } => {
            let result = install_browser(&BrowserInstallOptions {
                engine,
                version,
                dry_run,
                expected_sha256: sha256,
                install_root: None,
            })?;
            if json_mode {
                print_json_ok(&result)?;
            } else if result.dry_run {
                println!(
                    "[dry-run] {} {} ({})\n  url:  {}\n  dest: {}\n  bin:  {}",
                    result.engine,
                    result.version,
                    result.platform,
                    result.download_url,
                    result.install_dir.display(),
                    result.binary_path.display()
                );
            } else if result.already_installed {
                println!(
                    "{} {} already installed at {}",
                    result.engine,
                    result.version,
                    result.binary_path.display()
                );
            } else {
                println!(
                    "installed {} {} -> {} (sha256={}, verified={})",
                    result.engine,
                    result.version,
                    result.binary_path.display(),
                    result.sha256.as_deref().unwrap_or("n/a"),
                    result.verified_sha256
                );
            }
            Ok(())
        }
    }
}
