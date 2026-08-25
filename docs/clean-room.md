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

The local hook configuration samples supplied by the project owner were used
only to confirm event names and merge shapes; no notification script or vendor
implementation was copied.

Final review on 2026-08-25 found no copied or transformed external
implementation material. Provider mappings cite public contracts, and all
committed provider fixtures are minimal synthetic records without prompts,
paths, credentials, transcripts, tool inputs, or account data.
