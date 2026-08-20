# Yawl

Yawl is a small AI agent runner for macOS and Linux. It has a full-screen terminal interface, a scriptable print mode, Anthropic and OpenAI-compatible providers, persistent sessions, tool calling, and automatic context compaction.

The model can add tools without recompiling Yawl. Any executable in `~/.yawl/tools/` or `./.yawl/tools/` becomes a callable tool when it implements the exec-tool contract below.

Yawl is one Rust crate with blocking I/O and four direct dependencies: `ureq`, `serde`, `serde_json`, and `libc`.

## Install

Yawl requires Rust 1.88 or newer.

```sh
git clone https://github.com/alan13367/yawl.git
cd yawl
cargo install --path .
```

Set the API key for the provider you use:

```sh
export ANTHROPIC_API_KEY="..."
export OPENAI_API_KEY="..."
```

`OPENAI_API_KEY` may be empty for a local server that does not require authentication.

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
- `Up` and `Down` browse input history.
- `Ctrl+U`, `Ctrl+K`, and `Ctrl+W` delete text.
- The mouse wheel and `PageUp` or `PageDown` move through Yawl's internal scrollback.
- `Ctrl+C` aborts the active model response or tool. It does not exit Yawl.

The terminal interface renders headings, emphasis, inline code, lists, blockquotes, tables, and fenced code. Fenced blocks have lightweight highlighting for Rust, Python, JavaScript, TypeScript, Go, C, C++, Bash, JSON, TOML, HTML, and CSS.

## Slash commands

| Command | Effect |
| --- | --- |
| `/model [MODEL]` | Show or switch the model for the current session |
| `/clear` | Start a new session |
| `/compact` | Summarize older messages now |
| `/tools` | List builtin and discovered tools |
| `/resume [ID\|NUMBER]` | List or resume saved sessions |
| `/help` | Show terminal controls and commands |
| `/quit` | Exit the terminal interface |

## Models and configuration

Yawl reads `~/.yawl/config.json`, then applies values from `./.yawl/config.json`. Project values override global values. Every field is optional.

```json
{
  "model": "claude-sonnet-4-5",
  "anthropic_base_url": "https://api.anthropic.com",
  "openai_base_url": "http://localhost:11434/v1",
  "max_tokens": 8192,
  "compact_threshold": 0.85,
  "context_windows": {
    "openai:local-model": 32768
  }
}
```

Model names beginning with `claude` use Anthropic. Other names use the OpenAI-compatible endpoint. Prefix a model with `anthropic:` or `openai:` to select the provider explicitly:

```sh
yawl -m anthropic:claude-sonnet-4-5
yawl -m openai:local-model
```

The OpenAI-compatible provider works with servers that expose streaming chat completions, including OpenAI, Ollama, llama.cpp, and OpenRouter. Set `openai_base_url` to the API root that contains `/chat/completions`.

## Sessions and compaction

Yawl stores append-only JSONL session files in `~/.yawl/sessions/`. Each user message, assistant response, tool result, and compaction event is written as it happens. The original history remains in the log after compaction.

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

## Project instructions

If `YAWL.md` exists in the current directory, Yawl appends it to the built-in system prompt. Use it for repository-specific commands, constraints, and context.

## Development

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
