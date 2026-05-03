#![cfg(test)]
//! Integration tests spanning rules, bash parsing, approvals, and workspace.

use super::approvals::*;
use super::bash::*;
use super::rules::*;
use super::workspace::*;
use super::*;
use protocol::AgentMode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

fn perms_with_bash(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
    let mode = ModePerms {
        tools: HashMap::new(),
        bash: ruleset(allow, ask, deny),
        web_fetch: empty_ruleset(),
        mcp: empty_ruleset(),
    };
    Permissions {
        normal: mode.clone(),
        plan: mode.clone(),
        apply: mode.clone(),
        yolo: mode,
        restrict_to_workspace: false,
        workspace: PathBuf::new(),
    }
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
    assert_eq!(p.check_bash(mode, cmd), expected);
}

#[test]
fn yolo_allows_mcp_by_default() {
    let p = perms_with_bash(&[], &[], &[]);
    assert_eq!(
        p.check_mcp(AgentMode::Yolo, "filesystem_read_file"),
        Decision::Allow
    );
}

#[test]
fn normal_mode_asks_for_mcp_by_default() {
    let p = perms_with_bash(&[], &[], &[]);
    assert_eq!(
        p.check_mcp(AgentMode::Normal, "filesystem_read_file"),
        Decision::Ask
    );
}

// --- simple commands ---

#[test]
fn simple_allowed() {
    assert_bash(
        &["ls *"],
        &[],
        &[],
        AgentMode::Normal,
        "ls -la",
        Decision::Allow,
    );
}

#[test]
fn simple_denied() {
    assert_bash(
        &[],
        &[],
        &["rm *"],
        AgentMode::Normal,
        "rm -rf /",
        Decision::Deny,
    );
}

#[test]
fn simple_ask() {
    assert_bash(
        &[],
        &["rm *"],
        &[],
        AgentMode::Normal,
        "rm -rf /",
        Decision::Ask,
    );
}

// --- deny rules with chained commands ---

#[test]
fn deny_rm_simple() {
    assert_bash(
        &[],
        &[],
        &["rm *"],
        AgentMode::Normal,
        "rm -rf /",
        Decision::Deny,
    );
}

#[test]
fn deny_rm_after_ls() {
    assert_bash(
        &["ls *"],
        &[],
        &["rm *"],
        AgentMode::Normal,
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
        AgentMode::Normal,
        "rm -rf / && ls",
        Decision::Deny,
    );
}

// --- ask rules with chained commands ---

#[test]
fn ask_rm_simple() {
    assert_bash(
        &[],
        &["rm *"],
        &[],
        AgentMode::Normal,
        "rm -rf /",
        Decision::Ask,
    );
}

#[test]
fn ask_rm_after_ls() {
    assert_bash(
        &["ls *"],
        &["rm *"],
        &[],
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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
        AgentMode::Normal,
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

// --- edge cases ---

// Empty / whitespace-only commands
#[test]
fn empty_command() {
    assert_bash(&["ls *"], &[], &[], AgentMode::Normal, "", Decision::Ask);
}

#[test]
fn whitespace_only_command() {
    assert_bash(&["ls *"], &[], &[], AgentMode::Normal, "   ", Decision::Ask);
}

// --- quote-aware splitting (shlex) ---

// Operators inside quotes are NOT treated as operators
#[test]
fn operator_in_quoted_argument() {
    let p = perms_with_bash(&["grep *"], &[], &[]);
    // && inside quotes is not an operator — stays as single command
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"grep "&&" file.txt"#),
        Decision::Allow
    );
}

#[test]
fn semicolon_in_echo() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // shlex sees: ["echo", "hello; world"] — semicolon inside quotes
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"echo "hello; world""#),
        Decision::Allow
    );
}

#[test]
fn pipe_in_quoted_filename() {
    let p = perms_with_bash(&["cat *"], &[], &["rm *"]);
    // shlex sees: ["cat", "file|name"] — pipe inside quotes
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"cat "file|name""#),
        Decision::Allow
    );
}

