# Repository guide

## Project

Yawl is one Cargo package with a Rust 2024 library target and binary target for macOS and Linux. It uses blocking I/O and targets Rust 1.97.1 or newer. Keep the binary small and the direct dependency count low.

## Layout

- `src/main.rs`: binary bootstrap, session selection, and TUI/print-mode dispatch
- `src/cli.rs`, `src/print_mode.rs`: binary-local argument parsing and streamed text presentation
- `src/agent.rs`: model/tool turn loop and application orchestration
- `src/provider/mod.rs`: stable provider facade and re-exports
- `src/provider/types.rs`, `streaming.rs`, `resolution.rs`, `http.rs`: provider-neutral protocol, retries, provider selection, and SSE/HTTP support
- `src/provider/anthropic.rs`, `openai.rs`: provider-specific wire translation
- `src/provider/codex/`: Codex facade with separate OAuth and Responses modules
- `src/config.rs`: effective `Config` facade and stable re-exports
- `src/config/types.rs`, `schema.rs`, `loading.rs`, `storage.rs`, `change.rs`: runtime types, on-disk schema, merge logic, JSON storage, and validated mutations
- `src/tui/mod.rs`: `tui::run` facade and top-level event/submission coordination
- `src/tui/commands.rs`, `completion.rs`, `picker.rs`, `state.rs`, `worker.rs`: TUI behavior and state
- `src/tui/render.rs`, `terminal.rs`, `events.rs`, `input.rs`, `transcript.rs`: frame composition, terminal lifecycle, input decoding/editing, and transcript reduction
- `src/tui/markdown.rs`, `highlight.rs`, `tool_view.rs`: sanitized Markdown, syntax highlighting, and tool presentation
- `src/onboarding.rs`, `src/onboarding/`: onboarding coordination, model discovery, and terminal prompts
- `src/tools/`: built-in tools and executable tool discovery
- `src/session.rs`, `src/compaction.rs`: append-only sessions and context compaction
- `README.md`: user-facing behavior, contracts, and a concise architecture map

Most tests live in `#[cfg(test)]` modules beside their implementation. TUI cross-module tests use focused `*_tests.rs` modules under `src/tui/` so production visibility stays narrow.

## Commands

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo run -- --help
cargo install --path .
```

Run `cargo fmt --all` after editing Rust. After making code changes, run `cargo install --path .` before manual testing so the local `yawl` command uses the new build. Do not edit generated files under `target/`.

## Code rules

- Follow existing module boundaries and Rust naming conventions. Organize by responsibility, not by file size alone.
- Keep `main.rs`, `provider/mod.rs`, `config.rs`, `tui/mod.rs`, and `onboarding.rs` as facades. Put implementation in their private child modules.
- Preserve established public paths when moving code. Re-export from the facade instead of forcing callers to follow the internal layout.
- Prefer sibling visibility through `pub(super)` over widening internal APIs to `pub(crate)` or `pub`.
- Prefer the standard library over a new dependency. Commit `Cargo.lock` when dependencies change.
- Return errors for recoverable failures. Avoid `unwrap()` in production paths.
- Keep blocking I/O interruptible where the surrounding code supports `Ctrl+C`.
- Add a `// SAFETY:` comment to every `unsafe` block and keep its scope minimal.
- Add focused tests for behavior changes. Preserve provider streaming, session replay, config merging, and terminal escape sanitization invariants.
- Update `README.md` when changing CLI behavior, configuration, slash commands, or the exec-tool contract.

Before editing, check `git status` and preserve unrelated work. Before finishing, run the three validation commands above and review `git diff`.
