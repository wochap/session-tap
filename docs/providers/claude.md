# Claude Code compatibility

Contract basis: the official [Claude Code hooks reference](https://code.claude.com/docs/en/hooks).
The MVP maps `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PermissionRequest`, `Notification`, `Stop`, `StopFailure`, and `SessionEnd`.
`UserPromptSubmit` starts a turn, tool activity marks working, permission prompts
mark waiting for approval, input notifications mark waiting for input, and
`Stop`/`StopFailure` return a live process to idle. Raw prompts, tool input,
transcript paths, and assistant messages are discarded.

Minimum locally tested version: Claude Code 2.1.241. New event fields and
unknown events are ignored. Live smoke testing is opt-in; see
`docs/smoke-tests.md`.

