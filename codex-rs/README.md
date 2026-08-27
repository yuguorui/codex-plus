# Codex++ (Rust Implementation)

**Codex++** is a fork of [openai/codex](https://github.com/openai/codex) that adds support for third-party model providers (Chat Completions, Anthropic Messages, custom Responses-compatible gateways), non-GPT-friendly file tools, TUI workflow improvements, and dynamic multi-agent workflows alongside the original OpenAI Responses API. It builds as a standalone executable with a zero-dependency install and is installed as the `codex++` command.

## Installing

Codex++ is distributed independently from the upstream Codex CLI. It cannot be installed via `npm` (`@openai/codex`) or Homebrew (`brew install --cask codex`) - those install the official upstream version without Codex++ wire and tool additions. Use one of the methods below instead.

### One-command install (recommended)

Codex++ publishes pre-built binaries for macOS (Apple Silicon), Linux (x86_64), and Windows (x86_64) via a dedicated GitHub Actions release workflow. The installers below resolve the latest release, verify the SHA-256 checksum against `codex-package_SHA256SUMS`, extract the standalone package, and install the `codex++` command.

**macOS / Linux** (run in a terminal):

```shell
curl -fsSL https://github.com/yuguorui/codex-plus/releases/latest/download/install-fork.sh | sh
```

**Windows** (run in PowerShell):

```powershell
irm https://github.com/yuguorui/codex-plus/releases/latest/download/install-fork.ps1 | iex
```

The standalone package is stored under `~/.codex/packages/standalone` (`%USERPROFILE%\.codex\packages\standalone` on Windows), and the `codex++` command is installed into `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows). Make sure that directory is on your `PATH`.

#### Installing a specific version

The installers accept a version as the first argument: `latest` (default), `rust-vyyyymmddhhmm`, `vyyyymmddhhmm`, or the bare `yyyymmddhhmm`.

```shell
sh install-fork.sh rust-v202608271200
```

```powershell
.\install-fork.ps1 202608271200
```

#### Installer options

Both installers honor these environment variables:

- `CODEX_RELEASE_REPOSITORY` - GitHub `owner/repo` that publishes fork releases (default: `yuguorui/codex-plus`).
- `CODEX_INSTALL_DIR` - directory that receives the `codex++` executable (default: `~/.local/bin`).
- `CODEX_BIN_NAME` - installed command name (default: `codex++`).
- `CODEX_HOME` - Codex home directory that holds the standalone package tree (default: `~/.codex`).

#### Updating

Run the same installer again, or run `codex++ update` from a release install. Codex++ also checks for a newer `rust-v<yyyymmddhhmm>` release on startup (refreshed every 20 hours) and shows a banner with an **Update now** option when one is available.

You can also build from source:

```shell
cd codex-rs
cargo build --release --bin codex
# The binary appears at target/release/codex.
# Copy or symlink it as codex++ if you want to match the fork release command name.
```

## What's New in Codex++

### Chat Completions API (`wire_api = "chat"`)

Adds a full Chat Completions API wire protocol (`/chat/completions` with SSE streaming) as an alternative to the Responses API. This enables Codex++ to work with any OpenAI-compatible provider that exposes the Chat Completions endpoint, including local inference servers like Ollama, vLLM, and LiteLLM, as well as cloud providers such as DeepSeek, Mistral, Groq, Together, and DashScope.

**Configuration example (local Ollama via Chat Completions):**

```toml
[model_providers.local_ollama]
name = "Local Ollama"
base_url = "http://localhost:11434/v1"
env_key = "OLLAMA_API_KEY"
wire_api = "chat"
```

> **Note:** Built-in provider IDs (`openai`, `ollama`, `lmstudio`, `amazon-bedrock`, `amazon-bedrock-runtime`, `gcp`) are reserved and cannot be redefined as arbitrary custom providers. Use a distinct ID such as `local_ollama` or `ollama_chat`.

Key implementation details:

- Converts Codex's internal Responses-format prompts into Chat Completions format (`messages`, `tool_calls`, tool responses, reasoning).
- Coalesces consecutive function/tool calls into a single assistant message (required by the Chat API spec).
- Encodes namespaced tools (`mcp__calendar__lookup_order`) with reversible compound names so they remain callable and traceable.
- Wraps custom tools (such as `apply_patch`) as function tools with a `{ "input": "..." }` parameter schema.
- Forces `stream_options.include_usage = true` for token-usage reporting.
- Projects Codex reasoning effort onto the cross-provider-safe top-level `reasoning_effort` subset (`low` / `high` / `max`):
  - `minimal` / `low` -> `"low"`
  - `medium` / `high` / `xhigh` -> `"high"`
  - `max` / `ultra` -> `"max"`
  - `none` / `persistent` / `custom` -> omitted (providers disagree on how thinking-off is spelled; set it explicitly in `extra_body` if needed).
- Reconciles `thinking_budget` with projected `reasoning_effort`: after merging provider and model `extra_body`, if the final body carries a non-null top-level `thinking_budget`, Codex++ removes the client-projected `reasoning_effort` (avoiding 400 errors on providers such as Qwen3.8 that reject both together) unless `reasoning_effort` was explicitly declared in `extra_body`.

### Anthropic Messages API (`wire_api = "anthropic"`)

Adds a full Anthropic Messages API wire protocol (`/messages` with SSE streaming). This enables Codex++ to work directly with Claude models through Anthropic's native API or Anthropic-compatible gateways.

**Configuration example (Anthropic direct):**

```toml
[model_providers.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
env_key_auth = "x-api-key"
wire_api = "anthropic"
```

Key implementation details:

- Sends prompts to `<base_url>/messages` (so use `https://api.anthropic.com/v1` for Anthropic direct) with `anthropic-version: 2023-06-01` (unless overridden via headers) and SSE streaming.
- Maps Codex reasoning effort to Anthropic's `output_config.effort` field:
  - `none`, unset, or custom effort -> `output_config` omitted
  - `minimal` / `low` -> `"low"`
  - `medium` -> `"medium"`
  - `high` -> `"high"`
  - `xhigh` -> `"xhigh"`
  - `max` / `ultra` / `persistent` -> `"max"`
- Leaves `thinking` to the model or compatible provider default rather than sending the legacy manual `thinking: { type: "enabled", budget_tokens }` block (which newer Anthropic models reject and some compatible providers do not support). Providers or users that need an explicit `thinking` block can still set it via `extra_body`.
- Sets `max_tokens` from `capabilities.max_output_tokens`, defaulting to `64000` when unset.
- Automatically places ephemeral prompt-caching breakpoints (`cache_control: { "type": "ephemeral" }`) on the last `system` block, the last `tools` entry, and the last cacheable content block of the second-to-last `user` message.
- Attaches `metadata.user_id` (containing a process-stable `device_id` and the active `session_id`) for prompt-cache routing when a session ID is present.
- Preserves `Thinking` (with `signature`) and `RedactedThinking` blocks for context replay, validates `tool_use` / `tool_result` pairing locally before dispatch, converts `system`/`developer` messages to Anthropic `system` blocks, encodes namespaced tools, and wraps custom tools with `{ "input": "..." }`.
- Supports both `x-api-key` (default when `wire_api = "anthropic"` and `env_key_auth` is unset) and `bearer` via `env_key_auth`.

### `env_base_url` - Environment-Driven Base URL Override

New optional provider field `env_base_url` names an environment variable whose non-empty value overrides `base_url` at runtime:

```toml
[model_providers.my_gateway]
name = "My Gateway"
base_url = "https://default.gateway.example.com/v1"
env_base_url = "MY_GATEWAY_BASE_URL"
env_key = "MY_GATEWAY_API_KEY"
wire_api = "responses"
```

If `MY_GATEWAY_BASE_URL` is set and non-empty, its value is used; otherwise `base_url` is used.

### `[model_providers.<id>.capabilities]` - Provider Capability Overrides

Custom providers can declare fine-grained wire capability overrides under `[model_providers.<id>.capabilities]`:

```toml
[model_providers.my_gateway]
name = "My Gateway"
base_url = "https://gateway.example.com/v1"
env_key = "MY_GATEWAY_API_KEY"
wire_api = "responses"

[model_providers.my_gateway.capabilities]
supports_custom_exec_tool = false
supports_native_tool_search = false
max_output_tokens = 32768
```

Codex++ adds and honors:

- `supports_custom_exec_tool` (`bool`, default `true`) - controls whether the Responses endpoint accepts arbitrary OpenAI `custom` tools beyond `apply_patch`. Set to `false` for providers (such as DeepSeek) that only accept `custom` for `apply_patch`; Code Mode's `exec` is then serialized as a plain function tool taking an `input` string.
- `supports_native_tool_search` (`bool`, default `true` on `wire_api = "responses"`) - controls whether the full Responses tools path uses the native `tool_search` tool type and `tool_search_call` output items. Set to `false` for Responses gateways (such as BigModel) that accept the tool definition without exposing it to the model; Codex++ transports `tool_search` as a plain function tool and re-declares discovered tools client-side (including replay on resume/fork). `wire_api = "chat"` and `wire_api = "anthropic"` always use this function-backed client-side `tool_search` path automatically.
- `max_output_tokens` (`u32`, optional) - sets the output-token ceiling for wires that require an explicit cap (such as Anthropic Messages `max_tokens`, which defaults to `64000` when unset).

### `env_key_auth` - Auth Header Scheme

New config field that controls how the API key from `env_key` is sent:

- `"bearer"` - sends `Authorization: Bearer <key>` (default for most providers).
- `"x-api-key"` - sends `x-api-key: <key>` (default for Anthropic `wire_api` if `env_key_auth` is unset).

This is especially useful for Anthropic direct (which expects `x-api-key`) versus Anthropic-compatible proxies/gateways (which often expect `Authorization: Bearer`).

### `extra_body` - Provider-Specific Request Body Fields

New config field that lets you merge arbitrary JSON fields into the request body sent to the provider. This is useful for toggling provider-specific features that Codex doesn't expose directly.

Examples:

```toml
# DashScope / Qwen: enable thinking with a budget
[model_providers.dashscope]
extra_body = { "enable_thinking" = true, "thinking_budget" = 1024 }

# Anthropic: explicitly set a thinking block when needed by a model or proxy
[model_providers.anthropic]
extra_body = { "thinking" = { "type" = "adaptive" } }
```

`extra_body` merges with the same semantics on every wire API (Responses, Chat Completions, Anthropic):

- Nested objects deep-merge recursively.
- Arrays and scalars replace the existing value; arrays are replaced wholesale, not appended.
- `null` removes the key from the request body when present, and is a no-op when absent - at every nesting level. A merged body never gains a JSON `null` object value from `extra_body`, since several providers reject explicit nulls with a 400. This lets a model opt out of a projected default such as `reasoning_effort`. TOML has no null, so the opt-out is only expressible in JSON configuration (a model catalog, or a remote thread config's `extra_body_json`).

Provider-level `extra_body` is applied first, then per-request `extra_body` (from model info) is merged on top, so a model can override or remove what its provider declares.

Wire-specific guards still apply: on the Chat wire, `stream_options` is object-merged and `include_usage` is forced to `true` (a non-object override is rejected), and a non-null top-level `thinking_budget` reconciles away a client-projected `reasoning_effort`; on the Anthropic wire, `stream` must remain `true`, and any other value - including `null` - is rejected.

### `extra_headers` / `env_extra_headers` - Alias Header Fields

New aliases `extra_headers` and `env_extra_headers` are accepted alongside the existing `http_headers` and `env_http_headers`. These aliases match the naming convention used by other OpenAI-compatible tools, making config more portable.

### Non-GPT-Friendly File Tools

Codex++ provides structured file-editing alternatives for models that perform better with line-anchored or exact-replacement tools than with freeform patches:

Codex++ separates file editing into three mutually exclusive `apply_patch_tool_type` profiles and automatically aligns the generic default instructions with the active profile:

- **Freeform patch (`apply_patch_tool_type = "freeform"`)** - registers `apply_patch` (default for OpenAI GPT models).
- **`fuzz_view_edit` (`apply_patch_tool_type = "hashline"`)** - registers only `fuzz_view_edit`, supporting `read`, `edit`, `insert`, and `delete` over `LINE:HASH|content` 4-character xxHash line anchors, with strict stale checks for `LINE:HASH` writes, unique bare-hash targeting without re-reading line numbers, and fuzzy relocation for minor drift.
- **Claude Code-compatible `Read` and `Edit` (`apply_patch_tool_type = "claude_code"`)** - registers `Read` (`file_read`) and `Edit` (`file_edit`). `Read` reads text files with line-number prefixes (`offset`/`limit`), images, PDF page ranges (`pages`), and Jupyter notebooks; `Edit` performs exact `old_string` -> `new_string` replacements with `replace_all` support and read-before-edit safety checks.

### TUI Enhancements

- **Pending steers & validation-failure queue management** - while a turn is running with pending steers, press `Esc` twice (`esc esc`, or bind `tui.keymap.chat.cancel_pending_steers`) to cancel pending steers before the model consumes them and restore them to the composer; pressing `Esc` once still interrupts and sends immediately. If a turn fails local prompt/tool-history validation before any provider request is sent, queued inputs pause instead of auto-sending until you manually retry.
- **Side-conversation model switching** - `/model` is available inside active side conversations and switches the model thread-locally for that side conversation without overwriting your persisted default model settings.
- **Searchable subagent picker** - the `/agents` subagent picker supports interactive text search/filtering.
- **Typing-reactive Bongo Cat** - `/pets` lets you enable a typing-reactive Bongo Cat companion (full sprite rendering on Kitty, iTerm2, and Sixel terminals; ASCII fallback elsewhere).

### Decoupled Stream Preemption

User-input preemption (`input_preempt`) is decoupled from active stream preemption (`stream_preempt`):

- Responses Lite over WebSocket (which carries a native server-side interrupt) continues to preempt and drain cleanly when steering arrives mid-stream.
- Non-interruptible streams (Chat Completions SSE, Anthropic Messages SSE, Responses HTTP SSE, and non-Lite WebSocket) are no longer discarded locally on mid-stream steering, preventing abandoned server generations while still waking Code Mode `exec`/`wait` tools and skipping retry backoff before partial output arrives.

### 429 & Overload Retry

Custom model providers automatically retry on HTTP 429 (rate-limit) responses as well as `ServerOverloaded`, 5xx, and transport errors, using exponential backoff capped by `request_max_retry_delay_ms` (default 10 s, max 300 s).

```toml
[model_providers.my_provider]
request_max_retry_delay_ms = 15_000   # cap backoff at 15 seconds
```

### Fork Release Workflow

A dedicated GitHub Actions workflow (`fork-release.yml`) builds and publishes pre-built binaries for macOS (Apple Silicon), Linux (x86_64/musl), and Windows (x86_64 MSVC) on every manual or auto-rebase trigger. Releases are tagged `rust-v<yyyymmddhhmm>` and include SHA-256 checksums.

## Connecting to Chat and Anthropic Providers

### Chat Completions providers (Ollama, vLLM, LiteLLM, DeepSeek, etc.)

Any server that implements the OpenAI Chat Completions API (`/chat/completions`) can be used with Codex++ by setting `wire_api = "chat"`:

```toml
# Example: local Ollama (use a custom ID since `ollama` is a reserved built-in ID)
[model_providers.local_ollama]
name = "Local Ollama"
base_url = "http://localhost:11434/v1"
wire_api = "chat"

# Example: DeepSeek API
[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
env_key = "DEEPSEEK_API_KEY"
wire_api = "chat"

# Example: DashScope (Qwen) with thinking enabled
[model_providers.dashscope]
name = "DashScope"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
env_key = "DASHSCOPE_API_KEY"
wire_api = "chat"
extra_body = { "enable_thinking" = true }
```

Then run (or set `model_provider` and `model` in `config.toml` / a profile):

```shell
codex++ -c model_provider=local_ollama -m qwen3
codex++ -c model_provider=deepseek -m deepseek-chat
codex++ -c model_provider=dashscope -m qwen-plus
```

### Anthropic (Claude) providers

Use `wire_api = "anthropic"` with `env_key_auth = "x-api-key"` (or leave it unset; Anthropic `wire_api` defaults to `x-api-key`):

```toml
# Anthropic direct
[model_providers.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
env_key_auth = "x-api-key"
wire_api = "anthropic"

# Optional: lower max_tokens below the 64000 default for models with a smaller output cap
[model_providers.anthropic.capabilities]
max_output_tokens = 16384

# Anthropic via a proxy/gateway that expects Bearer auth
[model_providers.anthropic_proxy]
name = "Anthropic Proxy"
base_url = "https://my-gateway.example.com/v1"
env_key = "PROXY_API_KEY"
env_key_auth = "bearer"
wire_api = "anthropic"

# Optional: explicitly pass a thinking block via extra_body
[model_providers.anthropic_custom]
name = "Anthropic Custom Thinking"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic"
extra_body = { "thinking" = { "type" = "enabled", "budget_tokens" = 16000 } }
```

Then run:

```shell
codex++ -c model_provider=anthropic -m claude-sonnet-4-20250514
```

### Using reasoning / thinking effort

Both Chat and Anthropic wire APIs respect `model_reasoning_effort` (configured in `config.toml`, passed via `-c model_reasoning_effort=...`, or changed interactively via `/model`):

```shell
# Chat provider with low reasoning (sends reasoning_effort = "low")
codex++ -c model_provider=deepseek -m deepseek-chat -c model_reasoning_effort=low

# Anthropic with high reasoning (sends output_config.effort = "high")
codex++ -c model_provider=anthropic -m claude-sonnet-4-20250514 -c model_reasoning_effort=high

# Anthropic with no reasoning effort override (omits output_config)
codex++ -c model_provider=anthropic -m claude-sonnet-4-20250514 -c model_reasoning_effort=none
```

On the Anthropic wire, `thinking` itself is left to the model or compatible provider default; use `extra_body` if you need to force an explicit `thinking` block. On the Chat wire, `reasoning_effort` is projected to `low`/`high`/`max` and automatically dropped if a non-null top-level `thinking_budget` is present (unless `reasoning_effort` is explicitly set in `extra_body`).

## Upstream Features

The following features are inherited from the upstream openai/codex repository and remain unchanged:

### Config

Codex supports a rich set of configuration options. Note that the Rust CLI uses `config.toml` instead of `config.json`. See [`docs/config.md`](../docs/config.md) for details.

### Model Context Protocol Support

- **MCP client** - Codex++ connects to MCP servers on startup. See [`docs/config.md`](../docs/config.md#connecting-to-mcp-servers).
- **MCP server (experimental)** - Run `codex++ mcp-server` to let other MCP clients use Codex++ as a tool.

### Notifications

You can enable notifications by configuring a script that is run whenever the agent finishes a turn. See [`docs/config.md`](../docs/config.md#notify).

### `codex++ exec` to run programmatically/non-interactively

To run Codex++ non-interactively, run `codex++ exec PROMPT` (you can also pass the prompt via `stdin`). Use `codex++ exec --ephemeral ...` to run without persisting session rollout files to disk.

### Codex Sandbox

Use `codex++ sandbox [COMMAND]...` to test commands under the sandbox. The `--sandbox` flag (`-s`) lets you pick the sandbox policy:

```shell
codex++ --sandbox read-only        # default
codex++ --sandbox workspace-write  # allow writes in workspace
codex++ --sandbox danger-full-access  # no sandbox
```

## Code Organization

This folder is the root of a Cargo workspace. The key crates are:

- [`core/`](./core) - business logic for Codex sessions, prompt building, tool handlers (`fuzz_view_edit`, `Read`/`Edit`), and client orchestration.
- [`exec/`](./exec) - headless CLI for automation (`codex++ exec`).
- [`tui/`](./tui) - fullscreen TUI built with [Ratatui](https://ratatui.rs/).
- [`cli/`](./cli) - CLI multitool that provides the aforementioned CLIs via subcommands.
- [`codex-api/`](./codex-api) - API client with endpoint implementations for Responses, Chat Completions, and Anthropic Messages.
- [`model-provider-info/`](./model-provider-info) - provider configuration types (`WireApi`, `EnvKeyAuthScheme`, `ModelProviderCapabilities`, `env_base_url`, `extra_body`, etc.).
- [`model-provider/`](./model-provider) - provider resolution, auth dispatch, and API retry configuration.

If you want to contribute or inspect behavior in detail, start by reading the module-level `README.md` files under each crate and run the project workspace from the top-level `codex-rs` directory so shared config, features, and build scripts stay aligned.
