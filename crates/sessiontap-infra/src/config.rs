use crate::fs::read_private_config;
use sessiontap_core::config::Config;
use std::{io, path::Path};

/// Loads the daemon/CLI configuration. A missing file yields defaults; a
/// dangling symlink or an invalid file is an error.
pub fn load_config(path: &Path) -> io::Result<Config> {
    read_private_config(path)?.map_or_else(|| Ok(Config::default()), |raw| Config::from_toml(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn config_load_follows_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.toml");
        fs::write(&target, "version=1\nretention_days=99\n").unwrap();
        let link = temp.path().join("config.toml");
        symlink(&target, &link).unwrap();
        assert_eq!(load_config(&link).unwrap().retention_days, 99);
    }

    #[test]
    fn config_load_rejects_dangling_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("config.toml");
        symlink(temp.path().join("missing.toml"), &link).unwrap();
        let error = load_config(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("symlink target not found"));
    }

    #[test]
    fn missing_config_uses_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let config = load_config(&temp.path().join("config.toml")).unwrap();
        assert_eq!(config.version, 1);
    }
}
