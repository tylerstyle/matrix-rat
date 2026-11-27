//! The profile's folders on this machine: where the user's base folders are
//! (`etcetera`, from the XDG variables and the home folder) and making them, the
//! private ones owner-only.

use anyhow::{Context, Result};
use etcetera::{BaseStrategy, choose_base_strategy};

use super::{APP, Paths, validate_profile};

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
