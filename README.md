# Yawl

<div align="center">
  <img src="https://raw.githubusercontent.com/alan13367/yawl/main/Assets/WelcomeScreen.png" alt="Yawl welcome screen in a terminal" width="800">
  <p><strong>A small AI coding harness that stays in your terminal.</strong><br>
  Stream answers, run tools, keep sessions, and bring your own model.</p>
  <p>
    <a href="https://github.com/alan13367/yawl/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-7aa2f7?style=flat-square" alt="MIT license"></a>
    <img src="https://img.shields.io/badge/Rust-1.98%2B-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust 1.98 or newer">
    <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-8fbcbb?style=flat-square" alt="macOS and Linux">
  </p>
</div>

Yawl is a terminal AI agent for macOS and Linux. It has a full-screen TUI and a scriptable print mode, with provider selection, persistent sessions, tool calling, and automatic context compaction. The tool set can grow without recompiling: drop an executable into `~/.yawl/tools/` or `./.yawl/tools/` and the model can call it on the next step.

<table>
  <tr>
    <td width="50%" align="center"><img src="https://raw.githubusercontent.com/alan13367/yawl/main/Assets/CommandsList.png" alt="Yawl command and skill completion menu"><br><sub>Type <code>/</code> to search commands and skills</sub></td>
    <td width="50%" align="center"><img src="https://raw.githubusercontent.com/alan13367/yawl/main/Assets/SubagentsDashboard.png" alt="Yawl subagent dashboard"><br><sub>The subagent dashboard</sub></td>
  </tr>
</table>

## Install

Requires Rust 1.98 or newer.

```sh
git clone https://github.com/alan13367/yawl.git
cd yawl
cargo install --path .
```

Run `yawl` and a setup wizard walks through provider, endpoint, and model. Built-in Anthropic and OpenAI read `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`; keys can also be saved in `~/.yawl/config.json` with mode `0600`, and the environment variable wins. `yawl --setup` reopens the wizard. `yawl --doctor` checks the config files and offers interactive repairs.

## Use it

```sh
yawl                                  # full-screen terminal interface
yawl "Explain this repository"        # one prompt, streamed plain text
git diff | yawl "Review this patch"   # stdin becomes context
yawl -c                               # resume the latest session here
yawl --session 20260820-093301-1a2b   # resume by ID, from any directory
yawl -m openai:gpt-4o "Release note"  # pick a model for one run
yawl --list-tools                     # show the current tool registry
```

Project skills stay disabled until you trust the repository. `yawl --trust-project` trusts them for one invocation without changing the saved decision, and can be used with `yawl --setup`. The same decision gates provider endpoints, base URLs, and credentials from `./.yawl/config.json`: a cloned repository cannot repoint model traffic or resolve `$ENV_VAR` references into headers until you trust it. Setup and first-run onboarding resolve trust before listing project providers. `yawl --help` has the full reference.

## Terminal controls

