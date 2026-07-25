use std::io::Cursor;

use brush_parser::ast::{
    self, CommandPrefixOrSuffixItem, CompoundCommand, IoFileRedirectKind, IoFileRedirectTarget,
    IoRedirect, SourceLocation,
};
use brush_parser::word::{self, WordPiece, WordPieceWithSource};
use brush_parser::{tokenize_str, Parser, ParserOptions, SourceSpan, Token};
use smelt_buffer::text::{byte_of_char, slice};

const SEPARATOR_OPERATORS: &[&str] = &["&&", "||", ";", "|", "&", "\n"];

pub(super) fn parse(command: &str) -> Option<ast::Program> {
    if has_unrepresentable_io_number(command) {
        return None;
    }
    let mut parser = Parser::new(Cursor::new(command.as_bytes()), &ParserOptions::default());
    parser.parse_program().ok()
}

fn has_unrepresentable_io_number(command: &str) -> bool {
    let Ok(tokens) = tokenize_str(command) else {
        return false;
    };
    tokens.windows(2).any(|tokens| {
        let [Token::Word(number, number_span), Token::Operator(operator, operator_span)] = tokens
        else {
            return false;
        };
        number_span.end.index == operator_span.start.index
            && operator.starts_with(['<', '>'])
            && number.chars().all(|ch| ch.is_ascii_digit())
            && number.parse::<ast::IoFd>().is_err()
    })
}

pub(super) fn parse_word(word: &str) -> Option<Vec<WordPieceWithSource>> {
    word::parse(word, &ParserOptions::default()).ok()
}

pub(super) fn parse_heredoc_word(word: &str) -> Option<Vec<WordPieceWithSource>> {
    word::parse_heredoc(word, &ParserOptions::default()).ok()
}

pub(super) enum BraceExpansion {
    Absent,
    Expanded(Vec<String>),
    Unresolved,
}

pub(super) fn brace_expansion(word: &str, limit: usize) -> BraceExpansion {
    let parts = match word::parse_brace_expansions(word, &ParserOptions::default()) {
        Ok(Some(parts)) => parts,
        Ok(None) => return BraceExpansion::Absent,
        Err(_) => return BraceExpansion::Unresolved,
    };
    if !parts
        .iter()
        .any(|part| matches!(part, word::BraceExpressionOrText::Expr(_)))
    {
        return BraceExpansion::Absent;
    }
    match expand_brace_parts(&parts, limit) {
        Some(expanded) => BraceExpansion::Expanded(expanded),
        None => BraceExpansion::Unresolved,
    }
}

fn expand_brace_parts(parts: &[word::BraceExpressionOrText], limit: usize) -> Option<Vec<String>> {
    let mut expanded = vec![String::new()];
    for part in parts {
        match part {
            word::BraceExpressionOrText::Text(text) => {
                for value in &mut expanded {
                    value.push_str(text);
                }
            }
            word::BraceExpressionOrText::Expr(expression) => {
                let mut alternatives = Vec::new();
                for member in expression {
                    let word::BraceExpressionMember::Child(child) = member else {
                        // Sequence expansion carries formatting semantics such as zero padding.
                        // Leave it unresolved rather than approximate a different path.
                        return None;
                    };
                    alternatives.extend(expand_brace_parts(child, limit)?);
                    if alternatives.len() > limit {
                        return None;
                    }
                }

                let result_len = expanded.len().checked_mul(alternatives.len())?;
                if result_len > limit {
                    return None;
                }
                expanded = expanded
                    .into_iter()
                    .flat_map(|prefix| {
                        alternatives.iter().map(move |alternative| {
                            let mut value = prefix.clone();
                            value.push_str(alternative);
                            value
                        })
                    })
                    .collect();
            }
        }
    }
    Some(expanded)
}

pub(super) fn embedded_commands(pieces: &[WordPieceWithSource]) -> Vec<&str> {
    fn collect<'a>(pieces: &'a [WordPieceWithSource], out: &mut Vec<&'a str>) {
        for piece in pieces {
            match &piece.piece {
                WordPiece::CommandSubstitution(command)
                | WordPiece::BackquotedCommandSubstitution(command) => out.push(command),
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => collect(inner, out),
                _ => {}
            }
        }
    }

    let mut out = Vec::new();
    collect(pieces, &mut out);
    out
}

