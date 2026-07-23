#![cfg(test)]

use super::approvals::*;
use super::bash::*;
use super::rules::*;
use super::workspace::*;
use super::*;
use protocol::AgentMode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn resolved_tool_path(path: &str, base_dir: &Path) -> PathBuf {
    resolve_tool_path(path, base_dir)
        .resolved()
        .unwrap()
        .to_path_buf()
}

fn resolved_filesystem_path(path: &Path) -> PathBuf {
    resolve_filesystem_path(path)
        .resolved()
        .unwrap()
        .to_path_buf()
}

fn dirs_approved(rt: &RuntimeApprovals, paths: &[&str]) -> bool {
    paths.iter().all(|path| {
        rt.requirement_satisfied(&PermissionRequirement::PathPrefix {
            dir: normalize_approval_path(Path::new(path)),
        })
    })
}

fn mode(name: &str) -> AgentMode {
    AgentMode::parse(name).unwrap()
}

fn normal() -> AgentMode {
    AgentMode::normal()
}

fn plan() -> AgentMode {
    mode("plan")
}

fn apply() -> AgentMode {
    mode("apply")
}

fn yolo() -> AgentMode {
    mode("yolo")
}

fn ruleset(allow: &[&str], ask: &[&str], deny: &[&str]) -> RuleSet {
    RuleSet {
        allow: compile_patterns(&allow.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        ask: compile_patterns(&ask.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        deny: compile_patterns(&deny.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
    }
}

fn empty_ruleset() -> RuleSet {
    RuleSet {
        allow: vec![],
        ask: vec![],
        deny: vec![],
    }
}

fn mode_perms(tools: HashMap<String, Decision>, buckets: &[(&str, RuleSet)]) -> ModePerms {
    let mut patterns = HashMap::new();
    for (name, rs) in buckets {
        patterns.insert((*name).to_string(), rs.clone());
    }
    ModePerms {
        tools,
        effects: EffectPerms::default(),
        patterns,
    }
}

/// Mirrors the `subpattern_parser = "shell"` wiring from `tools/bash.lua`.
fn bash_parser_map() -> HashMap<String, std::sync::Arc<crate::permissions::SubpatternParserFn>> {
    let mut m = HashMap::new();
    if let Some(p) = crate::permissions::builtin_subpattern_parser("shell") {
        m.insert("bash".to_string(), p);
    }
    m
}

fn test_tool_effects() -> HashMap<String, ToolEffectKind> {
    [
        ("read_file", ToolEffectKind::Read),
        ("glob", ToolEffectKind::Read),
        ("grep", ToolEffectKind::Read),
        ("edit_file", ToolEffectKind::Write),
        ("write_file", ToolEffectKind::Write),
        ("edit_notebook", ToolEffectKind::Write),
        ("enter_worktree", ToolEffectKind::Write),
        ("web_fetch", ToolEffectKind::Network),
        ("web_search", ToolEffectKind::Network),
        ("ask_user_question", ToolEffectKind::User),
        ("read_process_output", ToolEffectKind::Process),
        ("stop_process", ToolEffectKind::Process),
        ("smelt_reload", ToolEffectKind::Config),
    ]
    .into_iter()
    .map(|(name, effect)| (name.to_string(), effect))
    .collect()
}

fn permissions_from_mode(
    mode: ModePerms,
    restrict_to_workspace: bool,
    workspace: PathBuf,
) -> Permissions {
    let modes = ["normal", "plan", "apply", "yolo"]
        .into_iter()
        .map(|name| (name.to_string(), mode.clone()))
        .collect();
    let mode_behaviors = HashMap::from([(
        "yolo".to_string(),
        ModeBehavior {
            default_decision: Decision::Allow,
            allow_subcommands_by_default: true,
            ask_on_output_redirection: false,
        },
    )]);
    Permissions {
        modes,
        mode_behaviors,
        restrict_to_workspace,
        active_root: workspace.clone(),
        home: engine::paths::home_dir(),
        allowed_roots: if workspace.as_os_str().is_empty() {
            Vec::new()
        } else {
            vec![workspace]
        },
        paths_fn: None,
        tool_decisions: HashMap::new(),
        tool_effects: test_tool_effects(),
        subpattern_parsers: bash_parser_map(),
        approvals: std::sync::Arc::new(std::sync::RwLock::new(RuntimeApprovals::new())),
    }
}

fn perms_with_bash(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
    let mode = mode_perms(HashMap::new(), &[("bash", ruleset(allow, ask, deny))]);
    permissions_from_mode(mode, false, PathBuf::new())
}

/// Stub `paths_fn` mirroring production `paths_for_workspace` callbacks
/// for structured file/path tools.
fn stub_paths_fn() -> std::sync::Arc<crate::permissions::PathsFn> {
    std::sync::Arc::new(|name, args| match name {
        "read_file" | "write_file" | "edit_file" => {
            let p = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            if p.is_empty() {
                vec![]
            } else {
                vec![ToolPath::file(p)]
            }
        }
        "edit_notebook" => {
            let p = args
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if p.is_empty() {
                vec![]
            } else {
                vec![ToolPath::file(p)]
            }
        }
        "glob" | "grep" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if p.is_empty() {
                vec![]
            } else {
                vec![ToolPath::directory(p)]
            }
        }
        _ => vec![],
    })
}

#[track_caller]
fn assert_bash(
    allow: &[&str],
    ask: &[&str],
    deny: &[&str],
    mode: AgentMode,
    cmd: &str,
    expected: Decision,
) {
    let p = perms_with_bash(allow, ask, deny);
    assert_eq!(p.check_subcommand(mode, "bash", cmd), expected);
}

#[test]
fn yolo_allows_mcp_by_default() {
    let p = perms_with_bash(&[], &[], &[]);
    assert_eq!(
        p.check_subcommand(yolo(), "mcp", "filesystem_read_file"),
        Decision::Allow
    );
}

#[test]
fn normal_mode_asks_for_mcp_by_default() {
    let p = perms_with_bash(&[], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "mcp", "filesystem_read_file"),
        Decision::Ask
    );
}

// --- simple commands ---

#[test]
fn simple_allowed() {
    assert_bash(&["ls *"], &[], &[], normal(), "ls -la", Decision::Allow);
}

#[test]
fn simple_denied() {
    assert_bash(&[], &[], &["rm *"], normal(), "rm -rf /", Decision::Deny);
}

#[test]
fn simple_ask() {
    assert_bash(&[], &["rm *"], &[], normal(), "rm -rf /", Decision::Ask);
}

// --- deny rules with chained commands ---

#[test]
fn deny_rm_simple() {
    assert_bash(&[], &[], &["rm *"], normal(), "rm -rf /", Decision::Deny);
}

#[test]
fn deny_rm_after_ls() {
    assert_bash(
        &["ls *"],
        &[],
        &["rm *"],
        normal(),
        "ls && rm -rf /",
        Decision::Deny,
    );
}

#[test]
fn deny_rm_before_ls() {
    assert_bash(
        &["ls *"],
        &[],
        &["rm *"],
        normal(),
        "rm -rf / && ls",
        Decision::Deny,
    );
}

// --- ask rules with chained commands ---

#[test]
fn ask_rm_simple() {
    assert_bash(&[], &["rm *"], &[], normal(), "rm -rf /", Decision::Ask);
}

#[test]
fn ask_rm_after_ls() {
    assert_bash(
        &["ls *"],
        &["rm *"],
        &[],
        normal(),
        "ls && rm -rf /",
        Decision::Ask,
    );
}

#[test]
fn ask_rm_before_ls() {
    assert_bash(
        &["ls *"],
        &["rm *"],
        &[],
        normal(),
        "rm -rf / && ls",
        Decision::Ask,
    );
}

// --- allow rule should not match chained commands ---

#[test]
fn allow_ls_does_not_allow_chained_rm() {
    assert_bash(
        &["ls *"],
        &[],
        &[],
        normal(),
        "ls && rm README.md",
        Decision::Ask,
    );
}

// --- both sub-commands allowed ---

#[test]
fn chained_both_allowed() {
    assert_bash(
        &["ls *", "rm *"],
        &[],
        &[],
        normal(),
        "ls && rm README.md",
        Decision::Allow,
    );
}

// --- pipes ---

#[test]
fn pipe_both_allowed() {
    assert_bash(
        &["cat *", "grep *"],
        &[],
        &[],
        normal(),
        "cat file.txt | grep foo",
        Decision::Allow,
    );
}

#[test]
fn pipe_second_not_allowed() {
    assert_bash(
        &["cat *"],
        &[],
        &[],
        normal(),
        "cat file.txt | rm foo",
        Decision::Ask,
    );
}

// --- semicolon ---

#[test]
fn semicolon_second_denied() {
    assert_bash(
        &["echo *"],
        &[],
        &["rm *"],
        normal(),
        "echo hi; rm -rf /",
        Decision::Deny,
    );
}

// --- or chain ---

#[test]
fn or_chain_both_allowed() {
    assert_bash(
        &["make *"],
        &[],
        &[],
        normal(),
        "make || make install",
        Decision::Allow,
    );
}

// --- deny wins over allow ---

#[test]
fn deny_wins_over_allow() {
    assert_bash(
        &["rm *"],
        &[],
        &["rm *"],
        normal(),
        "rm foo",
        Decision::Deny,
    );
}

// --- split helper ---

#[test]
fn split_shell_commands_basic() {
    assert_eq!(split_shell_commands("ls"), vec!["ls"]);
    assert_eq!(split_shell_commands("ls && rm foo"), vec!["ls", "rm foo"]);
    assert_eq!(
        split_shell_commands("a | b || c; d && e"),
        vec!["a", "b", "c", "d", "e"]
    );
}

#[test]
fn split_shell_commands_utf8_multibyte() {
    // Multi-byte characters should not cause panics on byte-index slicing.
    assert_eq!(split_shell_commands("✿"), vec!["✿"]);
    assert_eq!(split_shell_commands("echo ✿"), vec!["echo ✿"]);
    assert_eq!(
        split_shell_commands("echo ✿ && rm foo"),
        vec!["echo ✿", "rm foo"]
    );
    assert_eq!(
        split_shell_commands("echo '✿ && world'"),
        vec!["echo '✿ && world'"]
    );
}

#[test]
fn split_shell_commands_utf8_with_backslash() {
    // Backslash before a multi-byte char should not land mid-char.
    assert_eq!(split_shell_commands(r"\✿"), vec![r"\✿"]);
    assert_eq!(split_shell_commands(r"echo \✿"), vec![r"echo \✿"]);
}

#[test]
fn split_shell_commands_utf8_in_subshell() {
    // Subshell extraction with multi-byte chars - inner commands are appended.
    assert_eq!(
        split_shell_commands("echo $(echo ✿)"),
        vec!["echo $(echo ✿)", "echo ✿"]
    );
    assert_eq!(
        split_shell_commands("echo ✿ && (echo ✿)"),
        vec!["echo ✿", "(echo ✿)", "echo ✿"]
    );
}

#[test]
fn split_shell_commands_utf8_with_heredoc() {
    let cmd = "cat << 'EOF'\n✿\nEOF";
    assert_eq!(split_shell_commands(cmd), vec![cmd]);
}

#[test]
fn split_shell_commands_input_fd_duplication_is_not_background() {
    assert_eq!(
        split_shell_commands("cat <&0 | head"),
        vec!["cat <&0", "head"]
    );
    assert_eq!(
        split_shell_commands_with_ops("cat <&0 | head"),
        vec![
            ("cat <&0".to_string(), Some("|".to_string())),
            ("head".to_string(), None),
        ]
    );
    assert_eq!(
        split_shell_commands_with_ops("exec 3<&0 && cat <&3"),
        vec![
            ("exec 3<&0".to_string(), Some("&&".to_string())),
            ("cat <&3".to_string(), None),
        ]
    );
}

#[test]
fn split_shell_commands_ampersand_input_redirect_is_background() {
    assert_eq!(
        split_shell_commands_with_ops("cd /tmp &< sessions.jsonl head -1"),
        vec![
            ("cd /tmp".to_string(), Some("&".to_string())),
            ("< sessions.jsonl head -1".to_string(), None),
        ]
    );
}

#[test]
fn has_output_redirection_utf8() {
    // Multi-byte chars near redirection operators.
    assert!(!has_output_redirection("echo ✿"));
    assert!(has_output_redirection("echo ✿ > out.txt"));
}

#[test]
fn split_shell_commands_with_ops_utf8() {
    assert_eq!(
        split_shell_commands_with_ops("echo ✿ && rm foo"),
        vec![
            ("echo ✿".to_string(), Some("&&".to_string())),
            ("rm foo".to_string(), None),
        ]
    );
}

// --- edge cases ---

// Empty / whitespace-only commands
#[test]
fn empty_command() {
    assert_bash(&["ls *"], &[], &[], normal(), "", Decision::Ask);
}

#[test]
fn whitespace_only_command() {
    assert_bash(&["ls *"], &[], &[], normal(), "   ", Decision::Ask);
}

// --- quote-aware splitting (shlex) ---

// Operators inside quotes are NOT treated as operators
#[test]
fn operator_in_quoted_argument() {
    let p = perms_with_bash(&["grep *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"grep "&&" file.txt"#),
        Decision::Allow
    );
}

#[test]
fn semicolon_in_echo() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"echo "hello; world""#),
        Decision::Allow
    );
}

