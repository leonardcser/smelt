//! AWK command-line analysis.
//!
//! AWK programs are opaque permission subjects because they can compute paths and mutate `ARGV`.
//! This module only extracts unambiguous command-line paths and never interprets program text as a
//! filesystem operand.

use super::{
    command_name, is_variable_name, push_path, CommandEffects, OpaqueShellCommand, PathAccess,
    PathTargetKind, ShellState, ShellWord,
};
use std::path::Path;

const FLAG_OPTIONS: &str = "bcCghIkMNnOPrsStV";

enum OptionResult {
    Continue(usize),
    Stop,
    Exit,
}

struct Analyzer<'a> {
    words: &'a [ShellWord],
    state: &'a ShellState,
    effects: CommandEffects,
    program_supplied: bool,
    exec_mode: bool,
}

pub(super) fn analyze(words: &[ShellWord], state: &ShellState) -> CommandEffects {
    let Some(command) = words.first().map(command_name) else {
        return CommandEffects::default();
    };
    let mut analyzer = Analyzer {
        words,
        state,
        effects: CommandEffects::default(),
        program_supplied: false,
        exec_mode: false,
    };
    let mut options_done = false;
    let mut ambiguous = false;
    let mut exits_early = false;
    let mut i = 1;

    while i < words.len() {
        let Some(argument) = words[i].expanded.as_deref() else {
            ambiguous = true;
            break;
        };
        if !options_done && argument == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done && argument.starts_with('-') && argument != "-" {
            match analyzer.option(i, argument) {
                OptionResult::Continue(consumed) => {
                    i += consumed;
                    if analyzer.exec_mode {
                        options_done = true;
                    }
                    continue;
                }
                OptionResult::Stop => {
                    ambiguous = true;
                    break;
                }
                OptionResult::Exit => {
                    exits_early = true;
                    break;
                }
            }
        }

        if !analyzer.program_supplied {
            analyzer.program_supplied = true;
        } else if argument != "-" && (analyzer.exec_mode || !is_assignment(argument)) {
            analyzer.push_path(&words[i], PathAccess::Read);
        }
        i += 1;
    }

    if exits_early {
        analyzer.effects.paths.clear();
    } else if analyzer.program_supplied || ambiguous {
        analyzer.effects.opaque_commands.push(OpaqueShellCommand {
            command: format!("{command} *"),
        });
    }
    analyzer.effects
}

