# Yawl

Yawl is a small AI agent runner for macOS and Linux. It has a full-screen terminal interface, a scriptable print mode, Anthropic and OpenAI-compatible providers, persistent sessions, tool calling, and automatic context compaction.

The model can add tools without recompiling Yawl. Any executable in `~/.yawl/tools/` or `./.yawl/tools/` becomes a callable tool when it implements the exec-tool contract below.

Yawl is one Rust crate with blocking I/O and four direct dependencies: `ureq`, `serde`, `serde_json`, and `libc`.

## Install

Yawl requires Rust 1.97.1 or newer.

```sh
git clone https://github.com/alan13367/yawl.git
cd yawl
cargo install --path .
```

Run `yawl` after installation. On first use it asks you to choose a provider, configure its endpoint and authentication, and select a model. Yawl does not assume a default model. Run `yawl --setup` to repeat onboarding later.

API-key providers read their usual environment variables:

```sh
export ANTHROPIC_API_KEY="..."
export OPENAI_API_KEY="..."
```

## Use it

Open the terminal interface:

```sh
yawl
```

Run one prompt and stream plain text:

```sh
yawl "Explain this repository"
git diff | yawl "Review this patch"
```

Resume a session or choose a model:

```sh
yawl -c
yawl --session 20260820-093301-1a2b
yawl -m openai:gpt-4o "Write a release note"
```

List the tools currently available:

```sh
yawl --list-tools
```

Run `yawl --help` for the complete command-line reference.

## Terminal controls

- `Enter` submits the editor contents.
- `Shift+Enter` inserts a newline in terminals that support the kitty keyboard protocol.
- `Alt+Enter` is the multiline fallback.
- Pasted multiline text stays multiline through bracketed paste mode.
- Typing `/` opens a filtered command and skill menu. `Up`/`Down` select an item and `Tab` completes it. Enter completes and runs the command when only one match remains, so `/qui` runs `/quit`.
- Outside the completion menu, `Up` and `Down` browse input history.
- `Ctrl+U`, `Ctrl+K`, and `Ctrl+W` delete text.
- `Ctrl+O` expands or collapses tool arguments and output. Tool blocks start compact.
- The mouse wheel and `PageUp` or `PageDown` move through Yawl's internal scrollback.
- Drag with the left mouse button to select visible text. Releasing the button copies the selection to the clipboard and briefly shows a `Copied!` box in the top-right corner, including while a response is streaming.
- `Escape` or `Ctrl+C` aborts the active model response or tool. Neither exits Yawl.

The terminal interface renders headings, emphasis, inline code, lists, blockquotes, tables, and fenced code. Fenced blocks have lightweight highlighting for Rust, Python, JavaScript, TypeScript, Go, C, C++, Bash, JSON, TOML, HTML, and CSS. Tool calls use separate full-width blocks with compact views for shell commands, file reads, writes, and edits.

## Slash commands

`/model` and `/settings` open lightweight keyboard pickers, including while a model response is still running. Use the arrow keys and Enter to choose, or Escape to close. Editable settings stay in the picker: Enter starts editing the current value, Enter again saves it, and the refreshed value is shown in the menu. A model or setting chosen during an active response is applied as soon as that response releases the agent, before the next message starts.

Messages submitted during an active response are queued automatically. Each pending message is shown below the live transcript with a `Queued` label, and the status bar shows the queue length. Run `/unqueue` to choose a pending message to remove, `/unqueue NUMBER` to remove one directly, or `/unqueue all` to clear the queue.

| Command | Effect |
| --- | --- |
| `/model [MODEL]` | Open a model picker, or switch the current session directly when `MODEL` is given |
| `/settings [KEY ...]` | Open the settings picker, or change a setting directly when arguments are given |
| `/new` | Start a new session without changing the current working directory |
| `/clear` | Alias for `/new` |
| `/compact` | Summarize older messages now |
| `/tools` | List builtin and discovered tools |
| `/skills` | List discovered skills and their search directories |
| `/skill:NAME [ARGS]` | Run a discovered Markdown skill |
| `/resume [ID\|NUMBER]` | Open the session picker, or resume directly by ID or number |
| `/unqueue [NUMBER\|all]` | Open the queued-message picker, remove one pending message, or clear the queue |
| `/help` | Show terminal controls and commands |
| `/quit` | Exit the terminal interface and print `yawl --session ID` |

