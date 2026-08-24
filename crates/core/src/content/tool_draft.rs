//! Retained tool arguments and incremental JSON draft parsing.
//!
//! Streamed draft bytes are moved into shared transcript content once. Top-level
//! string fields retain their own content handles so renderers can consume them
//! without copying a growing JSON value through snapshots.

use crate::transcript_content::{ContentId, TranscriptContent};
use protocol::StyledLines;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::ops::Range;

const LUA_ARGUMENT_PREVIEW_BYTES: usize = 4 * 1024;
const STRUCTURED_ARGUMENT_PREVIEW_BYTES: usize = 64 * 1024;

fn json_map_dynamic_retained_bytes(values: &HashMap<String, serde_json::Value>) -> usize {
    values
        .capacity()
        .saturating_mul(
            std::mem::size_of::<(String, serde_json::Value)>()
                .saturating_add(std::mem::size_of::<usize>()),
        )
        .saturating_add(
            values
                .iter()
                .map(|(key, value)| {
                    key.capacity()
                        .saturating_add(protocol::json_value_dynamic_retained_bytes(value))
                })
                .sum::<usize>(),
        )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArgumentField {
    pub name: String,
    pub content: TranscriptContent,
    pub complete: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolArguments {
    preview: HashMap<String, serde_json::Value>,
    structured_values: HashMap<String, serde_json::Value>,
    string_fields: Vec<ToolArgumentField>,
    structured_hash: u64,
}

impl ToolArguments {
    pub fn from_values(values: HashMap<String, serde_json::Value>) -> Self {
        Self::from_values_reusing(values, None)
    }

    pub fn from_values_reusing(
        values: HashMap<String, serde_json::Value>,
        draft: Option<&ToolDraft>,
    ) -> Self {
        let mut preview = HashMap::with_capacity(values.len());
        let mut structured_values = HashMap::new();
        let mut string_fields = Vec::new();
        let mut entries = values.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        for (name, value) in entries {
            match value {
                serde_json::Value::String(value) => {
                    let content = draft
                        .and_then(|draft| draft.string_field(&name))
                        .filter(|field| {
                            field.complete
                                && field.content.len() == value.len()
                                && field.content.content_hash() == seahash::hash(value.as_bytes())
                        })
                        .map(|field| field.content.clone())
                        .unwrap_or_else(|| TranscriptContent::from(value.clone()));
                    preview.insert(
                        name.clone(),
                        serde_json::Value::String(bounded_preview(&value)),
                    );
                    string_fields.push(ToolArgumentField {
                        name,
                        content,
                        complete: true,
                    });
                }
                value => {
                    preview.insert(name.clone(), structured_preview(&value));
                    structured_values.insert(name, value);
                }
            }
        }
        let structured_hash = hash_structured_values(&structured_values);
        Self {
            preview,
            structured_values,
            string_fields,
            structured_hash,
        }
    }

    pub fn preview(&self) -> &HashMap<String, serde_json::Value> {
        &self.preview
    }

    pub fn get(&self, name: &str) -> Option<&serde_json::Value> {
        self.preview.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> {
        self.preview.iter()
    }

    pub fn string_fields(&self) -> &[ToolArgumentField] {
        &self.string_fields
    }

    pub fn string_field(&self, name: &str) -> Option<&ToolArgumentField> {
        self.string_fields.iter().find(|field| field.name == name)
    }

    pub fn contents(&self) -> impl Iterator<Item = &TranscriptContent> {
        self.string_fields.iter().map(|field| &field.content)
    }

    pub fn content_hash(&self) -> u64 {
        let mut fields = self
            .string_fields
            .iter()
            .map(|field| {
                (
                    field.name.as_str(),
                    field.content.len(),
                    field.content.content_hash(),
                )
            })
            .collect::<Vec<_>>();
        fields.sort_unstable_by_key(|field| field.0);
        crate::utils::hash_serializable(&(self.structured_hash, fields))
    }

    pub fn dynamic_retained_bytes(&self) -> usize {
        json_map_dynamic_retained_bytes(&self.preview)
            .saturating_add(json_map_dynamic_retained_bytes(&self.structured_values))
            .saturating_add(
                self.string_fields
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ToolArgumentField>()),
            )
            .saturating_add(
                self.string_fields
                    .iter()
                    .map(|field| {
                        field
                            .name
                            .capacity()
                            .saturating_add(field.content.dynamic_retained_bytes())
                    })
                    .sum::<usize>(),
            )
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

impl From<HashMap<String, serde_json::Value>> for ToolArguments {
    fn from(values: HashMap<String, serde_json::Value>) -> Self {
        Self::from_values(values)
    }
}

impl Serialize for ToolArguments {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(
            self.structured_values
                .len()
                .saturating_add(self.string_fields.len()),
        ))?;
        for (name, value) in &self.structured_values {
            map.serialize_entry(name, value)?;
        }
        for field in &self.string_fields {
            map.serialize_entry(&field.name, &field.content.snapshot())?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ToolArguments {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        HashMap::<String, serde_json::Value>::deserialize(deserializer).map(Self::from_values)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDraft {
    pub stream_id: String,
    pub call_id: Option<String>,
    pub name: String,
    pub summary: StyledLines,
    pub arguments: ToolArguments,
    pub raw_arguments: TranscriptContent,
    pub finished: bool,
    #[serde(skip)]
    parser: DraftJsonParser,
}

impl ToolDraft {
    pub fn new(stream_id: String, call_id: Option<String>, name: String) -> Self {
        Self {
            stream_id,
            call_id,
            summary: StyledLines::from_plain(&name),
            name,
            arguments: ToolArguments::default(),
            raw_arguments: TranscriptContent::new(),
            finished: false,
            parser: DraftJsonParser::default(),
        }
    }

    pub fn append(&mut self, delta: String) -> ToolDraftAppend {
        let parsed = self.parser.append(&mut self.arguments, &delta);
        let raw_range = self.raw_arguments.append_owned(delta);
        ToolDraftAppend {
            raw_content_id: self.raw_arguments.id(),
            raw_range,
            field_appends: parsed.field_appends,
            new_fields: parsed.new_fields,
            removed_field_ids: parsed.removed_field_ids,
            presentation_changed: parsed.presentation_changed,
        }
    }

    pub fn finish(&mut self, mut arguments: String) -> ToolDraftAppend {
        let mut append = if content_matches(&self.raw_arguments, &arguments) {
            ToolDraftAppend::default()
        } else if content_is_prefix_of(&self.raw_arguments, &arguments) {
            let suffix = arguments.split_off(self.raw_arguments.len());
            self.append(suffix)
        } else {
            let removed_field_ids = self
                .arguments
                .string_fields
                .iter()
                .map(|field| field.content.id())
                .collect();
            self.arguments = ToolArguments::default();
            self.parser = DraftJsonParser::default();
            self.raw_arguments.clear();
            let mut append = self.append(arguments);
            append.removed_field_ids = removed_field_ids;
            append.presentation_changed = true;
            append
        };
        append.presentation_changed |= self.parser.finish(&mut self.arguments);
        if !self.finished {
            self.finished = true;
            append.presentation_changed = true;
        }
        append
    }

    pub fn update_identity(&mut self, call_id: Option<String>, name: Option<String>) -> bool {
        let mut changed = false;
        if call_id
            .as_deref()
            .is_some_and(|call_id| !call_id.is_empty())
            && self.call_id != call_id
        {
            self.call_id = call_id;
            changed = true;
        }
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            if self.name != name {
                self.name = name;
                changed = true;
            }
        }
        changed
    }

    pub fn string_fields(&self) -> &[ToolArgumentField] {
        self.arguments.string_fields()
    }

    pub fn string_field(&self, name: &str) -> Option<&ToolArgumentField> {
        self.arguments.string_field(name)
    }

    pub fn contents(&self) -> impl Iterator<Item = &TranscriptContent> {
        std::iter::once(&self.raw_arguments).chain(self.arguments.contents())
    }

    pub fn content_hash(&self) -> u64 {
        crate::utils::hash_serializable(&(
            &self.stream_id,
            &self.call_id,
            &self.name,
            &self.summary,
            self.arguments.content_hash(),
            self.raw_arguments.len(),
            self.raw_arguments.content_hash(),
            self.finished,
        ))
    }

    pub fn dynamic_retained_bytes(&self) -> usize {
        self.stream_id
            .capacity()
            .saturating_add(self.call_id.as_ref().map_or(0, String::capacity))
            .saturating_add(self.name.capacity())
            .saturating_add(self.summary.dynamic_retained_bytes())
            .saturating_add(self.arguments.dynamic_retained_bytes())
            .saturating_add(self.raw_arguments.dynamic_retained_bytes())
            .saturating_add(self.parser.dynamic_retained_bytes())
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDraftAppend {
    pub raw_content_id: ContentId,
    pub raw_range: Range<usize>,
    pub field_appends: Vec<ToolDraftFieldAppend>,
    pub new_fields: Vec<TranscriptContent>,
    pub removed_field_ids: Vec<ContentId>,
    pub presentation_changed: bool,
}

impl Default for ToolDraftAppend {
    fn default() -> Self {
        Self {
            raw_content_id: ContentId::new(0),
            raw_range: 0..0,
            field_appends: Vec::new(),
            new_fields: Vec::new(),
            removed_field_ids: Vec::new(),
            presentation_changed: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDraftFieldAppend {
    pub content: TranscriptContent,
    pub byte_range: Range<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct DraftJsonParser {
    state: PreviewState,
    key: String,
    raw_value: String,
    escaped: bool,
    unicode_value: u16,
    unicode_digits: u8,
    pending_high_surrogate: Option<u16>,
    nested_depth: usize,
    nested_in_string: bool,
    nested_escaped: bool,
    active_field: Option<usize>,
    preview_truncated: bool,
    raw_value_truncated: bool,
}

impl DraftJsonParser {
    fn dynamic_retained_bytes(&self) -> usize {
        self.key
            .capacity()
            .saturating_add(self.raw_value.capacity())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum PreviewState {
    #[default]
    BeforeObject,
    BeforeKey,
    InKey,
    AfterKey,
    BeforeValue,
    InString,
    InBare,
    InNested,
    AfterValue,
    Done,
}

enum UnicodeCompletion {
    Pending,
    One(char),
    Two(char, char),
}

#[derive(Default)]
struct ParseAppend {
    field_appends: Vec<ToolDraftFieldAppend>,
    new_fields: Vec<TranscriptContent>,
    removed_field_ids: Vec<ContentId>,
    presentation_changed: bool,
}

impl DraftJsonParser {
    fn append(&mut self, arguments: &mut ToolArguments, delta: &str) -> ParseAppend {
        let _perf = smelt_perf::perf::begin("transcript:draft:append_json");
        let mut buffers = self
            .active_field
            .and_then(|index| arguments.string_fields.get(index))
            .map(|field| vec![(field.content.id(), String::with_capacity(delta.len()))])
            .unwrap_or_default();
        let mut result = ParseAppend::default();
        for ch in delta.chars() {
            self.push_char(arguments, ch, &mut buffers, &mut result);
        }
        for (content_id, chunk) in buffers {
            if chunk.is_empty() {
                continue;
            }
            let Some(field) = arguments
                .string_fields
                .iter()
                .find(|field| field.content.id() == content_id)
            else {
                continue;
            };
            let byte_range = field.content.append_owned(chunk);
            result.field_appends.push(ToolDraftFieldAppend {
                content: field.content.clone(),
                byte_range,
            });
        }
        result
    }

    fn finish(&mut self, arguments: &mut ToolArguments) -> bool {
        if self.state != PreviewState::InBare {
            return false;
        }
        let presentation_changed = self.commit_raw_value(arguments);
        self.active_field = None;
        self.state = PreviewState::Done;
        presentation_changed
    }

    fn push_char(
        &mut self,
        arguments: &mut ToolArguments,
        ch: char,
        buffers: &mut Vec<(ContentId, String)>,
        result: &mut ParseAppend,
    ) {
        match self.state {
            PreviewState::BeforeObject => {
                if ch == '{' {
                    self.state = PreviewState::BeforeKey;
                }
            }
            PreviewState::BeforeKey => {
                if ch.is_whitespace() || ch == ',' {
                    return;
                }
                match ch {
                    '"' => {
                        self.key.clear();
                        self.escaped = false;
                        self.state = PreviewState::InKey;
                    }
                    '}' => self.state = PreviewState::Done,
                    _ => {}
                }
            }
            PreviewState::InKey => {
                if self.push_key_char(ch) {
                    self.state = PreviewState::AfterKey;
                }
            }
            PreviewState::AfterKey => {
                if ch.is_whitespace() {
                    return;
                }
                if ch == ':' {
                    self.state = PreviewState::BeforeValue;
                } else if ch == ',' {
                    self.state = PreviewState::BeforeKey;
                }
            }
            PreviewState::BeforeValue => {
                if ch.is_whitespace() {
                    return;
                }
                match ch {
                    '"' => {
                        self.begin_string(arguments, result);
                        self.escaped = false;
                        self.state = PreviewState::InString;
                    }
                    '{' | '[' => {
                        self.begin_raw(arguments, ch, buffers, result);
                        self.nested_depth = 1;
                        self.nested_in_string = false;
                        self.nested_escaped = false;
                        self.state = PreviewState::InNested;
                    }
                    '}' => self.state = PreviewState::Done,
                    _ => {
                        self.begin_raw(arguments, ch, buffers, result);
                        self.state = PreviewState::InBare;
                    }
                }
            }
            PreviewState::InString => {
                if self.push_value_char(arguments, ch, buffers, result) {
                    if let Some(field) = self
                        .active_field
                        .and_then(|index| arguments.string_fields.get_mut(index))
                    {
                        field.complete = true;
                    }
                    self.active_field = None;
                    self.state = PreviewState::AfterValue;
                }
            }
            PreviewState::InBare => {
                if ch == ',' || ch == '}' {
                    result.presentation_changed |= self.commit_raw_value(arguments);
                    self.active_field = None;
                    self.state = if ch == '}' {
                        PreviewState::Done
                    } else {
                        PreviewState::BeforeKey
                    };
                } else {
                    self.push_raw_value_char(arguments, ch, buffers);
                }
            }
            PreviewState::InNested => {
                self.push_raw_value_char(arguments, ch, buffers);
                self.advance_nested(ch, arguments, result);
            }
            PreviewState::AfterValue => {
                if ch.is_whitespace() {
                    return;
                }
                match ch {
                    ',' => self.state = PreviewState::BeforeKey,
                    '}' => self.state = PreviewState::Done,
                    _ => {}
                }
            }
            PreviewState::Done => {}
        }
    }

    fn begin_string(&mut self, arguments: &mut ToolArguments, result: &mut ParseAppend) {
        self.active_field = Some(self.begin_field(arguments, result));
        self.preview_truncated = false;
        arguments
            .preview
            .insert(self.key.clone(), serde_json::Value::String(String::new()));
    }

    fn begin_raw(
        &mut self,
        arguments: &mut ToolArguments,
        ch: char,
        buffers: &mut Vec<(ContentId, String)>,
        result: &mut ParseAppend,
    ) {
        self.raw_value.clear();
        self.raw_value_truncated = false;
        self.active_field = Some(self.begin_field(arguments, result));
        self.push_raw_value_char(arguments, ch, buffers);
    }

    fn begin_field(&self, arguments: &mut ToolArguments, result: &mut ParseAppend) -> usize {
        if let Some(index) = arguments
            .string_fields
            .iter()
            .position(|field| field.name == self.key)
        {
            let removed = arguments.string_fields.remove(index);
            result.removed_field_ids.push(removed.content.id());
        }
        arguments.preview.remove(&self.key);
        arguments.structured_values.remove(&self.key);
        let field = ToolArgumentField {
            name: self.key.clone(),
            content: TranscriptContent::new(),
            complete: false,
        };
        result.new_fields.push(field.content.clone());
        arguments.string_fields.push(field);
        result.presentation_changed = true;
        arguments.string_fields.len().saturating_sub(1)
    }

    fn push_key_char(&mut self, ch: char) -> bool {
        if self.unicode_digits > 0 {
            match self.push_unicode_digit(ch) {
                UnicodeCompletion::Pending => {}
                UnicodeCompletion::One(ch) => self.key.push(ch),
                UnicodeCompletion::Two(first, second) => {
                    self.key.push(first);
                    self.key.push(second);
                }
            }
            return false;
        }
        if self.escaped {
            self.escaped = false;
            if ch == 'u' {
                self.begin_unicode_escape();
            } else {
                if self.pending_high_surrogate.take().is_some() {
                    self.key.push(char::REPLACEMENT_CHARACTER);
                }
                self.key.push(decode_escape(ch));
            }
            return false;
        }
        match ch {
            '\\' => {
                self.escaped = true;
                false
            }
            '"' => {
                if self.pending_high_surrogate.take().is_some() {
                    self.key.push(char::REPLACEMENT_CHARACTER);
                }
                true
            }
            other => {
                if self.pending_high_surrogate.take().is_some() {
                    self.key.push(char::REPLACEMENT_CHARACTER);
                }
                self.key.push(other);
                false
            }
        }
    }

    fn push_value_char(
        &mut self,
        arguments: &mut ToolArguments,
        ch: char,
        buffers: &mut Vec<(ContentId, String)>,
        result: &mut ParseAppend,
    ) -> bool {
        if self.unicode_digits > 0 {
            match self.push_unicode_digit(ch) {
                UnicodeCompletion::Pending => {}
                UnicodeCompletion::One(ch) => {
                    self.push_decoded_value(arguments, ch, buffers, result);
                }
                UnicodeCompletion::Two(first, second) => {
                    self.push_decoded_value(arguments, first, buffers, result);
                    self.push_decoded_value(arguments, second, buffers, result);
                }
            }
            return false;
        }
        if self.escaped {
            self.escaped = false;
            if ch == 'u' {
                self.begin_unicode_escape();
            } else {
                if self.pending_high_surrogate.take().is_some() {
                    self.push_decoded_value(
                        arguments,
                        char::REPLACEMENT_CHARACTER,
                        buffers,
                        result,
                    );
                }
                self.push_decoded_value(arguments, decode_escape(ch), buffers, result);
            }
            return false;
        }
        match ch {
            '\\' => {
                self.escaped = true;
                false
            }
            '"' => {
                if self.pending_high_surrogate.take().is_some() {
                    self.push_decoded_value(
                        arguments,
                        char::REPLACEMENT_CHARACTER,
                        buffers,
                        result,
                    );
                }
                true
            }
            other => {
                if self.pending_high_surrogate.take().is_some() {
                    self.push_decoded_value(
                        arguments,
                        char::REPLACEMENT_CHARACTER,
                        buffers,
                        result,
                    );
                }
                self.push_decoded_value(arguments, other, buffers, result);
                false
            }
        }
    }

    fn begin_unicode_escape(&mut self) {
        self.unicode_value = 0;
        self.unicode_digits = 4;
    }

    fn push_unicode_digit(&mut self, ch: char) -> UnicodeCompletion {
        let Some(digit) = ch.to_digit(16) else {
            self.unicode_digits = 0;
            self.pending_high_surrogate = None;
            return UnicodeCompletion::One(char::REPLACEMENT_CHARACTER);
        };
        self.unicode_value = (self.unicode_value << 4) | digit as u16;
        self.unicode_digits = self.unicode_digits.saturating_sub(1);
        if self.unicode_digits != 0 {
            return UnicodeCompletion::Pending;
        }

        let value = self.unicode_value;
        if (0xD800..=0xDBFF).contains(&value) {
            self.pending_high_surrogate = Some(value);
            return UnicodeCompletion::Pending;
        }
        if (0xDC00..=0xDFFF).contains(&value) {
            return self.pending_high_surrogate.take().map_or(
                UnicodeCompletion::One(char::REPLACEMENT_CHARACTER),
                |high| {
                    let codepoint =
                        0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(value) - 0xDC00);
                    UnicodeCompletion::One(
                        char::from_u32(codepoint).unwrap_or(char::REPLACEMENT_CHARACTER),
                    )
                },
            );
        }

        let decoded = char::from_u32(u32::from(value)).unwrap_or(char::REPLACEMENT_CHARACTER);
        if self.pending_high_surrogate.take().is_some() {
            UnicodeCompletion::Two(char::REPLACEMENT_CHARACTER, decoded)
        } else {
            UnicodeCompletion::One(decoded)
        }
    }

    fn push_decoded_value(
        &mut self,
        arguments: &mut ToolArguments,
        ch: char,
        buffers: &mut Vec<(ContentId, String)>,
        result: &mut ParseAppend,
    ) {
        let Some(field_index) = self.active_field else {
            return;
        };
        if let Some(field) = arguments.string_fields.get(field_index) {
            push_field_char(buffers, field.content.id(), ch);
        }
        if let Some(serde_json::Value::String(preview)) = arguments.preview.get_mut(&self.key) {
            result.presentation_changed |= push_bounded_grapheme_char(
                preview,
                &mut self.preview_truncated,
                ch,
                LUA_ARGUMENT_PREVIEW_BYTES,
            );
        }
    }

    fn push_raw_value_char(
        &mut self,
        arguments: &ToolArguments,
        ch: char,
        buffers: &mut Vec<(ContentId, String)>,
    ) {
        push_bounded_grapheme_char(
            &mut self.raw_value,
            &mut self.raw_value_truncated,
            ch,
            STRUCTURED_ARGUMENT_PREVIEW_BYTES,
        );
        if let Some(field) = self
            .active_field
            .and_then(|field_index| arguments.string_fields.get(field_index))
        {
            push_field_char(buffers, field.content.id(), ch);
        }
    }

    fn commit_raw_value(&mut self, arguments: &mut ToolArguments) -> bool {
        let raw = self.raw_value.trim();
        let presentation_changed = if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
        {
            arguments
                .preview
                .insert(self.key.clone(), structured_preview(&value));
            true
        } else {
            false
        };
        if let Some(field) = self
            .active_field
            .and_then(|index| arguments.string_fields.get_mut(index))
        {
            field.complete = true;
        }
        presentation_changed
    }

    fn advance_nested(
        &mut self,
        ch: char,
        arguments: &mut ToolArguments,
        result: &mut ParseAppend,
    ) {
        if self.nested_in_string {
            if self.nested_escaped {
                self.nested_escaped = false;
            } else if ch == '\\' {
                self.nested_escaped = true;
            } else if ch == '"' {
                self.nested_in_string = false;
            }
            return;
        }
        match ch {
            '"' => self.nested_in_string = true,
            '{' | '[' => self.nested_depth += 1,
            '}' | ']' => {
                self.nested_depth = self.nested_depth.saturating_sub(1);
                if self.nested_depth == 0 {
                    result.presentation_changed |= self.commit_raw_value(arguments);
                    self.active_field = None;
                    self.state = PreviewState::AfterValue;
                }
            }
            _ => {}
        }
    }
}

fn push_bounded_grapheme_char(
    buffer: &mut String,
    truncated: &mut bool,
    ch: char,
    max_bytes: usize,
) -> bool {
    if *truncated {
        return false;
    }
    let previous_len = buffer.len();
    buffer.push(ch);
    if buffer.len() > max_bytes {
        let keep = smelt_buffer::text::grapheme_prefix(buffer, max_bytes).len();
        smelt_buffer::text::replace_range(buffer, keep..buffer.len(), "");
        *truncated = true;
    }
    buffer.len() != previous_len
}

fn push_field_char(buffers: &mut Vec<(ContentId, String)>, content_id: ContentId, ch: char) {
    if let Some((_, buffer)) = buffers.last_mut().filter(|(id, _)| *id == content_id) {
        buffer.push(ch);
    } else {
        let mut buffer = String::new();
        buffer.push(ch);
        buffers.push((content_id, buffer));
    }
}

fn decode_escape(ch: char) -> char {
    match ch {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'b' => '\u{0008}',
        'f' => '\u{000c}',
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        other => other,
    }
}

fn bounded_preview(value: &str) -> String {
    if value.len() <= LUA_ARGUMENT_PREVIEW_BYTES {
        return value.to_owned();
    }
    let mut preview =
        smelt_buffer::text::grapheme_prefix(value, LUA_ARGUMENT_PREVIEW_BYTES).to_owned();
    preview.push_str("\n…");
    preview
}

fn structured_preview(value: &serde_json::Value) -> serde_json::Value {
    let mut budget = STRUCTURED_ARGUMENT_PREVIEW_BYTES;
    cap_structured_value(value, &mut budget)
}

fn cap_structured_value(value: &serde_json::Value, budget: &mut usize) -> serde_json::Value {
    if *budget == 0 {
        return serde_json::Value::String("…".into());
    }
    *budget = budget.saturating_sub(1);
    match value {
        serde_json::Value::String(value) => {
            let max_bytes = value.len().min(LUA_ARGUMENT_PREVIEW_BYTES).min(*budget);
            let mut preview = smelt_buffer::text::grapheme_prefix(value, max_bytes).to_owned();
            *budget = budget.saturating_sub(preview.len());
            if preview.len() < value.len() && *budget > 0 {
                preview.push('…');
                *budget = budget.saturating_sub('…'.len_utf8());
            }
            serde_json::Value::String(preview)
        }
        serde_json::Value::Array(values) => {
            let mut preview = Vec::new();
            for value in values {
                if *budget == 0 {
                    preview.push(serde_json::Value::String("…".into()));
                    break;
                }
                preview.push(cap_structured_value(value, budget));
            }
            serde_json::Value::Array(preview)
        }
        serde_json::Value::Object(values) => {
            let mut preview = serde_json::Map::new();
            for (key, value) in values {
                if *budget <= key.len() {
                    break;
                }
                *budget = budget.saturating_sub(key.len());
                preview.insert(key.clone(), cap_structured_value(value, budget));
            }
            serde_json::Value::Object(preview)
        }
        value => value.clone(),
    }
}

fn hash_structured_values(values: &HashMap<String, serde_json::Value>) -> u64 {
    let mut fields = values
        .iter()
        .map(|(name, value)| (name.as_str(), crate::utils::hash_serializable(value)))
        .collect::<Vec<_>>();
    fields.sort_unstable_by_key(|field| field.0);
    crate::utils::hash_serializable(&fields)
}

fn content_matches(content: &TranscriptContent, value: &str) -> bool {
    content.len() == value.len() && content_is_prefix_of(content, value)
}

fn content_is_prefix_of(content: &TranscriptContent, value: &str) -> bool {
    if content.len() > value.len() {
        return false;
    }
    let read = content.read();
    let mut offset = 0usize;
    for chunk in read.chunks() {
        let end = offset.saturating_add(chunk.len());
        if value.get(offset..end) != Some(chunk.as_str()) {
            return false;
        }
        offset = end;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_fields_keep_stable_content_identity() {
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "write_file".into());
        let first = draft.append(r#"{"file_path":"src/main.rs","content":"hel"#.into());
        let content_id = draft.string_field("content").unwrap().content.id();
        assert!(first.presentation_changed);

        let second = draft.append("lo\nworld".into());
        let field = draft.string_field("content").unwrap();
        assert_eq!(field.content.id(), content_id);
        assert_eq!(field.content.snapshot(), "hello\nworld");
        assert!(second.presentation_changed);
    }

    #[test]
    fn presentation_changes_follow_bounded_argument_previews() {
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "grep".into());
        let expected_preview = "a".repeat(LUA_ARGUMENT_PREVIEW_BYTES);
        let first = draft.append(format!(r#"{{"pattern":"match","path":"{expected_preview}"#));
        assert!(first.presentation_changed);
        assert_eq!(
            draft
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str),
            Some(expected_preview.as_str())
        );

        let beyond_preview = draft.append("b".into());
        assert!(!beyond_preview.presentation_changed);
        assert_eq!(
            draft.string_field("path").unwrap().content.len(),
            LUA_ARGUMENT_PREVIEW_BYTES + 1
        );

        let mut structured = ToolDraft::new("structured".into(), None, "grep".into());
        structured.append(r#"{"head_limit":"#.into());
        let completed = structured.append("25,".into());
        assert!(completed.presentation_changed);
        assert_eq!(
            structured.arguments.get("head_limit"),
            Some(&serde_json::json!(25))
        );
    }

    #[test]
    fn bounded_argument_preview_drops_a_grapheme_completed_past_the_limit() {
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "grep".into());
        let prefix = "a".repeat(LUA_ARGUMENT_PREVIEW_BYTES - 1);
        draft.append(format!(r#"{{"path":"{prefix}e"#));
        let continued = draft.append("\u{301}ignored".into());

        assert!(continued.presentation_changed);
        assert_eq!(
            draft
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str),
            Some(prefix.as_str())
        );
        assert_eq!(
            draft.string_field("path").unwrap().content.snapshot(),
            format!("{prefix}e\u{301}ignored")
        );
    }

    #[test]
    fn final_arguments_reuse_streamed_string_content() {
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "write_file".into());
        draft.append(r#"{"content":"hel"#.into());
        draft.append("lo\"}".into());
        let content = draft.string_field("content").unwrap().content.clone();
        let arguments = ToolArguments::from_values_reusing(
            HashMap::from([("content".into(), serde_json::json!("hello"))]),
            Some(&draft),
        );
        assert_eq!(
            arguments.string_field("content").unwrap().content.id(),
            content.id()
        );
    }

    #[test]
    fn finish_appends_only_missing_suffix_and_reconciles_mismatches() {
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "write_file".into());
        draft.append(r#"{"content":"hel"#.into());
        let raw_id = draft.raw_arguments.id();
        let append = draft.finish(r#"{"content":"hello"}"#.into());
        assert_eq!(draft.raw_arguments.id(), raw_id);
        assert_eq!(draft.raw_arguments.snapshot(), r#"{"content":"hello"}"#);
        assert_eq!(
            draft.string_field("content").unwrap().content.snapshot(),
            "hello"
        );
        assert!(!append.raw_range.is_empty());

        let old_field_id = draft.string_field("content").unwrap().content.id();
        draft.finished = false;
        let replaced = draft.finish(r#"{"content":"goodbye"}"#.into());
        assert_eq!(draft.raw_arguments.id(), raw_id);
        assert_eq!(draft.raw_arguments.snapshot(), r#"{"content":"goodbye"}"#);
        assert_eq!(
            draft.string_field("content").unwrap().content.snapshot(),
            "goodbye"
        );
        assert!(replaced.removed_field_ids.contains(&old_field_id));
    }

    #[test]
    fn unicode_escapes_and_duplicate_fields_are_incremental() {
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "tool".into());
        draft.append(r#"{"emoji":"\uD83D"#.into());
        draft.append(r#"\uDE00","na\u006de":"first","name":"latest"}"#.into());

        assert_eq!(draft.string_fields().len(), 2);
        assert_eq!(
            draft.string_field("emoji").unwrap().content.snapshot(),
            "😀"
        );
        let field = draft.string_field("name").unwrap();
        assert_eq!(field.content.snapshot(), "latest");
        assert!(field.complete);
    }

    #[test]
    fn finishing_complete_edit_arguments_preserves_preview_fields() {
        let values = serde_json::json!({
            "file_path": "/tmp/preview.rs",
            "old_string": "println!(\"flicker-old\");",
            "new_string": "println!(\"flicker-new\");",
            "replace_all": false,
        });
        let mut draft = ToolDraft::new("stream".into(), Some("call".into()), "edit_file".into());

        draft.finish(serde_json::to_string(&values).unwrap());

        assert!(draft.finished);
        let expected = values
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        assert_eq!(draft.arguments.preview(), &expected);
    }

    #[test]
    fn serialization_preserves_complete_argument_values() {
        let nested_content = "x".repeat(100_000);
        let nested = serde_json::json!({"items": [{"content": nested_content}]});
        let arguments = ToolArguments::from_values(HashMap::from([
            ("content".into(), serde_json::json!("hello")),
            ("replace_all".into(), serde_json::json!(true)),
            ("nested".into(), nested.clone()),
        ]));
        assert_eq!(
            serde_json::to_value(&arguments).unwrap(),
            serde_json::json!({"content":"hello","replace_all":true,"nested":nested})
        );
        assert!(
            serde_json::to_string(arguments.get("nested").unwrap())
                .unwrap()
                .len()
                < 8_000
        );
    }
}
