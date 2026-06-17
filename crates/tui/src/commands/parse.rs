use super::ShellSink;

/// Parsed shape of a raw command line typed into the prompt or cmdline.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParsedCommand<'a> {
    Empty,
    Shell {
        script: &'a str,
        sink: ShellSink,
    },
    Slash {
        name: &'a str,
        arg: Option<&'a str>,
    },
    Ex {
        name: &'a str,
        bang: bool,
        arg: Option<&'a str>,
    },
    Bare {
        text: &'a str,
    },
}

fn split_name_arg(body: &str) -> (&str, Option<&str>) {
    match body.split_once(char::is_whitespace) {
        Some((name, arg)) => {
            let arg = arg.trim();
            (name, if arg.is_empty() { None } else { Some(arg) })
        }
        None => (body, None),
    }
}

/// Classify a raw command line without dispatching it. Prompt `/name` runs the
/// prompt-local command surface, `:name` runs the statusline command surface,
/// and shell escapes choose their output sink from the sigil that launched them.
/// Statusline `/` and `?` search inputs are handled by the cmdline UI before
/// command parsing.
pub(crate) fn parse_command_line(line: &str) -> ParsedCommand<'_> {
    let line = line.trim();
    if line.is_empty() {
        return ParsedCommand::Empty;
    }
    if let Some(rest) = line.strip_prefix('!') {
        return ParsedCommand::Shell {
            script: rest.trim_start(),
            sink: ShellSink::Transcript,
        };
    }
    if let Some(body) = line.strip_prefix(':') {
        if let Some(rest) = body.strip_prefix('!') {
            return ParsedCommand::Shell {
                script: rest.trim_start(),
                sink: ShellSink::Overlay,
            };
        }
        let (name, arg) = split_name_arg(body);
        let bang = name.ends_with('!');
        let name = if bang { &name[..name.len() - 1] } else { name };
        return ParsedCommand::Ex { name, bang, arg };
    }
    if let Some(body) = line.strip_prefix('/') {
        let (name, arg) = split_name_arg(body);
        return ParsedCommand::Slash { name, arg };
    }
    ParsedCommand::Bare { text: line }
}
