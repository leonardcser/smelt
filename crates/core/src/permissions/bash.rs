//! Shell command parsing for permission checks.
//!
//! Splits compound commands on `&&`, `||`, `;`, `|`, `&`, newline (quote-
//! aware), extracts embedded commands from `$(...)`, backticks, and `(...)`
//! subshells, parses heredocs, and detects output redirections.

use brush_parser::ast::{
    self, CommandPrefixOrSuffixItem, CompoundCommand, IoFileRedirectKind, IoFileRedirectTarget,
    IoRedirect,
};
use brush_parser::word::{Parameter, ParameterExpr, WordPiece, WordPieceWithSource};

mod awk;

use super::{
    shell_parse, workspace, OpaqueShellCommand, PathAccess, PathEffect, PathResolution,
    PathTargetKind, ShellRisk,
};
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const MAX_GLOB_ENTRIES: usize = 4096;
const MAX_BRACE_EXPANSIONS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellAnalysis {
    pub risk: ShellRisk,
    pub paths: Vec<PathEffect>,
    pub opaque_commands: Vec<OpaqueShellCommand>,
}

struct GlobPathAnalysis {
    effects: Vec<PathEffect>,
    matches: Vec<PathResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellState {
    cwd: PathResolution,
    home: PathBuf,
    variables: HashMap<String, Option<String>>,
}

impl ShellState {
    fn new(cwd: &Path, home: &Path) -> Self {
        Self {
            cwd: workspace::resolve_filesystem_path(cwd),
            home: home.to_path_buf(),
            variables: HashMap::from([(
                "HOME".to_string(),
                Some(home.to_string_lossy().into_owned()),
            )]),
        }
    }

    fn variable(&self, name: &str) -> Option<String> {
        if let Some(value) = self.variables.get(name) {
            return value.clone();
        }
        if name == "PWD" {
            return self
                .cwd
                .resolved()
                .map(|cwd| cwd.to_string_lossy().into_owned());
        }
        match std::env::var(name) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => Some(String::new()),
            Err(std::env::VarError::NotUnicode(_)) => None,
        }
    }

    fn set_variable(&mut self, name: String, value: Option<String>) {
        self.variables.insert(name, value);
    }