pub(super) fn split_with_ops(command: &str) -> Vec<(String, Option<String>)> {
    let Some(program) = parse(command) else {
        return tokenized_split(command);
    };
    split_program_with_ops(&program, command)
}

pub(super) fn split(command: &str) -> Vec<String> {
    let Some(program) = parse(command) else {
        return tokenized_split(command)
            .into_iter()
            .map(|(command, _)| command)
            .collect();
    };

    let mut out = split_program_with_ops(&program, command)
        .into_iter()
        .map(|(command, _)| command)
        .collect::<Vec<_>>();
    collect_nested_program(&program, command, &mut out);
    out
}

fn split_program_with_ops(program: &ast::Program, source: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for complete_command in &program.complete_commands {
        collect_list_segments(complete_command, source, &mut out);
    }
    out
}

fn collect_list_segments(
    list: &ast::CompoundList,
    source: &str,
    out: &mut Vec<(String, Option<String>)>,
) {
    for item in &list.0 {
        collect_and_or_segments(&item.0, source, out);
        let separator = match item.1 {
            ast::SeparatorOperator::Async => "&",
            ast::SeparatorOperator::Sequence => ";",
        };
        if let Some((_, following)) = out.last_mut() {
            *following = Some(separator.to_string());
        }
    }
    if let Some((_, following)) = out.last_mut() {
        if following.as_deref() == Some(";") {
            *following = None;
        }
    }
}

fn collect_and_or_segments(
    list: &ast::AndOrList,
    source: &str,
    out: &mut Vec<(String, Option<String>)>,
) {
    collect_pipeline_segments(&list.first, source, out);
    for item in &list.additional {
        let (operator, pipeline) = match item {
            ast::AndOr::And(pipeline) => ("&&", pipeline),
            ast::AndOr::Or(pipeline) => ("||", pipeline),
        };
        if let Some((_, following)) = out.last_mut() {
            *following = Some(operator.to_string());
        }
        collect_pipeline_segments(pipeline, source, out);
    }
}

fn collect_pipeline_segments(
    pipeline: &ast::Pipeline,
    source: &str,
    out: &mut Vec<(String, Option<String>)>,
) {
    for (index, command) in pipeline.seq.iter().enumerate() {
        if index > 0 {
            if let Some((_, following)) = out.last_mut() {
                *following = Some("|".to_string());
            }
        }
        let text = command_text(command, source);
        if !text.is_empty() {
            out.push((text, None));
        }
    }
}

fn command_text(command: &ast::Command, source: &str) -> String {
    if matches!(
        command,
        ast::Command::Simple(simple)
            if simple.word_or_name.is_none()
                || simple.prefix.as_ref().and_then(|prefix| prefix.0.first())
                    .is_some_and(|item| matches!(item, CommandPrefixOrSuffixItem::IoRedirect(_)))
    ) {
        return command.to_string().trim().to_string();
    }
    command_span(command)
        .map(|span| source_span_text(source, &span).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| command.to_string().trim().to_string())
}

fn command_span(command: &ast::Command) -> Option<SourceSpan> {
    match command {
        ast::Command::Simple(simple) => simple_command_span(simple),
        ast::Command::Compound(compound, redirects) => combine_spans(
            compound.location(),
            redirects.as_ref().and_then(redirect_list_span),
        ),
        ast::Command::Function(function) => function.location(),
        ast::Command::ExtendedTest(test, redirects) => combine_spans(
            test.location(),
            redirects.as_ref().and_then(redirect_list_span),
        ),
    }
}

