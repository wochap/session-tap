# Provider compatibility matrix

| Provider | Minimum tested | Hooks | Usage | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | 2.1.241 | Yes | Unknown in supplied hooks | Prompt/permission/effort metadata available |
| Codex CLI | 0.149.1 | Yes, after `/hooks` trust | Unknown in supplied hooks | Turn/model metadata available when supplied |
| Qwen Code | Not yet established | Contract implemented | Input tokens/context percent | Sanitized capture-backed mapping |

Usage, context, provider metadata, repository, provider-session, and tmux fields are optional.
Absence means unavailable, not zero. This matrix makes no compatibility promise
beyond tested contracts.

## Alpha data and deferred fields

SessionTap is single-user alpha software. The stopped-reason change is a
breaking public semantic and daemon/hub binaries must be upgraded together.
The daemon migrates legacy blocked-attention rows into the generalized current
status-reason table; the canonical alpha wire schema remains version 1.

`PublicAgentView` is observer-facing, not non-sensitive: working directories,
repository paths, session names, model metadata, usage, and bounded status
reasons can disclose personal or project information. Preserve local socket
permissions, sink authentication, and transport security.

Stopped `completed`/`failed` reasons are current-state-only. A live completed
agent stays stopped until a verified idle signal or reliable new work; process
exit can also produce stopped with no reason. Provider-store/history collection,
a public lifecycle field, and remote control remain deferred. Internal
lifecycle, activity, normalized event kinds, process identities, and
multiplexer state remain reducer/control-only data.