    fn unset_variable(&mut self, name: &str) {
        self.variables.insert(name.to_string(), Some(String::new()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellWord {
    raw: String,
    expanded: Option<String>,
    has_glob: bool,
    embedded_commands: Vec<String>,
    alternatives: Vec<ShellWord>,
}

impl ShellWord {
    fn raw(&self) -> &str {
        &self.raw
    }

    fn strip_literal_prefix(&self, prefix: &str) -> Option<Self> {
        let raw = self.raw.strip_prefix(prefix)?.to_string();
        let expanded = match &self.expanded {
            Some(value) => Some(value.strip_prefix(prefix)?.to_string()),
            None => None,
        };
        let alternatives = self
            .alternatives
            .iter()
            .map(|word| word.strip_literal_prefix(prefix))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            raw,
            expanded,
            has_glob: self.has_glob,
            embedded_commands: self.embedded_commands.clone(),
            alternatives,
        })
    }

    fn assignment(&self) -> Option<(String, Option<String>)> {
        let (name, _) = self.raw.split_once('=')?;
        if !is_variable_name(name) {
            return None;
        }
        let value = self
            .expanded
            .as_ref()
            .and_then(|expanded| expanded.split_once('=').map(|(_, value)| value.to_string()));
        Some((name.to_string(), value))
    }

    fn into_expanded_words(self) -> Vec<Self> {
        if self.alternatives.is_empty() {
            vec![self]
        } else {
            self.alternatives
        }
    }
}

/// Split a Bash program into commands and their following operators.
pub fn split_shell_commands_with_ops(command: &str) -> Vec<(String, Option<String>)> {
    super::shell_parse::split_with_ops(command)
}

/// Split a Bash program into commands, including nested and substituted commands.
pub fn split_shell_commands(command: &str) -> Vec<String> {
    super::shell_parse::split(command)
}

#[derive(Default)]
struct WordExpansion {
    raw: String,
    expanded: String,
    resolved: bool,
    has_glob: bool,
    embedded_commands: Vec<String>,
}

fn shell_word(raw: &str, state: &ShellState) -> ShellWord {
    let brace_expansion = shell_parse::brace_expansion(raw, MAX_BRACE_EXPANSIONS);
    let mut word = shell_word_without_brace_expansion(raw, state);
    match brace_expansion {
        shell_parse::BraceExpansion::Absent => {}
        shell_parse::BraceExpansion::Expanded(expanded) => {
            word.expanded = None;
            word.alternatives = expanded
                .into_iter()
                .map(|expanded| shell_word_without_brace_expansion(&expanded, state))
                .collect();
        }
        shell_parse::BraceExpansion::Unresolved => word.expanded = None,
    }
    word
}

fn shell_word_without_brace_expansion(raw: &str, state: &ShellState) -> ShellWord {
    let Some(pieces) = shell_parse::parse_word(raw) else {
        return ShellWord {
            raw: raw.to_string(),
            expanded: None,
            has_glob: false,
            embedded_commands: Vec::new(),
            alternatives: Vec::new(),
        };
    };
    let mut expansion = WordExpansion {
        resolved: true,
        ..WordExpansion::default()
    };
    expand_word_pieces(raw, &pieces, state, false, &mut expansion);
    ShellWord {
        raw: expansion.raw,
        expanded: expansion.resolved.then_some(expansion.expanded),
        has_glob: expansion.has_glob,
        embedded_commands: expansion.embedded_commands,
        alternatives: Vec::new(),
    }
}

fn expand_word_pieces(
    source: &str,
    pieces: &[WordPieceWithSource],
    state: &ShellState,
    quoted: bool,
    out: &mut WordExpansion,
) {
    for piece in pieces {
        let source_piece = smelt_buffer::text::slice(source, piece.start_index..piece.end_index);
        match &piece.piece {
            WordPiece::Text(value) => {
                out.raw.push_str(value);
                out.expanded.push_str(value);
                if !quoted && value.chars().any(|ch| matches!(ch, '*' | '?' | '[')) {
                    out.has_glob = true;
                }
            }
            WordPiece::SingleQuotedText(value) | WordPiece::AnsiCQuotedText(value) => {
                out.raw.push_str(value);
                out.expanded.push_str(value);
            }
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                expand_word_pieces(source, inner, state, true, out);
            }
            WordPiece::TildeExpansion(_) => {
                out.raw.push_str(source_piece);
                if let Some(value) = expand_shell_tilde(source_piece, state) {
                    out.expanded.push_str(&value);
                } else {
                    out.resolved = false;
                }
            }
            WordPiece::ParameterExpansion(parameter) => {
                out.raw.push_str(source_piece);
                match parameter_value(parameter, state) {
                    Some(value) if !quoted && value.chars().any(char::is_whitespace) => {
                        out.resolved = false;
                    }
                    Some(value) => {
                        if !quoted && value.chars().any(|ch| matches!(ch, '*' | '?' | '[')) {
                            out.has_glob = true;
                        }
                        out.expanded.push_str(&value);
                    }
                    None => out.resolved = false,
                }
            }
            WordPiece::CommandSubstitution(command)
            | WordPiece::BackquotedCommandSubstitution(command) => {
                out.raw.push_str(source_piece);
                out.resolved = false;
                out.embedded_commands.push(command.clone());
            }
            WordPiece::EscapeSequence(value) => {
                out.raw.push_str(value);
                out.expanded.push_str(value);
            }
            WordPiece::ArithmeticExpression(_) => {
                out.raw.push_str(source_piece);
                out.resolved = false;
            }
        }
    }
}

fn parameter_value(parameter: &ParameterExpr, state: &ShellState) -> Option<String> {
    let ParameterExpr::Parameter {
        parameter,
        indirect: false,
    } = parameter
    else {
        return None;
    };
    match parameter {
        Parameter::Named(name) => state.variable(name),
        Parameter::NamedWithIndex { .. }
        | Parameter::NamedWithAllIndices { .. }
        | Parameter::Positional(_)
        | Parameter::Special(_) => None,
    }
}

const MAX_SHELL_STATES: usize = 32;
const MAX_SHELL_NESTING: usize = 32;
const LOOP_UNROLL_LIMIT: usize = 4;

pub(super) fn analyze_shell_command(command: &str, base_dir: &Path) -> ShellAnalysis {
    analyze_shell_command_in(command, base_dir, &engine::paths::home_dir())
}

pub(super) fn analyze_shell_command_in(
    command: &str,
    base_dir: &Path,
    home: &Path,
) -> ShellAnalysis {
    let mut analysis = ShellAnalysis {
        risk: ShellRisk::ReadOnly,
        paths: Vec::new(),
        opaque_commands: Vec::new(),
    };
    let initial_state = ShellState::new(base_dir, home);
    let initial_states = vec![initial_state.clone()];
    let Some(program) = shell_parse::parse(command) else {
        record_unknown_shell(command, &initial_state.cwd, &mut analysis);
        return analysis;
    };
    eval_program(&program, initial_states, &mut analysis, 0);
    analysis.paths.dedup();
    analysis
}

fn record_unknown_shell(raw: &str, cwd: &PathResolution, analysis: &mut ShellAnalysis) {
    analysis.risk = merge_risk(analysis.risk.clone(), ShellRisk::Unknown);
    analysis.paths.push(PathEffect::from_shell_path(
        raw.to_string(),
        None,
        cwd,
        PathAccess::Unknown,
        PathTargetKind::Unknown,
    ));
}

fn eval_program(
    program: &ast::Program,
    mut states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    if depth >= MAX_SHELL_NESTING {
        if let Some(state) = states.first() {
            record_unknown_shell("<shell nesting limit>", &state.cwd, analysis);
        }
        return states;
    }
    for command in &program.complete_commands {
        states = eval_list(command, states, analysis, depth + 1);
    }
    states
}

fn eval_list(
    list: &ast::CompoundList,
    mut states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    for item in &list.0 {
        let entry = states.clone();
        let output = eval_and_or(&item.0, states, analysis, depth);
        states = if matches!(item.1, ast::SeparatorOperator::Async) {
            entry
        } else {
            output
        };
    }
    states
}

fn eval_and_or(
    list: &ast::AndOrList,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    let mut states = eval_pipeline(&list.first, states, analysis, depth);
    for item in &list.additional {
        let pipeline = match item {
            ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => pipeline,
        };
        let ran = eval_pipeline(pipeline, states.clone(), analysis, depth);
        states = union_states(states, ran);
    }
    states
}

fn eval_pipeline(
    pipeline: &ast::Pipeline,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    if pipeline.seq.len() == 1 {
        return eval_command(&pipeline.seq[0], states, analysis, depth);
    }
    for command in &pipeline.seq {
        eval_command(command, states.clone(), analysis, depth);
    }
    states
}

fn eval_command(
    command: &ast::Command,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    match command {
        ast::Command::Simple(command) => eval_simple_command(command, states, analysis, depth),
        ast::Command::Compound(command, redirects) => {
            analyze_redirect_list(redirects.as_ref(), &states, analysis, depth);
            eval_compound_command(command, states, analysis, depth)
        }
        ast::Command::Function(function) => {
            analyze_redirect_list(function.body.1.as_ref(), &states, analysis, depth);
            eval_compound_command(&function.body.0, states.clone(), analysis, depth);
            states
        }
        ast::Command::ExtendedTest(test, redirects) => {
            analyze_redirect_list(redirects.as_ref(), &states, analysis, depth);
            for state in &states {
                analyze_extended_test(&test.expr, state, analysis, depth);
            }
            states
        }
    }
}

#[derive(Default)]
struct SimpleCommandWords {
    assignments: Vec<ShellWord>,
    words: Vec<ShellWord>,
}

fn eval_simple_command(
    command: &ast::SimpleCommand,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    let mut output = Vec::new();
    for mut state in states {
        let mut command_words = SimpleCommandWords::default();
        if let Some(prefix) = &command.prefix {
            for item in &prefix.0 {
                analyze_simple_item(item, &state, &mut command_words, analysis, depth, true);
            }
        }
        if let Some(word) = &command.word_or_name {
            let word = shell_word(&word.value, &state);
            analyze_embedded_commands(&word, &state, analysis, depth);
            command_words.words.extend(word.into_expanded_words());
        }
        if let Some(suffix) = &command.suffix {
            for item in &suffix.0 {
                analyze_simple_item(item, &state, &mut command_words, analysis, depth, false);
            }
        }

        if command.word_or_name.is_none() {
            apply_assignments(&mut state, &command_words.assignments);
            output.push(state);
            continue;
        }

        let effective_words = effective_command_words(&command_words.words);
        analysis.risk = merge_risk(analysis.risk.clone(), classify_risk(effective_words));
        match effective_words.first().map(ShellWord::raw) {
            Some("cd") => apply_cd(&mut state, effective_words, &mut analysis.paths),
            Some("export") => apply_assignments(&mut state, &effective_words[1..]),
            Some("unset") => {
                for word in &effective_words[1..] {
                    state.unset_variable(word.raw());
                }
            }
            Some(_) => {
                let mut command_state = state.clone();
                apply_assignments(&mut command_state, &command_words.assignments);
                let effects = command_effects(effective_words, &command_state);
                analysis.paths.extend(effects.paths);
                analysis.opaque_commands.extend(effects.opaque_commands);
            }
            None => {}
        }
        output.push(state);
    }
    normalize_states(output)
}

fn analyze_simple_item(
    item: &CommandPrefixOrSuffixItem,
    state: &ShellState,
    command_words: &mut SimpleCommandWords,
    analysis: &mut ShellAnalysis,
    depth: usize,
    is_prefix: bool,
) {
    match item {
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            analyze_redirect(redirect, state, analysis, depth);
        }
        CommandPrefixOrSuffixItem::Word(word) => {
            let word = shell_word(&word.value, state);
            analyze_embedded_commands(&word, state, analysis, depth);
            command_words.words.extend(word.into_expanded_words());
        }
        CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
            let analyzed = shell_word(&word.value, state);
            analyze_embedded_commands(&analyzed, state, analysis, depth);
            if is_prefix {
                let assignment = if analyzed.alternatives.is_empty() {
                    analyzed
                } else {
                    shell_word_without_brace_expansion(&word.value, state)
                };
                command_words.assignments.push(assignment);
            } else {
                command_words.words.extend(analyzed.into_expanded_words());
            }
        }
        CommandPrefixOrSuffixItem::ProcessSubstitution(_, command) => {
            eval_list(&command.list, vec![state.clone()], analysis, depth + 1);
        }
    }
}

fn eval_compound_command(
    command: &CompoundCommand,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    match command {
        CompoundCommand::Arithmetic(_) => states,
        CompoundCommand::ArithmeticForClause(command) => {
            eval_loop(None, &command.body.list, states, analysis, depth)
        }
        CompoundCommand::BraceGroup(command) => {
            eval_list(&command.list, states, analysis, depth + 1)
        }
        CompoundCommand::Subshell(command) => {
            eval_list(&command.list, states.clone(), analysis, depth + 1);
            states
        }
        CompoundCommand::ForClause(command) => {
            for state in &states {
                if let Some(values) = &command.values {
                    for value in values {
                        let word = shell_word(&value.value, state);
                        analyze_embedded_commands(&word, state, analysis, depth);
                    }
                }
            }
            eval_loop(None, &command.body.list, states, analysis, depth)
        }
        CompoundCommand::CaseClause(command) => {
            for state in &states {
                let value = shell_word(&command.value.value, state);
                analyze_embedded_commands(&value, state, analysis, depth);
            }
            let mut output = states.clone();
            for case in &command.cases {
                for state in &states {
                    for pattern in &case.patterns {
                        let word = shell_word(&pattern.value, state);
                        analyze_embedded_commands(&word, state, analysis, depth);
                    }
                }
                if let Some(body) = &case.cmd {
                    output =
                        union_states(output, eval_list(body, states.clone(), analysis, depth + 1));
                }
            }
            output
        }
        CompoundCommand::IfClause(command) => eval_if_clause(command, states, analysis, depth),
        CompoundCommand::WhileClause(command) | CompoundCommand::UntilClause(command) => {
            eval_loop(Some(&command.0), &command.1.list, states, analysis, depth)
        }
        CompoundCommand::Coprocess(command) => {
            eval_command(&command.body, states.clone(), analysis, depth + 1);
            states
        }
    }
}

