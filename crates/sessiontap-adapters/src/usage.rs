//! Private, bounded provider-artifact usage collection.
//!
//! This module deliberately returns typed counters only. Paths, JSON records,
//! response identifiers, and cursors remain on the daemon's local path.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sessiontap_core::domain::{
    ArtifactIdentity, ArtifactLocator, CollectorCursor, CollectorDialect, PartialUsageObservation,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::{fs::MetadataExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

// Representative sanitized sessions were well below these ceilings. They
// leave headroom for long resumptions while bounding restart reconstruction
// and preventing one record from consuming the whole scan budget.
pub const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionResult {
    pub observation: PartialUsageObservation,
    pub cursor: CollectorCursor,
    pub identity: ArtifactIdentity,
}

pub fn collect(
    home: &Path,
    locator: &ArtifactLocator,
    prior: Option<&CollectorCursor>,
) -> Result<CollectionResult> {
    let mut artifact = open_artifact(home, locator)?;
    let incremental = prior.is_some_and(|cursor| {
        cursor.dialect == Some(locator.dialect)
            && cursor.identity.as_ref().is_some_and(|identity| {
                identity.device == artifact.identity.device
                    && identity.inode == artifact.identity.inode
                    && identity.stable_len <= artifact.identity.stable_len
                    && cursor.byte_offset <= identity.stable_len
            })
    });
    let mut cursor = if incremental {
        prior.cloned().unwrap_or_default()
    } else {
        CollectorCursor::default()
    };
    cursor.dialect = Some(locator.dialect);
    let start = cursor.byte_offset;
    let extent = artifact
        .identity
        .stable_len
        .checked_sub(start)
        .context("artifact cursor exceeds stable extent")?;
    if extent > MAX_SCAN_BYTES {
        bail!("provider artifact scan exceeds the configured byte limit");
    }
    artifact.file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(artifact.file.by_ref().take(extent));
    let mut offset = start;
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            bail!("provider artifact line exceeds the configured limit");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let value: Value =
            serde_json::from_slice(&line).context("provider artifact contains malformed JSONL")?;
        parse_line(locator, &value, &mut cursor)?;
        offset = offset
            .checked_add(u64::try_from(count)?)
            .context("provider artifact byte offset overflow")?;
    }
    cursor.byte_offset = offset;
    cursor.identity = Some(artifact.identity.clone());
    validate_after_read(&artifact.file, &artifact.identity)?;
    if locator.dialect == CollectorDialect::Codex && !cursor.session_bound {
        bail!("Codex rollout did not bind to the requested session");
    }
    let observation = PartialUsageObservation {
        input_tokens: Some(cursor.input_tokens),
        output_tokens: Some(cursor.output_tokens),
        context_tokens: cursor.context_tokens,
        context_window_percent: cursor.context_window_percent,
        context_observed: cursor.context_tokens.is_some()
            || cursor.context_window_percent.is_some(),
    };
    Ok(CollectionResult {
        observation,
        cursor,
        identity: artifact.identity,
    })
}

pub fn identity_matches(
    home: &Path,
    locator: &ArtifactLocator,
    expected: &ArtifactIdentity,
) -> bool {
    open_artifact(home, locator).is_ok_and(|artifact| {
        artifact.identity.device == expected.device
            && artifact.identity.inode == expected.inode
            && artifact.identity.stable_len >= expected.stable_len
    })
}

struct OpenArtifact {
    file: File,
    identity: ArtifactIdentity,
}

fn root(home: &Path, dialect: CollectorDialect) -> PathBuf {
    match dialect {
        CollectorDialect::Claude => home.join(".claude/projects"),
        CollectorDialect::Codex => home.join(".codex/sessions"),
        CollectorDialect::Qwen => home.join(".qwen/projects"),
    }
}