## Models and configuration

Yawl reads `~/.yawl/config.json`, then applies values from `./.yawl/config.json`. Project values override global values. Every field is optional. If the merged config has no `model`, interactive startup runs onboarding; print mode requires `--model`.

```json
{
  "model": "omlx:Qwen3-Coder",
  "anthropic_base_url": "https://api.anthropic.com",
  "openai_base_url": "https://api.openai.com/v1",
  "max_tokens": 8192,
  "reasoning_effort": "high",
  "hide_reasoning": false,
  "auto_compact": true,
  "compact_threshold": 0.85,
  "context_windows": {
    "omlx:Qwen3-Coder": 65536
  }
}
```

Yawl has built-in `anthropic:`, `openai:`, and `openai-codex:` routes. It also has local presets for Ollama, LM Studio, and OMLX:

```sh
yawl -m anthropic:claude-sonnet-4-5
yawl -m openai:gpt-4o
yawl -m openai-codex:gpt-5.6-sol
yawl -m ollama:qwen2.5-coder:7b
yawl -m lmstudio:local-model-id
yawl -m omlx:local-model-id
```

The preset endpoints are `http://127.0.0.1:11434/v1` for Ollama, `http://127.0.0.1:1234/v1` for LM Studio, and `http://127.0.0.1:8000/v1` for OMLX. A model ID may contain colons, so `ollama:llama3.1:8b` selects provider `ollama` and model `llama3.1:8b`.

### Use a ChatGPT subscription

Choose "OpenAI Codex" during onboarding to use a ChatGPT Plus or Pro subscription. Yawl starts OpenAI's device-code flow, stores the OAuth credential in `~/.yawl/auth.json` with mode `0600`, and refreshes it before expiry. You can log in again without changing the selected model:

```sh
yawl --login openai-codex
```

The provider uses the ChatGPT Codex Responses endpoint with SSE streaming, tool calls, token usage, and encrypted reasoning replay for multi-step tool runs. When Codex returns a reasoning summary, Yawl displays it as one muted line before the answer. Supported model IDs are listed by `/model`. After choosing a Codex model, Yawl opens a second picker containing the reasoning efforts supported by that model. The selection is sent as the Responses API `reasoning.effort`; OAuth authenticates the account but does not itself return model capability metadata.

### Add an OpenAI-compatible provider

Add providers under `providers`. This uses the same field names as pi's `models.json`, so an `openai-completions` provider block can be copied with little or no editing. Here is an OMLX configuration:

```json
{
  "model": "omlx:Qwen3-Coder",
  "providers": {
    "omlx": {
      "baseUrl": "http://127.0.0.1:8000/v1",
      "api": "openai-completions",
      "apiKey": "$OMLX_API_KEY",
      "authHeader": true,
      "models": [
        {
          "id": "Qwen3-Coder",
          "name": "Qwen3 Coder (local)",
          "contextWindow": 65536,
          "maxTokens": 32768
        }
      ]
    }
  }
}
```

`models` is optional. It supplies labels and token limits for `/model`; Yawl still accepts an unlisted model ID. Custom providers use streaming OpenAI Chat Completions at `BASE_URL/chat/completions` and support text, tool calls, and full reasoning from `reasoning_content`, `reasoning`, or `reasoning_text` deltas. Yawl displays full reasoning as a separate multi-line block. The OMLX preset also sends saved full reasoning back as `reasoning_content` during tool loops.

Set `hide_reasoning` to `true`, choose "Reasoning display" in `/settings`, or run `/settings hide_reasoning on` to remove both summary and full reasoning from the TUI and print-mode output. Yawl still records the reasoning in the session so it reappears if the setting is turned off. In print mode, visible reasoning goes to standard error and the answer remains on standard output.

Choose "Accent color" in `/settings` to set the status bar and text-box border from one palette. The same value can be set directly with `/settings accent_color blue` or `/settings accent_color '#7aa2f7'`. Palette names and `#RRGGBB` values are accepted; the default is white.

Provider keys and header values accept `$ENV_VAR` and `${ENV_VAR}` references. If `apiKey` is omitted, Yawl also checks an environment variable derived from the provider name, such as `OMLX_API_KEY` or `LMSTUDIO_API_KEY`. Keyless local servers need no placeholder key. Extra pi model fields such as `cost`, `input`, and `reasoning` are ignored.

