//! Seatbelt: the macOS sandbox, applied by running the program under
//! `sandbox-exec`. Paths are passed as `-D` parameters so the profile never
//! needs path quoting. Later rules win over earlier ones.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Policy;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const BASE_PROFILE: &str = "(version 1)\n(allow default)\n";

/// Deny writes except to the devices a shell needs.
const WRITE_PROFILE: &str = r#"(deny file-write*)
(allow file-write*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/dtracehelper")
  (regex #"^/dev/tty")
  (regex #"^/dev/fd/")
  (regex #"^/dev/ttys[0-9]+$"))
"#;

/// `sandbox-exec`, which passed the probe.
#[derive(Debug)]
pub(crate) struct Seatbelt;

impl Seatbelt {
    /// Apply an empty profile once. `sandbox-exec` exists on every macOS, but it
    /// fails inside another sandbox, so only a real run proves it works.
    pub(crate) fn probe() -> Result<Self, String> {
        let status = Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| format!("cannot run {SANDBOX_EXEC}: {error}"))?;
        if status.success() {
            Ok(Self)
        } else {
            Err(format!(
            "{SANDBOX_EXEC} cannot apply a profile here; smelt may already run inside a sandbox"
        ))
        }
    }

    pub(crate) fn command(&self, program: &OsStr, policy: &Policy) -> Command {
        let mut profile = Profile::default();
        profile.text.push_str(BASE_PROFILE);
        if let Some(writable) = &policy.writable {
            profile.text.push_str(WRITE_PROFILE);
            for path in writable {
                profile.rule("allow file-write*", path);
            }
        }
        // Deeper paths come later, so they win over the paths they are inside.
        let mut paths: Vec<(&PathBuf, bool)> = policy
            .hidden
            .iter()
            .map(|path| (path, true))
            .chain(policy.visible.iter().map(|path| (path, false)))
            .collect();
        paths.sort_by_key(|(path, _)| path.components().count());
        for (path, hide) in paths {
            if hide {
                profile.rule("deny file-read* file-write*", path);
                continue;
            }
            profile.rule("allow file-read*", path);
            let writable = policy
                .writable
                .as_ref()
                .is_none_or(|writable| writable.iter().any(|root| path.starts_with(root)));
            if writable {
                profile.rule("allow file-write*", path);
            }
        }
        let mut command = Command::new(SANDBOX_EXEC);
        for define in profile.defines {
            command.arg("-D").arg(define);
        }
        command.arg("-p").arg(profile.text).arg(program);
        command
    }
}

/// A profile under construction and the `-D` parameters it refers to.
#[derive(Default)]
struct Profile {
    text: String,
    defines: Vec<OsString>,
}

impl Profile {
    /// Append `(<action> (subpath|literal (param …)))` for `path`.
    fn rule(&mut self, action: &str, path: &Path) {
        let name = format!("PATH_{}", self.defines.len());
        let filter = if path.is_dir() { "subpath" } else { "literal" };
        self.text
            .push_str(&format!("({action} ({filter} (param \"{name}\")))\n"));
        let mut define = OsString::from(format!("{name}="));
        define.push(path.as_os_str());
        self.defines.push(define);
    }
}