fn open_artifact(home: &Path, locator: &ArtifactLocator) -> Result<OpenArtifact> {
    let root = fs::canonicalize(root(home, locator.dialect))
        .context("provider artifact root is unavailable")?;
    let unresolved = if locator.transcript_path.is_absolute() {
        locator.transcript_path.clone()
    } else {
        root.join(&locator.transcript_path)
    };
    let canonical = fs::canonicalize(&unresolved).context("provider artifact is unavailable")?;
    if !canonical.starts_with(&root) {
        bail!("provider artifact escapes its allowed root");
    }
    let link_meta = fs::symlink_metadata(&unresolved)?;
    if link_meta.file_type().is_symlink() {
        bail!("provider artifact must not be a symlink");
    }
    if canonical.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        bail!("provider artifact must be JSONL");
    }
    if matches!(
        locator.dialect,
        CollectorDialect::Claude | CollectorDialect::Qwen
    ) && canonical.file_stem().and_then(|value| value.to_str())
        != Some(locator.provider_session_id.as_str())
    {
        bail!("provider artifact filename does not match its session");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("provider artifact is not a regular file");
    }
    let identity = ArtifactIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        stable_len: metadata.len(),
    };
    Ok(OpenArtifact { file, identity })
}

fn validate_after_read(file: &File, identity: &ArtifactIdentity) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.dev() != identity.device
        || metadata.ino() != identity.inode
        || metadata.len() < identity.stable_len
    {
        bail!("provider artifact changed identity during collection");
    }
    Ok(())
}

fn parse_line(
    locator: &ArtifactLocator,
    value: &Value,
    cursor: &mut CollectorCursor,
) -> Result<()> {
    match locator.dialect {
        CollectorDialect::Claude => parse_claude(locator, value, cursor),
        CollectorDialect::Codex => parse_codex(locator, value, cursor),
        CollectorDialect::Qwen => parse_qwen(locator, value, cursor),
    }
}

fn parse_claude(
    locator: &ArtifactLocator,
    value: &Value,
    cursor: &mut CollectorCursor,
) -> Result<()> {
    if let Some(session) = value
        .get("sessionId")
        .or_else(|| value.get("session_id"))
        .and_then(Value::as_str)
        && session != locator.provider_session_id
    {
        bail!("Claude transcript session identity mismatch");
    }
    let Some(usage) = value
        .get("message")
        .and_then(|message| message.get("usage"))
        .or_else(|| value.get("usage"))
    else {
        return Ok(());
    };
    let response_id = value
        .get("message")
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .or_else(|| {
            value
                .get("requestId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
        });
    let Some(response_id) = response_id else {
        return Ok(());
    };
    if cursor.response_ids.contains(response_id) {
        return Ok(());
    }
    let input = optional_u64(usage, "input_tokens")?.unwrap_or(0);
    let cache_read = optional_u64(usage, "cache_read_input_tokens")?.unwrap_or(0);
    let cache_create = optional_u64(usage, "cache_creation_input_tokens")?.unwrap_or(0);
    let output = optional_u64(usage, "output_tokens")?.unwrap_or(0);
    let current = input
        .checked_add(cache_read)
        .and_then(|value| value.checked_add(cache_create))
        .context("Claude input token overflow")?;
    cursor.input_tokens = cursor
        .input_tokens
        .checked_add(current)
        .context("Claude cumulative input token overflow")?;
    cursor.output_tokens = cursor
        .output_tokens
        .checked_add(output)
        .context("Claude cumulative output token overflow")?;
    cursor.context_tokens = Some(current);
    cursor.response_ids.insert(response_id.to_owned());
    Ok(())
}

fn parse_codex(
    locator: &ArtifactLocator,
    value: &Value,
    cursor: &mut CollectorCursor,
) -> Result<()> {
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        let payload = value
            .get("payload")
            .context("Codex session metadata missing payload")?;
        let matches = ["session_id", "id"].into_iter().any(|field| {
            payload.get(field).and_then(Value::as_str) == Some(locator.provider_session_id.as_str())
        });
        if !matches {
            bail!("Codex rollout session identity mismatch");
        }
        cursor.session_bound = true;
    }
    let Some(payload) = value.get("payload") else {
        return Ok(());
    };
    if value.get("type").and_then(Value::as_str) != Some("event_msg")
        || payload.get("type").and_then(Value::as_str) != Some("token_count")
    {
        return Ok(());
    }
    let Some(info) = payload.get("info").filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let Some(total) = info.get("total_token_usage") else {
        return Ok(());
    };
    let Some(last) = info.get("last_token_usage") else {
        return Ok(());
    };
    let (Some(input), Some(output), Some(context)) = (
        optional_u64(total, "input_tokens")?,
        optional_u64(total, "output_tokens")?,
        optional_u64(last, "total_tokens")?,
    ) else {
        return Ok(());
    };
    cursor.input_tokens = input;
    cursor.output_tokens = output;
    cursor.context_tokens = Some(context);
    cursor.context_window_percent = match optional_u64(info, "model_context_window")? {
        Some(window) if window > 0 => Some(percent(context, window)?),
        _ => None,
    };
    Ok(())
}

