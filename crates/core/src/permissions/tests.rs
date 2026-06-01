#![cfg(test)]

use super::approvals::*;
use super::bash::*;
use super::rules::*;
use super::workspace::*;
use super::*;
use protocol::AgentMode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    let mut subcommands = HashMap::new();
    for (name, rs) in buckets {
        subcommands.insert((*name).to_string(), rs.clone());
    }
    ModePerms { tools, subcommands }
}

/// Mirrors the `subpattern_parser = "shell"` wiring from `tools/bash.lua`.
fn bash_parser_map() -> HashMap<String, std::sync::Arc<crate::permissions::SubpatternParserFn>> {
    let mut m = HashMap::new();
    if let Some(p) = crate::permissions::builtin_subpattern_parser("shell") {
        m.insert("bash".to_string(), p);
    }
    m
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
        workspace,
        paths_fn: None,
        decide_hook_fn: None,
        subpattern_parsers: bash_parser_map(),
        approvals: std::sync::Arc::new(std::sync::RwLock::new(RuntimeApprovals::new())),
    }
}

fn perms_with_bash(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
    let mode = mode_perms(HashMap::new(), &[("bash", ruleset(allow, ask, deny))]);
    permissions_from_mode(mode, false, PathBuf::new())
}

/// Installs a stub `decide_hook_fn` mirroring production Lua hooks for `bash` and `web_fetch`.
/// The captured clone has its hook cleared to prevent recursion.
fn install_stub_decide_hook(perms: &mut crate::permissions::Permissions) {
    let mut perms_for_hook = perms.clone();
    perms_for_hook.set_decide_hook_fn(std::sync::Arc::new(|_, _, _| None));
    perms.set_decide_hook_fn(std::sync::Arc::new(move |name, args, mode| match name {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let tool = perms_for_hook.check_tool(mode.clone(), "bash");
            if tool == protocol::Decision::Deny {
                return Some(protocol::Decision::Deny);
            }
            let sub = perms_for_hook.check_subcommand(mode, "bash", cmd);
            if sub == protocol::Decision::Deny {
                return Some(protocol::Decision::Deny);
            }
            if tool == protocol::Decision::Allow && sub == protocol::Decision::Ask {
                return Some(protocol::Decision::Ask);
            }
            Some(sub)
        }
        "web_fetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let tool = perms_for_hook.check_tool(mode.clone(), "web_fetch");
            if tool == protocol::Decision::Deny {
                return Some(protocol::Decision::Deny);
            }
            let pat = perms_for_hook.check_subcommand(mode, "web_fetch", url);
            if pat == protocol::Decision::Deny {
                return Some(protocol::Decision::Deny);
            }
            if pat == protocol::Decision::Allow {
                return Some(protocol::Decision::Allow);
            }
            if tool == protocol::Decision::Allow && pat == protocol::Decision::Ask {
                return Some(protocol::Decision::Ask);
            }
            Some(pat)
        }
        _ => None,
    }));
}

/// Stub `paths_fn` mirroring production `paths_for_workspace` callbacks
/// (file tools → `file_path`, glob/grep → `path`, bash → shell extraction).
fn stub_paths_fn() -> std::sync::Arc<crate::permissions::PathsFn> {
    std::sync::Arc::new(|name, args| match name {
        "read_file" | "write_file" | "edit_file" => {
            let p = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            if p.is_empty() {
                vec![]
            } else {
                vec![p.to_string()]
            }
        }
        "glob" | "grep" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if p.is_empty() {
                vec![]
            } else {
                vec![p.to_string()]
            }
        }
        "bash" => crate::permissions::workspace::extract_paths_from_command(
            args.get("command").and_then(|v| v.as_str()).unwrap_or(""),
        ),
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
    install_stub_decide_hook(&mut p);
    p
}

fn args_with(key: &str, val: &str) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(key.to_string(), Value::String(val.to_string()));
    m
}

// --- bash shell-string path extraction ---
// Only `extract_paths_from_command` lives in Rust; tool→path mapping is in Lua callbacks.

#[test]
fn shell_extracts_absolute_paths() {
    assert_eq!(
        extract_paths_from_command("rm -rf /tmp/foo"),
        vec!["/tmp/foo"]
    );
    assert_eq!(
        extract_paths_from_command("ls relative/dir"),
        Vec::<String>::new()
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
            assert_eq!(path.path, PathBuf::from("/home/user/project/src/main.rs"));
            assert_eq!(path.access, PathAccess::Write);
        }
        other => panic!("expected filesystem effect, got {other:?}"),
    }
}

