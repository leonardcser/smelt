#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use smelt_core::permissions::rules::{
    ModeBehavior, RawModePerms, RawPerms, RawRuleSet, ToolDefaults, ToolEffectKind, ToolPermDefaults,
};
use smelt_core::permissions::{
    builtin_subpattern_parser, split_shell_commands, split_shell_commands_with_ops, Permissions,
    ToolOrigin,
};
use std::collections::HashMap;

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

fuzz_target!(|input: Input| {
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
        ToolPermDefaults { modes: HashMap::from([("normal".into(), smelt_core::permissions::Decision::Allow)]) },
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

    let mode = protocol::AgentMode::parse(if input.read_only { "plan" } else { "normal" }).unwrap();
    let _tool_decision = perms.check_tool(mode.clone(), &input.tool);
    let _sub_decision = perms.check_subcommand(mode.clone(), &input.bucket, &input.value);
    let _bash_decision = perms.check_subcommand(mode.clone(), "bash", &input.command);
    let _split = split_shell_commands(&input.command);
    let _split_with_ops = split_shell_commands_with_ops(&input.command);

    let args = HashMap::from([
        ("command".to_string(), serde_json::Value::String(input.command)),
        ("url".to_string(), serde_json::Value::String(input.url)),
        ("path".to_string(), serde_json::Value::String(input.path)),
    ]);
    let outcome = perms.evaluate_tool(mode, origin(input.origin), &input.tool, &args);
    if outcome.downgraded_by_workspace {
        assert_eq!(outcome.decision, smelt_core::permissions::Decision::Ask);
        assert!(!outcome.outside_workspace_paths.is_empty());
    }
});