- `Enter` submits. During a response it steers the running turn at the next safe boundary. `Ctrl+G` does the same explicitly and works in legacy terminals, as does `Ctrl+Enter` where the terminal reports modifiers. `Tab` queues a message instead.
- `Shift+Enter`, `Alt+Enter`, and `Ctrl+J` insert newlines. Pastes over 400 characters or 8 lines appear as a `[Pasted #N]` marker in the editor; the model receives the full text.
- `Ctrl+V` attaches a clipboard image, up to five PNG, JPEG, GIF, or WebP files of 5 MB each per prompt. Its tinted `[Image #N]` tag moves and deletes as one token in the editor. Typing the same text does not attach an image. Image numbers continue across `/new` and `/resume` while Yawl stays open. On Linux this needs `wl-paste` or `xclip`.
- `/` opens the command and skill menu. `@` tags a project file for the model.
- `Tab` focuses the transcript when idle. Arrows move between blocks, `h`/`l` fold, `Enter` opens a viewer, `y` copies. `Ctrl+F` searches the transcript and `Ctrl+O` expands or collapses all tool output. Clicking a tool card expands or collapses only that card; clicking a thinking tag toggles just that thinking block.
- Drag to select text; releasing copies to the clipboard. The mouse wheel and `PageUp`/`PageDown` scroll.
- `Escape` or `Ctrl+C` aborts the active model response or tool. Neither exits Yawl.
- When the bell is on (the default), Yawl rings the terminal bell when a turn finishes and when the model asks a question, so an unfocused terminal window or tab announces activity. Turn it off with `/settings bell off` or Settings > Interface > Bell.
- `/undo` restores files changed through `write_file`/`edit_file` and drops that turn. `/diff` shows every file those tools changed this session as per-file diff cards. `/copy` and `/copy-all` put replies on the clipboard.
- Multiple-choice questions from the model replace the composer. A countdown accepts the recommended answers automatically so unattended turns continue; answering yourself cancels it.
- Active background terminals appear on a plain terminal-background row directly above the input box, without replacing its border or adding a gap. The row shows their count, latest process, and elapsed time when space allows. Running subagents get a matching row that names every child still working, so it shrinks as each one finishes; the status bar no longer lists them, and saved `active_subagents` status-bar items are ignored. `/ps` opens the background-process dashboard, `/subagents` the subagent dashboard, and `/git` the git dashboard, all while the model is busy.
- Git operations run in the background so typing and navigation remain responsive. `Ctrl+C` cancels the current Git operation. Results preserve commit-message edits made while the operation runs.
- Click `+` beside the `UNSTAGED` heading to stage all changes, including untracked files, just like the `a` shortcut.
- `/git` groups unstaged tracked changes and untracked files under Unstaged, with `U` marking untracked files. It refreshes the open diff as working or staged contents change. Large diffs fall back to compact hunks and show a warning if the preview is still truncated. Unstaging works before the first commit. Discarding untracked files preserves ignored files and nested repositories.
- In `/git`, moving the mouse highlights file and history rows without changing keyboard selection. Click a file or a commit to open its diff; `Esc` closes the current diff and a second `Esc` closes `/git`. History fills the bottom of the panel and pages in older commits as you scroll, including when a diff is open. Scroll over the history rows to browse older commits. Diff and log scrolling stop at the bottom and reverse immediately. Clickable controls use a hand cursor in Kitty, Ghostty, and foot; other terminals retain their own cursor. Hover requires mouse-motion reporting support.

The transcript renders Markdown with syntax highlighting for common languages. Tool calls use full-width cards: diffs for edits, image previews for `read_file` in terminals that support them.

Model reasoning renders as a collapsible thinking block, collapsed by default while streaming and after it settles. During streaming the tag shows a spinner and `Thinking` with no timer; once it settles it becomes `+ Thought: 4.2s` in a softer, hue-shifted shade of the accent colour. Neutral accents use a blue-gray tint to distinguish tags from reply text. Click the tag (or unfold the selected block with `l`) to show the traces under a dimmed `- Thought: 4.2s` header. An open block keeps streaming new traces until it settles. Durations persist with the session, so resumed transcripts show them; blocks without a recorded duration show just `+ Thought`. The `hide_reasoning` setting removes thinking blocks entirely.

## Slash commands

