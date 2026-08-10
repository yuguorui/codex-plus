# Workflow Script API

## Invocation

Workflow accepts one of these sources:

- `script`: inline JavaScript for a new custom workflow.
- `name`: a built-in, plugin, user, or project workflow. Plugin names use
  `pluginName:workflowName`.
- `scriptPath`: a persisted script from an earlier invocation.

When several source fields are present, `scriptPath` takes precedence over `script`, and `script`
takes precedence over `name`. When the selected execution environment filesystem is foreign to the
app-server host, use an inline `script`. Automatic
review handles complete actions directly and routes larger actions according to the configured
review policy.

Pass `args` as a JSON value. The script receives that value unchanged through the global `args`.
When `args` is omitted, the global is `undefined`; explicit `null` stays `null`.
Every resolved script is persisted, and the launch result includes its `scriptPath` and `runId`.
The launch remains asynchronous. When the owning model's next critical-path step needs one final
result, call `WaitWorkflow` with that `runId`. Use `WaitWorkflows` with `mode: "any"` or
`mode: "all"` for a focused set of runs. Both calls are safe to repeat and return at terminal
completion, timeout, or new owning-turn user input. `interruptedByUserInput: true` preserves that
input, so handle it before deciding whether to wait again.

`WaitWorkflows` reports status and result descriptors for every requested run in request order, but
no run text, and never returns result content for `mode: "all"`. With `mode: "any"` it also returns
`winner`: the run that ended the wait, carrying the same bounded result head `WaitWorkflow` would
have returned, so a race does not cost a second round trip. `winner` is `null` for `mode: "all"`,
when no run satisfied the condition, and when it had to be dropped to fit the response size cap;
re-wait that run with `WaitWorkflow`, use `ListWorkflows` for its error and summary, and
`ReadWorkflowResult` for any entry whose `resultAvailable` is `true`.

`ListWorkflows` returns focused status summaries for runs owned by the current thread. A small
terminal result is included by `WaitWorkflow`. `ListWorkflowAgents` exposes each Workflow agent's
stable index, invocation identity, and `agentId`; call `wait_agent` with the `agentId` of a completed
agent when its intermediate detail is useful. Call `ReadWorkflowResult` only for the terminal
Workflow result, optionally choosing `maxBytes`; concatenate `chunk` values and continue from
`nextOffset` while `complete` is false. Use `StopWorkflow` for an active run. A running agent can
be controlled with `RetryWorkflowAgent` or `SkipWorkflowAgent` and its zero-based agent index. An active retry affects only that attempt; retrying a settled agent reruns that
invocation and every later recorded invocation in the current execution. Pass
`{"dryRun": true}` to `RetryWorkflowAgent` to preview those counts without changing the run.
Owning-model orchestration tools remain with the owning agent.

## Script Format

Begin with a literal metadata declaration:

```javascript
export const meta = {
  name: "inspect-and-summarize",
  description: "Inspect several areas and synthesize the findings",
  title: "Inspect and summarize",
  phases: [
    { title: "Inspect", detail: "Explore independent areas" },
    { title: "Synthesize", detail: "Combine the findings" },
  ],
  inputs: ["config/*.json"],
};
```

`name` and `description` are required. `title`, `whenToUse`, `phases`, and `inputs` are optional.
Metadata is a pure object literal. `inputs` is an optional list of workspace-relative glob patterns;
matched UTF-8 text files are frozen before launch and participate in Workflow resume identity.
Workflows without `inputs` do not scan the workspace. Use `/` separators; absolute paths, parent
traversal, matched symlink files, and unmatched patterns are rejected; glob walks do not follow
directory symlinks. Declared inputs require exactly one selected execution environment. The limits
are 64 patterns, 512 bytes per pattern, 256 files, 256 KiB per file, and 2 MiB total. Glob expansion
examines at most 4,096 entries, 1,024 directories, and 64 directory levels, so prefer narrow scan
roots. Use phase titles consistently between `meta.phases`, `phase()`, and agent options. A phase
may declare a `model`; agents in that phase inherit it unless they pass an explicit `model`.

The body is JavaScript in an async context, so top-level `await` is available. Finish with
`return value`; the value must be JSON-compatible. The runtime supplies a deterministic,
self-contained JavaScript environment with its API as globals and sanitizes values across host
boundaries. Supply time, randomness, and other external values through `args` so resume remains
deterministic.

## Runtime Globals

### `budget`

`budget` is a frozen object with `total`, `spent()`, and `remaining()`. `total` is `null` when the
host has no token target, and `remaining()` is then `Infinity`. A positive total is enforced before
each real agent dispatch, including queued and retried attempts; cached journal replays do not
consume the budget. `spent()` includes the host's shared turn spend snapshot, the persisted workflow
prefix of a resumed run, and workflow tokens observed by this run. Host spend and workflow tokens
may use different weighting, so treat `spent()` as the host's budget accounting rather than a direct
raw-token count.

