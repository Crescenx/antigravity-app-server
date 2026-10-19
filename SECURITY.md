# Security and supported use

This is an experimental local developer tool for a single trusted user on
Linux x86_64 (including WSL2). Its stdio RPC interface is not an authenticated
multi-user server. Do not expose it directly over a public network or pass
untrusted clients access to the RPC stream, environment or state directory.

Commands and file mutations use the installed Codex sandbox helper. Missing
or incompatible helpers fail closed. `danger-full-access` and approved sandbox
bypass intentionally allow execution with the local user's permissions.
MCP servers are separately trusted programs/services configured by the user;
the filesystem sandbox is not a sandbox for arbitrary MCP implementations.

Automatic approval is a model-based decision, not a proof of safety. Review
errors do not approve actions. Plan enforces read-only commands, denies file
changes and sandbox bypass, and disconnects session MCP/child tools. Stop
prevents subsequent tool launches; it cannot undo completed side effects or
guarantee immediate cancellation of remote inference.

State contains prompts, tool arguments, outputs, review evidence and session
history. Keep the state directory private. These may contain sensitive data
even when explicit API-key fields are never logged. Do not publish state,
OAuth files, model caches or raw local test reports. Auto-review sends its
selected evidence to the configured Codex model provider.

Local service discovery uses WebSocket framing on the local Codex control socket, or
starts a private stdio process. It never scans ports or restarts the shared
daemon. Cancellation targets only the newly created review thread. Shared
daemon cleanup is best effort if the connection is lost; the host still
refuses stale decisions.

For suspected vulnerabilities, use the hosting platform's private security
reporting feature when available. Otherwise contact the repository owner
privately before sharing exploit details or private session data. This source
package does not yet promise a security response SLA or stable public API.
