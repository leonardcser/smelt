//! `smelt upgrade`: check for and install newer smelt builds.
//!
//! The stable channel follows GitHub's `releases/latest` redirect. It points
//! at the newest published, non-prerelease app release (crate releases opt out
//! of "latest") and costs no API quota. An install downloads the release
//! archive, verifies it against the release's `SHA256SUMS`, runs the extracted
//! binary once, and atomically swaps it into place. A running smelt keeps
//! executing its old image; the next launch uses the new one.
//!
//! The unstable channel compares the build commit with `main` and rebuilds
//! with `cargo install`.
//!
//! `--json` prints one [`Report`] line on stdout for the bundled upgrade
//! plugin. Sessions started before an upgrade run the new binary's CLI, so
//! keep its fields backward compatible (see `json_report_keeps_its_field_names`).

use clap::{Args, Subcommand, ValueEnum};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const WEB_URL: &str = "https://github.com/leonardcser/smelt";
const API_URL: &str = "https://api.github.com/repos/leonardcser/smelt";
const REPO_URL: &str = "https://github.com/leonardcser/smelt.git";
const STAGING_PREFIX: &str = ".smelt-upgrade-";
/// Staging directories older than this belong to an interrupted upgrade.
const STALE_STAGING_AGE: Duration = Duration::from_secs(60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const SELF_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Args)]
pub struct UpgradeArgs {
    /// Upgrade channel to check or install from
    #[arg(long, value_enum, default_value_t = Channel::Stable, global = true)]
    channel: Channel,
    /// Print one JSON status line on stdout instead of text
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<UpgradeCommand>,
}

#[derive(Debug, Clone, Subcommand)]
enum UpgradeCommand {
    /// Check for updates without installing
    Check,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Channel {
    Stable,
    Unstable,
}

/// Outcome of a check or an install.
#[derive(Debug, Serialize)]
struct Report {
    channel: Channel,
    /// Identity of the binary that produced this report, as `--version` prints it.
    current: String,
    /// Newest build on the channel: a release tag or `main@<sha>`.
    latest: String,
    /// Whether `latest` is newer than `current`.
    update: bool,
    /// Whether the update may install without asking: a release build
    /// replacing itself with a prebuilt stable release.
    auto_install: bool,
    /// Build that this run installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    installed: Option<String>,
}

/// The running binary.
struct Build {
    display: String,
    version: Version,
    release: bool,
    sha: Option<String>,
    target: Option<String>,
}

impl Build {
    fn current() -> Self {
        Self::from_parts(
            env!("CARGO_PKG_VERSION"),
            tui::BUILD_TAG,
            tui::DISPLAY,
            tui::BUILD_SHA,
            tui::BUILD_TARGET,
        )
    }

    fn from_parts(version: &str, tag: &str, display: &str, sha: &str, target: &str) -> Self {
        let release = version != tui::SOURCE_BUILD_VERSION;
        // Source builds keep the manifest placeholder; their nearest release
        // tag tells which release they contain.
        let version = if release { version } else { tag };
        Self {
            display: display.to_string(),
            version: parse_version(version).unwrap_or(Version::new(0, 0, 0)),
            release,
            sha: known(sha).map(str::to_string),
            target: known(target).map(str::to_string),
        }
    }
}

/// Where releases and source live. Tests point this at a local server.
struct Source {
    web: String,
    api: String,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            web: WEB_URL.to_string(),
            api: API_URL.to_string(),
        }
    }
}