#[test]
fn decide_request_uses_typed_effects_for_workspace_downgrade() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/etc/passwd");
    let effects = p.effects_for_tool(ToolOrigin::Lua, "read_file", &args);
    assert_eq!(
        p.decide_request(PermissionRequest {
            mode: normal(),
            tool_name: "read_file",
            args: &args,
            origin: ToolOrigin::Lua,
            effects,
        }),
        Decision::Ask
    );
}

#[test]
fn mcp_request_uses_mcp_origin_without_boolean_branch() {
    let p = perms_with_bash(&[], &[], &[]);
    let args = HashMap::new();
    let effects = p.effects_for_tool(ToolOrigin::Mcp, "filesystem_read_file", &args);
    assert_eq!(
        p.decide_request(PermissionRequest {
            mode: yolo(),
            tool_name: "filesystem_read_file",
            args: &args,
            origin: ToolOrigin::Mcp,
            effects,
        }),
        Decision::Allow
    );
}

#[test]
fn workspace_allows_file_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/home/user/project/src/main.rs");
    assert_eq!(
        p.decide(normal(), "read_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_downgrades_file_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/etc/passwd");
    assert_eq!(p.decide(normal(), "read_file", &args, false), Decision::Ask);
}

#[test]
fn workspace_allows_relative_path() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "src/main.rs");
    assert_eq!(
        p.decide(normal(), "write_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_downgrades_bash_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /tmp/foo");
    assert_eq!(p.decide(normal(), "bash", &args, false), Decision::Ask);
}

#[test]
fn workspace_allows_bash_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert_eq!(p.decide(normal(), "bash", &args, false), Decision::Allow);
}

#[test]
fn workspace_allows_bash_relative() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cargo build");
    assert_eq!(p.decide(normal(), "bash", &args, false), Decision::Allow);
}

#[test]
fn workspace_downgrades_yolo_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /etc");
    assert_eq!(p.decide(yolo(), "bash", &args, false), Decision::Ask);
}

#[test]
fn workspace_yolo_allows_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/home/user/project/foo.txt");
    assert_eq!(
        p.decide(yolo(), "write_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_restriction_off_allows_everything() {
    let mut p = perms_with_workspace("/home/user/project");
    p.restrict_to_workspace = false;
    let args = args_with("file_path", "/etc/passwd");
    assert_eq!(
        p.decide(normal(), "read_file", &args, false),
        Decision::Allow
    );
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
    assert_eq!(
        p.decide(normal(), "write_file", &args, false),
        Decision::Ask
    );
}

#[test]
fn workspace_glob_outside_downgrades() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("path", "/tmp");
    assert_eq!(p.decide(normal(), "glob", &args, false), Decision::Ask);
}

#[test]
fn workspace_no_path_tools_unaffected() {
    let p = perms_with_workspace("/home/user/project");
    let args = HashMap::new();
    assert_eq!(
        p.decide(yolo(), "web_search", &args, false),
        Decision::Allow
    );
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
    assert!(has_output_redirection("cat << 'EOF' > file.txt"));
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
    let mut perms = permissions_from_mode(mode, false, PathBuf::new());
    install_stub_decide_hook(&mut perms);
    let args = args_with("command", "git push origin main");
    assert_eq!(decide_base(&perms, yolo(), "bash", &args), Decision::Ask);
}

// --- override tightening ---

#[test]
fn override_tightens_allow_to_ask() {
    let mut tools = HashMap::new();
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", empty_ruleset())]);
    let mut perms = permissions_from_mode(mode, false, PathBuf::new());
    install_stub_decide_hook(&mut perms);
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
    assert_eq!(p.decide(normal(), "bash", &args, false), Decision::Ask);
}

#[test]
fn cd_inside_workspace_stays_allowed() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /home/user/project/src && ls");
    assert_eq!(p.decide(normal(), "bash", &args, false), Decision::Allow);
}

#[test]
fn cd_workspace_root_stays_allowed() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /home/user/project && cargo build");
    assert_eq!(p.decide(normal(), "bash", &args, false), Decision::Allow);
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
    assert!(!rt.is_auto_approved(&p, normal(), "bash", &args, "rm -rf /tmp/foo"));
}

#[test]
fn runtime_tool_and_dir_approval_allow_outside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "rm -rf /tmp/foo");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args, "rm -rf /tmp/foo"));
}

#[test]
fn runtime_dir_approval_allows_default_allowed_command_outside_workspace() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "cat /tmp/foo");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args, "cat /tmp/foo"));
}

#[test]
fn runtime_tool_approval_allows_inside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert!(rt.is_auto_approved(
        &p,
        normal(),
        "bash",
        &args,
        "rm -rf /home/user/project/target",
    ));
}

