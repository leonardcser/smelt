//! `smelt.os` — environment and system primitives (getenv, setenv, platform, cwd, pid, etc.).

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "os",
        "Environment and system primitives: getenv, setenv, platform, cwd, pid, etc.",
        Tier::Host,
    )?;
    m.fn_(
        "getenv",
        "Return the value of the environment variable `name`, or `nil` if it is not set.",
        &["name"],
        |_, name: String| Ok(std::env::var(name).ok()),
    )?;

    m.fn_(
        "setenv",
        "Set the process environment variable `name` to `value`. Mutates the live process env; visible to subsequent `getenv` calls and child processes.",
        &["name", "value"],
        |_, (name, value): (String, String)| -> LuaResult<()> {
            // `std::env::set_var` panics on empty / `=`-containing / NUL-
            // containing names; reject them as a Lua error instead.
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(mlua::Error::RuntimeError(format!(
                    "setenv: invalid name {name:?}"
                )));
            }
            if value.contains('\0') {
                return Err(mlua::Error::RuntimeError(
                    "setenv: value contains NUL".into(),
                ));
            }
            // SAFETY: Lua runs on a single thread; setenv on POSIX is
            // safe so long as nothing else is reading concurrently.
            unsafe { std::env::set_var(name, value) };
            Ok(())
        },
    )?;

    m.fn_(
        "unsetenv",
        "Remove the environment variable `name` from the process environment.",
        &["name"],
        |_, name: String| -> LuaResult<()> {
            // Mirror `setenv`'s validation; the libc call panics on the
            // same illegal inputs.
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(mlua::Error::RuntimeError(format!(
                    "unsetenv: invalid name {name:?}"
                )));
            }
            unsafe { std::env::remove_var(name) };
            Ok(())
        },
    )?;

    m.fn_(
        "platform",
        "Return the target operating system as reported by `std::env::consts::OS` (e.g. `\"macos\"`, `\"linux\"`).",
        &[],
        |_, ()| Ok(std::env::consts::OS),
    )?;

    m.fn_(
        "arch",
        "Return the target CPU architecture as reported by `std::env::consts::ARCH` (e.g. `\"x86_64\"`, `\"aarch64\"`).",
        &[],
        |_, ()| Ok(std::env::consts::ARCH),
    )?;

    m.fn_(
        "tempdir",
        "Return the platform temporary directory path.",
        &[],
        |_, ()| Ok(std::env::temp_dir().to_string_lossy().into_owned()),
    )?;

    m.fn_(
        "home",
        "Return the user's home directory, or `nil` if it cannot be determined.",
        &[],
        |_, ()| Ok(dirs::home_dir().map(|p| p.to_string_lossy().into_owned())),
    )?;

    m.fn_(
        "cwd",
        "Return the current working directory as `(path, nil)`, or `(nil, err_string)` on failure.",
        &[],
        |_, ()| match std::env::current_dir() {
            Ok(p) => Ok((Some(p.to_string_lossy().into_owned()), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    m.fn_(
        "set_cwd",
        "Change the process working directory to `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match std::env::set_current_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    m.fn_(
        "pid",
        "Return the OS process id of the running smelt instance.",
        &[],
        |_, ()| Ok(std::process::id()),
    )?;

    m.fn_(
        "open_url",
        "Open `url` in the system's default browser. macOS uses `open`, Windows uses `cmd /c start`, everything else tries `xdg-open` then falls back to `open`. Only `http(s)://`, `mailto:`, and `file://` URLs are accepted. Returns `(true, nil)` on a successful spawn, or `(false, err_string)` if the scheme is rejected or every launcher errored.",
        &["url"],
        |_, url: String| Ok(match open_url(&url) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        }),
    )?;

    Ok(())
}

fn open_url(url: &str) -> Result<(), String> {
    // Reject anything that isn't a vetted user-facing scheme. The launcher
    // hands the string to the OS shell, so we don't want `javascript:` or a
    // bare argument that could be mistaken for a flag (`-foo`).
    let lower = url.to_ascii_lowercase();
    let allowed = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("file://");
    if !allowed {
        return Err(format!(
            "open_url: refusing to open {url:?} (only http(s)/mailto/file schemes are allowed)"
        ));
    }

    let attempts: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin; the empty quoted "" is `start`'s title arg
        // so URLs containing `&` aren't reinterpreted by the shell.
        &[("cmd", &["/C", "start", ""])]
    } else {
        &[("xdg-open", &[]), ("open", &[])]
    };

    let mut last_err = String::new();
    for (program, prefix) in attempts {
        let mut cmd = std::process::Command::new(program);
        cmd.args(*prefix).arg(url);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_) => return Ok(()),
            Err(e) => last_err = format!("{program}: {e}"),
        }
    }
    Err(format!("open_url: no launcher available ({last_err})"))
}