#[test]
fn pipe_in_quoted_filename() {
    let p = perms_with_bash(&["cat *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"cat "file|name""#),
        Decision::Allow
    );
}

// --- single & (background operator) now handled ---

#[test]
fn single_ampersand_background() {
    let p = perms_with_bash(&["sleep *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "sleep 5 & rm foo"),
        Decision::Deny
    );
}

// --- subshell / substitution ---

#[test]
fn split_shell_commands_nested_subshells_once_per_level() {
    let mut command = "rm foo".to_string();
    for _ in 0..12 {
        command = format!("( {command}; )");
    }
    let commands = split_shell_commands(&command);
    assert_eq!(commands.len(), 13);
    assert_eq!(commands[0], command);
    assert_eq!(commands.last().map(String::as_str), Some("rm foo"));
}

#[test]
fn command_substitution() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "echo $(rm -rf /)"),
        Decision::Deny
    );
}

#[test]
fn backtick_substitution() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "echo `rm -rf /`"),
        Decision::Deny
    );
}

// --- newline separator ---

#[test]
fn newline_separator() {
    let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "ls\nrm -rf /"),
        Decision::Deny
    );
}

// --- trailing / leading operators ---

#[test]
fn trailing_operator() {
    assert_bash(&["ls *"], &[], &[], normal(), "ls &&", Decision::Allow);
}

#[test]
fn split_trailing_operator() {
    assert_eq!(split_shell_commands("ls &&"), vec!["ls"]);
}

#[test]
fn leading_operator() {
    let p = perms_with_bash(&["rm *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "&& rm foo"),
        Decision::Ask
    );
}

#[test]
fn split_leading_operator() {
    assert_eq!(split_shell_commands("&& rm foo"), vec!["rm foo"]);
}

// --- triple &&& ---

#[test]
fn triple_ampersand() {
    assert_eq!(split_shell_commands("ls &&&rm foo"), vec!["ls", "rm foo"]);
}

#[test]
fn triple_ampersand_spaced() {
    assert_eq!(split_shell_commands("ls &&& rm foo"), vec!["ls", "rm foo"]);
}

// --- bare commands ---

#[test]
fn bare_command_matches_star_pattern() {
    assert_bash(&["ls *"], &[], &[], normal(), "ls", Decision::Allow);
}

#[test]
fn trailing_space_no_false_positive() {
    assert_bash(&["ls *"], &[], &[], normal(), "lsof", Decision::Ask);
}

// --- unclosed quotes ---

#[test]
fn unclosed_quote() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"echo "hello && rm foo"#),
        Decision::Allow
    );
}

// --- escaped operators outside quotes ---

#[test]
fn escaped_ampersand_not_split() {
    // \&\& is two literal & chars in bash, not an operator
    assert_eq!(
        split_shell_commands(r"ls \&\& rm foo"),
        vec![r"ls \&\& rm foo"]
    );
}

#[test]
fn escaped_semicolon_not_split() {
    assert_eq!(
        split_shell_commands(r"echo hello\; world"),
        vec![r"echo hello\; world"]
    );
}

#[test]
fn escaped_pipe_not_split() {
    assert_eq!(
        split_shell_commands(r"echo hello\|world"),
        vec![r"echo hello\|world"]
    );
}

// --- mixed quote types ---

#[test]
fn single_quotes_inside_double() {
    // echo "it's fine" && rm foo → two commands
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"echo "it's fine" && rm foo"#),
        Decision::Deny
    );
}

#[test]
fn double_quotes_inside_single() {
    // echo '"hello"' && rm foo → two commands
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"echo '"hello"' && rm foo"#),
        Decision::Deny
    );
}

// --- escaped quote inside double quotes ---

#[test]
fn escaped_quote_inside_double_quotes() {
    // echo "he said \"hi\" && rm" is all one quoted string - single command
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"echo "he said \"hi\" && rm""#),
        Decision::Allow
    );
}

// --- consecutive operators ---

#[test]
fn double_semicolons() {
    // ls ;; rm → empty command between ;; is dropped, both ls and rm checked
    assert_eq!(split_shell_commands("ls ;; rm"), vec!["ls", "rm"]);
}

#[test]
fn double_semicolons_deny() {
    assert_bash(
        &["ls *"],
        &[],
        &["rm *"],
        normal(),
        "ls ;; rm foo",
        Decision::Deny,
    );
}

// --- operator-only input ---

#[test]
fn only_operators() {
    // No actual commands, just operators
    assert_eq!(split_shell_commands("&& || ;"), Vec::<String>::new());
}

// --- whitespace around operators ---

#[test]
fn extra_whitespace_around_operators() {
    assert_eq!(
        split_shell_commands("  ls   &&   rm foo  "),
        vec!["ls", "rm foo"]
    );
}

// --- single-command leading whitespace ---

#[test]
fn leading_whitespace_single_command() {
    let p = perms_with_bash(&["ls *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "  ls -la"),
        Decision::Allow
    );
}

#[test]
fn leading_whitespace_chained_command() {
    let p = perms_with_bash(&["ls *", "echo *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "  ls -la && echo hi"),
        Decision::Allow
    );
}

// --- subshells / parentheses ---

#[test]
fn subshell_not_parsed() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "echo hi && (rm -rf /)"),
        Decision::Deny
    );
}

#[test]
fn subshell_hides_denied_command() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // extract_embedded_commands scans through quotes; $() is found and rm is extracted.
    assert_eq!(
        p.check_subcommand(normal(), "bash", r#"echo "$(rm -rf /)""#),
        Decision::Deny
    );
}

// --- approval_pattern with background operator ---

#[test]
fn split_with_ops_background() {
    let result = split_shell_commands_with_ops("sleep 5 & echo done");
    assert_eq!(
        result,
        vec![
            ("sleep 5".to_string(), Some("&".to_string())),
            ("echo done".to_string(), None),
        ]
    );
}

#[test]
fn split_with_ops_preserves_operators() {
    let result = split_shell_commands_with_ops("ls && rm foo | grep err; echo done");
    assert_eq!(
        result,
        vec![
            ("ls".to_string(), Some("&&".to_string())),
            ("rm foo".to_string(), Some("|".to_string())),
            ("grep err".to_string(), Some(";".to_string())),
            ("echo done".to_string(), None),
        ]
    );
}

// --- backslash at end of string ---

#[test]
fn trailing_backslash() {
    // Trailing backslash with nothing after - should not panic
    assert_eq!(split_shell_commands("ls \\"), vec!["ls \\"]);
}

// --- here-string / redirection ---

#[test]
fn redirection_not_split() {
    // << is not a shell operator we handle, so it stays as one command
    assert_eq!(split_shell_commands("cat << EOF"), vec!["cat << EOF"]);
}

// --- heredoc content not treated as commands ---

#[test]
fn heredoc_content_not_split() {
    let cmd = "cat << 'EOF'\nhello world\nsome content\nEOF";
    assert_eq!(
        split_shell_commands(cmd),
        vec!["cat << 'EOF'\nhello world\nsome content\nEOF"]
    );
}

#[test]
fn heredoc_with_chained_command_not_split() {
    let cmd = "cd /tmp && uv run python3 << 'EOF'\nfrom main import load\ndata = load(num_features=25)\nEOF";
    let cmds = split_shell_commands(cmd);
    assert_eq!(cmds.len(), 2, "Expected [cd, uv], got: {cmds:?}");
    assert!(cmds[0].starts_with("cd "), "first: {:?}", cmds[0]);
    assert!(cmds[1].starts_with("uv "), "second: {:?}", cmds[1]);
}

#[test]
fn heredoc_with_pipe() {
    let cmd = "cat << 'EOF' | grep foo\nhello\nworld\nEOF";
    // The heredoc body should not produce extra commands
    let cmds = split_shell_commands(cmd);
    assert!(!cmds.iter().any(|c| c == "hello" || c == "world"));
}

#[test]
fn heredoc_permission_check() {
    let p = perms_with_bash(&["cat *", "grep *"], &[], &["rm *"]);
    let cmd = "cat << 'EOF' | grep foo\nrm -rf /\nEOF";
    // "rm -rf /" is heredoc content, not a command - should not be denied
    assert_eq!(p.check_subcommand(normal(), "bash", cmd), Decision::Allow);
}

// --- 2>&1 not split on & ---

#[test]
fn redirect_stderr_not_split() {
    assert_eq!(
        split_shell_commands("cargo build 2>&1"),
        vec!["cargo build 2>&1"]
    );
}

#[test]
fn redirect_stderr_permission() {
    assert_bash(
        &["cargo *"],
        &[],
        &[],
        normal(),
        "cargo build 2>&1",
        Decision::Allow,
    );
}

#[test]
fn redirect_ampersand_greater() {
    // &> /dev/null
    assert_eq!(
        split_shell_commands("cargo build &> /dev/null"),
        vec!["cargo build &> /dev/null"]
    );
}

// --- newline as separator ---

#[test]
fn newline_treated_as_separator() {
    assert_eq!(split_shell_commands("ls\nrm -rf /"), vec!["ls", "rm -rf /"]);
}

// ── workspace restriction ────────────────────────────────────────

fn perms_with_workspace(workspace: &str) -> Permissions {
    let mut tools = HashMap::new();
    tools.insert("read_file".to_string(), Decision::Allow);
    tools.insert("write_file".to_string(), Decision::Allow);
    tools.insert("edit_file".to_string(), Decision::Allow);
    tools.insert("glob".to_string(), Decision::Allow);
    tools.insert("grep".to_string(), Decision::Allow);
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(
        tools,
        &[(
            "bash",
            RuleSet {
                allow: vec![glob::Pattern::new("*").unwrap()],
                ask: vec![],
                deny: vec![],
            },
        )],
    );
    let mut p = permissions_from_mode(mode, true, PathBuf::from(workspace));
    p.set_paths_fn(stub_paths_fn());
    p
}

fn args_with(key: &str, val: &str) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(key.to_string(), Value::String(val.to_string()));
    m
}

fn canonical_abs(path: &str) -> PathBuf {
    resolved_filesystem_path(Path::new(path))
}

fn decide(
    permissions: &Permissions,
    mode: AgentMode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    permissions
        .evaluate_tool(mode, ToolOrigin::Lua, tool_name, args)
        .decision
}

// --- bash shell-string path extraction ---
// Only `extract_paths_from_command` lives in Rust; tool→path mapping is in Lua callbacks.

#[test]
fn shell_extracts_explicit_paths() {
    assert_eq!(
        extract_paths_from_command("rm -rf /tmp/foo"),
        vec!["/tmp/foo"]
    );
    assert_eq!(
        extract_paths_from_command("ls relative/dir"),
        vec!["relative/dir"]
    );
    assert_eq!(
        extract_paths_from_command("cat ~/secret.txt"),
        vec!["~/secret.txt"]
    );
}

#[test]
fn shell_strips_quotes_around_paths() {
    assert_eq!(
        extract_paths_from_command("rm '/etc/passwd'"),
        vec!["/etc/passwd"]
    );
}

// --- is_in_workspace ---

#[test]
fn relative_path_in_workspace() {
    assert!(is_in_workspace(
        "src/main.rs",
        Path::new("/home/user/project")
    ));
}

#[test]
fn absolute_path_in_workspace() {
    assert!(is_in_workspace(
        "/home/user/project/src/main.rs",
        Path::new("/home/user/project")
    ));
}

#[test]
fn absolute_path_outside_workspace() {
    assert!(!is_in_workspace(
        "/etc/passwd",
        Path::new("/home/user/project")
    ));
}

#[test]
fn dotdot_escape_outside_workspace() {
    assert!(!is_in_workspace(
        "/home/user/project/../../etc/passwd",
        Path::new("/home/user/project")
    ));
}

#[test]
fn workspace_root_itself_is_in_workspace() {
    assert!(is_in_workspace(
        "/home/user/project",
        Path::new("/home/user/project")
    ));
}

// --- decide with workspace restriction ---

#[test]
fn effects_for_file_tool_records_write_access_and_base() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "src/main.rs");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "write_file", &args);
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        ToolEffect::Fs(path) => {
            assert_eq!(path.raw_path, "src/main.rs");
            assert_eq!(path.base_dir, PathBuf::from("/home/user/project"));
            assert_eq!(
                path.resolution.path(),
                resolved_tool_path("src/main.rs", Path::new("/home/user/project"))
            );
            assert_eq!(path.access, PathAccess::Write);
        }
        other => panic!("expected filesystem effect, got {other:?}"),
    }
}

#[test]
fn evaluate_request_uses_typed_effects_for_workspace_downgrade() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/etc/passwd");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "read_file", &args);
    assert_eq!(
        p.evaluate_request(PermissionRequest {
            mode: normal(),
            tool_name: "read_file",
            args: &args,
            origin: ToolOrigin::Lua,
            effects,
        })
        .decision,
        Decision::Ask
    );
}

