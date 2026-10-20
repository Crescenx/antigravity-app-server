# Stdio protocol

The execution backend is Antigravity App 2.24.0's Linux x86_64 `language_server`,
selected by `AGY_APP_CORE_BINARY` and pinned to SHA-256
`4a3854e31d77200ccb1cd4c44cb5c01611c07d81006586b40c1ab1f2c9e80713`.
It exposes local Connect RPCs for Cascade lifecycle and indexed state updates.
The public stdio Thread / Turn / Item interface remains the client boundary.
SDK / ACP `localharness_external` execution and the wrapper's model proxy have
been removed. `AGY_EXTERNAL_BINARY`, `AGY_PERSONAL_CREDENTIALS`,
`AGY_API_BASE_URL`, `AGY_MODEL_REQUEST_MAX_BYTES`, and
`AGY_MODEL_IDLE_TIMEOUT_SECS` no longer configure model execution.

Run `antigravity-app-server --login`, open the printed Google OAuth URL, and
paste the authorization code if requested. Native token creation ends the command;
Ctrl-C cancels. Authentication belongs to the App core;
the auth home is `AGY_APP_AUTH_HOME`, defaulting to `app-auth` beneath the service
state directory. Its native token file is `.gemini/jetski-standalone-oauth-token`.
Use the same auth home for login and ordinary runs. Legacy ACP tokens and API-key
proxy credentials are not imported. `--doctor` checks binary identity, token-file
presence, and Codex review connectivity without testing Google authentication or
inference. Upstream requests and refresh are managed by the native App.

New threads carry `runtime: "app-core-v1"`. Older SDK records remain readable,
but are read-only for execution; create a new App thread to continue work.
Migration does not silently replay or convert old native histories.

`StreamAgentStateUpdates` supplies complete replacements at absolute step indices.
The adapter reconciles the complete indexed snapshot on reconnect with a fresh subscriber ID; reconnects
do not resend user input. The root public thread ID is separate from
`nativeTrajectoryId`, which addresses native approvals. `fullyIdle` after input
observation ends a turn; an initial idle snapshot or transport EOF does not prove
completion. Native configuration credentials are redacted from exported events,
including raw App snapshots; user messages and tool output remain private data.

User `image` and `localImage` inputs share preprocessing: images already within
2576 pixels on each side and 5 MiB of Base64 retain their original bytes; larger
images are resized to both budgets. PNG retains transparency; other resized
formats use JPEG quality 85 (GIF uses its first frame, noted in the media
description). Input images are limited to 20,000,000 bytes, 64 million pixels,
and 32768 pixels per dimension. MIME mismatches are rejected. These wrapper
budgets apply before sending App input; native tool reads keep native behavior.
PDF/audio bodies and repeated history attachments are preserved in full.

Native `viewFile` steps appear as `dynamicToolCall` items named `view_file`, with
raw arguments and readable `contentItems`; `result` supplies the same summary
for older clients. Displayed line ranges are one-based. Native terminal errors
populate `turn.error.message` as well as `turn.nativeStop`.

Native `run_command` preserves its model-visible schema and original command
history. Restricted execution rewrites only the execution-side command to the
existing Codex OS sandbox. Approval binds the original and rewritten arguments,
connection generation, turn, and current permissions; a missing or stale binding
is denied. A session grant covers identical arguments only and expires on
reconnection. `never` denies restricted bypass requests. Nonzero command exits
produce failed command items; interruption records an unknown outcome without
replaying the operation.

Restricted sessions retain external sandboxed file write/edit tools and reject
persistent or reused terminals; native `manage_task` is disabled because its
input route bypasses the runtime's policy and hook callbacks. Full-access
sessions use native file and task tools. Changing permissions reconnects the
same native conversation with the appropriate tool configuration.

One JSON object per line. Requests use `id`, `method`, `params`; responses echo `id` with `result` or `error`. The optional `jsonrpc` field must be `"2.0"`. Notifications have `method` and `params`. Initialize advertises supported methods.

```json
{"id":1,"method":"initialize","params":{}}
{"id":2,"method":"thread/start","params":{"cwd":"/absolute/workspace","model":"gemini-2.5-flash","sandbox":"read-only","approvalPolicy":"never","approvalsReviewer":"user"}}
{"id":3,"method":"turn/start","params":{"threadId":"RETURNED_ID","clientUserMessageId":"unique-request-id","input":[{"type":"text","text":"Explain README.md"}]}}
```

