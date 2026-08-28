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

Direct permission requests carry bounded approval context;
`ask_user_question` and documented input-needed notifications are ordinary
input waits. Observed `auto`, `plan`, and `yolo` permission modes are retained,
and valid hook timestamps become the event's provider-observed time. Empty
`UserPromptSubmit` callbacks emitted around tool activity are enrichment rather
than new turns. Delayed permission reminders are broker-deduplicated,
`idle_prompt` is enrichment, and post-tool signals resume work. `Stop` means
completion; `StopFailure` carries only an allowlisted category. Raw failure
details and assistant messages are excluded.

An independently sanitized stop capture establishes top-level `input_tokens`
and fractional `context_usage`; `0.5` becomes 50 percent. Other usage remains unknown.
