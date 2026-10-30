//! A control surface for the installed, unmodified Claude Code agent.
//!
//! This uses the same subprocess protocol as the official Agent SDK, not
//! Anthropic's HTTP API. Authentication, model requests, context management,
//! and native tool execution stay inside Claude Code.

mod agents;
mod catalog;
mod context;
mod interaction;
mod models;
mod plans;
pub use catalog::smelt_tool;
pub use context::context_window;
pub use models::{cached as cached_models, refresh as refresh_models, stale as models_stale};
pub use plans::directory as plan_directory;
pub use transport::installed;
pub use usage::usage;
mod session;
mod shells;
#[cfg(all(test, unix))]
mod tests;
mod transcript;
mod transport;
mod usage;

use crate::{agent, EngineConfig, EngineEventSender, HostCallSender};
use futures_util::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use protocol::{
    Content, Decision, EngineAskError, EngineAskErrorKind, EngineEvent, HistoryItem,
    ReasoningEffort, StartTurnInput, StartTurnPayload, ToolEvaluation, UiCommand,
};
use serde_json::{json, Value};
use smelt_provider::CancellationToken;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use transcript::Transcript;
use transport::{control, reply, Process};

/// How often running shells' output is read while they run.
const SHELL_READ_INTERVAL: Duration = Duration::from_millis(500);

/// Sleeps until `deadline`, or forever without one.
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct Gate {
    policy_revision: u64,
    control_id: String,
    hook: bool,
    call_id: String,
    tool_name: String,
    args: HashMap<String, Value>,
    in_workspace_root: bool,
}

struct InteractionRequest {
    policy_revision: u64,
    control_id: String,
    native_name: &'static str,
    native_id: Option<String>,
}

struct ToolCall {
    policy_revision: u64,
    control_id: String,
    target: interaction::ReplyTarget,
    call_id: String,
    invocation_id: protocol::InvocationId,
    tool_name: String,
    args: HashMap<String, Value>,
}

/// A smelt turn on the native session: one prompt and Claude Code's answer.
struct OpenTurn {
    id: u64,
    started: std::time::Instant,
    stage: Stage,
}

enum Stage {
    /// Its model history is loading.
    Preparing,
    /// Its prompt waits for Claude Code to be initialized.
    Queued(Vec<Value>),
    /// Claude Code has the prompt with this uuid and takes it once it finishes
    /// what it works on, such as a reply to a monitor event.
    Taking(String),
    Sent,
    /// Claude Code answered with steers still queued, which it answers next.
    Steering,
    /// Claude Code answered; headless turns still wait for its work to settle.
    Answered,
}

impl Stage {
    /// Whether Claude Code has the turn's prompt.
    fn prompted(&self) -> bool {
        matches!(
            self,
            Self::Taking(_) | Self::Sent | Self::Steering | Self::Answered
        )
    }

    /// Whether Claude Code works on the turn's prompt, or answered it.
    fn taken(&self) -> bool {
        matches!(self, Self::Sent | Self::Steering | Self::Answered)
    }
}

/// Claude Code's way to the workspace smelt moved to. It cannot change
/// directory during a turn, so a working turn is interrupted once its tools
/// returned and continued after the move.
enum Move {
    /// Waits for the root agent's tools to return.
    Waiting,
    /// The turn was interrupted to move.
    Interrupted,
    /// Claude Code was asked to move to `path`; `resume` continues the
    /// interrupted turn there.
    Sent { path: PathBuf, resume: bool },
}

enum Preparation {
    Ready(agent::LoadedModelHistory),
    /// The turn failed or was cancelled before its prompt.
    Dropped,
    Ended,
}

/// Everything scoped to one Claude Code process, replaced whole when the
/// process is retired.
struct Native<'a> {
    process: Option<Process>,
    /// The launch settings of the process. A turn asking for others relaunches it.
    launch: Value,
    session_id: Option<String>,
    /// Points the native session can be forked at, and its latest assistant message.
    checkpoints: Vec<session::Checkpoint>,
    last_message: Option<String>,
    plans: plans::Plans,
    init_deadline: Option<tokio::time::Instant>,
    usage: UsageTracker,
    agents: agents::Agents,
    shells: shells::Shells,
    /// When the shells' output is read next.
    next_read: Option<tokio::time::Instant>,
    gates: FuturesUnordered<BoxFuture<'static, (Gate, ToolEvaluation)>>,
    approvals: HashMap<u64, Gate>,
    evaluations: HashMap<u64, ToolCall>,
    executions: HashMap<u64, ToolCall>,
    tool_results: FuturesUnordered<BoxFuture<'a, (ToolCall, crate::tools::ToolResult)>>,
    plan_inputs: FuturesUnordered<BoxFuture<'static, (InteractionRequest, Result<Value, String>)>>,
    active_controls: HashMap<String, Option<protocol::InvocationId>>,
    moving: Option<Move>,
    session_state: Option<String>,
    /// Tasks Claude Code runs in the background. When one ends, it wakes to
    /// report it, or goes idle when the task was stopped.
    background: HashSet<String>,
    awaiting_wake: bool,
    root_in_progress: bool,
    /// The turn was interrupted and Claude Code still owes its result. Root
    /// frames before it belong to the cancelled turn.
    interrupting: bool,
    /// The `working` state last reported to the host, if the process was announced.
    reported: Option<bool>,
}

impl Native<'_> {
    fn new(tx: &EngineEventSender, payload: &StartTurnPayload) -> Self {
        Self {
            process: None,
            launch: Value::Null,
            session_id: None,
            checkpoints: Vec::new(),
            last_message: None,
            plans: plans::Plans::new(Default::default(), None),
            init_deadline: None,
            usage: UsageTracker::default(),
            agents: agents::Agents::new(
                tx.clone(),
                &payload.session_id,
                &payload.model_target.provider,
                payload.ephemeral,
            ),
            shells: shells::Shells::new(&payload.model_target.provider),
            next_read: None,
            gates: FuturesUnordered::new(),
            approvals: HashMap::new(),
            evaluations: HashMap::new(),
            executions: HashMap::new(),
            tool_results: FuturesUnordered::new(),
            plan_inputs: FuturesUnordered::new(),
            active_controls: HashMap::new(),
            moving: None,
            session_state: None,
            background: HashSet::new(),
            awaiting_wake: false,
            root_in_progress: false,
            interrupting: false,
            reported: None,
        }
    }
}

/// One Claude Code process per smelt session. Each turn is another user
/// message on its stdin, so native context, caches, and background agents
/// carry over between turns.
struct Runtime<'a> {
    config: &'a mut EngineConfig,
    dispatcher: &'a dyn crate::tools::ToolDispatcher,
    payload: StartTurnPayload,
    commands: &'a mut mpsc::UnboundedReceiver<UiCommand>,
    tx: &'a EngineEventSender,
    host: &'a HostCallSender,
    client: &'a reqwest::Client,
    bg_cancel: &'a CancellationToken,
    next_command: &'a mut Option<UiCommand>,
    native: Native<'a>,
    catalog: catalog::Catalog,
    mode: protocol::AgentMode,
    model: String,
    effort: ReasoningEffort,
    fast_mode: bool,
    policy_revision: u64,
    turn: Option<OpenTurn>,
    dispatcher_turn: Option<u64>,
    /// The conversation Claude Code holds, as smelt history.
    history: Vec<HistoryItem>,
    transcript: Transcript,
    side_results: FuturesUnordered<BoxFuture<'a, (u64, crate::tools::ToolResult)>>,
    /// Steers waiting for the turn's prompt to be sent.
    deferred: Vec<StartTurnInput>,
    /// Steers Claude Code holds until its next step, by message UUID.
    steers: Vec<(String, HistoryItem)>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session(
    config: &mut EngineConfig,
    dispatcher: &dyn crate::tools::ToolDispatcher,
    payload: StartTurnPayload,
    commands: &mut mpsc::UnboundedReceiver<UiCommand>,
    tx: &EngineEventSender,
    host: &HostCallSender,
    client: &reqwest::Client,
    bg_cancel: &CancellationToken,
    next_command: &mut Option<UiCommand>,
) {
    let mut runtime = Runtime {
        native: Native::new(tx, &payload),
        transcript: Transcript::new(
            tx.clone(),
            payload.model_target.provider.clone(),
            payload.turn_id,
            0,
        ),
        mode: payload.mode.clone(),
        model: payload.model_target.model.clone(),
        effort: payload.reasoning_effort.clone(),
        fast_mode: fast_mode(&payload),
        payload,
        config,
        dispatcher,
        commands,
        tx,
        host,
        client,
        bg_cancel,
        next_command,
        catalog: catalog::Catalog::default(),
        policy_revision: 0,
        turn: None,
        dispatcher_turn: None,
        history: Vec::new(),
        side_results: FuturesUnordered::new(),
        deferred: Vec::new(),
        steers: Vec::new(),
    };
    let result = match runtime.open().await {
        Ok(true) => runtime.drive().await,
        Ok(false) => Ok(()),
        Err(message) => Err(message),
    };
    runtime.end(result).await;
}

