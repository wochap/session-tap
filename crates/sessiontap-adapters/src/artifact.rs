//! Provider-neutral building blocks for session collectors.
//!
//! A provider owns its collector end to end: which root it trusts, how the
//! locator is validated, how records are read and decoded, and what its cursor
//! means. The helpers here are optional plain functions for the checks that
//! every file-backed collector repeats; none of them reads or decodes records.

use crate::{CollectSessionDataRequest, OpaqueCursor, SessionEnrichment};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Reads a provider's session artifacts for one collection request.
pub trait SessionCollector: Send + Sync + 'static {
    /// Whether this collector reads artifacts at all. The driver reports
    /// `CollectionOutcome::Unsupported` without calling `collect` when false.
    const COLLECTS: bool = true;

    /// Blocking. Runs on the driver's `spawn_blocking` thread.
    fn collect(&self, request: &CollectSessionDataRequest) -> Result<Collected, CollectError>;
}

pub enum Collected {
    Complete {
        enrichment: SessionEnrichment,
        cursor: OpaqueCursor,
    },
    Unchanged {
        cursor: OpaqueCursor,
    },
}

#[derive(Debug)]
pub enum CollectError {
    Cancelled,
    Failed(anyhow::Error),
}

impl fmt::Display for CollectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("collection cancelled"),
            Self::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for CollectError {}

/// Recovers a typed cancellation that travelled through `anyhow` via `?`, so a
/// collector can run its loop in `anyhow::Result` and still report
/// `Cancelled` distinctly from failure.
impl From<anyhow::Error> for CollectError {
    fn from(error: anyhow::Error) -> Self {
        if matches!(error.downcast_ref::<Self>(), Some(Self::Cancelled)) {
            Self::Cancelled
        } else {
            Self::Failed(error)
        }
    }
}

/// Collector for providers whose usage arrives only through managed hooks.
#[derive(Debug, Clone, Copy)]
pub struct NoCollector;

impl SessionCollector for NoCollector {
    const COLLECTS: bool = false;

    fn collect(&self, _request: &CollectSessionDataRequest) -> Result<Collected, CollectError> {
        Err(CollectError::Failed(anyhow::anyhow!(
            "provider has no artifact collector"
        )))
    }
}

/// Resolves `locator` beneath `root` and checks it names a regular artifact:
/// not a symlink, inside the canonical root, with the expected extension and,
/// when given, the expected file stem.
pub fn validate_under_root(
    root: &Path,
    locator: &Path,
    expected_stem: Option<&str>,
    extension: &str,
) -> Result<PathBuf> {
    let root = fs::canonicalize(root).context("artifact root is unavailable")?;
    let unresolved = if locator.is_absolute() {
        locator.to_path_buf()
    } else {
        root.join(locator)
    };
    if fs::symlink_metadata(&unresolved)?.file_type().is_symlink() {
        bail!("artifact must not be a symlink");
    }
    let canonical = fs::canonicalize(unresolved)?;
    if !canonical.starts_with(&root) {
        bail!("artifact escapes allowed root");
    }
    if canonical.extension().and_then(|value| value.to_str()) != Some(extension) {
        bail!("artifact has an unexpected extension");
    }
    if let Some(stem) = expected_stem
        && canonical.file_stem().and_then(|value| value.to_str()) != Some(stem)
    {
        bail!("artifact identity mismatch");
    }
    Ok(canonical)
}

/// Opens `path` without following a final symlink and rejects anything that
/// is not a regular file of at most `max_bytes`.
pub fn open_bounded_nofollow(path: &Path, max_bytes: u64) -> Result<(File, Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("artifact is not a bounded regular file");
    }
    Ok((file, metadata))
}

/// File identity and the length observed when the scan started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactCursor {
    pub device: u64,
    pub inode: u64,
    pub stable_len: u64,
}

impl From<&Metadata> for ArtifactCursor {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            stable_len: metadata.len(),
        }
    }
}

impl ArtifactCursor {
    /// Checks that `file` still has this identity and has not shrunk, so the
    /// scan read a consistent prefix.
    pub fn ensure_stable(&self, file: &File) -> Result<()> {
        let after = file.metadata()?;
        if after.dev() != self.device || after.ino() != self.inode || after.len() < self.stable_len
        {
            bail!("artifact changed identity during collection");
        }
        Ok(())
    }
}

/// True when `prior` is an [`ArtifactCursor`] equal to `current`. A cursor of
/// any other type never matches.
#[must_use]
pub fn cursor_unchanged(prior: Option<&OpaqueCursor>, current: &ArtifactCursor) -> bool {
    prior
        .and_then(OpaqueCursor::downcast_ref::<ArtifactCursor>)
        .is_some_and(|prior| prior == current)
}

pub fn check_cancelled(request: &CollectSessionDataRequest) -> Result<(), CollectError> {
    if request.cancellation.is_cancelled() {
        return Err(CollectError::Cancelled);
    }
    Ok(())
}

