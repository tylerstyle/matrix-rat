//! Where configuration, data, logs and runtime files live.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use etcetera::{BaseStrategy, choose_base_strategy};

const APP: &str = "matrix-rat";

#[derive(Debug, Clone)]
pub struct Paths {
    /// `~/.config/matrix-rat` — config and themes, shared by all profiles.
    pub config_dir: PathBuf,
    /// `~/.local/share/matrix-rat/<profile>` — session and SDK stores.
    pub data_dir: PathBuf,
    /// `~/.local/state/matrix-rat/<profile>` — logs.
    pub log_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/matrix-rat` — pid files used for theme reload signals.
    pub runtime_dir: PathBuf,
    pub profile: String,
}

impl Paths {
    pub fn new(profile: &str) -> Result<Self> {
        validate_profile(profile)?;
        let base = choose_base_strategy().context("could not determine home directory")?;
        let state = base.state_dir().unwrap_or_else(|| base.data_dir());
        let runtime = base
            .runtime_dir()
            .unwrap_or_else(|| std::env::temp_dir().join(format!("{APP}-runtime")));
        Ok(Self {
            config_dir: base.config_dir().join(APP),
            data_dir: base.data_dir().join(APP).join(profile),
            log_dir: state.join(APP).join(profile),
            runtime_dir: runtime.join(APP),
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
        Ok(())
    }

    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("lock")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}.pid", self.profile))
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
        assert_eq!(a.config_dir, b.config_dir);
    }
}