#[test]
fn mcp_request_uses_mcp_origin_without_boolean_branch() {
    let p = perms_with_bash(&[], &[], &[]);
    let args = HashMap::new();
    let effects = p.effects_for_tool(ToolOrigin::Mcp, "filesystem_read_file", &args);
    assert_eq!(
        p.evaluate_request(PermissionRequest {
            mode: yolo(),
            tool_name: "filesystem_read_file",
            args: &args,
            origin: ToolOrigin::Mcp,
            effects,
        })
        .decision,
        Decision::Allow
    );
}

#[test]
fn workspace_allows_multiple_project_roots() {
    let mut p = perms_with_workspace("/home/user/project/.worktrees/feature");
    p.set_allowed_roots(
        PathBuf::from("/home/user/project/.worktrees/feature"),
        vec![PathBuf::from("/home/user/project")],
    );

    let base_args = args_with("command", "cd /home/user/project && git merge feature");
    assert_eq!(decide(&p, normal(), "bash", &base_args), Decision::Allow);

    let outside_args = args_with("command", "cd /home/user/other && git status");
    assert_eq!(decide(&p, normal(), "bash", &outside_args), Decision::Ask);
}

#[test]
fn workspace_allows_file_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/home/user/project/src/main.rs");
    assert_eq!(decide(&p, normal(), "read_file", &args), Decision::Allow);
}

#[test]
fn workspace_downgrades_file_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/etc/passwd");
    assert_eq!(decide(&p, normal(), "read_file", &args), Decision::Ask);
}

#[test]
fn workspace_allows_relative_path() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "src/main.rs");
    assert_eq!(decide(&p, normal(), "write_file", &args), Decision::Allow);
}

#[test]
fn workspace_path_bash_does_not_treat_shell_status_as_a_path() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let command = concat!(
        "set -o pipefail; cargo xtask gen-lua-docs 2>&1 | tail -120; ",
        "status=${PIPESTATUS[0]}; if [ \"$status\" -ne 0 ]; then exit \"$status\"; fi; ",
        "git diff --exit-code -- docs/docs/reference/api runtime/lua/smelt/_meta ",
        "docs/zensical.toml runtime/skills/customize/SKILL.md >/tmp/smelt-doc-diff || ",
        "{ cat /tmp/smelt-doc-diff; exit 1; }",
    );
    let args = args_with("command", command);

    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    let path_requirements: Vec<_> = outcome
        .missing_requirements
        .iter()
        .filter_map(|requirement| match requirement {
            PermissionRequirement::PathPrefix { dir } => Some(dir.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        path_requirements,
        vec![resolved_filesystem_path(Path::new("/tmp"))]
    );
}

#[test]
fn workspace_path_bash_tracks_file_tests_inside_control_flow() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "secret").unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let args = args_with(
        "command",
        "candidate=../outside/secret; if [ -f \"$candidate\" ]; then echo found; fi",
    );

    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_bash_preserves_all_cwd_states_after_conditional_branch() {
    let temp = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir_in(temp.path().parent().unwrap()).unwrap();
    let workspace = temp.path().join("workspace");
    let sibling_name = sibling.path().file_name().unwrap().to_str().unwrap();
    let nested_outside = temp.path().join(sibling_name);
    std::fs::create_dir_all(workspace.join("child")).unwrap();
    std::fs::create_dir_all(&nested_outside).unwrap();
    std::fs::write(sibling.path().join("secret"), "secret").unwrap();
    std::fs::write(nested_outside.join("secret"), "secret").unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let args = args_with(
        "command",
        &format!("if maybe; then cd child; fi; cat ../../{sibling_name}/secret"),
    );

    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    for outside in [sibling.path(), nested_outside.as_path()] {
        assert!(outcome
            .missing_requirements
            .contains(&PermissionRequirement::PathPrefix {
                dir: resolved_filesystem_path(outside)
            }));
    }
}

#[test]
fn workspace_path_bash_ignores_pathlike_data_for_pathless_builtins() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with(
        "command",
        "status=1; if [[ \"$status\" -ne 0 ]]; then printf '%s\\n' /tmp/report; else echo /etc/passwd; fi",
    );

    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert!(outcome
        .missing_requirements
        .iter()
        .all(|requirement| !matches!(requirement, PermissionRequirement::PathPrefix { .. })));
}

#[test]
fn workspace_path_bash_tracks_redirection_on_assignment_only_command() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let args = args_with("command", "RESULT=ok > ../outside/result.txt");

    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert!(outcome
        .missing_requirements
        .contains(&PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }));
}

#[test]
fn workspace_path_bash_detects_bare_parent_directory() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "ls ..");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(temp.path())
        }]
    );
}

#[test]
fn workspace_path_bash_detects_relative_traversal_without_dot_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join("nested")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat nested/../../outside/secret.txt");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_bash_expands_bare_tilde() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cd ~ && pwd");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&engine::paths::home_dir())
        }]
    );
}

#[test]
fn workspace_path_bash_expands_current_directory_tilde() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat ~+/../outside/secret.txt");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_bash_expands_environment_variables() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat \"$HOME/.config/smelt/config\"");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&engine::paths::home_dir().join(".config/smelt"))
        }]
    );
}

#[test]
fn workspace_path_shell_single_quotes_disable_expansion() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    for command in [
        "cat '$HOME/secret'",
        r"cat \$HOME/secret",
        "cat \"~/secret\"",
    ] {
        let args = args_with("command", command);
        assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
    }
}

#[test]
fn workspace_path_shell_tracks_assignment_state() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "OUT=../outside; cat \"$OUT/secret\"");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_shell_uses_pre_command_values_for_prefix_assignments() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("inside")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "OUT=inside; OUT=../outside cat \"$OUT/secret\"");

    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[test]
fn workspace_path_shell_tracks_pwd_assignments() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "PWD=../outside; cat \"$PWD/secret\"");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_shell_fails_closed_after_conditional_cd() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join("child")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "false && cd child; cat ../outside/secret");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };

    assert!(paths.iter().any(|path| {
        path.resolution
            .resolved()
            .is_some_and(|path| path.starts_with(&outside))
    }));
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn workspace_path_shell_propagates_variables_into_embedded_commands() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with(
        "command",
        "export OUT=../outside; echo $(cat \"$OUT/secret\")",
    );
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_shell_unset_variables_expand_to_empty() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with(
        "command",
        "SMELT_PATH_TEST=../outside; unset SMELT_PATH_TEST; cat \"$SMELT_PATH_TEST/secret\"",
    );
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };

    assert_eq!(
        paths.last().unwrap().resolution.path(),
        Path::new("/secret")
    );
}

#[test]
fn workspace_path_shell_does_not_persist_pipeline_cd() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join("child")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "echo x | cd child; cat ../outside/secret");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_shell_keeps_cwd_after_failed_cd() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cd missing/child; cat ../outside/secret");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_shell_globbed_cd_updates_cwd_for_one_match() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let child = workspace.join("child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("note.txt"), "note\n").unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cd child*; cat note.txt");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };

    assert_eq!(
        paths.last().unwrap().resolution.path(),
        resolved_filesystem_path(&child.join("note.txt"))
    );
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[test]
fn workspace_path_shell_globbed_cd_keeps_cwd_for_multiple_matches() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("child-a")).unwrap();
    std::fs::create_dir_all(workspace.join("child-b")).unwrap();
    std::fs::write(workspace.join("note.txt"), "note\n").unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cd child-*; cat note.txt");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };

    assert_eq!(
        paths.last().unwrap().resolution.path(),
        resolved_filesystem_path(&workspace.join("note.txt"))
    );
}

#[test]
fn workspace_path_shell_globbed_env_chdir_updates_command_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let child = workspace.join("child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("note.txt"), "note\n").unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "env -C child* cat note.txt");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };

    assert_eq!(
        paths.last().unwrap().resolution.path(),
        resolved_filesystem_path(&child.join("note.txt"))
    );
}

#[test]
fn workspace_path_shell_tracks_oldpwd_for_cd_and_tilde() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join("child")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    for command in [
        "cd child; cd -; cat ../outside/secret",
        "cd child; cat ~-/../outside/secret",
    ] {
        let args = args_with("command", command);
        let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);
        assert_eq!(
            outcome.missing_requirements,
            vec![PermissionRequirement::PathPrefix {
                dir: resolved_filesystem_path(&outside)
            }],
            "command={command}"
        );
    }
}

#[test]
fn workspace_path_checks_embedded_command_paths() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "echo $(cat ../outside/secret.txt)");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[cfg(unix)]
#[test]
fn workspace_path_checks_pipelines_inside_command_substitutions() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside.txt");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    for command in [
        "echo x$(cat alias | head -1)",
        "echo $(OUT=alias; cat \"$OUT\")",
    ] {
        let args = args_with("command", command);
        let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);
        assert_eq!(
            outcome.missing_requirements,
            vec![PermissionRequirement::PathPrefix {
                dir: resolved_filesystem_path(temp.path())
            }],
            "command={command}"
        );
    }
}

#[test]
fn workspace_path_unresolved_named_tilde_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat ~smelt-user-that-does-not-exist/private.txt");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };

    assert!(matches!(paths[0].resolution, PathResolution::Unresolved(_)));
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn workspace_path_brace_expansion_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat {src,../outside}/secret.txt");

    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn workspace_path_shell_resolves_matching_globs_inside_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for dir in [
        "fuzz/fuzz_targets",
        "fuzz/src",
        "fuzz/src/bin",
        "crates/xtask/src/fuzz",
    ] {
        std::fs::create_dir_all(workspace.join(dir)).unwrap();
        std::fs::write(workspace.join(dir).join("target.rs"), "fn main() {}\n").unwrap();
    }
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let command = concat!(
        "printf 'HEAD: '; git log -1 --format='%h %cI %s'; ",
        "printf '\\nFuzz directory history:\\n'; ",
        "git log --format='%h %cI %s' -25 -- fuzz; ",
        "printf '\\nTarget sizes:\\n'; ",
        "wc -l fuzz/fuzz_targets/*.rs fuzz/src/*.rs fuzz/src/bin/*.rs ",
        "crates/xtask/src/fuzz/*.rs | sort -n",
    );
    let args = args_with("command", command);
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell paths, got {effects:?}");
    };
    let workspace = resolved_filesystem_path(&workspace);

    assert!(!paths.is_empty());
    assert!(paths.iter().all(|path| {
        path.resolution
            .resolved()
            .is_some_and(|path| path.starts_with(&workspace))
    }));
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[cfg(unix)]
#[test]
fn workspace_path_shell_globs_resolve_symlinked_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let outside = resolved_filesystem_path(&outside);

    for command in ["cat alias/*", "cd alias*; cat secret"] {
        let args = args_with("command", command);
        let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
        let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
            panic!("expected shell paths, got {effects:?}");
        };
        let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

        assert!(
            paths
                .iter()
                .any(|path| path.resolution.resolved() == Some(outside.as_path())),
            "command={command} paths={paths:?}"
        );
        assert_eq!(
            outcome.missing_requirements,
            vec![PermissionRequirement::PathPrefix {
                dir: outside.clone()
            }],
            "command={command}"
        );
    }
}

#[cfg(unix)]
#[test]
fn workspace_path_shell_globs_resolve_matching_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join("links")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let outside_file = outside.join("secret.rs");
    std::fs::write(&outside_file, "secret\n").unwrap();
    std::os::unix::fs::symlink(&outside_file, workspace.join("links/escape.rs")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "wc -l links/*.rs");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );

    let quoted = args_with("command", "wc -l 'links/*.rs'");
    assert_eq!(decide(&p, normal(), "bash", &quoted), Decision::Allow);
}

#[test]
fn workspace_path_shell_glob_mutation_is_not_covered_by_read_trust() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("delete.txt"), "delete me\n").unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    p.approvals
        .write()
        .unwrap()
        .add_session_path_trust("bash", PathAccess::Read, outside.clone());

    let command = format!("rm {}/*.txt", outside.display());
    let args = args_with("command", &command);
    let outcome = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_shell_incomplete_words_fail_closed() {
    let p = perms_with_workspace("/home/user/project");

    for command in ["cat 'relative/path", "cat relative/path\\"] {
        let args = args_with("command", command);
        let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
        let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
            panic!("expected shell paths, got {effects:?}");
        };
        assert!(matches!(paths[0].resolution, PathResolution::Unresolved(_)));
        assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
    }
}

#[cfg(unix)]
#[test]
fn workspace_path_symlink_loop_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::os::unix::fs::symlink("loop", workspace.join("loop")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("file_path", "loop/secret.txt");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "read_file", &args);
    let [ToolEffect::Fs(path)] = effects.as_slice() else {
        panic!("expected filesystem path, got {effects:?}");
    };

    assert!(matches!(path.resolution, PathResolution::Unresolved(_)));
    assert_eq!(decide(&p, normal(), "read_file", &args), Decision::Ask);
}

