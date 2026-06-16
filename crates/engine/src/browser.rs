use std::ffi::OsString;
use std::process::Stdio;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserOpenResult {
    Opened,
    Unavailable(&'static str),
    Failed(String),
}

impl BrowserOpenResult {
    pub fn opened(&self) -> bool {
        matches!(self, Self::Opened)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserStatus {
    can_open: bool,
    reason: Option<&'static str>,
}

impl BrowserStatus {
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
enum BrowserPlatform {
    Macos,
    Linux,
    Windows,
    Other,
}

impl BrowserPlatform {
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
struct BrowserCommand {
    program: &'static str,
    args: Vec<String>,
}

pub fn browser_status() -> BrowserStatus {
    browser_status_for(BrowserPlatform::current(), |name| std::env::var_os(name))
}

fn browser_status_for<F>(platform: BrowserPlatform, env: F) -> BrowserStatus
where
    F: Fn(&str) -> Option<OsString>,
{
    let has = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let has_graphical_display = has("DISPLAY") || has("WAYLAND_DISPLAY");
    let is_ssh = has("SSH_CONNECTION") || has("SSH_CLIENT") || has("SSH_TTY");

    if is_ssh && !has_graphical_display {
        return BrowserStatus {
            can_open: false,
            reason: Some("running over SSH without a graphical display"),
        };
    }

    match platform {
        BrowserPlatform::Linux if !has_graphical_display => BrowserStatus {
            can_open: false,
            reason: Some("DISPLAY or WAYLAND_DISPLAY is not set"),
        },
        BrowserPlatform::Macos | BrowserPlatform::Linux | BrowserPlatform::Windows => {
            BrowserStatus {
                can_open: true,
                reason: None,
            }
        }
        BrowserPlatform::Other => BrowserStatus {
            can_open: false,
            reason: Some("unsupported platform"),
        },
    }
}

fn browser_commands_for(platform: BrowserPlatform, url: &str) -> Vec<BrowserCommand> {
    match platform {
        BrowserPlatform::Macos => vec![BrowserCommand {
            program: "open",
            args: vec![url.to_string()],
        }],
        BrowserPlatform::Linux => vec![
            BrowserCommand {
                program: "xdg-open",
                args: vec![url.to_string()],
            },
            BrowserCommand {
                program: "open",
                args: vec![url.to_string()],
            },
        ],
        BrowserPlatform::Windows => vec![BrowserCommand {
            program: "cmd",
            args: vec![
                "/C".to_string(),
                "start".to_string(),
                "".to_string(),
                url.to_string(),
            ],
        }],
        BrowserPlatform::Other => Vec::new(),
    }
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
    let commands = browser_commands_for(BrowserPlatform::current(), url);
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
        "failed to launch browser: {}",
        last_err.unwrap_or_else(|| "no launcher available".to_string())
    ))
}

pub fn open_url_if_available(url: &str) -> BrowserOpenResult {
    let status = browser_status();
    open_url_with_status(url, status, open_url)
}

fn open_url_with_status<F>(url: &str, status: BrowserStatus, opener: F) -> BrowserOpenResult
where
    F: FnOnce(&str) -> Result<(), String>,
{
    if !status.can_open() {
        return BrowserOpenResult::Unavailable(
            status.reason().unwrap_or("browser auto-open unavailable"),
        );
    }
    match opener(url) {
        Ok(()) => BrowserOpenResult::Opened,
        Err(err) => BrowserOpenResult::Failed(err),
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
    fn browser_status_detects_linux_display_headless_and_ssh() {
        assert!(!browser_status_for(BrowserPlatform::Linux, env_with(&[])).can_open());
        assert_eq!(
            browser_status_for(BrowserPlatform::Linux, env_with(&[])).reason(),
            Some("DISPLAY or WAYLAND_DISPLAY is not set")
        );
        assert!(browser_status_for(
            BrowserPlatform::Linux,
            env_with(&[("WAYLAND_DISPLAY", "wayland-1")])
        )
        .can_open());

        let ssh = browser_status_for(
            BrowserPlatform::Linux,
            env_with(&[("SSH_CONNECTION", "client server")]),
        );
        assert!(!ssh.can_open());
        assert_eq!(
            ssh.reason(),
            Some("running over SSH without a graphical display")
        );
    }

    #[test]
    fn browser_status_allows_local_macos_and_windows() {
        assert!(browser_status_for(BrowserPlatform::Macos, env_with(&[])).can_open());
        assert!(browser_status_for(BrowserPlatform::Windows, env_with(&[])).can_open());
        assert!(!browser_status_for(BrowserPlatform::Other, env_with(&[])).can_open());
    }

    #[test]
    fn open_url_with_status_reports_unavailable_without_opening() {
        let result = open_url_with_status(
            "https://example.test",
            BrowserStatus::new(false, Some("headless")),
            |_| panic!("opener should not be called"),
        );

        assert_eq!(result, BrowserOpenResult::Unavailable("headless"));
    }

    #[test]
    fn open_url_with_status_reports_opened() {
        let result = open_url_with_status(
            "https://example.test",
            BrowserStatus::new(true, None),
            |url| {
                assert_eq!(url, "https://example.test");
                Ok(())
            },
        );

        assert_eq!(result, BrowserOpenResult::Opened);
    }

    #[test]
    fn open_url_with_status_reports_open_failure() {
        let result = open_url_with_status(
            "https://example.test",
            BrowserStatus::new(true, None),
            |_| Err("missing opener".to_string()),
        );

        assert_eq!(
            result,
            BrowserOpenResult::Failed("missing opener".to_string())
        );
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
    fn browser_commands_match_platform() {
        assert_eq!(
            browser_commands_for(BrowserPlatform::Macos, "https://example.test"),
            vec![BrowserCommand {
                program: "open",
                args: vec!["https://example.test".to_string()]
            }]
        );
        assert_eq!(
            browser_commands_for(BrowserPlatform::Linux, "https://example.test"),
            vec![
                BrowserCommand {
                    program: "xdg-open",
                    args: vec!["https://example.test".to_string()]
                },
                BrowserCommand {
                    program: "open",
                    args: vec!["https://example.test".to_string()]
                }
            ]
        );
        assert_eq!(
            browser_commands_for(BrowserPlatform::Windows, "https://example.test"),
            vec![BrowserCommand {
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
            browser_commands_for(BrowserPlatform::Other, "https://example.test"),
            Vec::<BrowserCommand>::new()
        );
    }
}