impl Runtime<'_> {
    fn session_tx(&self) -> EngineEventSender {
        self.tx.for_native_session(&self.payload.session_id)
    }

    /// Events of the open turn go to the plain channel. Outside a turn they
    /// must be session-scoped to reach the frontend.
    fn live_tx(&self) -> EngineEventSender {
        if self.turn.is_some() {
            self.tx.clone()
        } else {
            self.session_tx()
        }
    }

    /// Whether Claude Code's main agent is working, or owes a wake. It also
    /// works on its own, such as when a monitor event wakes it, and says so
    /// before its first frame. A prompt sent meanwhile waits in its queue.
    fn root_busy(&self) -> bool {
        self.native.root_in_progress
            || self.native.awaiting_wake
            || self
                .native
                .session_state
                .as_deref()
                .is_some_and(|state| state != "idle")
    }

    /// Whether nothing holds the next prompt back. Background tasks don't.
    fn quiet(&self) -> bool {
        !self.root_busy()
            && self.native.active_controls.is_empty()
            && self.native.gates.is_empty()
            && self.native.tool_results.is_empty()
            && self.native.plan_inputs.is_empty()
            && self.side_results.is_empty()
    }

    /// Whether nothing native runs, background tasks included, which need
    /// the process.
    fn settled(&self) -> bool {
        self.quiet() && !self.native.agents.has_live() && self.native.shells.is_empty()
    }

    fn publish_shells(&mut self) {
        if let Some(processes) = self.native.shells.changed() {
            let _ = self.tx.send(EngineEvent::NativeProcesses { processes });
        }
    }

    fn report(&mut self) {
        let working = self.turn.is_some() || !self.quiet();
        if self.native.reported != Some(working) {
            self.native.reported = Some(working);
            let _ = self.tx.send(EngineEvent::NativeSessionState {
                session_id: self.payload.session_id.clone(),
                active: true,
                working,
            });
        }
    }

    /// Opens the turn in `self.payload`: on the running process while it holds
    /// the payload's conversation and launch settings, otherwise on a new one
    /// that resumes or imports it. Returns false when the session should end.
    async fn open(&mut self) -> Result<bool, String> {
        if self.payload.request_config.redact_secrets {
            emit_error(self.tx, "The Claude Code backend cannot redact native tool output before Claude Code sends it. Disable redact_secrets explicitly or use smelt's native agent backend.".into());
            return Ok(self.native.process.is_some());
        }
        if self.payload.mode != self.mode {
            self.policy_revision = self.policy_revision.saturating_add(1);
            self.write(control("smelt-mode", json!({"subtype":"set_permission_mode", "mode":transport::permission_mode(&self.payload.mode)}))).await?;
        }
        if self.payload.model_target.model != self.model {
            self.switch_model(self.payload.model_target.model.clone())
                .await?;
        }
        self.mode = self.payload.mode.clone();
        self.sync_flags().await?;
        self.turn = Some(OpenTurn {
            id: self.payload.turn_id,
            started: self.config.clock.instant_now(),
            stage: Stage::Preparing,
        });
        let loaded = match self.prepare().await? {
            Preparation::Ready(loaded) => loaded,
            Preparation::Dropped => {
                self.turn = None;
                return Ok(self.native.process.is_some());
            }
            Preparation::Ended => return Ok(false),
        };
        let history = loaded.items;
        let index = loaded
            .coordinates
            .canonical_index(protocol::ModelHistoryIndex::new(history.len()))
            .get();
        let setup = (|| {
            Ok::<_, String>((
                prompt_content(&self.payload.input)?,
                catalog::Catalog::new(&self.payload.tools, self.payload.tool_visibility)?,
                plan_directory(&self.payload.sessions_root, &self.payload.session_id)?,
            ))
        })();
        let (mut content, catalog, plans_directory) = match setup {
            Ok(setup) => setup,
            Err(message) => {
                self.turn = None;
                emit_error(self.tx, message);
                return Ok(self.native.process.is_some());
            }
        };
        self.catalog = catalog;
        let prompt = self.payload.native_prompt.take().unwrap_or_else(|| {
            crate::build_native_system_prompt(
                self.config.system_prompt_override.as_deref(),
                self.config.instructions.as_deref(),
                None,
                &[],
            )
        });
        let target = &self.payload.model_target;
        let launch = json!({
            "cwd": self.config.cwd,
            "thinking": !matches!(self.payload.reasoning_effort, ReasoningEffort::Off),
            "prompt": prompt.append,
            "override_prompt": prompt.override_prompt,
            "disallowed_tools": self.catalog.disabled_native,
            "tools": target.config.tool_calling(),
            "ephemeral": self.payload.ephemeral,
            "api_base": target.api_base,
            "api_key": !target.api_key.is_empty(),
            "smelt_tools": self.tool_list(),
        });
        self.history.append(&mut self.transcript.committed);
        let reuse = self.native.process.is_some()
            && launch == self.native.launch
            && session::history_digest(&history) == session::history_digest(&self.history);
        if !reuse {
            self.retire(None).await;
        }
        self.dispatcher.begin_turn(self.payload.turn_id);
        if let Some(previous) = self.dispatcher_turn.replace(self.payload.turn_id) {
            self.dispatcher.end_turn(previous);
        }
        if !reuse {
            let link = (!self.payload.ephemeral)
                .then(|| {
                    session::load(
                        &self.payload.sessions_root,
                        &self.payload.session_id,
                        &self.config.cwd,
                        &history,
                    )
                })
                .flatten();
            // A provider switch or edited history cannot reuse a native
            // transcript. Import its readable context into a fresh native session.
            if link.is_none() && !history.is_empty() {
                content.insert(0, json!({"type":"text", "text":imported_history(&history)}));
            }
            let target = &self.payload.model_target;
            self.native.process = Some(Process::spawn(transport::LaunchOptions {
                cwd: &self.config.cwd,
                target,
                mode: &self.mode,
                effort: &self.payload.reasoning_effort,
                resume: link.as_ref().map(|link| link.native_id.as_str()),
                resume_at: link.as_ref().and_then(|link| link.at.as_deref()),
                prompt: Some(&prompt.append),
                override_prompt: prompt.override_prompt.as_deref(),
                plans_directory: Some(&plans_directory),
                disallowed_tools: &self.catalog.disabled_native,
                tools: target.config.tool_calling(),
                persist_session: !self.payload.ephemeral,
                fast_mode: self.payload.fast_mode,
                schema: None,
            })?);
            self.native.plans = plans::Plans::new(
                plans_directory,
                link.as_ref().and_then(|link| link.plan_path.as_deref()),
            );
            self.native.checkpoints = link.map(|link| link.checkpoints).unwrap_or_default();
            self.native.launch = launch;
            self.native.init_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(60));
            self.write(control(
                "smelt-init",
                json!({"subtype":"initialize", "forwardSubagentText":true,
                    // smelt stops background tasks one by one, so an interrupt spares them.
                    "perTaskStopAffordance":true, "hooks":{
                    "PreToolUse":[{"hookCallbackIds":["smelt-permission"]}]
                }}),
            ))
            .await?;
        }
        self.history = history;
        self.transcript
            .retarget(self.tx.clone(), self.payload.turn_id, index);
        if let Some(turn) = &mut self.turn {
            turn.stage = Stage::Queued(content);
        }
        Ok(true)
    }

    /// Loads the turn's model history, applying commands as they arrive.
    async fn prepare(&mut self) -> Result<Preparation, String> {
        let cancel = CancellationToken::new();
        let history = agent::prepare_model_history(
            std::mem::take(&mut self.payload.history),
            self.payload.sessions_root.clone(),
            self.payload.session_id.clone(),
            cancel.clone(),
        );
        tokio::pin!(history);
        loop {
            tokio::select! {
                result = &mut history => {
                    return Ok(match result {
                        Ok(loaded) => Preparation::Ready(loaded),
                        Err(message) => {
                            let _ = self.tx.send(EngineEvent::TurnPreparationError {
                                turn_id: self.payload.turn_id,
                                session_id: self.payload.session_id.clone(),
                                persistence: self.payload.persistence,
                                message,
                            });
                            Preparation::Dropped
                        }
                    });
                }
                command = self.commands.recv() => {
                    if !self.command(command.unwrap_or(UiCommand::EndNativeSession)).await? {
                        cancel.cancel();
                        return Ok(Preparation::Ended);
                    }
                    if self.turn.is_none() {
                        cancel.cancel();
                        return Ok(Preparation::Dropped);
                    }
                }
            }
        }
    }

    fn background(&self, command: UiCommand) {
        agent::dispatch_background_cmd(
            command,
            &agent::BackgroundCtx {
                config: self.config,
                http_client: self.client,
                dispatcher: self.dispatcher,
                event_tx: self.tx,
                host_tx: self.host,
                bg_cancel: self.bg_cancel.clone(),
            },
        );
    }

    /// Writes to the running process. Without one there is nothing to update:
    /// the next launch starts from the current settings.
    async fn write(&mut self, message: Value) -> Result<(), String> {
        match &mut self.native.process {
            Some(process) => process.write(message).await,
            None => Ok(()),
        }
    }

    /// Switches the running process to `model` from its next request, and
    /// recounts the context, whose reading the old model took.
    async fn switch_model(&mut self, model: String) -> Result<(), String> {
        self.write(control(
            "smelt-model",
            json!({"subtype":"set_model", "model":model}),
        ))
        .await?;
        self.write(control(
            "smelt-context",
            json!({"subtype":"get_context_usage"}),
        ))
        .await?;
        self.model = model;
        Ok(())
    }

    /// Brings the running process's effort and fast mode to the payload's from
    /// its next request. Thinking on or off is a launch setting instead.
    async fn sync_flags(&mut self) -> Result<(), String> {
        let effort = self.payload.reasoning_effort.clone();
        let fast_mode = fast_mode(&self.payload);
        let mut settings = serde_json::Map::new();
        if effort != self.effort
            && !matches!(effort, ReasoningEffort::Off)
            && !matches!(self.effort, ReasoningEffort::Off)
        {
            settings.insert("effortLevel".into(), json!(effort.label()));
        }
        if fast_mode != self.fast_mode {
            settings.insert("fastMode".into(), json!(fast_mode));
        }
        if !settings.is_empty() {
            self.write(control(
                "smelt-flags",
                json!({"subtype":"apply_flag_settings", "settings":settings}),
            ))
            .await?;
        }
        self.effort = effort;
        self.fast_mode = fast_mode;
        Ok(())
    }

    async fn send_prompt(&mut self) -> Result<(), String> {
        let Some(turn) = &mut self.turn else {
            return Ok(());
        };
        if !matches!(turn.stage, Stage::Queued(_)) {
            return Ok(());
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let Stage::Queued(content) =
            std::mem::replace(&mut turn.stage, Stage::Taking(uuid.clone()))
        else {
            unreachable!()
        };
        if !is_continuation(&self.payload.input) {
            self.transcript.append(input_history(&self.payload.input));
        }
        let mut message = user_message(content);
        message["uuid"] = json!(uuid);
        self.write(message).await
    }

    /// Whether the turn's prompt has this uuid and waits to be taken.
    fn taking(&self, uuid: &Value) -> bool {
        self.turn.as_ref().is_some_and(
            |turn| matches!(&turn.stage, Stage::Taking(taking) if uuid == taking.as_str()),
        )
    }

    /// Follows Claude Code taking the prompt. What it did before, such as its
    /// reply to a monitor event, does not answer the turn.
    async fn take(&mut self) -> Result<(), String> {
        if let Some(turn) = &mut self.turn {
            turn.stage = Stage::Sent;
        }
        self.send_steers().await
    }

    /// Sends steers once Claude Code took the prompt. It takes each at its next
    /// step boundary, as smelt's own loop does, and echoes it back then.
    async fn send_steers(&mut self) -> Result<(), String> {
        let Some(turn) = &mut self.turn else {
            return Ok(());
        };
        if self.deferred.is_empty() || !turn.stage.taken() || self.native.moving.is_some() {
            return Ok(());
        }
        // An answered turn continues with Claude Code's answer to the steers.
        turn.stage = Stage::Sent;
        for input in std::mem::take(&mut self.deferred) {
            let uuid = uuid::Uuid::new_v4().to_string();
            let mut message = user_message(user_content(&input.provider_content())?);
            message["priority"] = json!("next");
            message["uuid"] = json!(uuid);
            self.write(message).await?;
            self.steers.push((uuid, input_history(&input)));
        }
        Ok(())
    }

    /// Cancels the open turn and what Claude Code is answering. The process
    /// keeps running, with its shells and background agents, and keeps the
    /// conversation smelt has: the steps it already recorded.
    async fn cancel(&mut self) -> Result<(), String> {
        let answering = self.root_busy()
            || self.turn.as_ref().is_some_and(|turn| {
                matches!(turn.stage, Stage::Taking(_) | Stage::Sent | Stage::Steering)
            });
        // Queued messages go back to the prompt. The interrupt withdraws those
        // Claude Code has, with the events queued meanwhile, which would
        // otherwise wake it at once.
        self.deferred.clear();
        self.steers.clear();
        if answering && !self.native.interrupting {
            self.write(control(
                "smelt-interrupt",
                json!({"subtype":"interrupt", "cancel_queued":true}),
            ))
            .await?;
            self.native.interrupting = true;
        }
        if let Some(Move::Sent { resume, .. }) = &mut self.native.moving {
            *resume = false;
        }
        self.transcript.abandon();
        self.history.append(&mut self.transcript.committed);
        if self.turn.as_ref().is_some_and(|turn| turn.stage.prompted()) && !self.payload.ephemeral {
            // It may have done more than smelt saw, so it is never resumed past.
            session::invalidate(&self.payload.sessions_root, &self.payload.session_id);
        }
        if let Some(turn) = self.turn.take() {
            emit_complete(self.tx, turn.id, turn.started.elapsed(), true);
            self.transcript.route(self.session_tx());
        }
        // The host forgot the session's work state with the turn.
        self.native.reported = None;
        Ok(())
    }

    /// Moves Claude Code to smelt's workspace: at once when it is idle, and
    /// otherwise by interrupting its turn once the root agent's tools returned.
    async fn advance_move(&mut self) -> Result<(), String> {
        if !matches!(self.native.moving, Some(Move::Waiting)) || self.native.init_deadline.is_some()
        {
            return Ok(());
        }
        let working = self.native.root_in_progress
            || self
                .turn
                .as_ref()
                .is_some_and(|turn| matches!(turn.stage, Stage::Sent | Stage::Steering));
        if !working {
            return self.send_cwd(false).await;
        }
        if self.transcript.settled()
            && self.steers.is_empty()
            && self.native.active_controls.is_empty()
            && self.native.gates.is_empty()
            && self.native.tool_results.is_empty()
            && self.native.plan_inputs.is_empty()
        {
            self.write(control("smelt-move", json!({"subtype":"interrupt"})))
                .await?;
            self.native.moving = Some(Move::Interrupted);
        }
        Ok(())
    }

    async fn send_cwd(&mut self, resume: bool) -> Result<(), String> {
        let path = self.config.cwd.clone();
        self.write(control(
            "smelt-cwd",
            json!({"subtype":"set_cwd", "path":path}),
        ))
        .await?;
        self.native.moving = Some(Move::Sent { path, resume });
        Ok(())
    }

    /// Follows Claude Code's answer to a move. It asks to trust a directory it
    /// has not seen, which smelt grants: its user moved the session there.
    async fn moved(&mut self, id: &str, response: &Value) -> Result<(), String> {
        let Some(Move::Sent { path, resume }) = &self.native.moving else {
            return Ok(());
        };
        let (path, resume) = (path.clone(), *resume);
        let answer = &response["response"];
        match answer["status"].as_str() {
            Some("needs_trust") if id == "smelt-cwd" => {
                self.write(control(
                    "smelt-cwd-trusted",
                    json!({"subtype":"set_cwd", "path":path, "trust_accepted":true,
                        "trusted_directory":answer["directory"]}),
                ))
                .await
            }
            Some("ok") if path != self.config.cwd => self.send_cwd(resume).await,
            Some("ok") => {
                self.native.moving = None;
                if !resume {
                    return Ok(());
                }
                // Only Claude Code sees the interruption, so only it is told
                // to carry on.
                self.write(user_message(vec![json!({"type":"text", "text":format!(
                    "Continue where you left off; the workspace moved to {}.",
                    path.display()
                )})]))
                .await
            }
            _ => Err(format!(
                "Claude Code could not move to {}: {}",
                path.display(),
                answer["reason"]
                    .as_str()
                    .or(answer["status"].as_str())
                    .or(response["error"].as_str())
                    .unwrap_or("unknown reason")
            )),
        }
    }

    /// Records the conversation Claude Code acknowledged and links the native
    /// session to it, so a later process can resume it.
    fn commit(&mut self) {
        self.history.append(&mut self.transcript.committed);
        if self.payload.ephemeral || self.native.moving.is_some() {
            return;
        }
        let Some(native_id) = &self.native.session_id else {
            return;
        };
        if let Some(message) = &self.native.last_message {
            let checkpoint = session::Checkpoint::new(&self.history, message.clone());
            if self.native.checkpoints.last() != Some(&checkpoint) {
                self.native.checkpoints.push(checkpoint);
            }
        }
        if let Err(message) = session::save(
            &self.payload.sessions_root,
            &self.payload.session_id,
            native_id,
            &self.config.cwd,
            &self.history,
            self.native.plans.active(),
            &self.native.checkpoints,
        ) {
            let _ = self.tx.send(EngineEvent::RequestAuditError { message });
        }
    }

    fn complete(&mut self) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        self.commit();
        emit_complete(self.tx, turn.id, turn.started.elapsed(), false);
        self.transcript.route(self.session_tx());
    }

    /// Stops the process and forgets everything scoped to it. Unfinished native
    /// work unlinks the session, so it is never resumed with unseen effects.
    async fn retire(&mut self, error: Option<&str>) {
        let Some(mut process) = self.native.process.take() else {
            return;
        };
        self.native.agents.finish(error);
        self.transcript.finish();
        self.steers.clear();
        let prompted = self.turn.as_ref().is_some_and(|turn| turn.stage.prompted());
        if error.is_some() || prompted || !self.quiet() || self.native.moving.is_some() {
            if !self.payload.ephemeral {
                session::invalidate(&self.payload.sessions_root, &self.payload.session_id);
            }
        } else {
            self.commit();
        }
        for invocation_id in self.native.active_controls.values().flatten() {
            let _ = self.session_tx().send(EngineEvent::ToolExecutionCancelled {
                turn_id: self.payload.turn_id,
                invocation_id: *invocation_id,
            });
        }
        // Claude Code starts each shell in a session of its own, which closing
        // the process does not reach, so it stops them first.
        for stop in self.native.shells.stop_all() {
            let _ = process.write(stop).await;
        }
        process.close().await;
        if !self.native.shells.is_empty() {
            let _ = self.tx.send(EngineEvent::NativeProcesses {
                processes: Vec::new(),
            });
        }
        self.native = Native::new(self.tx, &self.payload);
        let _ = self.tx.send(EngineEvent::NativeSessionState {
            session_id: self.payload.session_id.clone(),
            active: false,
            working: false,
        });
    }

    async fn end(mut self, result: Result<(), String>) {
        let error = result.err();
        self.retire(error.as_deref()).await;
        if let Some(turn) = self.turn.take() {
            match error {
                None => emit_complete(self.tx, turn.id, turn.started.elapsed(), true),
                Some(message) => emit_error(self.tx, message),
            }
        } else if let Some(message) = error {
            emit_error(&self.session_tx(), message);
        }
        if let Some(turn_id) = self.dispatcher_turn {
            self.dispatcher.end_turn(turn_id);
        }
    }

    async fn drive(&mut self) -> Result<(), String> {
        loop {
            self.advance_move().await?;
            self.send_steers().await?;
            let interactive = self.payload.tool_visibility == protocol::ToolVisibility::Interactive;
            if self.turn.as_ref().is_some_and(|turn| {
                matches!(turn.stage, Stage::Answered) && (interactive || self.settled())
            }) {
                self.complete();
            }
            if self.native.process.is_none() {
                return Ok(());
            }
            if self.native.init_deadline.is_none()
                && !self.native.interrupting
                && self.native.moving.is_none()
            {
                self.send_prompt().await?;
            }
            self.report();
            self.publish_shells();
            let stop_deadline = self.native.agents.stop_deadline();
            let init_deadline = self.native.init_deadline;
            if self.native.shells.is_empty() {
                self.native.next_read = None;
            } else if self.native.init_deadline.is_none() {
                self.native
                    .next_read
                    .get_or_insert_with(|| tokio::time::Instant::now() + SHELL_READ_INTERVAL);
            }
            let next_read = self.native.next_read;
            // An idle process is released only when its conversation can be
            // resumed; the next turn relaunches it.
            let idle = (self.turn.is_none() && self.settled() && !self.payload.ephemeral)
                .then(|| tokio::time::Instant::now() + self.config.native_idle_timeout);
            tokio::select! {
                biased;
                _ = until(stop_deadline) => {
                    self.native.agents.expire_stops();
                }
                _ = until(next_read) => {
                    self.native.next_read = None;
                    for read in self.native.shells.reads() {
                        self.write(read).await?;
                    }
                }
                command = self.commands.recv() => match command.unwrap_or(UiCommand::EndNativeSession) {
                    UiCommand::StartTurn(payload)
                        if self.turn.is_none()
                            && payload.session_id == self.payload.session_id
                            && smelt_provider::BackendKind::is_claude_code(&payload.model_target.provider_type) =>
                    {
                        self.payload = *payload;
                        if !self.open().await? {
                            return Ok(());
                        }
                    }
                    command => {
                        if !self.command(command).await? {
                            return Ok(());
                        }
                    }
                },
                Some((gate, evaluation)) = self.native.gates.next(), if !self.native.gates.is_empty() => self.gated(gate, evaluation).await?,
                Some((call, result)) = self.native.tool_results.next(), if !self.native.tool_results.is_empty() => {
                    self.tool_reply(call, protocol::ToolOutcome::new(result.content, result.is_error, result.metadata)).await?;
                }
                Some((request, input)) = self.native.plan_inputs.next(), if !self.native.plan_inputs.is_empty() => {
                    self.finish_interaction(request, input).await?;
                }
                Some((request_id, result)) = self.side_results.next(), if !self.side_results.is_empty() => {
                    let _ = self.live_tx().send(EngineEvent::CoreToolResult { request_id, content: result.content, is_error: result.is_error, metadata: result.metadata });
                }
                _ = until(init_deadline) => {
                    return Err("Claude Code initialization timed out. Check `claude --version` and `claude auth login`.".into());
                }
                _ = until(idle) => {
                    self.retire(None).await;
                }
                frame = read(&mut self.native.process) => match frame? {
                    Some(frame) => self.frame(frame).await?,
                    None => return Err(exited(&mut self.native.process).await),
                }
            }
        }
    }

    fn track_background(&mut self, frame: &Value) {
        if frame["type"] != "system" {
            return;
        }
        let task = frame["task_id"].as_str().unwrap_or_default();
        let background = &mut self.native.background;
        match frame["subtype"].as_str() {
            // The full set, so a missed start or end doesn't linger.
            Some("background_tasks_changed") => {
                *background = frame["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|task| task["task_id"].as_str().map(str::to_owned))
                    .collect();
            }
            Some("task_started") if frame["is_backgrounded"] == true => {
                background.insert(task.into());
            }
            Some("task_updated") if frame["patch"]["is_backgrounded"] == true => {
                background.insert(task.into());
            }
            Some("task_notification") if background.remove(task) => {
                self.native.awaiting_wake = true;
            }
            _ => {}
        }
    }

    /// The context's latest size. The turn's result reports it again, so a
    /// reading taken without a model request must replace the last response's.
    fn context_read(&mut self, tokens: u32) {
        self.transcript.context_tokens = Some(tokens);
        let _ = self.live_tx().send(EngineEvent::ContextTokens { tokens });
    }

    /// Reports Claude Code's own compaction. Its boundary carries the context
    /// size the compacted history leaves, which the next reading replaces.
    async fn compaction(&mut self, frame: &Value) -> Result<bool, String> {
        if frame["type"] != "system" {
            return Ok(false);
        }
        let state = match frame["subtype"].as_str() {
            Some("status") if frame["compact_result"] == "failed" => {
                protocol::NativeCompaction::Failed {
                    message: frame["compact_error"].as_str().map(str::to_owned),
                }
            }
            Some("status") if frame["status"] == "compacting" => {
                protocol::NativeCompaction::Started
            }
            Some("compact_boundary") => {
                let tokens = |key: &str| {
                    frame["compact_metadata"][key]
                        .as_u64()
                        .map(|tokens| tokens.min(u32::MAX as u64) as u32)
                };
                let after_tokens = tokens("post_tokens").filter(|&tokens| tokens > 0);
                if let Some(tokens) = after_tokens {
                    self.context_read(tokens);
                }
                // Its figure is an estimate. Claude Code's own count is exact.
                self.write(control(
                    "smelt-context",
                    json!({"subtype":"get_context_usage"}),
                ))
                .await?;
                protocol::NativeCompaction::Finished {
                    before_tokens: tokens("pre_tokens"),
                    after_tokens,
                }
            }
            _ => return Ok(false),
        };
        let _ = self.live_tx().send(EngineEvent::NativeCompaction { state });
        Ok(true)
    }

    async fn frame(&mut self, frame: Value) -> Result<(), String> {
        let now_ms = crate::clock::unix_time_ms(self.config.clock.as_ref());
        if frame["type"] == "system" && frame["subtype"] == "session_state_changed" {
            self.native.session_state = frame["state"].as_str().map(str::to_owned);
            if self.native.session_state.as_deref() == Some("idle") {
                self.native.awaiting_wake = false;
            }
            return Ok(());
        }
        if self.compaction(&frame).await? {
            return Ok(());
        }
        self.track_background(&frame);
        if self.native.shells.frame(&frame, now_ms) {
            for note in self.native.shells.finished() {
                self.transcript.note(HistoryItem::note(note));
            }
            return Ok(());
        }
        if self.native.agents.frame(&frame, now_ms)? {
            return Ok(());
        }
        match frame["type"].as_str() {
            Some("control_response") => {
                if let Some(printed) = self.native.shells.output(&frame["response"]) {
                    self.transcript.printed(&printed.tool_use_id, printed.lines);
                }
                if self.native.init_deadline.is_some()
                    && frame["response"]["request_id"].as_str() == Some("smelt-init")
                {
                    if frame["response"]["subtype"].as_str() == Some("error") {
                        return Err(format!(
                            "Claude Code initialization failed: {}",
                            frame["response"]["error"]
                                .as_str()
                                .unwrap_or("unknown error")
                        ));
                    }
                    models::record(&frame["response"]["response"]["models"]);
                    self.native.init_deadline = None;
                } else if let Some(uuid) = frame["response"]["request_id"]
                    .as_str()
                    .and_then(|id| id.strip_prefix("smelt-unsteer-"))
                {
                    if frame["response"]["response"]["cancelled"] == true {
                        self.steers.retain(|(steer, _)| steer != uuid);
                        if let Some(turn) = &mut self.turn {
                            if matches!(turn.stage, Stage::Steering) && self.steers.is_empty() {
                                turn.stage = Stage::Answered;
                            }
                        }
                    }
                } else if frame["response"]["request_id"].as_str() == Some("smelt-context") {
                    if let Some(tokens) = frame["response"]["response"]["totalTokens"].as_u64() {
                        self.context_read(tokens.min(u32::MAX as u64) as u32);
                    }
                } else if let Some(id @ ("smelt-cwd" | "smelt-cwd-trusted")) =
                    frame["response"]["request_id"].as_str()
                {
                    self.moved(id, &frame["response"]).await?;
                }
            }
            Some("control_request") => self.control_request(&frame).await?,
            Some("control_cancel_request") => {
                if let Some(id) = frame["request_id"].as_str() {
                    if let Some(Some(invocation_id)) = self.native.active_controls.remove(id) {
                        let _ = self.session_tx().send(EngineEvent::ToolExecutionCancelled {
                            turn_id: self.payload.turn_id,
                            invocation_id,
                        });
                    }
                    self.native
                        .approvals
                        .retain(|_, gate| gate.control_id != id);
                    self.native
                        .evaluations
                        .retain(|_, call| call.control_id != id);
                    self.native
                        .executions
                        .retain(|_, call| call.control_id != id);
                }
            }
            // Claude Code reports a move as a result of its own.
            Some("result") if matches!(self.native.moving, Some(Move::Sent { .. })) => {}
            Some("result") if self.native.init_deadline.is_none() => {
                self.native.usage.emit(
                    &self.live_tx(),
                    &frame,
                    self.turn.is_none(),
                    self.transcript.context_tokens,
                );
                let interrupted = matches!(self.native.moving, Some(Move::Interrupted));
                let aborted = interrupted && frame["terminal_reason"] == "aborted_streaming";
                let cancelled = std::mem::take(&mut self.native.interrupting);
                if !aborted
                    && !cancelled
                    && (frame["is_error"].as_bool().unwrap_or(false)
                        || frame["subtype"].as_str() != Some("success"))
                {
                    return Err(result_error(&frame));
                }
                self.native.root_in_progress = false;
                let sent = matches!(
                    self.turn,
                    Some(OpenTurn {
                        stage: Stage::Sent | Stage::Steering,
                        ..
                    })
                );
                self.native.session_id = Some(
                    frame["session_id"]
                        .as_str()
                        .ok_or("Claude Code result has no session ID")?
                        .to_owned(),
                );
                self.transcript.finish();
                if interrupted {
                    // A turn that ended before the interrupt reached it, or
                    // that the user cancelled, has nothing to continue.
                    self.send_cwd(aborted && !cancelled).await?;
                    if aborted {
                        return Ok(());
                    }
                }
                if cancelled {
                    return Ok(());
                }
                match &mut self.turn {
                    Some(turn) if sent && !self.steers.is_empty() => turn.stage = Stage::Steering,
                    Some(turn) if sent => turn.stage = Stage::Answered,
                    _ => self.commit(),
                }
            }
            // Claude Code says when it starts on a queued message, before it
            // echoes the message.
            Some("command_lifecycle") => {
                if frame["state"] == "started" && self.taking(&frame["command_uuid"]) {
                    return self.take().await;
                }
            }
            Some("user") if frame["isReplay"] == true => {
                if self.taking(&frame["uuid"]) {
                    return self.take().await;
                }
                let Some(at) = self
                    .steers
                    .iter()
                    .position(|(uuid, _)| frame["uuid"] == uuid.as_str())
                else {
                    return Ok(());
                };
                let (_, item) = self.steers.remove(at);
                self.transcript.steer(item);
                if let Some(turn) = &mut self.turn {
                    turn.stage = Stage::Sent;
                }
            }
            Some("assistant" | "user" | "stream_event") if self.native.interrupting => {}
            _ => {
                if matches!(frame["type"].as_str(), Some("assistant" | "stream_event")) {
                    self.native.root_in_progress = true;
                }
                if frame["type"] == "assistant" {
                    if let Some(uuid) = frame["uuid"].as_str() {
                        self.native.last_message = Some(uuid.to_owned());
                    }
                }
                self.native.plans.frame(&frame);
                self.transcript.frame(&frame, now_ms);
            }
        }
        Ok(())
    }

    /// Applies a command that does not open a turn. Returns false when the
    /// session should end.
    async fn command(&mut self, command: UiCommand) -> Result<bool, String> {
        match command {
            UiCommand::Cancel => self.cancel().await?,
            UiCommand::EndNativeSession => {
                let _ = self
                    .write(control("smelt-interrupt", json!({"subtype":"interrupt"})))
                    .await;
                return Ok(false);
            }
            UiCommand::StartTurn(payload) => {
                *self.next_command = Some(UiCommand::StartTurn(payload));
                return Ok(false);
            }
            UiCommand::StopAgent {
                session_id,
                agent_id,
            } => {
                if let Some(request) = self.native.agents.stop_request(&session_id, &agent_id) {
                    self.write(request).await?;
                }
            }
            UiCommand::StopNativeProcess { session_id, id }
                if session_id == self.payload.session_id =>
            {
                if let Some(request) = self.native.shells.stop_request(&id) {
                    self.write(request).await?;
                }
            }
            UiCommand::ReadNativeProcess { session_id, id }
                if session_id == self.payload.session_id =>
            {
                if let Some(request) = self.native.shells.read_request(&id) {
                    self.write(request).await?;
                }
            }
            UiCommand::BackgroundNativeProcesses { session_id }
                if session_id == self.payload.session_id =>
            {
                for request in self.native.shells.background_requests() {
                    self.write(request).await?;
                }
            }
            UiCommand::Steer { input } => self.deferred.push(input),
            UiCommand::Unsteer { count } => {
                // The newest steers go: unsent ones here, and sent ones Claude
                // Code drops if it has not taken them yet.
                let unsent = count.min(self.deferred.len());
                self.deferred.truncate(self.deferred.len() - unsent);
                let sent: Vec<_> = self
                    .steers
                    .iter()
                    .rev()
                    .take(count - unsent)
                    .map(|(uuid, _)| uuid.clone())
                    .collect();
                for uuid in sent {
                    self.write(control(
                        &format!("smelt-unsteer-{uuid}"),
                        json!({"subtype":"cancel_async_message", "message_uuid":uuid}),
                    ))
                    .await?;
                }
            }
            UiCommand::PermissionDecision {
                request_id,
                approved,
                message,
            } => {
                if let Some(gate) = self.native.approvals.remove(&request_id) {
                    self.permission_reply(
                        gate,
                        approved,
                        message
                            .as_deref()
                            .unwrap_or("Permission denied by the user"),
                    )
                    .await?;
                }
            }
            UiCommand::ToolEvaluationResponse {
                request_id,
                evaluation,
            } => {
                if let Some(call) = self.native.evaluations.remove(&request_id) {
                    if !self.native.active_controls.contains_key(&call.control_id) {
                        return Ok(true);
                    }
                    if call.policy_revision != self.policy_revision || self.native.moving.is_some()
                    {
                        self.reject_tool(
                            call,
                            "Workspace or permission mode changed; retry the tool",
                        )
                        .await?;
                        return Ok(true);
                    }
                    match evaluation.decision {
                        Decision::Allow => self.execute_tool(call),
                        Decision::Ask => {
                            // Lua tools have their own normal smelt permission flow. Use
                            // the same request ID for execution after approval.
                            let gate = Gate {
                                policy_revision: call.policy_revision,
                                control_id: call.control_id.clone(),
                                hook: false,
                                call_id: call.call_id.clone(),
                                tool_name: call.tool_name.clone(),
                                args: call.args.clone(),
                                in_workspace_root: true,
                            };
                            self.request_approval(request_id, gate, evaluation.metadata);
                            self.native.executions.insert(request_id, call);
                        }
                        Decision::Deny | Decision::Error(_) => {
                            self.reject_tool(call, "Tool blocked by smelt permission policy")
                                .await?
                        }
                    }
                }
            }
            UiCommand::ToolResult {
                request_id,
                content,
                is_error,
                metadata,
                display_content,
                attachment,
                ..
            } => {
                if let Some(call) = self.native.executions.remove(&request_id) {
                    self.tool_reply(
                        call,
                        protocol::ToolOutcome::from_parts(
                            content,
                            is_error,
                            metadata,
                            display_content,
                            attachment,
                        ),
                    )
                    .await?;
                }
            }
            UiCommand::UpdateAgentProjectContext(context) => {
                self.policy_revision = self.policy_revision.saturating_add(1);
                if self.config.cwd != context.cwd && self.native.process.is_some() {
                    self.native.moving.get_or_insert(Move::Waiting);
                    // The process is bound for the new workspace, so a turn
                    // there keeps it.
                    self.native.launch["cwd"] = json!(context.cwd);
                }
                self.config.install_project_context(&context);
            }
            UiCommand::SetMode { mode } if mode != self.mode => {
                self.policy_revision = self.policy_revision.saturating_add(1);
                self.write(control("smelt-mode", json!({"subtype":"set_permission_mode", "mode":transport::permission_mode(&mode)}))).await?;
                self.mode = mode;
            }
            UiCommand::SetTurnModel { target, .. } => {
                if !smelt_provider::BackendKind::is_claude_code(&target.provider_type) {
                    return Err("Changing agent backends requires a new turn. Cancel this turn before switching models.".into());
                }
                self.switch_model(target.model.clone()).await?;
                self.payload.model_target = *target;
                self.sync_flags().await?;
            }
            UiCommand::SetReasoningEffort { effort } => {
                // Thinking on or off needs a relaunch, which the next turn makes.
                let thinking = |effort: &ReasoningEffort| !matches!(effort, ReasoningEffort::Off);
                if thinking(&effort) == thinking(&self.effort) {
                    self.payload.reasoning_effort = effort;
                    self.sync_flags().await?;
                }
            }
            UiCommand::SetFastMode { enabled } => {
                self.payload.fast_mode = enabled;
                self.sync_flags().await?;
            }
            UiCommand::CallCoreTool {
                request_id,
                tool_name,
                args,
                ..
            } => {
                if let Some(future) =
                    self.dispatcher
                        .dispatch(&tool_name, args, &crate::tools::ToolContext)
                {
                    self.side_results
                        .push(Box::pin(async move { (request_id, future.await) }));
                } else {
                    let _ = self.live_tx().send(EngineEvent::CoreToolResult {
                        request_id,
                        content: format!("tool not found: {tool_name}"),
                        is_error: true,
                        metadata: None,
                    });
                }
            }
            UiCommand::AppendHistoryItem { .. } if self.turn.is_none() => {}
            UiCommand::AppendHistoryItem { append } => {
                self.deferred
                    .push(StartTurnInput::note(protocol::HistoryNote::context(
                        serde_json::to_string(&append.item).unwrap_or_default(),
                    )));
            }
            other => self.background(other),
        }
        Ok(true)
    }

    async fn control_request(&mut self, frame: &Value) -> Result<(), String> {
        let id = frame["request_id"]
            .as_str()
            .ok_or("Claude Code control request has no ID")?;
        let request = &frame["request"];
        match request["subtype"].as_str() {
            Some("hook_callback")
                if request["callback_id"].as_str() == Some("smelt-permission") =>
            {
                let input = &request["input"];
                let name = input["tool_name"]
                    .as_str()
                    .ok_or("Claude Code permission hook has no tool name")?;
                if name.starts_with("mcp__smelt__")
                    || (matches!(name, "AskUserQuestion" | "ExitPlanMode")
                        && !self.catalog.owns(name))
                {
                    // Defer interactive tools to can_use_tool so the native agent
                    // receives updated input from the host dialog.
                    self.write(reply(id, json!({}))).await?;
                    return Ok(());
                }
                self.track_control(id)?;
                self.native_gate(
                    id,
                    true,
                    name,
                    &input["tool_input"],
                    request["tool_use_id"]
                        .as_str()
                        .or(input["tool_use_id"].as_str()),
                    input["cwd"].as_str(),
                );
            }
            Some("can_use_tool") => {
                let name = request["tool_name"]
                    .as_str()
                    .ok_or("Claude Code permission request has no tool name")?;
                if name.starts_with("mcp__smelt__") {
                    self.write(reply(
                        id,
                        json!({"behavior":"allow", "updatedInput":request["input"]}),
                    ))
                    .await?;
                } else if matches!(name, "AskUserQuestion" | "ExitPlanMode")
                    && !self.catalog.owns(name)
                {
                    self.native_interaction(
                        id,
                        name,
                        &request["input"],
                        request["tool_use_id"].as_str(),
                    )
                    .await?;
                } else {
                    self.track_control(id)?;
                    self.native_gate(
                        id,
                        false,
                        name,
                        &request["input"],
                        request["tool_use_id"].as_str(),
                        request["cwd"].as_str(),
                    );
                }
            }
            Some("mcp_message") if request["server_name"].as_str() == Some("smelt") => {
                self.mcp_request(id, &request["message"]).await?;
            }
            _ => {
                self.write(json!({"type":"control_response", "response":{
                    "subtype":"error", "request_id":id, "error":"Unsupported Claude Code control request"
                }})).await?;
            }
        }
        Ok(())
    }

    fn enter_plan_mode(&mut self) {
        if self.mode != protocol::AgentMode::parse("plan").expect("valid built-in mode") {
            self.mode = protocol::AgentMode::parse("plan").expect("valid built-in mode");
            self.policy_revision = self.policy_revision.saturating_add(1);
            let _ = self.live_tx().send(EngineEvent::NativeModeChanged {
                turn_id: self.payload.turn_id,
                mode: self.mode.clone(),
            });
        }
    }

    async fn native_interaction(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        native_id: Option<&str>,
    ) -> Result<(), String> {
        let native_name = match name {
            "AskUserQuestion" => "AskUserQuestion",
            "ExitPlanMode" => "ExitPlanMode",
            _ => unreachable!(),
        };
        self.track_control(id)?;
        let request = InteractionRequest {
            policy_revision: self.policy_revision,
            control_id: id.into(),
            native_name,
            native_id: native_id.map(str::to_owned),
        };
        if name == "ExitPlanMode" {
            let input = self.native.plans.input(input.clone());
            self.native
                .plan_inputs
                .push(Box::pin(async move { (request, input.await) }));
            Ok(())
        } else {
            self.finish_interaction(request, Ok(input.clone())).await
        }
    }

    async fn finish_interaction(
        &mut self,
        request: InteractionRequest,
        input: Result<Value, String>,
    ) -> Result<(), String> {
        if !self
            .native
            .active_controls
            .contains_key(&request.control_id)
        {
            return Ok(());
        }
        let tool_name = if request.native_name == "AskUserQuestion" {
            "ask_user_question"
        } else {
            "present_plan"
        };
        let parsed: Result<_, String> = (|| {
            if self.native.moving.is_some() || request.policy_revision != self.policy_revision {
                return Err("Workspace or permission mode changed; retry the interaction".into());
            }
            if !self.payload.tools.iter().any(|tool| {
                tool.name == tool_name && self.payload.tool_visibility.includes(tool.headless)
            }) {
                return Err(
                    "The smelt interaction adapter is unavailable; do not assume user approval"
                        .into(),
                );
            }
            let input = input?;
            let (target, args) = if request.native_name == "AskUserQuestion" {
                interaction::ReplyTarget::question(&input)?
            } else {
                interaction::ReplyTarget::plan(&input)?
            };
            Ok((input, target, args))
        })();
        let (input, target, args) = match parsed {
            Ok(parsed) => parsed,
            Err(message) => {
                self.native.active_controls.remove(&request.control_id);
                return self
                    .write(reply(&request.control_id, interaction::deny(&message)))
                    .await;
            }
        };
        if request.native_name == "ExitPlanMode" {
            self.enter_plan_mode();
        }
        let (call_id, invocation_id) = if let Some(id) = request.native_id {
            let invocation = self
                .native
                .agents
                .transcript_for_call(&id)
                .unwrap_or(&mut self.transcript)
                .invocation(&id);
            (id, invocation)
        } else {
            let native_args = input
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect();
            match self.native.agents.claim_tool(
                &mut self.transcript,
                request.native_name,
                &native_args,
            ) {
                Ok(call) => call,
                Err(message) => {
                    self.native.active_controls.remove(&request.control_id);
                    return self
                        .write(reply(&request.control_id, interaction::deny(&message)))
                        .await;
                }
            }
        };
        self.request_evaluation(ToolCall {
            policy_revision: self.policy_revision,
            control_id: request.control_id,
            target,
            call_id,
            invocation_id,
            tool_name: tool_name.into(),
            args,
        });
        Ok(())
    }

    fn request_evaluation(&mut self, call: ToolCall) {
        self.native
            .active_controls
            .insert(call.control_id.clone(), Some(call.invocation_id));
        let request_id = agent::next_request_id();
        let _ = self.session_tx().send(EngineEvent::ToolEvaluationRequest {
            request_id,
            invocation_id: call.invocation_id,
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            args: call.args.clone(),
            mode: self.mode.clone(),
        });
        self.native.evaluations.insert(request_id, call);
    }

    fn track_control(&mut self, id: &str) -> Result<(), String> {
        if self.native.active_controls.len()
            + self.native.gates.len()
            + self.native.tool_results.len()
            + self.native.plan_inputs.len()
            >= 256
        {
            return Err("Claude Code exceeded the pending control request limit".into());
        }
        if self
            .native
            .active_controls
            .insert(id.to_owned(), None)
            .is_some()
        {
            return Err("Claude Code reused a pending control request ID".into());
        }
        Ok(())
    }

    fn native_gate(
        &mut self,
        id: &str,
        hook: bool,
        name: &str,
        input: &Value,
        call_id: Option<&str>,
        cwd: Option<&str>,
    ) {
        let gate = Gate {
            policy_revision: self.policy_revision,
            control_id: id.into(),
            hook,
            call_id: call_id.unwrap_or(id).into(),
            tool_name: name.into(),
            args: input
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            in_workspace_root: cwd.is_none_or(|cwd| std::path::Path::new(cwd) == self.config.cwd),
        };
        if self.catalog.owns(name) {
            self.native
                .gates
                .push(Box::pin(async move { (gate, ToolEvaluation::deny()) }));
            return;
        }
        let (reply, rx) = oneshot::channel();
        let call = crate::HostCall::NativeToolEvaluation {
            turn_id: self.payload.turn_id,
            session_id: self.payload.session_id.clone(),
            cwd: cwd
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| self.config.cwd.clone()),
            tool_name: name.into(),
            args: gate.args.clone(),
            mode: self.mode.clone(),
            overrides: self.payload.permission_overrides.clone(),
            reply,
        };
        let _ = self.host.send(call);
        self.native.gates.push(Box::pin(async move {
            let evaluation = rx.await.unwrap_or_else(|_| ToolEvaluation::deny());
            (gate, evaluation)
        }));
    }

    async fn gated(&mut self, gate: Gate, evaluation: ToolEvaluation) -> Result<(), String> {
        if !self.native.active_controls.contains_key(&gate.control_id) {
            return Ok(());
        }
        if gate.policy_revision != self.policy_revision || self.native.moving.is_some() {
            return self
                .permission_reply(
                    gate,
                    false,
                    "Workspace or permission mode changed; retry the tool",
                )
                .await;
        }
        match evaluation.decision {
            Decision::Allow => {
                self.permission_reply(gate, true, "Allowed by smelt policy")
                    .await
            }
            Decision::Ask => {
                self.request_approval(agent::next_request_id(), gate, evaluation.metadata);
                Ok(())
            }
            Decision::Deny => {
                self.permission_reply(gate, false, "Blocked by smelt policy")
                    .await
            }
            Decision::Error(message) => self.permission_reply(gate, false, &message).await,
        }
    }

    fn request_approval(&mut self, id: u64, gate: Gate, metadata: protocol::ToolMetadata) {
        let invocation_id = self
            .native
            .agents
            .transcript_for_call(&gate.call_id)
            .unwrap_or(&mut self.transcript)
            .invocation(&gate.call_id);
        self.native
            .active_controls
            .insert(gate.control_id.clone(), Some(invocation_id));
        // Hosts recheck a request against their own workspace root, so it is
        // presented as a smelt tool only when Claude Code resolved it there.
        let (tool_name, args, native) = if gate.in_workspace_root {
            transcript::presented(&gate.tool_name, gate.args.clone())
        } else {
            (gate.tool_name.clone(), gate.args.clone(), true)
        };
        let _ = self.session_tx().send(EngineEvent::RequestPermission {
            request_id: id,
            invocation_id,
            call_id: gate.call_id.clone(),
            tool_name,
            provider: native.then(|| self.payload.model_target.provider.clone()),
            args,
            approval_patterns: metadata.approval_patterns,
            called_at_ms: crate::clock::unix_time_ms(self.config.clock.as_ref()),
            summary: metadata.summary,
        });
        self.native.approvals.insert(id, gate);
    }

    async fn permission_reply(
        &mut self,
        gate: Gate,
        approved: bool,
        message: &str,
    ) -> Result<(), String> {
        if !self.native.active_controls.contains_key(&gate.control_id) {
            return Ok(());
        }
        let approved = approved
            && gate.policy_revision == self.policy_revision
            && self.native.moving.is_none();
        if let Some(id) = self
            .native
            .executions
            .iter()
            .find_map(|(id, call)| (call.control_id == gate.control_id).then_some(*id))
        {
            let call = self.native.executions.remove(&id).expect("located call");
            if approved {
                self.execute_tool(call);
                return Ok(());
            }
            return self.reject_tool(call, message).await;
        }
        if approved {
            self.native
                .plans
                .register_write(&gate.call_id, &gate.tool_name, &gate.args);
        }
        if approved && gate.tool_name == "EnterPlanMode" {
            self.enter_plan_mode();
        }
        let data = if gate.hook {
            json!({"hookSpecificOutput": {"hookEventName":"PreToolUse", "permissionDecision": if approved {"allow"} else {"deny"}, "permissionDecisionReason":message}})
        } else if approved {
            json!({"behavior":"allow", "updatedInput":gate.args})
        } else {
            json!({"behavior":"deny", "message":message})
        };
        self.native.active_controls.remove(&gate.control_id);
        self.write(reply(&gate.control_id, data)).await
    }

    async fn mcp_request(&mut self, control_id: &str, message: &Value) -> Result<(), String> {
        if message.get("id").is_none() {
            return self.write(reply(control_id, json!({}))).await;
        }
        let id = message["id"].clone();
        let result = match message["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":"2024-11-05", "capabilities":{"tools":{}}, "serverInfo":{"name":"smelt", "version":env!("CARGO_PKG_VERSION")}})
            }
            Some("tools/list") => {
                json!({"tools":self.tool_list()})
            }
            Some("tools/call") => {
                let name = message["params"]["name"]
                    .as_str()
                    .ok_or("smelt MCP call has no tool name")?;
                self.track_control(control_id)?;
                let args: HashMap<_, _> = message["params"]["arguments"]
                    .as_object().cloned().unwrap_or_default().into_iter().collect();
                let (call_id, invocation_id) = match self.native.agents.claim_tool(&mut self.transcript, name, &args) {
                    Ok(call) => call,
                    Err(message) => {
                        self.native.active_controls.remove(control_id);
                        let response = interaction::ReplyTarget::Mcp(id).response(&protocol::ToolOutcome::new(message, true, None));
                        return self.write(reply(control_id, response)).await;
                    }
                };
                self.native.active_controls.insert(control_id.into(), Some(invocation_id));
                let call = ToolCall {
                    policy_revision: self.policy_revision,
                    control_id: control_id.into(),
                    target: interaction::ReplyTarget::Mcp(id),
                    call_id,
                    invocation_id,
                    tool_name: name.into(),
                    args,
                };
                if self.native.moving.is_some() {
                    return self.reject_tool(call, "Workspace changed; retry the tool").await;
                }
                if self.payload.tools.iter().any(|tool| tool.name == name && !self.catalog.exposes(tool, &self.mode)) {
                    return self.reject_tool(call, "Tool is native-owned or hidden in this mode").await;
                }
                let request_id = agent::next_request_id();
                if self.payload.tools.iter().any(|tool| tool.name == name && self.catalog.exposes(tool, &self.mode)) {
                    self.request_evaluation(call);
                } else if let Some(evaluation) = self.dispatcher.evaluate_tool_call(
                    self.payload.turn_id,
                    name,
                    &call.args,
                    self.mode.clone(),
                    self.payload.permission_overrides.as_ref(),
                ) {
                    self.native.evaluations.insert(request_id, call);
                    self.command(UiCommand::ToolEvaluationResponse {
                            request_id,
                            evaluation,
                        },
                    )
                    .await?;
                } else {
                    self.reject_tool(call,
                        "Tool is unavailable in this mode",
                    )
                    .await?;
                }
                return Ok(());
            }
            Some("ping") => json!({}),
            _ => return self.write(reply(control_id, json!({"mcp_response":{
                "jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"Method not found"}
            }}))).await,
        };
        self.write(reply(
            control_id,
            json!({"mcp_response":{"jsonrpc":"2.0", "id":id, "result":result}}),
        ))
        .await
    }

    /// smelt tools the native agent sees through the "smelt" MCP server.
    fn tool_list(&self) -> Vec<Value> {
        let mut tools: Vec<Value> = self.payload.tools.iter().filter(|tool| self.catalog.exposes(tool, &self.mode)).map(|tool| json!({"name":tool.name,"description":tool.description,"inputSchema":tool.parameters})).collect();
        tools.extend(self.dispatcher.definitions().into_iter().filter(|tool| !self.payload.tools.iter().any(|lua| lua.name == tool.function.name) && self.dispatcher.is_visible(self.payload.turn_id, &tool.function.name, self.mode.clone())).map(|tool| json!({"name":tool.function.name,"description":tool.function.description,"inputSchema":tool.function.parameters})));
        tools
    }

    fn execute_tool(&mut self, call: ToolCall) {
        let request_id = agent::next_request_id();
        if self
            .payload
            .tools
            .iter()
            .any(|tool| tool.name == call.tool_name)
        {
            let _ = self.session_tx().send(EngineEvent::ToolDispatch {
                request_id,
                invocation_id: call.invocation_id,
                call_id: call.call_id.clone(),
                tool_name: call.tool_name.clone(),
                args: call.args.clone(),
            });
            self.native.executions.insert(request_id, call);
        } else if let Some(future) = self.dispatcher.dispatch(
            &call.tool_name,
            call.args.clone(),
            &crate::tools::ToolContext,
        ) {
            self.native
                .tool_results
                .push(Box::pin(async move { (call, future.await) }));
        } else {
            self.native.tool_results.push(Box::pin(async move {
                (
                    call,
                    crate::tools::ToolResult::err("Tool is no longer available"),
                )
            }));
        }
    }

    async fn reject_tool(&mut self, call: ToolCall, message: &str) -> Result<(), String> {
        self.tool_reply(call, protocol::ToolOutcome::new(message.into(), true, None))
            .await
    }

    async fn tool_reply(
        &mut self,
        call: ToolCall,
        result: protocol::ToolOutcome,
    ) -> Result<(), String> {
        if self
            .native
            .active_controls
            .remove(&call.control_id)
            .is_none()
        {
            return Ok(());
        }
        let data = call.target.response(&result);
        self.native
            .agents
            .transcript_for_call(&call.call_id)
            .unwrap_or(&mut self.transcript)
            .retain_result(&call.call_id, result);
        self.write(reply(&call.control_id, data)).await
    }
}

