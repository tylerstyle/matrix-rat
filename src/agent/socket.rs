//! The agent socket as a file: where it is, who may reach it, and its removal.
//!
//! It lives in the app's runtime folder, which must be a folder of this user's that
//! nobody else can enter; the socket itself is owner-only as well, and every
//! connection is asked who is at the other end (`SO_PEERCRED`): anyone but this user
//! is dropped. The path is never logged.

use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

/// A unix socket's address holds 108 bytes, the closing zero included; some room is
/// kept.
pub const MAX_SOCKET_PATH: usize = 100;

/// Why the socket could not be made.
#[derive(Debug)]
pub enum BindError {
    /// The runtime folder is not a private folder of this user's.
    NotPrivate,
    /// The path is too long to be a socket's address.
    TooLong,
    /// Something that is not a socket is in the way; it is not ours to remove.
    InTheWay,
    /// Another program answers on the socket.
    InUse,
    Io(io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPrivate => f.write_str(
                "the runtime folder is not a private folder of this user (it must be a \
                 real folder, owned by you)",
            ),
            Self::TooLong => f.write_str("the path of the socket is too long for a socket"),
            Self::InTheWay => {
                f.write_str("something that is not a socket is where the socket should be")
            }
            Self::InUse => f.write_str("another program is already serving on the socket"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// The socket's file, removed when this is dropped: but only if it is still the file
/// that was made here, never one that another program has put in its place.
pub struct SocketFile {
    path: PathBuf,
    /// The device and the file number of the socket as it was made.
    made: (u64, u64),
}

/// Hand-written: the path names the user's runtime folder and the profile.
impl std::fmt::Debug for SocketFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SocketFile")
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let same = std::fs::symlink_metadata(&self.path).is_ok_and(|meta| {
            meta.file_type().is_socket() && (meta.dev(), meta.ino()) == self.made
        });
        if same {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Starts listening on `path`, in a folder that is checked (and, if missing, made) to
/// be this user's alone. A socket that an earlier run left behind is removed, after
/// making sure that nobody answers on it. Must be called inside the tokio runtime.
pub fn bind(path: &Path) -> Result<(UnixListener, SocketFile), BindError> {
    if !path.is_absolute() {
        return Err(BindError::NotPrivate);
    }
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(BindError::TooLong);
    }
    let folder = path.parent().ok_or(BindError::NotPrivate)?;
    crate::audio::private_dir(folder).map_err(|err| match err.kind() {
        io::ErrorKind::Other => BindError::NotPrivate,
        _ => BindError::Io(err),
    })?;
    remove_stale(path)?;
    let listener = UnixListener::bind(path).map_err(BindError::Io)?;
    // Before anyone is accepted. (Until here the folder alone kept others out.)
    let protect = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::symlink_metadata(path));
    match protect {
        Ok(meta) => Ok((
            listener,
            SocketFile {
                path: path.to_owned(),
                made: (meta.dev(), meta.ino()),
            },
        )),
        Err(err) => {
            drop(listener);
            let _ = std::fs::remove_file(path);
            Err(BindError::Io(err))
        }
    }
}

/// Removes what an earlier run left at `path`: a socket on which nobody answers.
/// Anything else that is there stays, and is an error.
fn remove_stale(path: &Path) -> Result<(), BindError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(BindError::Io(err)),
    };
    if !meta.file_type().is_socket() {
        return Err(BindError::InTheWay);
    }
    // Answered at once on a unix socket, whichever way.
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(BindError::InUse),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(BindError::Io(err)),
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BindError::Io(err)),
    }
}

/// This program's user.
pub fn own_uid() -> u32 {
    // SAFETY: a plain system call without arguments.
    unsafe { libc::geteuid() }
}

/// Whether a connection from `peer` is let in by a program running as `own`: the same
/// user, and nobody else (not even root, who needs no socket to read everything).
pub fn same_user(peer: u32, own: u32) -> bool {
    peer == own
}

