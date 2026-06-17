use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use smelt_store::{benchmark_zstd_compression, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT};

const DEFAULT_MAX_SAMPLES: usize = 512;
const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

pub fn run(args: Vec<String>) {
    let options = Options::parse(args).unwrap_or_else(|message| {
        eprintln!("xtask bench-store-compression: {message}");
        print_usage();
        std::process::exit(2);
    });

    let samples = collect_samples(&options).unwrap_or_else(|err| {
        eprintln!("xtask bench-store-compression: {err}");
        std::process::exit(1);
    });

    if samples.is_empty() {
        eprintln!(
            "no request/history payload samples found under {}",
            options.state_dir.display()
        );
        std::process::exit(1);
    }

    let total_raw: usize = samples.iter().map(Vec::len).sum();
    println!("samples: {}", samples.len());
    println!("raw bytes: {total_raw}");

    for level in [1, 3] {
        let report = benchmark_zstd_compression(samples.iter().map(Vec::as_slice), level)
            .unwrap_or_else(|err| {
                eprintln!("zstd level {level} failed: {err}");
                std::process::exit(1);
            });
        println!(
            "zstd level {level}: {} bytes, ratio {}%, savings {}%, elapsed {:?}, gate {}",
            report.total_zstd_size,
            report.compression_ratio_percent(),
            report.savings_percent(),
            report.elapsed,
            if report.supports_policy(DEFAULT_ZSTD_MIN_SAVINGS_PERCENT) {
                "pass"
            } else {
                "fail"
            }
        );
    }
}

#[derive(Debug)]
struct Options {
    state_dir: PathBuf,
    max_samples: usize,
    max_bytes: usize,
}

impl Options {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut state_dir = None;
        let mut max_samples = DEFAULT_MAX_SAMPLES;
        let mut max_bytes = DEFAULT_MAX_BYTES;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--max-samples" => {
                    max_samples = parse_next(&mut iter, "--max-samples")?;
                }
                "--max-bytes" => {
                    max_bytes = parse_next(&mut iter, "--max-bytes")?;
                }
                _ if arg.starts_with('-') => return Err(format!("unknown flag `{arg}`")),
                _ => {
                    if state_dir.replace(PathBuf::from(&arg)).is_some() {
                        return Err("state dir provided more than once".into());
                    }
                }
            }
        }
        let state_dir = state_dir.ok_or_else(|| "missing state dir".to_string())?;
        Ok(Self {
            state_dir,
            max_samples,
            max_bytes,
        })
    }
}

fn parse_next(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    let value = iter
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?;
    value
        .parse()
        .map_err(|_| format!("{flag} value must be an integer"))
}

fn print_usage() {
    eprintln!(
        "usage: cargo xtask bench-store-compression STATE_DIR [--max-samples N] [--max-bytes N]"
    );
}

fn collect_samples(options: &Options) -> std::io::Result<Vec<Vec<u8>>> {
    let mut files = Vec::new();
    collect_jsonl_files(&options.state_dir, &mut files)?;

    let mut samples = Vec::new();
    let mut sampled_bytes = 0usize;
    for path in files {
        if samples.len() >= options.max_samples || sampled_bytes >= options.max_bytes {
            break;
        }
        sample_jsonl_file(&path, options, &mut samples, &mut sampled_bytes)?;
    }
    Ok(samples)
}

fn collect_jsonl_files(root: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(path) = queue.pop_front() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                queue.push_back(path);
            } else if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("requests.jsonl" | "history.jsonl")
            ) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(())
}

fn sample_jsonl_file(
    path: &Path,
    options: &Options,
    samples: &mut Vec<Vec<u8>>,
    sampled_bytes: &mut usize,
) -> std::io::Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let is_request_log = path.file_name().and_then(|name| name.to_str()) == Some("requests.jsonl");

    for line in reader.lines() {
        if samples.len() >= options.max_samples || *sampled_bytes >= options.max_bytes {
            break;
        }
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            push_sample(line.into_bytes(), options, samples, sampled_bytes);
            continue;
        };
        if is_request_log {
            collect_request_payloads(&value, options, samples, sampled_bytes);
        } else {
            collect_history_payloads(&value, options, samples, sampled_bytes);
        }
    }
    Ok(())
}

fn collect_request_payloads(
    value: &Value,
    options: &Options,
    samples: &mut Vec<Vec<u8>>,
    sampled_bytes: &mut usize,
) {
    for key in ["body", "messages", "tools", "system_prompt"] {
        if let Some(payload) = value.get(key) {
            push_json_sample(payload, options, samples, sampled_bytes);
        }
    }
}

fn collect_history_payloads(
    value: &Value,
    options: &Options,
    samples: &mut Vec<Vec<u8>>,
    sampled_bytes: &mut usize,
) {
    collect_history_payloads_inner(value, options, samples, sampled_bytes);
}

fn collect_history_payloads_inner(
    value: &Value,
    options: &Options,
    samples: &mut Vec<Vec<u8>>,
    sampled_bytes: &mut usize,
) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "metadata" || key == "metadata_json" || key.ends_with("_metadata") {
                    push_json_sample(value, options, samples, sampled_bytes);
                } else {
                    collect_history_payloads_inner(value, options, samples, sampled_bytes);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_history_payloads_inner(value, options, samples, sampled_bytes);
            }
        }
        _ => {}
    }
}

fn push_json_sample(
    value: &Value,
    options: &Options,
    samples: &mut Vec<Vec<u8>>,
    sampled_bytes: &mut usize,
) {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    push_sample(bytes, options, samples, sampled_bytes);
}

fn push_sample(
    bytes: Vec<u8>,
    options: &Options,
    samples: &mut Vec<Vec<u8>>,
    sampled_bytes: &mut usize,
) {
    if bytes.is_empty()
        || samples.len() >= options.max_samples
        || *sampled_bytes >= options.max_bytes
    {
        return;
    }
    *sampled_bytes += bytes.len();
    samples.push(bytes);
}