fn eval_if_clause(
    command: &ast::IfClauseCommand,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    let condition_states = eval_list(&command.condition, states, analysis, depth + 1);
    let mut output = eval_list(&command.then, condition_states.clone(), analysis, depth + 1);
    let mut unmatched = condition_states;
    if let Some(elses) = &command.elses {
        for else_clause in elses {
            if let Some(condition) = &else_clause.condition {
                let conditioned = eval_list(condition, unmatched, analysis, depth + 1);
                output = union_states(
                    output,
                    eval_list(&else_clause.body, conditioned.clone(), analysis, depth + 1),
                );
                unmatched = conditioned;
            } else {
                output = union_states(
                    output,
                    eval_list(&else_clause.body, unmatched, analysis, depth + 1),
                );
                return output;
            }
        }
    }
    union_states(output, unmatched)
}

fn eval_loop(
    condition: Option<&ast::CompoundList>,
    body: &ast::CompoundList,
    states: Vec<ShellState>,
    analysis: &mut ShellAnalysis,
    depth: usize,
) -> Vec<ShellState> {
    let entry = if let Some(condition) = condition {
        eval_list(condition, states, analysis, depth + 1)
    } else {
        states
    };
    let mut output = entry.clone();
    let mut frontier = entry;
    for _ in 0..LOOP_UNROLL_LIMIT {
        let body_states = eval_list(body, frontier, analysis, depth + 1);
        let next = if let Some(condition) = condition {
            eval_list(condition, body_states, analysis, depth + 1)
        } else {
            body_states
        };
        let new_states = next
            .iter()
            .filter(|state| !output.contains(state))
            .cloned()
            .collect::<Vec<_>>();
        output = union_states(output, next);
        if new_states.is_empty() {
            return output;
        }
        frontier = new_states;
    }
    if output.len() > 1 {
        let widened = merge_shell_states(&output);
        output = union_states(output, vec![widened]);
    }
    output
}

fn analyze_redirect_list(
    redirects: Option<&ast::RedirectList>,
    states: &[ShellState],
    analysis: &mut ShellAnalysis,
    depth: usize,
) {
    let Some(redirects) = redirects else {
        return;
    };
    for state in states {
        for redirect in &redirects.0 {
            analyze_redirect(redirect, state, analysis, depth);
        }
    }
}

fn analyze_redirect(
    redirect: &IoRedirect,
    state: &ShellState,
    analysis: &mut ShellAnalysis,
    depth: usize,
) {
    match redirect {
        IoRedirect::File(_, kind, target) => {
            let access = match kind {
                IoFileRedirectKind::Read | IoFileRedirectKind::DuplicateInput => PathAccess::Read,
                IoFileRedirectKind::Write
                | IoFileRedirectKind::Append
                | IoFileRedirectKind::Clobber
                | IoFileRedirectKind::DuplicateOutput => PathAccess::Write,
                IoFileRedirectKind::ReadAndWrite => PathAccess::Unknown,
            };
            match target {
                IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                    if matches!(target, IoFileRedirectTarget::Duplicate(_))
                        && (word.value == "-" || word.value.chars().all(|ch| ch.is_ascii_digit()))
                    {
                        return;
                    }
                    let word = shell_word(&word.value, state);
                    analyze_embedded_commands(&word, state, analysis, depth);
                    push_path(
                        &mut analysis.paths,
                        &word,
                        &state.cwd,
                        access,
                        PathTargetKind::Unknown,
                    );
                }
                IoFileRedirectTarget::ProcessSubstitution(_, command) => {
                    eval_list(&command.list, vec![state.clone()], analysis, depth + 1);
                }
                IoFileRedirectTarget::Fd(_) => {}
            }
        }
        IoRedirect::HereDocument(_, heredoc) => {
            if heredoc.requires_expansion {
                analyze_heredoc_commands(&heredoc.doc.value, state, analysis, depth);
            }
        }
        IoRedirect::HereString(_, word) | IoRedirect::OutputAndError(word, _) => {
            let word = shell_word(&word.value, state);
            analyze_embedded_commands(&word, state, analysis, depth);
            if matches!(redirect, IoRedirect::OutputAndError(_, _)) {
                push_path(
                    &mut analysis.paths,
                    &word,
                    &state.cwd,
                    PathAccess::Write,
                    PathTargetKind::Unknown,
                );
            }
        }
    }
}

fn analyze_heredoc_commands(
    value: &str,
    state: &ShellState,
    analysis: &mut ShellAnalysis,
    depth: usize,
) {
    let Some(pieces) = shell_parse::parse_heredoc_word(value) else {
        record_unknown_shell(value, &state.cwd, analysis);
        return;
    };
    for command in shell_parse::embedded_commands(&pieces) {
        analyze_embedded_command(command, state, analysis, depth);
    }
}

fn analyze_embedded_commands(
    word: &ShellWord,
    state: &ShellState,
    analysis: &mut ShellAnalysis,
    depth: usize,
) {
    if !word.alternatives.is_empty() {
        for alternative in &word.alternatives {
            analyze_embedded_commands(alternative, state, analysis, depth);
        }
        return;
    }
    for command in &word.embedded_commands {
        analyze_embedded_command(command, state, analysis, depth);
    }
}

fn analyze_embedded_command(
    command: &str,
    state: &ShellState,
    analysis: &mut ShellAnalysis,
    depth: usize,
) {
    let Some(program) = shell_parse::parse(command) else {
        record_unknown_shell(command, &state.cwd, analysis);
        return;
    };
    eval_program(&program, vec![state.clone()], analysis, depth + 1);
}

