//! Shared renderer metadata for live tools and detached checkpoint entries.

use crate::content::tool_draft::{
    argument_preview, json_map_dynamic_retained_bytes, ToolArgumentField, ToolArguments,
};
use crate::transcript_content::TranscriptContent;
use crate::transcript_model::ToolOutput;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Range;

pub(crate) const LUA_CONTENT_PREVIEW_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TranscriptContentMetadata {
    // Content handles are process-local, not part of a persisted presentation.
    #[serde(skip)]
    pub content_id: Option<u64>,
    #[serde(skip)]
    pub content_revision: u64,
    pub content_bytes: usize,
    pub content_lines: usize,
    pub content_preview: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TranscriptToolOutputContentFieldMetadata {
    pub name: String,
    pub content: TranscriptContentMetadata,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TranscriptToolOutputMetadata {
    pub content: TranscriptContentMetadata,
    pub is_error: bool,
    pub metadata: Option<serde_json::Value>,
    pub content_fields: Vec<TranscriptToolOutputContentFieldMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TranscriptArgumentFieldMetadata {
    pub name: String,
    pub content: TranscriptContentMetadata,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolPresentation {
    pub(crate) args: HashMap<String, serde_json::Value>,
    pub(crate) argument_fields: Vec<TranscriptArgumentFieldMetadata>,
    pub(crate) output: Option<Box<TranscriptToolOutputMetadata>>,
}

impl ToolPresentation {
    pub(crate) fn new(args: &ToolArguments, output: Option<&ToolOutput>) -> Self {
        Self {
            args: args.preview().clone(),
            argument_fields: transcript_argument_field_metadata(args.string_fields()),
            output: output.map(transcript_tool_output_metadata).map(Box::new),
        }
    }

    pub fn from_result(
        args: HashMap<String, serde_json::Value>,
        result: &protocol::ToolOutcome,
    ) -> Self {
        let mut argument_fields = Vec::new();
        let args = args
            .into_iter()
            .map(|(name, value)| {
                if let serde_json::Value::String(content) = &value {
                    argument_fields.push(TranscriptArgumentFieldMetadata {
                        name: name.clone(),
                        content: text_metadata(content),
                        complete: true,
                    });
                }
                (name, argument_preview(&value))
            })
            .collect();
        argument_fields.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let output = Some(Box::new(TranscriptToolOutputMetadata {
            content: text_metadata(&result.content),
            is_error: result.is_error,
            metadata: result.metadata.clone(),
            content_fields: result
                .display_content
                .iter()
                .map(|field| TranscriptToolOutputContentFieldMetadata {
                    name: field.name.clone(),
                    content: text_metadata(&field.content),
                })
                .collect(),
        }));
        Self {
            args,
            argument_fields,
            output,
        }
    }

    pub(crate) fn dynamic_retained_bytes(&self) -> usize {
        json_map_dynamic_retained_bytes(&self.args)
            .saturating_add(
                self.argument_fields.capacity()
                    * std::mem::size_of::<TranscriptArgumentFieldMetadata>(),
            )
            .saturating_add(
                self.argument_fields
                    .iter()
                    .map(|field| field.name.capacity() + field.content.content_preview.capacity())
                    .sum::<usize>(),
            )
            .saturating_add(self.output.as_ref().map_or(0, |output| {
                std::mem::size_of::<TranscriptToolOutputMetadata>()
                    + output.content.content_preview.capacity()
                    + output
                        .metadata
                        .as_ref()
                        .map_or(0, protocol::json_value_dynamic_retained_bytes)
                    + output.content_fields.capacity()
                        * std::mem::size_of::<TranscriptToolOutputContentFieldMetadata>()
                    + output
                        .content_fields
                        .iter()
                        .map(|field| {
                            field.name.capacity() + field.content.content_preview.capacity()
                        })
                        .sum::<usize>()
            }))
    }
}

fn bounded_content_preview(len: usize, slice: impl Fn(Range<usize>) -> String) -> String {
    if len <= LUA_CONTENT_PREVIEW_BYTES {
        return slice(0..len);
    }
    const OMISSION_MARKER: &str = "\n…\n";
    let payload_bytes = LUA_CONTENT_PREVIEW_BYTES.saturating_sub(OMISSION_MARKER.len());
    let head_end = payload_bytes / 2;
    let tail_start = len.saturating_sub(payload_bytes.saturating_sub(head_end));
    let head = slice(0..head_end);
    let tail = slice(tail_start..len);
    let preview = [head.as_str(), OMISSION_MARKER, tail.as_str()].concat();
    if preview.len() > LUA_CONTENT_PREVIEW_BYTES {
        smelt_buffer::text::slice(&preview, 0..LUA_CONTENT_PREVIEW_BYTES).to_owned()
    } else {
        preview
    }
}

fn text_metadata(content: &str) -> TranscriptContentMetadata {
    TranscriptContentMetadata {
        content_id: None,
        content_revision: 0,
        content_bytes: content.len(),
        content_lines: content.lines().count(),
        content_preview: bounded_content_preview(content.len(), |range| {
            smelt_buffer::text::slice(content, range).to_owned()
        }),
    }
}

pub(crate) fn transcript_content_metadata(
    content: &TranscriptContent,
) -> TranscriptContentMetadata {
    let read = content.read();
    TranscriptContentMetadata {
        content_id: Some(content.id().get()),
        content_revision: read.revision(),
        content_bytes: read.len(),
        content_lines: read.logical_line_count(),
        content_preview: bounded_content_preview(read.len(), |range| {
            read.slice(range).into_owned()
        }),
    }
}

pub(crate) fn transcript_argument_field_metadata(
    fields: &[ToolArgumentField],
) -> Vec<TranscriptArgumentFieldMetadata> {
    fields
        .iter()
        .map(|field| TranscriptArgumentFieldMetadata {
            name: field.name.clone(),
            content: transcript_content_metadata(&field.content),
            complete: field.complete,
        })
        .collect()
}

pub(crate) fn transcript_tool_output_metadata(output: &ToolOutput) -> TranscriptToolOutputMetadata {
    TranscriptToolOutputMetadata {
        content: transcript_content_metadata(&output.content),
        is_error: output.is_error,
        metadata: output.metadata.clone(),
        content_fields: output
            .content_fields
            .iter()
            .map(|field| TranscriptToolOutputContentFieldMetadata {
                name: field.name.clone(),
                content: transcript_content_metadata(&field.content),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_presentation_matches_live_metadata_without_content_handles() {
        let large = "日本語\n".repeat(10_000);
        let args = serde_json::json!({"file_path":"example.rs", "content": large});
        let mut result = protocol::ToolOutcome::new(
            large.clone(),
            true,
            Some(serde_json::json!({"display_count":{"value":7,"unit":"match"}})),
        );
        result.display_content.push(protocol::ToolDisplayContent {
            name: "new_source".into(),
            content: large.clone().into(),
        });
        let args: HashMap<String, serde_json::Value> = serde_json::from_value(args).unwrap();
        let arguments = ToolArguments::from_values(args.clone());
        let output = ToolOutput::from_display_content(
            large,
            true,
            result.metadata.clone(),
            result.display_content.clone(),
        );
        let live = ToolPresentation::new(&arguments, Some(&output));
        let detached = ToolPresentation::from_result(args, &result);
        let serialized = serde_json::to_string(&detached).unwrap();
        assert_eq!(
            serde_json::to_value(&live).unwrap(),
            serde_json::to_value(&detached).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<ToolPresentation>(&serialized).unwrap(),
            detached
        );
        assert!(!serialized.contains("content_id"));
        assert!(serialized.len() < 30_000);
        assert!(detached.dynamic_retained_bytes() < 30_000);
        let output = detached.output.unwrap();
        assert_eq!(output.content.content_lines, 10_000);
        assert!(output.content.content_preview.len() <= LUA_CONTENT_PREVIEW_BYTES);
        assert_eq!(output.content_fields[0].content.content_lines, 10_000);
        assert!(
            output.content_fields[0].content.content_preview.len() <= LUA_CONTENT_PREVIEW_BYTES
        );
    }

    #[test]
    fn detached_line_counts_match_live_content() {
        for source in [
            "",
            "\n",
            "one",
            "one\n",
            "one\ntwo",
            "one\ntwo\n",
            "日本語\r\n",
        ] {
            let live = TranscriptContent::from(source.to_owned());
            let stored = text_metadata(source);
            let live = transcript_content_metadata(&live);
            assert_eq!(stored.content_lines, live.content_lines, "{source:?}");
            assert_eq!(stored.content_preview, live.content_preview);
        }
    }

    #[test]
    fn old_kept_tools_deserialize_without_presentation() {
        let old =
            serde_json::json!({"kind":"tool", "name":"read_file", "summary":[], "status":"Ok"});
        assert!(matches!(
            serde_json::from_value::<crate::transcript_model::KeptHistoryItem>(old).unwrap(),
            crate::transcript_model::KeptHistoryItem::Tool {
                presentation: None,
                ..
            }
        ));
    }
}
