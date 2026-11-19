//! bubblewrap: the Linux sandbox. `bwrap` runs the program in a new mount
//! namespace: `/` read-only (or read-write when writes are unrestricted),
//! the writable directories read-write, an unreadable empty directory over
//! each hidden directory, and `/dev/null` over each hidden file. Because
//! `bwrap` mounts it without devices, `/dev/null` there cannot be opened.
//! Deeper mounts come later, so they win over the paths they are inside.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Policy;

/// The `bwrap` binary that passed the probe.
#[derive(Debug)]
pub(crate) struct Bwrap {
    path: PathBuf,
}

impl Bwrap {
    /// Find `bwrap` on `PATH` and run it once. It fails where user
    /// namespaces are blocked, so only a real run proves it works.
    pub(crate) fn probe() -> Result<Self, String> {
        let path = std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join("bwrap"))
            .find(|path| path.is_file())
            .ok_or("bubblewrap (`bwrap`) is not installed")?;
        let output = Command::new(&path)
            .args(["--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run {}: {error}", path.display()))?;
        if output.status.success() {
            return Ok(Self { path });
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "bubblewrap cannot create a sandbox here, often because user namespaces are blocked: {}",
            stderr.trim()
        ))
    }

    pub(crate) fn command(&self, program: &OsStr, policy: &Policy) -> Command {
        let mut command = Command::new(&self.path);
        command.arg("--die-with-parent");
        let root = if policy.writable.is_some() {
            "--ro-bind"
        } else {
            "--bind"
        };
        command.args([root, "/", "/", "--dev-bind", "/dev", "/dev"]);
        for (path, mount) in mounts(policy) {
            match mount {
                Mount::Read => command.arg("--ro-bind").arg(path).arg(path),
                Mount::Write => command.arg("--bind").arg(path).arg(path),
                Mount::Hide if path.is_dir() => {
                    // Execute-only when something inside stays visible, so
                    // the command can still reach it.
                    let inside = policy
                        .visible
                        .iter()
                        .any(|visible| visible.starts_with(path));
                    let perms = if inside { "0111" } else { "0000" };
                    command.args(["--perms", perms, "--tmpfs"]).arg(path)
                }
                Mount::Hide => command.arg("--ro-bind").arg("/dev/null").arg(path),
            };
        }
        command.arg("--").arg(program);
        command
    }
}

/// What a command may do beneath a path. At the same depth a later variant
/// wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mount {
    Read,
    Write,
    Hide,
}

/// The mounts the policy needs, shallowest first. A mount that would not
/// change what its path inherits is left out: fewer mount points, and a
/// writable directory inside another one can still be renamed.
fn mounts(policy: &Policy) -> Vec<(&Path, Mount)> {
    let writable_beneath = |path: &Path| {
        policy
            .writable
            .as_ref()
            .is_none_or(|writable| writable.iter().any(|dir| path.starts_with(dir)))
    };
    let writable = policy
        .writable
        .iter()
        .flatten()
        .map(|path| (path, Mount::Write));
    let visible = policy.visible.iter().map(|path| {
        let mount = if writable_beneath(path) {
            Mount::Write
        } else {
            Mount::Read
        };
        (path, mount)
    });
    let hidden = policy.hidden.iter().map(|path| (path, Mount::Hide));
    let mut wanted: Vec<(&Path, Mount)> = writable
        .chain(visible)
        .chain(hidden)
        .map(|(path, mount)| (path.as_path(), mount))
        .collect();
    wanted.sort_by_key(|(path, mount)| (path.components().count(), *mount));
    let root = if policy.writable.is_some() {
        Mount::Read
    } else {
        Mount::Write
    };
    let mut mounts: Vec<(&Path, Mount)> = Vec::new();
    for (path, mount) in wanted {
        let inherited = mounts
            .iter()
            .rev()
            .find(|(outer, _)| path.starts_with(outer))
            .map_or(root, |(_, outer)| *outer);
        if inherited != mount {
            mounts.push((path, mount));
        }
    }
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_that_change_nothing_are_left_out() {
        let path = PathBuf::from;
        let policy = Policy {
            writable: Some(vec![path("/tmp"), path("/tmp/ws"), path("/home/u/ws")]),
            hidden: vec![
                path("/home/u/.ssh"),
                path("/home/u/.ssh/id"),
                path("/home/u/ws/.env"),
            ],
            visible: vec![path("/home/u/.ssh/known_hosts")],
            hidden_env: Vec::new(),
        };
        let mounts: Vec<(&Path, Mount)> = mounts(&policy);
        let expected: Vec<(&Path, Mount)> = vec![
            (Path::new("/tmp"), Mount::Write),
            (Path::new("/home/u/ws"), Mount::Write),
            (Path::new("/home/u/.ssh"), Mount::Hide),
            (Path::new("/home/u/.ssh/known_hosts"), Mount::Read),
            (Path::new("/home/u/ws/.env"), Mount::Hide),
        ];
        assert_eq!(mounts, expected);
    }
}
