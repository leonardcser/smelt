use protocol::ReasoningKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStreamEvent<'a> {
    TextDelta(&'a str),
    Reasoning(ReasoningStreamEvent<'a>),
    ToolCall(ToolCallStreamEvent<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStreamEvent<'a> {
    PartStarted {
        item_id: &'a str,
        part_index: u32,
        kind: ReasoningKind,
    },
    Delta {
        item_id: &'a str,
        part_index: u32,
        kind: ReasoningKind,
        delta: &'a str,
    },
    PartFinished {
        item_id: &'a str,
        part_index: u32,
        kind: ReasoningKind,
        content: Option<&'a str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStreamEvent<'a> {
    Started {
        stream_id: &'a str,
        call_id: Option<&'a str>,
        tool_name: Option<&'a str>,
    },
    ArgsDelta {
        stream_id: &'a str,
        call_id: Option<&'a str>,
        tool_name: Option<&'a str>,
        delta: &'a str,
    },
    Finished {
        stream_id: &'a str,
        call_id: &'a str,
        tool_name: &'a str,
        arguments: &'a str,
    },
}
