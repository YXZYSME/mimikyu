pub mod rss;
pub mod workflows;

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::discovery::discover_browser;
use crate::driver::EngineKind;
use crate::error::{MimikyuError, Result};
use crate::harness::HarnessServer;
use crate::input::BatchAction;
use crate::session::{ExtractMode, SessionCreateOpts, SessionManager, SessionManagerConfig, WaitRequest};
use crate::snapshot::SnapshotOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BenchSuite {
    #[default]
    All,
    Speed,
    Agentic,
}

impl fmt::Display for BenchSuite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BenchSuite::All => write!(f, "all"),
            BenchSuite::Speed => write!(f, "speed"),
            BenchSuite::Agentic => write!(f, "agentic"),
        }
    }
}

impl FromStr for BenchSuite {
    type Err = MimikyuError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(BenchSuite::All),
            "speed" => Ok(BenchSuite::Speed),
            "agentic" => Ok(BenchSuite::Agentic),
            other => Err(MimikyuError::InvalidConfig(format!(
                "unknown bench suite '{other}' (expected all|speed|agentic)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchOptions {
    pub iterations: u32,
    pub warmup: u32,
    pub engine: EngineKind,
    pub suite: BenchSuite,
    pub headless: bool,
    pub state_dir: Option<PathBuf>,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            iterations: 3,
            warmup: 1,
            engine: EngineKind::Chromium,
            suite: BenchSuite::All,
            headless: true,
            state_dir: None,
        }
    }
}

impl BenchOptions {
    pub fn validate(&self) -> Result<()> {
        if self.iterations < 1 {
            return Err(MimikyuError::InvalidConfig(format!(
                "bench iterations must be >= 1, got {}",
                self.iterations
            )));
        }
        if self.warmup.checked_add(self.iterations).is_none() {
            return Err(MimikyuError::InvalidConfig(format!(
                "bench warmup ({}) + iterations ({}) overflows u32",
                self.warmup, self.iterations
            )));
        }
        Ok(())
    }