Use the returned `thread.id`. Observe `item/started`, `item/completed`, `thread/tokenUsage/updated` and `turn/completed`. A successful turn/start response means accepted, not completed. Persist request identities; unknown execution outcomes must not be automatically replayed.

Native `thinking` is exposed through Codex-style `reasoning` items with
`summary: [text]` and `content: []`. Streaming uses
`item/reasoning/summaryPartAdded` and `item/reasoning/summaryTextDelta`
(`summaryIndex: 0`), bounded by `item/started` and `item/completed`.
The reasoning item ID is the native step ID plus `:reasoning`; any user-facing
answer retains the original step ID as an `agentMessage`. Tool-only model
steps do not produce empty messages. Historical reads apply the same mapping.
Full item snapshots replace prior content when the native text is revised;
replayed snapshots do not duplicate summary deltas. This is presentation
compatibility, not a claim that native thinking and Codex summaries have the
same generation process.

Sessions: `thread/list`, `thread/read`, `thread/resume`, `thread/unload`, `thread/name/set`, `thread/archive`, `thread/unarchive`. Read with `{threadId}`; resume reattaches persisted App session state. Legacy SDK records cannot resume execution.

Control: `turn/interrupt` uses `{threadId,turnId}`. `turn/steer` requires `threadId`, `expectedTurnId`, a unique `clientUserMessageId`, and the input array. Steering is durable and FIFO; dispatch acknowledgement is separate from provider consumption.

Configuration: `thread/start` and idle `thread/settings/update` accept `sandbox` (`read-only` or `workspace-write`), `approvalPolicy` (`on-request` or `never`), and `approvalsReviewer` (`user` or `auto_review`). Plan mode cannot execute writes.

`thread/start` accepts `ephemeral: true`. These threads are kept in memory and
excluded from `thread/list` and persistent thread/event files. The native runtime
uses an isolated temporary directory under `ephemeral/`; `thread/unload`, normal
exit, and SIGTERM clean it up. After a forced kill, the next process owning the
state directory removes leftover temporary state instead of resuming it. This is
a local lifecycle guarantee, not a provider-side data-retention setting. An
ephemeral thread cannot be resumed after unload or process exit.

`baseInstructions` replaces the selected built-in declarative prompt sections;
`developerInstructions` adds a separately labeled custom application section. Both accept
text (up to 128 KiB) or null to clear. They persist across turns; changes reconnect
the idle native session. `collaborationMode.settings.developer_instructions` also
accepts text. Google has no separate OpenAI developer-role tier: application
instructions are mapped to native prompt sections, with Plan restrictions appended
when applicable. Tools and sandbox permissions remain enforced independently.

The App planner uses `declarativeMixinConfig` for selected built-in and custom
prompt sections, tools, and required execution hooks. Its
`customizationDiscoveryConfig` sets `inheritUser:false` for rules, skills, MCP,
hooks, plugins, agents, and workflows. The wrapper explicitly supplies approved
skill paths, MCP servers, and its pre-tool authorization hook. It does not inherit
arbitrary extensions from the authentication home. Output/model retry counts are
configured to zero; a reconnect never authorizes a second user-input dispatch.

Native session creation and reconnection also load user/project rules without
requiring model tool calls. Global rules come from `CODEX_HOME` (default
`~/.codex`): the first nonempty `AGENTS.override.md`, otherwise `AGENTS.md`.
Project discovery walks from the nearest ancestor containing `.git` to the
thread's working directory. Each directory contributes its first existing
`AGENTS.override.md`, `AGENTS.md`, or configured fallback file. An empty project
override still shadows its sibling. With no root marker, only the working
directory is searched. Rules above the project root are excluded.

Rules are appended in global-to-local order with source paths, preserving the
native Google base prompt unless `baseInstructions` explicitly replaces it.
They are labeled user guidance; Google has no separate role tier for these
sections. More specific rules take precedence, and tool permissions remain
enforced separately. An unloaded thread reads fresh files on resume; an already
loaded thread does not reload rules on each turn or a no-op resume.