### `listInputs()` and `readInput(path)`

Both functions are asynchronous:

```javascript
const files = await listInputs();
const config = await readInput("config/app.json");
```

`listInputs()` returns the path, byte size, and SHA-256 hash of each file frozen by `meta.inputs`.
`readInput(path)` returns the frozen text for one listed path and rejects undeclared paths. These
APIs never read the live filesystem during Workflow execution. They are read-only and do not expose
Node.js `fs` or `process`; use an agent's ordinary tools for commands, live reads, and writes.

Declared workspace inputs are separate from the structured `inputs` option on `agent()`. The
former are frozen before Workflow launch and participate in Workflow resume identity. The latter
are JSON-compatible values passed to one subagent and participate in that agent call's journal
identity.

### `agent(prompt, options?)`

Runs one subagent. `prompt` is one non-empty string. The host wraps it with a Workflow preamble,
optional isolation text, and an output-schema contract before model submission. Keep the prompt to
task instructions and pass variable data through named `inputs`.

Options:

- `label`: short progress label.
- `phase`: explicit phase title, especially useful in concurrent work.
- `schema`: JSON Schema for a validated structured result.
- `model`: model override; omission, `inherit`, or `default` inherits the resolved parent model.
- `effort`: `low`, `medium`, `high`, `xhigh`, or `max`.
- `isolation`: `worktree` for a temporary git worktree when parallel agents mutate files.
- `agentType`: registered custom agent type.
- `stallMs`: optional timeout with no Codex-visible event and no concrete active work (tool,
  tracked process, or model stream). Use it to catch idle agents rather than long-running silent
  commands.
- `disallowedTools`: optional string array. Claude tool names are mapped to Codex tools; `Bash`
  denies `exec_command` and `write_stdin`, `Write`/`Edit` deny `apply_patch`, `Edit`, and
  `fuzz_view_edit`, `Agent` denies `spawn_agent`, and `*` denies every tool. `mcp__server__tool`
  matches both sanitized and unsanitized tool spellings under `mcp__server` and the non-prefixed
  `server` namespace; `mcp__server`, `mcp__server__*`, and `mcp__*` deny exact server namespaces or
  every namespaced tool. For multi-segment MCP namespaces, use `mcp__<namespace>__*`; a fully
  qualified `mcp__a__b__tool` spelling is ambiguous because `__` also separates the namespace.
  Unknown names are denied by their literal tool name. Workflow subagents cannot spawn nested agents
  in Codex++; the `Agent` mapping is defensive for portable scripts.
- `bashCommandClamp`: optional string array of `Bash(...)` command rules. The child shell tool
  fails closed when no rule matches; `Bash`/`Bash(*)` allows every command. Non-wildcard clamps
  admit one simple command and reject shell composition (`;`, `&`, `|`, redirects, command
  substitution, and newlines), force a non-login invocation of the default shell, and reject
  model-selected shells and TTY execution. The clamp also denies `write_stdin`. This is a command
  guardrail, not a sandbox: zsh may still source `.zshenv`, so combine it with `disallowedTools`
  and the thread permission policy when the agent must not write files or shell startup
  configuration.
- `inputs`: named JSON-compatible values copied from the Workflow isolate. The complete input
  object is validated and participates in journal replay identity. The model prompt carries task
  instructions while `inputs` retains the structured runtime values.

With `schema`, the result is the validated object; otherwise it is final text. Agent-level
failures (skip, terminal API error, throttle, block, or ordinary failure) resolve to `null`.
Exhausted stall retries and exhausted structured-output retries throw from a direct `agent()` call,
while `parallel()`/`pipeline()` turn those slot failures into `null`. Run-level failures
(cancellation, token budget exhaustion, and the per-run agent cap) throw, and
`parallel()`/`pipeline()` rethrow them instead of turning them into `null`. Use `agentSettled()`
when branching on an agent-level failure's kind and message; it reports `stalled` as a rejected
envelope.
Workflow agents inherit the parent's ordinary tools and connected MCP tools; orchestration and
user messaging stay with the owning agent, and each subagent's final response becomes the function
result. Stalled calls retry automatically with exponential backoff; after the retry budget a
direct `agent()` throws, `parallel()`/`pipeline()` turn that slot into `null`, and
`agentSettled()` returns a `stalled` rejection envelope.

