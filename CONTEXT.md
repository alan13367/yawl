# Domain context

- **Model target:** The selected model plus the provider needed to run it. A target may use an explicit prefix, such as `openai-codex:gpt-5.6-sol`, or resolve from an unprefixed model name.
- **Conversation transaction:** One persisted user-to-assistant cycle. It includes provider retries, tool calls and results, compaction, interruption recovery, and session writes.
- **Transcript:** The terminal-facing conversation state reconstructed from live turn events or saved session messages.