| Command | Effect |
| --- | --- |
| `/model [MODEL]` | Open a model picker, or switch directly when `MODEL` is given |
| `/reasoning [LEVEL]` | Show or set the reasoning effort for the current model |
| `/connect` | Configure a provider and model with guided setup |
| `/settings [KEY ...]` | Open the settings picker, or change a setting directly |
| `/new`, `/clear` | Start a new session in the same working directory |
| `/compact` | Summarize older messages now |
| `/usage` | Show token and prompt-cache usage |
| `/undo` | Restore files from before the last prompt and drop that turn |
| `/diff` | Show files changed through the file tools this session as diff cards |
| `/init` | Create or update `AGENTS.md` with project guidance for coding agents |
| `/copy`, `/copy-all` | Copy the last reply, or the whole conversation |
| `/tools`, `/skills` | List tools and skills |
| `/skill:NAME [ARGS]` | Run a discovered Markdown skill |
| `/subagents` | Open the subagent dashboard and takeover view |
| `/git` | Open the git dashboard with stage, undo, commit, push, history, and diff views; refreshes live as files change. Outside a repo it asks for a remote and runs the first-commit push (`init`, seeded `README.md`, `branch -M main`, `push -u origin main`) |
| `/ps` | Open the background-process dashboard |
| `/resume [ID\|NUMBER]` | Open the session picker or resume directly; `d` deletes a session |
| `/unqueue [NUMBER\|all]` | Edit, remove, or clear queued messages |
| `/goal [TEXT]` | Start, resume, cancel, or show a persistent goal |
| `/plan [TEXT]` | Start, resume, cancel, or show the planning workflow |
| `/help` | Show terminal controls and commands |
| `/hotkeys` | Show every keyboard shortcut, grouped by area |
| `/quit` | Exit and print the resumable `yawl --session ID` command |

`/settings` groups model, interface, context, provider, web, subagent, and skill options in one picker and applies generation-affecting changes before the next queued message. The status bar is customizable under Settings > Interface > Status bar, with live preview, reordering (`K`/`J`), and custom labels. The accent and selection color pickers preview the highlighted color across the whole interface as you move, so the composer border, status bar, and menus repaint before you commit; `Enter` saves it and `Esc` restores the previous color.

While a turn runs, `Enter` steers and `Tab` queues; queued messages show below the transcript. Canceling with Escape or Ctrl+C pauses waiting messages, including unaccepted steering, so they cannot immediately restart the turn. Use `/unqueue` and Enter to resume sending them. `/init` inspects the current directory and creates or curates `./AGENTS.md`; it keeps accurate project rules, removes stale or changelog-like material, and changes no other file. `/goal TEXT` keeps making model requests until the model calls an internal `goal_complete` tool; a plain reply does not finish it.

`/plan TEXT` runs a read-only planning mode that gathers requirements in batches of three multiple-choice questions, saves each finished plan revision as Markdown under `<session-directory>/<session-id>/plans/`, shows its path, and offers Implement or Return to editor. Question cards show a readable waiting state and answer summaries; the input panel separates option labels from their descriptions. Later prompts can revise or implement the active plan; unrelated prompts continue as ordinary turns without clearing it.

Starting implementation makes one additional summarization request, then replaces earlier model context with a brief summary and one copy of the plan while retaining the latest implementation request. The original transcript remains in the session log. Retries and resumed implementation reuse that handoff; later compaction can leave the saved path for rereading. If summarization or storage fails, implementation stops with the original context intact for retry. This reduces subsequent input size; billing savings depend on caching and implementation length.

## Models and configuration

Yawl merges `~/.yawl/config.json` with `./.yawl/config.json`, project over global. Built-in routes are `anthropic:`, `openai:`, and `openai-codex:`. Local presets are `ollama:`, `lmstudio:`, and `omlx:`, pointing at ports 11434, 1234, and 8000 on `127.0.0.1`. Model IDs may contain colons, so `ollama:llama3.1:8b` selects provider `ollama` and model `llama3.1:8b`.

```json
{
  "model": "omlx:Qwen3-Coder",
  "max_tokens": 8192,
  "reasoning_effort": "high",
  "auto_compact": true,
  "compact_threshold": 0.85,
  "context_windows": { "omlx:Qwen3-Coder": 65536 }
}
```

Every field is optional and validated on load. An out-of-range value fails startup and names the file and field; `yawl --doctor` is the repair path.

### Custom providers

`openai-codex:` uses a ChatGPT Plus or Pro subscription through device-code login (`yawl --login openai-codex`), with the credential stored in `~/.yawl/auth.json`. `/model`, `--setup`, and `/connect` refresh the account's Codex model catalog from OpenAI. Yawl saves the visible model IDs and their context, image, and reasoning metadata in `~/.yawl/codex-models.json` for use when the service is unavailable. A manual model ID remains available if discovery fails. Other OpenAI-compatible endpoints go under `providers`, using the same field names as pi's `models.json`:

```json
{
  "providers": {
    "omlx": {
      "baseUrl": "http://127.0.0.1:8000/v1",
      "api": "openai-completions",
      "apiKey": "$OMLX_API_KEY",
      "models": [
        {
          "id": "my-model",
          "reasoning_efforts": ["low", "medium", "high"]
        }
      ]
    }
  }
}
```

`apiKey` accepts `$ENV_VAR` and `${ENV_VAR}` references; omit it for keyless local servers. An optional `models` array supplies labels, token limits, and image support for `/model`; unlisted model IDs still work and remain text-only. In the `/model` picker, `d` or `Delete` removes the highlighted entry from its provider's `models` array in the global config after a confirmation; the active or default model setting is left unchanged, and Codex catalog models cannot be removed. `/connect` and Settings > Providers walk through the same setup interactively. `/settings provider NAME URL KEY` writes the direct form, with `-` in place of `KEY` removing a saved key.

For compatible providers, `--setup` and `/connect` ask which reasoning efforts the selected model accepts. Use arrows to move, Space to toggle, and Enter to confirm. Choices are `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, and `ultra`. The selections are saved in `providers.NAME.models[].reasoning_efforts` in `~/.yawl/config.json`; you can edit the array manually, or override it through a provider's `models` array in `./.yawl/config.json`. `reasoningEfforts` is also accepted. An empty or omitted list disables explicit reasoning effort for that model.

Codex models default to a 370,000-token context window when their catalog entry reports the older 272,000-token window. Spark keeps its smaller limit, other catalog limits remain unchanged, and `context_windows` can override any model.

Use `/reasoning` to choose an effort for the session, even while a response is running. A change made during a response applies to the next model request, including a follow-up prompted by a steering message; it cannot alter a request already streaming. Yawl does not print a confirmation notice over the running response. Use `/settings reasoning_effort high` to save a default in the top-level `reasoning_effort` field. Only levels listed for the active model are sent, using the Chat Completions `reasoning_effort` field. `default` or `off` lets the endpoint choose. After editing the config file, use `/settings reload` or restart Yawl.

## Sessions, undo, and compaction

Sessions are append-only JSONL under `~/.yawl/sessions/projects/<project-key>/`, scoped to the working directory. `-c` and `/resume` list sessions for the current directory; `--session ID` resumes from any directory. A resumed session continues with the model it last used, written as a `model_switch` event by `/model`, `/connect`, and `/settings model`; `-m MODEL` overrides it for that run, and a saved model whose provider is no longer configured falls back to the default. Messages, responses, reasoning, tool results, and usage are written as they happen; compaction records a summary without deleting the original log. Plan Markdown files are recoverable artifacts backed by the JSONL records; missing files are recreated when needed, revisions go through `/plan`, and deleting a session removes its plan files. If a tool result cannot be saved, the batch stops and Yawl retains the result in memory for a storage retry. Resuming an incomplete batch adds error results for missing outputs, marks execution as uncertain, and does not rerun those tools. `/usage` breaks out input, cache reads and writes, and output. Reopening the session picker reuses bounded in-memory metadata for unchanged logs; file changes invalidate the cache.

`/undo` saves a file's pre-image the first time `write_file` or `edit_file` touches it, capped at 32 MiB per file, and restores it on request. An undo intent is saved before restoration, and the checkpoint stays available until the history change commits. If interrupted, the next turn or `/undo` finishes the pending operation before starting another. In git repos where the agent moved `HEAD`, it soft-resets to the pre-turn commit. Files changed through `shell` or exec tools are not tracked. `/diff` uses the same pre-images: one diff card per touched file, comparing the oldest pre-image with the current contents, so files written back unchanged disappear and `/undo`-restored files drop out. Files that are non-UTF-8 or over 1 MiB on either side are listed in a notice instead of a card.

Before each request Yawl combines saved, model-specific token usage with estimates for newly added messages, tool output, and prompt changes. Older sessions without usage metadata use an approximate content-size estimate. At `compact_threshold`, 85 percent by default, it summarizes older conversation and normally retains the last ten messages, shortening that tail toward an estimated 8,000-token budget when recent outputs are large. The latest complete exchange and the undo anchor remain protected. Summarization retains complete user-question arguments and answers. With `auto_compact` enabled, an explicit provider context-limit error triggers one compaction-and-retry; a second rejection is returned to the user. `/compact` does the same on demand. For `openai-codex:` models the opaque Codex compaction item is stored and replayed so later turns keep server-side context.

## Web browsing

Off by default. Enable with `/settings web_browsing on` or Settings > Web. `web_search` returns up to five DuckDuckGo results and is free and keyless; Brave and Firecrawl need `BRAVE_API_KEY` or `FIRECRAWL_API_KEY`. `web_fetch` makes a direct HTTP(S) request with a five-redirect and 10 MiB body cap, runs no JavaScript, and returns cleaned text. HTML becomes Markdown with headings, numbered and nested lists, tables, fenced code with its language, and links resolved against the page or its `<base href>`. Sparse tables render as ordinary text blocks when rectangular padding would exceed four times the actual cell count. Pages up to `web_fetch_max_chars` (20,000 by default, at most 50,000) return inline. Longer pages return that much inline plus the path of the full page under `~/.yawl/artifacts/tool-output/` and the `start_line` to continue from with `read_file`. DuckDuckGo answers rapid automated searches with a bot check; `web_search` reports it as such, and waiting or switching to Brave or Firecrawl avoids it. Search results and fetched pages are marked untrusted, so page content is never treated as agent instructions. This is a content-trust boundary, not a network sandbox: `web_fetch` accepts any http(s) URL, including loopback and private addresses, because the model can already reach the network through `shell`. Run Yawl only where the agent's own network access is acceptable.

## Subagents

Off by default; enable with `/settings subagents on`. The model then gets `subagent_spawn`, `subagent_send`, `subagent_wait`, `subagent_cancel`, and `subagent_list`. Children run memory-only conversations in the same working directory, cannot spawn further subagents, and are capped at 16 concurrent runs. Settings > Subagents > Default subagent model offers Inherit, listed models, and manual IDs. `subagent_model` selects the child model, `inherit` by default; without a per-spawn choice, a preset pin takes priority over this setting, which takes priority over the active parent model; `subagent_request_budget` and `subagent_timeout_secs` cap each run. Interrupted results report when a timeout or request budget stopped the run and include partial output from that run when available. Canceling a child discards queued messages and unaccepted steering. JSON presets in `~/.yawl/agents/` or `./.yawl/agents/` pin a model, tool set, and role; an explicit user request for a listed model may set `subagent_spawn.model` for one child, overriding the preset pin and configured default while preserving its role and tool limits; Yawl bundles `scout`, a read-only investigator with native `list_files`, `search_files`, `read_file`, `read_skill`, and `git_inspect` tools. `/subagents` opens a dashboard where Enter takes over a child interactively.

Global and working-directory `AGENTS.md` instructions are injected into each model request. The orchestrator can delegate directly when the task has enough scope, without a mandatory repository inventory or rereading injected instructions. It is instructed to wait for every spawned child and consider every result before its final response. `subagent_wait` continues to wait for all selected IDs; while it runs, its transcript card lists only the children still working and names the finished ones below a done count. `subagent_send` steers an active child at its next safe turn boundary by default, or restarts it if settled; pass `queue: true` to queue a separate follow-up turn instead. An accepted orchestrator steer makes the current turn eligible for automatic result delivery, even if the user started that turn. Later user-only turns remain private.

Results longer than 2 KiB are saved in `~/.yawl/artifacts/subagents/`. Waits, status reads, and automatic delivery include the opening 1 KiB and an absolute path instead of the full report. Children are instructed to put a short summary first. These files survive child pruning and session restarts; they remain on disk until removed. Read details with `read_file` using `offset: 0` and `limit: 16384`, then continue with the returned `next_offset`. Artifact write failures are reported explicitly with a bounded excerpt.

## Builtin tools

- `shell` runs `sh -c` in the working directory with a 120-second default timeout. `background: true` returns a `bg-N` ID and runs without a timeout.
- `shell_output`, `shell_list`, and `shell_stop` read, list, and stop background commands.
- `read_file` returns UTF-8 files up to 56 KiB whole, and images up to 5 MB become model input when the model accepts images. Larger text files return a numbered first page of up to 400 lines. `start_line` and `line_count` (up to 2,000) read numbered line ranges, with a header showing the range and `next_start_line` or `EOF`. Pages stop at 32 KiB; a single longer line is cut and its header gives the byte `offset` to continue. Optional byte `offset` and `limit` enable paged UTF-8 reads, including for larger reports. These pages show a byte-range header with `next_offset` or `EOF`, followed by the file text. FIFOs, devices, and directories are rejected without blocking.
- `git_inspect` is available to restricted child presets such as `scout`. It accepts only `status`, `unstaged_diff`, or `staged_diff`, plus an optional literal relative `path` filter. It uses the working directory, disables external diff/text conversion and filesystem-monitor hooks, refuses repositories configured with clean or process filters, avoids optional index writes and lazy fetches, ignores submodule changes, and caps execution at 15 seconds with bounded output. Untracked file contents need `read_file`. This is a restricted tool interface, not an OS sandbox.
- Restricted child presets such as `scout` can opt into `list_files` and `search_files`. They are not exposed to the main agent or unrestricted children, which use `shell` with `rg` or Git. `list_files` returns one path per line; `search_files` returns `path:line:column: snippet` for case-sensitive literal matches. Both accept `path`, `path_contains`, and a result `limit` of 1–200. They skip non-UTF-8 filenames, hidden entries, symlinks, and nested `.git`, `target`, and `node_modules` directories, without interpreting ignore files. Use an explicit hidden path or `include_hidden: true` when needed. Traversal stops at 20,000 entries or depth 32; search skips files over 2 MiB and reads at most 32 MiB per call. Output indicates truncation and skipped files so the model can narrow its scope.
- `write_file` writes a file and creates missing parent directories. Results say whether the file was created or overwritten, with the previous size.
- `edit_file` performs an exact string replacement. It rejects missing matches, and repeated matches unless `replace_all` is true; errors list the lines of repeated matches, or point at text that matches apart from surrounding whitespace. In files with CRLF line endings, `\n` in the edit text matches `\r\n`. Results give the edited line numbers. It reads the whole file, so it rejects files over 16 MiB; use `shell` for larger files.

Foreground shell, planning-shell, and executable-tool output above 16 KiB is saved under `~/.yawl/artifacts/tool-output/`; model history receives a file path plus roughly 2 KiB each from the beginning and end. Read the captured output with paged `read_file`. These artifacts persist until manually removed; process capture limits still apply. File reads and structured background-tool responses keep their existing output format. Other tool output is capped at 60,000 characters, except user answers, which remain complete. Start dev servers and watchers with `shell` using `background=true`, rather than shell `&` or `nohup`, so Yawl can track and stop them. Escape/Ctrl+C and foreground timeouts remain active while collecting output, even after the shell exits with descendants still holding its pipes open. Steering is applied at the next safe boundary; cancel a blocked foreground call to return control. Up to 8 background commands are tracked per session; stopping one sends `SIGTERM` to its process group, then `SIGKILL` after two seconds.

Yawl has no approval prompt or permission layer. Review the model and working directory before giving it a task, and use `Ctrl+C` to stop the active turn.

## Exec tools

Before every model step Yawl scans `~/.yawl/tools/` and `./.yawl/tools/` and rescans after each tool batch, so the model can create a tool and call it in the same turn. Project tools override global tools with the same name. An executable implements two operations:

1. `--describe` prints one JSON object with `name`, `description`, `input_schema`, and optional `timeout_secs`.
2. A normal call reads JSON arguments from stdin and prints the result. A nonzero exit marks an error. Writing input is covered by the tool timeout and stops if the process exits or the call is interrupted.

```python
#!/usr/bin/env python3
import json, sys