fn analyze_extended_test(
    expr: &ast::ExtendedTestExpr,
    state: &ShellState,
    analysis: &mut ShellAnalysis,
    depth: usize,
) {
    match expr {
        ast::ExtendedTestExpr::And(left, right) | ast::ExtendedTestExpr::Or(left, right) => {
            analyze_extended_test(left, state, analysis, depth);
            analyze_extended_test(right, state, analysis, depth);
        }
        ast::ExtendedTestExpr::Not(expr) | ast::ExtendedTestExpr::Parenthesized(expr) => {
            analyze_extended_test(expr, state, analysis, depth);
        }
        ast::ExtendedTestExpr::UnaryTest(predicate, word) => {
            let word = shell_word(&word.value, state);
            analyze_embedded_commands(&word, state, analysis, depth);
            if !matches!(
                predicate,
                ast::UnaryPredicate::FdIsOpenTerminal
                    | ast::UnaryPredicate::ShellOptionEnabled
                    | ast::UnaryPredicate::ShellVariableIsSetAndAssigned
                    | ast::UnaryPredicate::ShellVariableIsSetAndNameRef
                    | ast::UnaryPredicate::StringHasZeroLength
                    | ast::UnaryPredicate::StringHasNonZeroLength
            ) {
                push_path(
                    &mut analysis.paths,
                    &word,
                    &state.cwd,
                    PathAccess::Read,
                    PathTargetKind::Unknown,
                );
            }
        }
        ast::ExtendedTestExpr::BinaryTest(predicate, left, right) => {
            let left = shell_word(&left.value, state);
            let right = shell_word(&right.value, state);
            analyze_embedded_commands(&left, state, analysis, depth);
            analyze_embedded_commands(&right, state, analysis, depth);
            if matches!(
                predicate,
                ast::BinaryPredicate::FilesReferToSameDeviceAndInodeNumbers
                    | ast::BinaryPredicate::LeftFileIsNewerOrExistsWhenRightDoesNot
                    | ast::BinaryPredicate::LeftFileIsOlderOrDoesNotExistWhenRightDoes
            ) {
                for word in [&left, &right] {
                    push_path(
                        &mut analysis.paths,
                        word,
                        &state.cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
            }
        }
    }
}

fn union_states(mut first: Vec<ShellState>, second: Vec<ShellState>) -> Vec<ShellState> {
    first.extend(second);
    normalize_states(first)
}

fn normalize_states(states: Vec<ShellState>) -> Vec<ShellState> {
    let mut unique = Vec::new();
    for state in states {
        if !unique.contains(&state) {
            unique.push(state);
        }
    }
    if unique.len() <= MAX_SHELL_STATES {
        return unique;
    }
    let summary = merge_shell_states(&unique);
    unique.truncate(MAX_SHELL_STATES - 1);
    if !unique.contains(&summary) {
        unique.push(summary);
    }
    unique
}

fn merge_shell_states(states: &[ShellState]) -> ShellState {
    let first = &states[0];
    let cwd = if states.iter().all(|state| state.cwd == first.cwd) {
        first.cwd.clone()
    } else {
        PathResolution::Unresolved(first.cwd.path().to_path_buf())
    };
    let variable_names: HashSet<_> = states
        .iter()
        .flat_map(|state| state.variables.keys().cloned())
        .collect();
    let variables = variable_names
        .into_iter()
        .map(|name| {
            let value = first.variable(&name);
            let value = states
                .iter()
                .skip(1)
                .all(|state| state.variable(&name) == value)
                .then_some(value)
                .flatten();
            (name, value)
        })
        .collect();
    ShellState {
        cwd,
        home: first.home.clone(),
        variables,
    }
}

fn apply_assignments(state: &mut ShellState, words: &[ShellWord]) {
    for word in words {
        if let Some((name, value)) = word.assignment() {
            state.set_variable(name, value);
        }
    }
}

fn effective_command_words(mut words: &[ShellWord]) -> &[ShellWord] {
    loop {
        match words.first().map(ShellWord::raw) {
            Some("command") => {
                words = &words[1..];
                if words
                    .iter()
                    .take_while(|word| word.raw().starts_with('-'))
                    .any(|word| matches!(word.raw(), "-v" | "-V"))
                {
                    return &[];
                }
                while words
                    .first()
                    .is_some_and(|word| matches!(word.raw(), "--" | "-p"))
                {
                    words = &words[1..];
                }
            }
            Some("builtin" | "exec") => {
                words = &words[1..];
                if words.first().is_some_and(|word| word.raw() == "--") {
                    words = &words[1..];
                }
            }
            Some("env") => {
                let env_words = words;
                words = &words[1..];
                if words.first().is_some_and(|word| word.raw() == "--") {
                    words = &words[1..];
                }
                if words
                    .first()
                    .is_some_and(|word| word.raw().starts_with('-'))
                {
                    return env_words;
                }
                while words
                    .first()
                    .is_some_and(|word| word.assignment().is_some())
                {
                    words = &words[1..];
                }
            }
            _ => return words,
        }
    }
}

fn apply_cd(state: &mut ShellState, words: &[ShellWord], paths: &mut Vec<PathEffect>) {
    let mut options_done = false;
    let target = words.iter().skip(1).find_map(|word| {
        if !options_done && word.raw() == "--" {
            options_done = true;
            None
        } else if !options_done && word.raw() == "-" {
            Some(ShellWord {
                raw: word.raw.clone(),
                expanded: state.variable("OLDPWD"),
                has_glob: false,
                embedded_commands: Vec::new(),
                alternatives: Vec::new(),
            })
        } else if !options_done && word.raw().starts_with('-') {
            None
        } else {
            Some(word.clone())
        }
    });
    let target = target.unwrap_or_else(|| ShellWord {
        raw: "~".to_string(),
        expanded: state.variable("HOME"),
        has_glob: false,
        embedded_commands: Vec::new(),
        alternatives: Vec::new(),
    });
    let previous_cwd = state.cwd.clone();
    let (effects, target) = directory_operand_effects(&target, &state.cwd, PathAccess::Unknown);
    paths.extend(effects);
    let Some(target) = target else {
        return;
    };
    match &target {
        PathResolution::Resolved(path) if path.is_dir() => {
            let pwd = path.to_string_lossy().into_owned();
            state.cwd = target;
            state.set_variable(
                "OLDPWD".to_string(),
                previous_cwd
                    .resolved()
                    .map(|cwd| cwd.to_string_lossy().into_owned()),
            );
            state.set_variable("PWD".to_string(), Some(pwd));
        }
        PathResolution::Resolved(_) => {}
        PathResolution::Unresolved(_) => {
            state.cwd = target;
            state.set_variable("OLDPWD".to_string(), None);
            state.set_variable("PWD".to_string(), None);
        }
    }
}

fn merge_risk(a: ShellRisk, b: ShellRisk) -> ShellRisk {
    use ShellRisk::*;
    match (a, b) {
        (Destructive, _) | (_, Destructive) => Destructive,
        (Writes, _) | (_, Writes) => Writes,
        (Unknown, _) | (_, Unknown) => Unknown,
        (ReadOnly, ReadOnly) => ReadOnly,
    }
}

struct CommandSpec {
    risk: CommandRisk,
    operands: OperandPolicy,
}

#[derive(Default)]
struct CommandEffects {
    paths: Vec<PathEffect>,
    opaque_commands: Vec<OpaqueShellCommand>,
}

enum CommandRisk {
    Fixed(ShellRisk),
    Sed,
    Perl,
    Git,
    Cargo,
}

enum OperandPolicy {
    None,
    ReadOperands,
    UnknownOperands,
    ReadExplicit,
    UnknownExplicit,
    Specialized(fn(&[ShellWord], &PathResolution) -> Vec<PathEffect>),
    Awk,
    Env,
}

impl CommandRisk {
    fn classify(self, words: &[ShellWord]) -> ShellRisk {
        match self {
            Self::Fixed(risk) => risk,
            Self::Sed => {
                if words
                    .iter()
                    .any(|word| word.raw() == "-i" || word.raw().starts_with("-i"))
                {
                    ShellRisk::Writes
                } else {
                    ShellRisk::ReadOnly
                }
            }
            Self::Perl => {
                if words
                    .iter()
                    .any(|word| matches!(word.raw(), "-pi" | "-p -i"))
                {
                    ShellRisk::Writes
                } else {
                    ShellRisk::Unknown
                }
            }
            Self::Git => match words.get(1).map(ShellWord::raw) {
                Some(
                    "commit" | "reset" | "checkout" | "clean" | "stash" | "apply" | "am" | "merge"
                    | "rebase",
                ) => ShellRisk::Writes,
                Some("status" | "diff" | "log" | "show" | "grep" | "ls-files") => {
                    ShellRisk::ReadOnly
                }
                _ => ShellRisk::Unknown,
            },
            Self::Cargo => match cargo_subcommand(words) {
                Some("metadata" | "tree" | "version" | "--version" | "-V") => ShellRisk::ReadOnly,
                Some(
                    "build" | "check" | "test" | "run" | "bench" | "doc" | "clippy" | "fmt" | "fix"
                    | "clean" | "install" | "add" | "remove" | "update" | "publish" | "nextest"
                    | "llvm-cov" | "xtask",
                ) => ShellRisk::Writes,
                _ => ShellRisk::Unknown,
            },
        }
    }
}

impl OperandPolicy {
    fn effects(self, words: &[ShellWord], state: &ShellState) -> CommandEffects {
        let paths = match self {
            Self::None => Vec::new(),
            Self::ReadOperands => operand_paths(words, &state.cwd, PathAccess::Read),
            Self::UnknownOperands => operand_paths(words, &state.cwd, PathAccess::Unknown),
            Self::ReadExplicit => explicit_paths(words, &state.cwd, PathAccess::Read),
            Self::UnknownExplicit => explicit_paths(words, &state.cwd, PathAccess::Unknown),
            Self::Specialized(analyze) => analyze(words, &state.cwd),
            Self::Awk => return awk::analyze(words, state),
            Self::Env => return env_effects(words, state),
        };
        CommandEffects {
            paths,
            opaque_commands: Vec::new(),
        }
    }
}

fn command_spec(command: &str) -> CommandSpec {
    use OperandPolicy::*;

    match command {
        "rm" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Destructive),
            operands: Specialized(rm_paths),
        },
        "rmdir" | "mv" | "chmod" | "chown" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Destructive),
            operands: UnknownOperands,
        },
        "cp" | "touch" | "ln" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Writes),
            operands: UnknownOperands,
        },
        "mkdir" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Writes),
            operands: Specialized(mkdir_paths),
        },
        "sed" => CommandSpec {
            risk: CommandRisk::Sed,
            operands: Specialized(sed_paths),
        },
        "perl" => CommandSpec {
            risk: CommandRisk::Perl,
            operands: UnknownExplicit,
        },
        "git" => CommandSpec {
            risk: CommandRisk::Git,
            operands: Specialized(git_paths),
        },
        "cargo" => CommandSpec {
            risk: CommandRisk::Cargo,
            operands: Specialized(cargo_paths),
        },
        "env" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Unknown),
            operands: Env,
        },
        "[" | "[[" | "test" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: Specialized(test_paths),
        },
        "." | "source" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Unknown),
            operands: ReadOperands,
        },
        "ssh" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Unknown),
            operands: Specialized(ssh_paths),
        },
        "grep" | "rg" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: Specialized(grep_paths),
        },
        "awk" | "gawk" | "mawk" | "nawk" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Unknown),
            operands: Awk,
        },
        "find" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: Specialized(find_paths),
        },
        "ls" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: Specialized(ls_paths),
        },
        "cat" | "df" | "diff" | "du" | "file" | "head" | "hexdump" | "less" | "md5sum"
        | "realpath" | "sha256sum" | "sort" | "stat" | "strings" | "tail" | "tree" | "uniq"
        | "wc" | "xxd" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: ReadOperands,
        },
        "cut" | "date" | "jq" | "tr" | "which" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: ReadExplicit,
        },
        ":" | "alias" | "basename" | "break" | "caller" | "continue" | "declare" | "dirname"
        | "echo" | "exit" | "export" | "false" | "getopts" | "hash" | "help" | "jobs" | "let"
        | "local" | "printf" | "pwd" | "read" | "readonly" | "return" | "set" | "shift"
        | "shopt" | "true" | "typeset" | "ulimit" | "umask" | "unalias" | "unset" | "wait"
        | "whoami" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::ReadOnly),
            operands: None,
        },
        "curl" | "wget" | "scp" | "rsync" | "python" | "python3" | "node" | "ruby" | "bash"
        | "sh" => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Unknown),
            operands: UnknownExplicit,
        },
        _ => CommandSpec {
            risk: CommandRisk::Fixed(ShellRisk::Unknown),
            operands: UnknownExplicit,
        },
    }
}