These compatibility fields are supported at provider or model level:

```json
{
  "compat": {
    "supportsUsageInStreaming": false,
    "supportsFinishReason": false,
    "requiresToolResultName": true,
    "requiresReasoningContentOnAssistantMessages": true,
    "maxTokensField": "max_tokens"
  }
}
```

You can configure an endpoint from the TUI without editing JSON:

```text
/settings provider omlx http://127.0.0.1:8000/v1 $OMLX_API_KEY
/settings model omlx:Qwen3-Coder
```

Omit the key for a keyless server. Pass `-` in the key position to remove a saved key. `/settings` writes `~/.yawl/config.json` with mode `0600`; `./.yawl/config.json` can still override it. When that happens, Yawl reports that the global value was saved while the project value remains effective. `/settings` also changes `max_tokens`, Codex reasoning effort, reasoning visibility, the TUI accent color, automatic compaction, the compaction threshold, context windows, and built-in endpoint URLs.

## Sessions and compaction

Yawl stores append-only JSONL session files in `~/.yawl/sessions/`. Each user message, assistant response, reasoning block, tool result, and compaction event is written as it happens. The original history remains in the log after compaction. `/new` starts a blank session without changing the current working directory. Leaving the terminal interface prints `yawl --session ID` so you can resume that conversation.

Yawl checks the last provider-reported token usage before each request. At the configured threshold, 85 percent by default, it asks the current model to summarize the older conversation and keeps roughly the last ten messages unchanged. Use `/compact` to do this manually.

## Builtin tools

The model always has these tools:

- `shell` runs `sh -c` in the current directory, with a 120-second default timeout.
- `read_file` reads a UTF-8 file.
- `write_file` writes a file and creates missing parent directories.
- `edit_file` performs one exact string replacement and rejects missing or repeated matches.

Tool output sent back to the model is capped at 60,000 characters. A command timeout or `Ctrl+C` kills the command's process group so child processes do not remain behind.

Yawl has no approval prompt or permission layer. Review the current model and working directory before giving it a task. Use `Ctrl+C` to stop the active turn.

## Add an exec tool

Yawl scans these directories before every model step:

1. `~/.yawl/tools/`
2. `./.yawl/tools/`

Project tools override global tools with the same name. Describe results are cached until the executable's modification time changes.

An executable must support two operations:

1. With `--describe`, print one JSON object containing `name`, `description`, `input_schema`, and an optional `timeout_secs`.
2. During a normal call, read JSON arguments from standard input. Print the result to standard output. Exit nonzero to mark the result as an error; Yawl appends standard error to that result.

The process inherits Yawl's working directory and receives the current session ID in `YAWL_SESSION_ID`.

This Python example adds a `word_count` tool:

```python
#!/usr/bin/env python3
import json
import sys

if "--describe" in sys.argv:
    print(json.dumps({
        "name": "word_count",
        "description": "Count words in supplied text",
        "input_schema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        },
        "timeout_secs": 10
    }))
    raise SystemExit(0)

arguments = json.load(sys.stdin)
print(len(arguments["text"].split()))
```

Install it for the current project:

```sh
mkdir -p .yawl/tools
cp word_count .yawl/tools/word_count
chmod +x .yawl/tools/word_count
.yawl/tools/word_count --describe
printf '{"text":"small useful core"}' | .yawl/tools/word_count
```

The model can also create, mark executable, test, and call such a tool within one agent turn because Yawl rescans the registry after every tool batch.

## Skills

Yawl discovers Markdown skills from `~/.yawl/skills/` and `~/.agents/skills/` by default. A skill may be either `NAME/SKILL.md` or `NAME.md`; optional YAML frontmatter can provide `name` and `description`. Skills appear in the `/` completion menu as `/skill:NAME` and can receive trailing instructions, for example `/skill:review focus on security`.

Manage search directories from the TUI:

```text
/settings skills add ~/shared/skills
/settings skills remove ~/.agents/skills
```

The resulting `skill_dirs` array is stored in `~/.yawl/config.json`. Later directories override earlier directories when skill names collide.

## Project instructions

Yawl reads global instructions from `~/.yawl/AGENTS.md` and project instructions from `AGENTS.md` in the current directory. It appends the global file first and the project file second, after the built-in system prompt. Empty or missing files are ignored. Use these files for commands, constraints, and context that should apply to every project or only the current repository.

## Development

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
