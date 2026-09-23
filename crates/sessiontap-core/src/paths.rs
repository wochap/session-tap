use std::{env, io, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> io::Result<Self> {
        Self::from_env(|key| env::var_os(key).map(PathBuf::from))
    }

    pub fn from_env(mut get: impl FnMut(&str) -> Option<PathBuf>) -> io::Result<Self> {
        let home = get("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let config = get("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let state = get("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let data = get("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let runtime = get("XDG_RUNTIME_DIR")
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| state.join("runtime"));
        Ok(Self {
            config_dir: config.join("sessiontap"),
            state_dir: state.join("sessiontap"),
            data_dir: data.join("sessiontap"),
            runtime_dir: runtime.join("sessiontap"),
        })
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.state_dir.join("sessiontap.sqlite3")
    }
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("sessiontap.sock")
    }
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.runtime_dir.join("sessiontap.lock")
    }
    #[must_use]
    pub fn hook_inspection_socket(&self) -> PathBuf {
        self.runtime_dir.join("hook-inspection.sock")
    }
    #[must_use]
    pub fn hook_inspection_lock(&self) -> PathBuf {
        self.runtime_dir.join("hook-inspection.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xdg_paths_and_fallbacks() {
        let paths = AppPaths::from_env(|k| match k {
            "HOME" => Some("/home/u".into()),
            "XDG_RUNTIME_DIR" => Some("relative".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/u/.config/sessiontap/config.toml")
        );
        assert_eq!(
            paths.socket(),
            PathBuf::from("/home/u/.local/state/runtime/sessiontap/sessiontap.sock")
        );
        assert_eq!(
            paths.hook_inspection_socket(),
            PathBuf::from("/home/u/.local/state/runtime/sessiontap/hook-inspection.sock")
        );
    }
}
