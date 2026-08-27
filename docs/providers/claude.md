# Claude Code compatibility

Contract basis: the official [Claude Code hooks reference](https://code.claude.com/docs/en/hooks).
The MVP maps `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PermissionRequest`, `Notification`, `Stop`, `StopFailure`, and `SessionEnd`.
`UserPromptSubmit` starts a turn, tool activity marks working, permission prompts
mark waiting for approval, input notifications mark waiting for input, and
`Stop`/`StopFailure` return a live process to idle. Raw prompts, tool input,
transcript paths, and assistant messages are discarded.
When a readable `transcript_path` is supplied, SessionTap extracts only the
latest bounded `aiTitle` or `customTitle` value for provider-session metadata;
the transcript body and path are not normalized or persisted.

Direct `PermissionRequest` carries bounded approval context. `AskUserQuestion`
and MCP `Elicitation` are ordinary input waits. `agent_needs_input` is an input
wait, delayed `permission_prompt` is broker-deduplicated, and `idle_prompt` is
enrichment rather than completion. Post-tool signals resume work. `Stop` means
completion; `StopFailure` carries only an allowlisted failure category.

An independently sanitized capture maps `prompt_id` to current turn and
allowlisted `permission_mode` and `effort.level` to provider metadata, but
establishes no usage fields. Question/options content is discarded.

Minimum locally tested version: Claude Code 2.1.241. New event fields and
unknown events are ignored. Live smoke testing is opt-in; see
`docs/smoke-tests.md`.
