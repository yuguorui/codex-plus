use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSessionCellExecutionLimits;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::ToolDefinition;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_runtime::InProcessCodeModeSession;
use codex_code_mode_runtime::StringCodeGeneration;
use codex_protocol::ToolName;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::MAX_WORKFLOW_AGENT_STALL_MS;
use crate::MAX_WORKFLOW_PROGRESS_TEXT_BYTES;
use crate::MemoryWorkflowInputArtifactStore;
use crate::ValidatedWorkflowScript;
use crate::WorkflowAgentActivity;
use crate::WorkflowAgentFailure;
use crate::WorkflowAgentFailureKind;
use crate::WorkflowAgentInputs;
use crate::WorkflowAgentOptions;
use crate::WorkflowAgentOutcome;
use crate::WorkflowAgentProgress;
use crate::WorkflowAgentProgressUpdate;
use crate::WorkflowAgentRequest;
use crate::WorkflowAgentResult;
use crate::WorkflowAgentState;
use crate::WorkflowEvent;
use crate::WorkflowExecutionError;
use crate::WorkflowInputArtifactRef;
use crate::WorkflowInputArtifactStore;
use crate::WorkflowInputDescriptor;
use crate::WorkflowJournalResult;
use crate::WorkflowProgressKind;
use crate::WorkflowRunOutcome;
use crate::WorkflowTokenBudget;
use crate::WorkflowTokenUsage;
use crate::inputs::validate_workflow_input_value;
use crate::inputs::workflow_agent_inputs_sha256;
use crate::script::WorkflowScriptContext;
use crate::script::compile_workflow_source_with_context;
use crate::serialize_workflow_result;
use crate::store_workflow_input_descriptor;
use crate::validate_v8_lossless_json_numbers;

mod delegate;

#[cfg(test)]
#[path = "runtime_control_tests.rs"]
mod control_tests;

const AGENT_TOOL_NAME: &str = "workflow_agent";
const CHILD_TOOL_NAME: &str = "workflow_child";
const DECLARED_INPUT_TOOL_NAME: &str = "workflow_declared_input";
const INPUT_ARTIFACT_TOOL_NAME: &str = "workflow_input_artifact";
const RESULT_TOOL_NAME: &str = "workflow_result";
const MAX_LOG_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_SETTLED_FAILURE_MESSAGE_BYTES: usize = 512;
const MAX_TERMINAL_RUNTIME_ERROR_BYTES: usize = 32 * 1024;
const MAX_WORKFLOW_FAILURES: usize = 256;
const MAX_WORKFLOW_LOGS: usize = 4096;
const WORKFLOW_LOG_HEAD_LEN: usize = MAX_WORKFLOW_LOGS / 4;
const WORKFLOW_LOG_TAIL_LEN: usize = MAX_WORKFLOW_LOGS - WORKFLOW_LOG_HEAD_LEN - 1;
const PROMPT_PREVIEW_BYTES: usize = 400;
const WORKFLOW_ISOLATE_HEAP_BYTES: usize = 64 * 1024 * 1024;
const WORKFLOW_RUN_ERROR_PREFIX: &str = "WorkflowRunError: ";
const WORKFLOW_AGENT_ERROR_PREFIX: &str = "WorkflowAgentError: ";
const DEFAULT_MAX_AGENTS_PER_RUN: usize = 1_000;
/// Stable journal namespace for one Workflow lineage.
///
/// The journal itself is scoped to one run, so this value deliberately does not
/// include the workflow script or frozen child composition. Agent calls are
/// addressed by their prompt/options/inputs identity, which lets a model fix a
/// broken workflow script and resume without re-running unchanged earlier calls.
const WORKFLOW_CACHE_ROOT: &str = "workflow";

pub type WorkflowAgentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkflowAgentResult, WorkflowAgentFailure>> + Send + 'a>>;
pub type WorkflowAgentStartedCallback<'a> = Box<dyn FnOnce(String) + Send + 'a>;
pub type WorkflowAgentProgressFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
pub type WorkflowEventSinkFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
/// Receives live token and tool-use snapshots while an agent attempt is running.
///
/// Implementations await each callback in order and cancel the returned future when the attempt
/// is cancelled or stalls.
pub type WorkflowAgentProgressCallback<'a> =
    Box<dyn Fn(WorkflowAgentProgressUpdate) -> WorkflowAgentProgressFuture<'a> + Send + Sync + 'a>;
pub type WorkflowChildFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedWorkflowChild, String>> + Send + 'a>>;
pub type WorkflowJournalReplayFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<WorkflowJournalResult>, String>> + Send + 'a>>;
pub type WorkflowJournalStartedFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;
pub type WorkflowJournalFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Executes one workflow agent call using host-owned agent infrastructure.
///
/// Implementations must observe `cancellation` and return promptly after it is
/// cancelled. Both successful results and failures must include the final usage
/// observed for the attempt; progress callbacks are only live snapshots.
/// Structured output validation belongs in the implementation when
/// `request.options.schema` is present.
pub trait WorkflowAgentRuntime: Send + Sync {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a>;

    fn run_agent_with_started<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        _on_started: WorkflowAgentStartedCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        self.run_agent(request, cancellation)
    }

    fn run_agent_with_progress<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        on_started: WorkflowAgentStartedCallback<'a>,
        _on_progress: WorkflowAgentProgressCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        self.run_agent_with_started(request, cancellation, on_started)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowChildRequest {
    pub name_or_ref: JsonValue,
    pub args: JsonValue,
    pub args_present: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedWorkflowChild {
    pub script: ValidatedWorkflowScript,
    pub args: JsonValue,
    pub args_present: bool,
}

/// Resolves saved workflows without coupling the runtime to host filesystem policy.
pub trait WorkflowChildResolver: Send + Sync {
    fn resolve_child<'a>(&'a self, request: WorkflowChildRequest) -> WorkflowChildFuture<'a>;
}