    pub fn from_cli(
        iterations: i64,
        warmup: i64,
        engine: EngineKind,
        suite: &str,
    ) -> Result<Self> {
        if iterations < 1 {
            return Err(MimikyuError::InvalidConfig(format!(
                "--iterations must be >= 1, got {iterations}"
            )));
        }
        if warmup < 0 {
            return Err(MimikyuError::InvalidConfig(format!(
                "--warmup must be >= 0, got {warmup}"
            )));
        }
        let iter_u32 = u32::try_from(iterations).map_err(|_| {
            MimikyuError::InvalidConfig(format!("--iterations out of range: {iterations}"))
        })?;
        let warmup_u32 = u32::try_from(warmup).map_err(|_| {
            MimikyuError::InvalidConfig(format!("--warmup out of range: {warmup}"))
        })?;
        let parsed_suite: BenchSuite = suite.parse()?;
        Ok(Self {
            iterations: iter_u32,
            warmup: warmup_u32,
            engine,
            suite: parsed_suite,
            ..Default::default()
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchTaskResult {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub passed: bool,
    pub steps: usize,
    pub latency_ms: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    pub iterations: u32,
    pub warmup: u32,
    pub engine: EngineKind,
    pub suite: BenchSuite,
    pub cold_launch_ms: f64,
    pub warm_context_ms: f64,
    pub navigation_ms: f64,
    pub snapshot_ms: f64,
    pub extract_ms: f64,
    pub batch_ms: f64,
    pub avg_iteration_ms: f64,
    pub median_iteration_ms: f64,
    pub total_ms: f64,
    pub memory_rss_bytes: u64,
    pub memory_rss_mb: f64,
    pub raw_html_bytes: usize,
    pub snapshot_bytes: usize,
    pub raw_estimated_tokens: usize,
    pub snapshot_estimated_tokens: usize,
    pub token_compression_ratio: f64,
    pub token_compression: f64,
    pub tasks: Vec<BenchTaskResult>,
    pub completed_tasks: usize,
    pub total_tasks: usize,
    pub completion_rate: f64,
}

pub async fn run_benchmark(opts: BenchOptions) -> Result<BenchReport> {
    opts.validate()?;
    discover_browser(opts.engine, opts.headless, None)?;

    let tmp_dir = tempfile::Builder::new()
        .prefix("mimikyu-bench-")
        .tempdir()?;
    let state_dir = opts
        .state_dir
        .clone()
        .unwrap_or_else(|| tmp_dir.path().to_path_buf());

    let mgr = SessionManager::with_config(SessionManagerConfig {
        state_dir,
        default_engine: opts.engine,
        default_headless: opts.headless,
        ..Default::default()
    })?;
    let harness = HarnessServer::start().await?;

    let res = run_benchmark_inner(&opts, &mgr, &harness).await;
    let shutdown_res = mgr.shutdown_all().await;
    harness.shutdown().await;
    drop(tmp_dir);

    match (res, shutdown_res) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

struct SpeedSample {
    cold_launch_ms: Vec<f64>,
    warm_context_ms: Vec<f64>,
    navigation_ms: Vec<f64>,
    snapshot_ms: Vec<f64>,
    extract_ms: Vec<f64>,
    batch_ms: Vec<f64>,
    iter_total_ms: Vec<f64>,
    memory_rss_bytes: u64,
    raw_html_bytes: usize,
    snapshot_bytes: usize,
}

fn elapsed_ms(t0: Instant) -> f64 {
    (t0.elapsed().as_secs_f64() * 1000.0).max(0.001)
}

fn median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) * 0.5
    } else {
        sorted[mid]
    }
}

fn mean_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / (values.len() as f64)
}

async fn run_speed_rounds(
    rounds: u32,
    warmup: u32,
    opts: &BenchOptions,
    mgr: &SessionManager,
    harness: &HarnessServer,
) -> Result<SpeedSample> {
    let mut sample = SpeedSample {
        cold_launch_ms: Vec::new(),
        warm_context_ms: Vec::new(),
        navigation_ms: Vec::new(),
        snapshot_ms: Vec::new(),
        extract_ms: Vec::new(),
        batch_ms: Vec::new(),
        iter_total_ms: Vec::new(),
        memory_rss_bytes: 0,
        raw_html_bytes: 0,
        snapshot_bytes: 0,
    };

    let catalog_url = harness.url("/catalog");
    let form_url = harness.url("/form");
    let mut pool_primed = false;

    for round in 0..rounds {
        let measured = round >= warmup;

        // 1. Cold ephemeral browser launch + live RSS sampling
        let cold_id = format!("bench-cold-{round}");
        let t_cold = Instant::now();
        let summary = mgr
            .create_session(SessionCreateOpts {
                session_id: Some(cold_id.clone()),
                engine: Some(opts.engine),
                ephemeral: true,
                headless: opts.headless,
                main_world_eval: false,
                proxy: None,
                ..Default::default()
            })
            .await?;
        let cold_ms = elapsed_ms(t_cold);

        let cold_round_res: Result<()> = async {
            mgr.open(&cold_id, &catalog_url).await?;
            mgr.wait(
                &cold_id,
                WaitRequest {
                    idle: true,
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            )
            .await?;
            if measured || sample.memory_rss_bytes == 0 {
                if let Some(pid) = summary.pid
                    && let Some(bytes) = rss::measure_process_tree_rss_bytes(pid)
                {
                    sample.memory_rss_bytes = bytes;
                }
                if sample.raw_html_bytes == 0 {
                    let html_val = mgr
                        .eval(&cold_id, "document.documentElement.outerHTML", false)
                        .await?;
                    let html_len = html_val.as_str().map_or(0, str::len);
                    let snap = mgr.snapshot(&cold_id, SnapshotOptions::default()).await?;
                    sample.raw_html_bytes = html_len.max(1);
                    sample.snapshot_bytes = snap.text.len().max(1);
                }
            }
            Ok(())
        }
        .await;
        let close_cold = mgr.close_session(&cold_id).await;
        cold_round_res?;
        close_cold?;

        // Prime virtual pool once if warmup == 0 so warm_context_ms measures context creation
        if !pool_primed && warmup == 0 {
            let prime_id = "bench-pool-prime";
            mgr.create_session(SessionCreateOpts {
                session_id: Some(prime_id.to_string()),
                engine: Some(opts.engine),
                virtual_ctx: true,
                headless: opts.headless,
                main_world_eval: false,
                proxy: None,
                ..Default::default()
            })
            .await?;
            mgr.close_session(prime_id).await?;
            pool_primed = true;
        } else if round < warmup {
            pool_primed = true;
        }

        // 2. Warm virtual context creation + navigation + snapshot + extract + batch
        let warm_id = format!("bench-warm-{round}");
        let t_warm = Instant::now();
        mgr.create_session(SessionCreateOpts {
            session_id: Some(warm_id.clone()),
            engine: Some(opts.engine),
            virtual_ctx: true,
            headless: opts.headless,
            main_world_eval: false,
            proxy: None,
            ..Default::default()
        })
        .await?;
        let warm_ms = elapsed_ms(t_warm);

        let warm_round_res: Result<(f64, f64, f64, f64)> = async {
            let t_nav = Instant::now();
            mgr.open(&warm_id, &catalog_url).await?;
            mgr.wait(
                &warm_id,
                WaitRequest {
                    idle: true,
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            )
            .await?;
            let nav_ms = elapsed_ms(t_nav);

            let t_snap = Instant::now();
            let snap = mgr.snapshot(&warm_id, SnapshotOptions::default()).await?;
            let snap_ms = elapsed_ms(t_snap);
            if snap.text.is_empty() {
                return Err(MimikyuError::Protocol {
                    code: -1,
                    message: "snapshot returned empty text during speed benchmark".to_string(),
                });
            }

            let t_ext = Instant::now();
            let md = mgr.extract(&warm_id, ExtractMode::Markdown).await?;
            let ext_ms = elapsed_ms(t_ext);
            if md["markdown"].as_str().unwrap_or("").is_empty() {
                return Err(MimikyuError::Protocol {
                    code: -1,
                    message: "extract(markdown) returned empty content".to_string(),
                });
            }

            // Prepare form page for batch timing
            mgr.open(&warm_id, &form_url).await?;
            mgr.wait(
                &warm_id,
                WaitRequest {
                    idle: true,
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            )
            .await?;
            let form_snap = mgr
                .snapshot(
                    &warm_id,
                    SnapshotOptions {
                        interactive_only: true,
                        ..Default::default()
                    },
                )
                .await?;
            let name_ref = workflows::find_ref(&form_snap, &["textbox"], "Applicant Name")?;
            let submit_ref = workflows::find_ref(&form_snap, &["button"], "Submit Application")?;

            let t_batch = Instant::now();
            let batch_report = mgr
                .batch(
                    &warm_id,
                    vec![
                        BatchAction::Fill {
                            target: name_ref,
                            value: "Bench User".to_string(),
                            humanize: Some(false),
                        },
                        BatchAction::Click {
                            target: submit_ref,
                            humanize: Some(false),
                            button: None,
                            click_count: None,
                        },
                        BatchAction::WaitIdle {
                            timeout_ms: Some(10_000),
                            network_quiet_ms: Some(100),
                            target: None,
                        },
                    ],
                )
                .await?;
            let batch_ms = elapsed_ms(t_batch);
            if !batch_report.ok {
                return Err(MimikyuError::Protocol {
                    code: -1,
                    message: format!("speed batch failed: {:?}", batch_report.error),
                });
            }

            Ok((nav_ms, snap_ms, ext_ms, batch_ms))
        }
        .await;

        let close_warm = mgr.close_session(&warm_id).await;
        let (nav_ms, snap_ms, ext_ms, batch_ms) = warm_round_res?;
        close_warm?;

        if measured {
            sample.cold_launch_ms.push(cold_ms);
            sample.warm_context_ms.push(warm_ms);
            sample.navigation_ms.push(nav_ms);
            sample.snapshot_ms.push(snap_ms);
            sample.extract_ms.push(ext_ms);
            sample.batch_ms.push(batch_ms);
            sample
                .iter_total_ms
                .push(cold_ms + warm_ms + nav_ms + snap_ms + ext_ms + batch_ms);
        }
    }

    if sample.memory_rss_bytes == 0 {
        sample.memory_rss_bytes = rss::measure_self_rss_bytes();
    }

    Ok(sample)
}

async fn run_benchmark_inner(
    opts: &BenchOptions,
    mgr: &SessionManager,
    harness: &HarnessServer,
) -> Result<BenchReport> {
    let total_start = Instant::now();

    let (rounds, warmup_for_speed) = match opts.suite {
        BenchSuite::All | BenchSuite::Speed => {
            let total = opts.warmup.checked_add(opts.iterations).ok_or_else(|| {
                MimikyuError::InvalidConfig(format!(
                    "bench warmup ({}) + iterations ({}) overflows u32",
                    opts.warmup, opts.iterations
                ))
            })?;
            (total, opts.warmup)
        }
        BenchSuite::Agentic => (1, 0),
    };

    let sample = run_speed_rounds(rounds, warmup_for_speed, opts, mgr, harness).await?;

    let tasks = match opts.suite {
        BenchSuite::All | BenchSuite::Agentic => {
            workflows::run_all_workflows(mgr, harness, opts.engine, opts.headless).await?
        }
        BenchSuite::Speed => Vec::new(),
    };

    let total_tasks = tasks.len();
    let completed_tasks = tasks.iter().filter(|t| t.ok && t.passed).count();
    let completion_rate = if total_tasks > 0 {
        (completed_tasks as f64) / (total_tasks as f64)
    } else {
        0.0
    };

    let raw_estimated_tokens = sample.raw_html_bytes.div_ceil(4).max(1);
    let snapshot_estimated_tokens = sample.snapshot_bytes.div_ceil(4).max(1);
    let token_compression_ratio =
        (raw_estimated_tokens as f64) / (snapshot_estimated_tokens as f64);

    let memory_rss_bytes = sample.memory_rss_bytes.max(1);
    let memory_rss_mb = (memory_rss_bytes as f64) / (1024.0 * 1024.0);

    Ok(BenchReport {
        iterations: opts.iterations,
        warmup: opts.warmup,
        engine: opts.engine,
        suite: opts.suite,
        cold_launch_ms: median_f64(&sample.cold_launch_ms),
        warm_context_ms: median_f64(&sample.warm_context_ms),
        navigation_ms: median_f64(&sample.navigation_ms),
        snapshot_ms: median_f64(&sample.snapshot_ms),
        extract_ms: median_f64(&sample.extract_ms),
        batch_ms: median_f64(&sample.batch_ms),
        avg_iteration_ms: mean_f64(&sample.iter_total_ms),
        median_iteration_ms: median_f64(&sample.iter_total_ms),
        total_ms: elapsed_ms(total_start),
        memory_rss_bytes,
        memory_rss_mb,
        raw_html_bytes: sample.raw_html_bytes,
        snapshot_bytes: sample.snapshot_bytes,
        raw_estimated_tokens,
        snapshot_estimated_tokens,
        token_compression_ratio,
        token_compression: token_compression_ratio,
        tasks,
        completed_tasks,
        total_tasks,
        completion_rate,
    })
}

pub fn format_bench_human(report: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "mimikyu bench (engine={}, suite={}, iterations={}, warmup={})\n",
        report.engine, report.suite, report.iterations, report.warmup
    ));
    out.push_str(&format!(
        "  speed (median ms): cold_launch={:.2}ms, warm_context={:.2}ms, nav={:.2}ms, snapshot={:.2}ms, extract={:.2}ms, batch={:.2}ms\n",
        report.cold_launch_ms,
        report.warm_context_ms,
        report.navigation_ms,
        report.snapshot_ms,
        report.extract_ms,
        report.batch_ms,
    ));
    out.push_str(&format!(
        "  iteration summary: avg={:.2}ms, median={:.2}ms, total={:.2}ms\n",
        report.avg_iteration_ms, report.median_iteration_ms, report.total_ms
    ));
    out.push_str(&format!(
        "  memory rss: {:.2} MB ({} bytes)\n",
        report.memory_rss_mb, report.memory_rss_bytes
    ));
    out.push_str(&format!(
        "  token compression: {:.2}x (raw {} bytes / {} est. tokens -> snapshot {} bytes / {} est. tokens)\n",
        report.token_compression_ratio,
        report.raw_html_bytes,
        report.raw_estimated_tokens,
        report.snapshot_bytes,
        report.snapshot_estimated_tokens
    ));
    out.push_str(&format!(
        "  agentic workflows: {}/{} passed (rate={:.1}%)\n",
        report.completed_tasks,
        report.total_tasks,
        report.completion_rate * 100.0
    ));
    for t in &report.tasks {
        let status = if t.ok && t.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "    - [{}] {} ({}) steps={} latency={:.2}ms ({})\n",
            status, t.id, t.name, t.steps, t.latency_ms, t.detail
        ));
    }
    out
}
