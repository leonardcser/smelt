use mlua::{Lua, Table, Value};
use smelt_core::lua::TranscriptGroupSpec;
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
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
    Block { id: BlockId, block_index: usize },
    Group(Box<RenderGroupNode>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderGroupNode {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) bucket: String,
    pub(crate) child_range: Range<usize>,
    pub(crate) child_ids: Vec<BlockId>,
    pub(crate) default_view_state: ViewState,
}

impl RenderNode {
    pub(crate) fn id(&self) -> RenderNodeId {
        match self {
            Self::Block { id, .. } => RenderNodeId::Block(*id),
            Self::Group(group) => RenderNodeId::Group(group.id),
        }
    }
}

pub(crate) const TRANSCRIPT_APPEND_HEADROOM: usize = 64;

#[derive(Clone, Debug, Default)]
struct RenderNodeIndex {
    blocks: Vec<(BlockId, usize)>,
    groups: HashMap<u64, usize>,
}

impl RenderNodeIndex {
    fn for_nodes(nodes: &[RenderNode]) -> Self {
        let mut index = Self {
            blocks: Vec::with_capacity(nodes.len().saturating_add(TRANSCRIPT_APPEND_HEADROOM)),
            groups: HashMap::new(),
        };
        for (node_index, node) in nodes.iter().enumerate() {
            match node.id() {
                RenderNodeId::Block(id) => index.blocks.push((id, node_index)),
                RenderNodeId::Group(id) => {
                    index.groups.insert(id, node_index);
                }
            }
        }
        index.blocks.sort_unstable_by_key(|(id, _)| *id);
        index
    }

    fn insert(&mut self, id: RenderNodeId, node_index: usize) {
        match id {
            RenderNodeId::Block(id) => match self.blocks.binary_search_by_key(&id, |(id, _)| *id) {
                Ok(pos) => self.blocks[pos] = (id, node_index),
                Err(pos) => self.blocks.insert(pos, (id, node_index)),
            },
            RenderNodeId::Group(id) => {
                self.groups.insert(id, node_index);
            }
        }
    }

    fn get(&self, id: RenderNodeId) -> Option<usize> {
        match id {
            RenderNodeId::Block(id) => self
                .blocks
                .binary_search_by_key(&id, |(id, _)| *id)
                .ok()
                .map(|pos| self.blocks[pos].1),
            RenderNodeId::Group(id) => self.groups.get(&id).copied(),
        }
    }

    fn contains(&self, id: RenderNodeId) -> bool {
        self.get(id).is_some()
    }

