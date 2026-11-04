//! Black-box command benchmark for this project and an installed fatresize.
//!
//! Compile with:
//! rustc -O benchmarks/compare.rs -o target/fatresize-compare
//!
//! Run with:
//! target/fatresize-compare target/release/fatresize /usr/sbin/fatresize IMAGE 50 48Mi

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !(3..=6).contains(&arguments.len()) {
        eprintln!(
            "usage: compare RUST_BINARY ORIGINAL_BINARY IMAGE [ITERATIONS] [RESIZE_SIZE] [PARTITION]"
        );
        std::process::exit(2);
    }
    let iterations = arguments
        .get(3)
        .map_or(Ok(30), |value| value.parse::<usize>())
        .expect("ITERATIONS must be a positive integer");
    assert!(iterations > 0, "ITERATIONS must be positive");

    println!("iterations: {iterations}");
    let partition = arguments.get(5).map(String::as_str);
    if let Some(partition) = partition {
        println!("partition: {partition}");
    }
    benchmark("help: Rust", &arguments[0], &["--help"], iterations);
    benchmark("help: original", &arguments[1], &["--help"], iterations);

    let mut rust_info = vec!["--info", "--quiet"];
    let mut original_info = vec!["-i"];
    if let Some(partition) = partition {
        rust_info.extend(["--partition", partition]);
        original_info.extend(["-n", partition]);
    }
    rust_info.push(&arguments[2]);
    original_info.push(&arguments[2]);
    benchmark("info: Rust", &arguments[0], &rust_info, iterations);
    benchmark("info: original", &arguments[1], &original_info, iterations);
    benchmark(
        "check: Rust",
        &arguments[0],
        &["--check", "--quiet", &arguments[2]],
        iterations,
    );
    if let Some(size) = arguments.get(4) {
        let resize_iterations = iterations.min(10);
        benchmark_resize(
            "resize: Rust",
            &arguments[0],
            &arguments[2],
            size,
            resize_iterations,
            false,
            partition,
        );
        benchmark_resize(
            "resize: original",
            &arguments[1],
            &arguments[2],
            size,
            resize_iterations,
            true,
            partition,
        );
    }
}

fn benchmark(label: &str, program: &str, arguments: &[&str], iterations: usize) {
    for _ in 0..3 {
        let _ = run(program, arguments);
    }

    let mut samples = Vec::with_capacity(iterations);
    let mut failures = 0;
    for _ in 0..iterations {
        match run(program, arguments) {
            Ok(duration) => samples.push(duration),
            Err(()) => failures += 1,
        }
    }
    if samples.is_empty() {
        println!("{label}: all {failures} runs failed");
        return;
    }
    samples.sort_unstable();
    let total = samples.iter().sum::<Duration>();
    let mean = total / samples.len() as u32;
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    println!(
        "{label}: median={:.3} ms mean={:.3} ms p95={:.3} ms failures={failures}",
        milliseconds(median),
        milliseconds(mean),
        milliseconds(p95),
    );
}

fn run(program: &str, arguments: &[&str]) -> Result<Duration, ()> {
    let started = Instant::now();
    let status = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| ())?;
    let elapsed = started.elapsed();
    status.success().then_some(elapsed).ok_or(())
}

fn benchmark_resize(
    label: &str,
    program: &str,
    baseline: &str,
    size: &str,
    iterations: usize,
    original: bool,
    partition: Option<&str>,
) {
    let mut samples = Vec::with_capacity(iterations);
    let mut failures = 0;
    for iteration in 0..iterations {
        let stem = format!("fatresize-benchmark-{}-{iteration}", std::process::id());
        let image = std::env::temp_dir().join(format!("{stem}.img"));
        let recovery = std::env::temp_dir().join(format!("{stem}.recovery"));
        std::fs::copy(baseline, &image).expect("failed to copy benchmark image");
        let image_text = image.to_string_lossy();
        let recovery_text = recovery.to_string_lossy();
        let mut arguments = if original {
            vec!["-f", "-q", "-s", size]
        } else {
            vec!["--force-yes", "--quiet", "--size", size]
        };
        if let Some(partition) = partition {
            arguments.extend(if original {
                ["-n", partition]
            } else {
                ["--partition", partition]
            });
        }
        if !original {
            arguments.extend(["--backup", recovery_text.as_ref()]);
        }
        arguments.push(image_text.as_ref());
        match run(program, &arguments) {
            Ok(duration) => samples.push(duration),
            Err(()) => failures += 1,
        }
        let _ = std::fs::remove_file(image);
        let _ = std::fs::remove_file(recovery);
    }
    if samples.is_empty() {
        println!("{label}: all {failures} runs failed");
        return;
    }
    samples.sort_unstable();
    let total = samples.iter().sum::<Duration>();
    let mean = total / samples.len() as u32;
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    println!(
        "{label}: median={:.3} ms mean={:.3} ms p95={:.3} ms failures={failures}",
        milliseconds(median),
        milliseconds(mean),
        milliseconds(p95),
    );
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
