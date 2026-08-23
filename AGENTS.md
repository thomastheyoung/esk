# esk

Rust CLI for encrypted secrets management with multi-target deploy.

**Architecture, core design, conventions, and testing details live in [CLAUDE.md](CLAUDE.md).** Read it before making changes. This file covers only what is specific to working as an agent in this repo.

## Quick reference

```bash
cargo build --release
cargo test
cargo fmt --check
cargo clippy -- -D warnings    # CI gate: warnings are errors
```

CI runs `fmt --check`, `clippy -D warnings`, and `cargo test` on every push. Verify all three before considering a change complete.

## Reference docs

| Document                     | Covers                                        |
| ---------------------------- | --------------------------------------------- |
| [CLAUDE.md](CLAUDE.md)       | Architecture, traits, store format, testing    |
| [API.md](API.md)             | Every command, flag, and exit code             |
| [TARGETS.md](TARGETS.md)     | Deploy targets and per-target verify fidelity  |
| [REMOTES.md](REMOTES.md)     | Sync remotes                                   |
| [docs/llm.md](docs/llm.md)   | LLM-oriented reference (`esk llm-context`)     |

## Rules that bite

- **Never `git push` unless explicitly asked** — commit locally, let the user decide when to push.
- No `unwrap()` on fallible operations — propagate errors.
- No hardcoded project names or paths — everything comes from config.
- Never remove or weaken existing tests.
- Secret values must never reach logs, error messages, or `ps` output. Validation diagnostics are value-free by design.

## Issue Tracking

This project uses **bd (beads)** for issue tracking.
Run `bd prime` for workflow context, or install hooks (`bd hooks install`) for auto-injection.

**Quick reference:**

- `bd ready` - Find unblocked work
- `bd create "Title" --type task --priority 2` - Create issue
- `bd close <id>` - Complete work
- `bd sync` - Sync with git (run at session end)

For full workflow details: `bd prime`
