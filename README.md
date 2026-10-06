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

Yawl is a terminal AI agent for macOS and Linux. Use the full-screen interface for interactive work or pipe input into print mode. It supports persistent sessions, custom providers, and executable tools you can add without recompiling.

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

Run `yawl` to choose a provider and model. Use `yawl --setup` to reopen setup or `yawl --doctor` to check and repair configuration.

## Usage

```sh
yawl                                 # full-screen interface
yawl "Explain this repository"       # streamed plain text
git diff | yawl "Review this patch"   # use stdin as context
yawl -c                              # resume the latest session here
yawl --session SESSION_ID            # resume from any directory
yawl -m openai:gpt-4o "Release note"  # choose a model for one run
yawl --list-tools                    # inspect available tools
```

Use `yawl --help` for CLI options and `/hotkeys` for keyboard shortcuts.

Yawl has no approval prompt or permission layer. Tools run with your account's access to files and the network. Review the working directory before starting a task; `Escape` or `Ctrl+C` stops the active response or tool without exiting.

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

Long pastes appear as compact markers but reach the model in full. Each prompt accepts up to five clipboard images in PNG, JPEG, GIF, or WebP format, up to 5 MB each. Linux clipboard images require `wl-paste` or `xclip`.

Canceling a response pauses queued messages and pending steering. Use `/unqueue` to edit the queue and `Enter` to resume. When a request fails, press `Enter` on an empty prompt to continue the turn from its saved history; `/continue` does the same after a failure or a cancel, keeping goal and plan modes. Model questions appear in the composer; their countdown accepts recommended answers unless you intervene. Disable completion and question bells with `/settings bell off`.

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
| `/plan [TEXT]` | Start, resume, cancel, or show the planning workflow |
| `/help` | Show terminal controls and commands |
| `/hotkeys` | Show every keyboard shortcut, grouped by area |
| `/quit` | Exit and print the resumable `yawl --session ID` command |

The Git dashboard supports keyboard and mouse navigation. Click a file or commit to inspect its diff; `Esc` closes the diff, then the dashboard. Use `a` or the `+` beside Unstaged to stage all changes. `Ctrl+C` cancels an active Git operation.

### Goals and plans

`/goal TEXT` keeps the agent working until it marks the goal complete. `/plan TEXT` starts a read-only workflow that asks questions and saves a Markdown plan. You can revise the plan or choose Implement. Implementation starts from the saved plan alone, without an extra summarization request; the original conversation stays in the session log. Plan phases keep the same system prompt and tool list and add their instructions as hidden messages, so local servers can reuse their prompt cache; planning still rejects any tool that is not read-only.

### Remote control