#[cfg(unix)]
#[test]
fn workspace_path_non_path_operand_does_not_probe_filesystem() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside.txt");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "echo alias");

    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[cfg(unix)]
#[test]
fn workspace_path_shell_parses_expanded_command_names() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside.txt");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "CMD=cat; \"$CMD\" alias");

    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[cfg(unix)]
#[test]
fn workspace_path_shell_checks_process_substitutions_once() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside.txt");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat <(cat alias)");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(temp.path())
        }]
    );
}

#[cfg(unix)]
#[test]
fn workspace_path_shell_unwraps_command_launchers() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "secret").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let outside = resolved_filesystem_path(&outside);

    for command in [
        "command cat alias",
        "env MODE=test cat alias",
        "env -C alias cat secret",
        "exec cat alias",
    ] {
        let args = args_with("command", command);
        let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
        let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
            panic!("expected shell paths, got {effects:?}");
        };
        assert!(
            paths.iter().any(|path| path.resolution.path() == outside),
            "command={command}, paths={paths:?}"
        );
    }
}

#[test]
fn workspace_path_structured_tools_treat_tilde_literally() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("file_path", "~/notes.txt");

    assert_eq!(decide(&p, normal(), "write_file", &args), Decision::Allow);
}

#[test]
fn workspace_path_parents_cannot_escape_filesystem_root() {
    let workspace = Path::new("/home/user/project");
    assert!(is_in_workspace(
        "/../../home/user/project/src/main.rs",
        workspace
    ));
}

#[cfg(unix)]
#[test]
fn workspace_path_resolves_parent_after_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    let target = outside.join("target");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("file_path", "alias/../secret.txt");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "read_file", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[cfg(unix)]
#[test]
fn workspace_path_bash_detects_bare_relative_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside.txt");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat alias");

    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[cfg(unix)]
#[test]
fn workspace_path_resolves_dangling_symlink_target() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("alias")).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("file_path", "alias/missing.txt");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "write_file", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_bash_expands_braced_pwd() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());

    let args = args_with("command", "cat ${PWD}/../outside/secret.txt");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );
}

#[test]
fn workspace_path_relative_escape_offers_effective_approval() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let p = perms_with_workspace(workspace.to_str().unwrap());
    let args = args_with("file_path", "../outside/new.txt");

    let before = p.evaluate_tool(normal(), ToolOrigin::Lua, "write_file", &args);
    assert_eq!(before.decision, Decision::Ask);
    assert_eq!(
        before.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&outside)
        }]
    );

    p.approvals
        .write()
        .unwrap()
        .add_session_dir(outside.clone());
    let after = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "write_file", &args);
    assert_eq!(after.decision, Decision::Allow);
    assert!(after.missing_requirements.is_empty());
}

#[test]
fn workspace_bash_ignores_dev_null_redirect_in_command_substitution() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let state_dir = temp.path().join("state/agent-mux");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let sock = state_dir.join("daemon.sock");
    let lock = state_dir.join("watch.lock");
    std::fs::write(&lock, "1234").unwrap();

    let p = perms_with_workspace(workspace.to_str().unwrap());
    let command = format!(
        "lsof {} {} 2>/dev/null | head -20; ps -p $(cat {} 2>/dev/null) -o pid=,command= 2>/dev/null || true",
        sock.display(),
        lock.display(),
        lock.display(),
    );
    let args = args_with("command", &command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&state_dir)
        }]
    );
}

#[test]
fn workspace_bash_ignores_standard_stream_devices() {
    let p = perms_with_workspace("/home/user/project");
    let command = "git diff main -- crates/store/src/session_commit.rs crates/core/src/session.rs crates/tui/src/persist.rs | git diff --no-index -- /dev/null /dev/stdin >/dev/null || true; git diff main --numstat | awk '{add+=$1; del+=$2} END {printf \"total additions: %d\\ntotal deletions: %d\\n\", add, del}'";
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(yolo(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Allow);
    assert!(outcome.missing_requirements.is_empty());
}

#[test]
fn workspace_bash_ls_directory_requires_that_directory() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "ls -la /tmp | head -50 && git status --short");
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: canonical_abs("/tmp")
        }]
    );
}

#[test]
fn workspace_bash_du_find_tmp_command_requires_tmp_not_root() {
    let p = perms_with_workspace("/home/user/project");
    let command = r#"du -sh /tmp 2>/dev/null; find /tmp -maxdepth 1 -user "$USER" \( -name 'smelt*' -o -name '*swap*' -o -name '.tmp*' \) -printf '%f\n' 2>/dev/null | wc -l; find /tmp -maxdepth 1 -user "$USER" -name '.tmp*' -mtime +1 -printf '%p\0' 2>/dev/null | xargs -0r du -sch 2>/dev/null | tail -1"#;
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: canonical_abs("/tmp")
        }]
    );
}

#[test]
fn workspace_bash_mkdir_requires_created_directory_not_parent() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let output_dir = temp.path().join("tmp");
    std::fs::create_dir_all(&workspace).unwrap();

    let p = perms_with_workspace(workspace.to_str().unwrap());
    let command = format!("mkdir -p {}", output_dir.display());
    let args = args_with("command", &command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&output_dir)
        }]
    );
}

#[test]
fn workspace_bash_benchmark_command_requires_tmp_dir_not_home() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("smelt");
    let worktree = project.join(".worktrees/transcript-virtualization");
    let tmp = temp.path().join("tmp");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();

    let mut p = perms_with_workspace(worktree.to_str().unwrap());
    p.set_allowed_roots(worktree.clone(), vec![project.clone()]);
    let command = format!(
        "cd {} && mkdir -p {} && set -o pipefail; TMPDIR={} cargo xtask bench-transcript-layout --runs 1 --workloads mixed_10mib --search --search-bytes 524288000 --resume --resume-bytes 524288000 --no-warmup 2>&1 | tee {}/smelt-transcript-scroll-model-bench-phase8.txt | tail -160",
        worktree.display(),
        tmp.display(),
        tmp.display(),
        tmp.display(),
    );
    let args = args_with("command", &command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: resolved_filesystem_path(&tmp)
        }]
    );
}

#[test]
fn workspace_bash_cargo_install_root_requires_root_directory() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let package = temp.path().join("thirdparty/agent-mux");
    let install_root = temp.path().join("home/.local");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&install_root).unwrap();

    let p = perms_with_workspace(workspace.to_str().unwrap());
    let command = format!(
        "cd {} && cargo install --path . --root {} --force",
        package.display(),
        install_root.display()
    );
    let args = args_with("command", &command);
    let effects = p.effects_for_tool(ToolOrigin::Lua, "bash", &args);
    let [ToolEffect::Shell { paths, .. }] = effects.as_slice() else {
        panic!("expected shell effect, got {effects:?}");
    };
    assert!(paths
        .iter()
        .any(|path| { path.resolution.path() == package && path.access == PathAccess::Read }));
    assert!(paths.iter().any(|path| {
        path.resolution.path() == install_root && path.access == PathAccess::Write
    }));
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![
            PermissionRequirement::PathPrefix {
                dir: resolved_filesystem_path(&package)
            },
            PermissionRequirement::PathPrefix {
                dir: resolved_filesystem_path(&install_root)
            }
        ]
    );
}

#[test]
fn workspace_downgrades_bash_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /tmp/foo");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn workspace_allows_bash_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[test]
fn workspace_allows_bash_relative() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cargo build");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[test]
fn workspace_downgrades_yolo_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /etc");
    assert_eq!(decide(&p, yolo(), "bash", &args), Decision::Ask);
}

#[test]
fn session_dir_approval_covers_resolved_bash_cd_paths() {
    let p = perms_with_workspace("/home/user/project");
    let command = "cd /tmp && cat ./foo";
    let args = args_with("command", command);

    let before = p.evaluate_tool(yolo(), ToolOrigin::Lua, "bash", &args);
    assert_eq!(before.decision, Decision::Ask);
    assert_eq!(
        before.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: canonical_abs("/tmp")
        }]
    );

    p.approvals
        .write()
        .unwrap()
        .add_session_dir(PathBuf::from("/tmp"));

    let after = p.evaluate_tool_with_approvals(yolo(), ToolOrigin::Lua, "bash", &args);
    assert_eq!(after.decision, Decision::Allow);
    assert!(after.missing_requirements.is_empty());
}

#[test]
fn session_path_grant_covers_matching_outside_workspace_tool_path() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/tmp/plan.md");

    let before = p.evaluate_tool(normal(), ToolOrigin::Lua, "read_file", &args);
    assert_eq!(before.decision, Decision::Ask);

    p.approvals.write().unwrap().add_session_path_grant(
        normal(),
        "read_file",
        PathAccess::Read,
        PathBuf::from("/tmp"),
    );

    let after = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "read_file", &args);
    assert_eq!(after.decision, Decision::Allow);
    assert!(after.missing_requirements.is_empty());
}

#[test]
fn session_path_trust_covers_matching_outside_workspace_tool_path_in_all_modes() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/tmp/plan.md");

    p.approvals.write().unwrap().add_session_path_trust(
        "read_file",
        PathAccess::Read,
        PathBuf::from("/tmp"),
    );

    for mode in [normal(), plan(), apply()] {
        let outcome = p.evaluate_tool_with_approvals(mode, ToolOrigin::Lua, "read_file", &args);
        assert_eq!(outcome.decision, Decision::Allow);
        assert!(outcome.missing_requirements.is_empty());
    }
}

#[test]
fn session_path_grant_removes_path_requirement_but_not_tool_ask() {
    let mut p = perms_with_workspace("/home/user/project");
    p.modes.get_mut("normal").unwrap().tools.remove("edit_file");
    let args = args_with("file_path", "/tmp/plan.md");

    let before = p.evaluate_tool(normal(), ToolOrigin::Lua, "edit_file", &args);
    assert_eq!(before.decision, Decision::Ask);
    assert_eq!(
        before.missing_requirements,
        vec![
            PermissionRequirement::Tool {
                tool: "edit_file".to_string()
            },
            PermissionRequirement::PathPrefix {
                dir: canonical_abs("/tmp")
            }
        ]
    );

    p.approvals.write().unwrap().add_session_path_trust(
        "edit_file",
        PathAccess::Write,
        PathBuf::from("/tmp"),
    );

    let after = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "edit_file", &args);
    assert_eq!(after.decision, Decision::Ask);
    assert_eq!(
        after.missing_requirements,
        vec![PermissionRequirement::Tool {
            tool: "edit_file".to_string()
        }]
    );
}

#[test]
fn session_path_grant_does_not_cover_other_outside_workspace_tools() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/tmp/plan.md");

    p.approvals.write().unwrap().add_session_path_grant(
        normal(),
        "edit_file",
        PathAccess::Read,
        PathBuf::from("/tmp"),
    );

    let outcome = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "read_file", &args);
    assert_eq!(outcome.decision, Decision::Ask);
}

#[test]
fn yolo_outside_workspace_dialog_offers_dir_not_command_pattern() {
    let p = perms_with_workspace("/home/user/project");
    let command = "rm -rf /tmp/foo";
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(yolo(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix {
            dir: canonical_abs("/tmp")
        }]
    );
    let options = p.approval_options("bash", &["rm *".to_string()], &outcome);
    assert_eq!(
        options.grant_sets,
        vec![vec![PermissionGrant::PathPrefix {
            dir: canonical_abs("/tmp")
        }]]
    );
}

#[test]
fn command_pattern_offer_remains_when_subcommand_causes_ask() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(&[], &["*"], &[]))]);
    let p = permissions_from_mode(mode, true, PathBuf::from("/home/user/project"));
    let command = "python3 ./build.py";
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    assert!(matches!(
        outcome.missing_requirements.as_slice(),
        [PermissionRequirement::Command { .. }]
    ));
    let options = p.approval_options("bash", &["python3 *".to_string()], &outcome);
    assert_eq!(
        options.grant_sets,
        vec![vec![PermissionGrant::Command {
            tool: "bash".to_string(),
            pattern: "python3 *".to_string()
        }]]
    );
}

#[test]
fn dialog_combines_dir_and_pattern_when_both_are_required() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(&[], &["*"], &[]))]);
    let p = permissions_from_mode(mode, true, PathBuf::from("/home/user/project"));
    let command = "python3 /tmp/build.py";
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    let options = p.approval_options("bash", &["python3 *".to_string()], &outcome);
    assert_eq!(
        options.grant_sets,
        vec![vec![
            PermissionGrant::Command {
                tool: "bash".to_string(),
                pattern: "python3 *".to_string()
            },
            PermissionGrant::PathPrefix {
                dir: canonical_abs("/tmp")
            }
        ]]
    );
}

