use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OWNER: &str = "leonardcser";
const REPO: &str = "smelt";
const REPO_URL: &str = "https://github.com/leonardcser/smelt.git";

#[derive(Debug, Clone, Args)]
pub struct UpgradeArgs {
    /// Upgrade channel to check or install from
    #[arg(long, value_enum, default_value_t = UpgradeChannel::Stable, global = true)]
    channel: UpgradeChannel,
    #[command(subcommand)]
    command: Option<UpgradeCommand>,
}

#[derive(Debug, Clone, Subcommand)]
enum UpgradeCommand {
    /// Check for updates without installing
    Check,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum UpgradeChannel {
    Stable,
    Unstable,
}

#[derive(Debug)]
struct UpgradeInfo {
    channel: UpgradeChannel,
    current: String,
    next: Option<String>,
    has_update: bool,
    target: UpgradeTarget,
}

#[derive(Debug)]
enum UpgradeTarget {
    Stable { tag: String },
    Unstable { sha: String },
    None,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
}

#[derive(Debug, Deserialize)]
struct CompareResponse {
    status: String,
    commits: Vec<CompareCommit>,
}

#[derive(Debug, Deserialize)]
struct CompareCommit {
    sha: String,
}

pub async fn run_upgrade_command(args: UpgradeArgs) {
    let client = reqwest::Client::builder()
        .user_agent(concat!("smelt-upgrade/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let info = match check_for_update(&client, args.channel).await {
        Ok(info) => info,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    if matches!(args.command, Some(UpgradeCommand::Check)) {
        print_check_result(&info);
        return;
    }

    if !info.has_update {
        println!("smelt is already up to date ({})", info.current);
        return;
    }

    print_check_result(&info);
    let result = match info.target {
        UpgradeTarget::Stable { tag } => install_stable(&tag),
        UpgradeTarget::Unstable { sha } => install_unstable(&sha),
        UpgradeTarget::None => Ok(()),
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn check_for_update(
    client: &reqwest::Client,
    channel: UpgradeChannel,
) -> Result<UpgradeInfo, String> {
    match channel {
        UpgradeChannel::Stable => check_stable(client).await,
        UpgradeChannel::Unstable => check_unstable(client).await,
    }
}

async fn check_stable(client: &reqwest::Client) -> Result<UpgradeInfo, String> {
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases?per_page=30");
    let releases: Vec<Release> = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("github request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("github response was not valid JSON: {e}"))?;

    let latest = releases
        .into_iter()
        .filter(|release| !release.draft)
        .max_by(|a, b| compare_versions(&a.tag_name, &b.tag_name));
    let Some(latest) = latest else {
        return Err("github: no releases found".to_string());
    };

    let current_version = env!("CARGO_PKG_VERSION");
    let has_update = compare_versions(&latest.tag_name, current_version).is_gt();
    Ok(UpgradeInfo {
        channel: UpgradeChannel::Stable,
        current: tui::DISPLAY.to_string(),
        next: Some(latest.tag_name.clone()),
        has_update,
        target: if has_update {
            UpgradeTarget::Stable {
                tag: latest.tag_name,
            }
        } else {
            UpgradeTarget::None
        },
    })
}

async fn check_unstable(client: &reqwest::Client) -> Result<UpgradeInfo, String> {
    let local_sha = optional_build_value(tui::BUILD_SHA).ok_or_else(|| {
        "unstable channel requires a build SHA; this binary was built without git".to_string()
    })?;
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/compare/{local_sha}...main");
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpgradeInfo {
            channel: UpgradeChannel::Unstable,
            current: tui::DISPLAY.to_string(),
            next: None,
            has_update: false,
            target: UpgradeTarget::None,
        });
    }
    let compare: CompareResponse = response
        .error_for_status()
        .map_err(|e| format!("github request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("github response was not valid JSON: {e}"))?;

    let head_sha = compare
        .commits
        .last()
        .map(|commit| commit.sha.clone())
        .filter(|sha| !sha.is_empty());
    let has_update = compare.status == "behind" && head_sha.is_some();
    Ok(UpgradeInfo {
        channel: UpgradeChannel::Unstable,
        current: tui::DISPLAY.to_string(),
        next: head_sha.as_deref().map(short_main_ref),
        has_update,
        target: if has_update {
            UpgradeTarget::Unstable {
                sha: head_sha.unwrap(),
            }
        } else {
            UpgradeTarget::None
        },
    })
}

fn print_check_result(info: &UpgradeInfo) {
    let channel = match info.channel {
        UpgradeChannel::Stable => "stable",
        UpgradeChannel::Unstable => "unstable",
    };
    println!("channel: {channel}");
    println!("current: {}", info.current);
    if let Some(next) = &info.next {
        println!("latest:  {next}");
    } else {
        println!("latest:  unknown");
    }
    println!(
        "status:  {}",
        if info.has_update {
            "update available"
        } else {
            "up to date"
        }
    );
}

fn install_stable(tag: &str) -> Result<(), String> {
    let target = optional_build_value(tui::BUILD_TARGET)
        .ok_or_else(|| "smelt build target is unknown; cannot pick a release asset".to_string())?;
    let exe = std::env::current_exe().map_err(|e| format!("current executable: {e}"))?;
    let dir = exe.parent().ok_or_else(|| {
        format!(
            "current executable has no parent directory: {}",
            exe.display()
        )
    })?;
    let tmp_tar = temp_tar_path(&exe, tag);
    let asset = format!("smelt-{target}.tar.gz");
    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{asset}");

    println!("downloading {tag} for {target}...");
    run_command(
        "curl",
        &["-fLso", path_str(&tmp_tar)?, &url],
        None,
        "download release asset",
    )?;

    println!("installing to {}...", exe.display());
    let tar_result = run_command(
        "tar",
        &["-xzf", path_str(&tmp_tar)?, "-C", path_str(dir)?, "smelt"],
        None,
        "extract release asset",
    );
    let _ = std::fs::remove_file(&tmp_tar);
    tar_result?;

    let extracted = dir.join("smelt");
    if extracted != exe {
        std::fs::rename(&extracted, &exe)
            .map_err(|e| format!("rename {} to {}: {e}", extracted.display(), exe.display()))?;
    }
    println!("upgraded to {tag}; restart smelt to use it");
    Ok(())
}

fn install_unstable(sha: &str) -> Result<(), String> {
    println!(
        "building main@{} via cargo install; this may take a few minutes...",
        short_sha(sha)
    );
    run_command(
        "cargo",
        &[
            "install",
            "--git",
            REPO_URL,
            "--branch",
            "main",
            "--package",
            "smelt-agent",
            "--force",
            "--locked",
        ],
        None,
        "cargo install",
    )?;
    println!(
        "upgraded to main@{}; restart smelt to use it",
        short_sha(sha)
    );
    Ok(())
}

fn run_command(
    program: &str,
    args: &[&str],
    current_dir: Option<&Path>,
    label: &str,
) -> Result<(), String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("{label}: failed to spawn {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label}: {program} exited with {status}"))
    }
}

fn temp_tar_path(exe: &Path, tag: &str) -> PathBuf {
    let mut safe_tag = String::new();
    for ch in tag.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            safe_tag.push(ch);
        } else {
            safe_tag.push('_');
        }
    }
    exe.with_file_name(format!(
        ".smelt-upgrade-{safe_tag}-{}.tar.gz",
        std::process::id()
    ))
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn optional_build_value(value: &str) -> Option<&str> {
    if value.is_empty() || value == "unknown" {
        None
    } else {
        Some(value)
    }
}

fn short_main_ref(sha: &str) -> String {
    format!("main@{}", short_sha(sha))
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a = parse_version(a);
    let b = parse_version(b);
    for idx in 0..3 {
        match a.parts[idx].cmp(&b.parts[idx]) {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    match (a.pre.as_deref(), b.pre.as_deref()) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    }
}

#[derive(Debug)]
struct ParsedVersion {
    parts: [u64; 3],
    pre: Option<String>,
}

fn parse_version(value: &str) -> ParsedVersion {
    let value = value.trim().trim_start_matches('v');
    let (core, pre) = value
        .split_once('-')
        .map(|(core, pre)| (core, Some(pre.to_string())))
        .unwrap_or((value, None));
    let mut parts = [0; 3];
    for (idx, part) in core.split('.').take(3).enumerate() {
        parts[idx] = part.parse().unwrap_or(0);
    }
    ParsedVersion { parts, pre }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_with_v_prefix_and_prerelease() {
        assert!(compare_versions("v0.6.0", "0.5.0-alpha.5").is_gt());
        assert!(compare_versions("0.6.0", "0.6.0-alpha.1").is_gt());
        assert!(compare_versions("0.6.0-alpha.2", "0.6.0-alpha.1").is_gt());
        assert!(compare_versions("0.6.0", "v0.6.0").is_eq());
    }
}
