use smelt_core::lua::TranscriptGroupSpec;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey, ViewState};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum RenderNodeId {
    Block(BlockId),
    Group(u64),
}

impl RenderNodeId {
    pub(crate) fn as_block_id(self) -> Option<BlockId> {
        match self {
            Self::Block(id) => Some(id),
            Self::Group(_) => None,
        }
    }
}

pub(crate) type NodeLayoutKey = LayoutKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenderNode {
    Block {
        id: BlockId,
        block_index: usize,
    },
    Group {
        id: u64,
        name: String,
        bucket: String,
        child_range: Range<usize>,
        child_ids: Vec<BlockId>,
        view_state: ViewState,
    },
}

impl RenderNode {
    pub(crate) fn id(&self) -> RenderNodeId {
        match self {
            Self::Block { id, .. } => RenderNodeId::Block(*id),
            Self::Group { id, .. } => RenderNodeId::Group(*id),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RenderPlan {
    pub(crate) history_generation: u64,
    pub(crate) group_generation: u64,
    pub(crate) group_cache_key: Option<u64>,
    pub(crate) nodes: Vec<RenderNode>,
    index_by_id: HashMap<RenderNodeId, usize>,
    pub(crate) fingerprint: u64,
}

impl RenderPlan {
    pub(crate) fn empty() -> Self {
        Self {
            history_generation: 0,
            group_generation: 0,
            group_cache_key: None,
            nodes: Vec::new(),
            index_by_id: HashMap::new(),
            fingerprint: 0,
        }
    }

    pub(crate) fn for_history_with_groups(
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Self {
        let nodes = build_nodes(history, groups);
        let index_by_id = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id(), index))
            .collect();
        let fingerprint = smelt_core::utils::hash_serializable(&PlanFingerprint {
            history_generation: history.generation(),
            group_generation,
            group_cache_key,
            node_ids: nodes.iter().map(RenderNode::id).collect(),
            node_keys: nodes
                .iter()
                .map(|node| node_fingerprint(history, node))
                .collect(),
        });
        Self {
            history_generation: history.generation(),
            group_generation,
            group_cache_key,
            nodes,
            index_by_id,
            fingerprint,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn node(&self, index: usize) -> Option<&RenderNode> {
        self.nodes.get(index)
    }

    pub(crate) fn node_id(&self, index: usize) -> Option<RenderNodeId> {
        self.nodes.get(index).map(RenderNode::id)
    }

    pub(crate) fn contains_id(&self, id: RenderNodeId) -> bool {
        self.index_by_id.contains_key(&id)
    }

    pub(crate) fn node_key(
        &self,
        history: &BlockHistory,
        index: usize,
        base_key: LayoutKey,
    ) -> Option<NodeLayoutKey> {
        if history.generation() != self.history_generation {
            return None;
        }
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(history.resolve_key(*id, base_key)),
            RenderNode::Group {
                id,
                name,
                bucket,
                child_range,
                child_ids,
                view_state,
            } => Some(LayoutKey {
                width: base_key.width,
                show_thinking: base_key.show_thinking,
                view_state: *view_state,
                content_hash: smelt_core::utils::hash_serializable(&GroupContentKey {
                    id: *id,
                    name,
                    bucket,
                    group_generation: self.group_generation,
                    group_cache_key: self.group_cache_key,
                    view_state: *view_state,
                    child_ids,
                    child_hashes: child_range
                        .clone()
                        .filter_map(|block_index| {
                            history
                                .order
                                .get(block_index)
                                .map(|id| history.content_hash(*id))
                        })
                        .collect(),
                }),
                sidecar_hash: group_sidecar_hash(history, child_range.clone()),
            }),
        }
    }

    pub(crate) fn rendered_node_gap(
        &self,
        history: &BlockHistory,
        index: usize,
        rendered_rows: usize,
    ) -> u16 {
        if rendered_rows == 0 {
            return 0;
        }
        self.node_start_block_index(index)
            .map(|block_index| history.block_gap(block_index))
            .unwrap_or(0)
    }

    fn node_start_block_index(&self, index: usize) -> Option<usize> {
        match self.nodes.get(index)? {
            RenderNode::Block { block_index, .. } => Some(*block_index),
            RenderNode::Group { child_range, .. } => Some(child_range.start),
        }
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = RenderNodeId> + '_ {
        self.nodes.iter().map(RenderNode::id)
    }
}

#[derive(serde::Serialize)]
struct PlanFingerprint {
    history_generation: u64,
    group_generation: u64,
    group_cache_key: Option<u64>,
    node_ids: Vec<RenderNodeId>,
    node_keys: Vec<u64>,
}

#[derive(serde::Serialize)]
struct GroupIdKey<'a> {
    name: &'a str,
    bucket: &'a str,
    first_child: BlockId,
}

#[derive(serde::Serialize)]
struct GroupContentKey<'a> {
    id: u64,
    name: &'a str,
    bucket: &'a str,
    group_generation: u64,
    group_cache_key: Option<u64>,
    view_state: ViewState,
    child_ids: &'a [BlockId],
    child_hashes: Vec<u64>,
}

fn build_nodes(history: &BlockHistory, groups: &[TranscriptGroupSpec]) -> Vec<RenderNode> {
    if groups.is_empty() {
        return history
            .order
            .iter()
            .copied()
            .enumerate()
            .map(|(block_index, id)| RenderNode::Block { id, block_index })
            .collect();
    }

    let mut nodes = Vec::new();
    let mut index = 0usize;
    while index < history.order.len() {
        let Some((spec, bucket)) = matching_group(history, groups, index) else {
            nodes.push(RenderNode::Block {
                id: history.order[index],
                block_index: index,
            });
            index += 1;
            continue;
        };
        let start = index;
        index += 1;
        while index < history.order.len()
            && matching_group(history, groups, index).as_ref().is_some_and(
                |(next_spec, next_bucket)| next_spec.name == spec.name && next_bucket == &bucket,
            )
        {
            index += 1;
        }
        if index - start < spec.min {
            nodes.extend((start..index).map(|block_index| RenderNode::Block {
                id: history.order[block_index],
                block_index,
            }));
            continue;
        }
        let child_ids = history.order[start..index].to_vec();
        let id = smelt_core::utils::hash_serializable(&GroupIdKey {
            name: &spec.name,
            bucket: &bucket,
            first_child: child_ids[0],
        });
        nodes.push(RenderNode::Group {
            id,
            name: spec.name.clone(),
            bucket,
            child_range: start..index,
            child_ids,
            view_state: view_state(spec.default_view.as_deref()),
        });
    }
    nodes
}

fn matching_group<'a>(
    history: &BlockHistory,
    groups: &'a [TranscriptGroupSpec],
    block_index: usize,
) -> Option<(&'a TranscriptGroupSpec, String)> {
    groups.iter().find_map(|spec| {
        selector_matches(history, spec, block_index).then(|| {
            let bucket =
                group_bucket(history, spec, block_index).unwrap_or_else(|| spec.name.clone());
            (spec, bucket)
        })
    })
}

