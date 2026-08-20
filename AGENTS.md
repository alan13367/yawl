# Repository guide

## Project

Yawl is a single Rust 2024 crate for macOS and Linux. It uses blocking I/O and targets Rust 1.97.1 or newer. Keep the binary small and the direct dependency count low.

## Layout

- `src/main.rs`: CLI, print mode, and TUI startup
- `src/agent.rs`: model/tool turn loop
- `src/provider/`: Anthropic, OpenAI-compatible, and Codex providers
- `src/tools/`: built-in tools and executable tool discovery
- `src/tui/`: terminal input, rendering, Markdown, and tool views
- `src/config.rs`, `src/session.rs`, `src/compaction.rs`: persisted state and context management
- `README.md`: user-facing behavior and contracts

Tests live beside the code in `#[cfg(test)]` modules.

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

- Follow existing module boundaries and Rust naming conventions.
- Prefer the standard library over a new dependency. Commit `Cargo.lock` when dependencies change.
- Return errors for recoverable failures. Avoid `unwrap()` in production paths.
- Keep blocking I/O interruptible where the surrounding code supports `Ctrl+C`.
- Add a `// SAFETY:` comment to every `unsafe` block and keep its scope minimal.
- Add focused tests for behavior changes. Preserve provider streaming, session replay, config merging, and terminal escape sanitization invariants.
- Update `README.md` when changing CLI behavior, configuration, slash commands, or the exec-tool contract.

Before editing, check `git status` and preserve unrelated work. Before finishing, run the three validation commands above and review `git diff`.
