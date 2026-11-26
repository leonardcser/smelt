//! Command lines of Lua-declared commands. One [`CommandSpec`] describes a
//! command's arguments, and one clap parser reads them both for `smelt
//! <name>` and for `/name`, so the two accept the same input and report the
//! same errors.

use std::collections::BTreeMap;

/// A Lua-declared flag of smelt or of one command. Mirrors the subset of
/// clap we need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagSpec {
    pub name: String,
    pub kind: FlagKind,
    pub default: FlagValue,
    pub description: Option<String>,
    pub short: Option<char>,
    pub long: Option<String>,
    /// String/Integer flags only. When true, the flag may appear without a
    /// value (`smelt -r` valid alongside `smelt -r abc`); the absent-value
    /// form yields `""` for String and `0` for Integer. Ignored for Boolean.
    pub value_optional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagKind {
    Boolean,
    String,
    Integer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagValue {
    Boolean(bool),
    String(String),
    Integer(i64),
    None,
}

impl FlagSpec {
    /// The long name, `--long`.
    pub fn long(&self) -> &str {
        self.long.as_deref().unwrap_or(&self.name)
    }

    /// The clap argument of this flag.
    pub fn arg(&self) -> clap::Arg {
        use clap::{Arg, ArgAction};

        let mut arg = Arg::new(self.name.clone()).long(self.long().to_string());
        if let Some(short) = self.short {
            arg = arg.short(short);
        }
        if let Some(desc) = &self.description {
            arg = arg.help(desc.clone());
        }
        let missing = match self.kind {
            FlagKind::Boolean => return arg.action(ArgAction::SetTrue),
            FlagKind::String => "",
            FlagKind::Integer => "0",
        };
        let mut arg = arg
            .action(ArgAction::Set)
            .value_name(self.name.to_uppercase());
        if self.kind == FlagKind::Integer {
            arg = arg.value_parser(clap::value_parser!(i64));
        }
        if self.value_optional {
            arg = arg.num_args(0..=1).default_missing_value(missing);
        }
        arg
    }

    /// The parsed value of this flag, or its default when absent.
    pub fn value(&self, matches: &clap::ArgMatches) -> FlagValue {
        let key = self.name.as_str();
        let value = match self.kind {
            FlagKind::Boolean => return FlagValue::Boolean(matches.get_flag(key)),
            FlagKind::String => matches
                .get_one::<String>(key)
                .cloned()
                .map(FlagValue::String),
            FlagKind::Integer => matches.get_one::<i64>(key).copied().map(FlagValue::Integer),
        };
        value.unwrap_or_else(|| self.default.clone())
    }

    /// Whether clap reads `next` as the value of this flag, given as a
    /// whole word without `=value`.
    fn takes(&self, next: Option<&str>) -> bool {
        match self.kind {
            FlagKind::Boolean => false,
            _ if self.value_optional => next.is_some_and(|next| !next.starts_with('-')),
            _ => true,
        }
    }

    /// Its usage in a command hint, such as `[--base <BASE>]`.
    fn usage(&self) -> String {
        let long = self.long();
        match self.kind {
            FlagKind::Boolean => format!("[--{long}]"),
            _ if self.value_optional => format!("[--{long}[=<{}>]]", self.name.to_uppercase()),
            _ => format!("[--{long} <{}>]", self.name.to_uppercase()),
        }
    }
}

/// A free argument or the `--` section of a command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Operand {
    /// Its name in usage and help, such as `QUERY`.
    pub name: String,
    pub description: Option<String>,
}

/// The arguments a Lua-declared command reads, such as those of `smelt diff`
/// and `/diff`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    pub description: Option<String>,
    /// Its free arguments, such as `QUERY`. Without it the command takes none.
    pub args: Option<Operand>,
    /// Whether the free arguments are one text. A slash command keeps it as
    /// typed, quotes and spacing included, so its flags go first; on the
    /// command line the shell has split it, so the words are joined with
    /// spaces.
    pub raw: bool,
    /// The arguments after `--`, such as `PATH`, kept apart from the free
    /// ones. Without it, arguments after `--` are free ones too.
    pub rest: Option<Operand>,
    pub flags: Vec<FlagSpec>,
}

/// A command's arguments as its spec reads them: the free ones, the value
/// of each flag, absent ones at their defaults, and those after `--`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandArgs {
    pub name: String,
    pub args: Vec<String>,
    pub flags: BTreeMap<String, FlagValue>,
    pub rest: Vec<String>,
}

const ARGS: &str = "args";
const REST: &str = "rest";