fn selector_matches(
    history: &BlockHistory,
    spec: &TranscriptGroupSpec,
    block_index: usize,
) -> bool {
    let Some(id) = history.order.get(block_index).copied() else {
        return false;
    };
    let Some(block) = history.blocks.get(&id) else {
        return false;
    };
    if spec
        .selector
        .kind
        .as_deref()
        .is_some_and(|kind| kind != block_kind(block))
    {
        return false;
    }
    if spec
        .selector
        .name
        .as_deref()
        .is_some_and(|name| tool_name(block) != Some(name))
    {
        return false;
    }
    if let Some(terminal) = spec.selector.terminal {
        let terminal_state = match block {
            Block::ToolCall { call_id, .. } => history
                .tool_state(call_id)
                .is_some_and(smelt_core::ToolState::is_terminal),
            _ => false,
        };
        if terminal_state != terminal {
            return false;
        }
    }
    true
}

fn group_bucket(
    history: &BlockHistory,
    spec: &TranscriptGroupSpec,
    block_index: usize,
) -> Option<String> {
    let bucket = spec.bucket.as_ref()?;
    let values: Vec<_> = bucket
        .fields
        .iter()
        .map(|field| block_field(history, block_index, field).unwrap_or_default())
        .collect();
    Some(values.join("\u{1f}"))
}

fn block_field(history: &BlockHistory, block_index: usize, field: &str) -> Option<String> {
    let id = history.order.get(block_index).copied()?;
    let block = history.blocks.get(&id)?;
    match field {
        "kind" => Some(block_kind(block).to_string()),
        "name" => tool_name(block).map(str::to_string),
        "status" => match block {
            Block::ToolCall { call_id, .. } => history
                .tool_state(call_id)
                .map(|state| state.status.label().to_string()),
            _ => None,
        },
        field => field.strip_prefix("args.").and_then(|arg| match block {
            Block::ToolCall { args, .. } => args.get(arg).map(json_bucket_value),
            _ => None,
        }),
    }
}

fn json_bucket_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn node_fingerprint(history: &BlockHistory, node: &RenderNode) -> u64 {
    match node {
        RenderNode::Block { id, .. } => smelt_core::utils::hash_serializable(&(
            history.content_hash(*id),
            block_sidecar_hash(history, *id),
        )),
        RenderNode::Group { child_range, .. } => group_sidecar_hash(history, child_range.clone()),
    }
}

fn group_sidecar_hash(history: &BlockHistory, child_range: Range<usize>) -> u64 {
    smelt_core::utils::hash_serializable(
        &child_range
            .filter_map(|block_index| history.order.get(block_index).copied())
            .map(|id| block_sidecar_hash(history, id))
            .collect::<Vec<_>>(),
    )
}

fn block_sidecar_hash(history: &BlockHistory, id: BlockId) -> u64 {
    match history.blocks.get(&id) {
        Some(Block::ToolCall { call_id, .. }) => history
            .tool_state(call_id)
            .map(smelt_core::ToolState::display_hash)
            .unwrap_or(0),
        _ => 0,
    }
}

fn tool_name(block: &Block) -> Option<&str> {
    match block {
        Block::ToolCall { name, .. } => Some(name),
        _ => None,
    }
}

fn block_kind(block: &Block) -> &'static str {
    block.kind()
}

fn view_state(value: Option<&str>) -> ViewState {
    match value {
        Some("collapsed") => ViewState::Collapsed,
        Some("trimmed_head") => ViewState::TrimmedHead { keep: 1 },
        Some("trimmed_tail") => ViewState::TrimmedTail { keep: 1 },
        _ => ViewState::Expanded,
    }
}
