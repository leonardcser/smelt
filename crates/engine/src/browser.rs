use std::ffi::OsString;
use std::process::Stdio;

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

fn browser_command_for(platform: BrowserPlatform, url: &str) -> Option<BrowserCommand> {
    match platform {
        BrowserPlatform::Macos => Some(BrowserCommand {
            program: "open",
            args: vec![url.to_string()],
        }),
        BrowserPlatform::Linux => Some(BrowserCommand {
            program: "xdg-open",
            args: vec![url.to_string()],
        }),
        BrowserPlatform::Windows => Some(BrowserCommand {
            program: "cmd",
            args: vec![
                "/C".to_string(),
                "start".to_string(),
                "".to_string(),
                url.to_string(),
            ],
        }),
        BrowserPlatform::Other => None,
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    let command = browser_command_for(BrowserPlatform::current(), url)
        .ok_or_else(|| "unsupported platform".to_string())?;
    std::process::Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch browser: {e}"))
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
    fn browser_command_matches_platform() {
        assert_eq!(
            browser_command_for(BrowserPlatform::Macos, "https://example.test"),
            Some(BrowserCommand {
                program: "open",
                args: vec!["https://example.test".to_string()]
            })
        );
        assert_eq!(
            browser_command_for(BrowserPlatform::Linux, "https://example.test"),
            Some(BrowserCommand {
                program: "xdg-open",
                args: vec!["https://example.test".to_string()]
            })
        );
        assert_eq!(
            browser_command_for(BrowserPlatform::Windows, "https://example.test"),
            Some(BrowserCommand {
                program: "cmd",
                args: vec![
                    "/C".to_string(),
                    "start".to_string(),
                    "".to_string(),
                    "https://example.test".to_string()
                ]
            })
        );
        assert_eq!(
            browser_command_for(BrowserPlatform::Other, "https://example.test"),
            None
        );
    }
}
