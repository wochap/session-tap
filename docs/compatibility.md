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

SessionTap is single-user alpha software. The public-view cutover intentionally
does not migrate or read older local or hub protocols and databases. If an
existing `sessiontap` or `sessiontap-hub` SQLite database is incompatible,
stop the corresponding daemon, remove that database, and let the service
recreate it. No schema-version increment or compatibility shim is provided.

`PublicAgentView` is observer-facing, not non-sensitive: working directories,
repository paths, session names, model metadata, usage, and bounded blocked
reasons can disclose personal or project information. Preserve local socket
permissions, sink authentication, and transport security.

Running/stopped reasons, a public `last_outcome`, provider-store/history
collection, and remote control are deferred. Internal lifecycle, activity,
normalized event kinds, process identities, and multiplexer state remain
reducer/control-only data.
