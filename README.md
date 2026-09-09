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

Project skills stay disabled until you trust the repository. `yawl --trust-project` trusts them for a single noninteractive run without changing the saved decision. `yawl --help` has the full reference.

## Terminal controls

- `Enter` submits. During a response it steers the running turn at the next safe boundary. `Ctrl+G` does the same explicitly and works in legacy terminals, as does `Ctrl+Enter` where the terminal reports modifiers. `Tab` queues a message instead.
- `Shift+Enter`, `Alt+Enter`, and `Ctrl+J` insert newlines. Pastes over 400 characters or 8 lines appear as a `[Pasted #N]` marker in the editor; the model receives the full text.
- `Ctrl+V` attaches a clipboard image, up to five PNG, JPEG, GIF, or WebP files of 5 MB each per prompt. On Linux this needs `wl-paste` or `xclip`.
- `/` opens the command and skill menu. `@` tags a project file for the model.
- `Tab` focuses the transcript when idle. Arrows move between blocks, `h`/`l` fold, `Enter` opens a viewer, `y` copies. `Ctrl+F` searches the transcript and `Ctrl+O` expands or collapses tool output.
- Drag to select text; releasing copies to the clipboard. The mouse wheel and `PageUp`/`PageDown` scroll.
- `Escape` or `Ctrl+C` aborts the active model response or tool. Neither exits Yawl.
- When the bell is on (the default), Yawl rings the terminal bell when a turn finishes and when the model asks a question, so an unfocused terminal window or tab announces activity. Turn it off with `/settings bell off` or Settings > Interface > Bell.
- `/undo` restores files changed through `write_file`/`edit_file` and drops that turn. `/diff` shows every file those tools changed this session as per-file diff cards. `/copy` and `/copy-all` put replies on the clipboard.
- Multiple-choice questions from the model replace the composer. A countdown accepts the recommended answers automatically so unattended turns continue; answering yourself cancels it.
- `/ps` opens the background-process dashboard, `/subagents` the subagent dashboard, and `/git` the git dashboard, all while the model is busy.
- Git operations run in the background so typing and navigation remain responsive. `Ctrl+C` cancels the current Git operation. Results preserve commit-message edits made while the operation runs.
- Click `+` beside the `UNSTAGED` heading to stage all changes, including untracked files, just like the `a` shortcut.
- `/git` groups unstaged tracked changes and untracked files under Unstaged, with `U` marking untracked files. It refreshes the open diff as working or staged contents change. Large diffs fall back to compact hunks and show a warning if the preview is still truncated. Unstaging works before the first commit. Discarding untracked files preserves ignored files and nested repositories.
- In `/git`, moving the mouse highlights file and history rows without changing keyboard selection. Click a file or a commit to open its diff; `Esc` closes the current diff and a second `Esc` closes `/git`. History fills the bottom of the panel and pages in older commits as you scroll, including when a diff is open. Scroll over the history rows to browse older commits. Diff and log scrolling stop at the bottom and reverse immediately. Clickable controls use a hand cursor in Kitty, Ghostty, and foot; other terminals retain their own cursor. Hover requires mouse-motion reporting support.

The transcript renders Markdown with syntax highlighting for common languages. Tool calls use full-width cards: diffs for edits, image previews for `read_file` in terminals that support them.

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

`/settings` groups model, interface, context, provider, web, subagent, and skill options in one picker and applies generation-affecting changes before the next queued message. The status bar is customizable under Settings > Interface > Status bar, with live preview, reordering (`K`/`J`), and custom labels.

While a turn runs, `Enter` steers and `Tab` queues; queued messages show below the transcript. `/init` inspects the current directory and creates or curates `./AGENTS.md`; it keeps accurate project rules, removes stale or changelog-like material, and changes no other file. `/goal TEXT` keeps making model requests until the model calls an internal `goal_complete` tool; a plain reply does not finish it. `/plan TEXT` runs a read-only planning mode that gathers requirements in batches of three multiple-choice questions, stores the finished plan with the session, and offers Implement or Return to editor. Later prompts can revise or implement the active plan; unrelated prompts continue as ordinary turns without clearing it.

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

