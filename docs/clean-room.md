# Clean-room provenance policy

SessionTap is original MIT-licensed work. Contributors must not read for
implementation, copy, translate, or mechanically transform source, tests,
generated scripts, fixtures, or implementation-specific hook trust logic from
any external non-MIT or restrictively licensed codebase into this repository.

Acceptable evidence is limited to public provider documentation, public CLI
help/version output, and behavior independently captured by running a provider.
Committed captures must remove prompts, transcripts, paths, account data,
tokens, tool inputs, and other secrets. Hand-authored synthetic fixtures are
preferred and must identify the public contract they model.

`sessiontap inspect-hooks` is for independently observing public provider
behavior. Reproduce with synthetic, non-sensitive inputs where possible,
redact before sharing, and never commit raw account payloads. Any mapping based
on a capture must record the provider version and either cite the public
provider contract or identify the behavior as an independent observation.

The local hook configuration samples supplied by the project owner were used
only to confirm event names and merge shapes; no notification script or vendor
implementation was copied.

Final review on 2026-08-25 found no copied or transformed external
implementation material. Provider mappings cite public contracts, and all
committed provider fixtures are minimal synthetic records without prompts,
paths, credentials, transcripts, tool inputs, or account data.

Metadata mappings use sanitized independent captures for Codex session/turn
ordering, Claude prompt/permission/effort, and Qwen top-level usage. Captures
contain no conversational or raw tool content.