Discovery reads `project_doc_max_bytes` (default 32768, project content only),
`project_doc_fallback_filenames` (default `[]`), and `project_root_markers`
(default `[".git"]`) from `CODEX_HOME/config.toml`. The same keys under RPC
`config` override these values and persist with the thread. Zero disables
project rules but retains global rules; empty root markers search only the
working directory. Filenames and markers must be single path components.
Truncated project content is labeled. This supports Codex rule discovery,
not its entire layered config/trust/profile system, model prompts, or remote
environment discovery. Rule-file edits take effect at the next native
connection, not during an active turn.

`turn/start.outputSchema` accepts an object-root JSON Schema up to 64 KiB and
applies to that turn only. It adds native prompt instructions requesting a JSON
object; the wrapper validates the final `agentMessage.text`. Missing or invalid
JSON fails the turn rather than claiming success. External schema references
are not fetched. This is not the former SDK `finishToolSchemaJson` contract or
a guarantee of provider-enforced structured generation.

For isolated workers, `config: {"tools_disabled": true, "mcp_servers": {}}`
disables native filesystem, command, browsing, subagent, and skill tools and
prevents MCP startup. Plain final responses, including validated JSON, remain available. This
setting is enforced at tool dispatch too; it is not merely a prompt instruction.
Each app-server process still needs its own state directory, including a forked
worker. The Viewer creates a temporary directory and uses these same APIs for
Google short titles without attaching the main session's files or history.

Manual approvals have a server-issued `id` and method `item/commandExecution/requestApproval` or `item/fileChange/requestApproval`. Reply `{"id":SERVER_ID,"result":{"decision":"decline"}}` to reject, or `"accept"` for the action. Client disconnect cancels pending approvals/reviews.

Discovery: `model/list`, `config/read`, `account/read`, `guardian/status` (optional `threadId`) and `collaborationMode/list`. Guardian status checks connectivity without inference.

`model/list` reads the App's authenticated `GetAvailableModels` catalog, caches
it for five minutes, and accepts `limit`, `cursor`, and `includeHidden`.
Agent-recommended models appear by default; `includeHidden: true` also includes
the remaining account-advertised entries. Discovery does not run inference.

Encoded model variants are grouped under a canonical model name with
`supportedReasoningEfforts` and `defaultReasoningEffort`, for example:

```json
{"model":"claude-sonnet-5-5","reasoningEffort":"high"}
```

The native request uses the exact advertised `claude-sonnet-5-5-high` ID.
`turn/start` and settings updates also accept the standard `effort` field;
`collaborationMode.settings.reasoning_effort` is supported. Only account-advertised
combinations can be selected. Older raw suffixed IDs remain accepted and persisted;
responses expose canonical names and efforts. A legacy `-thinking` variant maps to
`high` as a compatibility alias, without implying an equivalent reasoning budget.
Gemini IDs with encoded levels are converted the same way. Missing/null effort
preserves an existing valid selection or uses the catalog default.

`account/rateLimits/read` returns the Codex-shaped `rateLimits` and
`rateLimitsByLimitId`. Two independent entries, `antigravity-google` and
`antigravity-external`, each have `primary` (300 minutes) and `secondary` (10080
minutes). `usedPercent` is consumed quota, and `resetsAt` is Unix seconds.
Unavailable windows stay null. Data comes from the App's
`RetrieveUserQuotaSummary`. Google is the legacy top-level `rateLimits`
entry; it must not be treated as a combined allowance for all models.

`thread/inject_items` accepts application-context messages with `type: "message"`,
`role: "developer"`, and text `content` entries of `type: "input_text"` on an idle
thread. This supported subset is persisted without starting inference. The next
turn, including the first automatic Goal turn, consumes it exactly once alongside
that turn's input. Historical content is labeled as untrusted context and is not
new user authorization. A switching client can await injection, then activate
the Goal; no extra user message or bootstrap inference is needed. Unknown
injection outcomes must be reconciled rather than automatically replayed.

Also available: `thread/goal/get|set|clear`, `skills/list`, `skills/config/write`, `mcpServerStatus/list`, `mcpServer/tool/call`, `mcpServer/resources/list`, `mcpServer/resource/read`. This is a protocol subset, not full Codex DTO compatibility.

