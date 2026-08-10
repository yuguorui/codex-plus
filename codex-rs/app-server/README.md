# User verification cancellation (experimental)

Local UI clients can cancel a native user-verification RPC by sending
`userVerification/cancel` with `{requestId}` and the `experimentalApi` opt-in.
The result is an empty acknowledgment (`{}`). This API does not enable desktop
verification capability advertisement.

`requestId` is the original status, enroll, delete, or verify RPC's string or
integer ID on the same connection, not the server elicitation ID. Use fresh IDs
for each operation and a distinct ID for the cancel RPC. Unknown, finished,
unrelated, and other-connection requests are no-ops.

The acknowledgment confirms the cancellation signal without waiting for the OS
prompt to close. The original RPC completes independently, with
`cancelled/interrupted` when cancellation prevents completion. Cancellation
cannot roll back completed effects. It remains effective while a proof waits for
outbound queue capacity, but cannot retract a response already enqueued.

Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. Clients must cancel that RPC separately and discard
late proofs after the approval is canceled or resolved. Only one native worker
runs per app-server; if an OS call remains active after cancellation or timeout,
subsequent local operations return `failed/providerError` until that worker exits.

# Hosted Codex Apps MCP protocol

The host-owned HTTP `codex_apps` server uses Legacy by default in app-server and
standalone Codex. To discover the 2026-07-28 protocol, set
`codex_apps_mcp_2026_07_28 = true` under `[features]`, or send a true runtime
override via `experimentalFeature/enablement/set`. Discovery falls back to Legacy
when the server does not support it. Explicit config takes precedence.
The dedicated setting does not apply to third-party HTTP or local `codex_app`
stdio servers. The existing `mcp_2026_07_28` flag still governs eligible other
servers, regardless of whether their names or URLs resemble hosted Apps.
App-server does not persist this selection.

# Thread removal

`thread/archive` and `thread/delete` reject attempts to remove a live internal
worker with JSON-RPC error `-32600`. The worker's owner controls its shutdown.
For example, a Guardian reviewer remains available to its parent conversation
after a client tries to archive or delete it.

After the owner releases the worker, its saved conversation can be archived or
deleted normally. Ordinary client-controlled threads keep their existing behavior.

## User verification (experimental)

Codex app-server advertises `openai/elicitation.userVerification` to the
host-owned plugin service for bundled, in-process TUI sessions (`codex-tui`) and
local stdio desktop sessions (`Codex Desktop`) on devices with supported biometric
hardware and the `experimentalApi` opt-in. This is an app-server decision,
independent of whether a key exists; TUI/Desktop/mobile do not advertise this MCP
capability. Mobile integration requires a separate rollout. Other clients and
network connections do not receive this mode, even with a recognized client name.
Before sending verification requests to desktop sessions, deploy a GUI that
handles the typed verification request, cancellation, and late proofs. The general
`experimentalApi` opt-in does not identify a compatible GUI version.

Local UI clients use five methods. They require the existing
`experimentalApi` opt-in. The local provider reports
`unavailable/providerUnavailable` on unsupported platforms or without the required
ChatGPT account identity.

| Method | Params | Result |
| --- | --- | --- |
| `userVerification/status` | `{}` | `{credentialId, unavailableReason, unavailableMessage}` |
| `userVerification/enroll` | `{}` | `{credentialId}` |
| `userVerification/delete` | `{}` | `{}` |
| `userVerification/verify` | `{challenge, title, description}` | `{proof: {credentialId, signature}}` |
| `userVerification/cancel` | `{requestId}` | `{}` |

Status reads local readiness without prompting or contacting a backend. A null
`unavailableReason` means local checks passed, not that registration is valid.
Unsupported platforms and missing account identity are reported in the status
response's `unavailableReason` field.
The initial enrollment creates or reuses the local key only. Backend
registration and revocation are integration TODOs; local success is not server
enrollment. Deletion currently removes that local key synchronously.
Enrollment and deletion coordinate credential lifecycle; callers do not issue
separate generate or rotate commands. Identity comes from the authenticated
account; this API exposes no caller-selected scope.