/// Whether the other end of `stream` is a program of this user, as the kernel says.
/// A stream that cannot say is not let in.
pub fn peer_is_us(stream: &UnixStream) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|peer| same_user(peer.uid(), own_uid()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testing::Dir;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[tokio::test]
    async fn the_socket_is_private_in_a_private_folder_and_gone_afterwards() {
        let root = Dir::new("bind");
        let folder = root.path().join("run");
        let path = folder.join("bot.sock");
        let (listener, file) = bind(&path).unwrap();
        // Folder and socket: the owner's only.
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert_eq!(format!("{file:?}"), "SocketFile");
        // We can reach it, and are who we are.
        let client = UnixStream::connect(&path).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        assert!(peer_is_us(&accepted));
        assert!(peer_is_us(&client));
        // While it is served, a second `serve` is refused and the socket stays.
        assert!(matches!(bind(&path), Err(BindError::InUse)));
        assert!(path.exists());
        drop(listener);
        drop(file);
        assert!(!path.exists(), "the socket is removed on the way out");
        // A folder that is open to others is closed before it is used.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_listener, _file) = bind(&path).unwrap();
        assert_eq!(mode(&folder), 0o700);
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_and_anything_else_is_left_alone() {
        let root = Dir::new("stale");
        let path = root.path().join("bot.sock");
        // Left by a run that was killed: a socket nobody listens on.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (listener, file) = bind(&path).unwrap();
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(listener);
        drop(file);
        assert!(!path.exists());

        // A file, a folder and a link are not ours to remove.
        std::fs::write(&path, "important").unwrap();
        assert!(matches!(bind(&path), Err(BindError::InTheWay)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "important");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(bind(&path), Err(BindError::InTheWay)));
        std::fs::remove_dir(&path).unwrap();
        let elsewhere = root.path().join("elsewhere.sock");
        drop(std::os::unix::net::UnixListener::bind(&elsewhere).unwrap());
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        assert!(matches!(bind(&path), Err(BindError::InTheWay)));
        assert!(elsewhere.exists());
    }

    #[tokio::test]
    async fn the_guard_removes_only_the_socket_it_made() {
        let root = Dir::new("guard");
        let path = root.path().join("bot.sock");
        let (listener, file) = bind(&path).unwrap();
        // Someone put another file in its place meanwhile: it stays.
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "not ours").unwrap();
        drop(file);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not ours");
        // So does another socket.
        std::fs::remove_file(&path).unwrap();
        let (_listener, file) = bind(&path).unwrap();
        let other = root.path().join("other.sock");
        let _keep = std::os::unix::net::UnixListener::bind(&other).unwrap();
        std::fs::rename(&other, &path).unwrap();
        drop(file);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_folder_that_is_not_ours_or_a_path_too_long_is_refused() {
        let root = Dir::new("refuse");
        // A link to a folder, even to one of ours, is not used.
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            bind(&link.join("bot.sock")),
            Err(BindError::NotPrivate)
        ));
        assert!(!real.join("bot.sock").exists());
        // A file where the folder should be.
        let file = root.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(
            bind(&file.join("bot.sock")),
            Err(BindError::NotPrivate | BindError::Io(_))
        ));
        // Longer than a socket's address can be.
        let long = root
            .path()
            .join(format!("{}.sock", "p".repeat(MAX_SOCKET_PATH)));
        assert!(matches!(bind(&long), Err(BindError::TooLong)));
        assert!(!long.exists());
        // A relative path is in nobody's folder.
        assert!(matches!(
            bind(Path::new("bot.sock")),
            Err(BindError::NotPrivate)
        ));
    }

    #[test]
    fn only_the_same_user_is_let_in() {
        let me = own_uid();
        assert!(same_user(me, me));
        assert!(!same_user(me.wrapping_add(1), me));
        // Root is someone else too (unless we are root).
        assert_eq!(same_user(0, me), me == 0);
        // The errors say what is wrong without naming a path.
        for err in [
            BindError::NotPrivate,
            BindError::TooLong,
            BindError::InTheWay,
            BindError::InUse,
        ] {
            let text = err.to_string();
            assert!(!text.is_empty() && !text.contains('/'), "{text}");
        }
    }
}
