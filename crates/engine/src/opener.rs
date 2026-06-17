use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenResult {
    Opened,
    Unavailable(&'static str),
    Failed(String),
}

impl OpenResult {
    pub fn opened(&self) -> bool {
        matches!(self, Self::Opened)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStatus {
    can_open: bool,
    reason: Option<&'static str>,
}

impl OpenStatus {
    pub fn can_open(&self) -> bool {
        self.can_open
    }

    pub fn reason(&self) -> Option<&'static str> {
        self.reason
    }

    #[cfg(test)]
    pub(crate) fn new(can_open: bool, reason: Option<&'static str>) -> Self {
        Self { can_open, reason }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenPlatform {
    Macos,
    Linux,
    Windows,
    Other,
}

impl OpenPlatform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenCommand {
    program: &'static str,
    args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOpenTarget {
    path: PathBuf,
    line: Option<u32>,
    col: Option<u32>,
}

impl FileOpenTarget {
    pub fn new(path: PathBuf, line: Option<u32>, col: Option<u32>) -> Self {
        Self { path, line, col }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn display_location(&self) -> String {
        match (self.line, self.col) {
            (Some(line), Some(col)) => format!("{}:{line}:{col}", self.path.display()),
            (Some(line), None) => format!("{}:{line}", self.path.display()),
            _ => self.path.display().to_string(),
        }
    }

    fn editor_location_arg(&self) -> Option<String> {
        let line = self.line?;
        let path = self.path.to_string_lossy();
        Some(match self.col {
            Some(col) => format!("{path}:{line}:{col}"),
            None => format!("{path}:{line}"),
        })
    }
}

impl From<PathBuf> for FileOpenTarget {
    fn from(path: PathBuf) -> Self {
        Self::new(path, None, None)
    }
}

impl From<&Path> for FileOpenTarget {
    fn from(path: &Path) -> Self {
        Self::new(path.to_path_buf(), None, None)
    }
}

pub fn open_status() -> OpenStatus {
    open_status_for(OpenPlatform::current(), |name| std::env::var_os(name))
}

fn open_status_for<F>(platform: OpenPlatform, env: F) -> OpenStatus
where
    F: Fn(&str) -> Option<OsString>,
{
    let has = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let has_graphical_display = has("DISPLAY") || has("WAYLAND_DISPLAY");
    let is_ssh = has("SSH_CONNECTION") || has("SSH_CLIENT") || has("SSH_TTY");
    let is_mosh = has("MOSH_IP") || has("MOSH_PORT");

    if (is_ssh || is_mosh) && !has_graphical_display {
        return OpenStatus {
            can_open: false,
            reason: Some("running over a remote terminal without a graphical display"),
        };
    }

    match platform {
        OpenPlatform::Linux if !has_graphical_display => OpenStatus {
            can_open: false,
            reason: Some("DISPLAY or WAYLAND_DISPLAY is not set"),
        },
        OpenPlatform::Macos | OpenPlatform::Linux | OpenPlatform::Windows => OpenStatus {
            can_open: true,
            reason: None,
        },
        OpenPlatform::Other => OpenStatus {
            can_open: false,
            reason: Some("unsupported platform"),
        },
    }
}

fn url_commands_for(platform: OpenPlatform, url: &str) -> Vec<OpenCommand> {
    match platform {
        OpenPlatform::Macos => vec![OpenCommand {
            program: "open",
            args: vec![url.to_string()],
        }],
        OpenPlatform::Linux => vec![
            OpenCommand {
                program: "xdg-open",
                args: vec![url.to_string()],
            },
            OpenCommand {
                program: "open",
                args: vec![url.to_string()],
            },
        ],
        OpenPlatform::Windows => vec![OpenCommand {
            program: "cmd",
            args: vec![
                "/C".to_string(),
                "start".to_string(),
                "".to_string(),
                url.to_string(),
            ],
        }],
        OpenPlatform::Other => Vec::new(),
    }
}

fn file_commands_for(platform: OpenPlatform, target: &FileOpenTarget) -> Vec<OpenCommand> {
    let path = target.path.to_string_lossy().to_string();
    let mut commands = Vec::new();
    if let Some(location) = target.editor_location_arg() {
        commands.push(OpenCommand {
            program: "code",
            args: vec!["-g".to_string(), location],
        });
    }
    commands.extend(match platform {
        OpenPlatform::Macos => vec![OpenCommand {
            program: "open",
            args: vec![path],
        }],
        OpenPlatform::Linux => vec![
            OpenCommand {
                program: "xdg-open",
                args: vec![path.clone()],
            },
            OpenCommand {
                program: "open",
                args: vec![path],
            },
        ],
        OpenPlatform::Windows => vec![OpenCommand {
            program: "cmd",
            args: vec!["/C".to_string(), "start".to_string(), "".to_string(), path],
        }],
        OpenPlatform::Other => Vec::new(),
    });
    commands
}

fn spawn_first(commands: Vec<OpenCommand>, label: &str) -> Result<(), String> {
    if commands.is_empty() {
        return Err("unsupported platform".to_string());
    }
    let mut last_err = None;
    for command in commands {
        match std::process::Command::new(command.program)
            .args(command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(err) => last_err = Some(format!("{}: {err}", command.program)),
        }
    }
    Err(format!(
        "failed to launch {label}: {}",
        last_err.unwrap_or_else(|| "no launcher available".to_string())
    ))
}

fn validate_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let allowed = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("file://");
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "open_url: refusing to open {url:?} (only http(s)/mailto/file schemes are allowed)"
        ))
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    validate_url(url)?;
    spawn_first(url_commands_for(OpenPlatform::current(), url), "browser")
}

pub fn open_file(target: &FileOpenTarget) -> Result<(), String> {
    if !target.path().is_file() {
        return Err(format!(
            "open_file: file does not exist: {}",
            target.path().display()
        ));
    }
    spawn_first(
        file_commands_for(OpenPlatform::current(), target),
        "file opener",
    )
}

pub fn open_url_if_available(url: &str) -> OpenResult {
    let status = open_status();
    open_url_with_status(url, status, open_url)
}

pub fn open_file_if_available(target: &FileOpenTarget) -> OpenResult {
    let status = open_status();
    open_file_with_status(target, status, open_file)
}

fn open_url_with_status<F>(url: &str, status: OpenStatus, opener: F) -> OpenResult
where
    F: FnOnce(&str) -> Result<(), String>,
{
    if !status.can_open() {
        return OpenResult::Unavailable(status.reason().unwrap_or("browser auto-open unavailable"));
    }
    match opener(url) {
        Ok(()) => OpenResult::Opened,
        Err(err) => OpenResult::Failed(err),
    }
}

fn open_file_with_status<F>(target: &FileOpenTarget, status: OpenStatus, opener: F) -> OpenResult
where
    F: FnOnce(&FileOpenTarget) -> Result<(), String>,
{
    if !status.can_open() {
        return OpenResult::Unavailable(status.reason().unwrap_or("file open unavailable"));
    }
    match opener(target) {
        Ok(()) => OpenResult::Opened,
        Err(err) => OpenResult::Failed(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn open_status_detects_linux_display_headless_and_ssh() {
        assert!(!open_status_for(OpenPlatform::Linux, env_with(&[])).can_open());
        assert_eq!(
            open_status_for(OpenPlatform::Linux, env_with(&[])).reason(),
            Some("DISPLAY or WAYLAND_DISPLAY is not set")
        );
        assert!(open_status_for(
            OpenPlatform::Linux,
            env_with(&[("WAYLAND_DISPLAY", "wayland-1")])
        )
        .can_open());

        let ssh = open_status_for(
            OpenPlatform::Linux,
            env_with(&[("SSH_CONNECTION", "client server")]),
        );
        assert!(!ssh.can_open());
        assert_eq!(
            ssh.reason(),
            Some("running over a remote terminal without a graphical display")
        );

        let mosh = open_status_for(OpenPlatform::Linux, env_with(&[("MOSH_IP", "host")]));
        assert!(!mosh.can_open());
        assert_eq!(
            mosh.reason(),
            Some("running over a remote terminal without a graphical display")
        );
    }

    #[test]
    fn open_status_allows_local_macos_and_windows() {
        assert!(open_status_for(OpenPlatform::Macos, env_with(&[])).can_open());
        assert!(open_status_for(OpenPlatform::Windows, env_with(&[])).can_open());
        assert!(!open_status_for(OpenPlatform::Other, env_with(&[])).can_open());
    }

    #[test]
    fn open_url_with_status_reports_unavailable_without_opening() {
        let result = open_url_with_status(
            "https://example.test",
            OpenStatus::new(false, Some("headless")),
            |_| panic!("opener should not be called"),
        );

        assert_eq!(result, OpenResult::Unavailable("headless"));
    }

    #[test]
    fn open_url_with_status_reports_opened() {
        let result =
            open_url_with_status("https://example.test", OpenStatus::new(true, None), |url| {
                assert_eq!(url, "https://example.test");
                Ok(())
            });

        assert_eq!(result, OpenResult::Opened);
    }

    #[test]
    fn open_url_with_status_reports_open_failure() {
        let result =
            open_url_with_status("https://example.test", OpenStatus::new(true, None), |_| {
                Err("missing opener".to_string())
            });

        assert_eq!(result, OpenResult::Failed("missing opener".to_string()));
    }

    #[test]
    fn validate_url_rejects_non_user_facing_schemes() {
        assert!(validate_url("https://example.test").is_ok());
        assert!(validate_url("http://example.test").is_ok());
        assert!(validate_url("mailto:test@example.test").is_ok());
        assert!(validate_url("file:///tmp/example.html").is_ok());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("-bad").is_err());
    }

    #[test]
    fn url_commands_match_platform() {
        assert_eq!(
            url_commands_for(OpenPlatform::Macos, "https://example.test"),
            vec![OpenCommand {
                program: "open",
                args: vec!["https://example.test".to_string()]
            }]
        );
        assert_eq!(
            url_commands_for(OpenPlatform::Linux, "https://example.test"),
            vec![
                OpenCommand {
                    program: "xdg-open",
                    args: vec!["https://example.test".to_string()]
                },
                OpenCommand {
                    program: "open",
                    args: vec!["https://example.test".to_string()]
                }
            ]
        );
        assert_eq!(
            url_commands_for(OpenPlatform::Windows, "https://example.test"),
            vec![OpenCommand {
                program: "cmd",
                args: vec![
                    "/C".to_string(),
                    "start".to_string(),
                    "".to_string(),
                    "https://example.test".to_string()
                ]
            }]
        );
        assert_eq!(
            url_commands_for(OpenPlatform::Other, "https://example.test"),
            Vec::<OpenCommand>::new()
        );
    }

    #[test]
    fn file_commands_include_editor_location_when_present() {
        let target = FileOpenTarget::new(PathBuf::from("src/lib.rs"), Some(12), Some(3));

        assert_eq!(
            file_commands_for(OpenPlatform::Linux, &target),
            vec![
                OpenCommand {
                    program: "code",
                    args: vec!["-g".to_string(), "src/lib.rs:12:3".to_string()]
                },
                OpenCommand {
                    program: "xdg-open",
                    args: vec!["src/lib.rs".to_string()]
                },
                OpenCommand {
                    program: "open",
                    args: vec!["src/lib.rs".to_string()]
                }
            ]
        );
    }
}
