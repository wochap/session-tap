use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::PermissionsExt, net},
    path::Path,
};
use tokio::net::{UnixDatagram, UnixListener};

/// Takes a non-blocking exclusive lock on `path`, creating it if needed. The
/// returned file holds the lock until dropped; contention fails with
/// `WouldBlock`.
pub fn acquire_exclusive_lock(path: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    lock.try_lock_exclusive().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("another process holds {}: {error}", path.display()),
        )
    })?;
    Ok(lock)
}

/// Wraps a bind failure from [`bind_private_unix_socket`] or
/// [`bind_private_unix_datagram`]: lock contention and a live socket mean
/// another `name` instance is running; anything else is reported as a bind
/// failure for `socket`.
pub fn bind_error(name: &str, socket: &Path, error: io::Error) -> anyhow::Error {
    let already_running = matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::AddrInUse
    );
    let error = anyhow::Error::new(error);
    if already_running {
        error.context(format!("{name} is already running"))
    } else {
        error.context(format!("{name}: cannot bind {}", socket.display()))
    }
}

/// Removes a leftover socket file at `path` unless a live peer still accepts
/// on it. Both stream and datagram peers count as live.
fn clear_stale_socket(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    let live = net::UnixStream::connect(path).is_ok()
        || net::UnixDatagram::unbound().is_ok_and(|probe| probe.connect(path).is_ok());
    if live {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is already accepting connections", path.display()),
        ));
    }
    fs::remove_file(path)
}

/// Acquires the exclusive lock at `lock`, replaces a stale socket, and binds
/// a stream listener at `socket` with mode 0600. A live listener is never
/// displaced. Must be called within a Tokio runtime.
pub fn bind_private_unix_socket(socket: &Path, lock: &Path) -> io::Result<(UnixListener, File)> {
    let lock = acquire_exclusive_lock(lock)?;
    clear_stale_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok((listener, lock))
}

/// Datagram counterpart of [`bind_private_unix_socket`], with the same lock,
/// stale-socket probe, and permissions.
pub fn bind_private_unix_datagram(socket: &Path, lock: &Path) -> io::Result<(UnixDatagram, File)> {
    let lock = acquire_exclusive_lock(lock)?;
    clear_stale_socket(socket)?;
    let datagram = UnixDatagram::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok((datagram, lock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_holder_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("x.lock");
        let first = acquire_exclusive_lock(&path).unwrap();
        assert!(acquire_exclusive_lock(&path).is_err());
        drop(first);
        acquire_exclusive_lock(&path).unwrap();
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_with_private_mode() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("x.sock");
        drop(net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists(), "stale socket file remains after drop");
        let (listener, _lock) =
            bind_private_unix_socket(&socket, &temp.path().join("x.lock")).unwrap();
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let client = tokio::net::UnixStream::connect(&socket).await.unwrap();
        listener.accept().await.unwrap();
        drop(client);
    }

    #[tokio::test]
    async fn live_socket_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("x.sock");
        let live = net::UnixListener::bind(&socket).unwrap();
        let error = bind_private_unix_socket(&socket, &temp.path().join("x.lock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        net::UnixStream::connect(&socket).expect("live listener still reachable");
        drop(live);
    }

    #[tokio::test]
    async fn live_datagram_socket_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("x.sock");
        let live = net::UnixDatagram::bind(&socket).unwrap();
        let error = bind_private_unix_datagram(&socket, &temp.path().join("x.lock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        drop(live);
        let (_datagram, _lock) =
            bind_private_unix_datagram(&socket, &temp.path().join("x.lock")).unwrap();
    }

    #[tokio::test]
    async fn bind_errors_distinguish_running_instance() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("x.lock");
        let socket = temp.path().join("x.sock");
        let _held = acquire_exclusive_lock(&lock).unwrap();
        let error = bind_private_unix_socket(&socket, &lock).unwrap_err();
        assert!(
            bind_error("svc", &socket, error)
                .to_string()
                .contains("already running")
        );
        let missing = temp.path().join("missing/x.sock");
        let error = bind_private_unix_socket(&missing, &temp.path().join("y.lock")).unwrap_err();
        let message = bind_error("svc", &missing, error).to_string();
        assert!(message.contains("cannot bind"), "{message}");
    }

    #[tokio::test]
    async fn held_lock_prevents_bind() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("x.lock");
        let _held = acquire_exclusive_lock(&lock).unwrap();
        assert!(bind_private_unix_socket(&temp.path().join("x.sock"), &lock).is_err());
        assert!(!temp.path().join("x.sock").exists());
    }
}
