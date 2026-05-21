//! Wire-protocol and shared domain types between the engine and the UI.
//!
//! Layout:
//! - [`content`]: multipart message content (text + images)
//! - [`message`]: `Message`, `Role`, tool calls, tool outcomes
//! - [`mode`]: agent modes and reasoning effort levels
//! - [`usage`]: token usage, turn metadata, per-turn overrides
//! - [`event`]: the wire contract — `EngineEvent` and `UiCommand`

pub mod content;
pub mod event;
pub mod history;
pub mod message;
pub mod mode;
pub mod style;
pub mod usage;

pub use content::{Content, ContentPart};
pub use event::{
    AskModel, AskResponseFormat, Decision, EngineAskError, EngineAskErrorKind, EngineEvent,
    StartTurnPayload, ToolDef, ToolExecutionMode, ToolHookFlags, ToolHooks, UiCommand,
};
pub use history::{
    history_from_messages, history_to_message_positions, history_to_messages,
    message_to_history_positions, AssistantTurn, HistoryItem, ToolInvocation,
};
pub use message::{FunctionCall, Message, ReasoningBlock, Role, ToolCall, ToolOutcome};
pub use mode::{mode_change_note, AgentMode, ReasoningEffort, MODE_NOTE_PREFIX};
pub use style::{StyledLines, StyledSpan};
pub use usage::{ModelConfigOverrides, PermissionOverrides, RuleSetOverride, TokenUsage, TurnMeta};
