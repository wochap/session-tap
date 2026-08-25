# Provider compatibility matrix

| Provider | Minimum tested | Hooks | Usage | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | 2.1.241 | Yes | Optional | Hook-derived state; usage may be absent |
| Codex CLI | 0.149.1 | Yes, after `/hooks` trust | Optional | Unknown/new hooks degrade safely |
| Qwen Code | Not yet established | Contract implemented | Optional side channel | No local binary was available |

Usage, context, repository, provider-session, and tmux fields are optional.
Absence means unavailable, not zero. This matrix makes no compatibility promise
beyond tested contracts.