fn parse_qwen(
    locator: &ArtifactLocator,
    value: &Value,
    cursor: &mut CollectorCursor,
) -> Result<()> {
    if let Some(session) = value.get("sessionId").and_then(Value::as_str)
        && session != locator.provider_session_id
    {
        bail!("Qwen transcript session identity mismatch");
    }
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Ok(());
    }
    let Some(usage) = value.get("usageMetadata") else {
        return Ok(());
    };
    let (Some(input), Some(output)) = (
        optional_u64(usage, "promptTokenCount")?,
        optional_u64(usage, "candidatesTokenCount")?,
    ) else {
        return Ok(());
    };
    cursor.input_tokens = cursor
        .input_tokens
        .checked_add(input)
        .context("Qwen cumulative input token overflow")?;
    cursor.output_tokens = cursor
        .output_tokens
        .checked_add(output)
        .context("Qwen cumulative output token overflow")?;
    cursor.context_tokens = Some(input);
    cursor.context_window_percent = match optional_u64(value, "contextWindowSize")? {
        Some(window) if window > 0 => Some(percent(input, window)?),
        _ => None,
    };
    Ok(())
}

fn optional_u64(object: &Value, field: &str) -> Result<Option<u64>> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    value
        .as_u64()
        .map(Some)
        .context("provider usage value is not a non-negative integer")
}