fn classify_risk(words: &[ShellWord]) -> ShellRisk {
    let Some(command) = words.first().map(command_name) else {
        return ShellRisk::ReadOnly;
    };
    command_spec(command).risk.classify(words)
}

fn cargo_subcommand(words: &[ShellWord]) -> Option<&str> {
    words
        .iter()
        .skip(1)
        .map(ShellWord::raw)
        .find(|word| !word.starts_with('+') && !word.starts_with('-'))
}

fn command_effects(words: &[ShellWord], state: &ShellState) -> CommandEffects {
    let Some(command) = words.first().map(command_name) else {
        return CommandEffects::default();
    };
    command_spec(command).operands.effects(words, state)
}

fn test_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    const UNARY_PATH_OPERATORS: &[&str] = &[
        "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-k", "-L", "-N", "-O", "-G", "-p", "-r",
        "-S", "-s", "-u", "-w", "-x",
    ];
    const BINARY_PATH_OPERATORS: &[&str] = &["-ef", "-nt", "-ot"];

    let mut out = Vec::new();
    for (index, word) in words.iter().enumerate().skip(1) {
        if UNARY_PATH_OPERATORS.contains(&word.raw()) {
            if let Some(path) = words.get(index + 1) {
                push_path(
                    &mut out,
                    path,
                    cwd,
                    PathAccess::Read,
                    PathTargetKind::Unknown,
                );
            }
        } else if BINARY_PATH_OPERATORS.contains(&word.raw()) {
            for path in [
                index.checked_sub(1).and_then(|i| words.get(i)),
                words.get(index + 1),
            ]
            .into_iter()
            .flatten()
            {
                push_path(
                    &mut out,
                    path,
                    cwd,
                    PathAccess::Read,
                    PathTargetKind::Unknown,
                );
            }
        }
    }
    out
}

fn command_name(word: &ShellWord) -> &str {
    let name = word.expanded.as_deref().unwrap_or_else(|| word.raw());
    Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name)
}

fn cwd_after_chdir(current: &PathResolution, target: &PathResolution) -> PathResolution {
    match target {
        PathResolution::Resolved(path) if path.is_dir() => target.clone(),
        PathResolution::Resolved(_) => current.clone(),
        PathResolution::Unresolved(_) => target.clone(),
    }
}

fn env_effects(words: &[ShellWord], state: &ShellState) -> CommandEffects {
    let cwd = &state.cwd;
    let mut paths = Vec::new();
    let mut command_state = state.clone();
    let mut command_cwd = cwd.clone();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "--" => {
                i += 1;
                break;
            }
            "-C" | "--chdir" => {
                if let Some(path) = words.get(i + 1) {
                    let (effects, target) = directory_operand_effects(path, cwd, PathAccess::Read);
                    if let Some(target) = target {
                        command_cwd = cwd_after_chdir(cwd, &target);
                    }
                    paths.extend(effects);
                }
                i += 2;
            }
            option if option.starts_with("--chdir=") => {
                if let Some(path) = words[i].strip_literal_prefix("--chdir=") {
                    let effect = if path.has_glob {
                        unresolved_path_effect(
                            &path,
                            cwd,
                            PathAccess::Read,
                            PathTargetKind::Directory,
                        )
                    } else {
                        path_effect(&path, cwd, PathAccess::Read, PathTargetKind::Directory)
                    };
                    command_cwd = cwd_after_chdir(cwd, &effect.resolution);
                    paths.push(effect);
                }
                i += 1;
            }
            "-f" | "--file" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut paths,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::File,
                    );
                }
                i += 2;
            }
            option if option.starts_with("--file=") => {
                if let Some(path) = words[i].strip_literal_prefix("--file=") {
                    push_path(
                        &mut paths,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::File,
                    );
                }
                i += 1;
            }
            "-u" | "--unset" => {
                if let Some(name) = words.get(i + 1).and_then(|word| word.expanded.as_deref()) {
                    command_state.unset_variable(name);
                }
                i += 2;
            }
            option if option.starts_with("--unset=") => {
                if let Some(name) = words[i]
                    .expanded
                    .as_deref()
                    .and_then(|word| word.strip_prefix("--unset="))
                {
                    command_state.unset_variable(name);
                }
                i += 1;
            }
            "-a" | "--argv0" => i += 2,
            option if option.starts_with("--argv0=") => i += 1,
            "-i"
            | "--ignore-environment"
            | "-0"
            | "--null"
            | "-v"
            | "--debug"
            | "--list-signal-handling"
            | "-" => i += 1,
            option
                if option.starts_with("--ignore-signal")
                    || option.starts_with("--default-signal")
                    || option.starts_with("--block-signal") =>
            {
                i += 1;
            }
            "-h" | "--help" | "-V" | "--version" => {
                return CommandEffects {
                    paths,
                    opaque_commands: Vec::new(),
                };
            }
            "-S" | "--split-string" => return opaque_env_effects(words, paths),
            option if option.starts_with("--split-string=") => {
                return opaque_env_effects(words, paths);
            }
            option if option.starts_with('-') => return opaque_env_effects(words, paths),
            _ => {
                if let Some((name, value)) = words[i].assignment() {
                    command_state.set_variable(name, value);
                    i += 1;
                } else {
                    break;
                }
            }
        }
    }
    command_state.cwd = command_cwd;
    let mut effects = command_effects(&words[i..], &command_state);
    paths.append(&mut effects.paths);
    effects.paths = paths;
    effects
}