// --- single & (background operator) now handled ---

#[test]
fn single_ampersand_background() {
    let p = perms_with_bash(&["sleep *"], &[], &["rm *"]);
    // shlex sees: ["sleep", "5", "&", "rm", "foo"]
    // splits to ["sleep 5", "rm foo"] — rm is denied
    assert_eq!(
        p.check_bash(AgentMode::Normal, "sleep 5 & rm foo"),
        Decision::Deny
    );
}

// --- subshell / substitution (still not caught) ---

#[test]
fn command_substitution() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // rm inside $() is now extracted and checked
    assert_eq!(
        p.check_bash(AgentMode::Normal, "echo $(rm -rf /)"),
        Decision::Deny
    );
}

#[test]
fn backtick_substitution() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // rm inside backticks is now extracted and checked
    assert_eq!(
        p.check_bash(AgentMode::Normal, "echo `rm -rf /`"),
        Decision::Deny
    );
}

// --- newline separator ---

#[test]
fn newline_separator() {
    let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
    // Newline is now treated as a command separator
    assert_eq!(
        p.check_bash(AgentMode::Normal, "ls\nrm -rf /"),
        Decision::Deny
    );
}

// --- trailing / leading operators ---

#[test]
fn trailing_operator() {
    assert_bash(
        &["ls *"],
        &[],
        &[],
        AgentMode::Normal,
        "ls &&",
        Decision::Allow,
    );
}

#[test]
fn split_trailing_operator() {
    assert_eq!(split_shell_commands("ls &&"), vec!["ls"]);
}

#[test]
fn leading_operator() {
    let p = perms_with_bash(&["rm *"], &[], &[]);
    // shlex sees: ["&&", "rm", "foo"] → splits to ["rm foo"]
    // single-command path uses original "&& rm foo" which won't match
    assert_eq!(p.check_bash(AgentMode::Normal, "&& rm foo"), Decision::Ask);
}

#[test]
fn split_leading_operator() {
    assert_eq!(split_shell_commands("&& rm foo"), vec!["rm foo"]);
}

// --- triple &&& ---

#[test]
fn triple_ampersand() {
    // "ls &&&rm foo" — && consumes first two, & consumes third → ["ls", "rm foo"]
    assert_eq!(split_shell_commands("ls &&&rm foo"), vec!["ls", "rm foo"]);
}

#[test]
fn triple_ampersand_spaced() {
    // "ls &&& rm foo" → shlex: ["ls", "&&", "&", "rm", "foo"]
    // splits on && and &: ["ls", "rm foo"]
    assert_eq!(split_shell_commands("ls &&& rm foo"), vec!["ls", "rm foo"]);
}

// --- bare commands ---

#[test]
fn bare_command_matches_star_pattern() {
    assert_bash(
        &["ls *"],
        &[],
        &[],
        AgentMode::Normal,
        "ls",
        Decision::Allow,
    );
}

#[test]
fn trailing_space_no_false_positive() {
    assert_bash(
        &["ls *"],
        &[],
        &[],
        AgentMode::Normal,
        "lsof",
        Decision::Ask,
    );
}

// --- unclosed quotes ---

#[test]
fn unclosed_quote() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // shlex returns None for unclosed quotes — treated as single command
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"echo "hello && rm foo"#),
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
        p.check_bash(AgentMode::Normal, r#"echo "it's fine" && rm foo"#),
        Decision::Deny
    );
}

#[test]
fn double_quotes_inside_single() {
    // echo '"hello"' && rm foo → two commands
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"echo '"hello"' && rm foo"#),
        Decision::Deny
    );
}

// --- escaped quote inside double quotes ---

