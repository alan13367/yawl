# Repository guide

## Project

Yawl is one Cargo package with a Rust 2024 library target and binary target for macOS, Linux, and Windows. It uses blocking I/O and targets Rust 1.99.0 or newer. Keep the binary small and the direct dependency count low and avoid over-engineering the codebase.

Never spawn subagents if not explicitly requested by the user.

## Layout

- `src/main.rs`: one-line binary that calls `yawl::main`. `src/lib.rs` declares every module crate-private and exports only `main`, so the compiler reports unused code
- `src/app.rs`: startup, session selection, and TUI/print-mode dispatch
- `src/cli.rs`, `src/project_trust.rs`: argument parsing with `--help` text and output formats, and project trust resolution (the question itself is a `tui/setup.rs` screen; inspection resolves it quietly)
- `src/print_mode.rs`: one-shot runs: prompt and bounded stdin reading, `--timeout`, the outcome, and the JSON result object. `print_mode/text.rs` streams plain text, `print_mode/json.rs` turns turn events into JSON lines, and `print_mode/report.rs` collects the final answer, tool counts, edited files, and usage from the events
- `src/exit_status.rs`: process exit codes and the error kinds machine-readable output reports
- `src/inspect.rs`, `src/inspect/`: non-turn commands for people and other agents: `--check` readiness, `--list-models` with optional provider probes, and the session, preset, and tool listings
- `src/skill_export.rs`: the compiled-in `skill_export/SKILL.md` that teaches other harnesses to call Yawl, with `--print-skill` and `--install-skill` targets. Yawl's own skill discovery skips it
- `src/agent.rs`: `Agent` facade: one `run(TurnRequest)` entry point for every kind of turn, plus session, undo, and config operations
- `src/agent/conversation.rs`, `events.rs`: conversation state over a session (a file for the main agent, memory-only for subagents), session switching, undo, and streamed turn events
- `src/agent/conversation/turn.rs`, `conversation/step.rs`: the turn lifecycle (`TurnRequest`, preamble, per-step preparation, provider request with retries, tool execution, compaction) and one model step (`classify` plus one handler per step kind)
- `src/agent/conversation/goal.rs`, `conversation/plan.rs`, `conversation/steer.rs`, `conversation/recovery.rs`, `conversation/context.rs`: goal and planning helpers, live steering, undo and tool-result recovery, and context estimates
- `src/prompt.rs`: compact system prompt, skill catalog, and `AGENTS.md` instruction injection
- `src/provider/mod.rs`: stable provider facade and re-exports
- `src/provider/types.rs`, `streaming.rs`, `resolution.rs`, `http.rs`: provider-neutral protocol, retries, provider selection, and SSE/HTTP support
- `src/provider/http/interruptible.rs`, `http/ready.rs`, `http/scenarios.rs`: `cancellable_agent`, whose connector fails blocked reads within 100 ms of cancellation or a stall without signals; the Windows TCP transport that waits with `WSAPoll` instead of receive timeouts; and the request scenarios both connector chains are tested against
- `src/provider/anthropic.rs`, `openai.rs`: provider-specific wire translation
- `src/provider/codex/`: Codex facade with separate OAuth, model catalog, Responses, and remote compaction modules. `codex/auth/windows.rs` opens the login page with `ShellExecuteW`
- `src/config.rs`: effective `Config` facade and stable re-exports
- `src/config/types.rs`, `schema.rs`, `loading.rs`, `storage.rs`, `change.rs`: runtime types, on-disk schema, merge logic, JSON storage, and validated mutations
- `src/tui/mod.rs`: `tui::run` facade and idle submission handling, which returns the job to start
- `src/tui/controller.rs`: the single event loop. It lends `&mut Agent` to a scoped worker thread for each job (turn, `/compact`, subagent follow-up) and takes it back on completion, so busy-mode code cannot reach the agent
- `src/tui/commands.rs`, `picker.rs`: slash-command and picker facades with dispatch and shared models
- `src/tui/commands/table.rs`: every slash command defined once: names, `/help` and completion text, and whether it runs while busy
- `src/tui/commands/settings.rs`, `commands/copy.rs`: settings parsing/application plus usage summaries, and transcript-to-clipboard formatting
- `src/tui/commands/modes.rs`, `commands/queue.rs`, `commands/sessions.rs`, `commands/diff.rs`: `/init`, `/goal`, and `/plan`; queued-message editing; new, resumed, and deleted sessions with undo reports; and `/diff` cards
- `src/tui/picker/settings.rs`, `picker/status_bar.rs`: settings/color/provider picker construction and status-bar editor construction
- `src/tui/picker/models.rs`, `picker/render.rs`, `picker/field.rs`: model and reasoning pickers with background Codex catalog refresh, picker rendering, and single-line picker fields
- `src/tui/completion.rs`, `files.rs`, `state.rs`: completion, file mentions, and view state
- `src/tui/worker.rs`: busy-mode input: steering, mid-turn commands and picker actions against the config snapshot, pending questions, and cancellation
- `src/tui/render.rs`: frame-composition facade and coordinator
- `src/tui/render/cache.rs`: rendered-entry reuse, invalidation, row indexing, and cached image lookup
- `src/tui/render/entries.rs`: individual transcript entry rendering, image preview layout, and user/queued/steer panels
- `src/tui/render/reasoning.rs`: thinking tags, accent-derived thinking colors, duration labels, and reasoning trace formatting
- `src/tui/render/transcript.rs`: visible transcript windows, scrolling and selection reveal, pending inputs, and loading indicators
- `src/tui/render/welcome.rs`, `render/questions.rs`: welcome animation with project details, and question composer
- `src/tui/render/activity.rs`: compact background-terminal and subagent activity strips above the composer
- `src/tui/render/composer.rs`, `render/scroll.rs`: composer borders, mode labels, key hints, and plan handoff choices; transcript scroll bar and the scroll-to-bottom pill
- `src/tui/terminal.rs`, `events.rs`, `input.rs`, `transcript.rs`: terminal lifecycle, input decoding/editing, and transcript reduction. `terminal/windows.rs` copies text through the Win32 clipboard
- `src/tui/markdown.rs`, `highlight.rs`, `status_bar.rs`: sanitized Markdown, syntax highlighting, and configurable status rendering
- `src/tui/theme.rs`: dark and light palettes for fixed interface colors, terminal background detection, and accent darkening on light backgrounds
- `src/tui/tool_view.rs`: generic tool-card rendering facade
- `src/tui/tool_view/calls.rs`, `tool_view/card.rs`, `tool_view/diff.rs`: per-tool call headers and result summaries, card painting and wrapping, and edit-diff calculation with context elision
- `src/tui/git.rs`: Git dashboard entry points, shared state and geometry, and refresh coordination
- `src/tui/git/repository.rs`, `git/jobs.rs`, `git/operations.rs`: Git subprocess execution and data loading, cancellable workers, UI result merging, and blocking repository actions
- `src/tui/git/input.rs`, `git/init.rs`, `git/render.rs`, `git/diff.rs`: dashboard input and commit editing, repository initialization, panel rendering, and diff wrapping/highlighting
- `src/tui/processes.rs`, `subagents.rs`, `connection.rs`, `dashboard.rs`: background-process and subagent dashboards, the provider connection flow shared by `/connect` and first-run setup (it shows screens through `ConnectView`), and shared dashboard layout
- `src/tui/setup.rs`: screens shown before a session starts, provider setup and the project trust question, as pickers behind a `Surface`. `setup/screen.rs` draws them full screen under the wordmark; `setup/lines.rs` asks numbered prompts on stderr for piped input
- `src/tui/remote.rs`: `/remote` command, merged host/remote input reader, and host lock screen
- `src/tui/remote/hub.rs`, `remote/server.rs`, `remote/http.rs`, `remote/address.rs`, `remote/qr.rs`: shared session state and input wake-ups between UI and server threads, pairing and SSE/POST routes, minimal keep-alive HTTP/1.1, Tailscale address discovery with pairing secrets, and the QR encoder for the pairing link. `remote/index.html` is the embedded page for phones and computers
- `src/tui/remote/unix.rs`, `remote/windows.rs`, `remote/address/{unix,windows}.rs`: the host terminal reader (`TerminalInput`), the input wake handle and wait (a self-pipe with `poll`, or an event with `WaitForMultipleObjects`), and interface listing with random bytes
- `src/onboarding.rs`, `src/onboarding/`: the setup outcome (configured or skipped), the provider catalog, and model discovery shared with `/connect`
- `src/doctor.rs`, `src/doctor/`: configuration diagnosis, interactive repair, and report rendering
- `src/tools/`: builtin registry and custom-tool discovery
- `src/tools/registry.rs`: per-step tool registry, tool entries and outcomes, and dispatch to each tool
- `src/tools/custom.rs`: custom tool manifests (validation, `{{param}}` shell-quoted expansion, trust-gated discovery cache) and the `define_tool` builtin
- `src/tools/catalog.rs`: per-scan caches for builtin entries, skills, and subagent presets
- `src/tools/output.rs`: bounded excerpts and saved artifacts for verbose command and web output
- `src/tools/shell.rs`: foreground/background shell schemas and execution
- `src/tools/files.rs`: native file-tool facade with shared argument, cancellation, and no-follow open helpers
- `src/tools/files/read.rs`, `files/write.rs`, `files/discovery.rs`: `read_file` whole, line-page, byte-page, and image reads; `write_file` and `edit_file`; restricted-child `list_files` and `search_files`. `files/open.rs` opens files for reading without blocking on FIFOs or devices or following a final symlink, and `files/test_support.rs` holds shared test fixtures
- `src/tools/git.rs`: fixed read-only Git inspection for restricted children
- `src/tools/mode.rs`: goal and plan single-call mode entries
- `src/tools/orchestration.rs`: subagent spawn/send/wait/cancel/list entries and dispatch
- `src/tools/skills.rs`: `read_skill` entry and execution
- `src/tools/exec.rs`: the one child-process runner (tools, Git inspection, the Git dashboard, and `/undo` checkpoints): process trees, deadlines, cancellation, head/tail output capture, and `shell_command`, which builds every shell invocation
- `src/tools/exec/unix.rs`, `exec/windows.rs`, `exec/git_bash.rs`: sessions and process groups with nonblocking pipes; Job Objects, suspended starts, and `PeekNamedPipe` polling; and Git for Windows `bash.exe` discovery and command-line quoting
- `src/tools/planning_shell.rs`, `src/tools/user_input.rs`: gated planning inspection and interactive question broker
- `src/tools/web.rs`: web tool facade with the shared HTTP agent, bounded body reads, cancellation, and untrusted-content delimiters
- `src/tools/web/search.rs`, `web/fetch.rs`, `web/html.rs`: DuckDuckGo, Brave, and Firecrawl search backends; page fetch with inline limits and saved full pages; HTML-to-Markdown extraction with bounded concurrency. `web/test_support.rs` holds local HTTP test fixtures
- `src/subagent/manager.rs`: subagent capacity, execution, waiting, and delivery facade
- `src/subagent/manager/capacity.rs`, `manager/execution.rs`, `manager/waiting.rs`, `manager/delivery.rs`, `manager/validation.rs`: admission and queueing, worker threads, blocking observers, deferred results, and input validation
- `src/subagent/manager/format.rs`: snapshot and deferred-result presentation
- `src/subagent/reports.rs`: durable large-report storage and bounded result excerpts
- `src/background.rs`, `src/subagent/`, `src/cancellation.rs`: session-bound shell processes, parallel subagents, and interrupt tokens. `background/{unix,windows}.rs` hold the process-tree `Group`, and `cancellation/{unix,windows}.rs` hold the Ctrl+C handler (SIGINT, or a console control handler) and the Unix `SIGUSR1` thread wake. `lib.rs` re-exports the interrupt helpers
- `src/terminal_mode.rs`: shared raw-terminal lifecycle for the TUI and setup screens, over `unix.rs` (termios) and `windows.rs` (console modes and the `ConsoleInput` key reader). `key_text.rs` turns console key records into UTF-8 on every platform so it can be tested
- `src/paths.rs`, `src/paths/windows.rs`: `canonicalize` without Windows verbatim prefixes, reserved device names, `replace_file`, and Windows file identity from `GetFileInformationByHandle`
- `src/skills.rs`, `src/trust.rs`, `src/image.rs`: skill discovery, project skill trust, and image input
- `src/text.rs`: terminal-safe text: the one rule for which characters (controls and bidirectional overrides) must not reach the terminal, and `json_line`, which escapes them in machine-readable output
- `src/session.rs`, `src/session/state.rs`: append-only session logs that own the conversation history. Every change is an event written first and then applied by `SessionState::apply`, the one reducer replay also uses
- `src/session/recovery.rs`, `src/session/plans.rs`, `src/session/index.rs`, `src/compaction.rs`, `src/checkpoint.rs`: recovery records, saved plan revisions and implementation handoffs, session-list metadata caching, compaction summaries, and `/undo` working-tree snapshots
- `README.md`: user-facing behavior, contracts, and a concise architecture map