#[test]
fn dialog_combines_command_pattern_candidates() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(&[], &["*"], &[]))]);
    let p = permissions_from_mode(mode, true, PathBuf::from("/home/user/project"));
    let command = "python3 /tmp/build.py";
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    let options = p.approval_options(
        "bash",
        &["python3 *".to_string(), "python3 /tmp/*".to_string()],
        &outcome,
    );
    assert_eq!(
        options.grant_sets,
        vec![vec![
            PermissionGrant::Command {
                tool: "bash".to_string(),
                pattern: "python3 *".to_string()
            },
            PermissionGrant::Command {
                tool: "bash".to_string(),
                pattern: "python3 /tmp/*".to_string()
            },
            PermissionGrant::PathPrefix {
                dir: canonical_abs("/tmp")
            }
        ]]
    );
}

#[test]
fn dialog_combines_path_grants_when_multiple_dirs_are_required() {
    let p = perms_with_workspace("/home/user/project");
    let command = "cat /tmp/a /var/b";
    let args = args_with("command", command);
    let outcome = p.evaluate_tool(yolo(), ToolOrigin::Lua, "bash", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    let options = p.approval_options("bash", &[], &outcome);
    assert_eq!(
        options.grant_sets,
        vec![vec![
            PermissionGrant::PathPrefix {
                dir: canonical_abs("/tmp")
            },
            PermissionGrant::PathPrefix {
                dir: canonical_abs("/var")
            }
        ]]
    );
}

#[test]
fn opaque_awk_requires_approval_beyond_blanket_bash() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", empty_ruleset())]);
    let p = permissions_from_mode(mode, true, PathBuf::from("/home/user/project"));
    let args = args_with("command", "awk '/cargo\\/fuzz/ {print}'");

    let initial = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);
    assert_eq!(
        initial.missing_requirements,
        vec![PermissionRequirement::OpaqueCommand {
            tool: "bash".to_string(),
            command: "awk *".to_string(),
        }]
    );

    p.approvals
        .write()
        .unwrap()
        .add_session_tool("bash", Vec::new());
    let after_blanket = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "bash", &args);
    assert_eq!(after_blanket.decision, Decision::Ask);

    let options = p.approval_options(
        "bash",
        &["ps *".to_string(), "awk *".to_string()],
        &after_blanket,
    );
    assert_eq!(
        options.grant_sets,
        vec![vec![PermissionGrant::Command {
            tool: "bash".to_string(),
            pattern: "awk *".to_string(),
        }]]
    );
    p.approvals
        .write()
        .unwrap()
        .add_session_grant(options.grant_sets[0][0].clone());

    let after_pattern = p.evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "bash", &args);
    assert_eq!(after_pattern.decision, Decision::Allow);
    assert!(after_pattern.missing_requirements.is_empty());

    let entries = p.approvals.read().unwrap().session_tool_approvals();
    assert_eq!(
        entries,
        vec![
            SessionToolApproval {
                tool: "bash".to_string(),
                pattern: None,
            },
            SessionToolApproval {
                tool: "bash".to_string(),
                pattern: Some("awk *".to_string()),
            },
        ]
    );
    let mut restored = RuntimeApprovals::new();
    restored.set_session(entries, Vec::new(), Vec::new());
    assert!(
        restored.requirement_satisfied(&PermissionRequirement::Tool {
            tool: "bash".to_string(),
        })
    );
    assert!(restored.requirement_satisfied(&initial.missing_requirements[0]));
}

#[test]
fn opaque_awk_approval_combines_with_outside_path_approval() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", empty_ruleset())]);
    let p = permissions_from_mode(mode, true, PathBuf::from("/home/user/project"));
    let args = args_with("command", "awk '{print}' /tmp/input");

    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "bash", &args);
    assert_eq!(outcome.decision, Decision::Ask);
    assert_eq!(
        outcome.missing_requirements,
        vec![
            PermissionRequirement::PathPrefix {
                dir: canonical_abs("/tmp"),
            },
            PermissionRequirement::OpaqueCommand {
                tool: "bash".to_string(),
                command: "awk *".to_string(),
            },
        ]
    );
    let options = p.approval_options("bash", &["awk *".to_string()], &outcome);
    assert_eq!(
        options.grant_sets,
        vec![vec![
            PermissionGrant::Command {
                tool: "bash".to_string(),
                pattern: "awk *".to_string(),
            },
            PermissionGrant::PathPrefix {
                dir: canonical_abs("/tmp"),
            },
        ]]
    );
}

#[test]
fn workspace_yolo_allows_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/home/user/project/foo.txt");
    assert_eq!(decide(&p, yolo(), "write_file", &args), Decision::Allow);
}

#[test]
fn workspace_restriction_off_allows_everything() {
    let mut p = perms_with_workspace("/home/user/project");
    p.restrict_to_workspace = false;
    let args = args_with("file_path", "/etc/passwd");
    assert_eq!(decide(&p, normal(), "read_file", &args), Decision::Allow);
}

#[test]
fn workspace_ask_stays_ask() {
    let mut p = perms_with_workspace("/home/user/project");
    p.modes
        .get_mut("normal")
        .unwrap()
        .tools
        .remove("write_file"); // defaults to Ask
    let args = args_with("file_path", "/home/user/project/foo.txt");
    assert_eq!(decide(&p, normal(), "write_file", &args), Decision::Ask);
}

#[test]
fn workspace_glob_outside_downgrades() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("path", "/tmp");
    assert_eq!(decide(&p, normal(), "glob", &args), Decision::Ask);
}

#[test]
fn workspace_glob_directory_grant_uses_directory_itself() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let p = perms_with_workspace(workspace.to_str().unwrap());
    let args = args_with("path", outside.to_str().unwrap());
    let outcome = p.evaluate_tool(normal(), ToolOrigin::Lua, "glob", &args);

    assert_eq!(outcome.decision, Decision::Ask);
    let outside = resolved_filesystem_path(&outside);
    assert_eq!(
        outcome.missing_requirements,
        vec![PermissionRequirement::PathPrefix { dir: outside }]
    );
}

#[test]
fn workspace_filesystem_resolution_fails_closed_for_relative_paths() {
    assert!(matches!(
        resolve_filesystem_path(Path::new("relative/path")),
        PathResolution::Unresolved(_)
    ));
}

#[cfg(unix)]
#[test]
fn workspace_resolves_symlink_prefix_for_missing_children() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let alias = temp.path().join("alias");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();

    assert_eq!(
        resolved_tool_path(
            alias.join("missing/child.txt").to_str().unwrap(),
            Path::new("/")
        ),
        resolved_filesystem_path(&target).join("missing/child.txt")
    );
}

#[cfg(unix)]
#[test]
fn workspace_allows_missing_children_under_symlinked_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let alias = temp.path().join("alias");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();

    let p = perms_with_workspace(alias.to_str().unwrap());
    let args = args_with("file_path", "missing/child.txt");

    assert_eq!(decide(&p, normal(), "write_file", &args), Decision::Allow);
}

#[test]
fn workspace_no_path_tools_unaffected() {
    let p = perms_with_workspace("/home/user/project");
    let args = HashMap::new();
    assert_eq!(decide(&p, yolo(), "web_search", &args), Decision::Allow);
}

// --- mode behavior is configurable ---

fn yolo_allow_permissions() -> Permissions {
    Permissions::from_raw_with_mode_behaviors(
        &RawConfig::default().permissions,
        &ToolDefaults::default(),
        HashMap::from([(
            "yolo".to_string(),
            ModeBehavior {
                default_decision: Decision::Allow,
                allow_subcommands_by_default: true,
                ask_on_output_redirection: false,
            },
        )]),
    )
}

#[test]
fn configured_allow_mode_defaults_to_allow() {
    let p = yolo_allow_permissions();
    assert_eq!(p.check_tool(yolo(), "bash"), Decision::Allow);
    assert_eq!(p.check_tool(yolo(), "edit_file"), Decision::Allow);
    assert_eq!(p.check_tool(yolo(), "write_file"), Decision::Allow);
    assert_eq!(p.check_tool(yolo(), "read_file"), Decision::Allow);
}

#[test]
fn configured_allow_mode_unknown_tool_defaults_allow() {
    let p = yolo_allow_permissions();
    assert_eq!(p.check_tool(yolo(), "some_unknown_tool"), Decision::Allow);
}

#[test]
fn configured_allow_mode_bash_allows_everything_by_default() {
    let p = yolo_allow_permissions();
    assert_eq!(
        p.check_subcommand(yolo(), "bash", "rm -rf /"),
        Decision::Allow
    );
}

#[test]
fn normal_unknown_tool_defaults_ask() {
    let p = Permissions::load();
    assert_eq!(p.check_tool(normal(), "some_unknown_tool"), Decision::Ask);
}

fn plan_policy_permissions() -> Permissions {
    let mut tools = HashMap::new();
    for name in [
        "read_file",
        "glob",
        "grep",
        "ask_user_question",
        "read_process_output",
    ] {
        tools.insert(name.to_string(), Decision::Allow);
    }
    tools.insert("enter_worktree".to_string(), Decision::Ask);
    let mut mode = mode_perms(tools, &[("bash", ruleset(&["ls *"], &[], &[]))]);
    mode.effects = EffectPerms {
        read: Some(Decision::Allow),
        write: Some(Decision::Deny),
        network: Some(Decision::Ask),
        process: Some(Decision::Deny),
        config: Some(Decision::Deny),
        user: Some(Decision::Allow),
        other: Some(Decision::Ask),
    };
    let mut p = permissions_from_mode(mode, false, PathBuf::new());
    p.set_paths_fn(stub_paths_fn());
    p
}

#[test]
fn plan_policy_classifies_registered_tools() {
    let p = plan_policy_permissions();
    let cases = [
        (
            "read_file",
            args_with("file_path", "src/lib.rs"),
            Decision::Allow,
        ),
        ("glob", args_with("path", "src"), Decision::Allow),
        ("grep", args_with("path", "src"), Decision::Allow),
        ("ask_user_question", HashMap::new(), Decision::Allow),
        ("read_process_output", HashMap::new(), Decision::Allow),
        (
            "edit_file",
            args_with("file_path", "src/lib.rs"),
            Decision::Deny,
        ),
        (
            "write_file",
            args_with("file_path", "src/lib.rs"),
            Decision::Deny,
        ),
        (
            "edit_notebook",
            args_with("notebook_path", "notebook.ipynb"),
            Decision::Deny,
        ),
        ("edit_file", HashMap::new(), Decision::Deny),
        ("write_file", HashMap::new(), Decision::Deny),
        ("edit_notebook", HashMap::new(), Decision::Deny),
        ("custom_tool", HashMap::new(), Decision::Ask),
        (
            "web_fetch",
            args_with("url", "https://example.com"),
            Decision::Ask,
        ),
        ("web_search", HashMap::new(), Decision::Ask),
        ("enter_worktree", HashMap::new(), Decision::Ask),
        ("stop_process", HashMap::new(), Decision::Deny),
        ("smelt_reload", HashMap::new(), Decision::Deny),
    ];
    for (tool, args, expected) in cases {
        assert_eq!(decide(&p, plan(), tool, &args), expected, "tool={tool}");
    }
}

#[test]
fn plan_policy_classifies_bash_by_effects() {
    let p = plan_policy_permissions();
    let mut background_pwd = args_with("command", "pwd");
    background_pwd.insert("background".to_string(), Value::Bool(true));
    let cases = [
        (args_with("command", "ls src"), Decision::Allow),
        (args_with("command", "python3 script.py"), Decision::Ask),
        (args_with("command", "echo hi > out.txt"), Decision::Deny),
        (args_with("command", "cargo test"), Decision::Deny),
        (args_with("command", "cargo +nightly test"), Decision::Deny),
        (args_with("command", "rm -rf target"), Decision::Deny),
        (
            args_with("command", "env MODE=test rm -rf target"),
            Decision::Deny,
        ),
        (
            args_with(
                "command",
                "status=0; if [ \"$status\" -eq 0 ]; then cargo test; fi",
            ),
            Decision::Deny,
        ),
        (background_pwd, Decision::Deny),
    ];
    for (args, expected) in cases {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            decide(&p, plan(), "bash", &args),
            expected,
            "command={command}"
        );
    }
}

#[test]
fn bash_pattern_allow_overrides_process_effect_for_background_tool_run() {
    let p = plan_policy_permissions();
    let mut args = args_with("command", "ls src");
    args.insert("background".to_string(), Value::Bool(true));
    assert_eq!(decide(&p, plan(), "bash", &args), Decision::Allow);
}

#[test]
fn plan_policy_mcp_defaults_to_ask() {
    let p = plan_policy_permissions();
    assert_eq!(
        p.evaluate_tool(
            plan(),
            ToolOrigin::Mcp,
            "filesystem_write_file",
            &HashMap::new()
        )
        .decision,
        Decision::Ask
    );
}

#[test]
fn plan_policy_tool_rule_overrides_write_effect() {
    let mut p = plan_policy_permissions();
    p.modes
        .get_mut("plan")
        .unwrap()
        .tools
        .insert("edit_file".into(), Decision::Ask);
    let args = args_with("file_path", "src/lib.rs");
    assert_eq!(decide(&p, plan(), "edit_file", &args), Decision::Ask);
}

