#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStreamEvent<'a> {
    TextDelta(&'a str),
    ThinkingDelta(&'a str),
    ToolCall(ToolCallStreamEvent<'a>),
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