#[test]
fn runtime_dir_approval_does_not_affect_inside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert!(!rt.is_auto_approved(
        &p,
        normal(),
        "bash",
        &args,
        "rm -rf /home/user/project/target",
    ));
}

// --- tilde path normalization in dirs_approved ---

#[test]
fn dirs_approved_tilde_stored_absolute_queried() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    let home = engine::paths::home_dir();
    let abs = format!("{}/syncthing/vault/file.txt", home.display());
    assert!(rt.dirs_approved(&[abs]));
}

#[test]
fn dirs_approved_absolute_stored_tilde_queried() {
    let home = engine::paths::home_dir();
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(home.join("syncthing"));
    assert!(rt.dirs_approved(&["~/syncthing/vault/file.txt".to_string()]));
}

#[test]
fn dirs_approved_both_tilde() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    assert!(rt.dirs_approved(&["~/syncthing/vault".to_string()]));
}

#[test]
fn dirs_approved_both_absolute() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp/data"));
    assert!(rt.dirs_approved(&["/tmp/data/subdir/file.txt".to_string()]));
}

#[test]
fn dirs_approved_no_false_prefix_match() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/sync"));
    // ~/syncthing should NOT match ~/sync (different directory)
    assert!(!rt.dirs_approved(&["~/syncthing/file.txt".to_string()]));
}

#[test]
fn dirs_approved_exact_dir_match() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing/vault"));
    // File directly in the approved dir
    assert!(rt.dirs_approved(&["~/syncthing/vault/file.txt".to_string()]));
}

#[test]
fn dirs_approved_parent_not_covered() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing/vault"));
    // Parent dir is NOT covered
    assert!(!rt.dirs_approved(&["~/syncthing/other/file.txt".to_string()]));
}

#[test]
fn dirs_approved_path_is_dir_itself() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    // Path that IS the approved dir (e.g. a directory argument)
    assert!(rt.dirs_approved(&["/tmp".to_string()]));
}

#[test]
fn dirs_approved_multiple_paths_all_covered() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    rt.add_session_dir(PathBuf::from("/tmp"));
    assert!(rt.dirs_approved(&[
        "~/syncthing/vault/a.txt".to_string(),
        "/tmp/b.txt".to_string(),
    ]));
}

#[test]
fn dirs_approved_multiple_paths_one_uncovered() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    assert!(!rt.dirs_approved(&[
        "~/syncthing/vault/a.txt".to_string(),
        "/tmp/b.txt".to_string(),
    ]));
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
    tools.insert("bash".to_string(), Decision::Allow);
    let mode = mode_perms(tools, &[("bash", ruleset(bash_allow, &[], &[]))]);
    let mut p = permissions_from_mode(mode, true, PathBuf::from(workspace));
    p.set_paths_fn(stub_paths_fn());
    install_stub_decide_hook(&mut p);
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
    assert!(rt.is_auto_approved(&p, normal(), "read_file", &args, &file));
}

#[test]
fn absolute_dir_approval_works_for_tilde_bash() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(home.join("syncthing"));
    let args = args_with("command", "cat ~/syncthing/vault/notes.md");
    assert!(rt.is_auto_approved(
        &p,
        normal(),
        "bash",
        &args,
        "cat ~/syncthing/vault/notes.md",
    ));
}

#[test]
fn dir_approval_alone_insufficient_for_ask_command_outside_workspace() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_bash_allow(&workspace, &[]);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    let args = args_with("command", "rm ~/syncthing/vault/old.md");
    assert!(!rt.is_auto_approved(&p, normal(), "bash", &args, "rm ~/syncthing/vault/old.md",));
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
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args, "rm ~/syncthing/vault/old.md",));
}

#[test]
fn compound_command_default_allowed_with_dir_approval() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "find /tmp/data -type f | sort");
    assert!(rt.is_auto_approved(&p, normal(), "bash", &args, "find /tmp/data -type f | sort",));
}

#[test]
fn compound_command_with_ask_subcommand_needs_tool_approval() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_bash_allow(&workspace, &["find *"]);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "find /tmp/data -name '*.py' | python3");
    assert!(!rt.is_auto_approved(
        &p,
        normal(),
        "bash",
        &args,
        "find /tmp/data -name '*.py' | python3",
    ));
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
    assert!(rt.is_auto_approved(
        &p,
        normal(),
        "bash",
        &args,
        "find /tmp/data -name '*.py' | python3",
    ));
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
fn approvals_add_session_tool_empty_patterns_clears_existing_patterns() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("ls *")]);
    rt.add_session_tool("bash", Vec::new());
    // Blanket now applies; existing patterns dropped.
    assert!(rt.is_approved("bash", "anything", None));
}