pub async fn run_upgrade_command(args: UpgradeArgs) {
    let client = smelt_provider::http_client_builder()
        .user_agent(concat!("smelt-upgrade/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| smelt_provider::http_client());
    let source = Source::default();
    let build = Build::current();
    let result = match args.command {
        Some(UpgradeCommand::Check) => check(&client, &source, &build, args.channel).await,
        None => match std::env::current_exe() {
            Ok(exe) => upgrade(&client, &source, &build, args.channel, &exe).await,
            Err(err) => Err(format!("cannot locate the smelt executable: {err}")),
        },
    };
    match result {
        Ok(report) if args.json => println!(
            "{}",
            serde_json::to_string(&report).expect("report serializes")
        ),
        Ok(report) => print_report(&report, args.command.is_some()),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

fn print_report(report: &Report, check_only: bool) {
    if let Some(installed) = &report.installed {
        println!("upgraded to {installed}; restart smelt to use it");
    } else if !check_only && !report.update {
        println!("smelt is up to date ({})", report.current);
    } else {
        let channel = match report.channel {
            Channel::Stable => "stable",
            Channel::Unstable => "unstable",
        };
        println!("channel: {channel}");
        println!("current: {}", report.current);
        println!("latest:  {}", report.latest);
        let status = if report.update {
            "update available"
        } else {
            "up to date"
        };
        println!("status:  {status}");
    }
}

async fn check(
    client: &reqwest::Client,
    source: &Source,
    build: &Build,
    channel: Channel,
) -> Result<Report, String> {
    let (latest, update) = match channel {
        Channel::Stable => {
            let tag = latest_stable_tag(client, &source.web).await?;
            let update = parse_version(&tag).is_some_and(|latest| latest > build.version);
            (tag, update)
        }
        Channel::Unstable => unstable_status(client, &source.api, build.sha.as_deref()).await?,
    };
    Ok(Report {
        channel,
        current: build.display.clone(),
        latest,
        update,
        auto_install: update && channel == Channel::Stable && build.release,
        installed: None,
    })
}

async fn upgrade(
    client: &reqwest::Client,
    source: &Source,
    build: &Build,
    channel: Channel,
    exe: &Path,
) -> Result<Report, String> {
    let mut report = check(client, source, build, channel).await?;
    if !report.update {
        return Ok(report);
    }
    match channel {
        Channel::Stable => {
            let target = build.target.as_deref().ok_or_else(|| {
                "smelt build target is unknown; cannot pick a release asset".to_string()
            })?;
            install_release(client, &source.web, &report.latest, target, exe).await?;
        }
        Channel::Unstable => install_main(&report.latest)?,
    }
    report.installed = Some(report.latest.clone());
    Ok(report)
}

async fn latest_stable_tag(client: &reqwest::Client, web: &str) -> Result<String, String> {
    let response = client
        .head(format!("{web}/releases/latest"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("github returned {}", response.status()));
    }
    release_tag(response.url().path())
        .ok_or_else(|| "github has no stable smelt release".to_string())
}

/// Tag of a `/releases/tag/vX.Y.Z` page path. Prereleases and tags of other
/// release series are rejected.
fn release_tag(path: &str) -> Option<String> {
    let (_, tag) = path.trim_end_matches('/').rsplit_once("/releases/tag/")?;
    let version = Version::parse(tag.strip_prefix('v')?).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then(|| tag.to_string())
}

#[derive(Deserialize)]
struct Commit {
    sha: String,
}

#[derive(Deserialize)]
struct Comparison {
    status: String,
}

/// `(latest, update)` for the `main` branch.
async fn unstable_status(
    client: &reqwest::Client,
    api: &str,
    local_sha: Option<&str>,
) -> Result<(String, bool), String> {
    let local_sha = local_sha.ok_or_else(|| {
        "the unstable channel needs the build commit; this binary was built without git".to_string()
    })?;
    let head: Commit = github_json(client, &format!("{api}/commits/main"))
        .await?
        .ok_or_else(|| "github has no `main` branch for smelt".to_string())?;
    let latest = format!("main@{}", short_sha(&head.sha));
    if head.sha.starts_with(local_sha) {
        return Ok((latest, false));
    }
    // GitHub reports `head` relative to `base`: `ahead` means main has commits
    // this build lacks. `diverged` means the build also has commits main lacks,
    // and a missing commit means it was never pushed; keep that work.
    let comparison: Option<Comparison> =
        github_json(client, &format!("{api}/compare/{local_sha}...{}", head.sha)).await?;
    let update = comparison.is_some_and(|comparison| comparison.status == "ahead");
    Ok((latest, update))
}

/// GET a GitHub API resource. `None` when it does not exist.
async fn github_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<T>, String> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .map_err(|e| format!("github request failed: {e}"))?
        .json()
        .await
        .map(Some)
        .map_err(|e| format!("github response was not valid JSON: {e}"))
}

async fn install_release(
    client: &reqwest::Client,
    web: &str,
    tag: &str,
    target: &str,
    exe: &Path,
) -> Result<(), String> {
    // Staging next to the executable keeps the final rename on one
    // filesystem, and fails before any download when the directory is not
    // writable.
    let staging = Staging::create(exe)?;
    let asset = format!("smelt-{target}.tar.gz");
    let base = format!("{web}/releases/download/{tag}");
    eprintln!("downloading {asset} from {tag}...");
    let sums = download(client, &format!("{base}/SHA256SUMS"), REQUEST_TIMEOUT).await?;
    let expected = checksum_for(&String::from_utf8_lossy(&sums), &asset)
        .ok_or_else(|| format!("SHA256SUMS of {tag} has no entry for {asset}"))?;
    let archive = download(client, &format!("{base}/{asset}"), DOWNLOAD_TIMEOUT).await?;
    verify_checksum(&archive, &expected).map_err(|actual| {
        format!("{asset} checksum mismatch: expected {expected}, got {actual}")
    })?;
    let candidate = staging.unpack(&archive)?;
    self_check(&candidate, tag).await?;
    eprintln!("installing to {}...", exe.display());
    staging.install(exe, &candidate)
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("download failed: {e}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    Ok(bytes.to_vec())
}

/// Checksum listed for `file` in `sha256sum` output.
fn checksum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == file).then(|| hash.to_ascii_lowercase())
    })
}

