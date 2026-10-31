---
name: yawl
description: Delegate a task to Yawl, a terminal coding agent that runs one task to completion on any model the user has configured (local Ollama, LM Studio, oMLX, or vLLM models, OpenAI, Anthropic, or a ChatGPT subscription through Codex) and returns one JSON result. Use when the user asks to involve yawl, to use a local, cheaper, or different model, to get a second opinion from another model family, or to offload a self-contained coding, review, or research task to another agent.
---

# Delegating to Yawl

Yawl is a command-line coding agent. `yawl --json --no-stdin -m MODEL "TASK"` runs TASK in the current directory with its own tools (shell, file reading and editing, search, web, its own subagents), then prints one JSON object describing the result. These instructions were written for yawl {{version}}; `yawl --help` is authoritative if they disagree.

Use it to:

- run work on a model you cannot call yourself: a local model, or a ChatGPT subscription through Codex;
- get an independent review or second opinion from a different model family;
- offload self-contained work, such as a mechanical refactor, a test-writing pass, or a codebase survey, to save your own context;
- run several independent investigations in parallel.

Do not delegate work that depends on details only you know unless you write them into the prompt. Yawl sees the repository, not your conversation.

## 1. Check readiness once per session

```sh
yawl --check --json
```

Read `ready`. If it is `false`, each entry in `problems` has `severity`, `message`, and a `fix`. Configuration and logins are the user's to fix: show them the `fix` and stop. Never edit `~/.yawl` yourself. `--check --refresh --json` also contacts the provider to prove the endpoint and credential work, without spending a completion.

## 2. Pick a model

```sh
yawl --list-models --json            # saved models, instant, offline
yawl --list-models --refresh --json  # also ask every provider what it serves now
```

Each entry in `models` has an `id` to pass to `-m`, plus `context_window`, `reasoning_efforts`, `images`, and `default`. `providers` reports each provider's `credential` (`missing` means it cannot be used), and with `--refresh` whether it is `reachable`. Use the model the user names. Otherwise omit `-m` to use their default, which is the entry marked `default`. If you choose a model yourself, say which one you chose.

Any id the provider serves works with `-m`, even if it is not listed: `ollama:qwen3-coder:30b`, `openai-codex:gpt-5.5`, `anthropic:claude-sonnet-4-5`. The text before the first colon names the provider.

## 3. Run the task

```sh
yawl --json --no-stdin -m MODEL "TASK"
```

- Always pass `--no-stdin` unless you pipe input. Without it, yawl reads standard input until it closes, and a harness that leaves stdin open makes the call hang forever.
- To give yawl a large context, pipe it instead of quoting it: `git diff main | yawl --json -m MODEL "Review this diff for bugs"`. The piped text is appended to the prompt.
- Yawl works in the current directory. Run it from the project root.
- A run takes seconds to many minutes. Run long tasks with your harness's background or long-timeout execution, and bound them with `--timeout SECONDS`.

### Read the result

Standard output is exactly one JSON object:

```json
{
  "type": "result",
  "status": "completed",
  "exit_code": 0,
  "session_id": "20261028-142233-0f3a",
  "model": "ollama:qwen3-coder:30b",
  "text": "Fixed the off-by-one in src/range.rs and added a regression test. cargo test passes.",
  "error": null,
  "duration_ms": 48213,
  "requests": 7,
  "usage": {"input_tokens": 51234, "output_tokens": 2310, "cached_input_tokens": 40960, "cache_write_input_tokens": 0},
  "context": {"tokens": 18342, "window": 65536},
  "tool_calls": 12,
  "tool_errors": 1,
  "edited_files": ["src/range.rs", "tests/range.rs"],
  "warnings": []
}
```

- `status` is `completed`, `error`, `timeout`, or `interrupted`. Only `completed` means the model finished.
- `text` is yawl's final answer. It is the part to read, summarize, or act on.
- `edited_files` lists the files yawl's own file tools changed. Changes made through shell commands are not listed, so run `git status` or `git diff` to see everything that changed.
- `error` is `{kind, message, hint}` when the run failed.
- Standard error carries warnings for people; you can ignore it.