/// Stores agent outcomes for replay when a workflow is resumed.
///
/// The runtime replays successful outcomes. Skips and other failures are
/// retried instead of treated as reusable results.
pub trait WorkflowJournal: Send + Sync {
    /// Reads a journaled outcome. Errors prevent the corresponding agent from executing.
    fn replay<'a>(&'a self, key: &'a str) -> WorkflowJournalReplayFuture<'a>;

    /// Reports whether `key` was durably started but has no journaled outcome.
    ///
    /// A started call is a resume breakpoint: it must re-execute, but it does
    /// not invalidate the replay cache for calls that follow it.
    fn started<'a>(&'a self, _key: &'a str) -> WorkflowJournalStartedFuture<'a> {
        Box::pin(std::future::ready(Ok(false)))
    }

    /// Durably invalidates any replayable generation for `key` before execution starts.
    ///
    /// Callers must not execute the agent when this operation fails.
    fn append_started(&self, key: String) -> WorkflowJournalFuture<'_>;

    fn append_result(
        &self,
        key: String,
        result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_>;

    /// Flushes pending journal state when the owning workflow reaches a terminal state.
    fn close(&self) -> WorkflowJournalFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Receives ordered workflow progress snapshots as the runtime advances.
pub trait WorkflowEventSink: Send + Sync {
    fn emit(&self, execution_generation: u64, event: WorkflowEvent) -> WorkflowEventSinkFuture<'_>;
}

impl<F> WorkflowEventSink for F
where
    F: Fn(u64, WorkflowEvent) + Send + Sync,
{
    fn emit(&self, execution_generation: u64, event: WorkflowEvent) -> WorkflowEventSinkFuture<'_> {
        self(execution_generation, event);
        Box::pin(std::future::ready(()))
    }
}

/// A process-wide Workflow agent concurrency limit shared by workflow runs.
#[derive(Clone, Debug)]
pub struct WorkflowGlobalConcurrency {
    limit: NonZeroUsize,
    semaphore: Arc<Semaphore>,
}

impl WorkflowGlobalConcurrency {
    pub fn new(limit: NonZeroUsize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limit.get())),
            limit,
        }
    }

    pub fn limit(&self) -> NonZeroUsize {
        self.limit
    }

    async fn acquire_owned(&self) -> Result<OwnedSemaphorePermit, String> {
        Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| "global workflow concurrency limiter closed".to_string())
    }
}

#[derive(Clone)]
pub struct WorkflowRuntimeConfig {
    pub concurrency: usize,
    /// Optional process-wide limiter shared by the host's workflow runs.
    pub global_concurrency: Option<WorkflowGlobalConcurrency>,
    /// Number of automatic retries after an agent makes no progress.
    pub stall_retries: u32,
    /// Base of the exponential backoff between automatic stall retries.
    pub stall_retry_base_delay: Duration,
    /// Upper bound for the exponential stall retry backoff.
    pub stall_retry_max_delay: Duration,
    pub throttle_retry_delay: Duration,
    pub synchronous_timeout: Duration,
    /// Usage already consumed by this run before a restored runtime starts.
    pub initial_usage: WorkflowTokenUsage,
    /// Live host token budget checked before every real agent dispatch.
    pub budget: Option<Arc<dyn WorkflowTokenBudget>>,
    /// Maximum real agent dispatches in one run. Cached replays do not count.
    pub max_agents: usize,
    /// Whether the root workflow received an arguments value, including `null`.
    pub args_present: bool,
    pub child_resolver: Option<Arc<dyn WorkflowChildResolver>>,
    pub journal: Option<Arc<dyn WorkflowJournal>>,
    pub input_artifact_store: Arc<dyn WorkflowInputArtifactStore>,
    pub declared_inputs: Arc<crate::WorkflowDeclaredInputs>,
}

impl Default for WorkflowRuntimeConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);
        Self {
            concurrency: cores.saturating_sub(2).clamp(2, 16),
            global_concurrency: None,
            // Match Claude's five automatic restarts (six total attempts).
            stall_retries: 5,
            stall_retry_base_delay: Duration::from_secs(30),
            stall_retry_max_delay: Duration::from_secs(300),
            throttle_retry_delay: Duration::from_secs(45),
            synchronous_timeout: Duration::from_secs(30),
            initial_usage: WorkflowTokenUsage::default(),
            budget: None,
            max_agents: DEFAULT_MAX_AGENTS_PER_RUN,
            args_present: true,
            child_resolver: None,
            journal: None,
            input_artifact_store: Arc::new(MemoryWorkflowInputArtifactStore::default()),
            declared_inputs: Arc::new(crate::WorkflowDeclaredInputs::default()),
        }
    }
}

#[derive(Clone)]
pub struct WorkflowControl {
    state: Arc<ControlState>,
}

impl WorkflowControl {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ControlState::new()),
        }
    }

    pub fn stop(&self) {
        let _ = self.try_stop();
    }

    /// Requests cancellation if workflow execution has not already closed control submission.
    pub fn try_stop(&self) -> bool {
        self.state.try_stop()
    }

    pub fn skip_agent(&self, index: usize) -> bool {
        self.state.control_agent(index, AgentAction::Skip)
    }

    pub fn retry_agent(&self, index: usize) -> bool {
        self.state.control_agent(index, AgentAction::Retry)
    }

    pub fn agent_is_active(&self, index: usize) -> bool {
        self.state.agent_is_active(index)
    }

    /// Returns the authoritative latest state for an agent invocation.
    pub fn agent_progress(&self, index: usize) -> Option<WorkflowAgentProgress> {
        self.state.agent_progress(index)
    }

    /// Requests re-execution of a settled agent and everything downstream of it.
    ///
    /// Upstream agents replay from the run journal; the target agent and all agents
    /// invoked after it re-execute, so downstream stages that already ran are
    /// recomputed and stages that had not run yet simply run with the new value.
    pub fn rerun_from(&self, index: usize) -> bool {
        self.state.set_rerun_from(index)
    }
    pub fn is_cancelled(&self) -> bool {
        self.state.cancellation.is_cancelled()
    }

    pub fn is_open(&self) -> bool {
        self.state.is_open()
    }

    /// Closes control submission without cancelling workflow execution.
    pub fn close(&self) {
        self.state.close();
    }
}