#[test]
fn approvals_add_session_tool_existing_blanket_beats_incoming_patterns() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", Vec::new());
    rt.add_session_tool("bash", vec![pat("ls *")]);
    // Stays blanket - narrowing requires explicit clear.
    assert!(rt.is_approved("bash", "anything goes", None));
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
    assert!(rt.dirs_approved(&[path_in]));
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
    assert!(rt.dirs_approved(&["/work/x".into()]));
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
fn approvals_set_session_replaces_existing_session_entries() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![pat("a *")]);
    let mut tools: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
    tools.insert("bash".into(), vec![pat("z *")]);
    rt.set_session(tools, vec![PathBuf::from("/srv")]);
    assert!(rt.has_pattern("bash", "z *"));
    assert!(!rt.has_pattern("bash", "a *"));
}

#[test]
fn approvals_session_tool_entries_returns_sorted_tools_and_patterns() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("read", vec![pat("**")]);
    rt.add_session_tool("bash", vec![pat("ls *"), pat("cat *")]);
    let entries = rt.session_tool_entries();
    let tools: Vec<&str> = entries.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(tools, vec!["bash", "read"]);
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
    assert!(rt.dirs_approved(&[]));
}

#[test]
fn approvals_dirs_approved_returns_false_when_no_dirs_registered() {
    let rt = RuntimeApprovals::new();
    assert!(!rt.dirs_approved(&["/tmp/foo".into()]));
}

#[test]
fn approvals_dirs_approved_checks_parent_prefix_match() {
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/work"));
    assert!(rt.dirs_approved(&["/work/sub/file.rs".into()]));
    assert!(!rt.dirs_approved(&["/other/file.rs".into()]));
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
fn approvals_is_approved_requires_all_subcommands_match() {
    let mut rt = RuntimeApprovals::new();
    // Seed via set_session so the patterns actually persist.
    let mut seed: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
    seed.insert("bash".into(), vec![pat("ls *")]);
    rt.set_session(seed, vec![]);
    // Only `ls` is approved; chained `rm` should fail.
    assert!(!rt.is_approved("bash", "ls && rm -rf /", None));
}

// ── rules.rs (backfill: merge_mode / build_mode / for_mode) ─────────

#[test]
fn merge_mode_combines_default_and_mode_tools_and_subcommands() {
    use crate::permissions::rules::{merge_mode, RawModePerms, RawRuleSet};
    let default = RawModePerms {
        tools: RawRuleSet {
            allow: vec!["read".into()],
            ask: vec!["write".into()],
            deny: vec![],
        },
        subcommands: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["ls *".into()],
                ask: vec![],
                deny: vec![],
            },
        )]),
    };
    let mode = RawModePerms {
        tools: RawRuleSet {
            allow: vec!["exec".into()],
            ask: vec![],
            deny: vec!["delete".into()],
        },
        subcommands: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["cat *".into()],
                ask: vec![],
                deny: vec!["rm *".into()],
            },
        )]),
    };
    let merged = merge_mode(&default, &mode);
    assert!(merged.tools.allow.contains(&"read".to_string()));
    assert!(merged.tools.allow.contains(&"exec".to_string()));
    assert!(merged.tools.ask.contains(&"write".to_string()));
    assert!(merged.tools.deny.contains(&"delete".to_string()));
    let bash = &merged.subcommands["bash"];
    assert!(bash.allow.contains(&"ls *".to_string()));
    assert!(bash.allow.contains(&"cat *".to_string()));
    assert!(bash.deny.contains(&"rm *".to_string()));
}

#[test]
fn merge_mode_handles_subcommand_present_only_in_default() {
    use crate::permissions::rules::{merge_mode, RawModePerms, RawRuleSet};
    let default = RawModePerms {
        tools: RawRuleSet::default(),
        subcommands: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["ls *".into()],
                ..Default::default()
            },
        )]),
    };
    let mode = RawModePerms {
        tools: RawRuleSet::default(),
        subcommands: HashMap::new(),
    };
    let merged = merge_mode(&default, &mode);
    let bash = &merged.subcommands["bash"];
    assert!(bash.allow.contains(&"ls *".to_string()));
}

#[test]
fn merge_mode_handles_subcommand_present_only_in_mode() {
    use crate::permissions::rules::{merge_mode, RawModePerms, RawRuleSet};
    let default = RawModePerms {
        tools: RawRuleSet::default(),
        subcommands: HashMap::new(),
    };
    let mode = RawModePerms {
        tools: RawRuleSet::default(),
        subcommands: HashMap::from([(
            "bash".into(),
            RawRuleSet {
                allow: vec!["git *".into()],
                ..Default::default()
            },
        )]),
    };
    let merged = merge_mode(&default, &mode);
    let bash = &merged.subcommands["bash"];
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