impl CommandSpec {
    /// The clap command of this spec, such as the `smelt diff` subcommand.
    pub fn clap(&self) -> clap::Command {
        use clap::Arg;

        let mut command = clap::Command::new(self.name.clone());
        if let Some(desc) = &self.description {
            command = command.about(desc.clone());
        }
        for flag in &self.flags {
            command = command.arg(flag.arg());
        }
        let operand = |id: &'static str, operand: &Operand| {
            let arg = Arg::new(id).value_name(operand.name.clone()).num_args(0..);
            match &operand.description {
                Some(help) => arg.help(help.clone()),
                None => arg,
            }
        };
        if let Some(args) = &self.args {
            command = command.arg(operand(ARGS, args));
        }
        if let Some(rest) = &self.rest {
            command = command.arg(operand(REST, rest).last(true));
        }
        command
    }

    /// Read the matches of [`Self::clap`].
    pub fn read(&self, matches: &clap::ArgMatches) -> CommandArgs {
        let values = |id: &str| -> Vec<String> {
            matches
                .try_get_many::<String>(id)
                .ok()
                .flatten()
                .map(|values| values.cloned().collect())
                .unwrap_or_default()
        };
        let mut args = values(ARGS);
        if self.raw && !args.is_empty() {
            args = vec![args.join(" ")];
        }
        CommandArgs {
            name: self.name.clone(),
            args,
            flags: self
                .flags
                .iter()
                .map(|flag| (flag.name.clone(), flag.value(matches)))
                .collect(),
            rest: values(REST),
        }
    }

    /// Read the text a slash command was run with. Words split as a shell
    /// splits them, without expanding anything; quotes and backslashes
    /// escape spaces. A raw spec reads its flags only before the free text.
    pub fn parse(&self, text: &str) -> Result<CommandArgs, String> {
        let (words, raw) = if self.raw {
            self.leading_flags(text)?
        } else {
            (split(text)?, None)
        };
        let mut command = self.parse_words(words)?;
        if self.raw {
            command.args = raw.into_iter().collect();
        }
        Ok(command)
    }

    /// Read words split already, as the shell splits `smelt <name> ...`.
    /// Errors are one line, without usage.
    pub fn parse_words(&self, words: Vec<String>) -> Result<CommandArgs, String> {
        let matches = self
            .clap()
            .no_binary_name(true)
            .disable_help_flag(true)
            .color(clap::ColorChoice::Never)
            .try_get_matches_from(words)
            .map_err(|error| {
                let message = error.to_string();
                let first = message.lines().next().unwrap_or_default();
                first.strip_prefix("error: ").unwrap_or(first).to_string()
            })?;
        Ok(self.read(&matches))
    }

    /// Split the flags off the start of `text` from the free text after
    /// them, which `--` may open. The free text is never split into words,
    /// so its quotes need not pair.
    fn leading_flags(&self, text: &str) -> Result<(Vec<String>, Option<String>), String> {
        let raw = |start: usize| Some(text[start..].trim_end().to_string());
        let mut flags = Vec::new();
        let mut at = 0;
        while let Some((start, word)) = word_at(text, at) {
            if !text[start..].starts_with('-') {
                return Ok((flags, raw(start)));
            }
            let (word, end) = word?;
            if word == "-" {
                return Ok((flags, raw(start)));
            }
            if word == "--" {
                return Ok((flags, word_at(text, end).and_then(|(next, _)| raw(next))));
            }
            at = end;
            let next = word_at(text, end).map(|(next, _)| &text[next..]);
            let takes = self
                .flags
                .iter()
                .find(|flag| match word.strip_prefix("--") {
                    Some(long) => flag.long() == long,
                    None => word.chars().nth(1) == flag.short && word.chars().count() == 2,
                })
                .is_some_and(|flag| flag.takes(next));
            flags.push(word);
            if let Some((_, value)) = word_at(text, end).filter(|_| takes) {
                let (value, end) = value?;
                flags.push(value);
                at = end;
            }
        }
        Ok((flags, None))
    }

    /// The hint of a slash command, one label per flag and operand.
    pub fn usage(&self) -> Vec<String> {
        let mut usage: Vec<String> = self.flags.iter().map(FlagSpec::usage).collect();
        if let Some(args) = &self.args {
            usage.push(format!("[{}]", args.name));
        }
        if let Some(rest) = &self.rest {
            usage.push(format!("[-- {}...]", rest.name));
        }
        usage
    }
}

/// Split `text` into words. Single quotes keep everything; double quotes
/// and a backslash outside quotes keep the next character, so a space
/// inside them does not end a word. Nothing else is special.
fn split(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut at = 0;
    while let Some((_, word)) = word_at(text, at) {
        let (word, end) = word?;
        words.push(word);
        at = end;
    }
    Ok(words)
}

/// Where a word starts, then its text and where it ends.
type WordAt = Option<(usize, Result<(String, usize), String>)>;