/// Whether a turn runs in fast mode: only on models that offer it.
fn fast_mode(payload: &StartTurnPayload) -> bool {
    payload.fast_mode && payload.model_target.config.supports_fast_mode == Some(true)
}

fn input_history(input: &StartTurnInput) -> HistoryItem {
    match input {
        StartTurnInput::User {
            content,
            display,
            command,
            sent_at_ms,
            image_placement,
        } => HistoryItem::User {
            content: content.clone(),
            display: display.clone(),
            command: *command,
            sent_at_ms: *sent_at_ms,
            image_placement: image_placement.clone(),
        },
        StartTurnInput::Note { note } => HistoryItem::note(note.clone()),
    }
}

/// Bytes of imported conversation; older turns beyond it are omitted.
const IMPORT_LIMIT: usize = 200_000;
/// Characters kept from each tool call's arguments and result.
const IMPORT_TOOL_CHARS: usize = 2_000;

/// The conversation as a readable transcript for a native session that cannot
/// resume it, keeping its most recent part.
fn imported_history(history: &[HistoryItem]) -> String {
    let clip = |text: &str| -> String {
        let mut chars = text.chars();
        let kept: String = chars.by_ref().take(IMPORT_TOOL_CHARS).collect();
        if chars.next().is_some() {
            format!("{kept}…")
        } else {
            kept
        }
    };
    let mut entries = Vec::new();
    for item in history {
        match item {
            HistoryItem::User { content, .. } => {
                entries.push(format!("User: {}", content.text_content()));
            }
            HistoryItem::Assistant(step) => {
                if let Some(content) = &step.content {
                    entries.push(format!("Assistant: {}", content.text_content()));
                }
                for tool in &step.invocations {
                    entries.push(format!(
                        "Assistant called {} {}\n{}: {}",
                        tool.name,
                        clip(&tool.arguments),
                        if tool.result.is_error {
                            "Error"
                        } else {
                            "Result"
                        },
                        clip(&tool.result.content),
                    ));
                }
            }
            HistoryItem::Note(note) => entries.push(format!("Note: {}", note.to_model_text())),
            HistoryItem::System { content } => {
                entries.push(format!("System: {}", content.text_content()));
            }
        }
    }
    let mut size = 0;
    let kept = entries
        .iter()
        .rev()
        .take_while(|entry| {
            size += entry.len() + 2;
            size <= IMPORT_LIMIT
        })
        .count();
    let mut text = String::from("Previous conversation, imported from smelt. It is historical context, not a request to repeat tool execution.\n\n");
    if kept < entries.len() {
        text.push_str("[Earlier conversation omitted]\n\n");
    }
    text.push_str(&entries[entries.len() - kept..].join("\n\n"));
    text.push_str("\n\nEnd of previous conversation.");
    text
}