An agent with `inputs` receives `AnalyzeWorkflowInputs`. Each call runs a synchronous pure
JavaScript function body in a fresh V8 isolate with deep-frozen `globalThis.inputs`, `console` for
diagnostics, and `helpers.utf8Slice(value, startByte, maxBytes)`. Use multiple calls to inspect,
filter, rank, and aggregate the complete inputs while returning focused values to the model. Return
the formal result with `return`; use `console.log` only for diagnostics.

### `agentSettled(prompt, options?)`

Runs an agent with the same prompt and options as `agent()`, but always returns an explicit result
for a completed call. Success is `{status: "fulfilled", value}`. Failure is
`{status: "rejected", reason: {kind, message}}`, where `kind` is `failed`, `terminalApi`,
`structuredOutput`, `stalled`, `throttled`, `blocked`, or `skipped`. The result focuses on stable
status, value, and reason fields. Use this API when the script branches on agent-level failure;
run-level failures (cancellation, budget exhaustion, or the agent cap) still throw.

### `pipeline(items, ...stages)`

Moves every item through all stages independently. Each stage receives
`(previousResult, originalItem, index)`. A failed stage yields `null` for that item while other
items continue.

```javascript
const reviewed = await pipeline(
  args.dimensions,
  (dimension) => agent("Inspect the dimension supplied in inputs.", {
    label: `inspect:${dimension.name}`,
    phase: "Inspect",
    schema: FINDINGS_SCHEMA,
    inputs: { dimension },
  }),
  (found, dimension) => {
    const options = {
      label: `verify:${dimension.name}`,
      phase: "Verify",
      schema: VERDICTS_SCHEMA,
      inputs: { finding: found, dimension },
    };
    return agent(
      "Verify the complete finding supplied in inputs. Use AnalyzeWorkflowInputs to inspect it and return a verdict.",
      options,
    );
  },
);
```

### `parallel(thunks, options?)`

Runs functions concurrently and waits for all of them. Each failed thunk yields `null`; siblings
continue by default, which is appropriate for best-effort exploration. Set `requireAll: true` for
critical fan-in. Strict mode waits for every thunk, then fails the Workflow with a concise error
summary if any thunk failed. The failed run can resume from its unchanged journal prefix.

```javascript
const reports = await parallel(args.areas.map((area, index) => () =>
  agent("Inspect the area supplied in inputs and return a detailed report.", {
    label: `inspect:${index}`,
    phase: "Inspect",
    inputs: { area },
  })
)), { requireAll: true });

return agent(
  "Synthesize every report. Use AnalyzeWorkflowInputs to inspect, filter, and aggregate the complete reports before writing the final answer.",
  {
    label: "final",
    phase: "Synthesize",
    inputs: { reports },
  },
);
```

### Progress

- `phase(title)` activates a progress group.
- `log(message)` emits a bounded progress message.

Inside concurrent stages, set the agent `phase` option explicitly because the global active phase
is shared.

### `workflow(nameOrRef, childArgs?)`

Runs an approved saved child Workflow from a top-level local Workflow. The reference must appear directly
in the source as one of these static forms:

```javascript
await workflow("review", args);
await workflow({ name: "review" }, args);
await workflow({ scriptPath: "workflows/review.js" }, args);
```

Before approval, every referenced child is resolved, validated, and frozen into the reviewed
top-level action. Execution and restoration use those frozen artifacts. Keep composition to a
focused set of statically referenced approved local child Workflows.

## Runtime Safeguards and Recovery

Concurrency, scripts, schemas, prompts, results, and boundary values are validated by the runtime.
Additional concurrent calls queue, and prompts are checked before invoking the host and again
before a model request. Pass runtime data through `inputs` so the downstream agent can inspect the
complete values with `AnalyzeWorkflowInputs`.
Default helpers settle every item rather than failing fast and return `null` for failed calls.
Critical synthesis should use `parallel(..., { requireAll: true })` or explicitly verify counts.
Report intentional best-effort sampling through `log()`.

After a run stops or fails, invoke Workflow with `scriptPath`, `resumeFromRunId`, and any required
`args`. Successful agent calls replay from the journal; skips and other failures are retried rather
than treated as reusable results. A call that started but never produced a result re-executes
without setting the divergence flag, so later successful calls may still replay. A cache miss,
failure, or changed call sets the divergence flag; every later call then re-executes. Cache
identity includes the prompt, named
`inputs`, result mode, `schema`, `model`, `effort`, `isolation`, `agentType`, `disallowedTools`, and
`bashCommandClamp`, plus the stable invocation identity. Editing the workflow script does not by itself discard matching earlier calls:
only calls whose prompt, options, inputs, or invocation identity changed re-execute. Labels,
phases, and stall timeouts are excluded from cache identity.
Workflow resume additionally requires matching args, declared workspace inputs, execution
environment, model, permissions, and effective configuration. Undeclared workspace file changes
are intentionally ignored.
