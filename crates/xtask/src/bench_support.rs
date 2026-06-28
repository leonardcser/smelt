use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde_json::Value;

pub(crate) struct CargoTestBenchmark {
    pub(crate) package: &'static str,
    pub(crate) test_filter: &'static str,
    pub(crate) release: bool,
    pub(crate) features: &'static [&'static str],
    pub(crate) env: Vec<(String, String)>,
    pub(crate) bench_name: &'static str,
}

pub(crate) fn run_cargo_test_benchmark(bench: CargoTestBenchmark) {
    let executable = build_test_binary(&bench).unwrap_or_else(|err| {
        eprintln!("{}: failed to build test binary: {err}", bench.bench_name);
        std::process::exit(1);
    });

    let status = run_test_binary(&bench, executable).unwrap_or_else(|err| {
        eprintln!("{}: failed to run test binary: {err}", bench.bench_name);
        std::process::exit(1);
    });
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn build_test_binary(bench: &CargoTestBenchmark) -> Result<PathBuf, String> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "-p",
        bench.package,
        "--no-run",
        "--message-format=json",
    ]);
    if bench.release {
        cmd.arg("--release");
    }
    if !bench.features.is_empty() {
        cmd.arg("--features").arg(bench.features.join(","));
    }
    cmd.arg(bench.test_filter);

    let output = cmd
        .output()
        .map_err(|err| format!("spawn cargo test --no-run: {err}"))?;
    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        return Err(format!("cargo test --no-run exited with {}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut package_executable = None;
    let mut package_lib_executable = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(executable) = value.get("executable").and_then(Value::as_str) else {
            continue;
        };
        let package_id = value
            .get("package_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !package_id.contains(bench.package) {
            continue;
        }
        let path = PathBuf::from(executable);
        package_executable = Some(path.clone());
        if value
            .get("target")
            .and_then(|target| target.get("kind"))
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("lib")))
        {
            package_lib_executable = Some(path);
        }
    }

    package_lib_executable
        .or(package_executable)
        .ok_or_else(|| {
            format!(
                "cargo did not report a test executable for {}",
                bench.package
            )
        })
}

fn run_test_binary(
    bench: &CargoTestBenchmark,
    executable: PathBuf,
) -> Result<std::process::ExitStatus, String> {
    eprintln!(
        "running {} benchmark binary: {}",
        bench.bench_name,
        executable.display()
    );
    if std::path::Path::new("/usr/bin/time").is_file() {
        run_test_binary_with_time(bench, executable)
    } else {
        run_test_binary_with_proc_sampler(bench, executable)
    }
}

fn run_test_binary_with_time(
    bench: &CargoTestBenchmark,
    executable: PathBuf,
) -> Result<std::process::ExitStatus, String> {
    let time_output = unique_time_output_path(bench.bench_name);
    let mut cmd = Command::new("/usr/bin/time");
    cmd.args(["-v", "-o"])
        .arg(&time_output)
        .arg(&executable)
        .args([
            bench.test_filter,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
    for (key, value) in &bench.env {
        cmd.env(key, value);
    }

    let status = cmd
        .status()
        .map_err(|err| format!("spawn /usr/bin/time for {}: {err}", executable.display()))?;
    let memory = read_time_memory_summary(&time_output).unwrap_or_default();
    let _ = std::fs::remove_file(&time_output);
    print_memory_summary(bench.bench_name, &memory);
    Ok(status)
}

fn run_test_binary_with_proc_sampler(
    bench: &CargoTestBenchmark,
    executable: PathBuf,
) -> Result<std::process::ExitStatus, String> {
    let mut cmd = Command::new(&executable);
    cmd.args([
        bench.test_filter,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    for (key, value) in &bench.env {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("spawn {}: {err}", executable.display()))?;
    let pid = child.id();
    let mut memory = MemorySummary::default();
    loop {
        if let Some(sample) = read_memory_sample(pid) {
            memory.observe(sample);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|err| format!("wait for {}: {err}", executable.display()))?
        {
            print_memory_summary(bench.bench_name, &memory);
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn unique_time_output_path(bench_name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "smelt-{bench_name}-time-{}-{nanos}.txt",
        std::process::id()
    ))
}

fn read_time_memory_summary(path: &std::path::Path) -> Option<MemorySummary> {
    let output = std::fs::read_to_string(path).ok()?;
    let max_rss = output.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix("Maximum resident set size (kbytes):")
            .and_then(|value| value.trim().parse::<u64>().ok())
    })?;
    Some(MemorySummary {
        samples: 1,
        peak_rss_kb: max_rss,
        peak_hwm_kb: max_rss,
        peak_anon_kb: 0,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct MemorySample {
    rss_kb: u64,
    hwm_kb: u64,
    anon_kb: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct MemorySummary {
    samples: u64,
    peak_rss_kb: u64,
    peak_hwm_kb: u64,
    peak_anon_kb: u64,
}

impl MemorySummary {
    fn observe(&mut self, sample: MemorySample) {
        self.samples += 1;
        self.peak_rss_kb = self.peak_rss_kb.max(sample.rss_kb);
        self.peak_hwm_kb = self.peak_hwm_kb.max(sample.hwm_kb);
        self.peak_anon_kb = self.peak_anon_kb.max(sample.anon_kb);
    }
}

fn read_memory_sample(pid: u32) -> Option<MemorySample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut sample = MemorySample::default();
    for line in status.lines() {
        if let Some(value) = status_kb(line, "VmRSS:") {
            sample.rss_kb = value;
        } else if let Some(value) = status_kb(line, "VmHWM:") {
            sample.hwm_kb = value;
        } else if let Some(value) = status_kb(line, "RssAnon:") {
            sample.anon_kb = value;
        }
    }
    Some(sample)
}

fn status_kb(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?.trim();
    rest.split_whitespace().next()?.parse().ok()
}

fn print_memory_summary(bench_name: &str, memory: &MemorySummary) {
    if memory.samples == 0 {
        eprintln!("BENCH_MEMORY_SUMMARY bench={bench_name} unavailable=true");
        eprintln!(
            "BENCH_MEMORY_JSON {{\"type\":\"memory_summary\",\"bench\":\"{bench_name}\",\"available\":false}}"
        );
        return;
    }
    eprintln!(
        "BENCH_MEMORY_SUMMARY bench={} peak_rss_kb={} peak_hwm_kb={} peak_anon_kb={} samples={}",
        bench_name, memory.peak_rss_kb, memory.peak_hwm_kb, memory.peak_anon_kb, memory.samples
    );
    eprintln!(
        "BENCH_MEMORY_JSON {{\"type\":\"memory_summary\",\"bench\":\"{}\",\"available\":true,\"peak_rss_kb\":{},\"peak_hwm_kb\":{},\"peak_anon_kb\":{},\"samples\":{}}}",
        bench_name,
        memory.peak_rss_kb,
        memory.peak_hwm_kb,
        memory.peak_anon_kb,
        memory.samples
    );
}