fn simple_command_span(command: &ast::SimpleCommand) -> Option<SourceSpan> {
    let mut span = command
        .word_or_name
        .as_ref()
        .and_then(SourceLocation::location);
    for item in command
        .prefix
        .iter()
        .flat_map(|prefix| &prefix.0)
        .chain(command.suffix.iter().flat_map(|suffix| &suffix.0))
    {
        let item_span = match item {
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => redirect_span(redirect),
            CommandPrefixOrSuffixItem::Word(word)
            | CommandPrefixOrSuffixItem::AssignmentWord(_, word) => word.location(),
            CommandPrefixOrSuffixItem::ProcessSubstitution(_, command) => command.location(),
        };
        span = combine_spans(span, item_span);
    }
    span
}

fn redirect_list_span(redirects: &ast::RedirectList) -> Option<SourceSpan> {
    redirects
        .0
        .iter()
        .filter_map(redirect_span)
        .fold(None, |span, redirect| combine_spans(span, Some(redirect)))
}

fn redirect_span(redirect: &IoRedirect) -> Option<SourceSpan> {
    match redirect {
        IoRedirect::File(_, _, target) => match target {
            IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                word.location()
            }
            IoFileRedirectTarget::Fd(_) => None,
            IoFileRedirectTarget::ProcessSubstitution(_, command) => command.location(),
        },
        IoRedirect::HereDocument(_, heredoc) => {
            combine_spans(heredoc.here_end.location(), heredoc.doc.location())
        }
        IoRedirect::HereString(_, word) | IoRedirect::OutputAndError(word, _) => word.location(),
    }
}

fn combine_spans(first: Option<SourceSpan>, second: Option<SourceSpan>) -> Option<SourceSpan> {
    match (first, second) {
        (Some(first), Some(second)) => Some(SourceSpan {
            start: if first.start.index <= second.start.index {
                first.start
            } else {
                second.start
            },
            end: if first.end.index >= second.end.index {
                first.end
            } else {
                second.end
            },
        }),
        (span @ Some(_), None) | (None, span @ Some(_)) => span,
        (None, None) => None,
    }
}

fn source_span_text<'a>(source: &'a str, span: &SourceSpan) -> &'a str {
    let start = byte_of_char(source, span.start.index);
    let end = byte_of_char(source, span.end.index);
    slice(source, start..end)
}

fn collect_nested_program(program: &ast::Program, source: &str, out: &mut Vec<String>) {
    for complete_command in &program.complete_commands {
        collect_nested_list(complete_command, source, out);
    }
}

fn collect_nested_list(list: &ast::CompoundList, source: &str, out: &mut Vec<String>) {
    for item in &list.0 {
        collect_nested_and_or(&item.0, source, out);
    }
}

fn collect_nested_and_or(list: &ast::AndOrList, source: &str, out: &mut Vec<String>) {
    collect_nested_pipeline(&list.first, source, out);
    for item in &list.additional {
        match item {
            ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => {
                collect_nested_pipeline(pipeline, source, out);
            }
        }
    }
}

fn collect_nested_pipeline(pipeline: &ast::Pipeline, source: &str, out: &mut Vec<String>) {
    for command in &pipeline.seq {
        collect_nested_command(command, source, out);
    }
}

fn collect_nested_command(command: &ast::Command, source: &str, out: &mut Vec<String>) {
    match command {
        ast::Command::Simple(simple) => collect_simple_nested(simple, source, out),
        ast::Command::Compound(compound, redirects) => {
            collect_compound_body(compound, source, out);
            if let Some(redirects) = redirects {
                collect_redirects_nested(&redirects.0, source, out);
            }
        }
        ast::Command::Function(function) => {
            collect_compound_body(&function.body.0, source, out);
            if let Some(redirects) = &function.body.1 {
                collect_redirects_nested(&redirects.0, source, out);
            }
        }
        ast::Command::ExtendedTest(test, redirects) => {
            collect_extended_test_words(&test.expr, out);
            if let Some(redirects) = redirects {
                collect_redirects_nested(&redirects.0, source, out);
            }
        }
    }
}

fn collect_simple_nested(simple: &ast::SimpleCommand, source: &str, out: &mut Vec<String>) {
    if let Some(prefix) = &simple.prefix {
        collect_items_nested(&prefix.0, source, out);
    }
    if let Some(word) = &simple.word_or_name {
        collect_word_nested(&word.value, out);
    }
    if let Some(suffix) = &simple.suffix {
        collect_items_nested(&suffix.0, source, out);
    }
}