Goal `tokenBudget` prevents automatic continuation based on reported usage;
unknown usage blocks a budgeted Goal. It is not a per-token hard cutoff.
Thread `budget.maxModelCalls` and `maxToolCalls` limit logical model generations
and tool calls. `maxInputTokens`, `maxOutputTokens`, and `maxTotalTokens`
use a native post-invocation hook to inspect complete usage and stop before the
next generation. Limits and lifetime/forward baselines persist across restart.
The current generation and its tools may exceed the limit; no per-token hard
cutoff is promised. Missing usage stops continuation.
`usageUpdate.modelCallCount` counts root-trajectory `chatModel` generator records,
not steps, retry attempts, independent reviews, or child-thread calls. Missing
generator coverage or usage remains unknown rather than being reported as zero.

One process owns its state directory. Closing stdin stops the server. Raw native journals contain user content and should remain private.

### Collaboration tools (Codex V2 style)

Default sessions expose `spawn_agent`, `send_message`, `followup_task`,
`list_agents`, `wait_agent`, and `interrupt_agent` to the model. These are tools,
not additional JSON-RPC methods. Plan mode and `tools_disabled` disable them.
Each agent runs an ordinary independent native thread under its inherited
permissions; the wrapper composes the existing create/resume, steering,
interrupt and sandbox paths. Native built-in subagent scheduling is disabled.

| Tool | Arguments | Result |
| --- | --- | --- |
| `spawn_agent` | `task_name`, `message`; optional `fork_turns`, `model`, `reasoning_effort`, `agent_type` | `agent_id`, canonical `agent_name` |
| `send_message` | `target`, `message` | `{}` after durable queueing; does not wake an idle target |
| `followup_task` | `target`, `message` | `{}`; starts an idle non-root target or steers its running turn |
| `list_agents` | optional `path_prefix` | `agents` with `agent_name`, `agent_id`, `agent_status` |
| `wait_agent` | optional `timeout_ms` (default 30000, minimum 10000, maximum 3600000) | `message`, `timed_out` |
| `interrupt_agent` | `target` | `previous_status`; preserves the non-root target's identity/history |

Targets resolve by ID or canonical path inside the same tree; relative task
names resolve under the caller's path. Task names use lowercase ASCII letters,
digits and underscores. Duplicate names are rejected; reuse with `followup_task`.
Four concurrent agents including the root and one child level are supported.
Only `agent_type: "default"` is currently available. `fork_turns` accepts `all`
(default) or `none`. `all` injects the visible parent transcript as attributed
text, subject to the 256 KiB context limit; it is not a native trajectory clone.
Model/effort overrides require `none`. Permissions, workspace, rules and MCP
configuration are inherited; Goal state and granted approvals are not copied.

Child threads carry `source.subAgent.thread_spawn` with `parent_thread_id`,
`depth`, `agent_path`, `agent_nickname`, and `agent_role`. They support ordinary
thread/turn APIs. `thread/list` supports `ancestorThreadId` and the
`subAgent`/`subAgentThreadSpawn` source filters. Collaboration items use
`type: "collabAgentToolCall"` and normal `item/started`/`item/completed` events.
Children's command approvals and results belong to their own thread IDs.
Completion reports are delivered to the parent mailbox with the sender's name.
Idle recipients retain messages until their next turn. A running wait ends on
mailbox activity, new input, timeout or interruption, without blocking other
threads. Dispatch receipts prevent replay after ambiguous failures.

Root interruption also interrupts active children. Parent unload requires idle
children and unloads them; durable children resume on a later followup. Ephemeral
children inherit the parent's ephemeral lifetime and cannot resume after unload.
Each thread reports its own usage; parent Goal budgets do not aggregate children.
These tools do not implement legacy `send_input`/`wait`/`close_agent` aliases or
Codex's complete agent configuration and native context-forking formats.

Foreground native commands use a private revocable execution lease and a separate
process group (`setsid`). Interruption or disconnect revokes the lease; a delayed
launch then refuses execution and an active foreground group is terminated. This
compensates for native halt occasionally leaving its shell alive. Explicit
persistent terminals and terminal reuse in full-access sessions retain native
lifecycle behavior and are outside this foreground cancellation guarantee.