/// Text Claude Code receives to continue a turn that has no input of its own.
const CONTINUE_PROMPT: &str = "continue";

/// Whether the input only asks to continue: an empty user message, which
/// smelt's own loop answers by running again on the history it has.
fn is_continuation(input: &StartTurnInput) -> bool {
    matches!(input, StartTurnInput::User { content, .. } if content.is_empty())
}

/// What Claude Code gets for a turn's input. A continuation has no history
/// item, so the prompt that starts it stays out of the transcript.
fn prompt_content(input: &StartTurnInput) -> Result<Vec<Value>, String> {
    if is_continuation(input) {
        return user_content(&Content::text(CONTINUE_PROMPT));
    }
    user_content(&input.provider_content())
}

fn user_content(content: &Content) -> Result<Vec<Value>, String> {
    match content {
        Content::Text(text) => Ok(vec![json!({"type":"text", "text":text})]),
        Content::Parts(parts) => parts.iter().map(|part| match part {
            protocol::ContentPart::Text { text } => Ok(json!({"type":"text","text":text})),
            protocol::ContentPart::ImageUrl { url, .. } => {
                let (mime, data) = url.strip_prefix("data:").and_then(|s| s.split_once(";base64,")).ok_or("Claude Code requires image attachments as base64 data URLs")?;
                Ok(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
            }
        }).collect(),
    }
}

fn user_message(content: Vec<Value>) -> Value {
    json!({"type":"user","session_id":"","parent_tool_use_id":null,"message":{"role":"user","content":content}})
}

fn emit_error(tx: &EngineEventSender, message: String) {
    let _ = tx.send(EngineEvent::TurnError {
        message,
        kind: Some(EngineAskErrorKind::Other),
        retry_at_ms: None,
    });
}

fn emit_complete(tx: &EngineEventSender, turn_id: u64, elapsed: Duration, interrupted: bool) {
    let _ = tx.send(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: Some(protocol::TurnMeta {
            elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
            interrupted,
            avg_tps: None,
            display_tps: None,
        }),
    });
}

