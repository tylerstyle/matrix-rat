//! Where configuration, data, logs and runtime files live.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use etcetera::{BaseStrategy, choose_base_strategy};

const APP: &str = "matrix-rat";

#[derive(Clone)]
pub struct Paths {
    /// `~/.config/matrix-rat` — config and themes, shared by all profiles.
    pub config_dir: PathBuf,
    /// `~/.local/share/matrix-rat/<profile>` — session and SDK stores.
    pub data_dir: PathBuf,
    /// `~/.local/state/matrix-rat/<profile>` — logs.
    pub log_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/matrix-rat` (or `~/.local/state/matrix-rat/run`) — pid files used for theme reload signals.
    pub runtime_dir: PathBuf,
    pub profile: String,
}

/// Hand-written: the folders name the user's home folder, and the profile name is the
/// user's own choice.
impl std::fmt::Debug for Paths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Paths")
    }
}

impl Paths {
    pub fn new(profile: &str) -> Result<Self> {
        validate_profile(profile)?;
        let base = choose_base_strategy().context("could not determine home directory")?;
        let state = base.state_dir().unwrap_or_else(|| base.data_dir());
        // Without XDG_RUNTIME_DIR, stay inside the user's own state directory; a fixed
        // name under /tmp would be shared between users.
        let runtime_dir = base
            .runtime_dir()
            .map(|dir| dir.join(APP))
            .unwrap_or_else(|| state.join(APP).join("run"));
        Ok(Self {
            config_dir: base.config_dir().join(APP),
            data_dir: base.data_dir().join(APP).join(profile),
            log_dir: state.join(APP).join(profile),
            runtime_dir,
            profile: profile.to_owned(),
        })
    }

    pub fn create_dirs(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.log_dir,
            &self.runtime_dir,
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        // Holds the session, message stores, and runtime files: owner only.
        use std::os::unix::fs::PermissionsExt;
        for dir in [&self.data_dir, &self.runtime_dir] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("could not protect {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    pub fn session_file(&self) -> PathBuf {
        self.data_dir.join("session.json")
    }

    pub fn store_dir(&self) -> PathBuf {
        self.data_dir.join("store")
    }

    pub fn search_dir(&self) -> PathBuf {
        self.data_dir.join("search")
    }

    /// Transcripts and translations kept across restarts (the `keep_notes` setting):
    /// message text in plain form, outside the encrypted store.
    pub fn notes_file(&self) -> PathBuf {
        self.data_dir.join("notes.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("lock")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}.pid", self.profile))
    }

    /// Ephemeral PNG cache for desktop notification avatars of the current session.
    pub fn notify_icons_dir(&self) -> PathBuf {
        self.runtime_dir
            .join(format!("{}-notify-icons", self.profile))
    }

    /// The socket of `matrix-rat serve`, which `send`, `read`, `listen` and `mcp`
    /// talk to.
    pub fn agent_socket(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}.sock", self.profile))
    }

    /// The copies of voice and audio messages while they play, and mpv's control
    /// sockets: per profile, so that one instance's sweep at start never takes another
    /// one's sound away.
    pub fn audio_dir(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}-audio", self.profile))
    }
}

/// Profiles become directory names, so keep them to a safe character set.
fn validate_profile(profile: &str) -> Result<()> {
    let ok = !profile.is_empty()
        && profile.len() <= 64
        && profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("invalid profile name {profile:?}: use letters, digits, '-' and '_' only");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_names_no_folder_and_no_profile() {
        let paths = Paths {
            config_dir: "/home/alice/.config/matrix-rat".into(),
            data_dir: "/home/alice/.local/share/matrix-rat/secret-profile".into(),
            log_dir: "/home/alice/.local/state/matrix-rat/secret-profile".into(),
            runtime_dir: "/run/user/1000/matrix-rat".into(),
            profile: "secret-profile".into(),
        };
        let shown = format!("{paths:?}");
        for private in ["alice", "/home", "/run", "secret-profile"] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
    }

    #[test]
    fn profile_names() {
        assert!(validate_profile("default").is_ok());
        assert!(validate_profile("work_2-b").is_ok());
        assert!(validate_profile("").is_err());
        assert!(validate_profile("../etc").is_err());
        assert!(validate_profile("a b").is_err());
    }

    #[test]
    fn paths_are_per_profile() {
        let a = Paths::new("a").unwrap();
        let b = Paths::new("b").unwrap();
        assert_ne!(a.data_dir, b.data_dir);
        assert_ne!(a.pid_file(), b.pid_file());
        assert_ne!(a.search_dir(), b.search_dir());
        assert_ne!(a.notify_icons_dir(), b.notify_icons_dir());
        assert_ne!(a.audio_dir(), b.audio_dir());
        assert!(a.audio_dir().starts_with(&a.runtime_dir));
        assert_ne!(a.agent_socket(), b.agent_socket());
        assert_eq!(a.agent_socket().parent(), Some(a.runtime_dir.as_path()));
        assert!(a.agent_socket().to_string_lossy().ends_with("/a.sock"));
        assert_eq!(a.config_dir, b.config_dir);
    }
}
