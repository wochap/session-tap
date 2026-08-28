# Security and privacy review

Reviewed 2026-08-25 against the MVP threat model and the applicable OWASP Top
10 categories for access control, injection, insecure design, vulnerable
components, integrity, logging/privacy, and server-side requests.

- Unix runtime directories are mode 0700; the socket is mode 0600; symlinked
  runtime/database/hook/token targets are rejected where opened, while
  symlinked configuration files are resolved to their targets (dangling links
  are rejected with an explicit error).
- The daemon is the sole SQLite writer and WAL/outbox changes are transactional.
- Hook attribution requires invocation ID, exact provider, and a random
  per-invocation credential. Credentials are environment-only and compared
  without early byte mismatch.
- Stored/forwarded arguments are arrays with generic/provider redaction. Raw
  hook payloads, prompts, transcripts, terminal streams, and tool inputs are
  transient and excluded.
- Remote cleartext HTTP is rejected; timeouts and payload caps are bounded.
- tmux input uses `load-buffer`/`paste-buffer`, not a shell string, and target
  identities are checked immediately before each operation.
- Managed provider-hook commands use POSIX single-quote escaping for executable
  paths and provider names; a regression test covers spaces, quotes, and shell
  metacharacters.
- SQLite access uses parameterized statements, provider arguments use process
  argument arrays, and Rust source forbids `unsafe` code at the workspace level.
- `cargo audit` reported no known RustSec vulnerabilities and
  `cargo deny check licenses` passed for the locked dependency graph.

Known MVP limits: local same-user processes remain inside the OS trust boundary;
provider contracts can change; local same-user configuration remains trusted;
and manual provider/TUI tests remain necessary. The SessionTap name is only
provisionally acceptable and still requires public-release clearance.
