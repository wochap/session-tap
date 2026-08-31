# Qwen Code compatibility

Contract basis: the official [Qwen Code hooks documentation](https://qwenlm.github.io/qwen-code-docs/en/users/features/hooks/).
Dual-output behavior and flags follow the official [Dual Output contract](https://qwenlm.github.io/qwen-code-docs/en/users/features/dual-output/).
The MVP maps session, prompt, tool, permission, notification, stop/failure,
subagent, and session-end signals. It can add `--json-file <private-path>` only
when a cached/help capability probe confirms support and the user did not
already supply `--json-file` or `--json-fd`. It never selects headless
stream-JSON output.

No Qwen binary was available in the 2026-08-25 development environment, so a
minimum tested version is not yet claimed. Hook-only observation is the safe
fallback. Side-channel lines are bounded, parsed only after a newline, and
restart at offset zero after truncation.

Direct permission requests and exact `permission_prompt` notifications carry bounded approval context;
`ask_user_question` and documented input-needed notifications are ordinary
input waits. Observed `auto`, `auto_edit`, `plan`, and `yolo` permission modes are retained,
and valid hook timestamps become the event's provider-observed time. Empty
`UserPromptSubmit` callbacks emitted around tool activity are enrichment rather
than new turns. Repeated permission reminders are broker-deduplicated,
`idle_prompt` is explicit idle, and post-tool signals resume work. `Stop` with
exact `is_interrupt: true` is stopped without a completion or failure reason.
`Stop` otherwise makes
the live root agent publicly stopped and may select the first 100 sanitized
characters of `last_assistant_message` as `completed`; `StopFailure` may expose
only an allowlisted category. Raw failure details and complete assistant
messages are excluded. Approval summaries use a normalized tool label plus a
bounded description or command excerpt.

Any non-empty `agent_id`, `SubagentStart`, or `SubagentStop` payload is ignored
before root activity, reason, session metadata, or usage extraction. An
`agent_type` alone does not cause exclusion.

An independently sanitized stop capture establishes top-level `input_tokens`
and fractional `context_usage`; `0.5` becomes 50 percent. Private transcript
collection also sums `promptTokenCount` and `candidatesTokenCount` from
assistant records only. The latest prompt count is current context and is
divided by its positive `contextWindowSize`; compaction can lower current
context while cumulative totals remain monotonic. UI telemetry and
`totalTokenCount` are not added.
Unsupported exact event names and notification subtypes are ignored.