`openai-codex:` uses a ChatGPT Plus or Pro subscription through device-code login (`yawl --login openai-codex`), with the credential stored in `~/.yawl/auth.json`. Other OpenAI-compatible endpoints go under `providers`, using the same field names as pi's `models.json`:

```json
{
  "providers": {
    "omlx": {
      "baseUrl": "http://127.0.0.1:8000/v1",
      "api": "openai-completions",
      "apiKey": "$OMLX_API_KEY"
    }
  }
}
```

`apiKey` accepts `$ENV_VAR` and `${ENV_VAR}` references; omit it for keyless local servers. An optional `models` array supplies labels, token limits, and image support for `/model`; unlisted model IDs still work and remain text-only. `/connect` and Settings > Providers walk through the same setup interactively. `/settings provider NAME URL KEY` writes the direct form, with `-` in place of `KEY` removing a saved key.

## Sessions, undo, and compaction

Sessions are append-only JSONL under `~/.yawl/sessions/projects/<project-key>/`, scoped to the working directory. `-c` and `/resume` list sessions for the current directory; `--session ID` resumes from any directory. Messages, responses, reasoning, tool results, and usage are written as they happen; compaction records a summary without deleting the original log. If a tool result cannot be saved, the batch stops and Yawl retains the result in memory for a storage retry. Resuming an incomplete batch adds error results for missing outputs, marks execution as uncertain, and does not rerun those tools. `/usage` breaks out input, cache reads and writes, and output.

`/undo` saves a file's pre-image the first time `write_file` or `edit_file` touches it, capped at 32 MiB per file, and restores it on request. An undo intent is saved before restoration, and the checkpoint stays available until the history change commits. If interrupted, the next turn or `/undo` finishes the pending operation before starting another. In git repos where the agent moved `HEAD`, it soft-resets to the pre-turn commit. Files changed through `shell` or exec tools are not tracked. `/diff` uses the same pre-images: one diff card per touched file, comparing the oldest pre-image with the current contents, so files written back unchanged disappear and `/undo`-restored files drop out. Files that are non-UTF-8 or over 1 MiB on either side are listed in a notice instead of a card.

Before each request Yawl combines saved, model-specific token usage with estimates for newly added messages, tool output, and prompt changes. Older sessions without usage metadata use an approximate content-size estimate. At `compact_threshold`, 85 percent by default, it summarizes the older conversation and keeps roughly the last ten messages. With `auto_compact` enabled, an explicit provider context-limit error triggers one compaction-and-retry; a second rejection is returned to the user. `/compact` does the same on demand. For `openai-codex:` models the opaque Codex compaction item is stored and replayed so later turns keep server-side context.

## Web browsing

Off by default. Enable with `/settings web_browsing on` or Settings > Web. `web_search` returns up to five DuckDuckGo results and is free and keyless; Brave and Firecrawl need `BRAVE_API_KEY` or `FIRECRAWL_API_KEY`. `web_fetch` makes a direct HTTP(S) request with a five-redirect and 10 MiB body cap, runs no JavaScript, and returns cleaned text. Search results and fetched pages are marked untrusted, so page content is never treated as agent instructions.

## Subagents

Off by default; enable with `/settings subagents on`. The model then gets `subagent_spawn`, `subagent_send`, `subagent_wait`, `subagent_cancel`, and `subagent_list`. Children run memory-only conversations in the same working directory, cannot spawn further subagents, and are capped at 16 concurrent runs. `subagent_model` selects the child model, `inherit` by default; `subagent_request_budget` and `subagent_timeout_secs` cap each run. Interrupted results report when a timeout or request budget stopped the run and include partial output from that run when available. Canceling a child discards queued messages and unaccepted steering. JSON presets in `~/.yawl/agents/` or `./.yawl/agents/` pin a model, tool set, and role; Yawl bundles `scout`, a read-only investigator with native `list_files`, `search_files`, `read_file`, `read_skill`, and `git_inspect` tools. `/subagents` opens a dashboard where Enter takes over a child interactively.

Global and working-directory `AGENTS.md` instructions are injected into each model request. The orchestrator can delegate directly when the task has enough scope, without a mandatory repository inventory or rereading injected instructions. It is instructed to wait for every spawned child and consider every result before its final response. `subagent_wait` continues to wait for all selected IDs; follow-ups use queued `subagent_send` turns.

