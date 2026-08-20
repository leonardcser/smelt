use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;
use std::fs;
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

#[derive(Clone, Debug, Deserialize)]
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
    check_stable_from_url(
        client,
        &url,
        env!("CARGO_PKG_VERSION"),
        tui::DISPLAY.to_string(),
    )
    .await
}

async fn check_stable_from_url(
    client: &reqwest::Client,
    url: &str,
    current_version: &str,
    current_display: String,
) -> Result<UpgradeInfo, String> {
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

    stable_upgrade_info(releases, current_version, current_display)
}

fn stable_upgrade_info(
    releases: Vec<Release>,
    current_version: &str,
    current_display: String,
) -> Result<UpgradeInfo, String> {
    let current = parse_version(current_version)
        .ok_or_else(|| format!("current smelt version is invalid: `{current_version}`"))?;
    let latest = releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| parse_version(&release.tag_name).map(|version| (release, version)))
        .max_by(|(_, a), (_, b)| compare_parsed_versions(a, b));
    let Some((latest, latest_version)) = latest else {
        return Err("github: no valid non-draft releases found".to_string());
    };

    let has_update = compare_parsed_versions(&latest_version, &current).is_gt();
    Ok(UpgradeInfo {
        channel: UpgradeChannel::Stable,
        current: current_display,
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
    let asset = stable_release_asset(tag, target);
    let exe = std::env::current_exe().map_err(|e| format!("current executable: {e}"))?;
    let mut staging = UpgradeStaging::create(&exe, tag)?;
    println!("downloading {tag} for {target} ({})...", asset.name);
    let candidate = prepare_stable_candidate(&staging, &asset.url, run_command)?;
    let backup = staging
        .root
        .join(format!("previous-smelt{}", std::env::consts::EXE_SUFFIX));

    println!("installing to {}...", exe.display());
    let defer_cleanup =
        replace_executable(&exe, &candidate, &backup, |from, to| fs::rename(from, to)).map_err(
            |error| {
                if error.rollback_failed {
                    staging.preserve = true;
                }
                error.message
            },
        )?;
    if defer_cleanup {
        staging.preserve = true;
    }
    println!("upgraded to {tag}; restart smelt to use it");
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct StableReleaseAsset {
    name: String,
    url: String,
}

fn stable_release_asset(tag: &str, target: &str) -> StableReleaseAsset {
    let name = format!("smelt-{target}.tar.gz");
    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{name}");
    StableReleaseAsset { name, url }
}

fn prepare_stable_candidate(
    staging: &UpgradeStaging,
    url: &str,
    mut run: impl FnMut(&str, &[&str], Option<&Path>, &str) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let archive = staging.root.join("release.tar.gz");
    let executable_name = format!("smelt{}", std::env::consts::EXE_SUFFIX);
    let candidate = staging.root.join(&executable_name);
    run(
        "curl",
        &["-fLso", path_str(&archive)?, url],
        None,
        "download release asset",
    )?;
    run(
        "tar",
        &[
            "-xzf",
            path_str(&archive)?,
            "-C",
            path_str(&staging.root)?,
            &executable_name,
        ],
        None,
        "extract release asset",
    )?;
    validate_upgrade_candidate(&candidate)?;
    Ok(candidate)
}

struct UpgradeStaging {
    root: PathBuf,
    preserve: bool,
}

impl UpgradeStaging {
    fn create(exe: &Path, tag: &str) -> Result<Self, String> {
        let parent = exe.parent().ok_or_else(|| {
            format!(
                "current executable has no parent directory: {}",
                exe.display()
            )
        })?;
        let safe_tag = sanitize_tag(tag);
        for attempt in 0..100u32 {
            let root = parent.join(format!(
                ".smelt-upgrade-{safe_tag}-{}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    return Ok(Self {
                        root,
                        preserve: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "create upgrade staging directory {}: {error}",
                        root.display()
                    ));
                }
            }
        }
        Err("could not allocate a unique upgrade staging directory".to_string())
    }
}

impl Drop for UpgradeStaging {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

pub fn cleanup_stale_staging() {
    #[cfg(windows)]
    {
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let Some(parent) = executable.parent() else {
            return;
        };
        cleanup_stale_staging_in(parent, &executable);
    }
}

#[cfg(any(windows, test))]
fn cleanup_stale_staging_in(parent: &Path, executable: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_staging_dir = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".smelt-upgrade-"))
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        let has_deferred_backup = path
            .join(format!("previous-smelt{}", std::env::consts::EXE_SUFFIX))
            .is_file();
        if is_staging_dir && has_deferred_backup && !executable.starts_with(&path) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn sanitize_tag(tag: &str) -> String {
    let safe = tag
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "release".to_string()
    } else {
        safe
    }
}

