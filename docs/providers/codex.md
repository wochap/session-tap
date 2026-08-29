# Codex compatibility spike

Contract basis: the official [Codex hooks documentation](https://learn.chatgpt.com/docs/hooks),
checked 2026-08-25 against Codex CLI 0.149.1. Codex discovers user hooks in
`~/.codex/hooks.json`. Matching command hooks run concurrently. Non-managed
hooks are trusted by the user against the exact definition hash through
`/hooks`; SessionTap deliberately does not edit undocumented trust storage.

The MVP installs command hooks for `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`, and `SessionEnd`.
SessionTap setup preserves unrelated indices. After setup or refresh, `doctor`
reports that review is needed and the user must trust the entry in `/hooks`.
Unsupported versions degrade to process lifecycle observation and never block
provider launch.

Permission requests carry bounded approval context. Documented
`UserInputRequest`/question forms map to ordinary input when present, and
post-tool signals resume work. `turn_id` is retained as the sanitized current
turn identity. `Stop` makes the live agent publicly stopped and may expose the
first 100 sanitized characters of `last_assistant_message` as a current
`completed` reason. `SessionEnd` closes only the provider session and preserves
the current activity; wrapper exit remains authoritative. The supported public contract has no reliable turn-failure hook, so a
nonzero process exit is not promoted to a turn-level failure.

Approval summaries use `<normalized-tool> <description>`, falling back to the
first 100 sanitized command characters. Documented permission modes include
`dontAsk`. Payloads with non-empty `agent_id`, `SubagentStart`, or
`SubagentStop` are ignored before root normalization.

Minimum locally tested version: Codex CLI 0.149.1. Unknown fields/events are
ignored and raw hook input is never persisted. Supplied sanitized captures do
not establish token or context usage, so those fields remain absent.
