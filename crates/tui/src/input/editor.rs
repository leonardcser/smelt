//! `$EDITOR` integration: stage the prompt buffer to a tempfile, run the
//! editor under [`crate::term_setup::TuiTerminal::suspended`], and read the result
//! back. The pure pieces (`split_editor_command`, `prepare`, `finalize`) are
//! split out from the suspend/resume so they can be exercised without a TTY.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use tempfile::NamedTempFile;

pub(crate) struct EditorRequest {
    pub(crate) tmp: NamedTempFile,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}

/// Resolve `$VISUAL` then `$EDITOR`, falling back to `vi`.
pub(crate) fn editor_command_from_env() -> io::Result<(OsString, Vec<OsString>)> {
    let raw = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    split_editor_command(&raw)
}

/// Parse native command-line quoting without shell expansion or execution.
/// Blank uses `vi`. Empty programs and malformed Unix quoting are errors.
pub(crate) fn split_editor_command(raw: &str) -> io::Result<(OsString, Vec<OsString>)> {
    if raw.trim().is_empty() {
        return Ok(("vi".into(), Vec::new()));
    }
    if raw.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL byte in editor command",
        ));
    }
    #[cfg(windows)]
    let parts = split_windows_editor_command(raw)?;
    #[cfg(not(windows))]
    let parts = shlex::split(raw)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid quoting in editor command",
            )
        })?
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let mut parts = parts.into_iter();
    let Some(program) = parts.next() else {
        return Ok(("vi".into(), Vec::new()));
    };
    if program.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty program in editor command",
        ));
    }
    Ok((program, parts.collect()))
}

#[cfg(windows)]
fn split_windows_editor_command(raw: &str) -> io::Result<Vec<OsString>> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};

    // Leading whitespace makes CommandLineToArgvW produce an empty program.
    let command: Vec<u16> = std::ffi::OsStr::new(raw.trim_start())
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut count = 0;
    // SAFETY: command is NUL-terminated and count is a writable out-parameter.
    let argv = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
    if argv.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the OS returns count valid NUL-terminated strings in one allocation.
    let parts = unsafe {
        std::slice::from_raw_parts(argv, count as usize)
            .iter()
            .map(|&arg| {
                let mut len = 0;
                while *arg.add(len) != 0 {
                    len += 1;
                }
                OsString::from_wide(std::slice::from_raw_parts(arg, len))
            })
            .collect()
    };
    // SAFETY: argv is the allocation returned by CommandLineToArgvW, copied above.
    unsafe { LocalFree(argv.cast()) };
    Ok(parts)
}

/// Stage `text` into a tempfile and build an [`EditorRequest`].
pub(crate) fn prepare(text: &str, runtime_dir: &std::path::Path) -> io::Result<EditorRequest> {
    let tmp = tempfile::Builder::new()
        .suffix(".md")
        .tempfile_in(runtime_dir)?;
    std::fs::write(tmp.path(), text)?;
    let (program, mut args) = editor_command_from_env()?;
    args.push(tmp.path().as_os_str().to_os_string());
    Ok(EditorRequest { tmp, program, args })
}

/// Translate `Command::status` + final file state into `Ok(new_text)` or a
/// pre-formatted error message suitable for `notify_error`.
pub(crate) fn finalize(
    req: EditorRequest,
    status: io::Result<ExitStatus>,
) -> Result<String, String> {
    let display = PathBuf::from(&req.program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| req.program.to_string_lossy().into_owned());

    match status {
        Ok(s) if s.success() => {
            std::fs::read_to_string(req.tmp.path()).map_err(|e| format!("read tmp: {e}"))
        }
        Ok(s) => Err(format!("{display} exited with {s}")),
        Err(e) => Err(format!("{display}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_handles_bare_program() {
        let (prog, args) = split_editor_command("vim").unwrap();
        assert_eq!(prog, OsString::from("vim"));
        assert!(args.is_empty());
    }

    #[test]
    fn split_handles_program_with_args() {
        let (prog, args) = split_editor_command("nvim -p").unwrap();
        assert_eq!(prog, OsString::from("nvim"));
        assert_eq!(args, vec![OsString::from("-p")]);
    }

    #[test]
    fn split_handles_extra_whitespace() {
        let (prog, args) = split_editor_command("   code   --wait  -n ").unwrap();
        assert_eq!(prog, OsString::from("code"));
        assert_eq!(args, vec![OsString::from("--wait"), OsString::from("-n")]);
    }

    #[test]
    fn split_blank_falls_back_to_vi() {
        let (prog, args) = split_editor_command("   ").unwrap();
        assert_eq!(prog, OsString::from("vi"));
        assert!(args.is_empty());
    }

    #[test]
    fn split_preserves_quoted_paths_arguments_and_empty_arguments() {
        let (program, args) = split_editor_command(
            r#""path with spaces/editor" -c "set number" "" "$HOME;touch not-a-command""#,
        )
        .unwrap();
        assert_eq!(program, OsString::from("path with spaces/editor"));
        assert_eq!(
            args,
            ["-c", "set number", "", "$HOME;touch not-a-command"].map(OsString::from)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn split_handles_posix_quotes_and_escapes() {
        let (program, args) =
            split_editor_command(r#"path\ with\ spaces/editor -c 'set number' \"literal\""#)
                .unwrap();
        assert_eq!(program, OsString::from("path with spaces/editor"));
        assert_eq!(
            args,
            ["-c", "set number", "\"literal\""].map(OsString::from)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn split_rejects_malformed_quotes_and_escapes() {
        for command in ["nvim 'unfinished", "nvim \"unfinished", "nvim \\"] {
            assert_eq!(
                split_editor_command(command).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn split_preserves_windows_paths_and_escaped_quotes() {
        let (program, args) = split_editor_command(
            r#""C:\Program Files\Neovim\bin\nvim.exe" "C:\source files\app" "say \"hello\"""#,
        )
        .unwrap();
        assert_eq!(
            program,
            OsString::from(r"C:\Program Files\Neovim\bin\nvim.exe")
        );
        assert_eq!(
            args,
            [r"C:\source files\app", "say \"hello\""].map(OsString::from)
        );
    }

    #[test]
    fn split_rejects_empty_program_and_nul() {
        for command in ["\"\" -p", "nvim\0bad"] {
            assert_eq!(
                split_editor_command(command).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn prepare_writes_text_in_runtime_dir_and_appends_path() {
        let runtime_dir = tempfile::tempdir().expect("runtime directory");
        let req = prepare("hello\nworld\n", runtime_dir.path()).expect("prepare");
        assert!(req.tmp.path().starts_with(runtime_dir.path()));
        let on_disk = std::fs::read_to_string(req.tmp.path()).expect("read tmp");
        assert_eq!(on_disk, "hello\nworld\n");
        let last = req.args.last().expect("at least the path arg");
        assert_eq!(last.as_os_str(), req.tmp.path().as_os_str());
    }

    #[test]
    fn finalize_reads_file_on_success() {
        let req = prepare("first", &std::env::temp_dir()).expect("prepare");
        std::fs::write(req.tmp.path(), "second").expect("write");
        let status = std::process::Command::new("true").status();
        assert_eq!(finalize(req, status).as_deref(), Ok("second"));
    }

    #[test]
    fn finalize_reports_spawn_error() {
        let req = prepare("x", &std::env::temp_dir()).expect("prepare");
        let err = io::Error::new(io::ErrorKind::NotFound, "no such binary");
        let msg = finalize(req, Err(err)).expect_err("expected error");
        assert!(msg.contains("no such binary"), "{msg}");
    }
}