#[test]
fn plan_policy_keeps_shell_writes_denied() {
    let p = plan_policy_permissions();
    let temp = tempfile::tempdir().unwrap();
    let artifact_dir = temp.path().join("session/plans/20260101-000000-demo");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let shell_args = args_with(
        "command",
        &format!("echo hi > {}", artifact_dir.join("plan.md").display()),
    );
    assert_eq!(
        p.evaluate_tool_with_approvals(plan(), ToolOrigin::Lua, "bash", &shell_args)
            .decision,
        Decision::Deny
    );
}

// --- output redirection escalation ---

#[test]
fn has_output_redirection_simple_greater() {
    assert!(has_output_redirection("cat file > out.txt"));
}

#[test]
fn has_output_redirection_double_greater() {
    assert!(has_output_redirection("cat file >> out.txt"));
}

#[test]
fn has_output_redirection_ampersand_greater() {
    assert!(has_output_redirection("cargo build &> output.log"));
}

#[test]
fn has_output_redirection_double_ampersand_greater() {
    assert!(has_output_redirection("cargo build &>> output.log"));
}

#[test]
fn has_output_redirection_no_redirection() {
    assert!(!has_output_redirection("cat file"));
}

#[test]
fn has_output_redirection_input_only() {
    assert!(!has_output_redirection("cat < input.txt"));
}

#[test]
fn has_output_redirection_heredoc_only() {
    // << alone is not an output redirection
    assert!(!has_output_redirection("cat << EOF"));
}

#[test]
fn has_output_redirection_heredoc_with_output_redirect() {
    // heredoc with output redirection to a file
    assert!(has_output_redirection("cat << 'EOF' > file.txt\nbody\nEOF"));
}

#[test]
fn has_output_redirection_inside_quotes_ignored() {
    // > inside quotes should not be detected as redirection
    assert!(!has_output_redirection(r#"echo ">" file.txt"#));
}

#[test]
fn has_output_redirection_mixed_quotes() {
    assert!(has_output_redirection("cat file > 'out.txt'"));
}

#[test]
fn has_output_redirection_stderr_redirect() {
    // 2>&1 is fd duplication, not file output redirection
    assert!(!has_output_redirection("cargo build 2>&1"));
}

#[test]
fn dev_null_redirect_not_escalated() {
    assert!(!has_output_redirection("find /tmp 2>/dev/null"));
}

#[test]
fn dev_null_redirect_with_space() {
    assert!(!has_output_redirection("find /tmp 2> /dev/null"));
}

#[test]
fn dev_null_stdout_redirect() {
    assert!(!has_output_redirection("echo hello > /dev/null"));
}

#[test]
fn dev_null_append_redirect() {
    assert!(!has_output_redirection("echo hello >> /dev/null"));
}

#[test]
fn dev_null_ampersand_redirect() {
    assert!(!has_output_redirection("cargo build &> /dev/null"));
}

#[test]
fn standard_output_stream_redirects_are_not_escalated() {
    assert!(!has_output_redirection("cat > /dev/stdout"));
    assert!(!has_output_redirection("cat 2> /dev/stderr"));
}

#[test]
fn dev_null_in_chain_not_escalated() {
    // 2>/dev/null is harmless, the whole command should stay allowed
    assert!(!has_output_redirection(
        "tree -L 3 /tmp 2>/dev/null || find /tmp -type d"
    ));
}

#[test]
fn dev_null_mixed_with_real_redirect() {
    // One redirect to /dev/null, but another to a real file - should escalate
    assert!(has_output_redirection("cmd 2>/dev/null > out.txt"));
}

#[test]
fn dev_null_prefix_not_treated_as_dev_null() {
    // `/dev/null-x` is a real file, not `/dev/null` - should escalate
    assert!(has_output_redirection("echo hi > /dev/null-backup"));
}

#[test]
fn auto_allowed_with_dev_null_stays_allow() {
    assert_bash(
        &["find *"],
        &[],
        &[],
        normal(),
        "find /tmp 2>/dev/null",
        Decision::Allow,
    );
}

#[test]
fn auto_allowed_with_output_redirect_stays_allow_in_core_parser() {
    let p = perms_with_bash(&["cat *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "cat file.txt > output.txt"),
        Decision::Allow
    );
}

#[test]
fn auto_allowed_with_append_redirect_stays_allow_in_core_parser() {
    assert_bash(
        &["cat *"],
        &[],
        &[],
        normal(),
        "cat file.txt >> output.txt",
        Decision::Allow,
    );
}

#[test]
fn auto_allowed_heredoc_with_redirect_stays_allow_in_core_parser() {
    let p = perms_with_bash(&["cat *"], &[], &[]);
    let cmd = "cat << 'EOF' > long_file.txt\nhello\nworld\nEOF";
    assert_eq!(p.check_subcommand(normal(), "bash", cmd), Decision::Allow);
}

#[test]
fn auto_allowed_no_redirect_stays_allow() {
    // Without redirection, cat * should still be allowed
    let p = perms_with_bash(&["cat *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "cat file.txt"),
        Decision::Allow
    );
}

#[test]
fn chained_command_with_redirect_stays_allow_in_core_parser() {
    let p = perms_with_bash(&["ls *", "cat *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "ls -la && cat file > out.txt"),
        Decision::Allow
    );
}

#[test]
fn pipe_with_output_redirect_stays_allow_in_core_parser() {
    let p = perms_with_bash(&["cat *", "grep *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "cat file | grep foo > out.txt"),
        Decision::Allow
    );
}

#[test]
fn denied_command_with_redirect_stays_deny() {
    let p = perms_with_bash(&[], &[], &["rm *"]);
    // rm is denied regardless of redirection
    assert_eq!(
        p.check_subcommand(normal(), "bash", "rm file.txt > /dev/null"),
        Decision::Deny
    );
}

// --- specificity: specific ask beats broad allow ---

#[test]
fn specific_ask_beats_broad_allow() {
    let rs = ruleset(&["git *"], &["git push *"], &[]);
    assert_eq!(check_ruleset(&rs, "git push foo"), Decision::Ask);
}

#[test]
fn broad_allow_still_works_for_non_specific() {
    let rs = ruleset(&["git *"], &["git push *"], &[]);
    assert_eq!(check_ruleset(&rs, "git status"), Decision::Allow);
}

// --- bash decide_base ---

#[test]
fn bash_tool_allow_pattern_ask() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(&[], &["git push *"], &[]))]);
    let perms = permissions_from_mode(mode, false, PathBuf::new());
    let args = args_with("command", "git push origin main");
    assert_eq!(decide_base(&perms, yolo(), "bash", &args), Decision::Ask);
}

#[test]
fn bash_allowed_redirect_asks_in_normal_mode() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(&["echo *"], &[], &[]))]);
    let perms = permissions_from_mode(mode, false, PathBuf::new());
    let args = args_with("command", "echo hi > out.txt");
    assert_eq!(decide_base(&perms, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn web_fetch_pattern_allow_short_circuits_tool_ask() {
    let mut tools = HashMap::new();
    tools.insert("web_fetch".to_string(), Decision::Ask);
    let mode = mode_perms(
        tools,
        &[("web_fetch", ruleset(&["https://example.com/*"], &[], &[]))],
    );
    let perms = permissions_from_mode(mode, false, PathBuf::new());
    let args = args_with("url", "https://example.com/docs");
    assert_eq!(
        decide_base(&perms, normal(), "web_fetch", &args),
        Decision::Allow
    );
}

#[test]
fn web_fetch_pattern_allow_overrides_tool_deny() {
    let mut tools = HashMap::new();
    tools.insert("web_fetch".to_string(), Decision::Deny);
    let mode = mode_perms(
        tools,
        &[("web_fetch", ruleset(&["https://example.com/*"], &[], &[]))],
    );
    let perms = permissions_from_mode(mode, false, PathBuf::new());
    let args = args_with("url", "https://example.com/docs");
    assert_eq!(
        decide_base(&perms, normal(), "web_fetch", &args),
        Decision::Allow
    );
}

// --- override tightening ---

#[test]
fn override_tightens_allow_to_ask() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", empty_ruleset())]);
    let perms = permissions_from_mode(mode, false, PathBuf::new());
    let overrides = protocol::PermissionOverrides {
        tools: Some(protocol::RuleSetOverride {
            allow: vec![],
            ask: vec!["bash".to_string()],
            deny: vec![],
        }),
        subcommands: std::collections::HashMap::new(),
    };
    let tightened = perms.with_overrides(&overrides);
    assert_eq!(tightened.check_tool(yolo(), "bash"), Decision::Ask);
}

// --- cd command handling ---

#[test]
fn cd_alone_is_allowed() {
    assert_bash(&[], &[], &[], normal(), "cd /tmp", Decision::Allow);
}

#[test]
fn cd_no_args_is_allowed() {
    assert_bash(&[], &[], &[], normal(), "cd", Decision::Allow);
}

#[test]
fn cd_in_chain_does_not_escalate() {
    // cd should not contribute to the worst decision; only ls matters
    let p = perms_with_bash(&["ls *"], &[], &[]);
    assert_eq!(
        p.check_subcommand(normal(), "bash", "cd /tmp && ls -la"),
        Decision::Allow
    );
}

#[test]
fn cd_with_denied_command_still_denies() {
    assert_bash(
        &[],
        &[],
        &["rm *"],
        normal(),
        "cd /tmp && rm -rf foo",
        Decision::Deny,
    );
}

#[test]
fn cd_outside_workspace_downgrades_to_ask() {
    // cd itself is Allow, but the workspace path restriction catches /tmp
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /tmp && ls");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn cd_inside_workspace_stays_allowed() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /home/user/project/src && ls");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[test]
fn cd_workspace_root_stays_allowed() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /home/user/project && cargo build");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Allow);
}

#[test]
fn shell_ignores_git_commit_message_paths() {
    let paths = extract_paths_from_command("git commit -m 'fix /api/foo'");
    assert!(paths.is_empty(), "got: {paths:?}");
}

#[test]
fn shell_ignores_ssh_remote_command_paths() {
    let paths = extract_paths_from_command("ssh host 'cat /etc/passwd'");
    assert!(paths.is_empty(), "got: {paths:?}");
}

#[test]
fn shell_ignores_sed_script_but_records_file_operand() {
    let paths = extract_paths_from_command("sed 's#/old#/new#' file");
    assert_eq!(paths, vec!["file"]);
}

#[test]
fn shell_ignores_awk_program_text_but_records_input_files() {
    assert!(extract_paths_from_command(
        "ps -eo pid=,cmd= | awk '/cargo (xtask fuzz|fuzz)|fuzz\\/target/ && !/awk/ {print}'"
    )
    .is_empty());
    assert_eq!(
        extract_paths_from_command(
            "awk -F / -v root=/tmp '/cargo/ {print}' /tmp/processes relative/input"
        ),
        vec!["/tmp/processes", "relative/input"]
    );
    assert_eq!(
        extract_paths_from_command("gawk -f /tmp/program.awk -- /tmp/processes root=/tmp -"),
        vec!["/tmp/program.awk", "/tmp/processes"]
    );
}

#[test]
fn shell_models_awk_cli_paths_without_resolving_search_paths() {
    let cases: &[(&str, &[(&str, PathAccess)])] = &[
        (
            "awk -f script.awk -i library.awk -l extension input",
            &[("input", PathAccess::Read)],
        ),
        (
            "awk -f=./main.awk -i../library.awk -l/tmp/extension.so /tmp/input",
            &[
                ("./main.awk", PathAccess::Read),
                ("../library.awk", PathAccess::Read),
                ("/tmp/extension.so", PathAccess::Read),
                ("/tmp/input", PathAccess::Read),
            ],
        ),
        (
            "gawk --file=/tmp/main.awk --include ./library.awk --load=plugin --source '{print}' input",
            &[
                ("/tmp/main.awk", PathAccess::Read),
                ("./library.awk", PathAccess::Read),
                ("input", PathAccess::Read),
            ],
        ),
        (
            "gawk -d -o/tmp/pretty.awk -p=../profile --debug=./debug.cmd --source '{print}' input",
            &[
                ("awkvars.out", PathAccess::Write),
                ("/tmp/pretty.awk", PathAccess::Write),
                ("../profile", PathAccess::Write),
                ("./debug.cmd", PathAccess::Read),
                ("input", PathAccess::Read),
            ],
        ),
        (
            "gawk -D -o '{print}' /tmp/input",
            &[
                ("awkprof.out", PathAccess::Write),
                ("/tmp/input", PathAccess::Read),
            ],
        ),
        (
            "mawk -W exec ./main.awk /tmp/input",
            &[
                ("./main.awk", PathAccess::Read),
                ("/tmp/input", PathAccess::Read),
            ],
        ),
        (
            "gawk -E./main.awk root=/tmp -",
            &[
                ("./main.awk", PathAccess::Read),
                ("root=/tmp", PathAccess::Read),
            ],
        ),
    ];

    for (command, expected) in cases {
        let analysis = analyze_shell_command(command, Path::new("/workspace"));
        let actual: Vec<_> = analysis
            .paths
            .iter()
            .map(|path| (path.raw_path.as_str(), path.access.clone()))
            .collect();
        assert_eq!(actual, *expected, "command={command}");
    }
}