impl Default for WorkflowControl {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentAction {
    None = 0,
    Skip = 1,
    Retry = 2,
}

struct ActiveAgentControl {
    action: Arc<AtomicUsize>,
    cancellation: CancellationToken,
}

struct ControlState {
    cancellation: CancellationToken,
    lifecycle: Mutex<ControlLifecycle>,
    agents: Mutex<HashMap<usize, ActiveAgentControl>>,
    invocations: Mutex<AuthoritativeAgentRegistry>,
    rerun_from: Mutex<Option<usize>>,
    rerun_signal: watch::Sender<()>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AgentInvocationKey {
    execution_generation: u64,
    invocation_id: String,
    index: usize,
}

#[derive(Default)]
struct AuthoritativeAgentRegistry {
    by_key: HashMap<AgentInvocationKey, WorkflowAgentProgress>,
    latest_by_index: HashMap<usize, AgentInvocationKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlLifecycle {
    Open,
    Closed,
}

enum WorkflowCompletionDecision {
    Complete,
    Cancelled,
    Rerun,
}

impl ControlState {
    fn new() -> Self {
        let (rerun_signal, _) = watch::channel(());
        Self {
            cancellation: CancellationToken::new(),
            lifecycle: Mutex::new(ControlLifecycle::Open),
            agents: Mutex::new(HashMap::new()),
            invocations: Mutex::new(AuthoritativeAgentRegistry::default()),
            rerun_from: Mutex::new(None),
            rerun_signal,
        }
    }

    fn try_stop(&self) -> bool {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle == ControlLifecycle::Closed {
            return false;
        }
        self.cancellation.cancel();
        true
    }

    fn close(&self) {
        *self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ControlLifecycle::Closed;
    }

    fn is_open(&self) -> bool {
        *self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == ControlLifecycle::Open
    }

    fn agent_is_active(&self, index: usize) -> bool {
        self.agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&index)
    }

    fn control_agent(&self, index: usize, action: AgentAction) -> bool {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle == ControlLifecycle::Closed {
            return false;
        }
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(agent) = agents.get(&index) else {
            return false;
        };
        agent.action.store(action as usize, Ordering::Release);
        agent.cancellation.cancel();
        true
    }

    fn record_agent(&self, execution_generation: u64, progress: WorkflowAgentProgress) {
        let mut invocations = self
            .invocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            progress.state,
            WorkflowAgentState::Done | WorkflowAgentState::Error
        ) && !progress.awaiting_decision
        {
            let settled_key = invocations
                .latest_by_index
                .get(&progress.index)
                .filter(|current| {
                    current.execution_generation == execution_generation
                        && current.invocation_id == progress.invocation_id
                })
                .cloned();
            if let Some(settled_key) = settled_key {
                invocations.latest_by_index.remove(&progress.index);
                invocations.by_key.remove(&settled_key);
            }
            return;
        }
        if !self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&progress.index)
        {
            return;
        }
        let replace = invocations
            .latest_by_index
            .get(&progress.index)
            .is_none_or(|current| {
                execution_generation > current.execution_generation
                    || (execution_generation == current.execution_generation
                        && progress.invocation_id == current.invocation_id)
            });
        if replace {
            let key = AgentInvocationKey {
                execution_generation,
                invocation_id: progress.invocation_id.clone(),
                index: progress.index,
            };
            if let Some(previous) = invocations
                .latest_by_index
                .insert(progress.index, key.clone())
            {
                invocations.by_key.remove(&previous);
            }
            invocations.by_key.insert(key, progress);
        }
    }

    fn agent_progress(&self, index: usize) -> Option<WorkflowAgentProgress> {
        let invocations = self
            .invocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        invocations
            .latest_by_index
            .get(&index)
            .and_then(|key| invocations.by_key.get(key))
            .cloned()
    }

    fn set_rerun_from(&self, index: usize) -> bool {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle == ControlLifecycle::Closed {
            return false;
        }
        let mut invocations = self
            .invocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        invocations
            .latest_by_index
            .retain(|agent_index, _| *agent_index <= index);
        invocations.by_key.retain(|key, _| key.index <= index);
        drop(invocations);
        let mut rerun_from = self
            .rerun_from
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *rerun_from = Some(rerun_from.map_or(index, |current| current.min(index)));
        self.rerun_signal.send_replace(());
        true
    }

    fn finish_success(&self) -> WorkflowCompletionDecision {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.cancellation.is_cancelled() {
            *lifecycle = ControlLifecycle::Closed;
            return WorkflowCompletionDecision::Cancelled;
        }
        if self
            .rerun_from
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return WorkflowCompletionDecision::Rerun;
        }
        *lifecycle = ControlLifecycle::Closed;
        WorkflowCompletionDecision::Complete
    }

    fn finish_error(&self) -> WorkflowCompletionDecision {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.cancellation.is_cancelled() {
            *lifecycle = ControlLifecycle::Closed;
            return WorkflowCompletionDecision::Cancelled;
        }
        if self
            .rerun_from
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return WorkflowCompletionDecision::Rerun;
        }
        *lifecycle = ControlLifecycle::Closed;
        WorkflowCompletionDecision::Complete
    }

    fn take_rerun_from(&self) -> (Option<usize>, watch::Receiver<()>) {
        let mut rerun_from = self
            .rerun_from
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = rerun_from.take();
        let receiver = self.rerun_signal.subscribe();
        (pending, receiver)
    }
}

fn execution_error_to_tool_error(error: WorkflowExecutionError) -> String {
    match error {
        WorkflowExecutionError::Cancelled => {
            format!("{WORKFLOW_RUN_ERROR_PREFIX}workflow cancelled")
        }
        WorkflowExecutionError::RerunRequested => {
            format!("{WORKFLOW_RUN_ERROR_PREFIX}workflow rerun was requested")
        }
        WorkflowExecutionError::Runtime(message)
            if message.starts_with(WORKFLOW_RUN_ERROR_PREFIX) =>
        {
            message
        }
        WorkflowExecutionError::Runtime(message) => message,
    }
}

