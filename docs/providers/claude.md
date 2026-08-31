# Claude Code compatibility

Contract basis: the official [Claude Code hooks reference](https://code.claude.com/docs/en/hooks).
The managed exact-match table maps `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`,
`Elicitation`, `Notification`, `PreCompact`, `PostCompact`, `Stop`,
`StopFailure`, and `SessionEnd`.
`UserPromptSubmit` starts a turn, tool activity marks working, permission prompts
mark waiting for approval, input notifications mark waiting for input, and
`Stop`/`StopFailure` leave a live process publicly stopped. `Stop` with exact
`is_interrupt: true` is stopped without a completion or failure reason. A documented
`idle_prompt` explicitly moves it to idle. Raw prompts, arbitrary tool input,
transcript paths, and complete assistant messages are discarded; a root Stop
may select the first 100 sanitized characters of `last_assistant_message` as a
current `completed` reason.
When an authenticated root hook supplies a readable `transcript_path` matching
the provider session beneath `$HOME/.claude/projects`, the Claude adapter
schedules bounded background collection and returns only the latest bounded
`aiTitle` or `customTitle` plus normalized usage. The transcript body, path,
response identities, cursor, cancellation state, and diagnostics are not
normalized or persisted.

Assistant usage is counted once per non-empty `message.id`, falling back to
`requestId`; fresh input, cache reads, and cache creation form cumulative
input, while output is summed once per response. Rows without either identity
are ignored. The latest verified response input components supply current
context tokens. Claude context-window percentage remains absent because the
supported hook/transcript evidence does not provide an exact denominator.

`sessiontap setup claude`, doctor, and hook removal manage hooks only and leave
`statusLine` untouched. SessionTap does not read or execute statusline
configuration. For a development environment that installed the old managed
wrapper, follow the one-time manual restoration procedure in the README.

Direct `PermissionRequest` carries bounded approval context. `AskUserQuestion`
and MCP `Elicitation` are ordinary input waits. `agent_needs_input` is an input
wait, `permission_prompt` asserts approval, and `idle_prompt` is explicit
idle rather than completion. Post-tool signals resume work. `Stop` means
completion; `StopFailure` exposes only an allowlisted failure category when
available. Approval summaries use a normalized tool label plus the first 100
sanitized description characters, falling back to the command excerpt.

An independently sanitized capture maps `prompt_id` to current turn and
allowlisted `permission_mode` (including `dontAsk`) and `effort.level` to provider metadata. At most the first sanitized question is selected
as current input context; options and remaining question content are discarded.

Hooks with a non-empty `agent_id`, plus `SubagentStart` and `SubagentStop`, are
ignored before extracting root activity, reasons, session metadata, or usage.
An `agent_type` without `agent_id` is still treated as the main session.

Minimum locally tested version: Claude Code 2.1.241. New event fields and
unsupported exact event names or notification subtypes are ignored. Live smoke testing is opt-in; see
`docs/smoke-tests.md`.