Results longer than 2 KiB are saved in `~/.yawl/artifacts/subagents/`. Waits, status reads, and automatic delivery include the opening 1 KiB and an absolute path instead of the full report. Children are instructed to put a short summary first. These files survive child pruning and session restarts; they remain on disk until removed. Read details with `read_file` using `offset: 0` and `limit: 16384`, then continue with the returned `next_offset`. Artifact write failures are reported explicitly with a bounded excerpt.

## Builtin tools

- `shell` runs `sh -c` in the working directory with a 120-second default timeout. `background: true` returns a `bg-N` ID and runs without a timeout.
- `shell_output`, `shell_list`, and `shell_stop` read, list, and stop background commands.
- `read_file` reads UTF-8 files up to 1 MiB, and images up to 5 MB become model input when the model accepts images. Optional byte `offset` and `limit` enable paged UTF-8 reads, including for larger reports. Pages show a byte-range header with `next_offset` or `EOF`, followed by the file text.
- `git_inspect` is available to restricted child presets such as `scout`. It accepts only `status`, `unstaged_diff`, or `staged_diff`, plus an optional literal relative `path` filter. It uses the working directory, disables external diff/text conversion and filesystem-monitor hooks, refuses repositories configured with clean or process filters, avoids optional index writes and lazy fetches, ignores submodule changes, and caps execution at 15 seconds with bounded output. Untracked file contents need `read_file`. This is a restricted tool interface, not an OS sandbox.
- Restricted child presets such as `scout` can opt into `list_files` and `search_files`. They are not exposed to the main agent or unrestricted children, which use `shell` with `rg` or Git. `list_files` returns one path per line; `search_files` returns `path:line:column: snippet` for case-sensitive literal matches. Both accept `path`, `path_contains`, and a result `limit` of 1–200. They skip non-UTF-8 filenames, hidden entries, symlinks, and nested `.git`, `target`, and `node_modules` directories, without interpreting ignore files. Use an explicit hidden path or `include_hidden: true` when needed. Traversal stops at 20,000 entries or depth 32; search skips files over 2 MiB and reads at most 32 MiB per call. Output indicates truncation and skipped files so the model can narrow its scope.
- `write_file` writes a file and creates missing parent directories.
- `edit_file` performs one exact string replacement and rejects missing or repeated matches.

Tool output sent to the model is capped at 60,000 characters. Up to 8 background commands are tracked per session; stopping one sends `SIGTERM` to its process group, then `SIGKILL` after two seconds.

Yawl has no approval prompt or permission layer. Review the model and working directory before giving it a task, and use `Ctrl+C` to stop the active turn.

## Exec tools

Before every model step Yawl scans `~/.yawl/tools/` and `./.yawl/tools/` and rescans after each tool batch, so the model can create a tool and call it in the same turn. Project tools override global tools with the same name. An executable implements two operations:

1. `--describe` prints one JSON object with `name`, `description`, `input_schema`, and optional `timeout_secs`.
2. A normal call reads JSON arguments from stdin and prints the result. A nonzero exit marks an error.

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

Descriptions go into the system-prompt catalog and the model loads full instructions with `read_skill`, rendered as a purple `Skill NAME` card. `disable-model-invocation: true` keeps a skill out of the catalog but leaves `/skill:NAME [ARGS]` working. Project skill sources prompt for trust on first use, stored in `~/.yawl/trust.json`; piped runs never prompt, so use `--trust-project` there. `/settings skills add DIR` and `remove DIR` manage search directories.

## Project instructions

Yawl appends `~/.yawl/AGENTS.md` and then `./AGENTS.md` after its system prompt. Empty or missing files are ignored.

## Development

One Cargo package. Facade modules (`main.rs`, `agent.rs`, `provider/mod.rs`, `config.rs`, `tui/mod.rs`, and friends) re-export stable public paths while private child modules own the implementation. `AGENTS.md` has the full module map.

The Git dashboard follows the same structure. `src/tui/git.rs` owns shared state and coordinates refreshes; `git/repository.rs` loads repository data, `jobs.rs` and `operations.rs` handle background work, `input.rs` handles interaction, `init.rs` owns repository setup, and `render.rs` and `diff.rs` draw the dashboard. Unit tests stay with each responsibility, with dashboard regressions in `src/tui/git_tests.rs`.

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
