use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl HubPaths {
    pub fn discover() -> io::Result<Self> {
        Self::from_env(|key| env::var_os(key).map(PathBuf::from))
    }

    pub fn from_env(mut get: impl FnMut(&str) -> Option<PathBuf>) -> io::Result<Self> {
        let home = get("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let config = get("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let state = get("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let runtime = get("XDG_RUNTIME_DIR")
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| state.join("runtime"));
        Ok(Self {
            config_dir: config.join("sessiontap-hub"),
            state_dir: state.join("sessiontap-hub"),
            runtime_dir: runtime.join("sessiontap-hub"),
        })
    }

    pub fn prepare_private(path: &Path) -> io::Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "private path must be a real directory",
                ));
            }
        }
        fs::create_dir_all(path)?;
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        if fs::symlink_metadata(path)?.uid() != nix::unistd::Uid::effective().as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private directory is owned by another user",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.yaml")
    }
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.state_dir.join("hub.sqlite3")
    }
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("sessiontap-hub.sock")
    }
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.runtime_dir.join("sessiontap-hub.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hub_paths_follow_xdg_conventions() {
        let paths = HubPaths::from_env(|k| match k {
            "HOME" => Some("/home/u".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/u/.config/sessiontap-hub/config.yaml")
        );
        assert_eq!(
            paths.database(),
            PathBuf::from("/home/u/.local/state/sessiontap-hub/hub.sqlite3")
        );
        assert_eq!(
            paths.socket(),
            PathBuf::from("/home/u/.local/state/runtime/sessiontap-hub/sessiontap-hub.sock")
        );
    }
}