fn opaque_env_effects(words: &[ShellWord], paths: Vec<PathEffect>) -> CommandEffects {
    let command = words.first().map(command_name).unwrap_or("env");
    CommandEffects {
        paths,
        opaque_commands: vec![OpaqueShellCommand {
            command: format!("{command} *"),
        }],
    }
}

fn git_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-C" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Unknown,
                        PathTargetKind::Directory,
                    );
                }
                i += 2;
            }
            "commit" => {
                i += 1;
                while i < words.len() {
                    match words[i].raw() {
                        "-m" | "--message" => i += 2,
                        "-F" | "--file" => {
                            if let Some(path) = words.get(i + 1) {
                                push_path(
                                    &mut out,
                                    path,
                                    cwd,
                                    PathAccess::Read,
                                    PathTargetKind::Unknown,
                                );
                            }
                            i += 2;
                        }
                        w if w.starts_with("--message=") => i += 1,
                        w if w.starts_with("--file=") => {
                            if let Some(path) = words[i].strip_literal_prefix("--file=") {
                                push_path(
                                    &mut out,
                                    &path,
                                    cwd,
                                    PathAccess::Read,
                                    PathTargetKind::Unknown,
                                );
                            }
                            i += 1;
                        }
                        _ => {
                            maybe_push_explicit_path(&mut out, &words[i], cwd, PathAccess::Unknown);
                            i += 1;
                        }
                    }
                }
            }
            _ => {
                maybe_push_explicit_path(&mut out, &words[i], cwd, PathAccess::Unknown);
                i += 1;
            }
        }
    }
    out
}

fn cargo_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "--path" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Directory,
                    );
                }
                i += 2;
            }
            "--root" | "--target-dir" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Directory,
                    );
                }
                i += 2;
            }
            w if w.starts_with("--path=") => {
                if let Some(path) = words[i].strip_literal_prefix("--path=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Directory,
                    );
                }
                i += 1;
            }
            w if w.starts_with("--root=") => {
                if let Some(path) = words[i].strip_literal_prefix("--root=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Directory,
                    );
                }
                i += 1;
            }
            w if w.starts_with("--target-dir=") => {
                if let Some(path) = words[i].strip_literal_prefix("--target-dir=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Directory,
                    );
                }
                i += 1;
            }
            _ => {
                maybe_push_explicit_path(&mut out, &words[i], cwd, PathAccess::Unknown);
                i += 1;
            }
        }
    }
    out
}

fn ssh_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-i" | "-F" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += 2;
            }
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    out
}

fn sed_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut saw_script = false;
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-e" | "--expression" => {
                saw_script = true;
                i += 2;
            }
            "-f" | "--file" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                saw_script = true;
                i += 2;
            }
            "-l" | "--line-length" => i += 2,
            option if option.starts_with("--expression=") => {
                saw_script = true;
                i += 1;
            }
            option if option.starts_with("--file=") => {
                if let Some(path) = words[i].strip_literal_prefix("--file=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                saw_script = true;
                i += 1;
            }
            option if option.starts_with('-') => i += 1,
            _ if !saw_script => {
                saw_script = true;
                i += 1;
            }
            _ => {
                push_path(
                    &mut out,
                    &words[i],
                    cwd,
                    PathAccess::Unknown,
                    PathTargetKind::Unknown,
                );
                i += 1;
            }
        }
    }
    out
}

fn grep_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut saw_pattern = false;
    let mut options_done = false;
    let command = words.first().map(command_name).unwrap_or_default();
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        if !options_done && word.raw() == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done {
            let (option, attached) = word
                .raw()
                .split_once('=')
                .map_or((word.raw(), None), |(option, _)| {
                    (option, word.strip_literal_prefix(&format!("{option}=")))
                });
            if matches!(option, "-e" | "--regexp") {
                saw_pattern = true;
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if matches!(option, "-f" | "--file") {
                saw_pattern = true;
                if let Some(path) = attached.as_ref().or_else(|| words.get(i + 1)) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if matches!(option, "--exclude-from")
                || (command == "rg" && matches!(option, "--ignore-file"))
            {
                if let Some(path) = attached.as_ref().or_else(|| words.get(i + 1)) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if grep_option_takes_value(command, option) {
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if let Some(prefix) = ["-e", "-f"]
                .into_iter()
                .find(|prefix| word.raw().starts_with(prefix) && word.raw().len() > prefix.len())
            {
                if prefix == "-e" {
                    saw_pattern = true;
                } else if let Some(path) = word.strip_literal_prefix(prefix) {
                    saw_pattern = true;
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += 1;
                continue;
            }
            if word.raw().starts_with('-') {
                i += 1;
                continue;
            }
        }
        if !saw_pattern {
            saw_pattern = true;
        } else {
            push_path(
                &mut out,
                word,
                cwd,
                PathAccess::Read,
                PathTargetKind::Unknown,
            );
        }
        i += 1;
    }
    out
}

fn grep_option_takes_value(command: &str, option: &str) -> bool {
    matches!(
        option,
        "-A" | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "--binary-files"
            | "-D"
            | "--devices"
            | "-d"
            | "--directories"
            | "--exclude"
            | "--exclude-dir"
            | "--include"
            | "--label"
            | "-m"
            | "--max-count"
    ) || (command == "rg"
        && matches!(
            option,
            "--color"
                | "--colors"
                | "--encoding"
                | "--engine"
                | "-g"
                | "--glob"
                | "-j"
                | "--threads"
                | "-M"
                | "--max-columns"
                | "--path-separator"
                | "--pre"
                | "--pre-glob"
                | "-r"
                | "--replace"
                | "--sort"
                | "--sortr"
                | "-t"
                | "--type"
                | "--type-add"
                | "--type-clear"
        ))
}

fn find_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        match word.raw() {
            "--" => {
                i += 1;
                break;
            }
            "-H" | "-L" | "-P" => i += 1,
            "-D" => i += 2,
            option if option.starts_with("-O") => i += 1,
            option if option.starts_with('-') => return out,
            _ => break,
        }
    }
    while let Some(word) = words.get(i) {
        if word.raw().starts_with('-') || matches!(word.raw(), "!" | "(" | ")") {
            break;
        }
        push_path(
            &mut out,
            word,
            cwd,
            PathAccess::Read,
            PathTargetKind::Directory,
        );
        i += 1;
    }
    out
}

fn rm_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut recursive = false;
    let mut options_done = false;
    let mut operands = Vec::new();
    for word in words.iter().skip(1) {
        let value = word.raw();
        if !options_done && value == "--" {
            options_done = true;
        } else if !options_done && value.starts_with('-') {
            recursive |= value == "--recursive"
                || value.strip_prefix('-').is_some_and(|short| {
                    !short.starts_with('-') && short.chars().any(|ch| matches!(ch, 'r' | 'R'))
                });
        } else {
            operands.push(word);
        }
    }

    let target_kind = if recursive {
        PathTargetKind::Directory
    } else {
        PathTargetKind::Unknown
    };
    let mut out = Vec::new();
    for word in operands {
        push_path(
            &mut out,
            word,
            cwd,
            PathAccess::Unknown,
            target_kind.clone(),
        );
    }
    out
}

fn mkdir_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-m" | "--mode" | "-Z" | "--context" => i += 2,
            w if w.starts_with("--mode=") || w.starts_with("--context=") => i += 1,
            w if w.starts_with('-') => i += 1,
            _ => {
                push_path(
                    &mut out,
                    &words[i],
                    cwd,
                    PathAccess::Write,
                    PathTargetKind::Directory,
                );
                i += 1;
            }
        }
    }
    out
}

