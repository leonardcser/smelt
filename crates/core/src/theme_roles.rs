//! Smelt-host role → highlight-group mappings. The role vocabulary is
//! host-specific, so this lives in `smelt-core` rather than `smelt-style`.

use smelt_style::theme::{intern, HlGroup};

/// Resolve a role label to a theme group id. Renderers flow the id through
/// extmarks so theme switches update `Theme.styles[id]` without re-rendering.
/// Unknown labels intern as-is.
pub fn role_hl(role: &str) -> HlGroup {
    let name = match role {
        "Muted" => "Comment",
        "ErrorMsg" => "ErrorMsg",
        "Accent" => "SmeltAccent",
        "Slug" => "SmeltSlug",
        "UserBg" => "SmeltUserBg",
        "CodeBlockBg" => "SmeltCodeBlockBg",
        "Bar" => "SmeltBar",
        "ToolPending" => "SmeltToolPending",
        "ReasonOff" => "SmeltReasonOff",
        "Success" => "SmeltSuccess",
        "Apply" => "SmeltModeApply",
        "Plan" => "SmeltModePlan",
        "Exec" => "SmeltModeExec",
        "Heading" => "SmeltHeading",
        "ReasonLow" => "SmeltReasonLow",
        "ReasonMed" => "SmeltReasonMed",
        "ReasonHigh" => "SmeltReasonHigh",
        "ReasonMax" => "SmeltReasonMax",
        other => other,
    };
    intern(name)
}
