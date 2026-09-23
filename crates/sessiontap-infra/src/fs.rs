use std::{
    fs, io,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

/// Replaces `path` with `bytes` via a synced temporary file in the same
/// directory, so readers observe either the old or the new content, never a
/// partial write. The resulting file has `mode`.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Creates `path` as a directory owned by the effective user with mode 0700.
/// A symlink, a non-directory, or a directory owned by another user is
/// rejected with `PermissionDenied`; existing looser modes are tightened.
pub fn prepare_private_dir(path: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private path must be a real directory",
        ));
    }
    fs::create_dir_all(path)?;
    ensure_owned_private(path, nix::unistd::Uid::effective().as_raw())
}

fn ensure_owned_private(path: &Path, owner: u32) -> io::Result<()> {
    if fs::symlink_metadata(path)?.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory is owned by another user",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Reads an optional configuration file. A missing file yields `None`; a
/// symlink is followed, but a dangling symlink is reported as `NotFound`
/// rather than silently treated as absent.
pub fn read_private_config(path: &Path) -> io::Result<Option<String>> {
    let symlink =
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
    if !path.exists() {
        if symlink {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "configuration symlink target not found",
            ));
        }
        return Ok(None);
    }
    fs::read_to_string(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn mode(path: &Path) -> u32 {
        path.metadata().unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn atomic_write_replaces_content_with_mode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.json");
        fs::write(&path, b"old").unwrap();
        let reader = fs::File::open(&path).unwrap();
        atomic_write(&path, b"new", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600);
        // An open handle still sees the old inode: the replace is a rename,
        // not an in-place truncate.
        assert_eq!(std::io::read_to_string(reader).unwrap(), "old");
        let leftovers = fs::read_dir(temp.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temporary file left behind");
    }

    #[test]
    fn private_dir_is_created_with_mode_0700() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a/b");
        prepare_private_dir(&path).unwrap();
        assert_eq!(mode(&path), 0o700);
    }

    #[test]
    fn private_dir_tightens_existing_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        prepare_private_dir(&path).unwrap();
        assert_eq!(mode(&path), 0o700);
    }

    #[test]
    fn private_dir_rejects_symlink_and_file() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            prepare_private_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let file = temp.path().join("file");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            prepare_private_dir(&file).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn private_dir_rejects_foreign_owner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("foreign");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let other = nix::unistd::Uid::effective().as_raw().wrapping_add(1);
        assert_eq!(
            ensure_owned_private(&path, other).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(mode(&path), 0o755, "foreign directory must not be modified");
    }

    #[test]
    fn config_read_handles_missing_symlink_and_dangling() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_private_config(&temp.path().join("missing")).unwrap(),
            None
        );
        let target = temp.path().join("target.toml");
        fs::write(&target, "version = 1\n").unwrap();
        let link = temp.path().join("config.toml");
        symlink(&target, &link).unwrap();
        assert_eq!(
            read_private_config(&link).unwrap().as_deref(),
            Some("version = 1\n")
        );
        let dangling = temp.path().join("dangling.toml");
        symlink(temp.path().join("nope"), &dangling).unwrap();
        let error = read_private_config(&dangling).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("symlink target not found"));
    }
}