/// `Err` carries the actual checksum.
fn verify_checksum(data: &[u8], expected: &str) -> Result<(), String> {
    let actual = Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual == expected {
        Ok(())
    } else {
        Err(actual)
    }
}

/// Run the candidate once so a binary that cannot start on this machine, or
/// reports another version, never replaces a working one.
async fn self_check(candidate: &Path, tag: &str) -> Result<(), String> {
    let output = tokio::time::timeout(
        SELF_CHECK_TIMEOUT,
        tokio::process::Command::new(candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "downloaded smelt did not answer `--version`".to_string())?
    .map_err(|e| format!("downloaded smelt does not run on this machine: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.split_whitespace().any(|word| word == tag) {
        Ok(())
    } else {
        Err(format!(
            "downloaded smelt failed its self-check: expected {tag}, got `{}` ({})",
            stdout.trim(),
            output.status
        ))
    }
}

/// Private directory next to the executable. Removed on drop unless it must
/// outlive the process (Windows keeps the replaced executable there).
struct Staging {
    root: PathBuf,
    keep: bool,
}

impl Staging {
    fn create(exe: &Path) -> Result<Self, String> {
        let parent = exe.parent().ok_or_else(|| {
            format!(
                "current executable has no parent directory: {}",
                exe.display()
            )
        })?;
        for attempt in 0..100u32 {
            let root = parent.join(format!("{STAGING_PREFIX}{}-{attempt}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => return Ok(Self { root, keep: false }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(format!(
                        "cannot write to {}, where smelt is installed: {error}",
                        parent.display()
                    ));
                }
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

    /// Extract the executable from a release archive.
    fn unpack(&self, archive: &[u8]) -> Result<PathBuf, String> {
        let archive_path = self.root.join("release.tar.gz");
        fs::write(&archive_path, archive).map_err(|e| format!("write release archive: {e}"))?;
        let executable_name = format!("smelt{}", std::env::consts::EXE_SUFFIX);
        run(
            "tar",
            &[
                "-xzf",
                path_str(&archive_path)?,
                "-C",
                path_str(&self.root)?,
                &executable_name,
            ],
            "extract release archive",
        )?;
        let candidate = self.root.join(executable_name);
        validate_candidate(&candidate)?;
        Ok(candidate)
    }

    fn install(mut self, exe: &Path, candidate: &Path) -> Result<(), String> {
        let backup = self
            .root
            .join(format!("previous-smelt{}", std::env::consts::EXE_SUFFIX));
        match replace_executable(exe, candidate, &backup, |from, to| fs::rename(from, to)) {
            Ok(keep) => {
                self.keep = keep;
                Ok(())
            }
            Err(error) => {
                self.keep = error.rollback_failed;
                Err(error.message)
            }
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Remove staging directories left by interrupted upgrades, and on Windows the
/// replaced executables of finished ones.
pub fn cleanup_stale_staging() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let Some(parent) = executable.parent() else {
        return;
    };
    cleanup_stale_staging_in(parent, &executable, STALE_STAGING_AGE);
}

fn cleanup_stale_staging_in(parent: &Path, executable: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    // Install directories like `/usr/bin` are large: match names before
    // touching metadata.
    let staging = entries.flatten().filter(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(STAGING_PREFIX))
    });
    for entry in staging {
        let path = entry.path();
        let is_stale = entry.file_type().is_ok_and(|kind| kind.is_dir())
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age >= max_age));
        if is_stale && !executable.starts_with(&path) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn validate_candidate(candidate: &Path) -> Result<(), String> {
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

/// Swap `candidate` in at `executable`. Returns whether `backup` must outlive
/// this process.
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

fn install_main(latest: &str) -> Result<(), String> {
    eprintln!("building {latest} with cargo install; this may take a few minutes...");
    run(
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
        "cargo install",
    )
}

/// Run a helper program. Its output goes to stderr so stdout stays reserved
/// for the report.
fn run(program: &str, args: &[&str], label: &str) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
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

fn parse_version(value: &str) -> Option<Version> {
    let value = value.trim();
    Version::parse(value.strip_prefix('v').unwrap_or(value)).ok()
}

fn known(value: &str) -> Option<&str> {
    (!value.is_empty() && value != "unknown").then_some(value)
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn release_build(version: &str) -> Build {
        Build::from_parts(
            version,
            &format!("v{version}"),
            &format!("v{version}"),
            "abc1234",
            "x86_64-unknown-linux-gnu",
        )
    }

    fn source(server: &MockServer) -> Source {
        Source {
            web: server.uri(),
            api: server.uri(),
        }
    }

    async fn serve_latest_release(server: &MockServer, tag: &str) {
        Mock::given(method("HEAD"))
            .and(path("/releases/latest"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("/releases/tag/{tag}").as_str()),
            )
            .mount(server)
            .await;
        Mock::given(method("HEAD"))
            .and(path(format!("/releases/tag/{tag}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    #[test]
    fn release_tag_accepts_only_stable_app_releases() {
        assert_eq!(
            release_tag("/leonardcser/smelt/releases/tag/v0.6.2").as_deref(),
            Some("v0.6.2")
        );
        assert_eq!(
            release_tag("/releases/tag/v1.10.0/").as_deref(),
            Some("v1.10.0")
        );
        for path in [
            "/leonardcser/smelt/releases",
            "/releases/tag/v0.7.0-alpha.1",
            "/releases/tag/smelt-term-v0.7.0",
            "/releases/tag/0.6.2",
            "/releases/tag/v0.6",
            "/releases/tag/vgarbage",
        ] {
            assert_eq!(release_tag(path), None, "accepted `{path}`");
        }
    }

    /// Older sessions' upgrade plugins parse this JSON from newer binaries.
    /// Only add fields; renaming or removing one breaks them silently.
    #[test]
    fn json_report_keeps_its_field_names() {
        let report = Report {
            channel: Channel::Stable,
            current: "v0.6.2".into(),
            latest: "v0.6.3".into(),
            update: true,
            auto_install: true,
            installed: Some("v0.6.3".into()),
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "channel": "stable",
                "current": "v0.6.2",
                "latest": "v0.6.3",
                "update": true,
                "auto_install": true,
                "installed": "v0.6.3",
            })
        );
    }

    #[test]
    fn versions_compare_by_semver_precedence() {
        let newer = |a: &str, b: &str| parse_version(a).unwrap() > parse_version(b).unwrap();
        assert!(newer("v0.6.10", "0.6.9"));
        assert!(newer("0.7.0", "0.7.0-alpha.10"));
        assert!(newer("0.7.0-alpha.10", "0.7.0-alpha.9"));
        assert!(!newer("v0.6.2", "0.6.2"));
    }

    #[test]
    fn source_builds_compare_by_their_release_tag_and_never_auto_install() {
        let build = Build::from_parts(
            tui::SOURCE_BUILD_VERSION,
            "v0.6.2",
            "v0.6.2-5-abc1234",
            "abc1234",
            "x86_64-unknown-linux-gnu",
        );
        assert!(!build.release);
        assert_eq!(build.version, Version::new(0, 6, 2));

        let untagged =
            Build::from_parts(tui::SOURCE_BUILD_VERSION, "unknown", "v0.0.0-dev", "", "");
        assert_eq!(untagged.version, Version::new(0, 0, 0));
        assert_eq!(untagged.sha, None);
        assert_eq!(untagged.target, None);
    }

    #[test]
    fn checksums_match_by_exact_file_name() {
        let sums = "AAAA  smelt-aarch64-apple-darwin.tar.gz\n\
                    bbbb *smelt-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "smelt-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("aaaa")
        );
        assert_eq!(
            checksum_for(sums, "smelt-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("bbbb")
        );
        assert_eq!(checksum_for(sums, "smelt-x86_64-apple-darwin.tar.gz"), None);
        assert_eq!(checksum_for(sums, "apple-darwin.tar.gz"), None);

        let digest = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(verify_checksum(b"hello", digest), Ok(()));
        assert_eq!(verify_checksum(b"hellO", digest).unwrap_err().len(), 64);
    }

    #[tokio::test]
    async fn stable_check_follows_the_latest_release_redirect() {
        let server = MockServer::start().await;
        serve_latest_release(&server, "v0.6.3").await;
        let client = smelt_provider::http_client();

        let report = check(
            &client,
            &source(&server),
            &release_build("0.6.2"),
            Channel::Stable,
        )
        .await
        .unwrap();
        assert_eq!(report.latest, "v0.6.3");
        assert!(report.update);
        assert!(report.auto_install);

        let report = check(
            &client,
            &source(&server),
            &release_build("0.6.3"),
            Channel::Stable,
        )
        .await
        .unwrap();
        assert!(!report.update);
        assert!(!report.auto_install);
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "channel": "stable",
                "current": "v0.6.3",
                "latest": "v0.6.3",
                "update": false,
                "auto_install": false,
            })
        );
    }

    #[tokio::test]
    async fn stable_check_reports_missing_releases_and_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/releases/latest"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/releases"))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/releases"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = smelt_provider::http_client();
        let error = check(
            &client,
            &source(&server),
            &release_build("0.6.2"),
            Channel::Stable,
        )
        .await
        .unwrap_err();
        assert!(error.contains("no stable smelt release"), "{error}");

        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let error = check(
            &client,
            &source(&server),
            &release_build("0.6.2"),
            Channel::Stable,
        )
        .await
        .unwrap_err();
        assert!(error.contains("503"), "{error}");
    }

    #[tokio::test]
    async fn unstable_check_offers_main_only_when_it_is_ahead() {
        let head = "def5678000000000000000000000000000000000";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/commits/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "sha": head })),
            )
            .mount(&server)
            .await;
        for (base, status) in [("behind1", "ahead"), ("diverge", "diverged")] {
            Mock::given(method("GET"))
                .and(path(format!("/compare/{base}...{head}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "status": status })),
                )
                .mount(&server)
                .await;
        }
        let client = smelt_provider::http_client();
        let api = server.uri();

        let status = |sha: &'static str| {
            let client = client.clone();
            let api = api.clone();
            async move { unstable_status(&client, &api, Some(sha)).await.unwrap() }
        };
        assert_eq!(status("behind1").await, ("main@def5678".to_string(), true));
        assert_eq!(status("diverge").await, ("main@def5678".to_string(), false));
        assert_eq!(status("def5678").await, ("main@def5678".to_string(), false));
        // Never pushed: GitHub does not know the commit.
        assert_eq!(status("local00").await, ("main@def5678".to_string(), false));

        let error = unstable_status(&client, &api, None).await.unwrap_err();
        assert!(error.contains("built without git"), "{error}");
    }

    #[cfg(unix)]
    mod install {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        const TARGET: &str = "x86_64-unknown-linux-gnu";

        fn executable(path: &Path, contents: &str) {
            fs::write(path, contents).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// A release archive whose `smelt` prints `version_output`.
        fn release_archive(version_output: &str) -> Vec<u8> {
            let dir = tempfile::tempdir().unwrap();
            executable(
                &dir.path().join("smelt"),
                &format!("#!/bin/sh\necho 'smelt {version_output}'\n"),
            );
            fs::write(dir.path().join("LICENSE"), "MIT").unwrap();
            let archive = dir.path().join("release.tar.gz");
            let status = Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(dir.path())
                .args(["smelt", "LICENSE"])
                .status()
                .unwrap();
            assert!(status.success());
            fs::read(archive).unwrap()
        }

        fn sha256_hex(data: &[u8]) -> String {
            verify_checksum(data, "").unwrap_err()
        }

        async fn serve_release(server: &MockServer, tag: &str, archive: &[u8], sum: &str) {
            serve_latest_release(server, tag).await;
            let asset = format!("smelt-{TARGET}.tar.gz");
            Mock::given(method("GET"))
                .and(path(format!("/releases/download/{tag}/SHA256SUMS")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(format!("{sum}  {asset}\n0000  smelt-other.tar.gz\n")),
                )
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/releases/download/{tag}/{asset}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.to_vec()))
                .mount(server)
                .await;
        }

        fn staging_dirs(dir: &Path) -> Vec<PathBuf> {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(STAGING_PREFIX))
                })
                .collect()
        }

        async fn upgrade_in(dir: &Path, server: &MockServer) -> Result<Report, String> {
            let client = smelt_provider::http_client();
            upgrade(
                &client,
                &source(server),
                &release_build("0.6.2"),
                Channel::Stable,
                &dir.join("smelt"),
            )
            .await
        }

        #[tokio::test]
        async fn installs_a_verified_release_atomically() {
            let server = MockServer::start().await;
            let archive = release_archive("v0.6.3");
            serve_release(&server, "v0.6.3", &archive, &sha256_hex(&archive)).await;
            let dir = tempfile::tempdir().unwrap();
            executable(&dir.path().join("smelt"), "old");

            let report = upgrade_in(dir.path(), &server).await.unwrap();

            assert_eq!(report.installed.as_deref(), Some("v0.6.3"));
            let installed = fs::read_to_string(dir.path().join("smelt")).unwrap();
            assert!(installed.contains("smelt v0.6.3"), "{installed}");
            assert!(staging_dirs(dir.path()).is_empty());
        }

        #[tokio::test]
        async fn rejects_releases_that_fail_verification() {
            let good = release_archive("v0.6.3");
            // Archives embed file times, so build each one once.
            let wrong_version = release_archive("v9.9.9");
            let cases = [
                (
                    "checksum",
                    good.clone(),
                    "0".repeat(64),
                    "checksum mismatch",
                ),
                (
                    "version",
                    wrong_version.clone(),
                    sha256_hex(&wrong_version),
                    "self-check",
                ),
                (
                    "archive",
                    b"not a tarball".to_vec(),
                    sha256_hex(b"not a tarball"),
                    "extract",
                ),
            ];
            for (case, archive, sum, expected) in cases {
                let server = MockServer::start().await;
                serve_release(&server, "v0.6.3", &archive, &sum).await;
                let dir = tempfile::tempdir().unwrap();
                executable(&dir.path().join("smelt"), "old");

                let error = upgrade_in(dir.path(), &server).await.unwrap_err();

                assert!(error.contains(expected), "{case}: {error}");
                assert_eq!(fs::read_to_string(dir.path().join("smelt")).unwrap(), "old");
                assert!(staging_dirs(dir.path()).is_empty(), "{case}");
            }
        }

        #[tokio::test]
        async fn up_to_date_builds_do_not_download() {
            let server = MockServer::start().await;
            serve_latest_release(&server, "v0.6.2").await;
            let dir = tempfile::tempdir().unwrap();
            executable(&dir.path().join("smelt"), "old");

            let report = upgrade_in(dir.path(), &server).await.unwrap();

            assert!(!report.update);
            assert_eq!(report.installed, None);
            assert_eq!(fs::read_to_string(dir.path().join("smelt")).unwrap(), "old");
        }

        #[tokio::test]
        async fn unwritable_install_directories_fail_before_downloading() {
            let server = MockServer::start().await;
            serve_latest_release(&server, "v0.6.3").await;
            let dir = tempfile::tempdir().unwrap();
            executable(&dir.path().join("smelt"), "old");
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
            if fs::create_dir(dir.path().join("probe")).is_ok() {
                // Running as root: permissions are not enforced.
                return;
            }

            let error = upgrade_in(dir.path(), &server).await.unwrap_err();

            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(error.contains("cannot write to"), "{error}");
            let downloads = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|request| request.url.path().contains("/download/"))
                .count();
            assert_eq!(downloads, 0);
        }

        #[test]
        fn replacement_is_atomic_and_removes_the_previous_binary() {
            let dir = tempfile::tempdir().unwrap();
            let current = dir.path().join("current-smelt");
            let candidate = dir.path().join("candidate-smelt");
            let backup = dir.path().join("previous-smelt");
            executable(&current, "old");
            executable(&candidate, "new");

            replace_executable(&current, &candidate, &backup, |from, to| {
                assert_eq!(fs::read_to_string(&current).unwrap(), "old");
                assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
                assert_eq!(from, candidate);
                assert_eq!(to, current);
                fs::rename(from, to)
            })
            .unwrap();

            assert_eq!(fs::read_to_string(&current).unwrap(), "new");
            assert!(!candidate.exists());
            assert!(!backup.exists());
        }

        #[test]
        fn failed_install_keeps_the_previous_binary_untouched() {
            let dir = tempfile::tempdir().unwrap();
            let current = dir.path().join("current-smelt");
            let candidate = dir.path().join("candidate-smelt");
            let backup = dir.path().join("previous-smelt");
            executable(&current, "old");
            executable(&candidate, "new");
            let mut calls = 0;

            let error = replace_executable(&current, &candidate, &backup, |_, _| {
                calls += 1;
                Err(std::io::Error::other("injected install failure"))
            })
            .unwrap_err();

            assert_eq!(calls, 1);
            assert!(!error.rollback_failed);
            assert!(error.message.contains("current executable was not changed"));
            assert_eq!(fs::read_to_string(&current).unwrap(), "old");
            assert_eq!(fs::read_to_string(&candidate).unwrap(), "new");
            assert!(!backup.exists());
        }

        #[test]
        fn backup_creation_failure_does_not_touch_either_executable() {
            let dir = tempfile::tempdir().unwrap();
            let current = dir.path().join("current-smelt");
            let candidate = dir.path().join("candidate-smelt");
            let backup = dir.path().join("previous-smelt");
            executable(&current, "old");
            executable(&candidate, "new");
            executable(&backup, "occupied");
            let mut rename_called = false;

            let error = replace_executable(&current, &candidate, &backup, |_, _| {
                rename_called = true;
                Ok(())
            })
            .unwrap_err();

            assert!(!rename_called);
            assert!(!error.rollback_failed);
            assert!(error.message.contains("back up current executable"));
            assert_eq!(fs::read_to_string(&current).unwrap(), "old");
            assert_eq!(fs::read_to_string(&candidate).unwrap(), "new");
            assert_eq!(fs::read_to_string(&backup).unwrap(), "occupied");
        }

        #[test]
        fn candidate_validation_rejects_empty_non_executable_and_symlink_entries() {
            use std::os::unix::fs::symlink;

            let dir = tempfile::tempdir().unwrap();
            let candidate = dir.path().join("smelt");
            fs::write(&candidate, []).unwrap();
            assert!(validate_candidate(&candidate).is_err());

            fs::write(&candidate, b"binary").unwrap();
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(validate_candidate(&candidate).is_err());

            fs::remove_file(&candidate).unwrap();
            let target = dir.path().join("target");
            executable(&target, "binary");
            symlink(&target, &candidate).unwrap();
            assert!(validate_candidate(&candidate).is_err());
        }
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

        let keep = replace_executable(&current, &candidate, &backup, |from, to| {
            fs::rename(from, to)
        })
        .unwrap();

        assert!(keep);
        assert_eq!(fs::read(&current).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
    }

    #[test]
    fn stale_cleanup_removes_only_old_inactive_staging_directories() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(format!("{STAGING_PREFIX}1-0"));
        let active = dir.path().join(format!("{STAGING_PREFIX}2-0"));
        let unrelated = dir.path().join("keep-me");
        for path in [&stale, &active, &unrelated] {
            fs::create_dir(path).unwrap();
        }
        let executable = active.join("previous-smelt");
        fs::write(&executable, b"running").unwrap();

        cleanup_stale_staging_in(dir.path(), &executable, STALE_STAGING_AGE);
        assert!(
            stale.exists(),
            "fresh staging may belong to a running upgrade"
        );

        cleanup_stale_staging_in(dir.path(), &executable, Duration::ZERO);
        assert!(!stale.exists());
        assert!(active.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn staging_is_removed_on_drop_unless_kept() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("smelt");
        fs::write(&executable, b"current").unwrap();
        let root = {
            let staging = Staging::create(&executable).unwrap();
            fs::write(staging.root.join("partial"), b"partial").unwrap();
            staging.root.clone()
        };
        assert!(!root.exists());

        let root = {
            let mut staging = Staging::create(&executable).unwrap();
            staging.keep = true;
            staging.root.clone()
        };
        assert!(root.exists());
    }
}