#[test]
fn escaped_quote_inside_double_quotes() {
    // echo "he said \"hi\" && rm" is all one quoted string — single command
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"echo "he said \"hi\" && rm""#),
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
        AgentMode::Normal,
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

// --- single-command path inconsistency (pre-existing bug) ---

#[test]
fn leading_whitespace_single_command() {
    let p = perms_with_bash(&["ls *"], &[], &[]);
    // Input is trimmed before matching, so leading whitespace is fine
    assert_eq!(p.check_bash(AgentMode::Normal, "  ls -la"), Decision::Allow);
}

#[test]
fn leading_whitespace_chained_command() {
    let p = perms_with_bash(&["ls *", "echo *"], &[], &[]);
    // Multi-command path trims each part, so "ls -la" matches "ls *".
    assert_eq!(
        p.check_bash(AgentMode::Normal, "  ls -la && echo hi"),
        Decision::Allow
    );
}

// --- subshells / parentheses (known limitation) ---

#[test]
fn subshell_not_parsed() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // rm inside (...) subshell is now extracted and checked
    assert_eq!(
        p.check_bash(AgentMode::Normal, "echo hi && (rm -rf /)"),
        Decision::Deny
    );
}

#[test]
fn subshell_hides_denied_command() {
    let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
    // $() inside quotes: quotes prevent extraction in split_impl,
    // but extract_embedded_commands scans the full command including quotes.
    // The $() is found and rm is extracted → Deny.
    assert_eq!(
        p.check_bash(AgentMode::Normal, r#"echo "$(rm -rf /)""#),
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
    // Trailing backslash with nothing after — should not panic
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
    // "rm -rf /" is heredoc content, not a command — should not be denied
    assert_eq!(p.check_bash(AgentMode::Normal, cmd), Decision::Allow);
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
        AgentMode::Normal,
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
    let mode = ModePerms {
        tools: {
            let mut m = HashMap::new();
            m.insert("read_file".to_string(), Decision::Allow);
            m.insert("write_file".to_string(), Decision::Allow);
            m.insert("edit_file".to_string(), Decision::Allow);
            m.insert("glob".to_string(), Decision::Allow);
            m.insert("grep".to_string(), Decision::Allow);
            m.insert("bash".to_string(), Decision::Allow);
            m
        },
        bash: RuleSet {
            allow: vec![glob::Pattern::new("*").unwrap()],
            ask: vec![],
            deny: vec![],
        },
        web_fetch: empty_ruleset(),
        mcp: empty_ruleset(),
    };
    Permissions {
        normal: mode.clone(),
        plan: mode.clone(),
        apply: mode.clone(),
        yolo: mode,
        restrict_to_workspace: true,
        workspace: PathBuf::from(workspace),
    }
}

fn args_with(key: &str, val: &str) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(key.to_string(), Value::String(val.to_string()));
    m
}

// --- path extraction ---

#[test]
fn extract_paths_from_file_tools() {
    assert_eq!(
        extract_tool_paths("read_file", &args_with("file_path", "/etc/passwd")),
        vec!["/etc/passwd"]
    );
    assert_eq!(
        extract_tool_paths("write_file", &args_with("file_path", "relative.txt")),
        vec!["relative.txt"]
    );
    assert_eq!(
        extract_tool_paths("edit_file", &args_with("file_path", "")),
        Vec::<String>::new()
    );
}

#[test]
fn extract_paths_from_glob_grep() {
    assert_eq!(
        extract_tool_paths("glob", &args_with("path", "/tmp")),
        vec!["/tmp"]
    );
    assert_eq!(
        extract_tool_paths("grep", &args_with("path", "")),
        Vec::<String>::new()
    );
}

#[test]
fn extract_paths_from_bash() {
    assert_eq!(
        extract_tool_paths("bash", &args_with("command", "rm -rf /tmp/foo")),
        vec!["/tmp/foo"]
    );
    assert_eq!(
        extract_tool_paths("bash", &args_with("command", "ls relative/dir")),
        Vec::<String>::new()
    );
    assert_eq!(
        extract_tool_paths("bash", &args_with("command", "cat ~/secret.txt")),
        vec!["~/secret.txt"]
    );
}

#[test]
fn extract_paths_from_bash_strips_quotes() {
    assert_eq!(
        extract_tool_paths("bash", &args_with("command", "rm '/etc/passwd'")),
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
fn workspace_allows_file_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/home/user/project/src/main.rs");
    assert_eq!(
        p.decide(AgentMode::Normal, "read_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_downgrades_file_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/etc/passwd");
    assert_eq!(
        p.decide(AgentMode::Normal, "read_file", &args, false),
        Decision::Ask
    );
}

#[test]
fn workspace_allows_relative_path() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "src/main.rs");
    assert_eq!(
        p.decide(AgentMode::Normal, "write_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_downgrades_bash_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /tmp/foo");
    assert_eq!(
        p.decide(AgentMode::Normal, "bash", &args, false),
        Decision::Ask
    );
}

#[test]
fn workspace_allows_bash_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert_eq!(
        p.decide(AgentMode::Normal, "bash", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_allows_bash_relative() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cargo build");
    assert_eq!(
        p.decide(AgentMode::Normal, "bash", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_downgrades_yolo_outside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "rm -rf /etc");
    // Even in yolo, out-of-workspace should ask
    assert_eq!(
        p.decide(AgentMode::Yolo, "bash", &args, false),
        Decision::Ask
    );
}

#[test]
fn workspace_yolo_allows_inside() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("file_path", "/home/user/project/foo.txt");
    assert_eq!(
        p.decide(AgentMode::Yolo, "write_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_restriction_off_allows_everything() {
    let mut p = perms_with_workspace("/home/user/project");
    p.restrict_to_workspace = false;
    let args = args_with("file_path", "/etc/passwd");
    assert_eq!(
        p.decide(AgentMode::Normal, "read_file", &args, false),
        Decision::Allow
    );
}

#[test]
fn workspace_ask_stays_ask() {
    // If the tool is already Ask (not Allow), workspace restriction doesn't change it
    let mut p = perms_with_workspace("/home/user/project");
    // Remove write_file from allowed tools so it defaults to Ask
    p.normal.tools.remove("write_file");
    let args = args_with("file_path", "/home/user/project/foo.txt");
    // Even inside workspace, Ask stays Ask because the tool itself is Ask
    assert_eq!(
        p.decide(AgentMode::Normal, "write_file", &args, false),
        Decision::Ask
    );
}

#[test]
fn workspace_glob_outside_downgrades() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("path", "/tmp");
    assert_eq!(
        p.decide(AgentMode::Normal, "glob", &args, false),
        Decision::Ask
    );
}

#[test]
fn workspace_no_path_tools_unaffected() {
    let p = perms_with_workspace("/home/user/project");
    let args = HashMap::new();
    // web_search has no paths, should not be affected
    assert_eq!(
        p.decide(AgentMode::Yolo, "web_search", &args, false),
        Decision::Allow
    );
}

// --- yolo mode is configurable ---

#[test]
fn yolo_defaults_to_allow() {
    let p = Permissions::load();
    assert_eq!(p.check_tool(AgentMode::Yolo, "bash"), Decision::Allow);
    assert_eq!(p.check_tool(AgentMode::Yolo, "edit_file"), Decision::Allow);
    assert_eq!(p.check_tool(AgentMode::Yolo, "write_file"), Decision::Allow);
    assert_eq!(p.check_tool(AgentMode::Yolo, "read_file"), Decision::Allow);
}

#[test]
fn yolo_unknown_tool_defaults_allow() {
    let p = Permissions::load();
    assert_eq!(
        p.check_tool(AgentMode::Yolo, "some_unknown_tool"),
        Decision::Allow
    );
}

#[test]
fn yolo_bash_allows_everything_by_default() {
    let p = Permissions::load();
    assert_eq!(p.check_bash(AgentMode::Yolo, "rm -rf /"), Decision::Allow);
}

#[test]
fn normal_unknown_tool_defaults_ask() {
    let p = Permissions::load();
    assert_eq!(
        p.check_tool(AgentMode::Normal, "some_unknown_tool"),
        Decision::Ask
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
    // One redirect to /dev/null, but another to a real file — should escalate
    assert!(has_output_redirection("cmd 2>/dev/null > out.txt"));
}

#[test]
fn auto_allowed_with_dev_null_stays_allow() {
    assert_bash(
        &["find *"],
        &[],
        &[],
        AgentMode::Normal,
        "find /tmp 2>/dev/null",
        Decision::Allow,
    );
}

#[test]
fn auto_allowed_with_output_redirect_escalates_to_ask() {
    // cat * is in the default allowlist, but with > it should ask
    let p = perms_with_bash(&["cat *"], &[], &[]);
    assert_eq!(
        p.check_bash(AgentMode::Normal, "cat file.txt > output.txt"),
        Decision::Ask
    );
}

#[test]
fn auto_allowed_with_append_redirect_escalates_to_ask() {
    assert_bash(
        &["cat *"],
        &[],
        &[],
        AgentMode::Normal,
        "cat file.txt >> output.txt",
        Decision::Ask,
    );
}

#[test]
fn auto_allowed_heredoc_with_redirect_escalates_to_ask() {
    // cat << 'EOF' > file.txt matches cat * but has output redirection
    let p = perms_with_bash(&["cat *"], &[], &[]);
    let cmd = "cat << 'EOF' > long_file.txt\nhello\nworld\nEOF";
    assert_eq!(p.check_bash(AgentMode::Normal, cmd), Decision::Ask);
}

#[test]
fn auto_allowed_no_redirect_stays_allow() {
    // Without redirection, cat * should still be allowed
    let p = perms_with_bash(&["cat *"], &[], &[]);
    assert_eq!(
        p.check_bash(AgentMode::Normal, "cat file.txt"),
        Decision::Allow
    );
}

#[test]
fn chained_command_with_redirect_escalates() {
    let p = perms_with_bash(&["ls *", "cat *"], &[], &[]);
    // ls is allowed, cat with redirect should escalate
    assert_eq!(
        p.check_bash(AgentMode::Normal, "ls -la && cat file > out.txt"),
        Decision::Ask
    );
}

#[test]
fn pipe_with_output_redirect_escalates() {
    let p = perms_with_bash(&["cat *", "grep *"], &[], &[]);
    // pipe is allowed, but output redirect at end should escalate
    assert_eq!(
        p.check_bash(AgentMode::Normal, "cat file | grep foo > out.txt"),
        Decision::Ask
    );
}

#[test]
fn denied_command_with_redirect_stays_deny() {
    let p = perms_with_bash(&[], &[], &["rm *"]);
    // rm is denied regardless of redirection
    assert_eq!(
        p.check_bash(AgentMode::Normal, "rm file.txt > /dev/null"),
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

// --- bash decide_base: tool Allow + pattern Ask = Ask ---

#[test]
fn bash_tool_allow_pattern_ask() {
    let mode = ModePerms {
        tools: {
            let mut m = HashMap::new();
            m.insert("bash".to_string(), Decision::Allow);
            m
        },
        bash: ruleset(&[], &["git push *"], &[]),
        web_fetch: empty_ruleset(),
        mcp: empty_ruleset(),
    };
    let perms = Permissions {
        normal: mode.clone(),
        plan: mode.clone(),
        apply: mode.clone(),
        yolo: mode,
        restrict_to_workspace: false,
        workspace: PathBuf::new(),
    };
    let args = args_with("command", "git push origin main");
    assert_eq!(
        decide_base(&perms, AgentMode::Yolo, "bash", &args),
        Decision::Ask
    );
}

// --- override tightening: base Allow, override Ask → Ask ---

#[test]
fn override_tightens_allow_to_ask() {
    let mode = ModePerms {
        tools: {
            let mut m = HashMap::new();
            m.insert("bash".to_string(), Decision::Allow);
            m
        },
        bash: empty_ruleset(),
        web_fetch: empty_ruleset(),
        mcp: empty_ruleset(),
    };
    let perms = Permissions {
        normal: mode.clone(),
        plan: mode.clone(),
        apply: mode.clone(),
        yolo: mode,
        restrict_to_workspace: false,
        workspace: PathBuf::new(),
    };
    let overrides = protocol::PermissionOverrides {
        tools: Some(protocol::RuleSetOverride {
            allow: vec![],
            ask: vec!["bash".to_string()],
            deny: vec![],
        }),
        bash: None,
        web_fetch: None,
    };
    let tightened = perms.with_overrides(&overrides);
    assert_eq!(tightened.check_tool(AgentMode::Yolo, "bash"), Decision::Ask);
}

// --- cd command handling ---

#[test]
fn cd_alone_is_allowed() {
    assert_bash(&[], &[], &[], AgentMode::Normal, "cd /tmp", Decision::Allow);
}

#[test]
fn cd_no_args_is_allowed() {
    assert_bash(&[], &[], &[], AgentMode::Normal, "cd", Decision::Allow);
}

#[test]
fn cd_in_chain_does_not_escalate() {
    // cd should not contribute to the worst decision; only ls matters
    let p = perms_with_bash(&["ls *"], &[], &[]);
    assert_eq!(
        p.check_bash(AgentMode::Normal, "cd /tmp && ls -la"),
        Decision::Allow
    );
}

#[test]
fn cd_with_denied_command_still_denies() {
    assert_bash(
        &[],
        &[],
        &["rm *"],
        AgentMode::Normal,
        "cd /tmp && rm -rf foo",
        Decision::Deny,
    );
}

#[test]
fn cd_outside_workspace_downgrades_to_ask() {
    // cd itself is Allow, but the workspace path restriction catches /tmp
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /tmp && ls");
    assert_eq!(
        p.decide(AgentMode::Normal, "bash", &args, false),
        Decision::Ask
    );
}

#[test]
fn cd_inside_workspace_stays_allowed() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /home/user/project/src && ls");
    assert_eq!(
        p.decide(AgentMode::Normal, "bash", &args, false),
        Decision::Allow
    );
}

#[test]
fn cd_workspace_root_stays_allowed() {
    let p = perms_with_workspace("/home/user/project");
    let args = args_with("command", "cd /home/user/project && cargo build");
    assert_eq!(
        p.decide(AgentMode::Normal, "bash", &args, false),
        Decision::Allow
    );
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
    assert!(!rt.is_auto_approved(&p, AgentMode::Normal, "bash", &args, "rm -rf /tmp/foo"));
}

#[test]
fn runtime_tool_and_dir_approval_allow_outside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "rm -rf /tmp/foo");
    assert!(rt.is_auto_approved(&p, AgentMode::Normal, "bash", &args, "rm -rf /tmp/foo"));
}

#[test]
fn runtime_dir_approval_allows_default_allowed_command_outside_workspace() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "cat /tmp/foo");
    assert!(rt.is_auto_approved(&p, AgentMode::Normal, "bash", &args, "cat /tmp/foo"));
}

#[test]
fn runtime_tool_approval_allows_inside_workspace_request() {
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    let args = args_with("command", "rm -rf /home/user/project/target");
    assert!(rt.is_auto_approved(
        &p,
        AgentMode::Normal,
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
        AgentMode::Normal,
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

fn perms_with_workspace_default_bash(workspace: &str) -> Permissions {
    let mode = ModePerms {
        tools: {
            let mut m = HashMap::new();
            m.insert("read_file".to_string(), Decision::Allow);
            m.insert("write_file".to_string(), Decision::Allow);
            m.insert("edit_file".to_string(), Decision::Allow);
            m.insert("glob".to_string(), Decision::Allow);
            m.insert("grep".to_string(), Decision::Allow);
            m.insert("bash".to_string(), Decision::Allow);
            m
        },
        bash: RuleSet {
            allow: compile_patterns(
                &DEFAULT_BASH_ALLOW
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            ),
            ask: vec![],
            deny: vec![],
        },
        web_fetch: empty_ruleset(),
        mcp: empty_ruleset(),
    };
    Permissions {
        normal: mode.clone(),
        plan: mode.clone(),
        apply: mode.clone(),
        yolo: mode,
        restrict_to_workspace: true,
        workspace: PathBuf::from(workspace),
    }
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
    // read_file is Allow by default, downgraded to Ask by workspace restriction.
    // Dir approval should lift the restriction.
    assert!(rt.is_auto_approved(&p, AgentMode::Normal, "read_file", &args, &file));
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
        AgentMode::Normal,
        "bash",
        &args,
        "cat ~/syncthing/vault/notes.md",
    ));
}

#[test]
fn dir_approval_alone_insufficient_for_ask_command_outside_workspace() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_default_bash(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    // `rm` is not in DEFAULT_BASH_ALLOW → Ask. Dir approval alone shouldn't
    // auto-approve a command that requires its own permission.
    let args = args_with("command", "rm ~/syncthing/vault/old.md");
    assert!(!rt.is_auto_approved(
        &p,
        AgentMode::Normal,
        "bash",
        &args,
        "rm ~/syncthing/vault/old.md",
    ));
}

#[test]
fn dir_plus_tool_approval_for_ask_command_outside_workspace() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_default_bash(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("~/syncthing"));
    rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
    let args = args_with("command", "rm ~/syncthing/vault/old.md");
    assert!(rt.is_auto_approved(
        &p,
        AgentMode::Normal,
        "bash",
        &args,
        "rm ~/syncthing/vault/old.md",
    ));
}

#[test]
fn compound_command_default_allowed_with_dir_approval() {
    // `find | sort` — both are in DEFAULT_BASH_ALLOW.
    // With wildcard bash allow (perms_with_workspace), both are allowed
    // at the base level, so was_downgraded is true and dir approval suffices.
    let p = perms_with_workspace("/home/user/project");
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "find /tmp/data -type f | sort");
    assert!(rt.is_auto_approved(
        &p,
        AgentMode::Normal,
        "bash",
        &args,
        "find /tmp/data -type f | sort",
    ));
}

#[test]
fn compound_command_with_ask_subcommand_needs_tool_approval() {
    // `find | python3` — find is in DEFAULT_BASH_ALLOW, python3 is not.
    // Dir approval alone is insufficient; the Ask subcommand needs its own approval.
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_default_bash(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    let args = args_with("command", "find /tmp/data -name '*.py' | python3");
    assert!(!rt.is_auto_approved(
        &p,
        AgentMode::Normal,
        "bash",
        &args,
        "find /tmp/data -name '*.py' | python3",
    ));
}

#[test]
fn compound_command_with_ask_subcommand_and_tool_approval() {
    let home = engine::paths::home_dir();
    let workspace = format!("{}/dev/project", home.display());
    let p = perms_with_workspace_default_bash(&workspace);
    let mut rt = RuntimeApprovals::new();
    rt.add_session_dir(PathBuf::from("/tmp"));
    rt.add_session_tool("bash", vec![glob::Pattern::new("python3 *").unwrap()]);
    let args = args_with("command", "find /tmp/data -name '*.py' | python3");
    assert!(rt.is_auto_approved(
        &p,
        AgentMode::Normal,
        "bash",
        &args,
        "find /tmp/data -name '*.py' | python3",
    ));
}
