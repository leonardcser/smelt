//! Scenario generation: `FuzzInput`, swarm weighting, and the op builder table.

use crate::*;

#[derive(Arbitrary, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FuzzMode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl From<FuzzMode> for AgentMode {
    fn from(m: FuzzMode) -> Self {
        match m {
            FuzzMode::Normal => AgentMode::normal(),
            FuzzMode::Plan => AgentMode::parse("plan").unwrap(),
            FuzzMode::Apply => AgentMode::parse("apply").unwrap(),
            FuzzMode::Yolo => AgentMode::parse("yolo").unwrap(),
        }
    }
}

/// A full reproducible scenario: initial app config plus the event stream.
/// `FuzzInput` is also what `arbitrary` decodes from libFuzzer bytes, so a
/// crash artifact converts to a `Scenario` JSON via a single
/// `serde_json::to_string_pretty`. `Arbitrary` selects one named workload
/// profile up front, then samples operations from that profile. JSON
/// serialization round-trips the resulting operations; generator profile
/// state does not need to be persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzInput {
    pub vim: bool,
    pub mode: FuzzMode,
    pub ops: Vec<FuzzOp>,
}

impl<'a> Arbitrary<'a> for FuzzInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let vim = u.arbitrary()?;
        let mode = u.arbitrary()?;
        // One profile byte selects a coherent workload without consuming one
        // byte for every operation variant before any operation can decode.
        let swarm = app_swarm(u)?;
        let mut ops = Vec::new();
        while !u.is_empty() && ops.len() < MAX_OPS {
            let idx = swarm.pick(u)?;
            ops.push(build_fuzz_op(idx, u)?);
        }
        Ok(FuzzInput { vim, mode, ops })
    }
}

/// Fixed per-scenario operation weights. A target selects one named workload
/// profile with a single byte, then uses this table for the whole scenario.
/// Profiles make prerequisites common, isolate coherent subsystems, and leave
/// almost all input entropy for operations and payloads.
pub struct SwarmWeights {
    weights: Vec<u32>,
    total: u32,
}

impl SwarmWeights {
    pub fn profile(n: usize, enabled: &[usize], boosted: &[usize]) -> Self {
        assert!(n > 0, "swarm profiles need at least one variant");
        let mut weights = vec![0u32; n];
        for &index in enabled {
            *weights
                .get_mut(index)
                .unwrap_or_else(|| panic!("enabled swarm index {index} exceeds {n} variants")) = 4;
        }
        for &index in boosted {
            *weights
                .get_mut(index)
                .unwrap_or_else(|| panic!("boosted swarm index {index} exceeds {n} variants")) = 16;
        }
        if weights.iter().all(|weight| *weight == 0) {
            weights.fill(1);
        }
        let total = weights.iter().copied().sum();
        Self { weights, total }
    }

    pub fn uniform(n: usize) -> Self {
        assert!(n > 0, "uniform swarms need at least one variant");
        Self {
            weights: vec![1; n],
            total: n as u32,
        }
    }

    /// Sample a variant index, weighted by the selected profile.
    pub fn pick(&self, u: &mut Unstructured<'_>) -> arbitrary::Result<usize> {
        let pick = u.int_in_range(0u32..=self.total.saturating_sub(1))?;
        let mut acc = 0u32;
        for (i, &weight) in self.weights.iter().enumerate() {
            acc = acc.saturating_add(weight);
            if pick < acc {
                return Ok(i);
            }
        }
        Ok(self.weights.len() - 1)
    }
}

pub(crate) fn app_swarm(u: &mut Unstructured<'_>) -> arbitrary::Result<SwarmWeights> {
    const EDITING: &[usize] = &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 25, 32, 33, 42, 43, 44, 48, 53, 54, 55, 58, 60,
    ];
    const ENGINE: &[usize] = &[
        7, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 34, 35, 36,
        37, 38, 39, 40, 45, 46, 47, 48, 49, 50, 51, 56, 62,
    ];
    const TOOLS: &[usize] = &[10, 16, 17, 18, 28, 29, 30, 31, 34, 38, 39, 49, 52, 61];
    const RELOAD: &[usize] = &[
        0, 3, 7, 8, 9, 10, 25, 37, 40, 41, 42, 43, 44, 45, 46, 47, 48, 53, 54, 55, 56, 57, 58, 59,
        62,
    ];
    const TRANSCRIPT: &[usize] = &[
        6, 7, 9, 10, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 26, 27, 34, 37, 40, 42, 61, 62,
    ];

    let profile = u.arbitrary::<u8>()? % 6;
    Ok(match profile {
        0 => SwarmWeights::uniform(N_FUZZOP_VARIANTS),
        1 => SwarmWeights::profile(N_FUZZOP_VARIANTS, EDITING, &[0, 3, 5, 9, 60]),
        2 => SwarmWeights::profile(N_FUZZOP_VARIANTS, ENGINE, &[10, 12, 13, 37, 56, 62]),
        3 => SwarmWeights::profile(N_FUZZOP_VARIANTS, TOOLS, &[10, 16, 18, 28, 29, 39, 61]),
        4 => SwarmWeights::profile(N_FUZZOP_VARIANTS, RELOAD, &[10, 37, 41, 48, 57, 62]),
        _ => SwarmWeights::profile(N_FUZZOP_VARIANTS, TRANSCRIPT, &[9, 10, 13, 18, 37, 62]),
    })
}