if "--describe" in sys.argv:
    print(json.dumps({
        "name": "word_count",
        "description": "Count words in supplied text",
        "input_schema": {"type": "object",
                         "properties": {"text": {"type": "string"}},
                         "required": ["text"]}}))
else:
    print(len(json.load(sys.stdin)["text"].split()))
```

The process inherits the working directory and receives the session ID in `YAWL_SESSION_ID`. The `list_files`, `search_files`, `git_inspect`, and `subagent_*` names are reserved; `web_search` and `web_fetch` are reserved while web browsing is enabled.

## Skills

Markdown skills are discovered recursively from `~/.yawl/skills/` and `~/.agents/skills/`, plus project locations once trusted: `./.yawl/skills/`, every `.agents/skills/` directory from the Git root down, and configured `skill_dirs`. Frontmatter needs a `name` and a real `description`:

```markdown
---
name: review
description: Review a code change for correctness, regressions, and missing tests.
---
Read the implementation and tests before reporting findings.
```

Descriptions go into the system-prompt catalog and the model loads full instructions with `read_skill`, rendered as a purple `Skill NAME` card. `disable-model-invocation: true` keeps a skill out of the catalog but leaves `/skill:NAME [ARGS]` working. Project skill sources — and project provider endpoints, base URLs, and credentials — prompt for trust on first use, stored in `~/.yawl/trust.json`; piped runs never prompt, so use `--trust-project` there. `/settings skills add DIR` and `remove DIR` manage search directories.

## Project instructions

Yawl appends `~/.yawl/AGENTS.md` and then `./AGENTS.md` after its system prompt. Empty or missing files are ignored.

## Development

One Cargo package. Facade modules (`main.rs`, `agent.rs`, `provider/mod.rs`, `config.rs`, `tui/mod.rs`, and friends) re-export stable public paths while private child modules own the implementation. `AGENTS.md` has the full module map.

The transcript renderer separates presentation from caching under `src/tui/render/`:

| File | Responsibility and main entry points |
| --- | --- |
| `../render.rs` | Compose the full frame with `build_frame_with_images`; expose stable entry points to the TUI. |
| `cache.rs` | Reuse rendered entries with `RenderCache`, invalidate stale output, and index rows and image placements. No entry styling. |
| `entries.rs` | Draw one entry with `render_entry`, lay out image previews, and render user, queued, and steer panels. |
| `reasoning.rs` | Draw thinking tags with `render_thinking_tag` and traces with `render_reasoning`; own thinking colors and duration labels. |
| `transcript.rs` | Assemble `render_transcript_window`, apply scrolling and selection reveal, and append loading and pending-input rows. |
| `welcome.rs`, `questions.rs` | Draw the welcome animation and question composer. |

The rendering flow is frame → transcript window → cache → entry renderer → reasoning/tool/Markdown renderer. `src/tui/transcript.rs` owns transcript data and events; `src/tui/render/transcript.rs` only presents that data. Rendering regression tests stay in `src/tui/render_tests.rs`, while private cache and color tests live beside their implementations.

The Git dashboard follows the same structure. `src/tui/git.rs` owns shared state and coordinates refreshes; `git/repository.rs` loads repository data, `jobs.rs` and `operations.rs` handle background work, `input.rs` handles interaction, `init.rs` owns repository setup, and `render.rs` and `diff.rs` draw the dashboard. Unit tests stay with each responsibility, with dashboard regressions in `src/tui/git_tests.rs`.

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
