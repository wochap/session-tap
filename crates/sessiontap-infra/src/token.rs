use std::{fs, io, os::unix::fs::PermissionsExt, path::Path};

/// Reads a bearer token from a private file. The file must not be a symlink
/// and must grant no group or other permissions; surrounding whitespace is
/// trimmed.
pub fn read_private_token(path: &Path) -> io::Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "token file must be private and not a symlink",
        ));
    }
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

/// Compares two byte strings without an early exit on the first difference.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_token(dir: &Path, mode: u32) -> std::path::PathBuf {
        let path = dir.join("token");
        fs::write(&path, " secret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn private_token_is_trimmed() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_token(temp.path(), 0o600);
        assert_eq!(read_private_token(&path).unwrap(), "secret");
    }

    #[test]
    fn group_or_other_mode_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        for mode in [0o640, 0o604, 0o660] {
            let path = write_token(temp.path(), mode);
            assert_eq!(
                read_private_token(&path).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn symlinked_token_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let target = write_token(temp.path(), 0o600);
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            read_private_token(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn constant_time_eq_compares_content_and_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
