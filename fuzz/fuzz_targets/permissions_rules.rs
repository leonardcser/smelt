#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use smelt_core::permissions::rules::{
    ModeBehavior, RawModePerms, RawPerms, RawRuleSet, ToolDefaults, ToolEffectKind, ToolPermDefaults,
};
use smelt_core::permissions::{
    builtin_subpattern_parser, split_shell_commands, split_shell_commands_with_ops, Decision,
    PermissionRequirement, Permissions, ToolOrigin,
};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
struct Input {
    default: ModeSpec,
    normal: ModeSpec,
    plan: ModeSpec,
    read_only: bool,
    allow_subcommands_by_default: bool,
    ask_on_output_redirection: bool,
    tool: String,
    bucket: String,
    value: String,
    command: String,
    url: String,
    path: String,
    origin: u8,
}

#[derive(Debug)]
struct ModeSpec {
    tools: RuleSpec,
    bash: RuleSpec,
    web_fetch: RuleSpec,
    extra_bucket: String,
    extra: RuleSpec,
}

#[derive(Debug)]
struct RuleSpec {
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
}

impl<'a> Arbitrary<'a> for RuleSpec {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            allow: patterns(u)?,
            ask: patterns(u)?,
            deny: patterns(u)?,
        })
    }
}

impl<'a> Arbitrary<'a> for ModeSpec {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            tools: u.arbitrary()?,
            bash: u.arbitrary()?,
            web_fetch: u.arbitrary()?,
            extra_bucket: short_string(u, 24)?,
            extra: u.arbitrary()?,
        })
    }
}

impl<'a> Arbitrary<'a> for Input {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            default: u.arbitrary()?,
            normal: u.arbitrary()?,
            plan: u.arbitrary()?,
            read_only: u.arbitrary()?,
            allow_subcommands_by_default: u.arbitrary()?,
            ask_on_output_redirection: u.arbitrary()?,
            tool: choose_or_string(u, &["bash", "web_fetch", "edit", "read", "danger"] )?,
            bucket: choose_or_string(u, &["bash", "web_fetch", "mcp", "edit"] )?,
            value: short_string(u, 160)?,
            command: short_string(u, 256)?,
            url: short_string(u, 160)?,
            path: short_string(u, 96)?,
            origin: u.arbitrary()?,
        })
    }
}

fn patterns(u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<String>> {
    let n = u.int_in_range(0u8..=5)?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push(choose_or_string(
            u,
            &["*", "bash", "web_fetch", "git status*", "ls*", "rm*", "https://*", "*.rs"],
        )?);
    }
    Ok(out)
}

fn short_string(u: &mut Unstructured<'_>, max: usize) -> arbitrary::Result<String> {
    let len = u.int_in_range(0..=max)?;
    let bytes: Vec<u8> = (0..len)
        .map(|_| u.arbitrary::<u8>())
        .collect::<Result<_, _>>()?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn choose_or_string(u: &mut Unstructured<'_>, choices: &[&str]) -> arbitrary::Result<String> {
    if u.arbitrary::<bool>()? {
        let idx = u.int_in_range(0..=choices.len() - 1)?;
        Ok(choices[idx].to_string())
    } else {
        short_string(u, 48)
    }
}

fn raw_rules(spec: RuleSpec) -> RawRuleSet {
    RawRuleSet {
        allow: spec.allow,
        ask: spec.ask,
        deny: spec.deny,
    }
}

fn raw_mode(spec: ModeSpec) -> RawModePerms {
    let mut subcommands = HashMap::new();
    subcommands.insert("bash".to_string(), raw_rules(spec.bash));
    subcommands.insert("web_fetch".to_string(), raw_rules(spec.web_fetch));
    if !spec.extra_bucket.is_empty() {
        subcommands.insert(spec.extra_bucket, raw_rules(spec.extra));
    }
    RawModePerms {
        tools: raw_rules(spec.tools),
        subcommands,
    }
}

fn origin(raw: u8) -> ToolOrigin {
    match raw % 3 {
        0 => ToolOrigin::Lua,
        1 => ToolOrigin::Core,
        _ => ToolOrigin::Mcp,
    }
}

fn path_resolver() -> Arc<smelt_core::permissions::PathsFn> {
    Arc::new(|name, args| match name {
        "read" | "edit" => args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty())
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    })
}

fn approval_candidates(input: &Input) -> Vec<String> {
    let mut out = vec!["*".to_string()];
    for candidate in [&input.value, &input.command, &input.url, &input.path] {
        if !candidate.is_empty() && !out.iter().any(|existing| existing == candidate) {
            out.push(candidate.clone());
        }
    }
    out
}

