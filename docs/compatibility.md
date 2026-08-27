# Provider compatibility matrix

| Provider | Minimum tested | Hooks | Usage | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | 2.1.241 | Yes | Unknown in supplied hooks | Prompt/permission/effort metadata available |
| Codex CLI | 0.149.1 | Yes, after `/hooks` trust | Unknown in supplied hooks | Turn/model metadata available when supplied |
| Qwen Code | Not yet established | Contract implemented | Input tokens/context percent | Sanitized capture-backed mapping |

Usage, context, provider metadata, repository, provider-session, and tmux fields are optional.
Absence means unavailable, not zero. This matrix makes no compatibility promise
beyond tested contracts.
