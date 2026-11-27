//! What the doctor finds on this machine: the process's environment, programs on
//! `PATH`, the players and recorders the settings come to, and for `matrix-rat doctor`
//! the config file and the terminal.

use std::ffi::OsStr;
use std::path::Path;

use super::{Audio, Facts, Recording, Report, Summary, unbound};
use crate::config::{Config, ImageMode};
use crate::images::Graphics;
use crate::keymap::Keymap;
use crate::matrix::SecretStoreMode;
use crate::notify::NotifyMode;

impl Audio {
    /// From the setting, with this process's `PATH`.
    pub fn found(setting: &str) -> Self {
        Self::from_choice(&crate::audio::choose_here(setting))
    }
}

impl Recording {
    /// From the setting, with this process's `PATH` and what is seen of the sound here.
    pub fn found(setting: &str) -> Self {
        Self::from_choice(&crate::audio::recorder::choose_here(setting))
    }
}

/// Is a program of this name in one of the folders of `path` (the `PATH` variable)?
/// Looks only; nothing is run.
pub fn on_path(name: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| is_program(&dir.join(name)))
}

fn is_program(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `matrix-rat doctor`: the report for the config in `config_file` and this terminal,
/// as text.
///
/// Reads the config (a file that is not TOML is reported, not an error) and, only when
/// standard input and output are both a terminal, asks the terminal what the app asks
/// it at start, with the same code and time limits (see
/// `terminal::ask_for_doctor`). Takes no lock, logs nobody in, writes nothing.
pub fn command_line(config_file: &Path) -> String {
    use std::io::IsTerminal;
    let (config, config_broken) = read_config(config_file);
    let in_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let asked = in_terminal
        .then(|| crate::terminal::ask_for_doctor(config.images).ok())
        .flatten();
    let size = in_terminal
        .then(|| crossterm::terminal::size().ok())
        .flatten();
    Report::new(&command_line_facts(&config, config_broken, asked, size)).as_text()
}

/// The config as the app would read it at start, and whether it is not TOML at all
/// (then the defaults stand in). The reason is not kept: it quotes the file.
fn read_config(file: &Path) -> (Config, bool) {
    match Config::load(file) {
        Ok(config) => (config, false),
        Err(_) => (Config::default(), true),
    }
}

/// The facts for the command line: the environment, the config, and what the terminal
/// answered (`asked`: the keyboard protocol and pictures; `None` if it was not asked).
fn command_line_facts(
    config: &Config,
    config_broken: bool,
    asked: Option<(bool, Graphics)>,
    size: Option<(u16, u16)>,
) -> Facts {
    let (keymap, problems) = Keymap::from_config(config);
    let own_theme = [
        Some(config.theme.as_str()),
        config.theme_dark.as_deref(),
        config.theme_light.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|name| !name.is_empty())
    .any(|name| crate::theme::Theme::builtin(name).is_none());
    Facts {
        size,
        keyboard_enhanced: asked.map(|(keyboard, _)| keyboard),
        focus_reports: None,
        mouse: config.mouse,
        image_mode: config.images,
        graphics: asked.map(|(_, graphics)| graphics),
        notifications: config.notifications,
        do_not_disturb: config.do_not_disturb,
        secret_store: config.secret_store,
        own_theme,
        key_preset: keymap.preset_summary(),
        key_problems: problems.len(),
        unbound: unbound(&keymap),
        ignored_settings: config.ignored.clone(),
        config_broken,
        audio: Audio::found(&config.audio_player),
        recording: Recording::found(&config.audio_recorder),
        transcribe: crate::notes::TranscribeFound::of(&crate::notes::transcriber_here(
            &config.transcribe_command,
            &config.transcribe_model,
        )),
        translate: crate::notes::TranslateFound::of(&crate::notes::translator_here(
            &config.translate_command,
        )),
        summary: Summary::from_choice(&crate::catchup::choose_here(&config.summary_command)),
        ..Facts::from_environment()
    }
}

impl Facts {
    /// What the process's environment says, with everything else as a plain default
    /// for the caller to fill in from what it knows.
    pub fn from_environment() -> Self {
        let text = |name: &str| std::env::var(name).unwrap_or_default();
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        let path = std::env::var_os("PATH");
        Self {
            version: env!("CARGO_PKG_VERSION"),
            term: text("TERM"),
            term_program: text("TERM_PROGRAM"),
            size: None,
            truecolor: crate::theme::truecolor_supported(),
            no_color: crate::theme::no_color_wanted(),
            multiplexer: crate::images::in_multiplexer(),
            ssh: crate::portal::in_ssh_session(),
            keyboard_enhanced: None,
            focus_reports: None,
            mouse: true,
            image_mode: ImageMode::Auto,
            graphics: None,
            placeholders_broken: crate::images::placeholders_broken(),
            notifications: NotifyMode::Auto,
            do_not_disturb: false,
            session_bus: crate::notify::session_bus(),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            wl_paste: on_path("wl-paste", path.as_deref()),
            xclip: on_path("xclip", path.as_deref()),
            opener: on_path("xdg-open", path.as_deref()),
            audio: Audio::NoneFound,
            recording: Recording::NoMicrophone,
            transcribe: crate::notes::TranscribeFound::Not(crate::notes::NoTranscriber::NoProgram),
            translate: crate::notes::TranslateFound::Not(crate::notes::NoTranslator::NotSet),
            summary: Summary::NotSet,
            editor_set: crate::editor::command_from_env().is_ok(),
            secret_store: SecretStoreMode::File,
            own_theme: false,
            key_preset: String::new(),
            key_problems: 0,
            unbound: Vec::new(),
            ignored_settings: Vec::new(),
            config_broken: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `matrix-rat doctor` prints for this config file, without asking the
    /// terminal (the tests may well run in one).
    fn printed(file: &Path) -> String {
        let (config, broken) = read_config(file);
        Report::new(&command_line_facts(&config, broken, None, None)).as_text()
    }

    #[test]
    fn the_command_line_reads_a_config_without_writing_anything() {
        let dir = std::env::temp_dir().join(format!("mr-doctor-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config").join("config.toml");
        // No file: the defaults, and nothing is made.
        let text = printed(&file);
        assert!(
            text.contains("  Settings in the config: all understood"),
            "{text}"
        );
        assert!(text.contains("  Theme: built in"), "{text}");
        assert!(text.contains("~ Keyboard: not asked"), "{text}");
        assert!(!dir.exists());
        // Values that are not understood, by name; a theme of one's own, unnamed.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "mouse = \"yes\"\ntheme = \"alice-own-theme\"\n").unwrap();
        let text = printed(&file);
        assert!(
            text.contains("! Settings in the config: not understood: mouse"),
            "{text}"
        );
        assert!(text.contains("Theme: a file of your own"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        // A file that is not TOML at all is said to be so, without the reason (which
        // quotes the file and names its folder).
        std::fs::write(&file, "mouse = = 1").unwrap();
        let text = printed(&file);
        assert!(text.contains("the file is not valid TOML"), "{text}");
        assert!(!text.contains(&*dir.to_string_lossy()), "{text}");
        // Only the config is there: nothing was written beside it.
        assert_eq!(walk(&dir), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn a_program_is_found_on_the_path_without_being_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mr-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("some-tool");
        std::fs::write(&tool, "").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.as_path()]).unwrap();
        // A file that may not be run is not a program.
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!on_path("some-tool", Some(&path)));
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(on_path("some-tool", Some(&path)));
        assert!(!on_path("other-tool", Some(&path)));
        assert!(!on_path("some-tool", None));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
