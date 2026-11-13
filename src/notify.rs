//! Showing notifications: through the desktop's notification service, or through the
//! terminal where that is the better (or only) way.

use std::io::Write;

use serde::Deserialize;

use crate::matrix::Notice;
use crate::text::truncate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    /// The desktop's service when there is one, else the terminal.
    #[default]
    Auto,
    Desktop,
    /// Escape sequences the terminal turns into a notification (kitty, iTerm2, foot,
    /// WezTerm…). Also works over SSH.
    Terminal,
    Off,
}

impl NotifyMode {
    pub fn name(self) -> &'static str {
        match self {
            NotifyMode::Auto => "auto",
            NotifyMode::Desktop => "desktop",
            NotifyMode::Terminal => "terminal",
            NotifyMode::Off => "off",
        }
    }

    /// What `Auto` means on this machine.
    fn resolved(self) -> NotifyMode {
        if self != NotifyMode::Auto {
            return self;
        }
        let remote = std::env::var_os("SSH_CONNECTION").is_some();
        let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
        if bus && !remote {
            NotifyMode::Desktop
        } else {
            NotifyMode::Terminal
        }
    }
}

/// Title and text as shown: "sender in room" for rooms, the person for direct chats.
pub fn title_and_text(notice: &Notice, preview: bool) -> (String, String) {
    // Without preview nothing of the conversation is shown: not the text, not who
    // wrote, not where.
    if !preview {
        let text = if notice.highlight {
            "You were mentioned"
        } else {
            "New message"
        };
        return ("matrix-rat".to_owned(), text.to_owned());
    }
    let title = if notice.sender.is_empty() || notice.sender == notice.room {
        notice.room.clone()
    } else {
        format!("{} in {}", notice.sender, notice.room)
    };
    (truncate(&title, 120), notice.body.clone())
}

/// Shows a notification. Never blocks and never fails loudly: a notification that
/// cannot be shown is only logged.
pub fn show(mode: NotifyMode, notice: &Notice, preview: bool) {
    let (title, text) = title_and_text(notice, preview);
    match mode.resolved() {
        NotifyMode::Desktop => {
            let _ = desktop().send((title, text, notice.highlight));
        }
        NotifyMode::Terminal => {
            let mut out = std::io::stdout();
            let id = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = out
                .write_all(terminal_sequence(&title, &text, kitty(), id).as_bytes())
                .and_then(|()| out.flush());
        }
        NotifyMode::Auto | NotifyMode::Off => {}
    }
}

static SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// One thread shows all desktop notifications, one after the other: the service may be
/// slow or absent, which must not hold up the interface, and a burst must not start a
/// thread and a bus connection each.
fn desktop() -> &'static std::sync::mpsc::Sender<(String, String, bool)> {
    static SENDER: std::sync::OnceLock<std::sync::mpsc::Sender<(String, String, bool)>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<(String, String, bool)>();
        std::thread::spawn(move || {
            for (title, text, urgent) in receiver {
                let mut notification = notify_rust::Notification::new();
                notification
                    .appname("matrix-rat")
                    .summary(&escape_markup(&title))
                    .body(&escape_markup(&text))
                    .icon("mail-message-new");
                if urgent {
                    notification.urgency(notify_rust::Urgency::Critical);
                }
                if let Err(err) = notification.show() {
                    tracing::debug!(%err, "could not show a desktop notification");
                }
            }
        });
        sender
    })
}

fn kitty() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.contains("kitty"))
}

/// Notification servers read the text as simple markup.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// OSC 99 (kitty's protocol, with title and text) or the older OSC 9 (one line).
/// The text is from other people: nothing that could end the sequence may be in it.
fn terminal_sequence(title: &str, text: &str, kitty: bool, id: u32) -> String {
    let clean = |s: &str| -> String {
        crate::text::sanitize_line(s)
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    };
    let (title, text) = (clean(title), clean(text));
    if kitty {
        // Each notification has its own id, so a new one does not replace an unread one.
        format!(
            "\x1b]99;i=mr{id}:d=0:p=title;{title}\x1b\\\x1b]99;i=mr{id}:d=1:p=body;{text}\x1b\\"
        )
    } else {
        // In OSC 9 a leading "4;" would be read as a progress report, so no text of
        // ours may put a semicolon where the terminal looks for one.
        let line = format!("{title}: {text}").replace(';', ",");
        format!("\x1b]9;{line}\x1b\\")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(room: &str, sender: &str, body: &str, highlight: bool) -> Notice {
        Notice {
            room_id: "!a".into(),
            room: room.into(),
            sender: sender.into(),
            body: body.into(),
            highlight,
            thread: None,
        }
    }

    #[test]
    fn titles_and_previews() {
        let room = notice("Rust", "ann", "hello", false);
        assert_eq!(
            title_and_text(&room, true),
            ("ann in Rust".to_owned(), "hello".to_owned())
        );
        // A direct chat is named after the person; no "ann in ann".
        let direct = notice("ann", "ann", "hello", false);
        assert_eq!(title_and_text(&direct, true).0, "ann");
        // Without preview nothing of the conversation shows: no text, sender or room.
        assert_eq!(
            title_and_text(&room, false),
            ("matrix-rat".to_owned(), "New message".to_owned())
        );
        let mention = notice("Rust", "ann", "you: look", true);
        assert_eq!(title_and_text(&mention, false).1, "You were mentioned");
    }

    #[test]
    fn nothing_from_a_message_can_break_out() {
        let sequence = terminal_sequence("a\x1b]52;c;evil\x07", "b\x1b\\c\nd", true, 7);
        // Exactly the two sequences we wrote: two starts, two ends.
        assert_eq!(sequence.matches('\x1b').count(), 4);
        assert!(!sequence.contains('\x07') && !sequence.contains('\n'));
        assert!(sequence.contains("i=mr7:d=0:p=title;") && sequence.contains("i=mr7:d=1:p=body;"));
        assert_eq!(terminal_sequence("t", "x", false, 1), "\x1b]9;t: x\x1b\\");
        // A name must not turn the notification into a progress report (OSC 9;4).
        assert_eq!(
            terminal_sequence("4;3;50", "x;y", false, 1),
            "\x1b]9;4,3,50: x,y\x1b\\"
        );
        assert_eq!(
            escape_markup("<b>a & b</b>"),
            "&lt;b&gt;a &amp; b&lt;/b&gt;"
        );
    }

    #[test]
    fn explicit_modes_are_kept() {
        assert_eq!(NotifyMode::Off.resolved(), NotifyMode::Off);
        assert_eq!(NotifyMode::Terminal.resolved(), NotifyMode::Terminal);
        assert_ne!(NotifyMode::Auto.resolved(), NotifyMode::Auto);
        let config: crate::config::Config = toml::from_str("notifications = \"terminal\"").unwrap();
        assert_eq!(config.notifications, NotifyMode::Terminal);
    }
}