Most tests live in `#[cfg(test)]` modules beside their implementation. TUI cross-module tests use focused `*_tests.rs` modules under `src/tui/` so production visibility stays narrow. `src/test_support.rs` holds crate-wide test helpers (unique temp directories and local HTTP request and response writers), and `src/tui/test_support.rs` holds the agent fixture shared by TUI tests. Git unit tests follow their owning child modules; `src/tui/git/test_support.rs` contains shared test-only repository helpers, and `src/tui/git_tests.rs` covers dashboard behavior across children.

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
- Keep `main.rs` a one-line call into the library, and keep library modules crate-private.
- Keep `agent.rs`, `provider/mod.rs`, `config.rs`, `tui/mod.rs`, `tui/git.rs`, `tui/picker.rs`, `tui/commands.rs`, `tui/render.rs`, `tui/tool_view.rs`, `tools/mod.rs`, `tools/files.rs`, `tools/web.rs`, `subagent/manager.rs`, `onboarding.rs`, and `doctor.rs` as facades. Put implementation in their private child modules.
- Preserve established public paths when moving code. Re-export from the facade instead of forcing callers to follow the internal layout.
- Prefer sibling visibility through `pub(super)` over widening internal APIs to `pub(crate)` or `pub`.
- Prefer the standard library over a new dependency. Commit `Cargo.lock` when dependencies change.
- Return errors for recoverable failures. Avoid `unwrap()` in production paths.
- Keep blocking I/O interruptible where the surrounding code supports `Ctrl+C`.
- Add a `// SAFETY:` comment to every `unsafe` block and keep its scope minimal.
- Add focused tests for behavior changes. Preserve provider streaming, session replay, config merging, and terminal escape sanitization invariants.
- Change session history or state only by recording an event; `SessionState::apply` is the only place an event takes effect, for live sessions and replay alike.
- Send untrusted text to the terminal through `crate::text`, and run child processes through `tools::exec::run`. Build shell invocations with `tools::exec::shell_command`, build every ureq agent with `provider::cancellable_agent`, and canonicalize paths with `crate::paths::canonicalize`.
- Keep OS-specific code in private `unix.rs` and `windows.rs` children selected with `#[cfg(unix)]` and `#[cfg(windows)]`, or in small paired functions, and keep the logic they share testable on every platform. Gate tests that need POSIX behavior with `#[cfg(unix)]`. Windows code must at least type-check for `x86_64-pc-windows-msvc`.
- Define slash commands only in `tui/commands/table.rs`.
- Advertise `web_search` and `web_fetch` in the system prompt only when the current registry actually contains those builtins, not merely when `web_browsing` is enabled in config.
- Keep machine-readable output stable: add JSON fields rather than renaming or removing them, print it through `crate::text::json_line`, and keep `skill_export/SKILL.md` in step with the CLI (its tests check every flag it names against `--help`).
- Update `README.md` when changing CLI behavior, configuration, slash commands, or the custom-tool manifest contract. Update this file when the module layout above changes.

Before editing, check `git status` and preserve unrelated work. Before finishing, run the three validation commands above and review `git diff`.