fn workflow_budget_context(
    config: &WorkflowRuntimeConfig,
    total_tokens: u64,
) -> (Option<u64>, u64) {
    let total = config
        .budget
        .as_ref()
        .map(|budget| budget.total())
        .filter(|total| *total > 0);
    let host_spent = config.budget.as_ref().map_or(0, |budget| budget.spent());
    // Workflow subagents run without the host rollout budget, so host spend and
    // workflow usage are disjoint. `total_tokens` already includes the persisted
    // prefix and every completed session, which keeps reruns and child
    // workflows from under-reporting `budget.spent()`.
    let initial_spent = host_spent.saturating_add(total_tokens);
    (total, initial_spent)
}

pub async fn execute_workflow(
    script: &ValidatedWorkflowScript,
    args: JsonValue,
    agent_runtime: Arc<dyn WorkflowAgentRuntime>,
    event_sink: Arc<dyn WorkflowEventSink>,
    config: WorkflowRuntimeConfig,
    control: WorkflowControl,
) -> Result<WorkflowRunOutcome, WorkflowExecutionError> {
    let journal = config.journal.clone();
    let result =
        execute_workflow_inner(script, args, agent_runtime, event_sink, config, control).await;
    let Some(journal) = journal else {
        return result;
    };
    match journal.close().await {
        Ok(()) => result,
        Err(close_error) => match result {
            Ok(_) => Err(WorkflowExecutionError::Runtime(format!(
                "workflow journal close failed: {close_error}"
            ))),
            Err(error) => Err(WorkflowExecutionError::Runtime(format!(
                "{error}; workflow journal close failed: {close_error}"
            ))),
        },
    }
}

async fn execute_workflow_inner(
    script: &ValidatedWorkflowScript,
    args: JsonValue,
    agent_runtime: Arc<dyn WorkflowAgentRuntime>,
    event_sink: Arc<dyn WorkflowEventSink>,
    config: WorkflowRuntimeConfig,
    control: WorkflowControl,
) -> Result<WorkflowRunOutcome, WorkflowExecutionError> {
    struct CloseControlOnDrop(WorkflowControl);

    impl Drop for CloseControlOnDrop {
        fn drop(&mut self) {
            self.0.close();
        }
    }

    let _close_control = CloseControlOnDrop(control.clone());
    let started = Instant::now();
    let result_tool_name = RESULT_TOOL_NAME.to_string();
    let allow_child = config.child_resolver.is_some();
    let delegate = Arc::new(WorkflowDelegate::new(
        agent_runtime,
        event_sink,
        config.clone(),
        Arc::clone(&control.state),
        WORKFLOW_CACHE_ROOT.to_string(),
    ));
    for (index, phase) in script.meta.phases.iter().enumerate() {
        let title = sanitize_progress_text(&phase.title);
        delegate
            .emit(
                0,
                WorkflowEvent::WorkflowPhase {
                    index,
                    title,
                    kind: WorkflowProgressKind::Declared,
                },
            )
            .await;
    }
    loop {
        let (rerun_from, rerun_receiver) = control.state.take_rerun_from();
        let execution_generation = delegate.begin_session(rerun_from);
        let (budget_total, budget_initial_spent) =
            workflow_budget_context(&config, delegate.total_tokens.load(Ordering::Acquire));
        let source = validate_workflow_input_value(&args, "workflow arguments")
            .map_err(WorkflowExecutionError::Runtime)
            .and_then(|()| {
                compile_workflow_source_with_context(
                    script,
                    &args,
                    WorkflowScriptContext {
                        result_tool_name: Some(result_tool_name.clone()),
                        args_present: config.args_present,
                        budget_total,
                        budget_initial_spent,
                        ..WorkflowScriptContext::default()
                    },
                )
                .map_err(|error| WorkflowExecutionError::Runtime(error.to_string()))
            });
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                return match control.state.finish_error() {
                    WorkflowCompletionDecision::Cancelled => Err(WorkflowExecutionError::Cancelled),
                    WorkflowCompletionDecision::Complete | WorkflowCompletionDecision::Rerun => {
                        Err(error)
                    }
                };
            }
        };
        let result = run_workflow_source(WorkflowSourceExecution {
            source,
            delegate: delegate.clone(),
            control: Arc::clone(&control.state),
            invocation_cancellation: CancellationToken::new(),
            synchronous_timeout: config.synchronous_timeout,
            result_tool_name: result_tool_name.clone(),
            allow_child,
            rerun_receiver: Some(rerun_receiver),
            execution_generation,
            invocation_prefix: "root".to_string(),
        })
        .await;
        match result {
            Ok(result) => match control.state.finish_success() {
                WorkflowCompletionDecision::Complete => {
                    return Ok(delegate.outcome(result.value, started.elapsed()));
                }
                WorkflowCompletionDecision::Cancelled => {
                    return Err(WorkflowExecutionError::Cancelled);
                }
                WorkflowCompletionDecision::Rerun => {
                    delegate.record_log(
                        execution_generation,
                        "re-executing from the requested agent; downstream stages that already ran will be recomputed"
                            .to_string(),
                    )
                    .await;
                }
            },
            Err(WorkflowExecutionError::RerunRequested) => {
                delegate.record_log(
                    execution_generation,
                    "re-executing from the requested agent; downstream stages that already ran will be recomputed"
                        .to_string(),
                )
                .await;
            }
            Err(error) => match control.state.finish_error() {
                WorkflowCompletionDecision::Complete => return Err(error),
                WorkflowCompletionDecision::Cancelled => {
                    return Err(WorkflowExecutionError::Cancelled);
                }
                WorkflowCompletionDecision::Rerun => {
                    delegate.record_log(
                        execution_generation,
                        "re-executing from the requested agent after the previous execution failed"
                            .to_string(),
                    )
                    .await;
                }
            },
        }
    }
}

