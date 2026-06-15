use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey};
use std::collections::HashMap;

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

pub(crate) type NodeLayoutKey = LayoutKey;

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
    index_by_id: HashMap<RenderNodeId, usize>,
    pub(crate) fingerprint: u64,
}

impl RenderPlan {
    pub(crate) fn empty() -> Self {
        Self {
            history_generation: 0,
            nodes: Vec::new(),
            index_by_id: HashMap::new(),
            fingerprint: 0,
        }
    }

    pub(crate) fn for_history(history: &BlockHistory) -> Self {
        let nodes: Vec<_> = history
            .order
            .iter()
            .copied()
            .enumerate()
            .map(|(block_index, id)| RenderNode::Block { id, block_index })
            .collect();
        let index_by_id = nodes
            .iter()
            .copied()
            .enumerate()
            .map(|(index, node)| (node.id(), index))
            .collect();
        Self {
            history_generation: history.generation(),
            nodes,
            index_by_id,
            fingerprint: history.generation(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn node_id(&self, index: usize) -> Option<RenderNodeId> {
        self.nodes.get(index).copied().map(RenderNode::id)
    }

    pub(crate) fn contains_id(&self, id: RenderNodeId) -> bool {
        self.index_by_id.contains_key(&id)
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
        Some(history.resolve_key(block_id, base_key))
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = RenderNodeId> + '_ {
        self.nodes.iter().copied().map(RenderNode::id)
    }
}