fn result_error(frame: &Value) -> String {
    let errors = frame["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    format!(
        "Claude Code: {}",
        if !errors.is_empty() {
            errors
        } else {
            frame["result"]
                .as_str()
                .or(frame["subtype"].as_str())
                .unwrap_or("request failed")
                .to_owned()
        }
    )
}

#[derive(Default)]
struct UsageTracker {
    models: Value,
    cost: f64,
    api_ms: u64,
}

impl UsageTracker {
    fn emit(
        &mut self,
        tx: &EngineEventSender,
        frame: &Value,
        background: bool,
        context_tokens: Option<u32>,
    ) {
        // Result cost, API time, and modelUsage are cumulative within this
        // process. The legacy usage field is per prompt and excludes subagents
        // and compaction.
        let since = |current: u64, previous: u64| {
            if current >= previous {
                current - previous
            } else {
                current
            }
        };
        let number = |legacy: &str, field: &str| {
            if let Some(models) = frame["modelUsage"].as_object() {
                let total = models.iter().fold(0u64, |sum, (model, usage)| {
                    sum.saturating_add(since(
                        usage[field].as_u64().unwrap_or(0),
                        self.models[model][field].as_u64().unwrap_or(0),
                    ))
                });
                Some(total.min(u32::MAX as u64) as u32)
            } else {
                frame["usage"][legacy]
                    .as_u64()
                    .map(|n| n.min(u32::MAX as u64) as u32)
            }
        };
        let usage = protocol::TokenUsage {
            context_tokens,
            prompt_tokens: number("input_tokens", "inputTokens"),
            completion_tokens: number("output_tokens", "outputTokens"),
            cache_read_tokens: number("cache_read_input_tokens", "cacheReadInputTokens"),
            cache_write_tokens: number("cache_creation_input_tokens", "cacheCreationInputTokens"),
            reasoning_tokens: frame
                .get("modelUsage")
                .and_then(|_| number("", "thinkingTokens")),
        };
        let cost = frame["total_cost_usd"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0);
        let delta = cost.map(|cost| {
            if cost >= self.cost {
                cost - self.cost
            } else {
                cost
            }
        });
        if let Some(cost) = cost {
            self.cost = cost;
        }
        if let Some(models) = frame.get("modelUsage") {
            self.models = models.clone();
        }
        // Output over model time, as smelt rates its own requests.
        let api_ms = frame["duration_api_ms"].as_u64();
        let tokens_per_sec = usage
            .completion_tokens
            .zip(api_ms.map(|ms| since(ms, self.api_ms)))
            .filter(|(tokens, ms)| *tokens > 0 && *ms > 0)
            .map(|(tokens, ms)| f64::from(tokens) * 1000.0 / ms as f64);
        if let Some(ms) = api_ms {
            self.api_ms = ms;
        }
        let _ = tx.send(EngineEvent::TokenUsage {
            usage,
            tokens_per_sec,
            cost_usd: delta,
            background,
        });
    }
}

