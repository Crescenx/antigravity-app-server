# Contributing

Use a Rust toolchain supporting edition 2024. The default checks do not need
Codex, Google credentials, the Google runtime, Node.js, or a Viewer checkout:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests bind loopback ports and write fixtures below `.tmp/`. They do not call
real model providers. The ignored Rust live-review test and scripts with
`--live` consume account quota and must be invoked explicitly.

With Node.js 24+, the pinned Google runtime and Codex installed, additional
native integration tests use local response fixtures:

```sh
cargo build --locked
node scripts/check-native-permissions.mjs
node scripts/check-guardian.mjs
node scripts/check-native-steer.mjs
node scripts/check-models-goal.mjs
```

These scripts make no paid model calls. The model/Goal check verifies the exact
native model ID, reasoning round trip, context injection and first-turn budget.
Optional Viewer interoperability
checks in the development checkout are separate from the public standalone
test suite and require `VIEWER_ROOT`; they are not required to use this server.

Preserve unknown-outcome receipts, explicit approval boundaries, Plan mode
restrictions and cancellation checks. Add focused regression tests for changes
to those paths. Do not commit credentials, raw account data or private session
logs. Keep the Guardian Apache-2.0 notice with derived code.

The old `antigravity-harness` executable is a compatibility entry point for
existing clients; new integrations should use `antigravity-app-server`.
