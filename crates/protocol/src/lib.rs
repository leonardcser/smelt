//! Wire-protocol and shared domain types between the engine and the UI.
//!
//! Layout:
//! - [`content`]: multipart message content (text + images)
//! - [`message`]: `Message`, `Role`, tool calls, tool outcomes
//! - [`mode`]: agent modes and reasoning effort levels
//! - [`note`]: stable markers for synthetic model-visible history notes
//! - [`usage`]: token usage, turn metadata, per-turn overrides
//! - [`event`]: the wire contract - `EngineEvent` and `UiCommand`

pub mod content;
pub mod event;
pub mod history;
pub mod message;
pub mod mode;
pub mod model;
pub mod note;
pub mod request_log;
pub mod style;
pub mod usage;

pub use content::{Content, ContentPart};
pub use event::{
    AgentProjectContext, AskResponseFormat, CanonicalHistoryDelta, CanonicalHistoryIndex, Decision,
    EngineAskError, EngineAskErrorKind, EngineEvent, InvocationId, ModelHistoryCoordinates,
    ModelHistoryIndex, ModelHistorySource, PersistenceScope, ReasoningKind, StartTurnInput,
    StartTurnPayload, ToolDef, ToolEvaluation, ToolExecutionMode, ToolHookFlags, ToolMetadata,
    UiCommand,
};
pub use history::{
    apply_history_append, classify_user_history_content, compaction_summary_content,
    effective_mode_at, history_from_messages, history_item_from_user_content,
    history_item_message_count, history_to_messages, plan_history_append, replace_last_note_kind,
    transcript_block_kind_matches_history_item, AssistantStep, HistoryAppend, HistoryAppendPlan,
    HistoryAppendPolicy, HistoryAppendResult, HistoryAppendView, HistoryItem, HistoryNote,
    HistoryNoteKind, HistoryNoteProjection, HistoryTailBudget, JobTermination, ProcessStatusEvent,
    ToolInvocation, UserHistoryContent, COMPACTION_SUMMARY_PREFIX, DEFAULT_CONTEXT_NOTE_NAME,
};
pub use message::{
    json_value_dynamic_retained_bytes, json_value_retained_bytes,
    supports_image_tool_attachment_mime, supports_tool_attachment_mime,
    validate_tool_display_content, validate_tool_display_field_names, validate_tool_metadata,
    FunctionCall, Message, ReasoningBlock, Role, ToolAttachment, ToolAttachmentModality, ToolCall,
    ToolDisplayContent, ToolOutcome, ToolResultValidationError, IMAGE_TOOL_ATTACHMENT_MIME_TYPES,
    TOOL_DISPLAY_CONTENT_MAX_FIELDS, TOOL_DISPLAY_CONTENT_MAX_NAME_BYTES,
    TOOL_DISPLAY_METADATA_FIELDS, TOOL_METADATA_MAX_BYTES, TOOL_METADATA_MAX_DEPTH,
    TOOL_METADATA_MAX_KEY_BYTES, TOOL_METADATA_MAX_NODES,
};
pub use mode::{AgentMode, ReasoningEffort};
pub use model::{
    ModelCatalogMetadata, ModelConfig, ModelMetadata, ModelTarget, RequestAuditMode,
    RequestRuntimeConfig,
};
pub use note::{
    context_note, mode_change_note, process_status_note, CONTEXT_NOTE_PREFIX, MODE_NOTE_PREFIX,
    PROCESS_STATUS_NOTE_PREFIX,
};
pub use style::{StyledLines, StyledSpan};
pub use usage::{
    ModelConfigOverrides, PermissionOverrides, RuleSetOverride, ThinkingBudgets, TokenUsage,
    TurnMeta,
};