/// The first word of `text` at or after byte `from`, as [`split`] reads it.
fn word_at(text: &str, from: usize) -> WordAt {
    let start = from + text[from..].find(|ch: char| !ch.is_whitespace())?;
    let mut word = String::new();
    let mut quote = None;
    let mut chars = text[start..].char_indices();
    while let Some((at, ch)) = chars.next() {
        match (quote, ch) {
            (None, ch) if ch.is_whitespace() => return Some((start, Ok((word, start + at)))),
            (Some(open), _) if ch == open => quote = None,
            (Some('"') | None, '\\') => match chars.next() {
                Some((_, next)) => word.push(next),
                None => return Some((start, Err("trailing escape".into()))),
            },
            (Some(_), _) => word.push(ch),
            (None, '\'' | '"') => quote = Some(ch),
            (None, _) => word.push(ch),
        }
    }
    if quote.is_some() {
        return Some((start, Err("unterminated quote".into())));
    }
    Some((start, Ok((word, text.len()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(name: &str, short: Option<char>, kind: FlagKind) -> FlagSpec {
        FlagSpec {
            name: name.into(),
            kind,
            default: match kind {
                FlagKind::Boolean => FlagValue::Boolean(false),
                _ => FlagValue::None,
            },
            description: None,
            short,
            long: None,
            value_optional: false,
        }
    }

    fn spec(raw: bool, rest: bool) -> CommandSpec {
        CommandSpec {
            name: "probe".into(),
            args: Some(Operand {
                name: "ARGS".into(),
                description: None,
            }),
            raw,
            rest: rest.then(|| Operand {
                name: "PATH".into(),
                description: None,
            }),
            flags: vec![
                flag("git", Some('g'), FlagKind::Boolean),
                flag("base", Some('b'), FlagKind::String),
                FlagSpec {
                    default: FlagValue::Integer(1),
                    ..flag("count", None, FlagKind::Integer)
                },
            ],
            ..Default::default()
        }
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn words_split_on_spaces_outside_quotes() {
        let words = split(r#" alpha  "two words" 'it''s' a\ b "q\"" '' "#).unwrap();
        assert_eq!(words, ["alpha", "two words", "its", "a b", "q\"", ""]);
        assert_eq!(
            split("unterminated \"").err().unwrap(),
            "unterminated quote"
        );
        assert_eq!(split(r"trailing \").err().unwrap(), "trailing escape");
    }

    #[test]
    fn flags_read_anywhere_and_stop_at_dashes() {
        let spec = spec(false, false);
        let parsed = spec
            .parse(r#"one -g "two words" --base=main -- -b"#)
            .unwrap();
        assert_eq!(parsed.args, strings(&["one", "two words", "-b"]));
        assert_eq!(parsed.flags["git"], FlagValue::Boolean(true));
        assert_eq!(parsed.flags["base"], FlagValue::String("main".into()));
        assert_eq!(parsed.flags["count"], FlagValue::Integer(1));
        let parsed = spec.parse("-b dev --count 3").unwrap();
        assert!(parsed.args.is_empty());
        assert_eq!(parsed.flags["git"], FlagValue::Boolean(false));
        assert_eq!(parsed.flags["base"], FlagValue::String("dev".into()));
        assert_eq!(parsed.flags["count"], FlagValue::Integer(3));
    }

    #[test]
    fn bad_flags_report_one_line_errors() {
        let spec = spec(false, false);
        for (text, error) in [
            ("--regx", "unexpected argument '--regx' found"),
            (
                "--base",
                "a value is required for '--base <BASE>' but none was supplied",
            ),
            (
                "--git=yes",
                "unexpected value 'yes' for '--git' found; no more were expected",
            ),
            (
                "--count x",
                "invalid value 'x' for '--count <COUNT>': invalid digit found in string",
            ),
        ] {
            assert_eq!(spec.parse(text).unwrap_err(), error, "{text}");
        }
    }

    #[test]
    fn raw_text_keeps_what_follows_the_leading_flags() {
        let spec = spec(true, false);
        let parsed = spec.parse(r#" -g  it's  "quoted" -b "#).unwrap();
        assert_eq!(parsed.args, strings(&[r#"it's  "quoted" -b"#]));
        assert_eq!(parsed.flags["git"], FlagValue::Boolean(true));
        let parsed = spec.parse("-b main -- -g").unwrap();
        assert_eq!(parsed.args, strings(&["-g"]));
        assert_eq!(parsed.flags["base"], FlagValue::String("main".into()));
        assert_eq!(parsed.flags["git"], FlagValue::Boolean(false));
        assert!(spec.parse("-g").unwrap().args.is_empty());
        assert_eq!(
            spec.parse("--regx it").unwrap_err(),
            "unexpected argument '--regx' found"
        );
    }

    #[test]
    fn rest_holds_the_words_after_dashes() {
        let spec = spec(false, true);
        let parsed = spec.parse("main..dev -g -- src '*.rs'").unwrap();
        assert_eq!(parsed.args, strings(&["main..dev"]));
        assert_eq!(parsed.rest, strings(&["src", "*.rs"]));
        assert_eq!(parsed.flags["git"], FlagValue::Boolean(true));
    }

    #[test]
    fn usage_lists_flags_then_operands() {
        assert_eq!(
            spec(false, true).usage(),
            strings(&[
                "[--git]",
                "[--base <BASE>]",
                "[--count <COUNT>]",
                "[ARGS]",
                "[-- PATH...]"
            ])
        );
    }
}
