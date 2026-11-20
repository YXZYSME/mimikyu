use std::fmt;

use mimikyu_core::discovery::DoctorReport;
use mimikyu_core::input::BatchReport;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub data: Option<Value>,
}

impl CliError {
    pub fn with_data(message: impl Into<String>, data: Value) -> Self {
        Self {
            message: message.into(),
            data: Some(data),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

#[derive(Serialize)]
struct JsonOk<'a, T: Serialize> {
    ok: bool,
    data: &'a T,
}

#[derive(Serialize)]
struct JsonErr<'a> {
    ok: bool,
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a Value>,
}

pub fn print_json_ok<T: Serialize>(data: &T) -> anyhow::Result<()> {
    let envelope = JsonOk { ok: true, data };
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}

pub fn print_json_err(message: &str, data: Option<&Value>) {
    let payload = JsonErr {
        ok: false,
        error: message,
        data,
    };
    if let Ok(s) = serde_json::to_string(&payload) {
        println!("{s}");
    } else {
        println!("{{\"ok\":false,\"error\":\"internal serialization error\"}}");
    }
}

pub fn print_doctor_human(report: &DoctorReport) {
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
    if !report.skipped_candidates.is_empty() {
        println!(
            "\nSkipped candidates ({}):",
            report.skipped_candidates.len()
        );
        for s in &report.skipped_candidates {
            println!(
                "  - [{} / {:?}] {} (skipped: {})",
                s.engine,
                s.source,
                s.path.display(),
                s.reason
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

pub fn format_batch_failure(report: &BatchReport) -> String {
    match &report.error {
        Some(err) => format!(
            "batch failed at step {} ({}): {}",
            err.step_index, err.action, err.message
        ),
        None => "batch execution failed".to_string(),
    }
}