/// Reads an optional unsigned integer field; present but non-integer values
/// are errors rather than silently absent.
pub fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>> {
    value
        .get(field)
        .map(|value| {
            value
                .as_u64()
                .with_context(|| format!("invalid numeric field {field}"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CollectionCancellation, ProviderSessionKey};
    use serde_json::json;
    use sessiontap_core::ProviderId;
    use std::os::unix::fs::symlink;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        (temp, root)
    }

    #[test]
    fn locator_validation_rejects_unsafe_or_foreign_artifacts() {
        let (temp, root) = root();
        let good = root.join("s1.jsonl");
        fs::write(&good, "{}\n").unwrap();
        assert_eq!(
            validate_under_root(&root, &good, Some("s1"), "jsonl").unwrap(),
            fs::canonicalize(&good).unwrap()
        );
        assert!(validate_under_root(&root, Path::new("s1.jsonl"), Some("s1"), "jsonl").is_ok());
        assert!(validate_under_root(&root, &good, None, "jsonl").is_ok());

        let link = root.join("link.jsonl");
        symlink(&good, &link).unwrap();
        assert!(
            validate_under_root(&root, &link, None, "jsonl")
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );

        let outside = temp.path().join("s1.jsonl");
        fs::write(&outside, "{}\n").unwrap();
        assert!(
            validate_under_root(&root, &outside, Some("s1"), "jsonl")
                .unwrap_err()
                .to_string()
                .contains("escapes")
        );
        assert!(validate_under_root(&root, Path::new("../s1.jsonl"), Some("s1"), "jsonl").is_err());

        let wrong_extension = root.join("s1.json");
        fs::write(&wrong_extension, "{}\n").unwrap();
        assert!(
            validate_under_root(&root, &wrong_extension, Some("s1"), "jsonl")
                .unwrap_err()
                .to_string()
                .contains("extension")
        );

        assert!(
            validate_under_root(&root, &good, Some("s2"), "jsonl")
                .unwrap_err()
                .to_string()
                .contains("identity")
        );
        assert!(validate_under_root(&temp.path().join("missing"), &good, None, "jsonl").is_err());
    }

    #[test]
    fn bounded_open_rejects_oversized_non_regular_and_symlinked_files() {
        let (_temp, root) = root();
        let file = root.join("a.jsonl");
        fs::write(&file, "0123456789").unwrap();
        let (_, metadata) = open_bounded_nofollow(&file, 10).unwrap();
        assert_eq!(metadata.len(), 10);
        assert!(open_bounded_nofollow(&file, 9).is_err());
        assert!(open_bounded_nofollow(&root, 1 << 20).is_err());
        let link = root.join("link.jsonl");
        symlink(&file, &link).unwrap();
        assert!(open_bounded_nofollow(&link, 1 << 20).is_err());
    }

    #[test]
    fn cursor_matches_only_same_identity_and_length() {
        let (_temp, root) = root();
        let path = root.join("a.jsonl");
        fs::write(&path, "{}\n").unwrap();
        let current = ArtifactCursor::from(&fs::metadata(&path).unwrap());
        assert!(cursor_unchanged(
            Some(&OpaqueCursor::new(current)),
            &current
        ));
        assert!(!cursor_unchanged(None, &current));

        let grown = ArtifactCursor {
            stable_len: current.stable_len + 1,
            ..current
        };
        assert!(!cursor_unchanged(Some(&OpaqueCursor::new(grown)), &current));
        let replaced = ArtifactCursor {
            inode: current.inode + 1,
            ..current
        };
        assert!(!cursor_unchanged(
            Some(&OpaqueCursor::new(replaced)),
            &current
        ));
        assert!(!cursor_unchanged(
            Some(&OpaqueCursor::new((current.device, current.inode))),
            &current
        ));
    }

    #[test]
    fn stability_check_detects_replacement_and_truncation() {
        let (_temp, root) = root();
        let path = root.join("a.jsonl");
        fs::write(&path, "{}\n{}\n").unwrap();
        let (file, metadata) = open_bounded_nofollow(&path, 1 << 20).unwrap();
        let cursor = ArtifactCursor::from(&metadata);
        cursor.ensure_stable(&file).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .set_len(1)
            .unwrap();
        assert!(cursor.ensure_stable(&file).is_err());
    }

    #[test]
    fn cancellation_is_typed_and_survives_anyhow() {
        let request = CollectSessionDataRequest {
            home: PathBuf::from("/nonexistent"),
            key: ProviderSessionKey {
                configured_provider: "claude".into(),
                adapter_identity: ProviderId::Claude,
                provider_session_id: "s".into(),
            },
            locator: PathBuf::from("/nonexistent"),
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        };
        assert!(check_cancelled(&request).is_ok());
        request.cancellation.cancel();
        assert!(matches!(
            check_cancelled(&request),
            Err(CollectError::Cancelled)
        ));
        let through_anyhow = || -> Result<()> {
            check_cancelled(&request).context("while scanning")?;
            Ok(())
        };
        assert!(matches!(
            CollectError::from(through_anyhow().unwrap_err()),
            CollectError::Cancelled
        ));
        assert!(matches!(
            CollectError::from(anyhow::anyhow!("collection cancelled")),
            CollectError::Failed(_)
        ));
    }

    #[test]
    fn numeric_fields_are_optional_but_strict() {
        let value = json!({"n": 3, "bad": "3", "neg": -1});
        assert_eq!(optional_u64(&value, "n").unwrap(), Some(3));
        assert_eq!(optional_u64(&value, "missing").unwrap(), None);
        assert!(optional_u64(&value, "bad").is_err());
        assert!(optional_u64(&value, "neg").is_err());
    }
}
