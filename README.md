# Yawl

<div align="center">
  <img src="https://raw.githubusercontent.com/alan13367/yawl/main/Assets/WelcomeScreen.png" alt="Yawl welcome screen in a terminal" width="800">
  <p><strong>A small AI coding harness that stays in your terminal.</strong><br>
  Stream answers, run tools, keep sessions, and bring your own model.</p>
  <p>
    <a href="https://github.com/alan13367/yawl/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-7aa2f7?style=flat-square" alt="MIT license"></a>
    <img src="https://img.shields.io/badge/Rust-1.99%2B-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust 1.99 or newer">
    <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-8fbcbb?style=flat-square" alt="macOS, Linux, and Windows">
  </p>
</div>

Yawl is a terminal AI agent for macOS, Linux, and Windows. Use the full-screen interface for interactive work or pipe input into print mode. It supports persistent sessions, custom providers, and custom tools the agent builds for you and keeps across sessions.

<table>
  <tr>
    <td width="50%" align="center"><img src="https://raw.githubusercontent.com/alan13367/yawl/main/Assets/CommandsList.png" alt="Yawl command and skill completion menu"><br><sub>Type <code>/</code> to search commands and skills</sub></td>
    <td width="50%" align="center"><img src="https://raw.githubusercontent.com/alan13367/yawl/main/Assets/SubagentsDashboard.png" alt="Yawl subagent dashboard"><br><sub>The subagent dashboard</sub></td>
  </tr>
</table>

## Install

Requires Rust 1.99.1 or newer.

```sh
git clone https://github.com/alan13367/yawl.git
cd yawl
cargo install --path .
```

Run `yawl` to choose a provider and model. Setup uses the same screens as `/connect`, under the Yawl wordmark with a progress line, and you can connect several providers before starting. Use `yawl --setup` to reopen setup or `yawl --doctor` to check and repair configuration. Piped input gets numbered prompts on stderr instead.