#[test]
fn shell_treats_unknown_awk_options_as_opaque_without_guessing_paths() {
    let command = "awk --future-option '/cargo/foo/' /tmp/input";
    let analysis = analyze_shell_command(command, Path::new("/workspace"));

    assert!(analysis.paths.is_empty());
    assert_eq!(
        analysis.opaque_commands,
        vec![OpaqueShellCommand {
            command: "awk *".to_string(),
        }]
    );
}

#[test]
fn shell_marks_each_awk_implementation_as_opaque() {
    for command in ["awk", "gawk", "mawk", "nawk"] {
        let source = format!("{command} '{{print}}'");
        let analysis = analyze_shell_command(&source, Path::new("/workspace"));
        assert_eq!(
            analysis.opaque_commands,
            vec![OpaqueShellCommand {
                command: format!("{command} *"),
            }]
        );
    }

    for command in ["awk --help", "gawk --version", "mawk -W help"] {
        assert!(
            analyze_shell_command(command, Path::new("/workspace"))
                .opaque_commands
                .is_empty(),
            "command={command}"
        );
    }
}

#[test]
fn shell_preserves_opaque_awk_effects_through_env() {
    let analysis = analyze_shell_command(
        "env --file /tmp/environment -u MODE awk '/cargo\\/fuzz/ {print}'",
        Path::new("/workspace"),
    );

    assert_eq!(
        analysis
            .paths
            .iter()
            .map(|path| path.raw_path.as_str())
            .collect::<Vec<_>>(),
        vec!["/tmp/environment"]
    );
    assert_eq!(
        analysis.opaque_commands,
        vec![OpaqueShellCommand {
            command: "awk *".to_string(),
        }]
    );
}

#[test]
fn shell_treats_env_split_strings_as_opaque() {
    let analysis = analyze_shell_command(
        "env -S \"awk 'BEGIN { system(\\\"cat /etc/passwd\\\") }'\"",
        Path::new("/workspace"),
    );

    assert!(analysis.paths.is_empty());
    assert_eq!(
        analysis.opaque_commands,
        vec![OpaqueShellCommand {
            command: "env *".to_string(),
        }]
    );
}

#[test]
fn shell_reports_find_relative_escape() {
    let paths = extract_paths_from_command("find ../third_party -name '*.rs'");
    assert_eq!(paths, vec!["../third_party"]);
}

#[test]
fn shell_ignores_file_descriptor_redirections_as_paths() {
    assert!(extract_paths_from_command("cat <&0").is_empty());
    assert_eq!(extract_paths_from_command("cat file 2>&1"), vec!["file"]);
}

#[test]
fn shell_distinguishes_option_values_from_path_operands() {
    let cases = [
        ("head -n 5 file", vec!["file"]),
        ("tail --lines=5 file", vec!["file"]),
        ("ls --ignore '*.tmp' directory", vec!["directory"]),
        ("sed -f script.sed file", vec!["script.sed", "file"]),
        ("grep -f patterns.txt file", vec!["patterns.txt", "file"]),
        ("find -H ../outside -name file", vec!["../outside"]),
        (
            "sort -o ../outside/sorted.txt input",
            vec!["../outside/sorted.txt", "input"],
        ),
    ];

    for (command, expected) in cases {
        assert_eq!(
            extract_paths_from_command(command),
            expected,
            "command={command}"
        );
    }
}

#[test]
fn shell_cd_updates_relative_path_base_for_workspace() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd .. && grep needle other_project");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn shell_output_redirection_reports_write_effect() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "echo hi > /tmp/out");
    assert_eq!(decide(&p, normal(), "bash", &args), Decision::Ask);
}

#[test]
fn shell_dev_null_redirection_is_ignored() {
    let paths = extract_paths_from_command("echo hi > /dev/null");
    assert!(paths.is_empty(), "got: {paths:?}");
}

#[test]
fn shell_standard_stream_devices_are_ignored() {
    let paths = extract_paths_from_command("cat /dev/stdin > /dev/stdout 2> /dev/stderr");
    assert!(paths.is_empty(), "got: {paths:?}");
}

#[test]
fn heredoc_paths_not_extracted() {
    // Paths inside heredoc bodies are data, not shell arguments.
    let cmd = "python3 << 'PYEOF'\nwith open('/tmp/secret') as f:\n    pass\nPYEOF";
    let paths = extract_paths_from_command(cmd);
    assert!(paths.is_empty(), "got: {paths:?}");
}

#[test]
fn heredoc_paths_outside_body_still_extracted() {
    let cmd = "cd /tmp && python3 << 'EOF'\nopen('/etc/passwd')\nEOF";
    let paths = extract_paths_from_command(cmd);
    assert_eq!(paths, vec!["/tmp"]);
}

#[test]
fn runtime_tool_approval_does_not_bypass_workspace_restriction() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    let args = args_with("command", "rm -rf /tmp/foo");
    assert!(!rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn runtime_tool_and_dir_approval_allow_outside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "rm -rf /tmp/foo");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn runtime_dir_approval_allows_default_allowed_command_outside_workspace() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "cat /tmp/foo");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn runtime_tool_approval_allows_inside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn runtime_dir_approval_does_not_affect_inside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert!(!rt.is_auto_approved(&p, normal(), "bash", &args));
}

// --- tilde path normalization for path requirements ---

#[test]
fn dirs_approved_tilde_stored_absolute_queried() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    let home = engine::paths::home_dir();
    let abs = format!("{}/syncthing/vault/file.txt", home.display());
    assert!(dirs_approved(&rt, &[&abs]));
}

#[test]
fn dirs_approved_absolute_stored_tilde_queried() {
    let home = engine::paths::home_dir();
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(home.join("syncthing"));
    assert!(dirs_approved(&rt, &["~/syncthing/vault/file.txt"]));
}

#[test]
fn dirs_approved_both_tilde() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    assert!(dirs_approved(&rt, &["~/syncthing/vault"]));
}

#[test]
fn dirs_approved_both_absolute() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp/data"));
    assert!(dirs_approved(&rt, &["/tmp/data/subdir/file.txt"]));
}

#[test]
fn dirs_approved_no_false_prefix_match() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/sync"));
    assert!(!dirs_approved(&rt, &["~/syncthing/file.txt"]));
}

#[test]
fn dirs_approved_exact_dir_match() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing/vault"));
    assert!(dirs_approved(&rt, &["~/syncthing/vault/file.txt"]));
}

#[test]
fn dirs_approved_parent_not_covered() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing/vault"));
    assert!(!dirs_approved(&rt, &["~/syncthing/other/file.txt"]));
}

#[test]
fn dirs_approved_path_is_dir_itself() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    assert!(dirs_approved(&rt, &["/tmp"]));
}

#[test]
fn dirs_approved_multiple_paths_all_covered() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    rt.add_session_dir(PathBuf::from("/tmp"));
    assert!(dirs_approved(
        &rt,
        &["~/syncthing/vault/a.txt", "/tmp/b.txt"]
    ));
}

#[test]
fn dirs_approved_multiple_paths_one_uncovered() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    assert!(!dirs_approved(
        &rt,
        &["~/syncthing/vault/a.txt", "/tmp/b.txt"]
    ));
}

// --- tilde normalization in is_auto_approved ---

/// Workspace-restricted permissions with an explicit bash allow list.
fn perms_with_workspace_bash_allow(workspace: &str, bash_allow: &[&str]) -> Permissions {
    let mut tools = HashMap::new();
    tools.insert("read_file".to_string(), Decision::Allow);
    tools.insert("write_file".to_string(), Decision::Allow);
    tools.insert("edit_file".to_string(), Decision::Allow);
    tools.insert("glob".to_string(), Decision::Allow);
    tools.insert("grep".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(bash_allow, &[], &[]))]);
    let mut p = permissions_from_mode(mode, true, PathBuf::from(workspace));
    p.set_paths_fn(stub_paths_fn());
    p
}

#[test]
fn tilde_dir_approval_works_for_absolute_read_file() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    let file = format!("{}/syncthing/vault/notes.md", home.display());
    let args = args_with("file_path", &file);
    assert!(rt.is_auto_approved(&p, normal(), "read_file", &args));
}

#[test]
fn absolute_dir_approval_works_for_tilde_bash() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(home.join("syncthing"));
    let args = args_with("command", "cat ~/syncthing/vault/notes.md");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn dir_approval_alone_insufficient_for_ask_command_outside_workspace() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_bash_allow(&workspace, &[]);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    let args = args_with("command", "rm ~/syncthing/vault/old.md");
    assert!(!rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn dir_plus_tool_approval_for_ask_command_outside_workspace() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_bash_allow(&workspace, &[]);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    let args = args_with("command", "rm ~/syncthing/vault/old.md");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn compound_command_default_allowed_with_dir_approval() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "find /tmp/data -type f | sort");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn compound_command_with_ask_subcommand_needs_tool_approval() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_bash_allow(&workspace, &["find *"]);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "find /tmp/data -name '*.py' | python3");
    assert!(!rt.is_auto_approved(&p, normal(), "bash", &args));
}

#[test]
fn compound_command_with_ask_subcommand_and_tool_approval() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_bash_allow(&workspace, &["find *"]);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    rt.add_session_tool("bash", vec![glob::Pattern::new("python3").unwrap()]);
    let args = args_with("command", "find /tmp/data -name '*.py' | python3");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args));
}

// ── RuntimeApprovals lifecycle (backfill) ───────────────────────────

fn pat(s: &str) -> glob::Pattern {
    glob::Pattern::new(s).unwrap()
}

#[test]
fn approvals_add_session_tool_stores_patterns_on_first_call() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("ls *"), pat("cat *")]);
    assert!(rt.has_pattern("bash", "ls *"));
    assert!(rt.has_pattern("bash", "cat *"));
    // Blanket approval is NOT in effect - only patterns match.
    assert!(rt.is_approved("bash", "ls /tmp", None));
    assert!(!rt.is_approved("bash", "rm /tmp", None));
}

#[test]
fn approvals_add_session_tool_with_empty_patterns_grants_blanket_approval() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", Vec::new());
    assert!(rt.is_approved("bash", "anything goes", None));
}

#[test]
fn approvals_add_session_tool_blanket_retains_existing_patterns() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("ls *")]);
    rt.add_session_tool("bash", Vec::new());
    assert!(rt.is_approved("bash", "anything", None));
    assert!(rt.has_explicit_pattern("bash", "ls *"));
}

#[test]
fn approvals_add_session_tool_stores_patterns_alongside_blanket() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", Vec::new());
    rt.add_session_tool("bash", vec![pat("ls *")]);
    assert!(rt.is_approved("bash", "anything goes", None));
    assert!(rt.has_explicit_pattern("bash", "ls *"));
}

#[test]
fn approvals_add_session_tool_extends_existing_pattern_list() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("ls *")]);
    rt.add_session_tool("bash", vec![pat("cat *")]);
    assert!(rt.has_pattern("bash", "ls *"));
    assert!(rt.has_pattern("bash", "cat *"));
}

#[test]
fn approvals_add_workspace_tool_stores_patterns_on_first_call() {
    let mut rt = RuntimeApprovals::new();
    rt.add_workspace_tool("bash", vec![pat("git *")]);
    assert!(rt.has_pattern("bash", "git *"));
    assert!(rt.is_approved("bash", "git status", None));
    assert!(!rt.is_approved("bash", "rm -rf /", None));
}

#[test]
fn approvals_add_workspace_tool_with_empty_patterns_grants_blanket_approval() {
    let mut rt = RuntimeApprovals::new();
    rt.add_workspace_tool("bash", Vec::new());
    assert!(rt.is_approved("bash", "anything", None));
}

#[test]
fn approvals_add_workspace_tool_extends_existing_pattern_list() {
    let mut rt = RuntimeApprovals::new();
    rt.add_workspace_tool("bash", vec![pat("git *")]);
    rt.add_workspace_tool("bash", vec![pat("ls *")]);
    assert!(rt.has_pattern("bash", "git *"));
    assert!(rt.has_pattern("bash", "ls *"));
}

#[test]
fn approvals_add_session_dir_dedupes_after_tilde_expansion() {
    let mut rt = RuntimeApprovals::new();
    let abs = std::env::temp_dir();
    rt.add_session_dir(abs.clone());
    rt.add_session_dir(abs.clone());
    assert_eq!(rt.session_dirs().len(), 1);
}

#[test]
fn approvals_add_workspace_dir_dedupes_after_tilde_expansion() {
    let mut rt = RuntimeApprovals::new();
    rt.add_workspace_dir(std::env::temp_dir());
    rt.add_workspace_dir(std::env::temp_dir());
    // workspace_dirs is private; verify via dirs_approved against the temp path.
    let temp = std::env::temp_dir();
    let path_in = format!("{}/x", temp.to_string_lossy());
    assert!(dirs_approved(&rt, &[&path_in]));
}