fn percent(value: u64, window: u64) -> Result<u8> {
    let value = u128::from(value);
    let window = u128::from(window);
    let rounded = value
        .checked_mul(100)
        .and_then(|value| value.checked_add(window / 2))
        .context("context percentage overflow")?
        / window;
    Ok(u8::try_from(rounded.min(100))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn fixture(
        dialect: CollectorDialect,
        session: &str,
        lines: &[&str],
    ) -> (tempfile::TempDir, ArtifactLocator) {
        let temp = tempfile::tempdir().unwrap();
        let root = match dialect {
            CollectorDialect::Claude => temp.path().join(".claude/projects/p"),
            CollectorDialect::Codex => temp.path().join(".codex/sessions/2026/08/30"),
            CollectorDialect::Qwen => temp.path().join(".qwen/projects/p/chats"),
        };
        fs::create_dir_all(&root).unwrap();
        let name = match dialect {
            CollectorDialect::Codex => format!("rollout-now-{session}.jsonl"),
            _ => format!("{session}.jsonl"),
        };
        let path = root.join(name);
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        (
            temp,
            ArtifactLocator {
                dialect,
                provider_session_id: session.into(),
                transcript_path: path,
            },
        )
    }

    #[test]
    fn claude_deduplicates_and_counts_cache() {
        let sid = "s1";
        let row = r#"{"sessionId":"s1","type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3,"output_tokens":4}}}"#;
        let (temp, locator) = fixture(
            CollectorDialect::Claude,
            sid,
            &[
                row,
                row,
                r#"{"sessionId":"s1","message":{"usage":{"input_tokens":999}}}"#,
            ],
        );
        let result = collect(temp.path(), &locator, None).unwrap();
        assert_eq!(result.observation.input_tokens, Some(33));
        assert_eq!(result.observation.output_tokens, Some(4));
        assert_eq!(result.observation.context_tokens, Some(33));
    }

    #[test]
    fn codex_uses_latest_snapshot() {
        let sid = "s2";
        let meta = r#"{"type":"session_meta","payload":{"session_id":"s2"}}"#;
        let first = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":2},"last_token_usage":{"total_tokens":5},"model_context_window":100}}}"#;
        let last = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":40,"output_tokens":8},"last_token_usage":{"total_tokens":26},"model_context_window":100}}}"#;
        let (temp, locator) = fixture(CollectorDialect::Codex, sid, &[meta, first, last]);
        let result = collect(temp.path(), &locator, None).unwrap();
        assert_eq!(result.observation.input_tokens, Some(40));
        assert_eq!(result.observation.output_tokens, Some(8));
        assert_eq!(result.observation.context_window_percent, Some(26));
    }

    #[test]
    fn qwen_ignores_telemetry_and_handles_compaction() {
        let sid = "s3";
        let a = r#"{"sessionId":"s3","type":"assistant","usageMetadata":{"promptTokenCount":80,"candidatesTokenCount":5,"totalTokenCount":999},"contextWindowSize":100}"#;
        let telemetry = r#"{"sessionId":"s3","type":"system","subtype":"ui_telemetry","usageMetadata":{"promptTokenCount":800}}"#;
        let b = r#"{"sessionId":"s3","type":"assistant","usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":7},"contextWindowSize":100}"#;
        let (temp, locator) = fixture(CollectorDialect::Qwen, sid, &[a, telemetry, b]);
        let result = collect(temp.path(), &locator, None).unwrap();
        assert_eq!(result.observation.input_tokens, Some(100));
        assert_eq!(result.observation.output_tokens, Some(12));
        assert_eq!(result.observation.context_tokens, Some(20));
    }

    #[test]
    fn incremental_append_and_partial_line_are_exact() {
        let sid = "s4";
        let row = r#"{"sessionId":"s4","type":"assistant","usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1},"contextWindowSize":10}"#;
        let (temp, locator) = fixture(CollectorDialect::Qwen, sid, &[row]);
        let first = collect(temp.path(), &locator, None).unwrap();
        use std::io::Write;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&locator.transcript_path)
            .unwrap();
        write!(file, "{row}").unwrap();
        let partial = collect(temp.path(), &locator, Some(&first.cursor)).unwrap();
        assert_eq!(partial.observation.input_tokens, Some(2));
        writeln!(file).unwrap();
        let complete = collect(temp.path(), &locator, Some(&partial.cursor)).unwrap();
        assert_eq!(complete.observation.input_tokens, Some(4));
    }

    #[test]
    fn rejects_traversal_symlink_malformed_and_oversized_line() {
        let (temp, mut locator) = fixture(CollectorDialect::Claude, "s5", &["{}"]);
        let outside = temp.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();
        locator.transcript_path = outside;
        assert!(collect(temp.path(), &locator, None).is_err());

        let root = temp.path().join(".claude/projects/p");
        let link = root.join("s5.jsonl");
        fs::remove_file(&link).unwrap();
        symlink(temp.path().join("outside.jsonl"), &link).unwrap();
        locator.transcript_path = link;
        assert!(collect(temp.path(), &locator, None).is_err());

        let (temp, locator) = fixture(CollectorDialect::Claude, "s6", &["not-json"]);
        assert!(collect(temp.path(), &locator, None).is_err());
        fs::write(&locator.transcript_path, vec![b'x'; MAX_LINE_BYTES + 1]).unwrap();
        assert!(collect(temp.path(), &locator, None).is_err());
    }
}