fn ls_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut options_done = false;
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        if !options_done && word.raw() == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done {
            if option_value_kind("ls", word.raw()).is_some() {
                i += 2;
                continue;
            }
            if word.raw().starts_with('-') {
                i += 1;
                continue;
            }
        }
        push_path(
            &mut out,
            word,
            cwd,
            PathAccess::Read,
            PathTargetKind::Directory,
        );
        i += 1;
    }
    out
}

#[derive(Clone, Copy)]
enum OptionValueKind {
    Ignore,
    ReadPath,
    WritePath,
    ReadDirectory,
    WriteDirectory,
}

fn option_value_kind(command: &str, option: &str) -> Option<OptionValueKind> {
    use OptionValueKind::*;
    match command {
        "df" => match option {
            "-B" | "--block-size" | "-t" | "--type" | "-x" | "--exclude-type" => Some(Ignore),
            _ => None,
        },
        "diff" => match option {
            "-C"
            | "--context"
            | "-U"
            | "--unified"
            | "--label"
            | "--horizon-lines"
            | "--tabsize"
            | "--width"
            | "-I"
            | "--ignore-matching-lines" => Some(Ignore),
            "--from-file" | "--to-file" => Some(ReadPath),
            _ => None,
        },
        "du" => match option {
            "-B" | "--block-size" | "-d" | "--max-depth" | "-t" | "--threshold" | "--exclude" => {
                Some(Ignore)
            }
            "--exclude-from" | "--files0-from" => Some(ReadPath),
            _ => None,
        },
        "file" => match option {
            "-e" | "--exclude" | "--exclude-quiet" | "-P" | "--parameter" | "--separator" => {
                Some(Ignore)
            }
            "-f" | "--files-from" | "-m" | "--magic-file" => Some(ReadPath),
            _ => None,
        },
        "head" => match option {
            "-c" | "--bytes" | "-n" | "--lines" => Some(Ignore),
            _ => None,
        },
        "hexdump" => match option {
            "-e" | "--format" => Some(Ignore),
            "-f" | "--format-file" => Some(ReadPath),
            _ => None,
        },
        "less" => match option {
            "-j" | "--jump-target" | "-p" | "--pattern" | "-t" | "--tag" | "-x" | "--tabs" => {
                Some(Ignore)
            }
            "-k" | "--lesskey-file" | "-T" | "--tag-file" => Some(ReadPath),
            _ => None,
        },
        "ls" => match option {
            "--block-size" | "--format" | "--hide" | "-I" | "--ignore" | "--indicator-style"
            | "--quoting-style" | "--sort" | "-T" | "--tabsize" | "--time" | "--time-style"
            | "-w" | "--width" => Some(Ignore),
            _ => None,
        },
        "realpath" => match option {
            "--relative-to" | "--relative-base" => Some(ReadDirectory),
            _ => None,
        },
        "sort" => match option {
            "--batch-size" | "-S" | "--buffer-size" | "--compress-program" | "-k" | "--key"
            | "--parallel" | "-t" | "--field-separator" => Some(Ignore),
            "--random-source" => Some(ReadPath),
            "-o" | "--output" => Some(WritePath),
            "-T" | "--temporary-directory" => Some(WriteDirectory),
            _ => None,
        },
        "stat" => match option {
            "-c" | "--format" | "--printf" => Some(Ignore),
            _ => None,
        },
        "strings" => match option {
            "-e" | "--encoding" | "-n" | "--bytes" | "-s" | "--output-separator" | "-t"
            | "--radix" => Some(Ignore),
            _ => None,
        },
        "tail" => match option {
            "-c"
            | "--bytes"
            | "-n"
            | "--lines"
            | "--max-unchanged-stats"
            | "--pid"
            | "-s"
            | "--sleep-interval" => Some(Ignore),
            _ => None,
        },
        "tree" => match option {
            "-I" | "-L" | "-P" | "--charset" | "--filelimit" | "--sort" | "--timefmt" => {
                Some(Ignore)
            }
            "-o" => Some(WritePath),
            _ => None,
        },
        "uniq" => match option {
            "-f" | "--skip-fields" | "-s" | "--skip-chars" | "-w" | "--check-chars" => Some(Ignore),
            _ => None,
        },
        "wc" => match option {
            "--files0-from" => Some(ReadPath),
            _ => None,
        },
        "xxd" => match option {
            "-c" | "-g" | "-l" | "-o" | "-s" => Some(Ignore),
            _ => None,
        },
        "cp" | "mv" | "ln" => match option {
            "-S" | "--suffix" => Some(Ignore),
            "-t" | "--target-directory" => Some(WriteDirectory),
            _ => None,
        },
        "touch" => match option {
            "-d" | "--date" | "-t" => Some(Ignore),
            "-r" | "--reference" => Some(ReadPath),
            _ => None,
        },
        "chmod" | "chown" => match option {
            "--reference" => Some(ReadPath),
            _ => None,
        },
        _ => None,
    }
}

fn push_option_path(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    cwd: &PathResolution,
    kind: OptionValueKind,
) {
    use OptionValueKind::*;
    let (access, target_kind) = match kind {
        Ignore => return,
        ReadPath => (PathAccess::Read, PathTargetKind::Unknown),
        WritePath => (PathAccess::Write, PathTargetKind::Unknown),
        ReadDirectory => (PathAccess::Read, PathTargetKind::Directory),
        WriteDirectory => (PathAccess::Write, PathTargetKind::Directory),
    };
    push_path(out, word, cwd, access, target_kind);
}

fn operand_paths(words: &[ShellWord], cwd: &PathResolution, access: PathAccess) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut options_done = false;
    let mut i = 1;
    let command = words.first().map(command_name).unwrap_or_default();
    while i < words.len() {
        let word = &words[i];
        if !options_done && word.raw() == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done {
            if let Some((option, _)) = word.raw().split_once('=') {
                if let Some(kind) = option_value_kind(command, option) {
                    if let Some(value) = word.strip_literal_prefix(&format!("{option}=")) {
                        push_option_path(&mut out, &value, cwd, kind);
                    }
                    i += 1;
                    continue;
                }
            }
            if let Some(kind) = option_value_kind(command, word.raw()) {
                if let Some(value) = words.get(i + 1) {
                    push_option_path(&mut out, value, cwd, kind);
                }
                i += 2;
                continue;
            }
            if let Some(option) = word.raw().get(..2) {
                if let Some(kind) = option_value_kind(command, option) {
                    if let Some(value) = word.strip_literal_prefix(option) {
                        push_option_path(&mut out, &value, cwd, kind);
                    }
                    i += 1;
                    continue;
                }
            }
            if word.raw().starts_with('-') {
                i += 1;
                continue;
            }
        }
        push_path(&mut out, word, cwd, access.clone(), PathTargetKind::Unknown);
        i += 1;
    }
    out
}

fn explicit_paths(
    words: &[ShellWord],
    cwd: &PathResolution,
    access: PathAccess,
) -> Vec<PathEffect> {
    let mut out = Vec::new();
    for word in words.iter().skip(1) {
        maybe_push_explicit_path(&mut out, word, cwd, access.clone());
    }
    out
}

fn maybe_push_explicit_path(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
) {
    if is_explicit_path(word) {
        push_path(out, word, cwd, access, PathTargetKind::Unknown);
    }
}

fn push_path(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) {
    if !word.alternatives.is_empty() {
        for alternative in &word.alternatives {
            push_path(out, alternative, cwd, access.clone(), target_kind.clone());
        }
        return;
    }
    if is_shell_stream_word(word) {
        return;
    }
    if word.has_glob {
        if let Some(analysis) = glob_path_effects(word, cwd, access.clone(), target_kind.clone()) {
            out.extend(analysis.effects);
        } else {
            out.push(unresolved_path_effect(word, cwd, access, target_kind));
        }
    } else {
        out.push(path_effect(word, cwd, access, target_kind));
    }
}

