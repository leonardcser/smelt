use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum RenderNodeId {
    Block(BlockId),
}

impl RenderNodeId {
    pub(crate) fn as_block_id(self) -> Option<BlockId> {
        match self {
            Self::Block(id) => Some(id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NodeLayoutKey {
    pub(crate) width: u16,
    pub(crate) show_thinking: bool,
    pub(crate) view_state: ViewState,
    pub(crate) content_hash: u64,
    pub(crate) sidecar_hash: u64,
}

impl NodeLayoutKey {
    pub(crate) fn from_block_key(key: LayoutKey) -> Self {
        Self {
            width: key.width,
            show_thinking: key.show_thinking,
            view_state: key.view_state,
            content_hash: key.content_hash,
            sidecar_hash: key.sidecar_hash,
        }
    }

    pub(crate) fn into_block_key(self) -> LayoutKey {
        LayoutKey {
            width: self.width,
            show_thinking: self.show_thinking,
            view_state: self.view_state,
            content_hash: self.content_hash,
            sidecar_hash: self.sidecar_hash,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenderNode {
    Block { id: BlockId, block_index: usize },
}

impl RenderNode {
    pub(crate) fn id(self) -> RenderNodeId {
        match self {
            Self::Block { id, .. } => RenderNodeId::Block(id),
        }
    }

    pub(crate) fn as_block_id(self) -> Option<BlockId> {
        match self {
            Self::Block { id, .. } => Some(id),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RenderPlan {
    pub(crate) history_generation: u64,
    pub(crate) nodes: Vec<RenderNode>,
    pub(crate) fingerprint: u64,
}

impl RenderPlan {
    pub(crate) fn empty() -> Self {
        Self {
            history_generation: 0,
            nodes: Vec::new(),
            fingerprint: 0,
        }
    }

    pub(crate) fn for_history(history: &BlockHistory) -> Self {
        let nodes = history
            .order
            .iter()
            .copied()
            .enumerate()
            .map(|(block_index, id)| RenderNode::Block { id, block_index })
            .collect();
        Self {
            history_generation: history.generation(),
            nodes,
            fingerprint: history.generation(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn node_id(&self, index: usize) -> Option<RenderNodeId> {
        self.nodes.get(index).copied().map(RenderNode::id)
    }

    pub(crate) fn block_id(&self, index: usize) -> Option<BlockId> {
        self.nodes
            .get(index)
            .copied()
            .and_then(RenderNode::as_block_id)
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
        let block_id = self.block_id(index)?;
        Some(NodeLayoutKey::from_block_key(
            history.resolve_key(block_id, base_key),
        ))
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = RenderNodeId> + '_ {
        self.nodes.iter().copied().map(RenderNode::id)
    }
}