Verify signs 1–4096 decoded challenge bytes using P-256 ECDSA with SHA-256. The
challenge and DER signature use unpadded base64url. Title is 1–256 UTF-8 bytes;
description is at most 4096 bytes. The UI obtains approval for that display
context before calling. Verify does not require a pending elicitation; a UI with
its own authenticator can return proof directly in elicitation response content.
The calling flow owns pending-request checks and discards late proofs.
Native enroll, delete, and verify accept local stdio and in-process connections.
WebSocket and remote-control peers must use their own device authenticator;
status remains available for local readiness. Dropping an embedded RPC, disconnecting,
or changing authentication cancels its native operation. Responses recheck the
captured identity after waiting for outbound queue capacity.
Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. The GUI must use `userVerification/cancel` to
cancel that RPC and discard late proofs when an approval is canceled or resolved.
See [User verification cancellation](#user-verification-cancellation-experimental)
for request ID and acknowledgment semantics.
Only one native worker runs per app-server. If an OS call remains active after
cancellation or timeout, subsequent local operations return `failed/providerError`
until that worker exits.

Failures use the normal JSON-RPC error envelope with closed `{type, reason}` data:
`invalidRequest`, `unavailable`, `cancelled`, or `failed`. UI clients branch on
these values rather than message text. Native diagnostic payloads stay private.

# Amazon Bedrock authentication

If `model_providers.amazon-bedrock.aws.credential_export` is configured, Bedrock setup and
Bedrock login return an error without changing configuration or saved credentials. Remove the
exporter configuration before selecting another credential source. `aws.credential_export` and
`aws.profile` cannot be configured together.

## Stored thread attachments

- `thread/attachment/add` — add a durable resource reference to a stored thread without loading it. Repeated writes with the same attachment type and identity key return the existing attachment.
- `thread/attachment/list` — list attachments for one stored thread in a cursor-paginated request, including a thread that is not loaded.
- `thread/attachment/remove` — remove an attachment by its thread, attachment type, and identity key; returns `{}`.
- `thread/attachment/updated` — notification broadcast after an attachment is created or removed; contains the thread, attachment identity, attachment id, and operation.
### Example: Manage stored thread attachments

Attachments record the resources currently associated with a thread, independently of conversation history. Clients can add, remove, and list attachments for one stored thread at a time without resuming those threads. Adding or removing an attachment does not create or delete the underlying resource or rewrite history. An attachment is idempotently identified by its thread, `attachmentType`, and `identityKey`. For pull requests, clients should reuse the canonical application identity `JSON.stringify([canonicalHostname, lowercaseOwner, lowercaseRepository, pullRequestNumber])` so addition and removal agree across surfaces.

```json
{ "method": "thread/attachment/add", "id": 20, "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
    "payload": { "url": "https://github.com/openai/codex/pull/123" }
} }
{ "id": 20, "result": {
    "outcome": "created",
    "attachment": {
        "id": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
        "attachmentType": "pull_request",
        "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
        "payload": { "url": "https://github.com/openai/codex/pull/123" },
        "createdAt": 1750000000
    }
} }

{ "method": "thread/attachment/list", "id": 21, "params": {
    "threadId": "thr_123",
    "limit": 100
} }
{ "id": 21, "result": {
    "data": [{
        "id": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
        "attachmentType": "pull_request",
        "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
        "payload": { "url": "https://github.com/openai/codex/pull/123" },
        "createdAt": 1750000000
    }],
    "nextCursor": null
} }

{ "method": "thread/attachment/remove", "id": 22, "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]"
} }
{ "id": 22, "result": {} }

{ "method": "thread/attachment/updated", "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
    "attachmentId": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
    "operation": "deleted"
} }
```

`thread/attachment/list` accepts one `threadId` and returns at most 100 attachments per page, ordered by creation time and attachment id. Continue with `nextCursor` and the same `threadId` until the cursor is `null`. Each thread can retain up to 100 attachments. Removing an attachment frees a slot for a new attachment.

Attachment creation and deletion requests using the same thread ID are serialized across connections. The requesting client receives its response before the compact update is broadcast, and duplicate creates or absent deletes do not emit updates. Deleting the owning thread removes its attachments under the same lifecycle exclusion; queued attachment mutations then report that the thread was not found.
## Dynamic workflows (Codex++ fork extensions)

The Codex++ fork adds background dynamic-workflow runs driven by the model-facing
`Workflow` tool. All workflow RPCs below are experimental and require
`capabilities.experimentalApi`.

- `workflow/list` — experimental; page background dynamic-workflow runs for a loaded thread. Returns task snapshots with the latest bounded phase, agent, and log window, plus usage, terminal outcome counts, failures, and output paths.
- `workflowApprovalArtifact/read` — experimental; read and verify a bounded page of a Workflow approval action by `threadId`, content-addressed `artifactId`, and optional byte `offset`. Returns `{ sha256, offset, contents, nextOffset }`; continue from `nextOffset` until it is `null` to inspect the complete action without filesystem access to the app-server host.
- `workflow/stop` — experimental; request cancellation of an active workflow by `threadId` and `runId`. Returns `accepted: false` when the run is already terminal.
- `workflow/skipAgent` — experimental; stop the active attempt for one workflow agent and settle that slot as skipped.
- `workflow/retryAgent` — experimental; stop the active attempt for one workflow agent and schedule another attempt.

### Example: Inspect and control dynamic workflows

Dynamic workflows are model tools, not a separate launch RPC. Enable the `workflows` feature, initialize with `capabilities.experimentalApi: true`, and start a normal turn in which the user explicitly asks to run a workflow. The `Workflow` launch response marks `transcriptDir` and `scriptPath` as `appServerHostArtifact`; they are persisted host paths, not paths in the selected remote execution environment. After any required tool approval, the `Workflow` tool returns immediately while execution continues in the background. A resume call may omit `args` to reuse the terminal run's persisted arguments; explicit resume arguments replace them and disable journal replay. When the owning model needs the result before continuing its current turn, it can call `WaitWorkflow` with the returned `runId`; that tool waits for a terminal state or its configured timeout and returns a focused terminal result inline when available. When `resultTruncated` is true or `resultError` is non-null, the model reads the result with `ReadWorkflowResult`, using the same `runId`, starting at offset `0`, and continuing from each `nextOffset`. For a result that should not be paged through model context, the model may instead pass `ReadWorkflowResult` a `writePath` relative to the primary selected execution environment cwd (or absolute inside one of its workspace roots); Codex writes the complete verified JSON result through that environment's filesystem and returns only bounded metadata. `ReadWorkflowResult` also accepts an RFC 6901 `jsonPointer` no longer than 512 UTF-8 bytes; it returns the selected value directly when bounded, or writes exactly that projected JSON when combined with `writePath`. Projection cannot be combined with `offset` or `maxBytes`. `AnalyzeWorkflowInputs` returns a bounded `resultShape` and `nextAction` when a program result is too large, so the model can narrow its next program instead of receiving a generic error. `WaitWorkflow` accepts the same `writePath` option after its terminal wait, so a critical-path result can be written without a separate read call. The connection receives `workflow/started`, zero or more `workflow/progress` snapshots, and one terminal `workflow/completed` notification.

Workflow agent execution is independent of the model-visible multi-agent protocol. Agent v1 parent turns retain the `multi_agent_v1` tool namespace, Agent v2 parent turns retain the `collaboration` namespace, and both can launch the same `Workflow` tool and DSL. Workflow-owned child agents expose neither multi-agent namespace nor `Workflow`, which prevents nested orchestration without requiring a v2-only call path.

Structured workflow agents use the provider's native strict JSON Schema output on OpenAI providers. Other providers receive the same bounded schema in the child-agent prompt and are validated locally, so Chat, Anthropic, and open-model providers do not need to implement the Responses `text.format` field.

Workflow child agents inherit the effective configuration and selected executor of the sampling step that launched the Workflow, including model/provider, instructions, service tier, approval policy, and reviewer. Keep child prompts to stable task instructions and pass variable data through `agent(..., {inputs})`. Agents with inputs receive `AnalyzeWorkflowInputs`, which provides programmatic access to the complete deep-frozen input object in fresh V8 isolates. Use `parallel(..., {requireAll: true})` so critical synthesis starts with every required result.

Workflow approvals include a content-addressed `codex://workflow-approval/<threadId>/<sha256>` reference. Remote clients can read the exact reviewed bytes while the approval is pending:

```json
{ "method": "workflowApprovalArtifact/read", "id": 37, "params": {
    "threadId": "11111111-1111-4111-8111-111111111111",
    "artifactId": "<sha256 from the approval reference>",
    "offset": 0
} }
```

Each response includes `sha256`, `offset`, bounded `contents`, and a nullable `nextOffset`. Read every page by passing the preceding `nextOffset`. The content binds the frozen Workflow definition and arguments together with every selected environment's location, cwd, workspace roots, environment configuration, sandbox context, effective approval policy, redacted child capabilities, and opaque executor ID. Codex verifies the persisted bytes again after approval and launches from the already approved in-memory definition, child configuration, project-instruction snapshot, and captured executor handles.

Use `workflow/list` to rebuild UI state after reconnecting or opening a workflow panel:

```json
{ "method": "workflow/list", "id": 38, "params": {
    "threadId": "thr_123",
    "cursor": null,
    "limit": 20
} }
{ "id": 38, "result": {
    "data": [
        {
            "threadId": "thr_123",
            "turnId": "turn_456",
            "taskId": "w4f91a02c",
            "runId": "wf_01abc234",
            "workflowName": "code-review",
            "title": null,
            "status": "running",
            "summary": "Running workflow code-review",
            "transcriptDir": "/path/to/subagents/workflows/wf_01abc234",
            "scriptPath": "/path/to/workflows/scripts/code-review-wf_01abc234.js",
            "outputFile": "/path/to/sessions/thr_123/workflows/wf_01abc234.json",
            "progress": [],
            "progressVersion": 0,
            "usage": {
              "totalTokens": 0,
              "toolUses": 0,
              "durationMs": 0,
              "agentCount": 0,
              "successfulAgentCount": 0,
              "failedAgentCount": 0,
              "skippedAgentCount": 0,
              "nullAgentResultCount": 0
            },
            "failures": [],
            "error": null,
            "startedAt": 1786200000,
            "completedAt": null
        }
    ],
    "nextCursor": null
} }
```

Stop a whole run, or control one active agent by its stable progress `index`:

```json
{ "method": "workflow/stop", "id": 39, "params": {
    "threadId": "thr_123",
    "runId": "wf_01abc234"
} }
{ "id": 39, "result": { "accepted": true } }

{ "method": "workflow/skipAgent", "id": 40, "params": {
    "threadId": "thr_123",
    "runId": "wf_01abc234",
    "agentIndex": 3
} }
{ "id": 40, "result": { "accepted": true } }

{ "method": "workflow/retryAgent", "id": 41, "params": {
    "threadId": "thr_123",
    "runId": "wf_01abc234",
    "agentIndex": 3
} }
{ "id": 41, "result": { "accepted": true } }
```

All four methods are experimental and require `capabilities.experimentalApi`. Workflow notifications are thread-scoped and are sent only to connections currently subscribed to the owning thread.

### Dynamic workflow events (experimental)

- `workflow/started` — identifies the background task and run and includes `threadId`, `turnId`, `taskId`, `runId`, `workflowName`, nullable `title`, `summary`, `transcriptDir`, `scriptPath`, stable `deliveryKey`, and Unix-second `startedAt`.
- `workflow/progress` — carries the latest bounded `progress` snapshot plus cumulative `usage`. Progress items are tagged as `workflowPhase`, `workflowAgent`, or `workflowLog`. Agent items expose stable `invocationId`, queue/running/terminal state, retry attempt, cache/skip/block flags, token and tool counts, timing, and bounded prompt/result previews.
- `workflow/completed` — terminal notification with `status` (`completed`, `failed`, `paused`, or `killed`), `summary`, `outputFile`, nullable `error`, partial `failures`, cumulative `usage` with terminal agent outcome counts (`successfulAgentCount`, `failedAgentCount`, `skippedAgentCount`, and `nullAgentResultCount`), stable `deliveryKey`, `progressResyncRequired`, and Unix-second `completedAt`. When `progressResyncRequired` is true, refresh the task with `workflow/list` before rendering final progress. `outputFile` points to the run snapshot; the terminal snapshot includes a content-addressed result artifact descriptor alongside progress and usage. Running snapshots are persisted at most once every two seconds, with the result artifact and final snapshot durably written before this notification.

Clients should deduplicate `workflow/started` and `workflow/completed` retries by `deliveryKey`. Delivery is tracked independently for each subscribed connection; notification opt-out and missing experimental capability mean that connection is not a target. A disconnected or stalled target remains retryable until a later online attempt writes successfully. Clients should key live agent rows by `(taskId, invocationId, index)` and replace prior snapshots rather than append them. A workflow can outlive the turn that launched it; `turn/completed` does not imply `workflow/completed`. Respect the client animation setting when rendering running-state animation. The built-in TUI uses a truecolor shimmer when supported (with a reduced-motion fallback), cyan for running agents, green for completed work, red for failures or blocked work, and dim styling for skipped or stopped work.
