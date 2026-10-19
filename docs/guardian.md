# Independent automatic approval review

`src/guardian/` owns review evidence, model transport, assessment parsing and
action-bound decisions. The existing executor retains filesystem/network
enforcement. New threads and `config/read` default to `auto_review`; explicit
`approvalsReviewer: "user"` retains manual approval. Old saved threads missing
the field deserialize as `user`. Clients can explicitly update the reviewer
through thread settings.

This implements the previously deferred model-based review path. It is a
Guardian-derived port, **not the complete internal Codex Guardian pipeline**.
See [source attribution](../src/guardian/NOTICE.md).

## Configuration

On `thread/start`, `thread/resume`, or idle `thread/settings/update`:

```json
{
  "approvalsReviewer": "auto_review",
  "config": {
    "guardian": {
      "model": "codex-auto-review",
      "provider": "openai"
    }
  }
}
```

The default uses the existing Codex account through an existing local daemon when available, otherwise a private
`codex app-server` subprocess. Run `antigravity-app-server --doctor` to check
connectivity without inference. Set
`AGY_CODEX_REVIEW_BINARY` to select its executable. `CODEX_HOME` selects the
account/config directory; otherwise `$HOME/.codex` is used. The local model
catalog must contain the `codex-auto-review` policy. Missing policy fails closed.
No credentials are read by the harness or copied to the Google runtime.

Model and provider are independent of the Google worker model. A different
provider must already be configured in Codex and support the app-server's
structured-output contract. Model fallback is disabled. DeepSeek and other
providers have **not** been live-tested; changing the model name alone does not
establish equivalent approval accuracy. Upstream likewise has provider defaults
and `auto_review_model_override`; it is not intrinsically tied to one model.

## Execution and failure semantics

- Reviews run only at an existing approval boundary: explicit sandbox bypass
  requests or native dynamic policy requests. Ordinary sandboxed operations keep
  their existing behavior. `never`, Plan, and prior explicit full access retain
  their existing semantics; review cannot relax Plan.
- Each review captures the exact action, workspace, permission policy, user
  inputs, observations and steering messages. Observations are labeled as
  untrusted evidence. Review evidence over 256 KiB is rejected, not silently
  truncated. This initial implementation does not compact long histories.
- Every review owns a fresh ephemeral Codex thread and connection: read-only,
  approval `never`, reviewer `user` (no recursion), no environments/dynamic tools,
  shell/apps/web/multi-agent disabled, configured MCP servers disabled. Tool or
  approval requests from the reviewer invalidate the result.
  Shared daemon cancellation interrupts only the owned review turn and unsubscribes
  its thread; it never stops the daemon. Cleanup is bounded and best effort.
  Its working directory is Codex home, not the reviewed repository, and project
  instruction loading is disabled to avoid inheriting repository configuration
  or AGENTS.md as reviewer instructions.
- A 90-second deadline covers preparation and inference. Errors, malformed
  output, model substitution and timeout cannot approve execution. There is no
  automatic model-call retry or shared reviewer pool in this version.
- A decision binds to the issuing thread, connection generation, turn and
  revision. Steering invalidates an outstanding decision. Interrupt/disconnect
  cancels review jobs. Allow applies to one action, never a session-wide grant.
- Existing external-tool receipts prevent duplicate execution and replay of
  unknown outcomes. Explicit human grants remain scoped to identical arguments.
- Review start/completion records are appended to the native event journal.
  Completion records include assessment, model/provider, reviewer IDs and usage.
  Review tokens are separate from Google tokens and its native model-call budget.
  Notifications use `item/autoApprovalReview/started` and `/completed` with
  harness-specific payloads; this is not a claim of full Codex notification DTO parity.

## Validation

```sh
cargo test
cargo build
node scripts/check-guardian.mjs
node scripts/check-native-permissions.mjs
```

`check-guardian.mjs` uses the pinned native runtime, a local Google response
fixture, a deterministic Codex protocol fixture and the real executor. It checks
allow/deny, invalid output, reviewer tool attempts, cancellation, provider/model
selection and the isolation settings. It makes no paid calls.

Explicit real-account checks:

```sh
cargo test live_codex_allow_and_deny -- --ignored --nocapture
node scripts/check-guardian.mjs --live
```

The first reviews data only. The second connects two real Codex reviews to the
native fixture and actual executor, creating one temporary authorized file and
rejecting a synthetic exfiltration request. Neither test restarts production
services. Results and token usage are kept under `.tmp/guardian*`.