struct WorkflowSourceResult {
    value: JsonValue,
    tokens: u64,
}

struct WorkflowSourceExecution {
    source: String,
    delegate: Arc<WorkflowDelegate>,
    control: Arc<ControlState>,
    invocation_cancellation: CancellationToken,
    synchronous_timeout: Duration,
    result_tool_name: String,
    allow_child: bool,
    rerun_receiver: Option<watch::Receiver<()>>,
    execution_generation: u64,
    invocation_prefix: String,
}

async fn run_workflow_source(
    execution: WorkflowSourceExecution,
) -> Result<WorkflowSourceResult, WorkflowExecutionError> {
    let WorkflowSourceExecution {
        source,
        delegate,
        control,
        invocation_cancellation,
        synchronous_timeout,
        result_tool_name,
        allow_child,
        mut rerun_receiver,
        execution_generation,
        invocation_prefix,
    } = execution;
    let session_delegate: Arc<dyn CodeModeSessionDelegate> = Arc::new(WorkflowDelegateHandle {
        delegate: delegate.clone(),
        execution_generation,
        invocation_prefix,
    });
    let session = InProcessCodeModeSession::with_limits_and_string_code_generation(
        CodeModeSessionCellExecutionLimits {
            max_yield_time_ms: None,
            max_heap_size_bytes: Some(WORKFLOW_ISOLATE_HEAP_BYTES),
        },
        StringCodeGeneration::Deny,
    );
    let started_cell = session
        .execute(
            ExecuteRequest {
                tool_call_id: "workflow".to_string(),
                enabled_tools: workflow_tools(&result_tool_name, allow_child),
                source,
                yield_time_ms: Some(50),
                max_output_tokens: Some(1),
            },
            session_delegate,
            /*preempt*/ None,
        )
        .await
        .map_err(WorkflowExecutionError::Runtime)?;
    let cell_id = started_cell.cell_id.clone();
    let mut response = tokio::select! {
        response = tokio::time::timeout(synchronous_timeout, started_cell.initial_response()) => {
            match response {
                Ok(response) => response.map_err(WorkflowExecutionError::Runtime)?,
                Err(_) => {
                    let _ = session.terminate(cell_id.clone().into()).await;
                    let _ = session.shutdown().await;
                    return Err(WorkflowExecutionError::Runtime(
                        "await workflow APIs between bounded computation steps".to_string(),
                    ));
                }
            }
        }
        _ = control.cancellation.cancelled() => {
            let _ = session.terminate(cell_id.clone().into()).await;
            let _ = session.shutdown().await;
            delegate.record_run_error(format!("{WORKFLOW_RUN_ERROR_PREFIX}workflow cancelled"));
            return Err(WorkflowExecutionError::Cancelled);
        }
        _ = invocation_cancellation.cancelled() => {
            let _ = session.terminate(cell_id.clone().into()).await;
            let _ = session.shutdown().await;
            delegate.record_run_error(format!("{WORKFLOW_RUN_ERROR_PREFIX}workflow cancelled"));
            return Err(WorkflowExecutionError::Cancelled);
        }
        _ = wait_for_rerun(&mut rerun_receiver) => {
            let _ = session.terminate(cell_id.clone().into()).await;
            let _ = session.shutdown().await;
            return Err(WorkflowExecutionError::RerunRequested);
        }
    };
    loop {
        match response {
            RuntimeResponse::Result { error_text, .. } => {
                session
                    .shutdown()
                    .await
                    .map_err(WorkflowExecutionError::Runtime)?;
                if let Some(error) = error_text {
                    let error = truncate_utf8(&error, MAX_TERMINAL_RUNTIME_ERROR_BYTES);
                    if error.starts_with(WORKFLOW_RUN_ERROR_PREFIX) {
                        delegate.record_run_error(error.clone());
                    }
                    return Err(WorkflowExecutionError::Runtime(error));
                }
                if let Some(error) = delegate.current_run_error() {
                    return Err(WorkflowExecutionError::Runtime(error));
                }
                return delegate.take_result(&result_tool_name).ok_or_else(|| {
                    WorkflowExecutionError::Runtime(
                        "workflow completed without returning a result".to_string(),
                    )
                });
            }
            RuntimeResponse::Terminated { .. } => {
                let _ = session.shutdown().await;
                delegate.record_run_error(format!("{WORKFLOW_RUN_ERROR_PREFIX}workflow cancelled"));
                return Err(WorkflowExecutionError::Cancelled);
            }
            RuntimeResponse::Yielded { .. } => {
                response = tokio::select! {
                                    _ = control.cancellation.cancelled() => {
                                        session
                                            .terminate(cell_id.clone().into())
                                            .await
                                            .map_err(WorkflowExecutionError::Runtime)?
                                            .into()
                                    }
                _ = invocation_cancellation.cancelled() => {
                                        session
                                            .terminate(cell_id.clone().into())
                                            .await
                                            .map_err(WorkflowExecutionError::Runtime)?
                                            .into()
                                    }
                                    _ = wait_for_rerun(&mut rerun_receiver) => {
                                        session
                                            .terminate(cell_id.clone().into())
                                            .await
                                            .map_err(WorkflowExecutionError::Runtime)?;
                                        return Err(WorkflowExecutionError::RerunRequested);
                                    }
                                    waited = session.wait(
                                        WaitRequest {
                                            cell_id: cell_id.clone(),
                                            yield_time_ms: 100,
                                            max_output_tokens: None,
                                        },
                                        /*preempt*/ None,
                                    ) => waited.map_err(WorkflowExecutionError::Runtime)?.into(),
                                };
            }
        }
    }
}

async fn wait_for_rerun(rerun_receiver: &mut Option<watch::Receiver<()>>) {
    match rerun_receiver {
        Some(receiver) => {
            let _ = receiver.changed().await;
        }
        None => std::future::pending().await,
    }
}