fn validate_upgrade_candidate(candidate: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(candidate)
        .map_err(|error| format!("release archive did not contain `smelt`: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "release archive `smelt` entry is not a regular file: {}",
            candidate.display()
        ));
    }
    if metadata.len() == 0 {
        return Err("release archive contained an empty `smelt` executable".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("release archive `smelt` entry is not executable".to_string());
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ReplaceExecutableError {
    message: String,
    rollback_failed: bool,
}

fn replace_executable(
    executable: &Path,
    candidate: &Path,
    backup: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<bool, ReplaceExecutableError> {
    #[cfg(unix)]
    {
        fs::hard_link(executable, backup).map_err(|error| ReplaceExecutableError {
            message: format!(
                "back up current executable {} to {}: {error}",
                executable.display(),
                backup.display()
            ),
            rollback_failed: false,
        })?;

        if let Err(install_error) = rename(candidate, executable) {
            return match fs::remove_file(backup) {
                Ok(()) => Err(ReplaceExecutableError {
                    message: format!(
                        "install replacement executable {}: {install_error}; current executable was not changed",
                        executable.display()
                    ),
                    rollback_failed: false,
                }),
                Err(cleanup_error) => Err(ReplaceExecutableError {
                    message: format!(
                        "install replacement executable {}: {install_error}; current executable was not changed; cleanup failed and its backup remains at {}: {cleanup_error}",
                        executable.display(),
                        backup.display()
                    ),
                    rollback_failed: true,
                }),
            };
        }
        fs::remove_file(backup).map_err(|error| ReplaceExecutableError {
            message: format!(
                "replacement installed but removing previous executable {} failed: {error}",
                backup.display()
            ),
            rollback_failed: true,
        })?;
        Ok(false)
    }

    #[cfg(not(unix))]
    {
        rename(executable, backup).map_err(|error| ReplaceExecutableError {
            message: format!(
                "move current executable {} to staging: {error}",
                executable.display()
            ),
            rollback_failed: false,
        })?;

        if let Err(install_error) = rename(candidate, executable) {
            return match rename(backup, executable) {
                Ok(()) => Err(ReplaceExecutableError {
                    message: format!(
                        "install replacement executable {}: {install_error}; restored previous executable",
                        executable.display()
                    ),
                    rollback_failed: false,
                }),
                Err(rollback_error) => Err(ReplaceExecutableError {
                    message: format!(
                        "install replacement executable {}: {install_error}; rollback failed: {rollback_error}; previous executable remains at {}",
                        executable.display(),
                        backup.display()
                    ),
                    rollback_failed: true,
                }),
            };
        }
        Ok(true)
    }
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

#[cfg(test)]
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    Some(compare_parsed_versions(
        &parse_version(a)?,
        &parse_version(b)?,
    ))
}

fn compare_parsed_versions(a: &ParsedVersion, b: &ParsedVersion) -> std::cmp::Ordering {
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

fn parse_version(value: &str) -> Option<ParsedVersion> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let (core, pre) = value
        .split_once('-')
        .map(|(core, pre)| (core, Some(pre.to_string())))
        .unwrap_or((value, None));
    if pre.as_deref().is_some_and(str::is_empty) {
        return None;
    }
    let components = core.split('.').collect::<Vec<_>>();
    if components.is_empty()
        || components.len() > 3
        || components.iter().any(|part| part.is_empty())
    {
        return None;
    }
    let mut parts = [0; 3];
    for (idx, part) in components.into_iter().enumerate() {
        parts[idx] = part.parse().ok()?;
    }
    Some(ParsedVersion { parts, pre })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag_name: &str, draft: bool) -> Release {
        Release {
            tag_name: tag_name.to_string(),
            draft,
        }
    }

    fn serve_http_once(
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}/releases"), handle)
    }

    #[test]
    fn compares_versions_with_v_prefix_and_prerelease() {
        assert!(compare_versions("v0.6.0", "0.5.0-alpha.5").unwrap().is_gt());
        assert!(compare_versions("0.6.0", "0.6.0-alpha.1").unwrap().is_gt());
        assert!(compare_versions("0.6.0-alpha.2", "0.6.0-alpha.1")
            .unwrap()
            .is_gt());
        assert!(compare_versions("0.6.0", "v0.6.0").unwrap().is_eq());
    }

    #[test]
    fn malformed_versions_are_rejected_instead_of_treated_as_zero() {
        for version in ["", "v", "1..2", "1.2.3.4", "one.2.3", "1.2.3-"] {
            assert!(parse_version(version).is_none(), "accepted `{version}`");
        }
    }

    #[test]
    fn stable_selection_ignores_drafts_and_malformed_tags() {
        let info = stable_upgrade_info(
            vec![
                release("garbage", false),
                release("v9.0.0", true),
                release("v0.6.0", false),
                release("v0.7.0-alpha.1", false),
            ],
            "0.5.0",
            "smelt 0.5.0".into(),
        )
        .unwrap();

        assert_eq!(info.next.as_deref(), Some("v0.7.0-alpha.1"));
        assert!(info.has_update);
        assert!(matches!(
            info.target,
            UpgradeTarget::Stable { ref tag } if tag == "v0.7.0-alpha.1"
        ));
    }

    #[test]
    fn stable_selection_rejects_responses_without_valid_releases() {
        let error = stable_upgrade_info(
            vec![release("garbage", false), release("v1.0.0", true)],
            "0.5.0",
            "smelt 0.5.0".into(),
        )
        .unwrap_err();
        assert!(error.contains("no valid non-draft releases"));
    }

    #[tokio::test]
    async fn stable_check_rejects_http_errors_and_malformed_json() {
        let client = reqwest::Client::new();
        let (url, server) = serve_http_once("503 Service Unavailable", "text/plain", "offline");
        let error = check_stable_from_url(&client, &url, "0.5.0", "smelt 0.5.0".into())
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(error.contains("503"), "unexpected error: {error}");

        let (url, server) = serve_http_once("200 OK", "application/json", "not-json");
        let error = check_stable_from_url(&client, &url, "0.5.0", "smelt 0.5.0".into())
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(
            error.contains("not valid JSON"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn stable_release_asset_uses_the_exact_tag_and_build_target() {
        let asset = stable_release_asset("v1.2.3", "x86_64-unknown-linux-musl");
        assert_eq!(asset.name, "smelt-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(
            asset.url,
            "https://github.com/leonardcser/smelt/releases/download/v1.2.3/smelt-x86_64-unknown-linux-musl.tar.gz"
        );
    }

    #[cfg(unix)]
    fn executable(path: &Path, contents: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stable_candidate_preparation_downloads_extracts_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-smelt");
        executable(&current, b"old");
        let staging = UpgradeStaging::create(&current, "v1.2.3").unwrap();
        let root = staging.root.clone();
        let mut commands = Vec::new();

        let candidate = prepare_stable_candidate(
            &staging,
            "https://example.test/smelt.tar.gz",
            |program, args, _, _| {
                commands.push(program.to_string());
                match program {
                    "curl" => fs::write(args[1], b"archive").map_err(|error| error.to_string()),
                    "tar" => {
                        executable(&root.join("smelt"), b"new");
                        Ok(())
                    }
                    _ => unreachable!(),
                }
            },
        )
        .unwrap();

        assert_eq!(commands, ["curl", "tar"]);
        assert_eq!(fs::read(candidate).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn stable_candidate_failures_remove_partial_downloads_and_extractions() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-smelt");
        executable(&current, b"old");

        let download_root = {
            let staging = UpgradeStaging::create(&current, "download-failure").unwrap();
            let root = staging.root.clone();
            let error =
                prepare_stable_candidate(&staging, "https://example.test/archive", |_, _, _, _| {
                    Err("injected download failure".into())
                })
                .unwrap_err();
            assert!(error.contains("download failure"));
            root
        };
        assert!(!download_root.exists());

        let archive_root = {
            let staging = UpgradeStaging::create(&current, "archive-failure").unwrap();
            let root = staging.root.clone();
            let error = prepare_stable_candidate(
                &staging,
                "https://example.test/archive",
                |program, args, _, _| match program {
                    "curl" => {
                        fs::write(args[1], b"partial archive").map_err(|error| error.to_string())
                    }
                    "tar" => Err("injected archive failure".into()),
                    _ => unreachable!(),
                },
            )
            .unwrap_err();
            assert!(error.contains("archive failure"));
            assert!(root.join("release.tar.gz").exists());
            root
        };
        assert!(!archive_root.exists());

        let invalid_shape_root = {
            let staging = UpgradeStaging::create(&current, "invalid-shape").unwrap();
            let root = staging.root.clone();
            let error = prepare_stable_candidate(
                &staging,
                "https://example.test/archive",
                |program, args, _, _| match program {
                    "curl" => fs::write(args[1], b"archive").map_err(|error| error.to_string()),
                    "tar" => Ok(()),
                    _ => unreachable!(),
                },
            )
            .unwrap_err();
            assert!(error.contains("did not contain `smelt`"));
            root
        };
        assert!(!invalid_shape_root.exists());
    }

    #[test]
    fn stale_upgrade_cleanup_removes_only_inactive_staging_directories() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(".smelt-upgrade-old-1-0");
        let active = dir.path().join(".smelt-upgrade-active-2-0");
        let unrelated = dir.path().join("keep-me");
        fs::create_dir(&stale).unwrap();
        fs::create_dir(&active).unwrap();
        fs::create_dir(&unrelated).unwrap();
        let backup_name = format!("previous-smelt{}", std::env::consts::EXE_SUFFIX);
        fs::write(stale.join(&backup_name), b"old").unwrap();
        let executable = active.join(backup_name);
        fs::write(&executable, b"running").unwrap();

        cleanup_stale_staging_in(dir.path(), &executable);

        assert!(!stale.exists());
        assert!(active.exists());
        assert!(unrelated.exists());
    }

    #[cfg(not(unix))]
    #[test]
    fn replacement_defers_running_binary_cleanup_until_next_launch() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-smelt.exe");
        let candidate = dir.path().join("candidate-smelt.exe");
        let backup = dir.path().join("previous-smelt.exe");
        fs::write(&current, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();

        let cleanup = replace_executable(&current, &candidate, &backup, |from, to| {
            fs::rename(from, to)
        })
        .unwrap();

        assert!(cleanup);
        assert_eq!(fs::read(&current).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn replacement_is_atomic_and_removes_the_previous_binary() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-smelt");
        let candidate = dir.path().join("candidate-smelt");
        let backup = dir.path().join("previous-smelt");
        executable(&current, b"old");
        executable(&candidate, b"new");
        validate_upgrade_candidate(&candidate).unwrap();

        replace_executable(&current, &candidate, &backup, |from, to| {
            assert_eq!(fs::read(&current).unwrap(), b"old");
            assert_eq!(fs::read(&backup).unwrap(), b"old");
            assert_eq!(from, candidate);
            assert_eq!(to, current);
            fs::rename(from, to)
        })
        .unwrap();

        assert_eq!(fs::read(&current).unwrap(), b"new");
        assert!(!candidate.exists());
        assert!(!backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_install_keeps_the_previous_binary_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-smelt");
        let candidate = dir.path().join("candidate-smelt");
        let backup = dir.path().join("previous-smelt");
        executable(&current, b"old");
        executable(&candidate, b"new");
        let mut calls = 0;

        let error = replace_executable(&current, &candidate, &backup, |_, _| {
            calls += 1;
            Err(std::io::Error::other("injected install failure"))
        })
        .unwrap_err();

        assert_eq!(calls, 1);
        assert!(!error.rollback_failed);
        assert!(error.message.contains("current executable was not changed"));
        assert_eq!(fs::read(&current).unwrap(), b"old");
        assert_eq!(fs::read(&candidate).unwrap(), b"new");
        assert!(!backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn backup_creation_failure_does_not_touch_either_executable() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-smelt");
        let candidate = dir.path().join("candidate-smelt");
        let backup = dir.path().join("previous-smelt");
        executable(&current, b"old");
        executable(&candidate, b"new");
        executable(&backup, b"occupied");
        let mut rename_called = false;

        let error = replace_executable(&current, &candidate, &backup, |_, _| {
            rename_called = true;
            Ok(())
        })
        .unwrap_err();

        assert!(!rename_called);
        assert!(!error.rollback_failed);
        assert!(error.message.contains("back up current executable"));
        assert_eq!(fs::read(&current).unwrap(), b"old");
        assert_eq!(fs::read(&candidate).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"occupied");
    }

    #[cfg(unix)]
    #[test]
    fn candidate_validation_rejects_empty_non_executable_and_symlink_entries() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("smelt");
        fs::write(&candidate, []).unwrap();
        assert!(validate_upgrade_candidate(&candidate).is_err());

        fs::write(&candidate, b"binary").unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_upgrade_candidate(&candidate).is_err());

        fs::remove_file(&candidate).unwrap();
        let target = dir.path().join("target");
        executable(&target, b"binary");
        symlink(&target, &candidate).unwrap();
        assert!(validate_upgrade_candidate(&candidate).is_err());
    }

    #[test]
    fn staging_cleanup_runs_on_drop_and_preserves_recovery_artifacts_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("smelt-bin");
        fs::write(&executable, b"current").unwrap();
        let root = {
            let staging = UpgradeStaging::create(&executable, "v1/beta").unwrap();
            fs::write(staging.root.join("partial"), b"partial").unwrap();
            staging.root.clone()
        };
        assert!(!root.exists());

        let root = {
            let mut staging = UpgradeStaging::create(&executable, "v1/beta").unwrap();
            fs::write(staging.root.join("previous-smelt"), b"current").unwrap();
            staging.preserve = true;
            staging.root.clone()
        };
        assert!(root.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