pub(crate) fn spawn_ask(
    config: &EngineConfig,
    task: agent::AskTask,
    tx: &EngineEventSender,
    cancel: CancellationToken,
) {
    let cwd = config.cwd.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = auxiliary(&cwd, &task, &tx, &cancel).await;
        let (message, error) = match result {
            Ok(content) => (
                Some(protocol::Message::assistant(
                    Some(Content::text(content)),
                    None,
                    None,
                )),
                None,
            ),
            Err(message) => (
                None,
                Some(EngineAskError {
                    kind: if cancel.is_cancelled() {
                        EngineAskErrorKind::Cancelled
                    } else {
                        EngineAskErrorKind::Other
                    },
                    message,
                }),
            ),
        };
        let _ = tx.send(EngineEvent::EngineAskResponse {
            id: task.id,
            message,
            error,
        });
    });
}

async fn auxiliary(
    cwd: &std::path::Path,
    task: &agent::AskTask,
    tx: &EngineEventSender,
    cancel: &CancellationToken,
) -> Result<String, String> {
    if task.request_config.redact_secrets {
        return Err("The Claude Code backend cannot apply smelt's pre-request secret redaction. Disable redact_secrets explicitly or use smelt's native agent backend.".into());
    }
    let mut process = Process::spawn(transport::LaunchOptions {
        cwd,
        target: &task.target,
        mode: &protocol::AgentMode::normal(),
        effort: &task.reasoning_effort,
        resume: None,
        resume_at: None,
        prompt: None,
        override_prompt: Some(&task.system),
        plans_directory: None,
        disallowed_tools: &[],
        tools: false,
        persist_session: false,
        fast_mode: task.fast_mode,
        schema: task.response_format.as_ref().map(|fmt| &fmt.schema),
    })?;
    let result = async {
        process.write(control("smelt-init", json!({"subtype":"initialize","hooks":null}))).await?;
        let prompt = json!({"messages":task.messages}).to_string();
        let deadline = tokio::time::sleep(Duration::from_secs(180));
        tokio::pin!(deadline);
        let mut initialized = false;
        let mut usage = UsageTracker::default();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err("cancelled".into()),
                _ = &mut deadline => return Err("Claude Code auxiliary request timed out".into()),
                frame = process.read() => {
                    let frame = frame?.ok_or("Claude Code exited before its auxiliary request completed")?;
                    match frame["type"].as_str() {
                        Some("control_response") if !initialized && frame["response"]["request_id"].as_str() == Some("smelt-init") => {
                            if frame["response"]["subtype"].as_str() == Some("error") { return Err("Claude Code auxiliary initialization failed".into()); }
                            initialized = true;
                            process.write(user_message(vec![json!({"type":"text","text":prompt})])).await?;
                        }
                        Some("control_request") => process.write(reply(frame["request_id"].as_str().unwrap_or(""), json!({"behavior":"deny","message":"Tools are disabled for auxiliary requests"}))).await?,
                        Some("stream_event") if task.stream => {
                            if let Some(text) = frame["event"]["delta"]["text"].as_str() {
                                let _ = tx.send(EngineEvent::EngineAskDelta { id: task.id, delta: text.into() });
                            }
                        }
                        Some("result") if initialized => {
                            usage.emit(tx, &frame, true, None);
                            if frame["is_error"].as_bool().unwrap_or(false) || frame["subtype"].as_str() != Some("success") { return Err(result_error(&frame)); }
                            return Ok(if let Some(value) = frame.get("structured_output") { value.to_string() } else { frame["result"].as_str().unwrap_or("").to_owned() });
                        }
                        _ => {}
                    }
                }
            }
        }
    }.await;
    process.close().await;
    result
}

async fn exited(process: &mut Option<Process>) -> String {
    const WHAT: &str = "Claude Code exited unexpectedly";
    match process {
        Some(process) => process.exited(WHAT).await,
        None => format!("{WHAT}."),
    }
}

async fn read(process: &mut Option<Process>) -> Result<Option<Value>, String> {
    match process {
        Some(process) => process.read().await,
        None => std::future::pending().await,
    }
}
