//! Custom emoji and stickers from image packs: asking the worker for the packs when
//! something first needs them, and the sticker picker. (Custom emoji are offered in the
//! emoji picker too; see `App::pick_emoji`.)

use super::{App, ComposeMode};
use crate::input::{KeyCode, KeyModifiers};
use crate::lineedit::EditKeys;
use crate::matrix::{EmoteCatalog, MatrixCmd};
use crate::modal::{Modal, StickerPicker};

/// The image packs as the UI knows them.
#[derive(Default)]
pub struct EmoteState {
    /// What the worker sent last.
    pub catalog: EmoteCatalog,
    /// Whether the packs were asked for in this session, and for which room.
    requested: bool,
    room: Option<String>,
    /// Whether packs have arrived since they were first asked for.
    loaded: bool,
    /// Bumped when the catalog changes; reactions are shown by their shortcodes.
    pub rev: u64,
}

/// Hand-written: the room id stays out of logs.
impl std::fmt::Debug for EmoteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmoteState")
            .field("catalog", &self.catalog)
            .field("requested", &self.requested)
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

/// The filter argument of a `/sticker` command, or `None` if `text` is not one.
pub(super) fn sticker_command_argument(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/sticker")?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

impl App {
    /// Asks the worker for the packs of the account and the open room, once per room.
    /// Nothing is loaded before something needs them.
    pub(super) fn want_emotes(&mut self) {
        if self.matrix.is_none() {
            return;
        }
        let room = self.room_id();
        if self.emotes.requested && self.emotes.room == room {
            return;
        }
        self.emotes.requested = true;
        self.emotes.room.clone_from(&room);
        // Until they arrive, what is known lacks the new room's packs.
        self.emotes.loaded = false;
        self.send_matrix(MatrixCmd::EnsureEmotePacks { room_id: room });
    }

    /// Another room was opened: its packs join the catalog if packs are in use.
    pub(super) fn follow_room_emotes(&mut self) {
        if self.emotes.requested {
            self.want_emotes();
        }
    }

    /// Packs loaded for `room_id`. Those of a room left while they were on their way
    /// are dropped: the packs of the room open now follow.
    pub(super) fn on_emote_packs(&mut self, room_id: Option<String>, catalog: EmoteCatalog) {
        if !self.emotes.requested || room_id != self.emotes.room {
            return;
        }
        self.emotes.loaded = true;
        if catalog != self.emotes.catalog {
            self.emotes.catalog = catalog;
            self.emotes.rev += 1;
        }
        match &mut self.modal {
            Some(Modal::Stickers(picker)) => picker.set_catalog(&self.emotes.catalog, false),
            Some(Modal::Emoji(picker)) => picker.set_custom(&self.emotes.catalog),
            _ => {}
        }
        self.sync_completion();
    }

    pub(super) fn reset_emotes(&mut self) {
        self.emotes = EmoteState::default();
    }

    /// Handles `/sticker [filter]` on Enter in the composer. Returns `true` if handled.
    pub(super) fn sticker_command(&mut self) -> bool {
        let text = self.composer.text().to_owned();
        let Some(filter) = sticker_command_argument(&text) else {
            return false;
        };
        if self.open_stickers(filter) {
            self.composer.set_text("");
            if !self.restore_aside_draft() {
                self.sync_completion();
            }
        }
        true
    }

    /// Opens the sticker picker for the open room (or thread). `false` when it cannot be.
    pub(super) fn open_stickers(&mut self, filter: &str) -> bool {
        if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.show_toast("Finish or cancel the edit first");
            return false;
        }
        if let Some(hint) = self
            .room()
            .filter(|room| room.is_pending())
            .map(|room| room.kind.pending_hint())
        {
            self.show_toast(hint);
            return false;
        }
        if self.matrix.is_none() {
            self.show_toast("The demo has no sticker packs");
            return false;
        }
        let Some(room_id) = self.room_id() else {
            return false;
        };
        self.want_emotes();
        let loading = !self.emotes.loaded;
        self.open_modal(Modal::Stickers(StickerPicker::new(
            room_id,
            filter,
            &self.emotes.catalog,
            loading,
        )));
        true
    }

    pub(super) fn on_stickers_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Stickers(picker)) = &mut self.modal else {
            return;
        };
        let page = picker.page.max(1) as isize;
        match (code, mods) {
            (KeyCode::Esc, _) => self.modal = None,
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => picker.step(-1),
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => picker.step(1),
            (KeyCode::PageUp, _) => picker.step(-page),
            (KeyCode::PageDown, _) => picker.step(page),
            (KeyCode::Enter, _) => {
                let selected = picker.selected;
                self.send_sticker_at(selected);
            }
            _ => {
                let keys = EditKeys {
                    arrows: true,
                    home_end: true,
                    delete: true,
                    kill_to_end: true,
                };
                if picker.filter.handle_key(code, mods, keys).changed() {
                    picker.refilter();
                }
            }
        }
    }

    pub(super) fn on_stickers_paste(&mut self, first_line: &str) {
        if let Some(Modal::Stickers(picker)) = &mut self.modal
            && picker.filter.paste(first_line).changed()
        {
            picker.refilter();
        }
    }

    pub(super) fn scroll_stickers(&mut self, lines: isize) {
        if let Some(Modal::Stickers(picker)) = &mut self.modal {
            picker.scroll_by(lines);
        }
    }

    /// Sends the sticker on row `index` of the picker's list and closes the picker.
    pub(super) fn send_sticker_at(&mut self, index: usize) {
        let Some(Modal::Stickers(picker)) = &self.modal else {
            return;
        };
        let Some(sticker) = picker.sticker_at(index).cloned() else {
            return;
        };
        let room_id = picker.room_id.clone();
        self.modal = None;
        // The room may have gone from the list while the picker was open.
        if self.room_id().as_deref() != Some(room_id.as_str()) {
            self.show_toast("That room is not open any more");
            return;
        }
        self.send_matrix(MatrixCmd::SendSticker {
            room_id,
            name: sticker.name,
            url: sticker.url,
            dimensions: sticker.dimensions,
            size: sticker.size,
            mime: sticker.mime,
        });
        self.timeline.scroll = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::app::{Config, MatrixHandle};
    use crate::input::{InputEvent, KeyEvent};
    use crate::matrix::{CustomEmoji, EmotePack, MatrixEvent, StickerItem};
    use std::path::PathBuf;
    use tokio::sync::mpsc::UnboundedReceiver;

    const ROOM: &str = "!room:example.org";

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn logged_in() -> (App, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let mut app = App::new(&Config::default(), PathBuf::new(), Some(handle));
        app.rooms = crate::model::demo_rooms();
        app.rooms[0].id = ROOM.into();
        app.rooms[1].id = "!other:example.org".into();
        app.active = 0;
        (app, commands)
    }

    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        std::iter::from_fn(|| commands.try_recv().ok()).collect()
    }

    fn sticker(name: &str, code: &str, url: &str, pack: &str) -> StickerItem {
        StickerItem {
            shortcode: code.into(),
            name: name.into(),
            url: url.into(),
            pack_name: pack.into(),
            dimensions: Some((128, 96)),
            size: Some(4096),
            mime: Some("image/png".into()),
        }
    }

    pub(crate) fn catalog() -> EmoteCatalog {
        EmoteCatalog {
            packs: vec![
                EmotePack {
                    id: "user".into(),
                    name: "Personal".into(),
                    emojis: vec![CustomEmoji {
                        shortcode: "blobcat".into(),
                        url: "mxc://example.org/blobcat".into(),
                        body: None,
                        pack_name: "Personal".into(),
                    }],
                    stickers: vec![sticker(
                        "Waving crab",
                        "crab_wave",
                        "mxc://example.org/crab",
                        "Personal",
                    )],
                },
                EmotePack {
                    id: "room".into(),
                    name: "Rats".into(),
                    emojis: Vec::new(),
                    stickers: vec![
                        sticker("Happy rat", "rat_happy", "mxc://example.org/rat1", "Rats"),
                        sticker("Sad rat", "rat_sad", "mxc://example.org/rat2", "Rats"),
                    ],
                },
            ],
        }
    }

    /// The packs of the account and of `room`, as the worker sends them.
    fn packs(room: &str, catalog: EmoteCatalog) -> MatrixEvent {
        MatrixEvent::EmotePacks {
            room_id: Some(room.into()),
            catalog,
        }
    }

    #[test]
    fn the_sticker_picker_asks_for_the_packs_once_and_sends_the_chosen_sticker() {
        let (mut app, mut commands) = logged_in();
        app.perform(Action::OpenStickers);
        let Some(Modal::Stickers(picker)) = &app.modal else {
            panic!("the sticker picker is open");
        };
        assert!(picker.loading);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [MatrixCmd::EnsureEmotePacks { room_id: Some(room) }] if room == ROOM
        ));

        app.on_matrix(packs(ROOM, catalog()));
        let Some(Modal::Stickers(picker)) = &app.modal else {
            panic!("still open");
        };
        assert!(!picker.loading);
        assert_eq!(picker.filtered.len(), 3);

        // Every word must match the name, the shortcode or the pack.
        typed(&mut app, "rat sad");
        let Some(Modal::Stickers(picker)) = &app.modal else {
            panic!("still open");
        };
        assert_eq!(picker.filtered.len(), 1);
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let sent_now = sent(&mut commands);
        let [
            MatrixCmd::SendSticker {
                room_id,
                name,
                url,
                dimensions,
                size,
                mime,
            },
        ] = sent_now.as_slice()
        else {
            panic!("one sticker is sent: {sent_now:?}");
        };
        assert_eq!(room_id, ROOM);
        assert_eq!(name, "Sad rat");
        assert_eq!(url, "mxc://example.org/rat2");
        assert_eq!(*dimensions, Some((128, 96)));
        assert_eq!(*size, Some(4096));
        assert_eq!(mime.as_deref(), Some("image/png"));

        // Open again: the packs are known, nothing more is asked.
        app.perform(Action::OpenStickers);
        assert!(matches!(&app.modal, Some(Modal::Stickers(p)) if !p.loading));
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn the_sticker_command_opens_the_picker_with_its_filter_and_follows_the_room() {
        assert_eq!(sticker_command_argument("/sticker"), Some(""));
        assert_eq!(
            sticker_command_argument("  /sticker  happy rat "),
            Some("happy rat")
        );
        assert_eq!(sticker_command_argument("/stickers"), None);
        assert_eq!(sticker_command_argument("hello /sticker"), None);

        let (mut app, mut commands) = logged_in();
        app.composer.set_text("/sticker happy");
        app.on_event(key(KeyCode::Enter));
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, MatrixCmd::EnsureEmotePacks { .. }))
        );
        app.on_matrix(packs(ROOM, catalog()));
        let Some(Modal::Stickers(picker)) = &app.modal else {
            panic!("the sticker picker is open");
        };
        assert_eq!(picker.filter.as_str(), "happy");
        assert_eq!(picker.filtered.len(), 1);
        assert_eq!(app.composer.text(), "");

        // Packs in use: another room's packs are asked for when it is opened.
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::OpenRoom(1));
        assert!(sent(&mut commands).iter().any(|c| matches!(
            c,
            MatrixCmd::EnsureEmotePacks { room_id: Some(room) } if room == "!other:example.org"
        )));
        // Those of the room left behind, still on their way then, are not taken for
        // them: the picker waits for the right ones.
        assert!(app.open_stickers(""));
        let rats = EmoteCatalog {
            packs: catalog().packs[1..].to_vec(),
        };
        app.on_matrix(packs(ROOM, rats));
        let Some(Modal::Stickers(picker)) = &app.modal else {
            panic!("the sticker picker is open");
        };
        assert!(picker.loading);
        assert_eq!(app.emotes.catalog, catalog());
        app.on_matrix(packs("!other:example.org", EmoteCatalog::default()));
        let Some(Modal::Stickers(picker)) = &app.modal else {
            panic!("the sticker picker is open");
        };
        assert!(!picker.loading);
        assert!(picker.filtered.is_empty());
    }

    #[test]
    fn packs_are_not_asked_for_until_something_needs_them() {
        let (mut app, mut commands) = logged_in();
        app.perform(Action::OpenRoom(1));
        typed(&mut app, "hello there");
        assert!(
            !sent(&mut commands)
                .iter()
                .any(|c| matches!(c, MatrixCmd::EnsureEmotePacks { .. }))
        );
        // An emoji shortcode being typed is the first use.
        typed(&mut app, " :bl");
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, MatrixCmd::EnsureEmotePacks { .. }))
        );
    }

    fn reactions(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        sent(commands)
            .into_iter()
            .filter(|c| matches!(c, MatrixCmd::ToggleReaction { .. }))
            .collect()
    }

    /// Opens the emoji picker on the first message (to react to it) or for the draft,
    /// with the packs arrived, and types `query`.
    fn picked(react: bool, query: &str) -> (App, UnboundedReceiver<MatrixCmd>) {
        let (mut app, mut commands) = logged_in();
        app.rooms[0].messages[0].item = Some(crate::model::ItemRef::Event("$ev".into()));
        if react {
            app.timeline.selected = Some(0);
            app.perform(Action::React);
        } else {
            app.perform(Action::OpenEmoji);
        }
        assert!(matches!(app.modal, Some(Modal::Emoji(_))));
        // The packs are asked for, and offered as soon as they are there.
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, MatrixCmd::EnsureEmotePacks { .. }))
        );
        app.on_matrix(packs(ROOM, catalog()));
        typed(&mut app, query);
        (app, commands)
    }

    #[test]
    fn reacting_with_a_custom_emoji_sends_its_address_and_its_shortcode() {
        let (mut app, mut commands) = picked(true, "blobcat");
        let Some(Modal::Emoji(picker)) = &app.modal else {
            panic!("the picker is open");
        };
        assert_eq!(
            picker.custom_at(0).map(|e| e.shortcode.as_str()),
            Some("blobcat")
        );
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let sent_now = reactions(&mut commands);
        let [
            MatrixCmd::ToggleReaction {
                room_id,
                key: reacted,
                shortcode,
                ..
            },
        ] = sent_now.as_slice()
        else {
            panic!("one reaction is sent: {sent_now:?}");
        };
        assert_eq!(room_id, ROOM);
        assert_eq!(reacted, "mxc://example.org/blobcat");
        assert_eq!(shortcode.as_deref(), Some("blobcat"));

        // An ordinary emoji goes as itself, without a shortcode.
        let (mut app, mut commands) = picked(true, "thumbsup");
        app.on_event(key(KeyCode::Enter));
        let sent_now = reactions(&mut commands);
        assert!(
            matches!(
                sent_now.as_slice(),
                [MatrixCmd::ToggleReaction { key: reacted, shortcode: None, .. }] if reacted == "👍"
            ),
            "{sent_now:?}"
        );
    }

    #[test]
    fn a_picture_two_packs_name_differently_reacts_by_the_name_that_was_chosen() {
        let (mut app, mut commands) = logged_in();
        app.rooms[0].messages[0].item = Some(crate::model::ItemRef::Event("$ev".into()));
        app.timeline.selected = Some(0);
        app.perform(Action::React);
        let mut shared = catalog();
        shared.packs[1].emojis.push(CustomEmoji {
            shortcode: "kitty".into(),
            url: "mxc://example.org/blobcat".into(),
            body: None,
            pack_name: "Rats".into(),
        });
        app.on_matrix(packs(ROOM, shared));
        typed(&mut app, "kitty");
        app.on_event(key(KeyCode::Enter));
        let sent_now = reactions(&mut commands);
        assert!(
            matches!(
                sent_now.as_slice(),
                [MatrixCmd::ToggleReaction { key, shortcode: Some(code), .. }]
                    if key == "mxc://example.org/blobcat" && code == "kitty"
            ),
            "{sent_now:?}"
        );

        // A chip whose key came with spaces around it still goes by the packs' name,
        // and is taken back by exactly its key.
        app.toggle_reaction(0, " mxc://example.org/blobcat ");
        let sent_now = reactions(&mut commands);
        assert!(
            matches!(
                sent_now.as_slice(),
                [MatrixCmd::ToggleReaction { key, shortcode: Some(code), .. }]
                    if key == " mxc://example.org/blobcat " && code == "blobcat"
            ),
            "{sent_now:?}"
        );
    }

    #[test]
    fn logging_out_closes_the_emoji_picker() {
        let (mut app, _commands) = picked(true, "blob");
        app.reset_session();
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_custom_emoji_chosen_for_the_draft_is_sent_as_its_picture() {
        let (mut app, _commands) = picked(false, "blobcat");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.composer.text(), ":blobcat: ");
        let crate::format::Outgoing::Text {
            html: Some(html), ..
        } = app.format_outgoing(app.composer.text())
        else {
            panic!("formatted text");
        };
        assert!(
            html.contains("<img data-mx-emoticon src=\"mxc://example.org/blobcat\""),
            "{html}"
        );

        // After a word it is set apart, or it would stay text.
        let (mut app, _commands) = logged_in();
        app.composer.set_text("hello");
        app.perform(Action::OpenEmoji);
        app.on_matrix(packs(ROOM, catalog()));
        typed(&mut app, "blobcat");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.composer.text(), "hello :blobcat: ");
        let crate::format::Outgoing::Text {
            html: Some(html), ..
        } = app.format_outgoing(app.composer.text())
        else {
            panic!("formatted text");
        };
        assert!(html.contains("data-mx-emoticon"), "{html}");
    }

    #[test]
    fn the_demo_has_no_sticker_picker_and_the_picker_logs_nothing_private() {
        let mut demo = App::demo();
        demo.perform(Action::OpenStickers);
        assert!(demo.modal.is_none());
        assert!(demo.toast.is_some());

        let picker = StickerPicker::new(ROOM.into(), "", &catalog(), false);
        let debug = format!("{picker:?}");
        assert!(!debug.contains("example.org"), "{debug}");
        assert!(!debug.contains("rat"), "{debug}");
    }
}
