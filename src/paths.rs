//! Where configuration, data, logs and runtime files live.
//!
//! The profile's names for its folders and files are here. Finding the user's folders
//! (`etcetera`) and making them with their permissions is in [`system`].

mod system;

use std::path::PathBuf;

use anyhow::{Result, bail};

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

    /// The saved messages and their reminders: where they are and when, no text.
    pub fn saved_file(&self) -> PathBuf {
        self.data_dir.join("saved.json")
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
}