/// Per-variant builder. The Arbitrary impl picks an index into
/// `FUZZOP_BUILDERS` and calls the corresponding closure to mint the
/// variant with random payload. Centralising this as a slice means
/// adding a new variant is exactly one edit: append a closure here.
/// `N_FUZZOP_VARIANTS` derives from `.len()`, so the index space and the
/// dispatch table cannot drift.
pub(crate) type FuzzOpBuilder = fn(&mut Unstructured<'_>) -> arbitrary::Result<FuzzOp>;

pub(crate) const FUZZOP_BUILDERS: &[FuzzOpBuilder] = &[
    |u| Ok(FuzzOp::KeyUnicode(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeyCtrl(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeyShift(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeySpecial(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeySpecialShift(u.arbitrary()?)),
    |u| Ok(FuzzOp::Paste(u.arbitrary()?)),
    |u| Ok(FuzzOp::Mouse(u.arbitrary()?)),
    |u| Ok(FuzzOp::Tick(u.arbitrary()?)),
    |_| Ok(FuzzOp::LuaWakeup),
    |u| {
        Ok(FuzzOp::Resize {
            w: u.arbitrary()?,
            h: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::StartTurn(u.arbitrary()?)),
    |_| Ok(FuzzOp::EngineReady),
    |u| Ok(FuzzOp::EngineText(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineTextDelta(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineThinking(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineThinkingDelta(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineToolStart {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolOutput {
            call_id: u.arbitrary()?,
            chunk: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolFinish {
            call_id: u.arbitrary()?,
            is_error: u.arbitrary()?,
            content: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::ExecOutput(u.arbitrary()?)),
    |u| Ok(FuzzOp::ExecDone(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineTurnError(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineSteered {
            text: u.arbitrary()?,
            count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineRetrying {
            delay_ms: u.arbitrary()?,
            attempt: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineTokenUsage {
            prompt: u.arbitrary()?,
            completion: u.arbitrary()?,
            tps: u.arbitrary()?,
            cost_cents: u.arbitrary()?,
            background: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::PushQueuedMessage(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::JobCompleted {
            id: u.arbitrary()?,
            exit_code: u.arbitrary()?,
            termination: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineMessages {
            msg_count: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ApproveFirstConfirm),
    |u| {
        Ok(FuzzOp::DenyFirstConfirm {
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineCoreToolResult {
            req_id: u.arbitrary()?,
            content: u.arbitrary()?,
            is_error: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolEvaluationRequest {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InsertAttachment {
            label: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::TogglePaneFocus),
    |u| {
        Ok(FuzzOp::EngineToolDraftDelta {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            delta: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskResponse {
            id: u.arbitrary()?,
            content: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskError {
            id: u.arbitrary()?,
            kind_idx: u.arbitrary()?,
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineTurnComplete {
            msg_count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolDispatch {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineRequestPermission {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            summary: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineShutdown {
            reason: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ReloadLua),
    |u| {
        Ok(FuzzOp::OpenOverlay {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InstallPlaceholder {
            text: u.arbitrary()?,
            variant: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ClearPlaceholder),
    |u| {
        Ok(FuzzOp::EngineAskResponsePending {
            content: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskErrorPending {
            kind_idx: u.arbitrary()?,
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartExec {
            command: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::Cancel),
    |u| {
        Ok(FuzzOp::EngineRequestPermissionBash {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            command: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::Steer {
            text: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::Unsteer {
            count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::CallCoreTool {
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::SetAgentMode {
            mode: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InstallPromptCursorTrap {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartCustomCommand {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartEngineAsk {
            question: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ScheduleReload),
    |u| {
        Ok(FuzzOp::ProbePromptCursorAfterTurn {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::ProbeCompactionPrepareRequest {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::MacroPromptEdit {
            text: u.arbitrary()?,
            repetitions: u.arbitrary()?,
            movement: u.arbitrary()?,
            submit: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::MacroToolRoundTrip {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
            output: u.arbitrary()?,
            is_error: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::MacroStreamTurn {
            turn_id: u.arbitrary()?,
            text: u.arbitrary()?,
            thinking: u.arbitrary()?,
            chunks: u.arbitrary()?,
            error: u.arbitrary()?,
        })
    },
];

/// Total `FuzzOp` variant count, derived from the dispatch table so it
/// cannot drift.
pub const N_FUZZOP_VARIANTS: usize = FUZZOP_BUILDERS.len();

/// Build one `FuzzOp` by variant index. Indices outside the table never
/// happen because `Arbitrary` for `FuzzOp` always picks within range.
pub(crate) fn build_fuzz_op(idx: usize, u: &mut Unstructured<'_>) -> arbitrary::Result<FuzzOp> {
    FUZZOP_BUILDERS[idx](u)
}

/// Alias clarifying intent at use sites: on-disk JSON is a `Scenario`,
/// the in-memory fuzz input is a `FuzzInput`. Same bytes either way.
pub type Scenario = FuzzInput;