fn collect_items_nested(items: &[CommandPrefixOrSuffixItem], source: &str, out: &mut Vec<String>) {
    for item in items {
        match item {
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                collect_redirects_nested(std::slice::from_ref(redirect), source, out);
            }
            CommandPrefixOrSuffixItem::Word(word)
            | CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                collect_word_nested(&word.value, out);
            }
            CommandPrefixOrSuffixItem::ProcessSubstitution(_, command) => {
                collect_list_as_nested(&command.list, source, out);
            }
        }
    }
}

fn collect_compound_body(compound: &CompoundCommand, source: &str, out: &mut Vec<String>) {
    match compound {
        CompoundCommand::Arithmetic(_) => {}
        CompoundCommand::ArithmeticForClause(command) => {
            collect_list_as_nested(&command.body.list, source, out);
        }
        CompoundCommand::BraceGroup(command) => {
            collect_list_as_nested(&command.list, source, out);
        }
        CompoundCommand::Subshell(command) => {
            collect_list_as_nested(&command.list, source, out);
        }
        CompoundCommand::ForClause(command) => {
            if let Some(values) = &command.values {
                for word in values {
                    collect_word_nested(&word.value, out);
                }
            }
            collect_list_as_nested(&command.body.list, source, out);
        }
        CompoundCommand::CaseClause(command) => {
            collect_word_nested(&command.value.value, out);
            for case in &command.cases {
                for pattern in &case.patterns {
                    collect_word_nested(&pattern.value, out);
                }
                if let Some(list) = &case.cmd {
                    collect_list_as_nested(list, source, out);
                }
            }
        }
        CompoundCommand::IfClause(command) => {
            collect_list_as_nested(&command.condition, source, out);
            collect_list_as_nested(&command.then, source, out);
            if let Some(elses) = &command.elses {
                for else_clause in elses {
                    if let Some(condition) = &else_clause.condition {
                        collect_list_as_nested(condition, source, out);
                    }
                    collect_list_as_nested(&else_clause.body, source, out);
                }
            }
        }
        CompoundCommand::WhileClause(command) | CompoundCommand::UntilClause(command) => {
            collect_list_as_nested(&command.0, source, out);
            collect_list_as_nested(&command.1.list, source, out);
        }
        CompoundCommand::Coprocess(command) => {
            collect_nested_command(&command.body, source, out);
        }
    }
}

fn collect_list_as_nested(list: &ast::CompoundList, source: &str, out: &mut Vec<String>) {
    let mut segments = Vec::new();
    collect_list_segments(list, source, &mut segments);
    out.extend(segments.into_iter().map(|(command, _)| command));
    collect_nested_list(list, source, out);
}

fn collect_redirects_nested(redirects: &[IoRedirect], source: &str, out: &mut Vec<String>) {
    for redirect in redirects {
        match redirect {
            IoRedirect::File(_, _, target) => match target {
                IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                    collect_word_nested(&word.value, out);
                }
                IoFileRedirectTarget::ProcessSubstitution(_, command) => {
                    collect_list_as_nested(&command.list, source, out);
                }
                IoFileRedirectTarget::Fd(_) => {}
            },
            IoRedirect::HereDocument(_, heredoc) if heredoc.requires_expansion => {
                let Some(pieces) = parse_heredoc_word(&heredoc.doc.value) else {
                    continue;
                };
                collect_piece_commands(&pieces, out);
            }
            IoRedirect::HereString(_, word) | IoRedirect::OutputAndError(word, _) => {
                collect_word_nested(&word.value, out);
            }
            IoRedirect::HereDocument(_, _) => {}
        }
    }
}

fn collect_extended_test_words(expr: &ast::ExtendedTestExpr, out: &mut Vec<String>) {
    match expr {
        ast::ExtendedTestExpr::And(left, right) | ast::ExtendedTestExpr::Or(left, right) => {
            collect_extended_test_words(left, out);
            collect_extended_test_words(right, out);
        }
        ast::ExtendedTestExpr::Not(expr) | ast::ExtendedTestExpr::Parenthesized(expr) => {
            collect_extended_test_words(expr, out);
        }
        ast::ExtendedTestExpr::UnaryTest(_, word) => {
            collect_word_nested(&word.value, out);
        }
        ast::ExtendedTestExpr::BinaryTest(_, left, right) => {
            collect_word_nested(&left.value, out);
            collect_word_nested(&right.value, out);
        }
    }
}

