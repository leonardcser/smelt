//! `$EDITOR` integration: stage the prompt buffer to a tempfile, run the
//! editor under [`crate::term_setup::with_suspended`], and read the result
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

/// Resolve `$VISUAL` then `$EDITOR`, falling back to `vi`. Splits on
/// whitespace so values like `"nvim -p"` work.
pub(crate) fn editor_command_from_env() -> (OsString, Vec<OsString>) {
    let raw = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    split_editor_command(&raw)
}

/// Split a shell-style command string on whitespace. Returns `("vi", [])` if
/// the string is blank.
pub(crate) fn split_editor_command(raw: &str) -> (OsString, Vec<OsString>) {
    let mut parts = raw.split_whitespace();
    let Some(program) = parts.next() else {
        return ("vi".into(), Vec::new());
    };
    let args = parts.map(OsString::from).collect();
    (program.into(), args)
}

/// Stage `text` into a tempfile and build an [`EditorRequest`].
pub(crate) fn prepare(text: &str) -> io::Result<EditorRequest> {
    let tmp = tempfile::Builder::new().suffix(".md").tempfile()?;
    std::fs::write(tmp.path(), text)?;
    let (program, mut args) = editor_command_from_env();
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
        let (prog, args) = split_editor_command("vim");
        assert_eq!(prog, OsString::from("vim"));
        assert!(args.is_empty());
    }

    #[test]
    fn split_handles_program_with_args() {
        let (prog, args) = split_editor_command("nvim -p");
        assert_eq!(prog, OsString::from("nvim"));
        assert_eq!(args, vec![OsString::from("-p")]);
    }

    #[test]
    fn split_handles_extra_whitespace() {
        let (prog, args) = split_editor_command("   code   --wait  -n ");
        assert_eq!(prog, OsString::from("code"));
        assert_eq!(args, vec![OsString::from("--wait"), OsString::from("-n")]);
    }

    #[test]
    fn split_blank_falls_back_to_vi() {
        let (prog, args) = split_editor_command("   ");
        assert_eq!(prog, OsString::from("vi"));
        assert!(args.is_empty());
    }

    #[test]
    fn prepare_writes_text_and_appends_path() {
        let req = prepare("hello\nworld\n").expect("prepare");
        let on_disk = std::fs::read_to_string(req.tmp.path()).expect("read tmp");
        assert_eq!(on_disk, "hello\nworld\n");
        let last = req.args.last().expect("at least the path arg");
        assert_eq!(last.as_os_str(), req.tmp.path().as_os_str());
    }

    #[test]
    fn finalize_reads_file_on_success() {
        let req = prepare("first").expect("prepare");
        std::fs::write(req.tmp.path(), "second").expect("write");
        let status = std::process::Command::new("true").status();
        assert_eq!(finalize(req, status).as_deref(), Ok("second"));
    }

    #[test]
    fn finalize_reports_spawn_error() {
        let req = prepare("x").expect("prepare");
        let err = io::Error::new(io::ErrorKind::NotFound, "no such binary");
        let msg = finalize(req, Err(err)).expect_err("expected error");
        assert!(msg.contains("no such binary"), "{msg}");
    }
}
