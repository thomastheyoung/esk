# Audit remediation execution team

This workflow validates findings before implementation and keeps each phase
independently shippable. It uses at most five agents/roles; one person or agent
may hold more than one role when the work is small.

## Team

1. **Audit validator** — re-checks each finding against current source,
   reproduces reported failures, records the exact acceptance test, and flags
   dependencies or scope changes.
2. **Init/config owner** — owns scaffold, config parsing, migration behavior,
   and init/doctor tests.
3. **CI/release owner** — compares CI commands and platform matrix with the
   release workflow, then verifies feature-gated binaries and MSRV/toolchain
   assumptions.
4. **Security-boundary owner** — owns secret redaction, encryption/key-provider
   changes, and tests proving values never reach persisted or rendered errors.
5. **Verification/release gate** — runs focused tests, feature/platform checks,
   reviews the diff for regressions, and records residual findings for the next
   phase.

## Workflow

```text
validate findings
      ↓
split by phase and dependency
      ↓
implement independent items in parallel
      ↓
integrate at shared boundaries
      ↓
run acceptance tests + release-parity checks
      ↓
ship phase / carry forward residual work
```

For this pass, the validator confirmed Phase 1.1, 1.2, and 1.3. The init and
CI owners are implemented directly; the security owner added shared redaction
at deploy/sync display and index boundaries. Later MCP, encryption-format,
tracker-concurrency, product-feature, dependency, and architecture findings
remain separate workstreams because they require additional design and
migration decisions.

## Gates

- `esk init` creates YAML that `Config::load` accepts.
- CI tests both `--no-default-features` and `--features mcp`.
- CI checks the default build on macOS and Windows, matching release targets.
- Forced target/remote failures do not put a known sentinel value in rendered
  errors or `deploy-index.json`/`sync-index.json`.
- Targeted tests, clippy, and relevant feature builds pass. The repository's
  existing `cargo fmt --check` reports formatting drift in untouched target
  files; changed Rust files are formatted and that unrelated drift is left for
  a separate cleanup.
