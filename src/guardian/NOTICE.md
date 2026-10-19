# Guardian source provenance

OpenAI Codex — Copyright 2025 OpenAI.

Upstream: https://github.com/openai/codex
Pinned source commit: `8c076942cb70fba8e5c4732c23f1faabc4aed484`.
License: Apache-2.0, included in `LICENSE-APACHE`.

`assessment.rs` is adapted from
`codex-rs/ext/guardian-reviewer/src/assessment.rs`. Changes: protocol enums are
local, the tool-enabled output prompt is omitted, and local tests are supplied.
The tolerant parser, defaults and schema are retained. The transport expands the
schema's required fields because public app-server structured output is strict.

`host.rs` and `transport.rs` are project-specific implementations informed by
upstream `core/src/guardian/{reviewer_config,review_session}.rs` and
`ext/guardian-reviewer/{routing,deadline,model}.rs`. They do not copy the full
Codex session runtime, extension registry, telemetry, retry or pooling subsystem.
This is a Guardian-derived integration, not an invocation of Codex's internal
Guardian entrypoint or a claim of full behavioral parity.

The model catalog supplies the installed account's current Guardian policy at
runtime. No policy text, account credentials, or model weights are vendored here.