Treat `text` as another agent's claims, not as facts. Verify important claims, especially "tests pass", before you report them to the user.

### Exit codes

| Code | `error.kind` | Meaning and what to do |
|------|--------------|------------------------|
| 0 | | Completed. |
| 1 | `io`, `error` | Unexpected failure. Report `error.message`. |
| 2 | `usage`, `invalid` | Bad arguments or an empty prompt. Fix the command; `yawl --help` lists every option. |
| 3 | `config`, `auth` | No usable model, or a missing or rejected credential. Run `yawl --check --json` and give the user its fix. |
| 4 | `provider` | The provider failed or stopped responding after retries, or the model does not exist. Retry later, or pick another model. |
| 124 | `timeout` | `--timeout` expired. The work so far is saved: continue it with `--session`. |
| 130 | `interrupted` | Stopped by a signal. |

## 4. Choose how much yawl may do

Yawl has no approval prompts. A normal run can run any shell command and edit any file the user can. Choose the narrowest mode that fits the task:

- **Read-only** (investigation, review, questions): add `--agent scout`. Yawl gets only file reading, search, and Git inspection tools, with no shell and no writes. Use this whenever the task does not need changes.
- **Full** (implementation): no preset. Say in the prompt exactly which files or areas it may change, and check `git diff` afterwards.

`yawl --list-agents --json` lists the presets `--agent` accepts. Users can define their own.

Before a full run, make sure the user's uncommitted work is safe: commit or stash it first, or tell the user that yawl will edit the working tree. Give parallel runs that write files disjoint files.

## 5. Write the prompt

Yawl starts with no knowledge of your conversation. A good delegated prompt is self-contained:

1. The goal, in one sentence.
2. The relevant files, functions, errors, or commands, with paths.
3. Constraints: what not to touch, style rules, and whether to run tests.
4. What to return, such as "Reply with the root cause, the fix, and the test command you ran."

Yawl reads the repository's `AGENTS.md`, so do not repeat project conventions it already documents.

## 6. Follow up

Every result carries a `session_id`. Continue the same conversation with full history:

```sh
yawl --json --no-stdin --session SESSION_ID "Also handle the empty-range case"
```

Use `--session`, not `-c`: `-c` resumes the latest session in the directory, which may belong to another run. Add `--ephemeral` to one-off runs you will never continue, so they stay out of the user's session list. They then report `"session_id": null`.

## More options

- `--reasoning EFFORT` sets the reasoning effort when the model supports one. Valid values are the model's `reasoning_efforts`.
- `--format jsonl` streams one JSON event per line (`start`, `text`, `message`, `tool_start`, `tool_end`, `usage`, `warning`, `retry`, ...), and the last line is the same `result` object. Use it when you want progress from a long run.
- `--trust-project` lets yawl use this repository's own yawl skills, tools, and provider settings. Pass it only when the user says they trust the repository. `yawl --check --json` warns when such settings are being ignored.
- `--list-sessions --json` lists resumable sessions in the current directory.
- Text mode (no `--json`) streams the answer as plain text for people, which is hard to parse. Prefer `--json`.

## Examples

```sh
# Second-opinion review on a local model, read-only
git diff --staged | yawl --json --agent scout -m ollama:qwen3-coder:30b \
  "Review this staged diff. List real bugs with file:line and why; skip style nits."

# Implementation on the user's default model, bounded to 20 minutes
yawl --json --no-stdin --timeout 1200 \
  "In src/parser.rs, make parse_duration accept '1h30m'. Add unit tests beside the existing ones and run cargo test parser. Change no other files."

# Survey a codebase without spending your own context
yawl --json --no-stdin --agent scout --ephemeral \
  "Map how authentication works in this repository: entry points, token storage, and session expiry, with file paths."
```
