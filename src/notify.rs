//! Showing notifications: through the desktop's notification service, or through the
//! terminal where that is the better (or only) way.

use std::io::Write;

use serde::Deserialize;

use crate::matrix::Notice;

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
    let title = if notice.sender.is_empty() || notice.sender == notice.room {
        notice.room.clone()
    } else {
        format!("{} in {}", notice.sender, notice.room)
    };
    let text = if preview {
        notice.body.clone()
    } else if notice.highlight {
        "You were mentioned".to_owned()
    } else {
        "New message".to_owned()
    };
    (title, text)
}

/// Shows a notification. Never blocks and never fails loudly: a notification that
/// cannot be shown is only logged.
pub fn show(mode: NotifyMode, notice: &Notice, preview: bool) {
    let (title, text) = title_and_text(notice, preview);
    match mode.resolved() {
        NotifyMode::Desktop => {
            let urgent = notice.highlight;
            // The service may be slow or absent; that must not hold up the interface.
            std::thread::spawn(move || {
                let mut notification = notify_rust::Notification::new();
                notification
                    .appname("matrix-rat")
                    .summary(&title)
                    .body(&escape_markup(&text))
                    .icon("mail-message-new");
                if urgent {
                    notification.urgency(notify_rust::Urgency::Critical);
                }
                if let Err(err) = notification.show() {
                    tracing::debug!(%err, "could not show a desktop notification");
                }
            });
        }
        NotifyMode::Terminal => {
            let mut out = std::io::stdout();
            let _ = out
                .write_all(terminal_sequence(&title, &text, kitty()).as_bytes())
                .and_then(|()| out.flush());
        }
        NotifyMode::Auto | NotifyMode::Off => {}
    }
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
fn terminal_sequence(title: &str, text: &str, kitty: bool) -> String {
    let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).collect() };
    let (title, text) = (clean(title), clean(text));
    if kitty {
        format!(
            "\x1b]99;i=matrix-rat:d=0;{title}\x1b\\\x1b]99;i=matrix-rat:d=1:p=body;{text}\x1b\\"
        )
    } else {
        format!("\x1b]9;{title}: {text}\x1b\\")
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
        // Without preview the text of the message stays private.
        assert_eq!(title_and_text(&room, false).1, "New message");
        let mention = notice("Rust", "ann", "you: look", true);
        assert_eq!(title_and_text(&mention, false).1, "You were mentioned");
    }

    #[test]
    fn nothing_from_a_message_can_break_out() {
        let sequence = terminal_sequence("a\x1b]52;c;evil\x07", "b\x1b\\c\nd", true);
        // Exactly the two sequences we wrote: two starts, two ends.
        assert_eq!(sequence.matches('\x1b').count(), 4);
        assert!(!sequence.contains('\x07') && !sequence.contains('\n'));
        assert_eq!(terminal_sequence("t", "x", false), "\x1b]9;t: x\x1b\\");
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
