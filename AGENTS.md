# Repository guide

## Project

Yawl is one Cargo package with a Rust 2024 library target and binary target for macOS and Linux. It uses blocking I/O and targets Rust 1.98.0 or newer. Keep the binary small and the direct dependency count low and avoid over-engineering the codebase.

Never spawn subagents if not explicitly requested by the user.

## Layout

- `src/main.rs`: binary bootstrap, session selection, and TUI/print-mode dispatch
- `src/cli.rs`, `src/print_mode.rs`, `src/project_trust.rs`: binary-local argument parsing, streamed text presentation, and project skill trust prompts
- `src/agent.rs`: stable `Agent` facade and public turn events
- `src/agent/conversation.rs`, `conversation/turn.rs`, `conversation/goal.rs`, `conversation/plan.rs`, `conversation/steer.rs`, `conversation/recovery.rs`, `conversation/context.rs`, `events.rs`: conversation lifecycle, model/tool turns, goal and planning modes, live steering, undo and tool-result recovery, context estimates, streamed event translation, and optional session persistence
- `src/prompt.rs`: compact system prompt, skill catalog, and `AGENTS.md` instruction injection
- `src/provider/mod.rs`: stable provider facade and re-exports
- `src/provider/types.rs`, `streaming.rs`, `resolution.rs`, `http.rs`: provider-neutral protocol, retries, provider selection, and SSE/HTTP support
- `src/provider/anthropic.rs`, `openai.rs`: provider-specific wire translation
- `src/provider/codex/`: Codex facade with separate OAuth, Responses, and remote compaction modules
- `src/config.rs`: effective `Config` facade and stable re-exports
- `src/config/types.rs`, `schema.rs`, `loading.rs`, `storage.rs`, `change.rs`: runtime types, on-disk schema, merge logic, JSON storage, and validated mutations
- `src/tui/mod.rs`: `tui::run` facade and top-level event/submission coordination
- `src/tui/commands.rs`, `completion.rs`, `files.rs`, `picker.rs`, `state.rs`, `worker.rs`: TUI behavior and state
- `src/tui/render.rs`, `terminal.rs`, `events.rs`, `input.rs`, `transcript.rs`: frame composition, terminal lifecycle, input decoding/editing, and transcript reduction
- `src/tui/markdown.rs`, `highlight.rs`, `tool_view.rs`, `status_bar.rs`: sanitized Markdown, syntax highlighting, tool presentation, and configurable status rendering
- `src/tui/git/jobs.rs`, `git/operations.rs`: cancellable Git workers, UI result merging, and blocking repository operations
- `src/tui/processes.rs`, `subagents.rs`, `git.rs`, `connection.rs`, `dashboard.rs`: background-process, subagent, and git dashboards, provider setup, and shared dashboard layout
- `src/onboarding.rs`, `src/onboarding/`: setup wizard coordination, arrow-key selection, model discovery, and terminal prompts
- `src/doctor.rs`, `src/doctor/`: configuration diagnosis, interactive repair, and report rendering
- `src/tools/`: builtin registry and executable-tool discovery
- `src/tools/files.rs`: bounded native file listing, literal text search, and paged UTF-8 reads
- `src/tools/git.rs`: fixed read-only Git inspection for restricted children
- `src/tools/exec.rs`, `src/tools/planning_shell.rs`, `src/tools/user_input.rs`, `src/tools/web.rs`: exec-tool contract, gated planning inspection, interactive question broker, plus isolated web search/fetch adapters and HTML cleanup
- `src/subagent/reports.rs`: durable large-report storage and bounded result excerpts
- `src/background.rs`, `src/subagent/`, `src/cancellation.rs`: session-bound shell processes, parallel subagents, and interrupt tokens
- `src/terminal_mode.rs`: shared raw-terminal lifecycle for the TUI and onboarding selector
- `src/skills.rs`, `src/trust.rs`, `src/image.rs`: skill discovery, project skill trust, and image input
- `src/session.rs`, `src/session/recovery.rs`, `src/compaction.rs`, `src/checkpoint.rs`: append-only sessions, recovery records, context compaction, and `/undo` working-tree snapshots
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
- Keep `main.rs`, `agent.rs`, `provider/mod.rs`, `config.rs`, `tui/mod.rs`, `onboarding.rs`, and `doctor.rs` as facades. Put implementation in their private child modules.
- Preserve established public paths when moving code. Re-export from the facade instead of forcing callers to follow the internal layout.
- Prefer sibling visibility through `pub(super)` over widening internal APIs to `pub(crate)` or `pub`.
- Prefer the standard library over a new dependency. Commit `Cargo.lock` when dependencies change.
- Return errors for recoverable failures. Avoid `unwrap()` in production paths.
- Keep blocking I/O interruptible where the surrounding code supports `Ctrl+C`.
- Add a `// SAFETY:` comment to every `unsafe` block and keep its scope minimal.
- Add focused tests for behavior changes. Preserve provider streaming, session replay, config merging, and terminal escape sanitization invariants.
- Advertise `web_search` and `web_fetch` in the system prompt only when the current registry actually contains those builtins, not merely when `web_browsing` is enabled in config.
- Update `README.md` when changing CLI behavior, configuration, slash commands, or the exec-tool contract. Update this file when the module layout above changes.

Before editing, check `git status` and preserve unrelated work. Before finishing, run the three validation commands above and review `git diff`.