On Windows, Yawl needs Windows 10 version 1809 or later and runs in Windows Terminal or the standard console; the mintty window that Git Bash opens is not supported. Shell commands and custom tools run in the bash that comes with [Git for Windows](https://git-scm.com/download/win), so install it first. Yawl finds it through `git.exe` on `PATH` or the standard install folders; set `YAWL_GIT_BASH` to the full path of `bash.exe` to choose another. Paths written as `~/.yawl` below mean `%USERPROFILE%\.yawl`.

## Usage

```sh
yawl                                 # full-screen interface
yawl "Explain this repository"       # streamed plain text
git diff | yawl "Review this patch"   # use stdin as context
yawl -c                              # resume the latest session here
yawl --session SESSION_ID            # resume from any directory
yawl -m openai:gpt-4o "Release note"  # choose a model for one run
yawl --json --no-stdin "Fix the test"  # one JSON result for scripts and agents
yawl --list-models                   # providers, models, and capabilities
yawl --list-tools                    # inspect available tools
```

Use `yawl --help` for CLI options and `/hotkeys` for keyboard shortcuts.

Yawl has no approval prompt or permission layer. Tools run with your account's access to files and the network. Review the working directory before starting a task; `Escape` or `Ctrl+C` stops the active response or tool without exiting.

## Use from scripts and other agents

Print mode is built to be called by other programs, including agent harnesses such as Claude Code, Codex, and OpenCode, which can hand Yawl a task on any model you have configured.

```sh
yawl --install-skill all             # teach Claude Code, Codex, and OpenCode to use Yawl
yawl --check --json                  # can a turn start? if not, what to fix
yawl --list-models --refresh --json  # what each provider serves right now
yawl --json --no-stdin -m ollama:qwen3-coder:30b "Add tests for src/range.rs"
git diff | yawl --json --agent scout "Review this diff"   # read-only review
```

`--json` prints exactly one object on stdout when the run ends: `status` (`completed`, `error`, `timeout`, or `interrupted`), `exit_code`, `session_id`, `model`, `text` (the final answer), `error` (`kind`, `message`, and `hint`), `duration_ms`, `requests`, `usage`, `context`, `tool_calls`, `tool_errors`, `edited_files` (files changed by `write_file` and `edit_file`; shell commands are not tracked), and `warnings`. A run that fails before it starts, for example on an unknown option or a missing credential, prints an object of the same shape. `--format jsonl` streams one event per line instead (`start`, `text`, `reasoning`, `message`, `tool_start`, `tool_end`, `usage`, `retry`, `warning`, `compacted`, and others), ending with the same result object. Text in both formats is escaped so it cannot steer a terminal.

| Option | Effect |
| --- | --- |
| `--no-stdin` | Never read standard input. Without it, print mode reads stdin until it closes, and says on stderr after 3 seconds why it is waiting |
| `--timeout SECONDS` | Stop the whole run, including the wait for stdin, after this long; the session can be continued with `--session` |
| `--agent PRESET` | Use a subagent preset's tools, role, and model. `--agent scout` gives a read-only run with no shell |
| `--reasoning EFFORT` | Reasoning effort for this run, checked against what the model accepts |
| `--ephemeral` | Keep the session in memory only, out of `/resume` and `--list-sessions` |

`--check` reports whether a turn could start: configuration problems from the doctor, the model and its provider, a missing key or login, and project settings ignored until the project is trusted. With `--refresh` it also asks the provider for its model list, which proves the endpoint and credential work without spending a completion. `--list-models` lists providers with their credential state and every saved model with its context window, output limit, reasoning efforts, and image support; `--refresh` adds what each provider currently serves. `--list-sessions` and `--list-agents` list resumable sessions in the working directory and the presets `--agent` accepts. Every listing takes `--json`.

Print mode tells the model that the run is non-interactive, so it states its assumptions instead of asking, and ends with a self-contained final answer.

Exit codes are 0 on success, 1 for other failures, 2 for usage errors, 3 for configuration or authentication problems, 4 when the provider fails or cannot be reached, 124 on timeout, and 130 on interrupt.

`--print-skill` prints the [Agent Skills](https://agentskills.io) `SKILL.md` that teaches another harness all of this: when to delegate, how to choose a model and a preset, how to read the result, and how to continue a session. `--install-skill` writes it for `claude` (`~/.claude/skills`, or `$CLAUDE_CONFIG_DIR/skills`), `codex` or `agents` (`~/.agents/skills`), `opencode` (`~/.config/opencode/skills`), `all` (Claude and agents, which OpenCode also reads), or a skills directory path. Yawl does not load this skill itself. The skill is compiled into the binary, so reinstall it after upgrading Yawl.

### Terminal controls

| Control | Action |
| --- | --- |
| `Enter` | Send a prompt, or steer an active response |
| `Ctrl+G` | Steer an active response explicitly |
| `Tab` | Queue a message during a response; focus the transcript when idle |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Insert a newline |
| `Ctrl+V` | Attach a clipboard image |
| `/`, `@` | Find a command or skill; tag a project file |
| `Ctrl+F` | Search the transcript |
| `Ctrl+O` | Expand or collapse all tool output |
| `PageUp`, `PageDown`, mouse wheel | Scroll; `Ctrl`+wheel moves one row |
| Drag | Select and copy text |

Scrolling up while a response streams keeps the view where it is, and new output collects below it. A **↓ Scroll to bottom** button appears whenever the view is not at the bottom. Click it, or scroll back down, to follow the output again.

With the transcript focused, arrows select blocks, `h`/`l` fold or unfold, `Enter` opens a viewer, and `y` copies. Click tool cards or thinking tags to expand them. Reasoning is collapsed by default; `hide_reasoning` removes it entirely.

Picking a file from the `@` menu inserts a tag showing its file name, or its relative path when another project file has the same name. The model receives the path relative to the working directory. File tags and `/skill:` invocations are highlighted in the composer and in sent prompts.

Long pastes appear as compact markers but reach the model in full. Each prompt accepts up to five clipboard images in PNG, JPEG, GIF, or WebP format, up to 5 MB each. Linux clipboard images require `wl-paste` or `xclip`; Windows reads them through PowerShell.

A steer reaches the model with its next request, marked as an adjustment to the request in progress so the final reply still covers the whole request. Tool calls the model has already written, such as a large file, still run first, so cancel instead when they must not run. A steer that arrives as the turn finishes is sent as the next message. Canceling a response pauses queued messages and pending steering. Use `/unqueue` to edit the queue and `Enter` to resume. When a request fails, press `Enter` on an empty prompt to continue the turn from its saved history; `/continue` does the same after a failure or a cancel, keeping goal and plan modes. Model questions appear in the composer; outside planning, their countdown accepts recommended answers unless you press a key or scroll. Planning and plan-revision questions have no countdown and wait for your answer. Disable completion and question bells with `/settings bell off`.

### Slash commands

| Command | Effect |
| --- | --- |
| `/model [MODEL]` | Choose a model and its reasoning level, or switch to `MODEL` |
| `/reasoning [LEVEL]` | Show or set the reasoning effort for the current model |
| `/connect` | Configure a provider and model with guided setup |
| `/settings [KEY ...]` | Open the settings picker, or change a setting directly |
| `/new`, `/clear` | Start a new session in the same working directory |
| `/compact` | Summarize older messages now |
| `/usage` | Show token and prompt-cache usage |
| `/undo` | Restore files from before the last prompt and drop that turn |
| `/continue` | Resume a turn that failed or was interrupted from where it stopped, without resending your prompt |
| `/diff` | Show files changed through the file tools this session as diff cards |
| `/init` | Create or update `AGENTS.md` with project guidance for coding agents |
| `/copy`, `/copy-all` | Copy the last reply, or the whole conversation |
| `/tools`, `/skills` | List tools and skills |
| `/skill:NAME [ARGS]` | Run a discovered Markdown skill |
| `/subagents` | Open the subagent dashboard and takeover view |
| `/git` | Stage, discard, commit, push, and browse history and diffs |
| `/ps` | Open the background-process dashboard |
| `/remote [off]` | Control this session from a phone or browser in your Tailscale network |
| `/resume [ID\|NUMBER]` | Open the session picker or resume directly; `d` deletes a session |
| `/unqueue [NUMBER\|all]` | Edit, remove, or clear queued messages |
| `/goal [TEXT]` | Start, resume, cancel, or show a persistent goal |
| `/plan [TEXT\|revise TEXT\|implement]` | Start, revise, implement, resume, cancel, or show the planning workflow |
| `/help` | Show terminal controls and commands |
| `/hotkeys` | Show every keyboard shortcut, grouped by area |
| `/quit` | Exit and print the resumable `yawl --session ID` command |

The Git dashboard supports keyboard and mouse navigation. Click a file or commit to inspect its diff; `Esc` closes the diff, then the dashboard. Use `a` or the `+` beside Unstaged to stage all changes. `Ctrl+C` cancels an active Git operation.

### Goals and plans

`/goal TEXT` keeps the agent working until it marks the goal complete. `/plan TEXT` starts a read-only workflow that asks one to three questions (more only while material choices remain) and saves a Markdown plan. Once the plan is ready, a typed prompt is classified as a revision, an implementation request, or an unrelated message; `/plan revise TEXT` revises the plan directly and `/plan implement` implements it, with no classification step. Revisions ask questions only when the change leaves material choices open. `/plan TEXT` with a ready plan replaces it with a new plan, and `/undo` restores the ready one. While a plan is active, `revise` and `implement` are reserved first words; with no plan, `/plan revise …` starts a plan like any other objective. Subcommands match regardless of case. When implementation starts, the planning conversation, from `/plan` up to the implementation prompt, is replaced in the model's context by the saved plan, the objective, and your answers and prompts from planning, without an extra summarization request. Conversation from before `/plan` stays in context, and the full history stays in the session log. If implementation needs something only you can provide, or the model replies three times in a row without finishing a plan step, the turn stops and the plan stays active. A reply that answers or continues the work resumes implementation, and `/plan implement` resumes it directly. Plan phases keep the same system prompt and tool list and add their instructions as hidden messages, so local servers can reuse their prompt cache; planning still rejects any tool that is not read-only.

### Remote control

`/remote` lets another device in your [Tailscale](https://tailscale.com) network control the current session from a browser. Yawl listens only on this machine's Tailscale address, port 7474 by default. Each session has its own server, so other sessions running `/remote` take the next free port, from 7475 to 7483, and then any free port. It looks for the address on Tailscale's tunnel interface (`utun*` on macOS, `tailscale*` on Linux, the adapter named Tailscale on Windows), not just any address in the shared 100.64.0.0/10 range. It shows the address, a six-digit pairing code, and a QR code, and copies the link to the clipboard. This notice is removed once a device connects or remote control stops.
- **Scan the QR code:** the phone opens the page and pairs automatically. The link carries the code in its `#fragment`, which browsers never send to the server.
- **Use the clipboard:** with Universal Clipboard, paste the link on an iPhone.
- **Type it:** open the address on the other device and enter the code.

After five wrong codes, remote control stops.

The page mirrors the full-screen interface at the device's size, so every command, picker, and dashboard works. Once paired, the browser tab is named after the session: the project directory and the first line of its first prompt, such as `yawl · fix the remote pairing flow`. On phones and tablets:
- Tap Yawl's composer, or `⌨`, to type with the phone keyboard. Keystrokes stream live, so completion menus, `@` mentions, and autocorrect all work.
- `✎` opens a sheet for pasting or writing longer text.
- A key bar provides `Esc`, `Enter`, `Backspace`, arrows, `Tab`, `Ctrl+C`, `Ctrl+O`, and paging.
- Swipe to scroll and tap to click.
- `/copy` opens a sheet on the device.

On a computer, detected by a mouse or trackpad, the page behaves like a terminal. The touch controls are hidden, and the keyboard, mouse, and paste go straight to Yawl. `Shift+Enter` inserts a newline and `Ctrl+Enter` steers. Dragging to select or running `/copy` shows a Copy button, because plain HTTP pages cannot write the clipboard unprompted. `Option`-drag makes a normal browser selection for `⌘C`.

Only the device currently in control can type. When another device connects, the previous one is locked out even though it stays paired. A device that reconnects on its own, such as a phone waking up, never takes control back from another connected device. It offers a **Take control** button instead. The page loads xterm.js from `cdn.jsdelivr.net`, so the device needs internet access.

While a device has control, the host terminal shows a lock screen and ignores input. Press `Ctrl+C` there to end remote control and take back the terminal. While remote control runs, the host's `Ctrl+C` never raises an interrupt signal, so it cannot cancel work the device started. `/remote off` from either side stops it, even during a turn, and a new `/remote` requires pairing again. Traffic stays inside your tailnet, encrypted by WireGuard, but anyone with the code can run tools as you.

## Models and configuration

Yawl reads `~/.yawl/config.json` and then `./.yawl/config.json`; project settings override global settings. All fields are optional. Use `/settings` for interactive editing or `/settings reload` after editing files. Invalid values are reported with their file and field; `yawl --doctor` can help repair them.

```json
{
  "model": "omlx:Qwen3-Coder",
  "max_tokens": 8192,
  "auto_compact": true,
  "compact_threshold": 0.85,
  "context_windows": { "omlx:Qwen3-Coder": 65536 }
}
```

Built-in providers use `anthropic:`, `openai:`, and `openai-codex:` prefixes. Anthropic and OpenAI read `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`, which take precedence over saved keys. Saved configuration uses file mode `0600`; on Windows it relies on the private permissions of your user profile.

For Codex, run `yawl --login openai-codex` to sign in with your ChatGPT subscription. Yawl discovers your account's models and caches their capabilities for offline lookup.

Local presets are `ollama:`, `lmstudio:`, and `omlx:`, using localhost ports 11434, 1234, and 8000. Model IDs can contain colons: `ollama:llama3.1:8b` selects model `llama3.1:8b` from Ollama.

### Custom providers and reasoning

Use `/connect` for guided setup or add an OpenAI-compatible endpoint under `providers`:

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

`apiKey` accepts `$ENV_VAR` and `${ENV_VAR}` references; omit it for keyless servers. The optional `models` array supplies model capabilities and picker entries. When the models endpoint reports a served context length (`max_model_len`, as oMLX and vLLM do), `/connect` saves it as the model's `context_window`. Unlisted model IDs work but remain text-only. In `/model`, `d` or `Delete` removes a configured entry after confirmation.

Declare the reasoning levels your endpoint accepts in `reasoning_efforts`, or select them during setup. An empty list disables explicit reasoning for that model. Model selection offers supported levels and a provider-default option, including when you save a new connection for the current session.

Use `/reasoning` to change the session's effort, `/reasoning default` to let the provider choose, or `/settings reasoning_effort high` to save a default. Changes apply to the next model request. Codex levels come from its model catalog.

### Project trust

Project skills, project tools, and provider endpoints, base URLs, and credentials require trust before use. The first time Yawl starts in a project that defines any of them, it asks whether to trust it permanently, for this session, or not at all; "not at all" is selected by default, and Esc chooses it. A project tool the agent defines during a session works for the rest of that session; later sessions load it once you trust the project. Yawl stores your decision in `~/.yawl/trust.json`. `--trust-project` grants trust for one invocation, including piped runs that cannot prompt.

### Interface settings

`/settings` includes colors, the theme, the bell, reasoning display, and a configurable status bar. Color pickers preview as you move; `Enter` saves and `Esc` restores the previous color. The status-bar editor supports reordering with `K`/`J` and custom labels.

The `theme` setting (`auto`, `dark`, or `light`) picks interface colors for the terminal background. `auto`, the default, asks the terminal for its background color at startup, then falls back to `COLORFGBG`, then to dark. On a light background, accent and selection colors that would be too pale are darkened, so the default white accent stays visible. Remote-control pages switch their terminal background to match. Use `/settings theme light` when a terminal does not report its color, for example inside some multiplexers; an explicit `dark` or `light` skips the startup query.

## Sessions and undo

Sessions live under `~/.yawl/sessions/projects/<project-key>/`. `yawl -c` resumes the latest session in the working directory; `/resume` opens its session picker. `--session ID` resumes from any directory, and print mode reports the id on stderr, or as `session_id` in JSON. Sessions restore the selected model and reasoning effort; `-m MODEL` overrides the model for one run. A session log is append-only: every change is an event that replay applies the same way the live session did. Unreadable entries, for example from a newer Yawl version, are skipped with a warning when the session opens, and an entry cut short by a crash is dropped.

`/undo` restores files changed through `write_file`, `edit_file`, and `define_tool`, and removes the last turn. It does not track file changes made by shell commands or custom tools. Snapshots are limited to 32 MiB per file. If the agent moved Git `HEAD`, undo also soft-resets it to the pre-turn commit. When a turn's snapshot is missing, for example after copying a session log without `~/.yawl/checkpoints`, undo still removes the turn and warns that files were not restored. A session that cannot finish recovering never blocks `/new` or `/resume`. `/diff` compares files touched by the file tools with their original session contents.

Automatic compaction summarizes older conversation near the context limit, at 85% by default, while retaining recent exchanges. `/compact` requests it manually. The full transcript remains in the session log. `/usage` reports input, output, and prompt-cache usage.

Provider requests are retried with backoff on rate limits, 5xx responses, dropped connections, and DNS or connect failures: up to seven attempts, waiting from 0.5 s up to 15 s between them, so a network that is reconnecting after sleep has about 30 s to return. `Esc` or `Ctrl+C` cancels during a wait. A response that sends nothing for five minutes is treated as a dead connection and retried. After the computer wakes from sleep, for example when you open the lid, a stream that stays silent for 15 s is retried at once instead of hanging on a socket that did not survive the suspend. When an OpenAI-compatible server rejects a tool call it could not parse (`invalid_tool_call` or `incomplete_tool_call`, as oMLX reports), Yawl retries up to twice with a note asking the model to re-issue the call. The note is sent with the request but is not saved to the session.

## Web browsing

Enable with `/settings web_browsing on`. DuckDuckGo search needs no key; Brave and Firecrawl require `BRAVE_API_KEY` or `FIRECRAWL_API_KEY`. DuckDuckGo searches are spaced at least 3 seconds apart, and after a bot check they pause for a minute; to avoid the wait, switch providers in Settings > Web.

`web_fetch` retrieves HTTP(S) pages without JavaScript and converts HTML to Markdown. It returns up to `web_fetch_max_chars`, 20,000 by default, and saves longer pages for the model to read in sections. Web content is treated as untrusted input. Fetching is not network-sandboxed and can reach private addresses.

## Subagents

Enable with `/settings subagents on`. Children share the working directory, run separate in-memory conversations, and cannot spawn further children. The main agent delegates substantial work by default, then blocks until every child settles before it continues.

`/subagents` shows their progress; `Enter` takes over a child. The takeover mirrors the main screen under a `Subagent` header: `Enter` steers a running child or messages a settled one, `Tab` queues a private turn, `Ctrl+O` expands all tool output and thinking (or click one thinking tag or tool card), `Ctrl+C` cancels the run, and `Esc` returns to the dashboard. Steers show who sent them and are marked delivered once the child accepts them.

Settings > Subagents controls the default model, concurrency, request budget, and timeout. Children spawned past the concurrency limit queue and start in order as running ones settle. JSON presets in `~/.yawl/agents/` or `./.yawl/agents/` define roles, models, and tool access. The bundled `scout` preset provides read-only file search and Git inspection. Restricted tools limit available operations; they are not an OS sandbox. A restricted child's prompt lists its allowed tools, and any other call is rejected with that list and what to do instead, so weaker local models stop retrying. `yawl --agent PRESET` runs a print-mode turn under a preset the same way.

Long reports are saved under `~/.yawl/artifacts/subagents/`, with a summary and path returned to the parent. These files persist until manually removed.

## Tools

| Tool | Purpose |
| --- | --- |
| `shell` | Run a command, with a 120-second default timeout |
| `shell_output`, `shell_list`, `shell_stop` | Manage background commands |
| `read_file` | Read text, line or byte ranges, and supported images |
| `write_file` | Write a file, creating parent directories as needed |
| `edit_file` | Replace exact text; repeated matches require `replace_all` |
| `define_tool` | Save a custom tool for later steps and sessions |
| `read_skill` | Load a skill's instructions |
| `list_files`, `search_files`, `git_inspect` | File discovery and Git inspection for restricted children |

Use `shell` with `background: true` for servers and watchers so Yawl can track and stop them. Up to eight background commands can run per session; `/ps` opens their dashboard. `shell_stop` asks a command to exit and kills it after two seconds; Windows has no such request, so there it ends the whole process tree at once.

Each command output stream keeps its first and last 32 KiB, so build and test summaries printed last are never lost; a marker shows how much was omitted in between. Large command output is saved under `~/.yawl/artifacts/tool-output/`. The model receives an excerpt and a path for paged reading. These artifacts persist until manually removed. `edit_file` accepts files up to 16 MiB; use shell tools for larger files.

Shell and custom-tool results contain stdout, followed by any stderr under a `[stderr]` marker, even when the command succeeds. That way, compiler and linter warnings reach the model. Commands run without a terminal, so password and passphrase prompts from tools such as `ssh`, `sudo`, and `git` fail at once with their error instead of waiting for input; use keys, an agent, or `sudo -n`. On Windows, commands get a hidden console instead. Git prompts and Git Credential Manager dialogs are turned off, but other programs that prompt, such as `ssh` asking for a password, wait until the command times out. A Windows command line holds at most 32,767 characters, so write longer scripts to a file first.

### Custom tools

Yawl adapts to you by building its own tools. Ask for a command or workflow you repeat, such as "make a tool that runs our e2e suite against staging", and the agent saves it with `define_tool`. The tool can be called from the agent's next step and stays available in later sessions. The agent also creates tools without being asked when it notices a workflow you will need again. A card in the transcript shows each tool it saves, and `/tools` lists them separately from the builtins. To remove a tool, delete its file or ask the agent to.

Each tool is one JSON manifest named after the tool. Global tools live in `~/.yawl/tools/NAME.json` and follow you to every project. Project tools live in `./.yawl/tools/NAME.json`, and when both scopes define the same name, the project tool wins. Yawl rereads the directories before each model step and never runs anything to discover tools.

```json
{
  "description": "Run the test suite, optionally filtered by test name",
  "params": {
    "filter": { "type": "string", "description": "Substring of test names", "required": false },
    "release": { "type": "boolean", "flag": "--release" }
  },
  "command": "cargo test {{release}} -- {{filter}}",
  "timeout_secs": 600
}
```

- `command` runs with `sh -c` (Git for Windows' `bash -c` on Windows) in the working directory, with a 120-second default timeout.
- `{{name}}` expands to that argument, already shell-quoted, so spaces and quotes in values stay intact. Don't wrap a placeholder in quotes; the manifest is rejected if you do.
- A missing optional argument expands to nothing. An array expands to one word per item.
- A boolean with a `flag` expands to the flag text when true and to nothing when false.
- The full arguments, with defaults applied, also arrive as a JSON object on stdin.
- `YAWL_TOOL_DIR` holds the manifest's absolute directory, for helper scripts kept next to it. `YAWL_SESSION_ID` is also set.
- A nonzero exit marks an error. Output over 16 KB is saved under `~/.yawl/artifacts/tool-output/` like shell output.

Parameter types are `string`, `integer`, `number`, `boolean`, `array` (of strings), and `object`. A parameter is required unless it sets `"required": false`, has a `default`, or has a `flag`. String and number parameters may list allowed values in `enum`. Yawl checks arguments before running the command and returns errors that include the tool's signature, so the model can correct its call.

Manifests reject unknown fields. Problems with one, such as an undeclared or quoted placeholder, appear once as a warning and under `/tools`. Custom tools can't take a builtin's name: `shell`, the file tools, `define_tool`, `read_skill`, and the orchestration and mode tools. `web_search` and `web_fetch` are reserved while browsing is enabled. Custom tools don't run while planning.

Executables written for the old `--describe` protocol no longer load. To keep one, add a manifest whose command runs it, for example `"command": "\"$YAWL_TOOL_DIR/my_tool\""`. It still receives its JSON arguments on stdin.

## Skills and project instructions

Skills are Markdown files discovered recursively in `~/.yawl/skills/` and `~/.agents/skills/`. Trusted projects also load `./.yawl/skills/`, `.agents/skills/` directories from the Git root down, and configured `skill_dirs`. Discovery follows symlinked skill directories and files, so a skill can be linked in from elsewhere; link cycles and dangling links are skipped, and each skill directory stops after 2,000 subdirectories.

```markdown
---
name: review
description: Review code for correctness, regressions, and missing tests.
---
Read the implementation and tests before reporting findings.
```

The model sees skill descriptions and loads instructions as needed. `/skill:NAME [ARGS]` runs a skill directly. Set `disable-model-invocation: true` to make a skill manual-only. `/settings skills add DIR` and `/settings skills remove DIR` manage search directories.

Yawl includes `~/.yawl/AGENTS.md` and `./AGENTS.md` in model instructions. Use them for personal and project conventions.

## Development

Yawl is one Cargo package using blocking I/O: a one-line binary over a library whose modules are all crate-private, so the compiler reports unused code. Agent, provider, configuration, tool, and TUI facades keep their implementations in child modules. See [AGENTS.md](AGENTS.md) for the module map and contribution rules.

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