`/remote` lets another device in your [Tailscale](https://tailscale.com) network control the current session from a browser. Yawl listens only on this machine's Tailscale address, port 7474 by default. Each session has its own server, so other sessions running `/remote` take the next free port, from 7475 to 7483, and then any free port. It looks for the address on Tailscale's tunnel interface (`utun*` on macOS, `tailscale*` on Linux), not just any address in the shared 100.64.0.0/10 range. It shows the address, a six-digit pairing code, and a QR code, and copies the link to the clipboard. This notice is removed once a device connects or remote control stops.
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

Built-in providers use `anthropic:`, `openai:`, and `openai-codex:` prefixes. Anthropic and OpenAI read `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`, which take precedence over saved keys. Saved configuration uses file mode `0600`.

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

`apiKey` accepts `$ENV_VAR` and `${ENV_VAR}` references; omit it for keyless servers. The optional `models` array supplies model capabilities and picker entries. Unlisted model IDs work but remain text-only. In `/model`, `d` or `Delete` removes a configured entry after confirmation.

Declare the reasoning levels your endpoint accepts in `reasoning_efforts`, or select them during setup. An empty list disables explicit reasoning for that model. Model selection offers supported levels and a provider-default option, including when you save a new connection for the current session.

Use `/reasoning` to change the session's effort, `/reasoning default` to let the provider choose, or `/settings reasoning_effort high` to save a default. Changes apply to the next model request. Codex levels come from its model catalog.

### Project trust

Project skills and provider endpoints, base URLs, and credentials require trust before use. Yawl stores your decision in `~/.yawl/trust.json`. `--trust-project` grants trust for one invocation, including piped runs that cannot prompt.

### Interface settings

`/settings` includes colors, the bell, reasoning display, and a configurable status bar. Color pickers preview as you move; `Enter` saves and `Esc` restores the previous color. The status-bar editor supports reordering with `K`/`J` and custom labels.

## Sessions and undo

Sessions live under `~/.yawl/sessions/projects/<project-key>/`. `yawl -c` resumes the latest session in the working directory; `/resume` opens its session picker. `--session ID` resumes from any directory. Sessions restore the selected model and reasoning effort; `-m MODEL` overrides the model for one run.

`/undo` restores files changed through `write_file` and `edit_file` and removes the last turn. It does not track shell or executable-tool file changes. Snapshots are limited to 32 MiB per file. If the agent moved Git `HEAD`, undo also soft-resets it to the pre-turn commit. `/diff` compares files touched by the file tools with their original session contents.

Automatic compaction summarizes older conversation near the context limit, at 85% by default, while retaining recent exchanges. `/compact` requests it manually. The full transcript remains in the session log. `/usage` reports input, output, and prompt-cache usage.

Provider requests are retried with backoff on rate limits, 5xx responses, dropped connections, and DNS or connect failures: up to seven attempts, waiting from 0.5 s up to 15 s between them, so a network that is reconnecting after sleep has about 30 s to return. `Esc` or `Ctrl+C` cancels during a wait. A response that sends nothing for five minutes is treated as a dead connection and retried. After the computer wakes from sleep, for example when you open the lid, a stream that stays silent for 15 s is retried at once instead of hanging on a socket that did not survive the suspend. When an OpenAI-compatible server rejects a tool call it could not parse (`invalid_tool_call` or `incomplete_tool_call`, as oMLX reports), Yawl retries up to twice with a note asking the model to re-issue the call. The note is sent with the request but is not saved to the session.

## Web browsing

Enable with `/settings web_browsing on`. DuckDuckGo search needs no key; Brave and Firecrawl require `BRAVE_API_KEY` or `FIRECRAWL_API_KEY`. If DuckDuckGo blocks automated searches, wait or switch providers in Settings > Web.

`web_fetch` retrieves HTTP(S) pages without JavaScript and converts HTML to Markdown. It returns up to `web_fetch_max_chars`, 20,000 by default, and saves longer pages for the model to read in sections. Web content is treated as untrusted input. Fetching is not network-sandboxed and can reach private addresses.

## Subagents

Enable with `/settings subagents on`. Children share the working directory, run separate in-memory conversations, and cannot spawn further children. `/subagents` shows their progress; `Enter` lets you take over a child.

Settings > Subagents controls the default model, concurrency, request budget, and timeout. JSON presets in `~/.yawl/agents/` or `./.yawl/agents/` define roles, models, and tool access. The bundled `scout` preset provides read-only file search and Git inspection. Restricted tools limit available operations; they are not an OS sandbox.

Long reports are saved under `~/.yawl/artifacts/subagents/`, with a summary and path returned to the parent. These files persist until manually removed.

## Tools

| Tool | Purpose |
| --- | --- |
| `shell` | Run a command, with a 120-second default timeout |
| `shell_output`, `shell_list`, `shell_stop` | Manage background commands |
| `read_file` | Read text, line or byte ranges, and supported images |
| `write_file` | Write a file, creating parent directories as needed |
| `edit_file` | Replace exact text; repeated matches require `replace_all` |
| `read_skill` | Load a skill's instructions |
| `list_files`, `search_files`, `git_inspect` | File discovery and Git inspection for restricted children |

Use `shell` with `background: true` for servers and watchers so Yawl can track and stop them. Up to eight background commands can run per session; `/ps` opens their dashboard.

Large command output is saved under `~/.yawl/artifacts/tool-output/`. The model receives an excerpt and a path for paged reading. These artifacts persist until manually removed. `edit_file` accepts files up to 16 MiB; use shell tools for larger files.

### Executable tools

Put executables in `~/.yawl/tools/` or `./.yawl/tools/`. Yawl rescans during turns, and project tools override global tools with the same name. Each executable implements two operations:

1. `--describe` prints a JSON object with `name`, `description`, `input_schema`, and optional `timeout_secs`.
2. A normal call reads JSON arguments from stdin and prints its result. A nonzero exit marks an error.

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

Make the file executable with `chmod +x`. It inherits the working directory and receives `YAWL_SESSION_ID`. The `list_files`, `search_files`, `git_inspect`, and `subagent_*` names are reserved; `web_search` and `web_fetch` are reserved while browsing is enabled.

## Skills and project instructions

Skills are Markdown files discovered recursively in `~/.yawl/skills/` and `~/.agents/skills/`. Trusted projects also load `./.yawl/skills/`, `.agents/skills/` directories from the Git root down, and configured `skill_dirs`.

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

Yawl is one Cargo package with a library and binary, using blocking I/O. Agent, provider, configuration, tool, and TUI facades keep their implementations in child modules. See [AGENTS.md](AGENTS.md) for the module map and contribution rules.

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
