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
