//! For tests that need a message bus of their own.
//!
//! `dbus-daemon --session` reads the session's service files, so a name nobody on the
//! bus owns can be answered by starting the real program for it: asking a test's own
//! bus for `org.freedesktop.secrets` would start the desktop's keyring, which then asks
//! the person at the screen to unlock it. A bus started with this configuration knows
//! no service files and starts nothing.

use std::path::PathBuf;
use std::sync::OnceLock;

const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

/// The argument that makes `dbus-daemon` a bus that starts no services (in place of
/// `--session`). The file is written once per test run.
pub fn config_arg() -> String {
    static FILE: OnceLock<PathBuf> = OnceLock::new();
    let path = FILE.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("matrix-rat-testbus-{}.conf", std::process::id()));
        std::fs::write(&path, CONFIG).expect("the bus configuration can be written");
        path
    });
    format!("--config-file={}", path.display())
}