impl Analyzer<'_> {
    fn option(&mut self, index: usize, argument: &str) -> OptionResult {
        if argument.starts_with("--") {
            self.long_option(index, argument)
        } else {
            self.short_option(index, argument)
        }
    }

    fn long_option(&mut self, index: usize, argument: &str) -> OptionResult {
        let (option, attached) = argument
            .split_once('=')
            .map_or((argument, None), |(option, value)| (option, Some(value)));
        match option {
            "--field-separator" | "--assign" => self.required_value(index, attached, |_, _| {}),
            "--source" => self.required_value(index, attached, |this, _| {
                this.program_supplied = true;
            }),
            "--file" => self.required_value(index, attached, |this, value| {
                this.program_supplied = true;
                this.push_search_path(value, PathAccess::Read);
            }),
            "--exec" => self.required_value(index, attached, |this, value| {
                this.program_supplied = true;
                this.exec_mode = true;
                this.push_search_path(value, PathAccess::Read);
            }),
            "--include" | "--load" => self.required_value(index, attached, |this, value| {
                this.push_search_path(value, PathAccess::Read);
            }),
            "--debug" => {
                if let Some(value) = attached.filter(|value| !value.is_empty()) {
                    self.push_attached_path(value, PathAccess::Read);
                }
                OptionResult::Continue(1)
            }
            "--dump-variables" => {
                self.optional_output(attached, "awkvars.out");
                OptionResult::Continue(1)
            }
            "--pretty-print" | "--profile" => {
                self.optional_output(attached, "awkprof.out");
                OptionResult::Continue(1)
            }
            "--help" | "--version" | "--copyright" => OptionResult::Exit,
            "--characters-as-bytes"
            | "--traditional"
            | "--gen-pot"
            | "--trace"
            | "--csv"
            | "--bignum"
            | "--use-lc-numeric"
            | "--non-decimal-data"
            | "--optimize"
            | "--posix"
            | "--re-interval"
            | "--no-optimize"
            | "--sandbox"
            | "--lint-old" => {
                if attached.is_some() {
                    OptionResult::Stop
                } else {
                    OptionResult::Continue(1)
                }
            }
            "--lint" => OptionResult::Continue(1),
            _ => OptionResult::Stop,
        }
    }

    fn short_option(&mut self, index: usize, argument: &str) -> OptionResult {
        let mut chars = argument.chars();
        if chars.next() != Some('-') {
            return OptionResult::Stop;
        }
        let Some(option) = chars.next() else {
            return OptionResult::Stop;
        };
        let mut attached: String = chars.collect();
        if let Some(value) = attached.strip_prefix('=') {
            attached = value.to_string();
        }

        match option {
            'F' | 'v' => self.required_value(
                index,
                (!attached.is_empty()).then_some(attached.as_str()),
                |_, _| {},
            ),
            'e' => self.required_value(
                index,
                (!attached.is_empty()).then_some(attached.as_str()),
                |this, _| this.program_supplied = true,
            ),
            'f' => self.required_value(
                index,
                (!attached.is_empty()).then_some(attached.as_str()),
                |this, value| {
                    this.program_supplied = true;
                    this.push_search_path(value, PathAccess::Read);
                },
            ),
            'E' => self.required_value(
                index,
                (!attached.is_empty()).then_some(attached.as_str()),
                |this, value| {
                    this.program_supplied = true;
                    this.exec_mode = true;
                    this.push_search_path(value, PathAccess::Read);
                },
            ),
            'i' | 'l' => self.required_value(
                index,
                (!attached.is_empty()).then_some(attached.as_str()),
                |this, value| this.push_search_path(value, PathAccess::Read),
            ),
            'W' => self.mawk_option(index, attached.as_str()),
            'd' => {
                self.optional_output(
                    (!attached.is_empty()).then_some(attached.as_str()),
                    "awkvars.out",
                );
                OptionResult::Continue(1)
            }
            'D' => {
                if !attached.is_empty() {
                    self.push_attached_path(&attached, PathAccess::Read);
                }
                OptionResult::Continue(1)
            }
            'o' | 'p' => {
                self.optional_output(
                    (!attached.is_empty()).then_some(attached.as_str()),
                    "awkprof.out",
                );
                OptionResult::Continue(1)
            }
            'L' => OptionResult::Continue(1),
            _ if FLAG_OPTIONS.contains(option)
                && attached.chars().all(|ch| FLAG_OPTIONS.contains(ch)) =>
            {
                if "ChV".contains(option) || attached.chars().any(|ch| "ChV".contains(ch)) {
                    OptionResult::Exit
                } else {
                    OptionResult::Continue(1)
                }
            }
            _ => OptionResult::Stop,
        }
    }

    fn mawk_option(&mut self, index: usize, attached: &str) -> OptionResult {
        if !attached.is_empty() {
            return self.mawk_value(index, attached, 1);
        }
        let Some(value) = self
            .words
            .get(index + 1)
            .and_then(|word| word.expanded.as_deref())
        else {
            return OptionResult::Stop;
        };
        self.mawk_value(index, value, 2)
    }

    fn mawk_value(&mut self, index: usize, value: &str, consumed: usize) -> OptionResult {
        if let Some(path) = value.strip_prefix("exec=") {
            if path.is_empty() {
                return OptionResult::Stop;
            }
            self.program_supplied = true;
            self.exec_mode = true;
            self.push_search_path(path, PathAccess::Read);
            return OptionResult::Continue(consumed);
        }
        if value == "exec" {
            let Some(path) = self.words.get(index + consumed) else {
                return OptionResult::Stop;
            };
            self.program_supplied = true;
            self.exec_mode = true;
            self.push_search_path(
                path.expanded.as_deref().unwrap_or_default(),
                PathAccess::Read,
            );
            return OptionResult::Continue(consumed + 1);
        }
        OptionResult::Continue(consumed)
    }

    fn required_value(
        &mut self,
        index: usize,
        attached: Option<&str>,
        apply: impl FnOnce(&mut Self, &str),
    ) -> OptionResult {
        if let Some(value) = attached {
            if value.is_empty() {
                return OptionResult::Stop;
            }
            apply(self, value);
            return OptionResult::Continue(1);
        }
        let Some(value) = self
            .words
            .get(index + 1)
            .and_then(|word| word.expanded.as_deref())
        else {
            return OptionResult::Stop;
        };
        apply(self, value);
        OptionResult::Continue(2)
    }

    fn optional_output(&mut self, attached: Option<&str>, default: &str) {
        self.push_attached_path(
            attached
                .filter(|value| !value.is_empty())
                .unwrap_or(default),
            PathAccess::Write,
        );
    }

    fn push_search_path(&mut self, value: &str, access: PathAccess) {
        // Bare names depend on AWKPATH or AWKLIBPATH. The opaque command approval covers their
        // resolution; adding a cwd-relative path here would present a misleading path grant.
        if value != "-" && is_direct_path(value) {
            self.push_attached_path(value, access);
        }
    }

    fn push_attached_path(&mut self, value: &str, access: PathAccess) {
        let word = ShellWord {
            raw: value.to_string(),
            expanded: Some(value.to_string()),
            has_glob: false,
            embedded_commands: Vec::new(),
            alternatives: Vec::new(),
        };
        self.push_path(&word, access);
    }

    fn push_path(&mut self, word: &ShellWord, access: PathAccess) {
        push_path(
            &mut self.effects.paths,
            word,
            &self.state.cwd,
            access,
            PathTargetKind::File,
        );
    }
}

fn is_assignment(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(name, _)| is_variable_name(name))
}

fn is_direct_path(value: &str) -> bool {
    Path::new(value).is_absolute() || value.contains('/') || (cfg!(windows) && value.contains('\\'))
}
