# Codex compatibility spike

Contract basis: the official [Codex hooks documentation](https://learn.chatgpt.com/docs/hooks),
checked 2026-08-25 against Codex CLI 0.149.1. Codex discovers user hooks in
`~/.codex/hooks.json`. Matching command hooks run concurrently. Non-managed
hooks are trusted by the user against the exact definition hash through
`/hooks`; SessionTap deliberately does not edit undocumented trust storage.

The exact-match table installs command hooks for `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `UserInputRequest`,
`PostToolUse`, `PreCompact`, `PostCompact`, `Interrupt`, `Stop`, and
`SessionEnd`.
SessionTap setup preserves unrelated indices. After setup or refresh, `doctor`
reports that review is needed and the user must trust the entry in `/hooks`.
Unsupported versions degrade to process lifecycle observation and never block
provider launch.

Permission requests carry bounded approval context. Documented
`UserInputRequest`/question forms map to ordinary input when present, and
post-tool signals resume work. `turn_id` is retained as the sanitized current
turn identity. `Interrupt` makes the live agent publicly stopped without a
completion or failure reason. `Stop` makes the live agent publicly stopped and may expose the
first 100 sanitized characters of `last_assistant_message` as a current
`completed` reason. `SessionEnd` closes only the provider session and preserves
the current activity; wrapper exit remains authoritative. The supported public contract has no reliable turn-failure hook, so a
nonzero process exit is not promoted to a turn-level failure.

Approval summaries use `<normalized-tool> <description>`, falling back to the
first 100 sanitized command characters. Documented permission modes include
`dontAsk`. Payloads with non-empty `agent_id`, `SubagentStart`, or
`SubagentStop` are ignored before root normalization.

Minimum locally tested version: Codex CLI 0.149.1. Unknown fields and
unsupported exact event names are ignored, and raw hook input is never persisted.
When a root hook supplies a transcript locator, SessionTap scans the rollout
asynchronously and retains only the latest valid non-null `token_count` info
snapshot. Cumulative input/output come from `total_token_usage`; current
context comes from `last_token_usage.total_tokens`, with a whole used percent
only for a positive `model_context_window`. Snapshots are never summed, and a
missing locator or denominator leaves the corresponding fields absent.

During the same collection, SessionTap also scans the provider-owned
`~/.codex/session_index.jsonl` for records whose `id` exactly matches the
authenticated Codex session. The sanitized `thread_name` from the last
complete, valid matching record in file order is used; `updated_at` is not
interpreted. An index name takes precedence over rollout `session_name` or
`title`, while the rollout name remains the fallback.

Index collection accepts only a regular, non-symlink file and applies the same
64 MiB total-scan and 1 MiB line limits as rollout collection. An incomplete
non-newline-terminated tail is ignored to tolerate concurrent appends.
Missing, unreadable, malformed, unsafe, oversized, or non-matching index data
fails open: name enrichment is omitted or falls back to rollout metadata, and
valid rollout usage remains publishable.