fn collect_word_nested(word: &str, out: &mut Vec<String>) {
    let Some(pieces) = parse_word(word) else {
        return;
    };
    collect_piece_commands(&pieces, out);
}

fn collect_piece_commands(pieces: &[WordPieceWithSource], out: &mut Vec<String>) {
    for command in embedded_commands(pieces) {
        out.extend(split(command));
    }
}

fn tokenized_split(command: &str) -> Vec<(String, Option<String>)> {
    let Ok(tokens) = tokenize_str(command) else {
        let trimmed = command.trim();
        return (!trimmed.is_empty())
            .then(|| (trimmed.to_string(), None))
            .into_iter()
            .collect();
    };

    let mut out = Vec::new();
    let mut start = 0;
    for token in tokens {
        let Token::Operator(operator, span) = token else {
            continue;
        };
        let separator = if SEPARATOR_OPERATORS.contains(&operator.as_str()) {
            operator
        } else if operator.starts_with(';') {
            ";".to_string()
        } else {
            continue;
        };
        let operator_start = byte_of_char(command, span.start.index);
        let text = slice(command, start..operator_start).trim();
        if !text.is_empty() {
            out.push((text.to_string(), Some(separator)));
        }
        start = byte_of_char(command, span.end.index);
    }
    let trailing = slice(command, start..command.len()).trim();
    if !trailing.is_empty() {
        out.push((trailing.to_string(), None));
    }
    out
}

pub(super) fn has_output_redirection(command: &str) -> bool {
    parse(command).map_or_else(
        || tokenized_has_output_redirection(command),
        |program| program_has_output_redirection(&program),
    )
}

fn program_has_output_redirection(program: &ast::Program) -> bool {
    program
        .complete_commands
        .iter()
        .any(list_has_output_redirection)
}

fn list_has_output_redirection(list: &ast::CompoundList) -> bool {
    list.0.iter().any(|item| {
        pipeline_has_output_redirection(&item.0.first)
            || item.0.additional.iter().any(|item| match item {
                ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => {
                    pipeline_has_output_redirection(pipeline)
                }
            })
    })
}

fn pipeline_has_output_redirection(pipeline: &ast::Pipeline) -> bool {
    pipeline.seq.iter().any(command_has_output_redirection)
}

fn command_has_output_redirection(command: &ast::Command) -> bool {
    match command {
        ast::Command::Simple(simple) => simple
            .prefix
            .iter()
            .flat_map(|prefix| &prefix.0)
            .chain(simple.suffix.iter().flat_map(|suffix| &suffix.0))
            .any(|item| match item {
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    redirect_has_output_redirection(redirect)
                }
                CommandPrefixOrSuffixItem::ProcessSubstitution(_, command) => {
                    list_has_output_redirection(&command.list)
                }
                CommandPrefixOrSuffixItem::Word(_)
                | CommandPrefixOrSuffixItem::AssignmentWord(_, _) => false,
            }),
        ast::Command::Compound(compound, redirects) => {
            compound_has_output_redirection(compound)
                || redirects
                    .iter()
                    .flat_map(|redirects| &redirects.0)
                    .any(redirect_has_output_redirection)
        }
        ast::Command::Function(function) => {
            compound_has_output_redirection(&function.body.0)
                || function
                    .body
                    .1
                    .iter()
                    .flat_map(|redirects| &redirects.0)
                    .any(redirect_has_output_redirection)
        }
        ast::Command::ExtendedTest(_, redirects) => redirects
            .iter()
            .flat_map(|redirects| &redirects.0)
            .any(redirect_has_output_redirection),
    }
}

