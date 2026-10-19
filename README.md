# antigravity-app-server

**把 Antigravity 原生 agent runtime 接入熟悉的 Thread / Turn / Item 工作流。**

原生 ACP server 能对话，但把它接进一个完整的开发工具仍然费力：客户端还要处理会话恢复、运行中追加输入、工具审批、沙箱、使用量和长期任务。对已经围绕 Codex app-server 构建界面的应用来说，这些行为差异会直接变成适配成本。

这个项目的目标是：**提供一个行为上尽量接近 Codex 的 app server，同时保留 Google Antigravity 的原生模型调用、工具定义和会话执行。** 它是独立的 Rust stdio JSON-RPC 服务，客户端直接启动进程即可使用，不依赖特定前端。

当前支持 **Linux x86_64 / WSL2**，固定适配 **`localharness_external` 1.4.0**。这是社区项目，不属于 Google 或 OpenAI；实现的是 Codex 风格的协议子集，不保证完整 API、提示词或模型行为兼容。

[快速开始](#快速开始) · [沙箱与审批](#沙箱与审批) · [工具与命令](#工具与命令) · [规则与技能](#规则与技能) · [Goal](#持续任务-goal) · [子代理](#子代理与独立-worker) · [接口](#接口与客户端接入) · [协议参考](docs/protocol.md)

## 能做什么

| 能力 | 当前行为 |
| --- | --- |
| 会话与流式事件 | Thread / Turn / Item；持久化、恢复、卸载、命名、归档 |
| 运行中控制 | 中断、FIFO steering、请求去重、未知结果显式恢复 |
| 沙箱 | 只读、工作区写入、全权限；命令通过 Codex OS sandbox 执行 |
| 审批 | 人工审批或独立 Codex 自动审查；失败不放行 |
| 工作模式 | Default / Plan；Plan 强制只读并停用外部 MCP 和自主 Goal |
| 规则 | 自动加载全局与项目 `AGENTS.md`，支持 override、目录优先级和长度限制 |
| Skills / MCP | 本地技能发现、启停、显式技能输入；stdio / HTTP MCP |
| Goal | 持久目标、自动继续、暂停、完成、阻塞、token 预算 |
| 模型与附件 | 账户模型目录、思考强度、分组额度、图片及其他受支持附件 |
| 独立 worker | 临时会话、禁用工具、JSON Schema 输出；客户端可据此编排工作 |
| 子代理 | Codex V2 风格的创建、消息、后续任务、等待和中断；每个代理使用独立原生会话 |

## 快速开始

### 1. 准备依赖

- 支持 Rust 2024 edition 的工具链，用于从源码构建。
- Google `localharness_external` 的已适配版本，由用户单独取得。
- Codex CLI，用于受限命令的 OS sandbox；自动审批还需要 Codex 登录及相应模型权限。
- Google API key，或已经完成 onboarding 的个人账户凭据。
- Bash 与 util-linux 的 `setsid`（Linux / WSL 通常已安装），用于可取消的命令进程组。

生产服务不需要 Node.js 或 Python。Node.js 24+ 仅用于示例、源码导出和可选集成检查。

```sh
cargo build --release --locked
npm install -g @openai/codex
# 使用自动审批时，还需完成 Codex 登录：
codex login
```

本项目验证过 Codex 0.162.1；其他版本的 sandbox 与 app-server 连接兼容性需重新检查。

### 2. 准备原生 runtime

从 [Google 的 Linux x86_64 1.4.0 下载地址](https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.4.0-linux-x86_64.zip) 获取归档，解压后将环境变量指向 `localharness_external`，并赋予执行权限：

```sh
export AGY_EXTERNAL_BINARY="/absolute/path/to/localharness_external"
chmod +x "$AGY_EXTERNAL_BINARY"
export AGY_APP_SERVER_HOME="$HOME/.local/state/antigravity-app-server"
```

服务启动时严格校验 runtime SHA-256，不会静默使用其他版本：

```text
30c80540147b7f6ce9157f8fbfce1ce5308e96b10602d4236f9108463a839373
```

仓库不分发 Google 二进制、SDK 或凭据；使用者需遵守上游使用条件。

### 3. 配置模型访问

任选一种方式：

```sh
# API key；在本机通过环境变量提供真实值，不写入仓库。
export GEMINI_API_KEY="YOUR_API_KEY"
```

```sh
# 已有个人订阅凭据；此文件由上游登录流程生成。
unset GEMINI_API_KEY
export AGY_PERSONAL_CREDENTIALS="/absolute/private/path/acp_token.json"
```

个人 OAuth 路径属于实验性集成，优先于 API key。服务目前不提供首次登录、企业 onboarding 或账号管理界面。

```sh
./target/release/antigravity-app-server --doctor
./target/release/antigravity-app-server
```

`--doctor` 检查依赖、Codex 连接、登录状态、模型列表和审批策略，**不执行推理**；检查成功不等于账户上的每个模型都可实际调用。

服务等待 stdin 中的 JSON-RPC。交互式演示可使用：

```sh
# 会调用真实模型；示例默认只读、禁止提权，并设置调用预算。
node examples/client.mjs /absolute/workspace "解释 README.md"
```

## 运行架构

```text
你的客户端 / IDE / Web UI
          │ stdio JSON-RPC：Thread / Turn / Item
          ▼
 antigravity-app-server
   ├─ 会话持久化、steering、Goal、规则与技能发现
   ├─ 权限策略 ── Codex OS sandbox ── 本地命令与文件变更
   ├─ 审批边界 ── 人工客户端 / 独立 Codex 审批会话
   ├─ MCP 桥接 ── 用户配置的本地或远程服务
   └─ 本地原生连接 ── localharness_external ── 模型提供方
```

Google runtime 负责实际 agent 循环。包装层保留原生命令参数、输出和历史，把执行状态转成客户端可以消费的事件，并在执行前落实权限。模型名称、工具可用性和账户额度仍受上游约束。

每个服务进程独占自己的状态目录，可承载多个线程。不要让两个进程同时使用同一个 `AGY_APP_SERVER_HOME`。stdout 只输出协议消息，诊断走 stderr；关闭 stdin 会停止服务。

## 沙箱与审批

### 三种权限模式

| 模式 | 文件写入 | 命令联网 | 越权操作 |
| --- | --- | --- | --- |
| `read-only` | 拒绝 | 默认关闭 | 需显式申请并通过审批；`never` 拒绝 |
| `workspace-write`（默认） | 工作目录及额外授权目录 | 默认关闭 | 同上 |
| `danger-full-access` | 当前系统用户可写的位置 | 不限制 | 已明确授权全权限，不依赖逐次沙箱审批 |

受限模式是**写入与命令网络边界**，不是“只能读取工作区”的机密隔离容器：文件系统规则允许读取根目录。模型通信、原生网络工具和外部 MCP 的网络访问也不由命令的 `networkAccess` 开关统一隔离。

工作区写入模式额外保护每个可写根目录下的 `.git`、`.agents` 和 `.codex`。默认不把 `/tmp`、`TMPDIR` 加为可写目录。需要更精确的设置时，使用 `sandboxPolicy`，不要同时传 `sandbox`：

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

目录必须存在且为绝对路径。`cwd` 自动包含在工作区可写根中；`additionalDirectories` 也可增加根目录。安装的 Codex sandbox helper 不可用或不兼容时，受限执行失败，不退回无沙箱命令。

### 执行与审批是两层控制

`approvalPolicy` 支持 `on-request`、`never`；`approvalsReviewer` 支持 `user`、`auto_review`，新会话默认使用 `auto_review`。

- 普通命令在当前沙箱中执行。模型显式给出 `BypassSandbox` 才申请本次越权。
- 人工审批通过服务端发起的 JSON-RPC 请求完成。允许只作用于对应操作；会话授权也仅覆盖相同参数，并在重连时失效。
- 自动审批使用独立的 Codex 会话和默认 `codex-auto-review` 模型，消耗独立额度。审查证据包含用户意图、具体操作、权限和相关观察。
- 审批超时、错误、格式不合法或结果过期均不放行。Steering、权限变化或连接变化可能使待审批决定失效。
- 已批准的单次越权不会把后续命令永久切换为全权限。

自动审批不是安全证明。它采用 Guardian 派生实现，不是 Codex 内部 Guardian 的完整复制。连接配置、证据大小限制、取消和审查事件见 [Guardian 文档](docs/guardian.md)。

### Plan 模式

通过 `collaborationMode.mode: "plan"` 切换。Plan 的有效权限固定为只读、命令网络关闭；禁止文件修改、沙箱绕过和自主 Goal，且不启动会话外部 MCP。回到 `default` 后恢复请求的权限设置。它同时有提示词与执行检查，不只是给模型一句“不要修改”。

## 工具与命令

| 工具来源 | 加载及执行方式 |
| --- | --- |
| 原生读取与搜索 | 启用文件读取、目录查找和文本搜索；由原生 runtime 提供定义与执行 |
| `run_command` | 保留原生 schema 和命令历史；受限模式在执行前接入 Codex sandbox |
| 文件写入与替换 | 受限模式由外部 `write_to_file` / `replace_file_content` 在沙箱内执行；全权限使用原生文件工具 |
| 任务与持久终端 | 仅全权限启用原生 `manage_task` / daemon commands；受限模式禁止持久终端和终端复用 |
| Skills | 发现启用的技能目录，交给 runtime；也支持客户端显式选择 |
| MCP | 在连接会话时加载配置，通过本地桥接暴露工具与资源 |
| `harness_goal.goal_update` | 内置 Goal 状态工具；Plan 或禁用工具的会话不加载 |

受限命令使用 `/bin/bash --noprofile --norc -c`，清理环境后仅保留必要变量，如 `PATH`、`HOME` 和 locale。不要依赖交互 shell 的 alias、启动脚本、上一条命令的 `cd` 或 shell 变量。需要的工作目录与变量应在当前调用中明确提供。普通前台命令运行在独立进程组中，由运行时持有的执行许可控制取消：中断会撤销许可并停止该进程组，迟到的命令也不能再启动。全权限下显式请求的持久终端及终端复用仍交给原生 runtime 管理，不在这一前台命令取消保证内。

受限模式关闭 `manage_task` 的原因是：当前 runtime 的终端输入路径无法逐次经过同样的审批 hook，继续暴露会产生执行旁路。命令失败产生失败 item；中断或断线后的不确定操作不会自动重放。

本项目没有独立的 `/command` 注册表，也不自动加载 Codex 自定义 slash-command / prompt 目录。客户端可以将快捷命令展开为普通文本或显式 skill 输入；命令行的 `-c` 配置是启动参数，不是模型工具。

### MCP 配置

在 `thread/start` 的 `config.mcp_servers` 中提供配置；更新时整体替换该线程的服务器映射：

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

支持 stdio 和 HTTP JSON/SSE 响应、工具启用/禁用列表、环境变量和请求头配置。stdio 子进程不会继承完整环境；通过 `env_vars` 显式传递秘密，优先于把秘密写入持久化的 `env` 配置。MCP 失败不会自动重试未知结果。

MCP 是用户另行信任的程序或服务，**不在文件命令沙箱内**。Plan 停用会话 MCP；普通只读会话不等于其 MCP 工具也只读。CLI 也支持 `-c 'mcp_servers.NAME.KEY=VALUE'`，见 [协议参考](docs/protocol.md)。

## 规则与技能

### AGENTS.md

创建或重新连接原生会话时，包装层主动读取规则，无需模型自己调用工具：

1. 全局：在 `CODEX_HOME`（默认 `~/.codex`）读取第一个非空的 `AGENTS.override.md` 或 `AGENTS.md`。
2. 项目：从距当前工作目录最近的 `.git` 标记目录向下逐层读取，直到 `cwd`。
3. 每层：优先 `AGENTS.override.md`，其次 `AGENTS.md`，然后是配置的备用文件名。每层最多选一个；项目的空 override 仍会遮蔽同层普通文件。
4. 注入：全局在前，目录越深越靠后，发生冲突时更具体的规则优先；每段带来源路径。找不到项目根标记时，只读取 `cwd` 的项目规则。

默认项目规则总预算为 32 KiB，全局规则不占用此额度。以下三个键可写在 `CODEX_HOME/config.toml`，也可通过线程的 `config` 覆盖并持久化：

```toml
project_doc_max_bytes = 32768
project_doc_fallback_filenames = ["TEAM_RULES.md"]
project_root_markers = [".git"]
```

规则追加到 Google 原生基础提示词，不用 Codex 的系统提示词替换它。`baseInstructions` 是显式替换开关；`developerInstructions` 是额外的应用指令。Google 没有同构的 OpenAI developer-role 层级，不能由此推断完整角色优先级一致。

文件更改在下次原生连接时生效；已加载线程的普通下一轮和无变更 resume 不重新读取。需要刷新时，在空闲状态 unload，再 resume 持久线程。此实现不加载 Codex 的完整 trust/profile/项目配置栈，也不自动发现 `GEMINI.md`。

### Skills

发现顺序为：

1. `<cwd>/.agents/skills/*/SKILL.md`
2. `<cwd>/.codex/skills/*/SKILL.md`
3. `AGY_SKILLS_ROOT/*/SKILL.md`；未设置时使用 `~/.agents/skills/*/SKILL.md`

同名技能取先发现者，支持目录符号链接。技能需要包含 `name`、`description` 的 frontmatter；解析支持常用标量和多行 description，不是完整 YAML 实现。这里只扫描列出的目录，不递归向所有祖先查找技能。

`skills/list` 返回描述、路径、启用状态和发现错误。`skills/config/write` 持久化启停设置，修改前须卸载本进程全部已加载会话。显式使用技能：

```json
{"type":"skill","name":"example-skill","path":"/absolute/skills/example-skill/SKILL.md"}
```

将其放入 `turn/start.input`。路径和名称必须匹配已发现且启用的技能。技能提供指导，不增加执行权限；配置技能也不等于其所有依赖都已安装。

## 持续任务 Goal

Goal 让一个已加载线程围绕明确目标继续工作，不需要客户端在每轮结束后再次发送“继续”。

```json
{"id":10,"method":"thread/goal/set","params":{"threadId":"THREAD_ID","objective":"完成已授权的修复并运行相关测试","tokenBudget":20000}}
{"id":11,"method":"thread/goal/get","params":{"threadId":"THREAD_ID"}}
{"id":12,"method":"thread/goal/set","params":{"threadId":"THREAD_ID","status":"paused"}}
```

服务在 Goal 为 `active`、线程空闲且仍加载时自动发起下一轮，包括目标设置后的第一轮。模型通过 `harness_goal.goal_update` 携带原因明确报告 `complete` 或 `blocked`；一次普通回复结束不自动视为目标完成。

| 状态 | 含义 |
| --- | --- |
| `active` | 空闲后允许自动继续 |
| `paused` | 不再自动发起后续轮次；正在执行的一轮不会因此中断 |
| `complete` | 客户端或模型明确报告工作已完成 |
| `blocked` | 明确报告阻塞，或自动继续无法安全执行 |
| `budgetLimited` | token 使用量达到预算，或 native 报告预算耗尽 |

恢复使用 `thread/goal/set` 的 `status: "active"`；清除使用 `thread/goal/clear`。要立即停止当前轮，还需 `turn/interrupt`。卸载停止调度，但持久线程的目标会保留。

`tokenBudget` 是基于已报告用量阻止继续的预算，不是逐 token 的硬截断，当前轮可能超出额度。另可在创建线程时提供原生 `budget`（如 `maxModelCalls`、`maxTotalTokens`）；自动审批的 Codex 用量独立计算。用量变得未知时，带预算的 Goal 停止自动继续。扩展时间/费用限制和注册式验证器尚未实现；`complete` 也不是独立测试认证。

## 子代理与独立 worker

模型可使用以下 Codex V2 风格工具。包装层管理代理关系和消息，模型调用、上下文保存、恢复、steering 和命令执行复用原生 Thread / Turn 能力。

| 工具 | 行为 |
| --- | --- |
| `spawn_agent` | 接受 `task_name`、`message`，创建独立原生会话，返回 `agent_id` 和 `agent_name` |
| `send_message` | 向同一代理树投递消息；空闲代理只入队，运行中的代理在输入边界接收 |
| `followup_task` | 投递后续任务；空闲代理启动新一轮，运行中的代理接收追加输入 |
| `list_agents` | 查询本树代理状态，可按 `path_prefix` 筛选 |
| `wait_agent` | 异步等待消息、完成通知或新用户输入；默认 30 秒，范围 10 秒至 1 小时 |
| `interrupt_agent` | 停止指定非根代理当前任务，保留身份和上下文供后续使用 |

目标可使用代理 ID、绝对路径（如 `/root/review`）或相对当前代理的任务名。子代理通过 `source.subAgent.thread_spawn` 标明父线程和路径，`thread/list.ancestorThreadId` 可查询子线程；工具开始和完成使用标准 `collabAgentToolCall` item。客户端也可以直接使用子线程的 `thread/read`、`turn/interrupt` 等接口。

默认最多四个代理同时运行（含根代理），支持一层子代理。子代理继承创建时的模型、工作目录、规则、MCP 和权限；不继承 Goal 或已有审批授权。`fork_turns: "all"`（默认）将父会话的可见历史作为带来源的文本上下文传入，并非复制 Google 内部轨迹；超过 256 KiB 注入限制会明确失败。`fork_turns: "none"` 只传任务，允许 `model` / `reasoning_effort` 覆盖。当前只提供 `agent_type: "default"`。

普通消息不会唤醒空闲父代理；父代理应使用 `wait_agent` 等待结果。中断根线程会同时中断正在工作的子代理；卸载父线程要求子代理空闲，并卸载它们。持久子线程可由后续任务恢复，临时线程卸载后不可恢复。未知执行结果不自动重放，需要通过现有 reconcile 流程确认。

原生内置子代理工具仍被关闭：1.4.0 的 `kill` 会注销子代理，公开协议没有保留身份的定向中断，也没有只入队不唤醒的消息模式。这里仅补管理工具，未实现另一套模型调用循环。以上对应 Codex V2 协作语义，不提供旧版 `send_input` / `wait` / `close_agent` 别名，也不承诺完整 Codex 配置或内部历史格式兼容。用量和原生预算按各线程独立统计；父 Goal 的 tokenBudget 不自动汇总子代理费用。

客户端仍可直接创建一次性总结 worker，例如：

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

`tools_disabled` 在配置和分派时都禁用工具，不只是提示词要求；结构化 `finish` 仍可用。临时线程不进入持久线程列表，卸载或正常退出后清理本地 native 临时状态；强杀残留在状态目录下次打开时清理。**卸载后不能恢复临时线程**，也不承诺提供方的数据零保留。

这些机制适合客户端编排 worker，但不等于沙箱彼此隔离：同一工作目录上的多个线程仍能在其权限范围内访问相同文件。若要隔离并行修改，由客户端准备独立工作目录或 worktree；独立服务进程还必须使用各自状态目录。

## 接口与客户端接入

传输是逐行 JSON-RPC，不是 ACP。客户端启动进程后，先 `initialize`，从返回结果读取方法和 capabilities，再创建线程：

```json
{"id":1,"method":"initialize","params":{}}
{"id":2,"method":"model/list","params":{}}
{"id":3,"method":"thread/start","params":{"cwd":"/absolute/workspace","sandbox":"read-only","approvalPolicy":"never","approvalsReviewer":"user"}}
{"id":4,"method":"turn/start","params":{"threadId":"THREAD_ID","clientUserMessageId":"unique-message-id","input":[{"type":"text","text":"解释项目结构"}]}}
```

`THREAD_ID` 使用 `thread/start` 返回的 `thread.id`。可从 `model/list` 选择当前账户模型，再在 `thread/start` 传 `model` 和 `reasoningEffort`；不要把文档中的模型例子当成永久可用列表。

| 接口组 | 方法 |
| --- | --- |
| 生命周期 | `initialize`、`thread/start`、`thread/resume`、`thread/unload` |
| 查询与整理 | `thread/list`、`thread/read`、`thread/loaded/list`、`thread/name/set`、`thread/archive`、`thread/unarchive` |
| 配置 | `thread/settings/update`、带 revision 的 `thread/configure`、`config/read`、`collaborationMode/list` |
| 执行 | `turn/start`、`turn/steer`、`turn/interrupt` |
| 恢复与上下文 | `thread/reconcile`、`thread/inject_items` |
| 目标 | `thread/goal/get`、`thread/goal/set`、`thread/goal/clear` |
| 模型与账号 | `model/list`、`account/read`、`account/rateLimits/read`、`guardian/status` |
| 扩展 | `skills/list`、`skills/config/write`、`mcpServerStatus/list`、`mcpServer/tool/call`、`mcpServer/resources/list`、`mcpServer/resource/read` |

客户端持续处理 `item/started`、`item/completed`、文本 delta、`thread/tokenUsage/updated`，直到 `turn/completed`。`turn/start` 的成功响应仅表示已接受。服务也会向客户端发起带 `id` 的审批请求；这些请求需要响应，不能当作普通通知忽略。

运行中补充要求使用 `turn/steer`，提供当前 `expectedTurnId` 和唯一 `clientUserMessageId`。排队、native 接收和实际消费是不同阶段。断线后先检查状态；不能把缺少响应当作“操作未执行”然后自动重试。

详细字段、输入类型、JSON Schema、审批响应、事件和恢复示例见 [完整协议参考](docs/protocol.md)。最小可运行客户端见 [examples/client.mjs](examples/client.mjs)。

## 配置与数据

| 环境变量 | 用途 |
| --- | --- |
| `AGY_EXTERNAL_BINARY` | 必填；已适配的 Google runtime 路径 |
| `AGY_APP_SERVER_HOME` | 状态目录；兼容旧变量 `AGY_HARNESS_HOME`；均未设置时为当前目录下 `.antigravity-app-server` |
| `GEMINI_API_KEY` | Google API key |
| `AGY_PERSONAL_CREDENTIALS` | 已有个人 OAuth JSON 路径；优先于 API key |
| `AGY_API_BASE_URL` | API-key 模式的替代端点；通常只在本地 fixture 中设置 |
| `CODEX_HOME` | Codex 账号/配置、全局规则的根目录，默认 `~/.codex` |
| `AGY_CODEX_BINARY` | Codex 可执行文件公共覆盖路径 |
| `AGY_CODEX_REVIEW_BINARY` / `AGY_CODEX_SANDBOX_BINARY` | 分别覆盖审批和 sandbox 可执行文件 |
| `AGY_CODEX_CONNECTION` | `auto` / `daemon` / `stdio`，默认 `auto` |
| `AGY_CODEX_SOCKET` | 显式指定 Codex daemon Unix socket |
| `AGY_SKILLS_ROOT` | 替代用户级技能根目录 |
| `AGY_MODEL_REQUEST_MAX_BYTES` | 个人 OAuth 请求总大小上限，默认 512 MiB |
| `AGY_MODEL_IDLE_TIMEOUT_SECS` | 个人 OAuth 无响应数据超时，默认 600 秒 |

Codex 自动发现检查显式配置、PATH 和常见用户安装目录。`auto` 优先连接当前 Codex home 下已有 daemon，找不到时启动私有 stdio 进程；只在模型调用前回退，不重启共享服务，也不自动重试付费调用。

个人 OAuth 请求使用匿名临时文件缓存并流式转发，需要足够临时磁盘空间。持续有数据的流没有总时长截止；图片按 2576 边长和 5 MiB Base64 预算预处理。更多附件限制见协议文档。

状态包含用户消息、工具参数与结果、原生历史及审批证据，可能包含敏感信息。`transport.jsonl` 仅记录请求大小、耗时、阶段和错误元数据，不记录请求正文或凭据。保管好状态目录；不要公开原始会话或 `.tmp` 中的探测报告。

## 开发、验证与发布

默认检查不调用真实模型，也不需要 Google runtime、Codex、Node.js 或前端项目；测试需要 loopback 权限：

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

安装原生 runtime 与 Codex 后，可运行本地模型 fixture 集成验证：

```sh
cargo build --locked
node scripts/check-native-permissions.mjs
node scripts/check-guardian.mjs
node scripts/check-native-steer.mjs
node scripts/check-models-goal.mjs
node scripts/check-native-instructions.mjs
# 子代理生命周期、独立会话恢复和沙箱继承；本地模型夹具，不产生推理费用
node scripts/check-native-subagents.mjs
```

这些默认脚本不消耗真实模型额度。单元测试、fixture 原生请求验证和真实账户验证是三种不同证据；发布说明应明确是哪一种。贡献约定和显式 live 检查见 [CONTRIBUTING](CONTRIBUTING.md)。

公开源码从白名单导出：

```sh
node scripts/export-source.mjs
```

命令生成独立目录及 `SOURCE-MANIFEST.json`，不包含 Git 历史、Google 二进制、原生协议调查产物、前端集成调查或本地测试数据，并检查常见秘密和私人路径。**不要直接公开开发目录或沿用其旧 Git 历史**；开发材料不等于已清理的发布包。内容扫描只能发现已覆盖的模式，不能证明没有任何敏感信息。

## 已知边界

- Linux x86_64、固定 runtime 版本；没有原生 Windows/macOS 支持承诺。
- JSON-RPC 方法与 DTO 是明确实现的子集；不自动兼容任意 Codex 客户端。
- 子代理目前限一层、四个并发代理；历史继承使用可见文本上下文。没有自定义 slash-command 注册表或完整 Codex 配置/插件系统。
- 全局规则与项目规则加载可对齐，但 Google 原生 system prompt 和角色语义不同。
- MCP、原生网络工具及已授权全权限执行不受命令沙箱统一隔离。
- 中断不能撤销已发生的副作用；未知结果需要检查和显式恢复，不自动重放。
- 服务面向单个可信本地用户；stdio 不是带认证的多用户网络 API。

## 许可与安全

项目代码采用 [MIT](LICENSE)；Guardian 派生部分保留 Apache-2.0，详见 [NOTICE](NOTICE) 和 [来源说明](src/guardian/NOTICE.md)。第三方 runtime 独立分发。安全边界与漏洞报告方式见 [SECURITY](SECURITY.md)。