struct WorkflowDelegate {
    agent_runtime: Arc<dyn WorkflowAgentRuntime>,
    event_sink: Arc<dyn WorkflowEventSink>,
    config: WorkflowRuntimeConfig,
    control: Arc<ControlState>,
    semaphore: Arc<Semaphore>,
    child_semaphore: Arc<Semaphore>,
    invocation_state: Mutex<InvocationState>,
    /// Serializes journal replay decisions so the first miss and every later
    /// call observe one linearized divergence point.
    replay_gate: tokio::sync::Mutex<()>,
    cache_root: String,
    execution_generation: AtomicU64,
    total_tokens: AtomicU64,
    total_tool_calls: AtomicU64,
    real_agent_count: AtomicUsize,
    final_results: Mutex<HashMap<String, WorkflowSourceResult>>,
    logs: Mutex<WorkflowLogBuffer>,
    failures: Mutex<WorkflowFailureBuffer>,
    run_error: Mutex<Option<String>>,
}

#[derive(Default)]
struct InvocationState {
    agent_count: usize,
    next_index: usize,
    invocation_indices: HashMap<String, usize>,
    /// Agents at or after this index re-execute instead of replaying from the journal.
    rerun_from: Option<usize>,
    /// Once a call misses the journal after a prior entry or existing result,
    /// no later call may replay. This matches Claude's single run-level
    /// divergence flag.
    replay_diverged: bool,
}

#[derive(Default)]
struct WorkflowLogBuffer {
    head: Vec<String>,
    tail: VecDeque<String>,
    dropped: u64,
}

#[derive(Default)]
struct WorkflowFailureBuffer {
    items: VecDeque<String>,
    dropped: u64,
}

impl WorkflowFailureBuffer {
    fn push(&mut self, message: String) {
        if self.items.len() == MAX_WORKFLOW_FAILURES.saturating_sub(1) {
            self.items.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.items.push_back(truncate_utf8(
            &sanitize_progress_text(&message),
            MAX_SETTLED_FAILURE_MESSAGE_BYTES,
        ));
    }

    fn clear(&mut self) {
        self.items.clear();
        self.dropped = 0;
    }

    fn snapshot(&self) -> Vec<String> {
        let marker = (self.dropped > 0)
            .then(|| format!("[dropped {} earlier workflow failures]", self.dropped));
        marker
            .into_iter()
            .chain(self.items.iter().cloned())
            .collect()
    }
}

impl WorkflowLogBuffer {
    fn push(&mut self, message: String) {
        if self.head.len() < WORKFLOW_LOG_HEAD_LEN {
            self.head.push(message);
            return;
        }
        if self.tail.len() == WORKFLOW_LOG_TAIL_LEN {
            self.tail.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.tail.push_back(message);
    }

    fn snapshot(&self) -> Vec<String> {
        let mut logs =
            Vec::with_capacity(self.head.len() + self.tail.len() + usize::from(self.dropped > 0));
        logs.extend(self.head.iter().cloned());
        if self.dropped > 0 {
            logs.push(format!(
                "[dropped {} earlier workflow log messages]",
                self.dropped
            ));
        }
        logs.extend(self.tail.iter().cloned());
        logs
    }
}

impl WorkflowDelegate {
    fn new(
        agent_runtime: Arc<dyn WorkflowAgentRuntime>,
        event_sink: Arc<dyn WorkflowEventSink>,
        config: WorkflowRuntimeConfig,
        control: Arc<ControlState>,
        cache_root: String,
    ) -> Self {
        let initial_usage = config.initial_usage.clone();
        Self {
            agent_runtime,
            event_sink,
            semaphore: Arc::new(Semaphore::new(config.concurrency)),
            child_semaphore: Arc::new(Semaphore::new(config.concurrency)),
            config,
            control,
            invocation_state: Mutex::new(InvocationState {
                agent_count: 0,
                next_index: 0,
                invocation_indices: HashMap::new(),
                rerun_from: None,
                replay_diverged: false,
            }),
            replay_gate: tokio::sync::Mutex::new(()),
            cache_root,
            execution_generation: AtomicU64::new(0),
            total_tokens: AtomicU64::new(initial_usage.total_tokens),
            total_tool_calls: AtomicU64::new(initial_usage.tool_uses),
            real_agent_count: AtomicUsize::new(0),
            final_results: Mutex::new(HashMap::new()),
            logs: Mutex::new(WorkflowLogBuffer::default()),
            failures: Mutex::new(WorkflowFailureBuffer::default()),
            run_error: Mutex::new(None),
        }
    }

    /// Resets per-session invocation state and diagnostics. A `rerun_from` index
    /// re-executes that agent and everything after it while replaying earlier agents
    /// from the journal.
    fn begin_session(&self, rerun_from: Option<usize>) -> u64 {
        let mut state = self
            .invocation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.agent_count = 0;
        state.rerun_from = rerun_from;
        state.replay_diverged = false;
        drop(state);
        self.real_agent_count.store(0, Ordering::Release);
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .run_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.execution_generation.fetch_add(1, Ordering::AcqRel)
    }

    fn mark_replay_diverged(&self) {
        self.invocation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replay_diverged = true;
    }

    fn check_budget(&self) -> Result<(), String> {
        let Some(budget) = self.config.budget.as_ref() else {
            return Ok(());
        };
        if !budget.enforce_limit() {
            return Ok(());
        }
        let total = budget.total();
        if total == 0 {
            return Ok(());
        }
        // `total_tokens` starts at the persisted workflow prefix, so it is
        // disjoint from the host rollout spend.
        let spent = budget
            .spent()
            .saturating_add(self.total_tokens.load(Ordering::Acquire));
        if spent >= total {
            return Err(self.run_level_error(format!(
                "workflow token budget exceeded before agent dispatch (spent {spent} of {total} tokens)"
            )));
        }
        Ok(())
    }

    async fn emit(&self, execution_generation: u64, event: WorkflowEvent) {
        self.event_sink.emit(execution_generation, event).await;
    }