fn assert_outcome_shape(outcome: &smelt_core::permissions::PermissionOutcome) {
    match outcome.decision {
        Decision::Allow | Decision::Deny | Decision::Error(_) => {
            assert!(outcome.missing_requirements.is_empty())
        }
        Decision::Ask => assert!(!outcome.missing_requirements.is_empty()),
    }
    if outcome
        .missing_requirements
        .iter()
        .any(|req| matches!(req, PermissionRequirement::PathPrefix { .. }))
    {
        assert_eq!(outcome.decision, Decision::Ask);
    }
}

fuzz_target!(|input: Input| {
    let candidates = approval_candidates(&input);
    let raw = RawPerms {
        default: raw_mode(input.default),
        modes: HashMap::from([
            ("normal".to_string(), raw_mode(input.normal)),
            ("plan".to_string(), raw_mode(input.plan)),
        ]),
    };

    let mut defaults = ToolDefaults::default();
    defaults.subcommand_allow.insert("bash".into(), vec!["git status*".into(), "ls*".into()]);
    defaults.subcommand_allow.insert("web_fetch".into(), vec!["https://*".into()]);
    defaults.subpattern_parsers.insert("bash".into(), builtin_subpattern_parser("shell").unwrap());
    defaults.tool_effects.insert("read".into(), ToolEffectKind::PathRead);
    defaults.tool_effects.insert("edit".into(), ToolEffectKind::PathWrite);
    defaults.tool_decisions.insert(
        "read".into(),
        ToolPermDefaults {
            modes: HashMap::from([("normal".into(), Decision::Allow)]),
        },
    );

    let behaviors = HashMap::from([
        (
            "normal".to_string(),
            ModeBehavior {
                read_only: input.read_only,
                allow_subcommands_by_default: input.allow_subcommands_by_default,
                ask_on_output_redirection: input.ask_on_output_redirection,
                ..ModeBehavior::default()
            },
        ),
        (
            "plan".to_string(),
            ModeBehavior {
                read_only: true,
                ..ModeBehavior::default()
            },
        ),
    ]);

    let mut perms = Permissions::from_raw_with_mode_behaviors(&raw, &defaults, behaviors);
    perms.set_workspace(std::env::current_dir().unwrap_or_default());
    perms.set_paths_fn(path_resolver());

    let mode = protocol::AgentMode::parse(if input.read_only { "plan" } else { "normal" }).unwrap();
    let request_origin = origin(input.origin);
    let _tool_decision = perms.check_tool(mode.clone(), &input.tool);
    let _sub_decision = perms.check_subcommand(mode.clone(), &input.bucket, &input.value);
    let _bash_decision = perms.check_subcommand(mode.clone(), "bash", &input.command);
    let _split = split_shell_commands(&input.command);
    let _split_with_ops = split_shell_commands_with_ops(&input.command);

    let args = HashMap::from([
        (
            "command".to_string(),
            serde_json::Value::String(input.command.clone()),
        ),
        ("url".to_string(), serde_json::Value::String(input.url.clone())),
        (
            "path".to_string(),
            serde_json::Value::String(input.path.clone()),
        ),
    ]);
    let outcome = perms.evaluate_tool(mode.clone(), request_origin.clone(), &input.tool, &args);
    assert_outcome_shape(&outcome);

    let approval_tool = if request_origin == ToolOrigin::Mcp {
        "mcp"
    } else {
        input.tool.as_str()
    };
    let options = perms.approval_options(approval_tool, &candidates, &outcome);
    for grant_set in &options.grant_sets {
        assert!(!grant_set.is_empty());
        assert!(outcome
            .missing_requirements
            .iter()
            .all(|req| grant_set.iter().any(|grant| grant.satisfies(req))));
    }

    let has_empty_command_requirement = outcome.missing_requirements.iter().any(|req| {
        matches!(req, PermissionRequirement::Command { command, .. } if command.is_empty())
    });
    if outcome.decision == Decision::Ask && !has_empty_command_requirement {
        if let Some(grant_set) = options.grant_sets.first() {
            {
                let mut approvals = perms.approvals.write().unwrap();
                for grant in grant_set.iter().cloned() {
                    approvals.add_session_grant(grant);
                }
            }
            let approved =
                perms.evaluate_tool_with_approvals(mode, request_origin, &input.tool, &args);
            assert_eq!(approved.decision, Decision::Allow);
            assert!(approved.missing_requirements.is_empty());
        }
    }
});