#[test]
fn approvals_clear_session_removes_tools_and_dirs() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("git *")]);
    rt.add_session_dir(PathBuf::from("/tmp"));
    rt.clear_session();
    assert!(!rt.has_pattern("bash", "git *"));
    assert!(rt.session_dirs().is_empty());
}

#[test]
fn approvals_clear_session_preserves_workspace_state() {
    let mut rt = RuntimeApprovals::new();
    let mut seed: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
    seed.insert("bash".into(), vec![pat("git *")]);
    rt.load_workspace(seed, vec![PathBuf::from("/work")]);
    rt.add_session_tool("bash", vec![pat("ls *")]);
    rt.clear_session();
    assert!(rt.has_pattern("bash", "git *"));
    assert!(dirs_approved(&rt, &["/work/x"]));
}

#[test]
fn approvals_load_workspace_replaces_existing_workspace_entries() {
    let mut rt = RuntimeApprovals::new();
    rt.add_workspace_tool("bash", vec![pat("git *")]);
    let mut tools: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
    tools.insert("bash".into(), vec![pat("find *")]);
    rt.load_workspace(tools, vec![PathBuf::from("/var")]);
    assert!(rt.has_pattern("bash", "find *"));
    assert!(!rt.has_pattern("bash", "git *"));
}

#[test]
fn permissions_handle_snapshots_policy_and_shares_session_approvals() {
    let policy = |decision| {
        permissions_from_mode(
            mode_perms(HashMap::from([("demo".to_string(), decision)]), &[]),
            false,
            PathBuf::new(),
        )
    };
    let handle = PermissionsHandle::new(policy(Decision::Ask));
    let active_turn = handle.snapshot();

    handle.replace(policy(Decision::Deny));
    handle
        .approvals()
        .write()
        .unwrap()
        .add_session_tool("demo", Vec::new());

    let args = HashMap::new();
    assert_eq!(
        active_turn
            .evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "demo", &args)
            .decision,
        Decision::Allow,
        "the active policy snapshot must observe newly granted session approval"
    );
    assert_eq!(
        handle
            .evaluate_tool_with_approvals(normal(), ToolOrigin::Lua, "demo", &args)
            .decision,
        Decision::Deny,
        "future evaluations must use the replacement static policy"
    );
}

#[test]
fn permission_resolution_reloads_workspace_grants_without_losing_session_grants() {
    let state = tempfile::tempdir().unwrap();
    let workspace_store = store::WorkspacePermissionStore::new(state.path().to_path_buf());
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    workspace_store.save(
        &first.path().to_string_lossy(),
        &[store::Rule {
            tool: "bash".into(),
            patterns: vec!["git *".into()],
        }],
    );
    let settings = crate::config::ResolvedSettings::default();
    let handle = PermissionsHandle::from_resolution(resolve_permissions(
        &RawPerms::default(),
        &ToolDefaults::default(),
        HashMap::new(),
        &settings,
        PermissionRuntimePaths {
            cwd: first.path(),
            home: first.path(),
        },
        &workspace_store,
        None,
    ));
    handle
        .approvals()
        .write()
        .unwrap()
        .add_session_tool("bash", vec![pat("cargo *")]);
    assert!(handle
        .approvals()
        .read()
        .unwrap()
        .has_pattern("bash", "git *"));

    handle.apply_resolution(resolve_permissions(
        &RawPerms::default(),
        &ToolDefaults::default(),
        HashMap::new(),
        &settings,
        PermissionRuntimePaths {
            cwd: second.path(),
            home: second.path(),
        },
        &workspace_store,
        None,
    ));
    let approvals = handle.approvals();
    let approvals = approvals.read().unwrap();
    assert!(!approvals.has_pattern("bash", "git *"));
    assert!(approvals.has_pattern("bash", "cargo *"));
}

#[test]
fn approvals_set_session_replaces_existing_session_entries() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("a *")]);
    let tools = vec![SessionToolApproval {
        tool: "bash".into(),
        pattern: Some("z *".into()),
    }];
    rt.set_session(tools, vec![PathBuf::from("/srv")], vec![]);
    assert!(rt.has_pattern("bash", "z *"));
    assert!(!rt.has_pattern("bash", "a *"));
}

#[test]
fn approvals_session_tool_approvals_returns_sorted_tools_and_patterns() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("read", vec![pat("**")]);
    rt.add_session_tool("bash", vec![pat("ls *"), pat("cat *")]);
    assert_eq!(
        rt.session_tool_approvals(),
        vec![
            SessionToolApproval {
                tool: "bash".into(),
                pattern: Some("ls *".into()),
            },
            SessionToolApproval {
                tool: "bash".into(),
                pattern: Some("cat *".into()),
            },
            SessionToolApproval {
                tool: "read".into(),
                pattern: Some("**".into()),
            },
        ]
    );
}

#[test]
fn approvals_has_pattern_false_for_unknown_tool_or_pattern() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("git *")]);
    assert!(!rt.has_pattern("python", "git *"));
    assert!(!rt.has_pattern("bash", "rm *"));
}

#[test]
fn approvals_dirs_approved_returns_true_for_empty_paths() {
    let rt = RuntimeApprovals::new();
    assert!(dirs_approved(&rt, &[]));
}

#[test]
fn approvals_dirs_approved_returns_false_when_no_dirs_registered() {
    let rt = RuntimeApprovals::new();
    assert!(!dirs_approved(&rt, &["/tmp/foo"]));
}

#[test]
fn approvals_dirs_approved_checks_parent_prefix_match() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/work"));
    assert!(dirs_approved(&rt, &["/work/sub/file.rs"]));
    assert!(!dirs_approved(&rt, &["/other/file.rs"]));
}

#[test]
fn approvals_session_path_grants_are_specific_to_mode_tool_access() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_path_grant(
        plan(),
        "edit_file",
        PathAccess::Write,
        PathBuf::from("/session/plans/20260101-demo"),
    );
    assert!(rt.session_path_grant_approved_for_path(
        &plan(),
        "edit_file",
        &PathAccess::Write,
        Path::new("/session/plans/20260101-demo/plan.md")
    ));
    assert!(!rt.session_path_grant_approved_for_path(
        &normal(),
        "edit_file",
        &PathAccess::Write,
        Path::new("/session/plans/20260101-demo/plan.md")
    ));
    assert!(!rt.session_path_grant_approved_for_path(
        &plan(),
        "write_file",
        &PathAccess::Write,
        Path::new("/session/plans/20260101-demo/plan.md")
    ));
    assert!(!rt.session_path_grant_approved_for_path(
        &plan(),
        "edit_file",
        &PathAccess::Read,
        Path::new("/session/plans/20260101-demo/plan.md")
    ));
    assert!(!rt.session_path_grant_approved_for_path(
        &plan(),
        "edit_file",
        &PathAccess::Write,
        Path::new("/session/plans/other/plan.md")
    ));
}

#[test]
fn approvals_is_approved_blanket_session_overrides_pattern_check() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", Vec::new());
    assert!(rt.is_approved("bash", "anything goes", None));
}

#[test]
fn approvals_is_approved_returns_false_when_no_entry_exists() {
    let rt = RuntimeApprovals::new();
    assert!(!rt.is_approved("bash", "ls", None));
}

#[test]
fn approvals_is_approved_requires_all_patterns_match() {
    let mut rt = RuntimeApprovals::new();
    rt.set_session(
        vec![SessionToolApproval {
            tool: "bash".into(),
            pattern: Some("ls *".into()),
        }],
        vec![],
        vec![],
    );
    // Only `ls` is approved; chained `rm` should fail.
    assert!(!rt.is_approved("bash", "ls && rm -rf /", None));
}

// ── rules.rs (backfill: merge_mode / build_mode / for_mode) ─────────

#[test]
fn merge_mode_combines_default_and_mode_tools_and_patterns() {
    use crate::permissions::rules::{merge_mode, RawModePerms, RawRuleSet};
    let default = RawModePerms {
        tools: RawRuleSet {
            allow: vec!["read".into()],
            ask: vec!["write".into()],
            deny: vec![],
        },
        patterns: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["ls *".into()],
                ask: vec![],
                deny: vec![],
            },
        )]),
        ..Default::default()
    };
    let mode = RawModePerms {
        tools: RawRuleSet {
            allow: vec!["exec".into()],
            ask: vec![],
            deny: vec!["delete".into()],
        },
        patterns: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["cat *".into()],
                ask: vec![],
                deny: vec!["rm *".into()],
            },
        )]),
        ..Default::default()
    };
    let merged = merge_mode(&default, &mode);
    assert!(merged.tools.allow.contains(&"read".to_string()));
    assert!(merged.tools.allow.contains(&"exec".to_string()));
    assert!(merged.tools.ask.contains(&"write".to_string()));
    assert!(merged.tools.deny.contains(&"delete".to_string()));
    let bash = &merged.patterns["bash"];
    assert!(bash.allow.contains(&"ls *".to_string()));
    assert!(bash.allow.contains(&"cat *".to_string()));
    assert!(bash.deny.contains(&"rm *".to_string()));
}

#[test]
fn merge_mode_handles_subcommand_present_only_in_default() {
    use crate::permissions::rules::{merge_mode, RawModePerms, RawRuleSet};
    let default = RawModePerms {
        tools: RawRuleSet::default(),
        patterns: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["ls *".into()],
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let mode = RawModePerms {
        tools: RawRuleSet::default(),
        patterns: HashMap::new(),
        ..Default::default()
    };
    let merged = merge_mode(&default, &mode);
    let bash = &merged.patterns["bash"];
    assert!(bash.allow.contains(&"ls *".to_string()));
}

#[test]
fn merge_mode_handles_subcommand_present_only_in_mode() {
    use crate::permissions::rules::{merge_mode, RawModePerms, RawRuleSet};
    let default = RawModePerms {
        tools: RawRuleSet::default(),
        patterns: HashMap::new(),
        ..Default::default()
    };
    let mode = RawModePerms {
        tools: RawRuleSet::default(),
        patterns: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["git *".into()],
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let merged = merge_mode(&default, &mode);
    let bash = &merged.patterns["bash"];
    assert!(bash.allow.contains(&"git *".to_string()));
}

#[test]
fn tool_perm_defaults_for_mode_returns_per_mode_decisions() {
    let defaults = ToolPermDefaults {
        modes: HashMap::from([
            ("normal".to_string(), Decision::Ask),
            ("plan".to_string(), Decision::Allow),
            ("apply".to_string(), Decision::Allow),
            ("yolo".to_string(), Decision::Allow),
        ]),
    };
    assert_eq!(defaults.for_mode(&normal()), Some(&Decision::Ask));
    assert_eq!(defaults.for_mode(&plan()), Some(&Decision::Allow));
    assert_eq!(defaults.for_mode(&apply()), Some(&Decision::Allow));
    assert_eq!(defaults.for_mode(&yolo()), Some(&Decision::Allow));
}

#[test]
fn tool_perm_defaults_for_mode_falls_back_to_none_when_unset() {
    let defaults = ToolPermDefaults::default();
    assert_eq!(defaults.for_mode(&normal()), None);
    assert_eq!(defaults.for_mode(&yolo()), None);
}

// ── store.rs (backfill: into_approvals) ─────────────────────────────

#[test]
fn store_into_approvals_separates_directory_rules_from_tools() {
    use crate::permissions::store::{into_approvals, Rule};
    let rules = vec![
        Rule {
            tool: "bash".into(),
            patterns: vec!["ls *".into(), "git *".into()],
        },
        Rule {
            tool: "directory".into(),
            patterns: vec!["/srv".into()],
        },
        Rule {
            tool: "directory".into(),
            patterns: vec!["/var".into()],
        },
    ];
    let (tools, dirs) = into_approvals(&rules);
    assert_eq!(tools["bash"].len(), 2);
    assert_eq!(dirs.len(), 2);
    assert!(dirs.contains(&PathBuf::from("/srv")));
    assert!(dirs.contains(&PathBuf::from("/var")));
}

#[test]
fn store_into_approvals_drops_wildcard_only_pattern_for_tools() {
    use crate::permissions::store::{into_approvals, Rule};
    let rules = vec![Rule {
        tool: "bash".into(),
        patterns: vec!["*".into(), "ls *".into()],
    }];
    let (tools, _) = into_approvals(&rules);
    assert_eq!(tools["bash"].len(), 1);
    assert_eq!(tools["bash"][0].as_str(), "ls *");
}

#[test]
fn store_into_approvals_skips_invalid_glob_patterns() {
    use crate::permissions::store::{into_approvals, Rule};
    let rules = vec![Rule {
        tool: "bash".into(),
        // [ without ] is an invalid glob.
        patterns: vec!["[unclosed".into(), "ls *".into()],
    }];
    let (tools, _) = into_approvals(&rules);
    assert_eq!(tools["bash"].len(), 1);
}

#[test]
fn store_into_approvals_handles_empty_rules() {
    use crate::permissions::store::{into_approvals, Rule};
    let rules: Vec<Rule> = vec![];
    let (tools, dirs) = into_approvals(&rules);
    assert!(tools.is_empty());
    assert!(dirs.is_empty());
}
