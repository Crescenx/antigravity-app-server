# antigravity-app-server

[简体中文](README.md) | English

**Bring Antigravity's native agent runtime into a familiar Thread / Turn / Item workflow.**

The native ACP server can hold a conversation, but integrating it into a development tool also requires session recovery, input during execution, approvals, sandboxing, usage tracking, and long-running tasks. These behavioral differences create extra work for clients built around the Codex app-server interface.

This project provides **an app server whose behavior is as close to Codex as practical, while keeping Google Antigravity's native model calls, tool definitions, and session execution.** It is a standalone Rust stdio JSON-RPC service that any client can launch, without depending on a particular frontend.

**This project draws heavily on, and reuses, [OpenAI Codex](https://github.com/openai/codex) implementations. It was itself vibe-coded with Codex.** Codex informed the protocol and behavior; its CLI provides the OS sandbox and automatic-review backend; the Guardian assessment code is adapted from upstream Codex. See [Codex references and development](#codex-references-and-development) for the distinctions and attribution.

Currently supported: **Linux x86_64 / WSL2**, with **`localharness_external` 1.4.0** pinned. This is an independent community project, unaffiliated with Google or OpenAI. It implements a subset of the Codex-style protocol, not full API, prompt, or model-behavior compatibility.

[Quick start](#quick-start) · [Sandbox and approvals](#sandbox-and-approvals) · [Tools](#tools-and-commands) · [Instructions and skills](#instructions-and-skills) · [Goals](#persistent-goals) · [Subagents](#subagents-and-independent-workers) · [Client API](#client-api) · [Protocol reference](docs/protocol.md)

## Capabilities

| Capability | Current behavior |
| --- | --- |
| Sessions and streaming | Thread / Turn / Item; persistence, resume, unload, naming, and archiving |
| Control during execution | Interruption, FIFO steering, request deduplication, explicit recovery for unknown outcomes |
| Sandbox | Read-only, workspace-write, and full access; commands use the Codex OS sandbox |
| Approvals | Human approval or independent Codex automatic review; failures do not grant access |
| Collaboration modes | Default / Plan; Plan enforces read-only execution and disables external MCP and autonomous Goals |
| Instructions | Global and project `AGENTS.md`, overrides, directory precedence, and size limits |
| Skills / MCP | Local skill discovery, enable/disable, explicit skill input; stdio / HTTP MCP |
| Goals | Persistent objectives, automatic continuation, pause, completion, blocked state, and token budgets |
| Models and attachments | Account model catalog, reasoning effort, grouped quotas, images, and other supported attachments |
| Independent workers | Ephemeral sessions, disabled tools, and JSON Schema output |
| Subagents | Codex V2-style spawning, messaging, follow-up tasks, waiting, and interruption, using independent native sessions |

## Quick start

### 1. Install dependencies

- A Rust toolchain supporting the 2024 edition.
- The supported Google `localharness_external` binary, obtained separately.
- Codex CLI for the OS sandbox. Automatic review also requires Codex login and access to the review model.
- A Google API key, or personal-account credentials that have completed upstream onboarding.
- Bash and util-linux `setsid`, normally available on Linux / WSL, for cancellable command process groups.

The production service does not need Node.js or Python. Node.js 24+ is used only for examples, source export, and optional integration checks.

```sh
cargo build --release --locked
npm install -g @openai/codex
# Required if using automatic review:
codex login
```

Codex 0.162.1 has been tested with this project. Other versions require sandbox and app-server connection compatibility checks.

### 2. Set up the native runtime

Download [Google's Linux x86_64 1.4.0 archive](https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.4.0-linux-x86_64.zip), extract it, and point the environment variable at `localharness_external`:

```sh
export AGY_EXTERNAL_BINARY="/absolute/path/to/localharness_external"
chmod +x "$AGY_EXTERNAL_BINARY"
export AGY_APP_SERVER_HOME="$HOME/.local/state/antigravity-app-server"
```

Startup checks the runtime against the pinned SHA-256 and rejects other builds:

```text
30c80540147b7f6ce9157f8fbfce1ce5308e96b10602d4236f9108463a839373
```

This repository does not distribute Google's binaries, SDK, or credentials. Obtain and use them under their upstream terms.

### 3. Configure model access

Choose one of these methods:

```sh
# Supply the real value locally; do not commit it.
export GEMINI_API_KEY="YOUR_API_KEY"
```

```sh
# Existing personal-account credentials produced by upstream login.
unset GEMINI_API_KEY
export AGY_PERSONAL_CREDENTIALS="/absolute/private/path/acp_token.json"
```

Personal OAuth is experimental and takes precedence over an API key. The server does not provide first-time login, enterprise onboarding, or an account-management UI.

```sh
./target/release/antigravity-app-server --doctor
./target/release/antigravity-app-server
```

`--doctor` checks dependencies, the Codex connection, login status, model listings, and review policy. **It does not run inference** and cannot establish that every listed model is callable with the current account.

The service reads JSON-RPC from stdin. To try a minimal client:

```sh
# Calls a real model; defaults to read-only, no escalation, and a call budget.
node examples/client.mjs /absolute/workspace "Explain README.md"
```

## Architecture

```text
Your client / IDE / Web UI
          │ stdio JSON-RPC: Thread / Turn / Item
          ▼
 antigravity-app-server
   ├─ Session persistence, steering, Goals, instructions, skill discovery
   ├─ Permission policy ── Codex OS sandbox ── Commands and file changes
   ├─ Approval boundary ── Human client / independent Codex review session
   ├─ MCP bridge ── User-configured local or remote services
   └─ Local native connection ── localharness_external ── Model provider
```

Google's runtime runs the actual agent loop. The wrapper preserves native command arguments, output, and history, converts execution state into client events, and enforces permissions before execution. Models, tool availability, and quotas remain subject to upstream constraints.

Each server process exclusively owns its state directory and can host multiple threads. Do not share one `AGY_APP_SERVER_HOME` between running processes. stdout contains only protocol messages; diagnostics go to stderr. Closing stdin stops the service.

## Sandbox and approvals

### Permission modes

| Mode | File writes | Command network access | Actions outside the sandbox |
| --- | --- | --- | --- |
| `read-only` | Denied | Off by default | Require explicit escalation and approval; rejected with `never` |
| `workspace-write` (default) | Working directory and additional authorized roots | Off by default | Same as above |
| `danger-full-access` | Wherever the OS user can write | Unrestricted | Full access is already authorized; no per-action sandbox approval |

Restricted modes enforce **write and command-network boundaries**, not confidential workspace-only reads: filesystem rules permit reads from the root filesystem. Model traffic, native network tools, and external MCP traffic are not all governed by the command `networkAccess` setting.

Workspace-write additionally protects `.git`, `.agents`, and `.codex` under each writable root. `/tmp` and `TMPDIR` are not writable by default. For finer control, use `sandboxPolicy` instead of also passing `sandbox`:

```json
{
  "cwd": "/absolute/workspace",
  "sandboxPolicy": {
    "type": "workspaceWrite",
    "writableRoots": ["/absolute/shared-output"],
    "networkAccess": false,
    "excludeTmpdirEnvVar": true,
    "excludeSlashTmp": true
  },
  "approvalPolicy": "on-request",
  "approvalsReviewer": "user"
}
```

Directories must exist and use absolute paths. `cwd` is automatically included among writable workspace roots; `additionalDirectories` can add roots. If the installed Codex sandbox helper is unavailable or incompatible, restricted execution fails rather than falling back to unsandboxed commands.

### Execution and approval are separate controls

`approvalPolicy` accepts `on-request` or `never`. `approvalsReviewer` accepts `user` or `auto_review`; new sessions default to `auto_review`.

- Ordinary commands run inside the current sandbox. The model must explicitly supply `BypassSandbox` to request escalation for that invocation.
- Human approvals use server-initiated JSON-RPC requests. Approval applies to the corresponding action; session grants cover only matching arguments and expire on reconnect.
- Automatic approval uses an independent Codex session with the default `codex-auto-review` model and consumes separate quota. Evidence includes user intent, the proposed action, permissions, and relevant observations.
- Timeouts, errors, malformed responses, and stale decisions do not grant access. Steering and permission or connection changes can invalidate pending decisions.
- An approved escalation does not permanently grant full access to subsequent commands.

Automatic review is not a security proof. This is a Guardian-derived implementation, not a complete copy of Codex's internal Guardian. See [Guardian documentation](docs/guardian.md) for connection settings, evidence limits, cancellation, and review events.

### Plan mode

Set `collaborationMode.mode` to `"plan"`. Effective permissions become read-only with command networking disabled. File changes, sandbox bypass, autonomous Goals, and session external MCP are disabled. Returning to `default` restores the requested permissions. Plan mode uses execution checks as well as instructions to the model.

## Tools and commands

| Tool source | Loading and execution |
| --- | --- |
| Native reading and search | Native runtime definitions and execution for file reads, directory lookup, and text search |
| `run_command` | Native schema and history; restricted execution enters the Codex sandbox |
| File writes and replacements | External `write_to_file` / `replace_file_content` execute inside the sandbox in restricted modes; full access uses native file tools |
| Tasks and persistent terminals | Native `manage_task` / daemon commands only in full-access mode; restricted modes disable persistent terminals and terminal reuse |
| Skills | Enabled skill directories passed to the runtime; explicit client selection also supported |
| MCP | Configuration loaded on session connection; tools and resources exposed through a local bridge |
| `harness_goal.goal_update` | Built-in Goal status tool; omitted in Plan and tool-disabled sessions |

Restricted commands use `/bin/bash --noprofile --norc -c` with a cleaned environment retaining essentials such as `PATH`, `HOME`, and locale. Do not rely on interactive aliases, startup scripts, a previous command's `cd`, or shell variables. Supply the working directory and required variables in each call.

Ordinary foreground commands run in separate process groups with runtime-owned execution leases. Interruption revokes the lease and stops the process group; late commands cannot start. Explicit persistent terminals and terminal reuse in full-access mode remain under native runtime management and are outside that foreground cancellation guarantee.

Restricted modes disable `manage_task` because the current runtime's terminal-input path does not pass every action through the same approval hook. Failed commands produce failed items. Uncertain operations after interruption or disconnect are not automatically replayed.

There is no separate `/command` registry or automatic loading of Codex custom slash-command / prompt directories. Clients can expand shortcuts into text or explicit skill input. CLI `-c` options are startup configuration, not model tools.

### MCP configuration

Provide configuration through `thread/start.config.mcp_servers`. Updates replace the entire server map for that thread:

```json
{
  "config": {
    "mcp_servers": {
      "local": {
        "command": "/absolute/path/to/mcp-server",
        "args": ["--stdio"],
        "env_vars": ["SERVICE_TOKEN"],
        "disabled_tools": ["delete_record"]
      },
      "remote": {
        "url": "https://mcp.example.com/mcp",
        "bearer_token_env_var": "REMOTE_MCP_TOKEN",
        "tool_timeout_sec": 60
      }
    }
  }
}
```

Supported features include stdio, HTTP JSON/SSE responses, tool allow/deny lists, environment settings, and headers. stdio subprocesses do not inherit the full environment. Prefer explicit secret forwarding through `env_vars` over secrets in persisted `env` settings. Unknown MCP outcomes are not automatically retried.

MCP programs and services are separately trusted by the user and **are not inside the file-command sandbox**. Plan disables session MCP; an ordinary read-only session does not make its MCP tools read-only. CLI configuration also supports `-c 'mcp_servers.NAME.KEY=VALUE'`; see the [protocol reference](docs/protocol.md).

## Instructions and skills

### AGENTS.md

The wrapper reads instructions when creating or reconnecting a native session; the model does not need to fetch them:

1. Global: the first nonempty `AGENTS.override.md` or `AGENTS.md` in `CODEX_HOME` (default `~/.codex`).
2. Project: directories from the nearest ancestor containing a `.git` marker down to `cwd`.
3. At each level: `AGENTS.override.md`, then `AGENTS.md`, then configured fallback filenames. At most one file is selected per level; an empty project override still shadows the ordinary file.
4. Global instructions come first, then increasingly specific directories, with source paths attached. More specific rules take precedence on conflict. Without a project-root marker, only `cwd` project instructions are read.

The project instruction budget defaults to 32 KiB; global instructions do not count toward it. These settings may appear in `CODEX_HOME/config.toml` or in persisted thread `config` overrides:

```toml
project_doc_max_bytes = 32768
project_doc_fallback_filenames = ["TEAM_RULES.md"]
project_root_markers = [".git"]
```

Instructions are appended to Google's native base prompt; Codex's system prompt does not replace it. `baseInstructions` explicitly replaces the base prompt; `developerInstructions` adds application instructions. Google's runtime does not have an identical OpenAI developer-role hierarchy, so this does not establish full role-precedence equivalence.

Edits take effect on the next native connection, not on an ordinary next turn or unchanged resume of a loaded thread. To refresh, unload an idle persistent thread and resume it. This does not implement Codex's complete trust/profile/project configuration stack or automatically discover `GEMINI.md`.

### Skills

Discovery order:

1. `<cwd>/.agents/skills/*/SKILL.md`
2. `<cwd>/.codex/skills/*/SKILL.md`
3. `AGY_SKILLS_ROOT/*/SKILL.md`, or `~/.agents/skills/*/SKILL.md` if unset

The first skill with a given name wins. Directory symlinks are supported. Skills require frontmatter with `name` and `description`; common scalar and multiline descriptions are supported, not full YAML. Discovery scans only these directories, not every ancestor.

`skills/list` returns descriptions, paths, enabled states, and discovery errors. `skills/config/write` persists enable/disable settings; unload all sessions in the process before changing them. Explicit skill input looks like:

```json
{"type":"skill","name":"example-skill","path":"/absolute/skills/example-skill/SKILL.md"}
```

Include it in `turn/start.input`. The name and path must match a discovered, enabled skill. Skills provide instructions, not additional execution permissions or automatic dependency installation.

## Persistent Goals

A Goal lets a loaded thread keep working toward an explicit objective without the client sending another continuation after each turn.

```json
{"id":10,"method":"thread/goal/set","params":{"threadId":"THREAD_ID","objective":"Complete the authorized fix and run relevant tests","tokenBudget":20000}}
{"id":11,"method":"thread/goal/get","params":{"threadId":"THREAD_ID"}}
{"id":12,"method":"thread/goal/set","params":{"threadId":"THREAD_ID","status":"paused"}}
```

The server starts another turn when the Goal is `active` and the thread is loaded and idle, including the first turn after setting a Goal. The model explicitly reports `complete` or `blocked`, with a reason, using `harness_goal.goal_update`. Ending an ordinary response does not automatically complete the Goal.

| Status | Meaning |
| --- | --- |
| `active` | Automatic continuation allowed when idle |
| `paused` | No new automatic turns; an existing turn is not interrupted |
| `complete` | Client or model explicitly reports completion |
| `blocked` | Explicitly blocked, or automatic continuation cannot proceed safely |
| `budgetLimited` | Reported token usage reaches the budget, or native budget exhaustion is reported |

Resume with `thread/goal/set` and `status: "active"`; remove with `thread/goal/clear`. Use `turn/interrupt` to stop the current turn immediately. Unloading stops scheduling but preserves a persistent thread's Goal.

`tokenBudget` prevents continuation based on reported usage; it is not a hard per-token cutoff, and the current turn may exceed it. A native `budget`, such as `maxModelCalls` or `maxTotalTokens`, can also be supplied at thread creation. Codex review usage is counted separately. If usage becomes unknown, a budgeted Goal stops automatic continuation. Extended time/cost limits and registered verifiers are not implemented; `complete` is not independent verification.

## Subagents and independent workers

The model can use the following Codex V2-style tools. The wrapper manages agent relationships and messages, while model calls, context storage, resume, steering, and commands reuse native Thread / Turn execution.

| Tool | Behavior |
| --- | --- |
| `spawn_agent` | Accepts `task_name` and `message`; creates an independent native session and returns `agent_id` and `agent_name` |
| `send_message` | Delivers within the same agent tree; queues for idle agents and reaches running agents at input boundaries |
| `followup_task` | Delivers a task and starts a turn for an idle agent, or adds input to a running agent |
| `list_agents` | Lists tree members and statuses, optionally filtered by `path_prefix` |
| `wait_agent` | Asynchronously waits for messages, completion notices, or new user input; default 30 seconds, range 10 seconds to 1 hour |
| `interrupt_agent` | Stops a non-root agent's current task while retaining its identity and context |

Targets may be agent IDs, absolute paths such as `/root/review`, or task names relative to the current agent. Children expose their parent and path through `source.subAgent.thread_spawn`; `thread/list.ancestorThreadId` queries descendants. Tool start/completion uses standard `collabAgentToolCall` items. Clients may also call child-thread APIs such as `thread/read` and `turn/interrupt` directly.

Defaults: at most four running agents including the root, and one child level. Children inherit the model, working directory, instructions, MCP, and permissions at creation, but not Goals or existing approval grants. `fork_turns: "all"` (default) supplies attributed visible parent history as text, not a copy of Google's internal trajectory; context exceeding 256 KiB is rejected explicitly. `fork_turns: "none"` supplies only the task and allows `model` / `reasoning_effort` overrides. Only `agent_type: "default"` is currently available.

Ordinary messages do not wake an idle parent; use `wait_agent` to wait for results. Interrupting a root also interrupts its active children. Unloading a parent requires idle children and unloads them too. Persistent children can resume for follow-up tasks; unloaded ephemeral children cannot. Unknown execution outcomes require the existing reconcile flow and are not automatically replayed.

Native built-in subagent tools remain disabled: version 1.4.0 `kill` deregisters a child, and the public protocol lacks identity-preserving targeted interruption and queue-only messaging. The wrapper adds management tools, not a second model-execution loop. It follows Codex V2 collaboration semantics without legacy `send_input` / `wait` / `close_agent` aliases or full Codex configuration/history compatibility. Usage and native budgets are per thread; a parent Goal's token budget does not aggregate child usage.

Clients can also create one-off summary workers directly:

```json
{
  "cwd": "/absolute/workspace",
  "ephemeral": true,
  "approvalPolicy": "never",
  "approvalsReviewer": "user",
  "sandbox": "read-only",
  "config": {"tools_disabled": true, "mcp_servers": {}}
}
```

`tools_disabled` is enforced during configuration and dispatch, not only through prompts; structured `finish` remains available. Ephemeral threads are excluded from persistent listings. Their local native temporary state is cleaned on unload or normal exit; leftovers from forced termination are cleaned when the state directory is next opened. **Unloaded ephemeral threads cannot resume.** This does not promise provider-side zero retention.

Independent threads do not imply filesystem isolation: threads sharing a working directory can access the same files within their permissions. Clients should prepare separate directories or worktrees for isolated parallel changes. Independent server processes also need separate state directories.

## Client API

Transport is newline-delimited JSON-RPC, not ACP. Launch the process, call `initialize`, read its advertised methods and capabilities, then start a thread:

```json
{"id":1,"method":"initialize","params":{}}
{"id":2,"method":"model/list","params":{}}
{"id":3,"method":"thread/start","params":{"cwd":"/absolute/workspace","sandbox":"read-only","approvalPolicy":"never","approvalsReviewer":"user"}}
{"id":4,"method":"turn/start","params":{"threadId":"THREAD_ID","clientUserMessageId":"unique-message-id","input":[{"type":"text","text":"Explain the project structure"}]}}
```

Use the returned `thread.id` for `THREAD_ID`. Select an available model from `model/list`, then pass `model` and `reasoningEffort` to `thread/start`. Documentation examples are not a permanent model-availability list.

| Group | Methods |
| --- | --- |
| Lifecycle | `initialize`, `thread/start`, `thread/resume`, `thread/unload` |
| Query and organization | `thread/list`, `thread/read`, `thread/loaded/list`, `thread/name/set`, `thread/archive`, `thread/unarchive` |
| Configuration | `thread/settings/update`, revision-aware `thread/configure`, `config/read`, `collaborationMode/list` |
| Execution | `turn/start`, `turn/steer`, `turn/interrupt` |
| Recovery and context | `thread/reconcile`, `thread/inject_items` |
| Goals | `thread/goal/get`, `thread/goal/set`, `thread/goal/clear` |
| Models and accounts | `model/list`, `account/read`, `account/rateLimits/read`, `guardian/status` |
| Extensions | `skills/list`, `skills/config/write`, `mcpServerStatus/list`, `mcpServer/tool/call`, `mcpServer/resources/list`, `mcpServer/resource/read` |

Process `item/started`, `item/completed`, text deltas, and `thread/tokenUsage/updated` until `turn/completed`. A successful `turn/start` response means the request was accepted, not finished. The server also sends approval requests with IDs; respond to these rather than treating them as notifications.

Use `turn/steer` for input during execution, with the current `expectedTurnId` and a unique `clientUserMessageId`. Queuing, native acceptance, and consumption are separate stages. Check state after disconnect; a missing response does not establish that an action never ran and must not trigger blind replay.

See the [protocol reference](docs/protocol.md) for fields, input types, JSON Schema, approvals, events, and recovery examples. A minimal runnable client is in [examples/client.mjs](examples/client.mjs).

## Configuration and data

| Environment variable | Purpose |
| --- | --- |
| `AGY_EXTERNAL_BINARY` | Required path to the supported Google runtime |
| `AGY_APP_SERVER_HOME` | State directory; legacy `AGY_HARNESS_HOME` also accepted; otherwise `.antigravity-app-server` under the current directory |
| `GEMINI_API_KEY` | Google API key |
| `AGY_PERSONAL_CREDENTIALS` | Existing personal OAuth JSON; takes precedence over API key |
| `AGY_API_BASE_URL` | Alternative API-key endpoint, normally only for local fixtures |
| `CODEX_HOME` | Codex account/configuration and global instructions; default `~/.codex` |
| `AGY_CODEX_BINARY` | Shared override for the Codex executable |
| `AGY_CODEX_REVIEW_BINARY` / `AGY_CODEX_SANDBOX_BINARY` | Separate executable overrides for review and sandbox |
| `AGY_CODEX_CONNECTION` | `auto` / `daemon` / `stdio`; default `auto` |
| `AGY_CODEX_SOCKET` | Explicit Codex daemon Unix socket |
| `AGY_SKILLS_ROOT` | Alternative user-level skill directory |
| `AGY_MODEL_REQUEST_MAX_BYTES` | Personal OAuth request size limit; default 512 MiB |
| `AGY_MODEL_IDLE_TIMEOUT_SECS` | Personal OAuth timeout without response data; default 600 seconds |

Codex discovery checks explicit settings, PATH, and common user installation locations. `auto` prefers an existing daemon under the current Codex home, otherwise starts a private stdio process. Fallback occurs only before model calls; it does not restart shared services or automatically retry paid calls.

Personal OAuth requests are buffered in anonymous temporary files and forwarded as streams, requiring temporary disk space. Streams that continue producing data have no total-duration deadline. Images are preprocessed to a 2576-pixel side and a 5 MiB Base64 budget. See the protocol documentation for further attachment limits.

State includes user messages, tool arguments/results, native history, and approval evidence, and may contain sensitive information. `transport.jsonl` records only sizes, timing, stages, and error metadata, not request bodies or credentials. Protect the state directory and do not publish raw sessions or `.tmp` probe reports.

## Development, validation, and publication

Default checks make no real model calls and do not require the Google runtime, Codex, Node.js, or a frontend project. Tests require loopback access:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

With the native runtime and Codex installed, run local model-fixture integration checks:

```sh
cargo build --locked
node scripts/check-native-permissions.mjs
node scripts/check-guardian.mjs
node scripts/check-native-steer.mjs
node scripts/check-models-goal.mjs
node scripts/check-native-instructions.mjs
# Subagent lifecycle, independent-session recovery, and inherited sandbox;
# local model fixtures, no inference charges.
node scripts/check-native-subagents.mjs
```

These default scripts do not consume real model quota. Unit tests, native requests against model fixtures, and real-account checks provide different evidence; release notes should identify which were run. See [CONTRIBUTING](CONTRIBUTING.md) for contribution practices and explicit live checks.

Export public source through the allowlist:

```sh
node scripts/export-source.mjs
```

This creates a separate source directory, excluding Git history, Google binaries, protocol-investigation artifacts, frontend integration investigations, and local test data. It also checks common secret patterns and private paths. **Do not publish the development directory or reuse its old Git history directly.** Pattern scanning only detects covered patterns and cannot establish the absence of all sensitive content.

## Known limits

- Linux x86_64 and a pinned runtime; no native Windows/macOS support commitment.
- Explicit JSON-RPC method and DTO subset, not automatic compatibility with every Codex client.
- One child level, four concurrent agents, and visible-text history inheritance. No custom slash-command registry or complete Codex configuration/plugin system.
- Instruction-loading behavior can align with Codex, but Google's native system prompt and role semantics differ.
- MCP, native network tools, and authorized full-access execution are not all isolated by the command sandbox.
- Interruption cannot undo completed side effects. Unknown outcomes require inspection and explicit recovery, not automatic replay.
- Intended for one trusted local user; stdio is not an authenticated multi-user network API.

## Codex references and development

**This project relies extensively on [OpenAI Codex](https://github.com/openai/codex) as an implementation reference and reuses parts of its implementation. The project itself was vibe-coded with Codex.** That includes development of the code, documentation, and verification scripts through Codex-assisted iteration.

The reuse takes several forms:

- **Protocol and behavior:** Codex's app-server, Thread / Turn / Item model, steering, instruction loading, Goals, and V2 subagent collaboration informed the compatibility layer. The wrapper implements a selected subset for Google's native runtime.
- **Runtime components:** restricted commands invoke the installed Codex OS sandbox; automatic review connects to Codex app-server and its review model.
- **Adapted source:** the Guardian assessment parser and schema derive from Codex's `ext/guardian-reviewer/src/assessment.rs`. Their Apache-2.0 attribution and license are retained. See the [source provenance](src/guardian/NOTICE.md) for the pinned upstream revision and adaptations.

Vibe coding describes how the project was developed, not a guarantee of correctness or complete Codex parity. Validation evidence and current limitations are documented above. Credit for the upstream designs and reused components belongs to their authors; this is not an official Google or OpenAI project.

## License and security

Project-authored code is [MIT-licensed](LICENSE). Guardian-derived code retains Apache-2.0; see [NOTICE](NOTICE) and [source provenance](src/guardian/NOTICE.md). The third-party runtime is distributed separately. See [SECURITY](SECURITY.md) for security boundaries and vulnerability reporting.