fn compound_has_output_redirection(compound: &CompoundCommand) -> bool {
    match compound {
        CompoundCommand::Arithmetic(_) => false,
        CompoundCommand::ArithmeticForClause(command) => {
            list_has_output_redirection(&command.body.list)
        }
        CompoundCommand::BraceGroup(command) => list_has_output_redirection(&command.list),
        CompoundCommand::Subshell(command) => list_has_output_redirection(&command.list),
        CompoundCommand::ForClause(command) => list_has_output_redirection(&command.body.list),
        CompoundCommand::CaseClause(command) => command
            .cases
            .iter()
            .filter_map(|case| case.cmd.as_ref())
            .any(list_has_output_redirection),
        CompoundCommand::IfClause(command) => {
            list_has_output_redirection(&command.condition)
                || list_has_output_redirection(&command.then)
                || command.elses.iter().flatten().any(|else_clause| {
                    else_clause
                        .condition
                        .as_ref()
                        .is_some_and(list_has_output_redirection)
                        || list_has_output_redirection(&else_clause.body)
                })
        }
        CompoundCommand::WhileClause(command) | CompoundCommand::UntilClause(command) => {
            list_has_output_redirection(&command.0) || list_has_output_redirection(&command.1.list)
        }
        CompoundCommand::Coprocess(command) => command_has_output_redirection(&command.body),
    }
}

fn redirect_has_output_redirection(redirect: &IoRedirect) -> bool {
    match redirect {
        IoRedirect::File(_, kind, target) => match kind {
            IoFileRedirectKind::Write
            | IoFileRedirectKind::Append
            | IoFileRedirectKind::ReadAndWrite
            | IoFileRedirectKind::Clobber => redirect_target_is_file(target),
            IoFileRedirectKind::DuplicateOutput => match target {
                IoFileRedirectTarget::Duplicate(word) => {
                    !word.value.chars().all(|ch| ch.is_ascii_digit())
                        && word.value != "-"
                        && !is_shell_stream(&word.value)
                }
                _ => redirect_target_is_file(target),
            },
            IoFileRedirectKind::Read | IoFileRedirectKind::DuplicateInput => false,
        },
        IoRedirect::OutputAndError(word, _) => !is_shell_stream(&word.value),
        IoRedirect::HereDocument(_, _) | IoRedirect::HereString(_, _) => false,
    }
}

fn redirect_target_is_file(target: &IoFileRedirectTarget) -> bool {
    match target {
        IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
            !is_shell_stream(&word.value)
        }
        IoFileRedirectTarget::Fd(_) | IoFileRedirectTarget::ProcessSubstitution(_, _) => false,
    }
}

fn is_shell_stream(path: &str) -> bool {
    matches!(
        path.trim_matches(['\'', '"']),
        "/dev/null" | "/dev/stdin" | "/dev/stdout" | "/dev/stderr"
    )
}

fn tokenized_has_output_redirection(command: &str) -> bool {
    let Ok(tokens) = tokenize_str(command) else {
        return false;
    };
    let mut tokens = tokens.iter().peekable();
    while let Some(token) = tokens.next() {
        let Token::Operator(operator, _) = token else {
            continue;
        };
        if !matches!(operator.as_str(), ">" | ">>" | ">|" | "<>" | "&>" | "&>>") {
            continue;
        }
        let target = tokens
            .peek()
            .map(|token| token.to_str())
            .unwrap_or_default();
        if !is_shell_stream(target) {
            return true;
        }
    }
    false
}

pub(super) fn is_single_cd(command: &str) -> bool {
    let Some(program) = parse(command) else {
        return false;
    };
    let [complete] = program.complete_commands.as_slice() else {
        return false;
    };
    let [item] = complete.0.as_slice() else {
        return false;
    };
    if !item.0.additional.is_empty() || item.0.first.seq.len() != 1 {
        return false;
    }
    let ast::Command::Simple(simple) = &item.0.first.seq[0] else {
        return false;
    };
    simple.word_or_name.as_ref().is_some_and(|word| {
        parse_word(&word.value).is_some_and(|pieces| {
            matches!(pieces.as_slice(), [piece] if matches!(&piece.piece, WordPiece::Text(text) if text == "cd"))
        })
    })
}
