//! Smelt-host-specific role → highlight-group mappings.
//!
//! `role_hl("Accent")` resolves the abstract role label content code
//! uses (markdown highlighters, transcript parsers, prompt chrome) to
//! the canonical theme group name (`SmeltAccent`) the user's theme
//! configures. Lives in `smelt-core` rather than `smelt-style` because
//! the role table is host-specific — a different host (e.g. the
//! tcloc treemap) carries its own role vocabulary or none at all.

use smelt_style::theme::{intern, HlGroup};

/// Resolve a smelt role label (e.g. `"Accent"`, `"Slug"`, `"Muted"`)
/// to its canonical theme group id. Renderers flow the returned id
/// through extmarks so theme switches mutate `Theme.styles[id]` once
/// instead of re-rendering buffers. Unknown labels intern as-is.
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
