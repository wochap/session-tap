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
  hook payloads, complete prompts/assistant messages, transcripts, terminal
  streams, and arbitrary tool inputs are transient and excluded. The explicit
  observer contract may select only a control-free, whitespace-collapsed
  100-character question, description, command, final response, or allowlisted
  failure summary as the latest current status reason.
- Hook-supplied transcript locators travel only over the authenticated local
  daemon protocol. They never enter normalized events, persisted invocation
  snapshots, public envelopes, sinks, or hub state. Collection canonicalizes
  beneath the dialect-owned root, rejects final symlinks and non-regular files,
  verifies session/file identity, and enforces 64 MiB scan and 1 MiB line
  limits with checked offsets and token arithmetic.
- Provider setup and collection never inspect, install, wrap, execute, or
  remove statusline configuration. Legacy development wrappers require the
  explicit manual restoration documented in the README.
- Non-empty provider `agent_id` payloads and subagent lifecycle hooks are
  ignored before root activity, metadata, usage, or reason extraction.
- Configured sinks are trusted by the single operator and can receive bounded
  public reasons; sink access control and transport security protect that
  potentially sensitive observer data.
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

Collection is coalesced by configured provider, concrete adapter, and provider
session ID. Newer generations cooperatively cancel older scans; results are
revalidated against generation, session, credential, and invocation binding
before reduction. Collection failures emit bounded local diagnostics without
artifact paths or contents and preserve the last verified usage. Known MVP limits: local same-user processes remain inside the OS trust boundary;
provider contracts can change; local same-user configuration remains trusted;
and manual provider/TUI tests remain necessary. The SessionTap name is only
provisionally acceptable and still requires public-release clearance.