fn glob_path_effects(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> Option<GlobPathAnalysis> {
    let pattern = absolute_glob_path(word.expanded.as_deref()?, cwd)?;
    let components: Vec<_> = pattern.components().collect();

    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    let mut out = Vec::new();
    let mut matches = Vec::new();
    let mut candidates = vec![PathBuf::new()];
    let mut saw_glob = false;
    let mut entries_seen = 0;

    // Resolve every matched component so an intermediate symlink cannot hide
    // an outside-workspace directory when the complete pattern has no match.
    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        match component {
            Component::Prefix(prefix) => {
                for candidate in &mut candidates {
                    candidate.push(prefix.as_os_str());
                }
            }
            Component::RootDir => {
                for candidate in &mut candidates {
                    candidate.push(Path::new("/"));
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                for candidate in &mut candidates {
                    candidate.push("..");
                }
            }
            Component::Normal(component) => {
                let pattern = component.to_str()?;
                let component_has_glob = pattern.chars().any(|ch| matches!(ch, '*' | '?' | '['));
                if component_has_glob {
                    saw_glob = true;
                    let pattern = glob::Pattern::new(pattern).ok()?;
                    let mut next = Vec::new();
                    for candidate in candidates {
                        push_concrete_glob_effect(
                            &mut out,
                            word,
                            &candidate,
                            cwd,
                            access.clone(),
                            PathTargetKind::Directory,
                        )?;
                        let entries = match std::fs::read_dir(&candidate) {
                            Ok(entries) => entries,
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    ErrorKind::NotFound
                                        | ErrorKind::NotADirectory
                                        | ErrorKind::PermissionDenied
                                ) =>
                            {
                                continue;
                            }
                            Err(_) => return None,
                        };
                        for entry in entries {
                            entries_seen += 1;
                            if entries_seen > MAX_GLOB_ENTRIES {
                                return None;
                            }
                            let entry = entry.ok()?;
                            let name = entry.file_name();
                            let name = name.to_str()?;
                            if pattern.matches_with(name, options) {
                                let path = entry.path();
                                let resolution = push_concrete_glob_effect(
                                    &mut out,
                                    word,
                                    &path,
                                    cwd,
                                    access.clone(),
                                    if is_last {
                                        target_kind.clone()
                                    } else {
                                        PathTargetKind::Directory
                                    },
                                )?;
                                if is_last {
                                    matches.push(resolution);
                                }
                                next.push(path);
                            }
                        }
                    }
                    if next.len() > MAX_GLOB_ENTRIES {
                        return None;
                    }
                    candidates = dedupe_paths(next);
                } else {
                    for candidate in &mut candidates {
                        candidate.push(component);
                    }
                    if saw_glob {
                        let mut next = Vec::new();
                        for candidate in candidates {
                            match std::fs::symlink_metadata(&candidate) {
                                Ok(_) => {
                                    let resolution = push_concrete_glob_effect(
                                        &mut out,
                                        word,
                                        &candidate,
                                        cwd,
                                        access.clone(),
                                        if is_last {
                                            target_kind.clone()
                                        } else {
                                            PathTargetKind::Directory
                                        },
                                    )?;
                                    if is_last {
                                        matches.push(resolution);
                                    }
                                    next.push(candidate);
                                }
                                Err(err)
                                    if matches!(
                                        err.kind(),
                                        ErrorKind::NotFound | ErrorKind::NotADirectory
                                    ) => {}
                                Err(_) => return None,
                            }
                        }
                        candidates = dedupe_paths(next);
                    }
                }
            }
        }
    }

    saw_glob.then_some(GlobPathAnalysis {
        effects: out,
        matches,
    })
}

fn directory_operand_effects(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
) -> (Vec<PathEffect>, Option<PathResolution>) {
    if !word.alternatives.is_empty() {
        let mut effects = Vec::new();
        for alternative in &word.alternatives {
            effects.extend(directory_operand_effects(alternative, cwd, access.clone()).0);
        }
        return (effects, None);
    }
    if word.has_glob {
        return match glob_path_effects(word, cwd, access.clone(), PathTargetKind::Directory) {
            Some(mut analysis) => {
                let target = if analysis.matches.len() == 1 {
                    analysis.matches.pop()
                } else {
                    None
                };
                (analysis.effects, target)
            }
            None => {
                let effect = unresolved_path_effect(word, cwd, access, PathTargetKind::Directory);
                let resolution = effect.resolution.clone();
                (vec![effect], Some(resolution))
            }
        };
    }

    let effect = path_effect(word, cwd, access, PathTargetKind::Directory);
    let resolution = effect.resolution.clone();
    (vec![effect], Some(resolution))
}

fn absolute_glob_path(path: &str, cwd: &PathResolution) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        cwd.resolved().map(|cwd| cwd.join(path))
    }
}

fn push_concrete_glob_effect(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    path: &Path,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> Option<PathResolution> {
    let path = path.to_str()?;
    let effect =
        PathEffect::from_shell_path(word.raw.clone(), Some(path), cwd, access, target_kind);
    let resolution = effect.resolution.clone();
    if !out.contains(&effect) {
        out.push(effect);
    }
    Some(resolution)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn path_effect(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> PathEffect {
    PathEffect::from_shell_path(
        word.raw.clone(),
        word.expanded.as_deref(),
        cwd,
        access,
        target_kind,
    )
}

fn unresolved_path_effect(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> PathEffect {
    path_effect(
        &ShellWord {
            raw: word.raw.clone(),
            expanded: None,
            has_glob: false,
            embedded_commands: word.embedded_commands.clone(),
            alternatives: Vec::new(),
        },
        cwd,
        access,
        target_kind,
    )
}

fn is_explicit_path(word: &ShellWord) -> bool {
    fn explicit(value: &str) -> bool {
        !value.is_empty()
            && !value.starts_with('-')
            && !value.contains("://")
            && (matches!(value, "." | ".." | "~" | "~+" | "~-")
                || value.starts_with("~/")
                || value.starts_with("~+/")
                || value.starts_with("~-/")
                || value.starts_with('/')
                || value.contains('/'))
    }
    let pure_command_substitution = (word.raw.starts_with("$(") && word.raw.ends_with(')'))
        || (word.raw.starts_with('`') && word.raw.ends_with('`'));
    (!pure_command_substitution && explicit(&word.raw))
        || word.expanded.as_deref().is_some_and(explicit)
}

fn is_shell_stream_word(word: &ShellWord) -> bool {
    is_shell_stream_path(&word.raw) || word.expanded.as_deref().is_some_and(is_shell_stream_path)
}

fn is_shell_stream_path(path: &str) -> bool {
    matches!(
        path,
        "/dev/null" | "/dev/stdin" | "/dev/stdout" | "/dev/stderr"
    )
}

fn expand_shell_tilde(value: &str, state: &ShellState) -> Option<String> {
    let (head, suffix) = value
        .split_once('/')
        .map_or((value, String::new()), |(head, suffix)| {
            (head, format!("/{suffix}"))
        });
    let home = match head {
        "~" => state.variable("HOME").map(|home| {
            if home.is_empty() {
                state.home.to_string_lossy().into_owned()
            } else {
                home
            }
        }),
        "~+" => state
            .cwd
            .resolved()
            .map(|cwd| cwd.to_string_lossy().into_owned()),
        "~-" => state.variable("OLDPWD").filter(|path| !path.is_empty()),
        user if user.starts_with('~') => {
            let user = &user[1..];
            let current_user = state
                .variable("USER")
                .filter(|name| !name.is_empty())
                .or_else(|| state.variable("LOGNAME").filter(|name| !name.is_empty()));
            (current_user.as_deref() == Some(user))
                .then(|| state.variable("HOME"))
                .flatten()
                .filter(|home| !home.is_empty())
        }
        _ => return Some(value.to_string()),
    }?;
    Some(format!("{home}{suffix}"))
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// `cd` is always allowed at the command level; workspace path restriction handles the target.
pub(super) fn is_cd_command(command: &str) -> bool {
    shell_parse::is_single_cd(command)
}

/// Returns true if a Bash program redirects output to a real file.
pub(super) fn has_output_redirection(command: &str) -> bool {
    shell_parse::has_output_redirection(command)
}