    fn outcome(&self, result: JsonValue, elapsed: Duration) -> WorkflowRunOutcome {
        WorkflowRunOutcome {
            result,
            agent_count: self
                .invocation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .agent_count,
            logs: self
                .logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot(),
            failures: self
                .failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot(),
            total_tokens: self.total_tokens.load(Ordering::Acquire),
            total_tool_calls: self.total_tool_calls.load(Ordering::Acquire),
            duration_ms: duration_millis(elapsed),
        }
    }

    fn record_run_error(&self, message: String) {
        let mut run_error = self
            .run_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if run_error.is_none() {
            *run_error = Some(message);
        }
    }

    fn run_level_error(&self, message: impl std::fmt::Display) -> String {
        let message = format!("{WORKFLOW_RUN_ERROR_PREFIX}{message}");
        self.record_run_error(message.clone());
        message
    }

    fn current_run_error(&self) -> Option<String> {
        self.run_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn take_result(&self, result_tool_name: &str) -> Option<WorkflowSourceResult> {
        self.final_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(result_tool_name)
    }
}

struct WorkflowDelegateHandle {
    delegate: Arc<WorkflowDelegate>,
    execution_generation: u64,
    invocation_prefix: String,
}

impl CodeModeSessionDelegate for WorkflowDelegateHandle {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        let delegate = Arc::clone(&self.delegate);
        let execution_generation = self.execution_generation;
        let invocation_prefix = self.invocation_prefix.clone();
        Box::pin(async move {
            let tool_name = invocation.tool_name.name;
            match tool_name.as_str() {
                AGENT_TOOL_NAME => {
                    let mut input: AgentToolInput = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "workflow agent input is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid workflow agent input: {error}"))?;
                    input.invocation_id = format!("{invocation_prefix}/{}", input.invocation_id);
                    input.execution_generation = execution_generation;
                    delegate.invoke_agent(input, cancellation_token).await
                }
                INPUT_ARTIFACT_TOOL_NAME => {
                    let input: InputArtifactToolInput = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "workflow input artifact is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid workflow input artifact: {error}"))?;
                    let reference = store_workflow_input_descriptor(
                        input.descriptor,
                        &delegate.config.input_artifact_store,
                    )
                    .await?;
                    serde_json::to_value(reference).map_err(|error| error.to_string())
                }
                DECLARED_INPUT_TOOL_NAME => {
                    let input: DeclaredInputToolInput =
                        serde_json::from_value(invocation.input.ok_or_else(|| {
                            "workflow declared input request is missing".to_string()
                        })?)
                        .map_err(|error| {
                            format!("invalid workflow declared input request: {error}")
                        })?;
                    match input {
                        DeclaredInputToolInput::List => Ok(JsonValue::Array(
                            delegate
                                .config
                                .declared_inputs
                                .files
                                .iter()
                                .map(|(path, file)| {
                                    serde_json::json!({
                                        "path": path,
                                        "bytes": file.bytes,
                                        "sha256": file.sha256,
                                    })
                                })
                                .collect(),
                        )),
                        DeclaredInputToolInput::Read { path } => {
                            let file = delegate
                                .config
                                .declared_inputs
                                .files
                                .get(&path)
                                .ok_or_else(|| {
                                    format!(
                                        "readInput may only read files frozen by meta.inputs; `{path}` is unavailable"
                                    )
                                })?;
                            Ok(serde_json::json!({
                                "path": path,
                                "bytes": file.bytes,
                                "sha256": file.sha256,
                                "content": file.content,
                            }))
                        }
                    }
                }
                name if name == RESULT_TOOL_NAME || name.starts_with("workflow_result_") => {
                    let input: FinalResultInput = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "workflow result input is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid workflow result: {error}"))?;
                    serialize_workflow_result(&input.result).map_err(|error| error.to_string())?;
                    delegate
                        .final_results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(
                            tool_name,
                            WorkflowSourceResult {
                                value: input.result,
                                tokens: input.tokens,
                            },
                        );
                    Ok(JsonValue::Null)
                }
                CHILD_TOOL_NAME => {
                    let input = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "child workflow input is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid child workflow input: {error}"))?;
                    delegate
                        .invoke_child(
                            input,
                            cancellation_token,
                            execution_generation,
                            invocation_prefix,
                        )
                        .await
                }
                name => Err(format!("unknown workflow runtime tool `{name}`")),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        let delegate = Arc::clone(&self.delegate);
        let execution_generation = self.execution_generation;
        Box::pin(async move {
            let mut event: WorkflowEvent = serde_json::from_str(&text)
                .map_err(|error| format!("invalid workflow notification: {error}"))?;
            match &mut event {
                WorkflowEvent::WorkflowPhase { title, .. } => {
                    *title = sanitize_progress_text(title);
                    ensure_progress_text_bound("workflow phase title", title)?;
                }
                WorkflowEvent::WorkflowAgent(agent) => {
                    agent.label = sanitize_progress_text(&agent.label);
                    ensure_progress_text_bound("workflow agent label", &agent.label)?;
                    if let Some(phase_title) = &mut agent.phase_title {
                        *phase_title = sanitize_progress_text(phase_title);
                        ensure_progress_text_bound("workflow phase title", phase_title)?;
                    }
                }
                WorkflowEvent::WorkflowLog { .. } => {}
            }
            if let WorkflowEvent::WorkflowLog { message } = event {
                delegate.record_log(execution_generation, message).await;
            } else {
                delegate.emit(execution_generation, event).await;
            }
            Ok(())
        })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentToolInput {
    index: usize,
    #[serde(rename = "invocationId")]
    invocation_id: String,
    #[serde(skip)]
    execution_generation: u64,
    prompt: String,
    options: WorkflowAgentOptions,
    #[serde(rename = "phaseIndex")]
    phase_index: Option<usize>,
    #[serde(rename = "phaseTitle")]
    phase_title: Option<String>,
    #[serde(default, rename = "resultMode")]
    result_mode: AgentResultMode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputArtifactToolInput {
    descriptor: WorkflowInputDescriptor,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
enum AgentResultMode {
    #[default]
    Value,
    Settled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalResultInput {
    result: JsonValue,
    tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ChildToolInput {
    invocation_id: String,
    name_or_ref: JsonValue,
    args: JsonValue,
    #[serde(default = "default_true")]
    args_present: bool,
    phase_index: Option<usize>,
    phase_title: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Default)]
struct AgentEventDetails {
    queued_at: u64,
    started_at: Option<u64>,
    attempt: u32,
    agent_id: Option<String>,
    model: Option<String>,
    fallback_model: Option<String>,
    activity: Option<WorkflowAgentActivity>,
    cached: bool,
    blocked: bool,
    skipped: bool,
    awaiting_decision: bool,
    error: Option<String>,
    tokens: Option<u64>,
    tool_calls: Option<u64>,
    duration_ms: Option<u64>,
    result_preview: Option<String>,
    prompt_preview: String,
}

fn workflow_tools(result_tool_name: &str, allow_child: bool) -> Vec<ToolDefinition> {
    let mut names = vec![
        AGENT_TOOL_NAME.to_string(),
        DECLARED_INPUT_TOOL_NAME.to_string(),
        INPUT_ARTIFACT_TOOL_NAME.to_string(),
        result_tool_name.to_string(),
    ];
    if allow_child {
        names.push(CHILD_TOOL_NAME.to_string());
    }
    names
        .into_iter()
        .map(|name| ToolDefinition {
            tool_name: ToolName::plain(&name),
            name,
            description: String::new(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            input_schema_max_bytes: None,
            output_schema: None,
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum DeclaredInputToolInput {
    List,
    Read { path: String },
}

fn workflow_cache_key(
    cache_root: &str,
    invocation_id: &str,
    prompt: &str,
    options: &WorkflowAgentOptions,
    result_mode: AgentResultMode,
    inputs_sha256: Option<&str>,
) -> String {
    let mut selected = BTreeMap::new();
    if let Some(schema) = options.schema.as_ref() {
        selected.insert("schema", canonical_json(schema));
    }
    if let Some(model) = options.model.as_ref() {
        selected.insert("model", JsonValue::String(model.clone()));
    }
    if let Some(effort) = options.effort {
        selected.insert(
            "effort",
            serde_json::to_value(effort).unwrap_or(JsonValue::Null),
        );
    }
    if let Some(isolation) = options.isolation {
        selected.insert(
            "isolation",
            serde_json::to_value(isolation).unwrap_or(JsonValue::Null),
        );
    }
    if let Some(agent_type) = options.agent_type.as_ref() {
        selected.insert("agentType", JsonValue::String(agent_type.clone()));
    }
    if let Some(disallowed_tools) = options.disallowed_tools.as_ref() {
        let mut sorted = disallowed_tools.clone();
        sorted.sort();
        selected.insert(
            "disallowedTools",
            serde_json::to_value(sorted).unwrap_or(JsonValue::Null),
        );
    }
    if let Some(bash_command_clamp) = options.bash_command_clamp.as_ref() {
        let mut sorted = bash_command_clamp.clone();
        sorted.sort();
        selected.insert(
            "bashCommandClamp",
            serde_json::to_value(sorted).unwrap_or(JsonValue::Null),
        );
    }
    if result_mode == AgentResultMode::Settled {
        selected.insert("resultMode", JsonValue::String("settled".to_string()));
    }
    if let Some(inputs_sha256) = inputs_sha256 {
        selected.insert("inputsSha256", JsonValue::String(inputs_sha256.to_string()));
    }
    let canonical_options = serde_json::to_string(&selected).unwrap_or_else(|_| "{}".to_string());
    let mut digest = Sha256::new();
    digest.update(cache_root.as_bytes());
    digest.update([0]);
    digest.update(invocation_id.as_bytes());
    digest.update([0]);
    digest.update(prompt.as_bytes());
    digest.update([0]);
    digest.update(canonical_options.as_bytes());
    format!("v6:{:x}", digest.finalize())
}

/// Returns the journal identity for one workflow agent call.
///
/// Host-side recovery tooling and tests use this to inspect or seed a run
/// journal without duplicating the runtime's call-identity rules. `settled`
/// selects the `agentSettled` result envelope.
#[doc(hidden)]
pub fn workflow_agent_journal_key(
    invocation_id: &str,
    prompt: &str,
    options: &WorkflowAgentOptions,
    settled: bool,
    inputs_sha256: Option<&str>,
) -> String {
    workflow_cache_key(
        WORKFLOW_CACHE_ROOT,
        invocation_id,
        prompt,
        options,
        if settled {
            AgentResultMode::Settled
        } else {
            AgentResultMode::Value
        },
        inputs_sha256,
    )
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(canonical_json).collect()),
        JsonValue::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_value(sorted).unwrap_or(JsonValue::Null)
        }
        value => value.clone(),
    }
}

fn workflow_tool_result(
    value: JsonValue,
    tokens: u64,
    artifact: Option<WorkflowInputArtifactRef>,
) -> Result<JsonValue, String> {
    let result = serde_json::json!({
        "value": value,
        "tokens": tokens,
        "artifact": artifact,
    });
    validate_v8_lossless_json_numbers(&result, "workflow tool result")?;
    Ok(result)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn stall_retry_backoff(base: Duration, max: Duration, retries_used: u32) -> Duration {
    let multiplier = 1_u32 << retries_used.min(16);
    base.saturating_mul(multiplier).min(max)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn sanitize_progress_text(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| {
            if matches!(character, '\n' | '\r' | '\t') {
                Some(' ')
            } else if character.is_control() || is_bidi_control(character) {
                None
            } else {
                Some(character)
            }
        })
        .collect()
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn preview_json(value: &JsonValue) -> String {
    truncate_utf8(&value.to_string(), PROMPT_PREVIEW_BYTES)
}

fn ensure_progress_text_bound(field: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_WORKFLOW_PROGRESS_TEXT_BYTES {
        Err(format!("use a concise {field}"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