    fn truncate_from(&mut self, index: usize) {
        self.blocks.retain(|(_, node_index)| *node_index < index);
        self.groups.retain(|_, node_index| *node_index < index);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptDefaultViewPolicy {
    block_kinds: HashMap<String, ViewState>,
    tools: HashMap<String, ViewState>,
    groups: HashMap<String, ViewState>,
    disabled_groups: HashSet<String>,
}

impl Default for TranscriptDefaultViewPolicy {
    fn default() -> Self {
        let mut policy = Self {
            block_kinds: HashMap::new(),
            tools: HashMap::new(),
            groups: HashMap::new(),
            disabled_groups: HashSet::new(),
        };
        policy
            .block_kinds
            .insert("thinking".to_string(), ViewState::Peek);
        policy
            .block_kinds
            .insert("compacted".to_string(), ViewState::Collapsed);
        policy
            .block_kinds
            .insert("compaction_preview".to_string(), ViewState::Peek);
        for tool in [
            "load_skill",
            "read_file",
            "grep",
            "glob",
            "web_fetch",
            "enter_worktree",
        ] {
            policy.tools.insert(tool.to_string(), ViewState::Collapsed);
        }
        policy
    }
}

impl TranscriptDefaultViewPolicy {
    pub(crate) fn from_lua(lua: &smelt_core::lua::runtime::LuaRuntime) -> Self {
        let mut policy = Self::default();
        if let Err(err) = policy.apply_lua_config(&lua.lua) {
            lua.record_error(format!("smelt.settings.transcript.view: {err}"));
        }
        policy
    }

    fn apply_lua_config(&mut self, lua: &Lua) -> mlua::Result<()> {
        let globals = lua.globals();
        let smelt = globals.get::<Option<Table>>("smelt")?;
        let Some(smelt) = smelt else { return Ok(()) };
        let settings = smelt.get::<Option<Table>>("settings")?;
        let Some(settings) = settings else {
            return Ok(());
        };
        let transcript = settings.get::<Option<Table>>("transcript")?;
        let Some(transcript) = transcript else {
            return Ok(());
        };
        let config = transcript.get::<Option<Table>>("view")?;
        let Some(config) = config else { return Ok(()) };

        apply_view_section(&config, "blocks", &mut self.block_kinds)?;
        apply_view_section(&config, "tools", &mut self.tools)?;
        apply_group_view_section(&config, &mut self.groups, &mut self.disabled_groups)?;
        Ok(())
    }

    pub(crate) fn group_enabled(&self, name: &str) -> bool {
        !self.disabled_groups.contains(name)
    }

    pub(crate) fn node_default_view_state(
        &self,
        history: &BlockHistory,
        node: &RenderNode,
    ) -> ViewState {
        match node {
            RenderNode::Block { id, .. } => history
                .tool_name(*id)
                .and_then(|name| self.tools.get(name).copied())
                .or_else(|| {
                    history
                        .block_kind(*id)
                        .and_then(|kind| self.block_kinds.get(kind).copied())
                })
                .unwrap_or(ViewState::Expanded),
            RenderNode::Group(group) => self
                .groups
                .get(&group.name)
                .copied()
                .unwrap_or(group.default_view_state),
        }
    }

    #[cfg(test)]
    fn block_default_view_state(&self, block: &smelt_core::transcript_model::Block) -> ViewState {
        tool_name(block)
            .and_then(|name| self.tools.get(name).copied())
            .or_else(|| self.block_kinds.get(block_kind(block)).copied())
            .unwrap_or(ViewState::Expanded)
    }
}

fn apply_view_section(
    config: &Table,
    name: &str,
    target: &mut HashMap<String, ViewState>,
) -> mlua::Result<()> {
    let section = config.get::<Option<Table>>(name)?;
    let Some(section) = section else {
        return Ok(());
    };
    for pair in section.pairs::<String, Value>() {
        let (key, value) = pair?;
        if let Some(view_state) = lua_view_state(value)? {
            target.insert(key, view_state);
        }
    }
    Ok(())
}

fn apply_group_view_section(
    config: &Table,
    groups: &mut HashMap<String, ViewState>,
    disabled: &mut HashSet<String>,
) -> mlua::Result<()> {
    let section = config.get::<Option<Table>>("groups")?;
    let Some(section) = section else {
        return Ok(());
    };
    for pair in section.pairs::<String, Value>() {
        let (key, value) = pair?;
        if matches!(value, Value::Boolean(false) | Value::Nil) {
            groups.remove(&key);
            disabled.insert(key);
            continue;
        }
        if let Some(view_state) = lua_view_state(value)? {
            disabled.remove(&key);
            groups.insert(key, view_state);
        }
    }
    Ok(())
}

fn lua_view_state(value: Value) -> mlua::Result<Option<ViewState>> {
    match value {
        Value::String(s) => parse_lua_view_state(s.to_str()?.as_ref()).map(Some),
        Value::Table(t) => match t.get::<Option<String>>("default_view")?.as_deref() {
            Some(value) => parse_lua_view_state(value).map(Some),
            None => Ok(None),
        },
        Value::Nil | Value::Boolean(false) => Ok(None),
        other => Err(mlua::Error::external(format!(
            "expected \"collapsed\", \"peek\", or \"expanded\", got {}",
            other.type_name()
        ))),
    }
}

fn parse_lua_view_state(value: &str) -> mlua::Result<ViewState> {
    match value {
        "collapsed" => Ok(ViewState::Collapsed),
        "peek" => Ok(ViewState::Peek),
        "expanded" => Ok(ViewState::Expanded),
        other => Err(mlua::Error::external(format!(
            "unknown view state `{other}`; expected \"collapsed\", \"peek\", or \"expanded\""
        ))),
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
        policy: &TranscriptDefaultViewPolicy,
        plan: &RenderPlan,
        history: &BlockHistory,
        index: usize,
    ) -> Option<ViewState> {
        let id = plan.node_id(index)?;
        self.overrides
            .get(&id)
            .copied()
            .or_else(|| plan.node_default_view_state(policy, history, index))
    }

    pub(crate) fn set(
        &mut self,
        policy: &TranscriptDefaultViewPolicy,
        plan: &RenderPlan,
        history: &BlockHistory,
        id: RenderNodeId,
        view_state: ViewState,
    ) -> bool {
        if !plan.contains_id(id) {
            return false;
        }
        let Some(default) = plan.node_default_view_state_by_id(policy, history, id) else {
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
        policy: &TranscriptDefaultViewPolicy,
        plan: &RenderPlan,
        history: &BlockHistory,
        id: RenderNodeId,
    ) -> bool {
        let Some(index) = plan.index_of(id) else {
            return false;
        };
        let Some(current) = self.effective_view_state(policy, plan, history, index) else {
            return false;
        };
        let Some(default) = plan.node_default_view_state(policy, history, index) else {
            return false;
        };
        let next = if matches!(current, ViewState::Expanded) {
            if matches!(default, ViewState::Expanded) {
                ViewState::Collapsed
            } else {
                default
            }
        } else {
            ViewState::Expanded
        };
        self.set(policy, plan, history, id, next)
    }

    pub(crate) fn set_all(
        &mut self,
        policy: &TranscriptDefaultViewPolicy,
        plan: &RenderPlan,
        history: &BlockHistory,
        view_state: ViewState,
    ) -> bool {
        let mut changed = false;
        for id in plan.ids().collect::<Vec<_>>() {
            changed |= self.set(policy, plan, history, id, view_state);
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

#[derive(Clone, Copy, Debug)]
struct RenderPlanPrefix {
    len: usize,
    fingerprint: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RenderPlan {
    pub(crate) history_generation: u64,
    pub(crate) history_order_generation: u64,
    history_record_generation: u64,
    history_order_len: usize,
    pub(crate) group_generation: u64,
    pub(crate) group_cache_key: Option<u64>,
    pub(crate) nodes: Vec<RenderNode>,
    node_index: RenderNodeIndex,
    grouped_block_index_by_id: HashMap<BlockId, usize>,
    pub(crate) fingerprint: u64,
    append_prefix: Option<RenderPlanPrefix>,
}

impl RenderPlan {
    pub(crate) fn empty() -> Self {
        Self {
            history_generation: 0,
            history_order_generation: 0,
            history_record_generation: 0,
            history_order_len: 0,
            group_generation: 0,
            group_cache_key: None,
            nodes: Vec::new(),
            node_index: RenderNodeIndex::default(),
            grouped_block_index_by_id: HashMap::new(),
            fingerprint: 0,
            append_prefix: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_history_with_groups(
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Self {
        let _perf = smelt_perf::perf::begin("transcript:render_plan");
        Self::build_for_history_with_groups(history, groups, group_generation, group_cache_key)
    }

    pub(crate) fn refresh_for_history_with_groups(
        &mut self,
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> bool {
        let _perf = smelt_perf::perf::begin("transcript:render_plan");
        if let Some(prune_required) =
            self.try_refresh_incremental(history, groups, group_generation, group_cache_key)
        {
            smelt_perf::perf::record_value("transcript:render_plan:reused", 1);
            return prune_required;
        }
        smelt_perf::perf::record_value("transcript:render_plan:reused", 0);
        *self =
            Self::build_for_history_with_groups(history, groups, group_generation, group_cache_key);
        true
    }

    fn build_for_history_with_groups(
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Self {
        let nodes = {
            let _perf = smelt_perf::perf::begin("transcript:render_plan:build_nodes");
            build_nodes(history, groups)
        };
        let node_index = {
            let _perf = smelt_perf::perf::begin("transcript:render_plan:node_index");
            RenderNodeIndex::for_nodes(&nodes)
        };
        let mut grouped_block_index_by_id = HashMap::new();
        {
            let _perf = smelt_perf::perf::begin("transcript:render_plan:grouped_block_index");
            for (index, node) in nodes.iter().enumerate() {
                if let RenderNode::Group(group) = node {
                    for id in &group.child_ids {
                        grouped_block_index_by_id.insert(*id, index);
                    }
                }
            }
        }
        let fingerprint = {
            let _perf = smelt_perf::perf::begin("transcript:render_plan:fingerprint");
            render_plan_fingerprint(history, &nodes, group_generation, group_cache_key)
        };
        Self {
            history_generation: history.generation(),
            history_order_generation: history.order_generation(),
            history_record_generation: history.record_dirty_generation(),
            history_order_len: history.order.len(),
            group_generation,
            group_cache_key,
            nodes,
            node_index,
            grouped_block_index_by_id,
            fingerprint,
            append_prefix: None,
        }
    }

    fn try_refresh_incremental(
        &mut self,
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Option<bool> {
        if groups.is_empty()
            && self
                .nodes
                .iter()
                .any(|node| matches!(node, RenderNode::Group(_)))
        {
            return None;
        }
        if groups.is_empty() && history.order_generation() == self.history_order_generation {
            if self.history_order_len != history.order.len() {
                return None;
            }
            self.history_generation = history.generation();
            self.history_record_generation = history.record_dirty_generation();
            self.group_generation = group_generation;
            self.group_cache_key = group_cache_key;
            self.append_prefix = None;
            self.refresh_fingerprint(history);
            return Some(false);
        }
        if !groups.is_empty()
            && (self.group_generation != group_generation
                || self.group_cache_key != group_cache_key)
        {
            return None;
        }
        let old_order_len = self.history_order_len;
        if old_order_len > history.order.len()
            || history.record_changed_from_since(self.history_record_generation)? < old_order_len
        {
            return None;
        }
        let rebuild_from_block = if groups.is_empty() {
            old_order_len
        } else {
            group_append_recompute_start(history, groups, old_order_len)
        };
        let truncate_from_node = if rebuild_from_block == old_order_len {
            self.nodes.len()
        } else {
            let id = history.order.get(rebuild_from_block).copied()?;
            self.index_for_block(id)?
        };
        let old_fingerprint = self.fingerprint;
        let old_node_len = self.nodes.len();
        let pure_append = truncate_from_node == old_node_len && rebuild_from_block == old_order_len;
        let prune_required = truncate_from_node < old_node_len;
        self.truncate_from(truncate_from_node);
        {
            let _perf = smelt_perf::perf::begin("transcript:render_plan:append_nodes");
            for node in build_nodes_from(history, groups, rebuild_from_block) {
                self.push_node(node);
            }
        }
        self.history_generation = history.generation();
        self.history_order_generation = history.order_generation();
        self.history_record_generation = history.record_dirty_generation();
        self.history_order_len = history.order.len();
        self.group_generation = group_generation;
        self.group_cache_key = group_cache_key;
        if pure_append {
            self.extend_fingerprint(history);
            self.append_prefix = Some(RenderPlanPrefix {
                len: old_node_len,
                fingerprint: old_fingerprint,
            });
        } else {
            self.refresh_fingerprint(history);
            self.append_prefix = None;
        }
        Some(prune_required)
    }

    fn push_node(&mut self, node: RenderNode) {
        let index = self.nodes.len();
        self.node_index.insert(node.id(), index);
        if let RenderNode::Group(group) = &node {
            for id in &group.child_ids {
                self.grouped_block_index_by_id.insert(*id, index);
            }
        }
        self.nodes.push(node);
    }

    fn truncate_from(&mut self, index: usize) {
        self.nodes.truncate(index);
        self.node_index.truncate_from(index);
        self.grouped_block_index_by_id
            .retain(|_, node_index| *node_index < index);
    }

    fn refresh_fingerprint(&mut self, history: &BlockHistory) {
        let _perf = smelt_perf::perf::begin("transcript:render_plan:fingerprint");
        self.fingerprint = render_plan_fingerprint(
            history,
            &self.nodes,
            self.group_generation,
            self.group_cache_key,
        );
    }

    fn extend_fingerprint(&mut self, history: &BlockHistory) {
        let _perf = smelt_perf::perf::begin("transcript:render_plan:fingerprint_append");
        self.fingerprint = render_plan_fingerprint(
            history,
            &self.nodes,
            self.group_generation,
            self.group_cache_key,
        );
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn append_prefix(&self) -> Option<(usize, u64)> {
        self.append_prefix
            .map(|prefix| (prefix.len, prefix.fingerprint))
    }

    pub(crate) fn node(&self, index: usize) -> Option<&RenderNode> {
        self.nodes.get(index)
    }

    pub(crate) fn node_id(&self, index: usize) -> Option<RenderNodeId> {
        self.nodes.get(index).map(RenderNode::id)
    }

    pub(crate) fn contains_id(&self, id: RenderNodeId) -> bool {
        self.node_index.contains(id)
    }

    pub(crate) fn index_of(&self, id: RenderNodeId) -> Option<usize> {
        self.node_index.get(id)
    }

    pub(crate) fn index_for_block(&self, id: BlockId) -> Option<usize> {
        self.node_index
            .get(RenderNodeId::Block(id))
            .or_else(|| self.grouped_block_index_by_id.get(&id).copied())
    }

    pub(crate) fn block_ids_for_node(&self, index: usize) -> Option<Vec<BlockId>> {
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(vec![*id]),
            RenderNode::Group(group) => Some(group.child_ids.clone()),
        }
    }

    pub(crate) fn representative_block_id_for_node(&self, index: usize) -> Option<BlockId> {
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(*id),
            RenderNode::Group(group) => group.child_ids.first().copied(),
        }
    }

    pub(crate) fn node_default_view_state(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        index: usize,
    ) -> Option<ViewState> {
        self.nodes
            .get(index)
            .map(|node| policy.node_default_view_state(history, node))
    }

    pub(crate) fn node_default_view_state_by_id(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        id: RenderNodeId,
    ) -> Option<ViewState> {
        self.index_of(id)
            .and_then(|index| self.node_default_view_state(policy, history, index))
    }

    pub(crate) fn node_key(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        presentation: &TranscriptPresentationState,
        index: usize,
        base_key: LayoutKey,
    ) -> Option<NodeLayoutKey> {
        let view_state = presentation.effective_view_state(policy, self, history, index)?;
        self.node_key_with_view_state(policy, history, index, base_key, view_state)
    }

    pub(crate) fn node_key_with_view_state(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        index: usize,
        base_key: LayoutKey,
        view_state: ViewState,
    ) -> Option<NodeLayoutKey> {
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(LayoutKey {
                width: base_key.width,
                view_state,
                content_hash: history.content_hash(*id),
                sidecar_hash: block_sidecar_hash(history, *id),
            }),
            RenderNode::Group(group) => Some(LayoutKey {
                width: base_key.width,
                view_state,
                content_hash: smelt_core::utils::hash_serializable(&GroupContentKey {
                    id: group.id,
                    name: &group.name,
                    bucket: &group.bucket,
                    group_generation: self.group_generation,
                    group_cache_key: self.group_cache_key,
                    view_state,
                    child_ids: &group.child_ids,
                    child_view_states: group
                        .child_range
                        .clone()
                        .filter_map(|block_index| {
                            let id = *history.order.get(block_index)?;
                            Some(policy.node_default_view_state(
                                history,
                                &RenderNode::Block { id, block_index },
                            ))
                        })
                        .collect(),
                    child_hashes: group
                        .child_range
                        .clone()
                        .filter_map(|block_index| {
                            history
                                .order
                                .get(block_index)
                                .map(|id| history.content_hash(*id))
                        })
                        .collect(),
                }),
                sidecar_hash: group_sidecar_hash(history, group.child_range.clone()),
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
            RenderNode::Group(group) => Some(group.child_range.start),
        }
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = RenderNodeId> + '_ {
        self.nodes.iter().map(RenderNode::id)
    }
}

fn render_plan_fingerprint(
    history: &BlockHistory,
    nodes: &[RenderNode],
    group_generation: u64,
    group_cache_key: Option<u64>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    history.order_generation().hash(&mut hasher);
    history.record_dirty_generation().hash(&mut hasher);
    group_generation.hash(&mut hasher);
    group_cache_key.hash(&mut hasher);
    nodes.len().hash(&mut hasher);
    hasher.finish()
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
    child_view_states: Vec<ViewState>,
    child_hashes: Vec<u64>,
}

fn build_nodes(history: &BlockHistory, groups: &[TranscriptGroupSpec]) -> Vec<RenderNode> {
    build_nodes_from(history, groups, 0)
}

fn build_nodes_from(
    history: &BlockHistory,
    groups: &[TranscriptGroupSpec],
    start: usize,
) -> Vec<RenderNode> {
    if groups.is_empty() {
        let remaining = history.order.len().saturating_sub(start);
        let headroom = if start == 0 {
            TRANSCRIPT_APPEND_HEADROOM
        } else {
            0
        };
        let mut nodes = Vec::with_capacity(remaining.saturating_add(headroom));
        nodes.extend(
            history
                .order
                .iter()
                .copied()
                .enumerate()
                .skip(start)
                .map(|(block_index, id)| RenderNode::Block { id, block_index }),
        );
        return nodes;
    }

    let mut nodes = Vec::new();
    let mut index = start;
    while index < history.order.len() {
        let Some((spec, bucket, end)) = matching_group_run(history, groups, index) else {
            nodes.push(RenderNode::Block {
                id: history.order[index],
                block_index: index,
            });
            index += 1;
            continue;
        };

        let child_ids = history.order[index..end].to_vec();
        let id = smelt_core::utils::hash_serializable(&GroupIdKey {
            name: &spec.name,
            bucket: &bucket,
            first_child: child_ids[0],
        });
        nodes.push(RenderNode::Group(Box::new(RenderGroupNode {
            id,
            name: spec.name.clone(),
            bucket,
            child_range: index..end,
            child_ids,
            default_view_state: view_state(spec.default_view.as_deref()),
        })));
        index = end;
    }
    nodes
}

fn group_append_recompute_start(
    history: &BlockHistory,
    groups: &[TranscriptGroupSpec],
    old_order_len: usize,
) -> usize {
    if old_order_len == 0 || old_order_len >= history.order.len() {
        return old_order_len;
    }
    let mut start = old_order_len;
    while start > 0 && can_share_group_run(history, groups, start - 1, start) {
        start -= 1;
    }
    start
}

fn can_share_group_run(
    history: &BlockHistory,
    groups: &[TranscriptGroupSpec],
    left: usize,
    right: usize,
) -> bool {
    groups.iter().any(|spec| {
        selector_matches(history, spec, left)
            && selector_matches(history, spec, right)
            && group_bucket(history, spec, left).unwrap_or_else(|| spec.name.clone())
                == group_bucket(history, spec, right).unwrap_or_else(|| spec.name.clone())
    })
}

fn matching_group_run<'a>(
    history: &BlockHistory,
    groups: &'a [TranscriptGroupSpec],
    start: usize,
) -> Option<(&'a TranscriptGroupSpec, String, usize)> {
    groups.iter().find_map(|spec| {
        if !selector_matches(history, spec, start) {
            return None;
        }
        let bucket = group_bucket(history, spec, start).unwrap_or_else(|| spec.name.clone());
        let mut end = start + 1;
        while end < history.order.len()
            && selector_matches(history, spec, end)
            && group_bucket(history, spec, end).unwrap_or_else(|| spec.name.clone()) == bucket
        {
            end += 1;
        }
        (end - start >= spec.min).then_some((spec, bucket, end))
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
    if spec
        .selector
        .kind
        .as_deref()
        .is_some_and(|kind| history.block_kind(id) != Some(kind))
    {
        return false;
    }
    if spec
        .selector
        .name
        .as_deref()
        .is_some_and(|name| history.tool_name(id) != Some(name))
    {
        return false;
    }
    if !spec.selector.names.is_empty()
        && history.tool_name(id).is_none_or(|name| {
            !spec
                .selector
                .names
                .iter()
                .any(|candidate| candidate == name)
        })
    {
        return false;
    }
    if let Some(terminal) = spec.selector.terminal {
        let terminal_state = history
            .tool_call_id(id)
            .and_then(|call_id| history.tool_status(call_id))
            .is_some_and(|status| {
                matches!(
                    status,
                    smelt_core::ToolStatus::Ok
                        | smelt_core::ToolStatus::Err
                        | smelt_core::ToolStatus::Denied
                )
            });
        if terminal_state != terminal {
            return false;
        }
    }
    for field in &spec.selector.fields {
        if block_field(history, block_index, &field.field).as_deref() != Some(field.value.as_str())
        {
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
    match field {
        "kind" => history.block_kind(id).map(str::to_string),
        "name" => history.tool_name(id).map(str::to_string),
        "status" if history.is_tool_draft(id) => Some("drafting".to_string()),
        "status" => history
            .tool_call_id(id)
            .and_then(|call_id| history.tool_status(call_id))
            .map(|status| status.label().to_string()),
        "event" | "event_type" | "process_id" | "exit_code" => history.process_field(id, field),
        field => field
            .strip_prefix("args.")
            .and_then(|arg| history.arg_field(id, arg).map(json_bucket_value)),
    }
}

fn json_bucket_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn group_sidecar_hash(history: &BlockHistory, child_range: Range<usize>) -> u64 {
    hash_values(
        child_range
            .filter_map(|block_index| history.order.get(block_index).copied())
            .map(|id| block_sidecar_hash(history, id)),
    )
}

fn hash_values(values: impl IntoIterator<Item = u64>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for value in values {
        value.hash(&mut hasher);
    }
    hasher.finish()
}

fn block_sidecar_hash(history: &BlockHistory, id: BlockId) -> u64 {
    history.sidecar_hash(id)
}

#[cfg(test)]
fn tool_name(block: &smelt_core::transcript_model::Block) -> Option<&str> {
    match block {
        smelt_core::transcript_model::Block::ToolDraft { name, .. }
        | smelt_core::transcript_model::Block::ToolCall { name, .. } => Some(name),
        _ => None,
    }
}

#[cfg(test)]
fn block_kind(block: &smelt_core::transcript_model::Block) -> &'static str {
    block.kind()
}

fn view_state(value: Option<&str>) -> ViewState {
    match value {
        Some("collapsed") => ViewState::Collapsed,
        Some("peek") => ViewState::Peek,
        Some("trimmed_head") => ViewState::TrimmedHead { keep: 1 },
        Some("trimmed_tail") => ViewState::TrimmedTail { keep: 1 },
        _ => ViewState::Expanded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::transcript_model::Block;

    #[test]
    fn default_view_policy_peeks_thinking_blocks() {
        let policy = TranscriptDefaultViewPolicy::default();
        assert_eq!(
            policy.block_default_view_state(&Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "preview".into(),
            }),
            ViewState::Peek
        );
        assert_eq!(
            policy.block_default_view_state(&Block::Compacted {
                summary: "checkpoint".into(),
            }),
            ViewState::Collapsed
        );
        assert_eq!(
            policy.block_default_view_state(&Block::CompactionPreview {
                summary: "streaming".into(),
            }),
            ViewState::Peek
        );
        assert_eq!(
            policy.block_default_view_state(&Block::Text {
                content: "shown".into()
            }),
            ViewState::Expanded
        );
        for tool in [
            "load_skill",
            "read_file",
            "grep",
            "glob",
            "web_fetch",
            "enter_worktree",
        ] {
            assert_eq!(
                policy.block_default_view_state(&Block::ToolCall {
                    call_id: format!("call-{tool}"),
                    name: tool.into(),
                    summary: protocol::StyledLines::default(),
                    args: HashMap::new(),
                }),
                ViewState::Collapsed
            );
        }
        for tool in ["write_file", "edit_file", "edit_notebook"] {
            assert_eq!(
                policy.block_default_view_state(&Block::ToolCall {
                    call_id: format!("call-{tool}"),
                    name: tool.into(),
                    summary: protocol::StyledLines::default(),
                    args: HashMap::new(),
                }),
                ViewState::Expanded
            );
        }
    }

    #[test]
    fn default_view_policy_specific_tool_overrides_block_kind() {
        let mut policy = TranscriptDefaultViewPolicy::default();
        policy
            .block_kinds
            .insert("tool".to_string(), ViewState::Collapsed);
        policy
            .tools
            .insert("read_file".to_string(), ViewState::Expanded);

        assert_eq!(
            policy.block_default_view_state(&Block::ToolCall {
                call_id: "call".into(),
                name: "read_file".into(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new(),
            }),
            ViewState::Expanded
        );
    }

    #[test]
    fn process_status_typed_fields_match_selectors_and_buckets() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::ProcessStatus {
            text: "Background process 1 finished successfully.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "1",
                Some(0),
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "Background process 2 finished successfully.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "2",
                Some(0),
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "legacy process status".into(),
            event: None,
        });
        let spec = TranscriptGroupSpec {
            name: "background-processes".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("process_status".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: vec![smelt_core::lua::TranscriptGroupFieldMatch {
                    field: "event".into(),
                    value: "background_process_completed".into(),
                }],
            },
            bucket: Some(smelt_core::lua::TranscriptGroupBucket {
                fields: vec!["exit_code".into()],
            }),
        };

        let plan = RenderPlan::for_history_with_groups(&transcript.history, &[spec], 1, None);

        assert_eq!(plan.nodes.len(), 2);
        assert!(matches!(
            &plan.nodes[0],
            RenderNode::Group(group) if group.name == "background-processes" && group.bucket == "0" && group.child_range == (0..2)
        ));
        assert!(matches!(
            plan.nodes[1],
            RenderNode::Block { block_index: 2, .. }
        ));
    }

    #[test]
    fn tool_name_sets_group_mixed_adjacent_tools() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        for name in ["read_file", "grep", "outline", "edit_file"] {
            transcript.push(Block::ToolCall {
                call_id: format!("call-{name}"),
                name: name.into(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new(),
            });
        }
        let spec = TranscriptGroupSpec {
            name: "explore".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: Some("collapsed".into()),
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("tool".into()),
                name: None,
                names: vec!["read_file".into(), "grep".into(), "outline".into()],
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };

        let plan = RenderPlan::for_history_with_groups(&transcript.history, &[spec], 1, None);

        assert!(matches!(
            plan.nodes.as_slice(),
            [RenderNode::Group(group), RenderNode::Block { block_index: 3, .. }]
                if group.name == "explore" && group.child_range == (0..3)
        ));
    }

    #[test]
    fn lower_priority_group_can_match_when_higher_priority_run_is_below_min() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });

        let high_min = TranscriptGroupSpec {
            name: "high-min".into(),
            cache_key: None,
            priority: 10,
            registration_order: 0,
            min: 3,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let low_min = TranscriptGroupSpec {
            name: "low-min".into(),
            cache_key: None,
            priority: 0,
            registration_order: 1,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };

        let plan =
            RenderPlan::for_history_with_groups(&transcript.history, &[high_min, low_min], 1, None);

        assert!(matches!(
            plan.nodes.as_slice(),
            [RenderNode::Group(group)] if group.name == "low-min" && group.child_range == (0..2)
        ));
    }

    #[test]
    fn ungrouped_plan_extends_for_append_without_pruning() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });
        transcript.history.clear_record_dirty();
        let mut plan = RenderPlan::for_history_with_groups(&transcript.history, &[], 0, None);
        let initial_order_generation = plan.history_order_generation;

        transcript.push(Block::Text {
            content: "third".into(),
        });
        let appended = *transcript.history.order.last().unwrap();
        transcript.history.clear_record_dirty();
        let prune_required =
            plan.refresh_for_history_with_groups(&transcript.history, &[], 0, None);

        assert!(!prune_required);
        assert_ne!(plan.history_order_generation, initial_order_generation);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.index_for_block(appended), Some(2));
        assert!(matches!(
            plan.nodes[2],
            RenderNode::Block {
                id,
                block_index: 2
            } if id == appended
        ));
    }

    #[test]
    fn ungrouped_plan_reuses_structure_for_content_update() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        let id = *transcript.history.order.last().unwrap();
        transcript.push(Block::Text {
            content: "second".into(),
        });
        let mut plan = RenderPlan::for_history_with_groups(&transcript.history, &[], 0, None);
        let initial_order_generation = plan.history_order_generation;
        let initial_fingerprint = plan.fingerprint;

        transcript.history.rewrite(
            id,
            Block::Text {
                content: "changed".into(),
            },
        );
        let prune_required =
            plan.refresh_for_history_with_groups(&transcript.history, &[], 0, None);

        assert!(!prune_required);
        assert_eq!(plan.history_order_generation, initial_order_generation);
        assert_eq!(plan.len(), 2);
        assert_ne!(plan.fingerprint, initial_fingerprint);
        assert_eq!(plan.index_for_block(id), Some(0));
    }

    #[test]
    fn grouped_plan_rebuilds_on_append() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });
        let spec = TranscriptGroupSpec {
            name: "assistant-blocks".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let mut plan = RenderPlan::for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );

        transcript.push(Block::Text {
            content: "third".into(),
        });
        let prune_required =
            plan.refresh_for_history_with_groups(&transcript.history, &[spec], 1, None);

        assert!(prune_required);
        assert!(
            matches!(plan.nodes.as_slice(), [RenderNode::Group(group)] if group.child_range == (0..3))
        );
    }

    #[test]
    fn disabling_groups_rebuilds_grouped_plan_as_blocks() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });
        let spec = TranscriptGroupSpec {
            name: "assistant-blocks".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let mut plan = RenderPlan::for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );

        let prune_required =
            plan.refresh_for_history_with_groups(&transcript.history, &[], 2, None);

        assert!(prune_required);
        assert!(matches!(
            plan.nodes.as_slice(),
            [
                RenderNode::Block { block_index: 0, .. },
                RenderNode::Block { block_index: 1, .. }
            ]
        ));
    }

    #[test]
    fn default_view_policy_group_setting_overrides_registration_default() {
        let mut policy = TranscriptDefaultViewPolicy::default();
        policy
            .groups
            .insert("tools".to_string(), ViewState::Expanded);
        let transcript = smelt_core::content::transcript::Transcript::new();
        let node = RenderNode::Group(Box::new(RenderGroupNode {
            id: 1,
            name: "tools".into(),
            bucket: "tools".into(),
            child_range: 0..0,
            child_ids: Vec::new(),
            default_view_state: ViewState::Collapsed,
        }));

        assert_eq!(
            policy.node_default_view_state(&transcript.history, &node),
            ViewState::Expanded
        );
    }
}
