use std::collections::BTreeSet;

use mimikyu_core::bench::{run_benchmark, BenchOptions, BenchReport, BenchSuite};
use mimikyu_core::driver::EngineKind;
use mimikyu_core::error::MimikyuError;
use serde_json::Value;

fn assert_valid_report(report: &BenchReport, expected_tasks: usize) {
    assert!(report.cold_launch_ms.is_finite() && report.cold_launch_ms > 0.0);
    assert!(report.warm_context_ms.is_finite() && report.warm_context_ms > 0.0);
    assert!(report.navigation_ms.is_finite() && report.navigation_ms > 0.0);
    assert!(report.snapshot_ms.is_finite() && report.snapshot_ms > 0.0);
    assert!(report.extract_ms.is_finite() && report.extract_ms > 0.0);
    assert!(report.batch_ms.is_finite() && report.batch_ms > 0.0);
    assert!(report.avg_iteration_ms.is_finite() && report.avg_iteration_ms > 0.0);
    assert!(report.median_iteration_ms.is_finite() && report.median_iteration_ms > 0.0);
    assert!(report.total_ms.is_finite() && report.total_ms > 0.0);

    assert!(report.memory_rss_bytes > 0);
    assert!(report.memory_rss_mb.is_finite() && report.memory_rss_mb > 0.0);

    assert!(report.raw_html_bytes > report.snapshot_bytes);
    assert!(report.raw_estimated_tokens > report.snapshot_estimated_tokens);
    assert!(report.token_compression_ratio.is_finite() && report.token_compression_ratio > 1.0);
    assert!(report.token_compression.is_finite() && report.token_compression > 1.0);

    assert_eq!(report.tasks.len(), expected_tasks);
    assert_eq!(report.total_tasks, expected_tasks);
    if expected_tasks > 0 {
        assert_eq!(report.completed_tasks, expected_tasks);
        assert!((report.completion_rate - 1.0).abs() < 1e-9);
        for task in &report.tasks {
            assert!(
                task.ok && task.passed,
                "task {} ({}) failed: {}",
                task.id,
                task.name,
                task.detail
            );
            assert!(task.steps > 0, "task {} had 0 steps", task.id);
            assert!(
                task.latency_ms.is_finite() && task.latency_ms > 0.0,
                "task {} invalid latency {}",
                task.id,
                task.latency_ms
            );
        }
    } else {
        assert_eq!(report.completed_tasks, 0);
        assert_eq!(report.completion_rate, 0.0);
    }
}

#[tokio::test]
async fn bench_validation_rejects_zero_iterations_and_invalid_suite_and_cli_args() {
    let err = run_benchmark(BenchOptions {
        iterations: 0,
        ..Default::default()
    })
    .await
    .expect_err("iterations=0 must fail");
    assert!(matches!(err, MimikyuError::InvalidConfig(_)));

    assert!(matches!(
        BenchOptions::from_cli(0, 0, EngineKind::Chromium, "all"),
        Err(MimikyuError::InvalidConfig(_))
    ));
    assert!(matches!(
        BenchOptions::from_cli(-2, 0, EngineKind::Chromium, "all"),
        Err(MimikyuError::InvalidConfig(_))
    ));
    assert!(matches!(
        BenchOptions::from_cli(1, -1, EngineKind::Chromium, "all"),
        Err(MimikyuError::InvalidConfig(_))
    ));
    assert!(matches!(
        BenchOptions::from_cli(1, 0, EngineKind::Chromium, "invalid_suite"),
        Err(MimikyuError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn bench_chromium_all_suite_speed_rss_compression_and_5_tasks() {
    let report = run_benchmark(BenchOptions {
        iterations: 1,
        warmup: 0,
        engine: EngineKind::Chromium,
        suite: BenchSuite::All,
        headless: true,
        state_dir: None,
    })
    .await
    .expect("chromium all bench");

    assert_eq!(report.engine, EngineKind::Chromium);
    assert_eq!(report.suite, BenchSuite::All);
    assert_eq!(report.iterations, 1);
    assert_eq!(report.warmup, 0);
    assert_valid_report(&report, 5);
}

#[tokio::test]
async fn bench_firefox_all_suite_speed_rss_compression_and_5_tasks() {
    let report = run_benchmark(BenchOptions {
        iterations: 1,
        warmup: 0,
        engine: EngineKind::Firefox,
        suite: BenchSuite::All,
        headless: true,
        state_dir: None,
    })
    .await
    .expect("firefox all bench");

    assert_eq!(report.engine, EngineKind::Firefox);
    assert_eq!(report.suite, BenchSuite::All);
    assert_valid_report(&report, 5);
}

#[tokio::test]
async fn bench_reproducibility_identical_json_keys_and_task_ids() {
    let opts = BenchOptions {
        iterations: 1,
        warmup: 0,
        engine: EngineKind::Chromium,
        suite: BenchSuite::All,
        headless: true,
        state_dir: None,
    };
    let r1 = run_benchmark(opts.clone()).await.expect("run 1");
    let r2 = run_benchmark(opts).await.expect("run 2");

    let v1 = serde_json::to_value(&r1).unwrap();
    let v2 = serde_json::to_value(&r2).unwrap();
    let keys1: BTreeSet<String> = v1.as_object().unwrap().keys().cloned().collect();
    let keys2: BTreeSet<String> = v2.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys1, keys2);

    let ids1: Vec<&str> = r1.tasks.iter().map(|t| t.id.as_str()).collect();
    let ids2: Vec<&str> = r2.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids1.len(), 5);
    assert_eq!(ids1, ids2);
}

#[tokio::test]
async fn bench_suite_speed_and_agentic_modes() {
    let speed = run_benchmark(BenchOptions {
        iterations: 1,
        warmup: 0,
        engine: EngineKind::Chromium,
        suite: BenchSuite::Speed,
        headless: true,
        state_dir: None,
    })
    .await
    .expect("speed suite");
    assert_valid_report(&speed, 0);

    let agentic = run_benchmark(BenchOptions {
        iterations: 1,
        warmup: 0,
        engine: EngineKind::Chromium,
        suite: BenchSuite::Agentic,
        headless: true,
        state_dir: None,
    })
    .await
    .expect("agentic suite");
    assert_valid_report(&agentic, 5);

    let vs: Value = serde_json::to_value(&speed).unwrap();
    let va: Value = serde_json::to_value(&agentic).unwrap();
    let ks: BTreeSet<String> = vs.as_object().unwrap().keys().cloned().collect();
    let ka: BTreeSet<String> = va.as_object().unwrap().keys().cloned().collect();
    assert_eq!(ks, ka, "top-level JSON keys must match across suites");
}

#[tokio::test]
async fn bench_teardown_leaves_no_temp_dirs_in_explicit_parent() {
    let parent = tempfile::tempdir().expect("parent tmp");
    let custom_state = parent.path().join("bench-state");
    let report = run_benchmark(BenchOptions {
        iterations: 1,
        warmup: 0,
        engine: EngineKind::Chromium,
        suite: BenchSuite::Speed,
        headless: true,
        state_dir: Some(custom_state.clone()),
    })
    .await
    .expect("bench with custom state dir");
    assert_valid_report(&report, 0);
}
