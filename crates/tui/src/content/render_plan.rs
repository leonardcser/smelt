use smelt_core::lua::TranscriptGroupSpec;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey, ViewState};
use std::collections::{HashMap, HashSet};
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
        default_view_state: ViewState,
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

#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptPresentationState {
    overrides: HashMap<RenderNodeId, ViewState>,
    generation: u64,
}

impl TranscriptPresentationState {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn effective_view_state(
        &self,
        plan: &RenderPlan,
        history: &BlockHistory,
        index: usize,
    ) -> Option<ViewState> {
        let id = plan.node_id(index)?;
        self.overrides
            .get(&id)
            .copied()
            .or_else(|| plan.node_default_view_state(history, index))
    }

    pub(crate) fn set(
        &mut self,
        plan: &RenderPlan,
        history: &BlockHistory,
        id: RenderNodeId,
        view_state: ViewState,
    ) -> bool {
        if !plan.contains_id(id) {
            return false;
        }
        let Some(default) = plan.node_default_view_state_by_id(history, id) else {
            return false;
        };
        let changed = if view_state == default {
            self.overrides.remove(&id).is_some()
        } else if self.overrides.get(&id) == Some(&view_state) {
            false
        } else {
            self.overrides.insert(id, view_state);
            true
        };
        if changed {
            self.bump();
        }
        changed
    }

    pub(crate) fn toggle(
        &mut self,
        plan: &RenderPlan,
        history: &BlockHistory,
        id: RenderNodeId,
    ) -> bool {
        let Some(index) = plan.index_of(id) else {
            return false;
        };
        let Some(current) = self.effective_view_state(plan, history, index) else {
            return false;
        };
        let next = if matches!(current, ViewState::Expanded) {
            ViewState::Collapsed
        } else {
            ViewState::Expanded
        };
        self.set(plan, history, id, next)
    }

    pub(crate) fn set_all(
        &mut self,
        plan: &RenderPlan,
        history: &BlockHistory,
        view_state: ViewState,
    ) -> bool {
        let mut changed = false;
        for id in plan.ids().collect::<Vec<_>>() {
            changed |= self.set(plan, history, id, view_state);
        }
        changed
    }

    pub(crate) fn prune(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        let live: HashSet<RenderNodeId> = ids.into_iter().collect();
        let before = self.overrides.len();
        self.overrides.retain(|id, _| live.contains(id));
        if self.overrides.len() != before {
            self.bump();
        }
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
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

    pub(crate) fn index_of(&self, id: RenderNodeId) -> Option<usize> {
        self.index_by_id.get(&id).copied()
    }

    pub(crate) fn node_default_view_state(
        &self,
        history: &BlockHistory,
        index: usize,
    ) -> Option<ViewState> {
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => {
                let semantic = history.resolve_key(
                    *id,
                    LayoutKey {
                        width: 0,
                        show_thinking: false,
                        view_state: ViewState::Expanded,
                        content_hash: 0,
                        sidecar_hash: 0,
                    },
                );
                if semantic.view_state != ViewState::Expanded {
                    return Some(semantic.view_state);
                }
                match history.blocks.get(id) {
                    Some(Block::Thinking { .. }) => Some(ViewState::Collapsed),
                    _ => Some(ViewState::Expanded),
                }
            }
            RenderNode::Group {
                default_view_state, ..
            } => Some(*default_view_state),
        }
    }

    pub(crate) fn node_default_view_state_by_id(
        &self,
        history: &BlockHistory,
        id: RenderNodeId,
    ) -> Option<ViewState> {
        self.index_of(id)
            .and_then(|index| self.node_default_view_state(history, index))
    }

    pub(crate) fn node_key(
        &self,
        history: &BlockHistory,
        presentation: &TranscriptPresentationState,
        index: usize,
        base_key: LayoutKey,
    ) -> Option<NodeLayoutKey> {
        let view_state = presentation.effective_view_state(self, history, index)?;
        self.node_key_with_view_state(history, index, base_key, view_state)
    }

    pub(crate) fn node_key_with_view_state(
        &self,
        history: &BlockHistory,
        index: usize,
        base_key: LayoutKey,
        view_state: ViewState,
    ) -> Option<NodeLayoutKey> {
        if history.generation() != self.history_generation {
            return None;
        }
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(LayoutKey {
                width: base_key.width,
                show_thinking: base_key.show_thinking,
                view_state,
                content_hash: history.content_hash(*id),
                sidecar_hash: block_sidecar_hash(history, *id),
            }),
            RenderNode::Group {
                id,
                name,
                bucket,
                child_range,
                child_ids,
                ..
            } => Some(LayoutKey {
                width: base_key.width,
                show_thinking: base_key.show_thinking,
                view_state,
                content_hash: smelt_core::utils::hash_serializable(&GroupContentKey {
                    id: *id,
                    name,
                    bucket,
                    group_generation: self.group_generation,
                    group_cache_key: self.group_cache_key,
                    view_state,
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
            default_view_state: view_state(spec.default_view.as_deref()),
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
