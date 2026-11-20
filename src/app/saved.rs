//! The app's side of saved messages and reminders: saving the selected message, the
//! "Saved messages" popup with its reminder form, and reminders coming due.
//!
//! The list is the store's (`crate::saved`); who wrote a saved message and what it says
//! is asked from the worker while the popup shows it, and for a due reminder's
//! notification only when notifications show text at all.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use super::global_search::find_target_message;
use crate::action::Action;
use crate::lineedit::EditKeys;
use crate::matrix::{MatrixCmd, Notice, SAVED_MAX_FETCH, SavedCmd, SavedEvent};
use crate::modal::Modal;
use crate::model::{Message, MsgKind, SendState};
use crate::saved::{
    Preset, RemindForm, Reminder, RoomLabel, SavedDetails, SavedItem, SavedPopup, SavedStore,
    SavedText, Stage, parse_when, when_label,
};
use crate::text::{sanitize_line, truncate};

/// How long a due reminder waits for its message's text before its notification goes
/// out without it (offline, say).
pub(super) const TEXT_WAIT: Duration = Duration::from_secs(8);
/// Rows asked for beyond those the popup showed last, so that a step down finds its
/// row filled.
const FETCH_AHEAD: usize = 4;
/// Rows asked for before the popup was drawn once.
const FETCH_FIRST: usize = 12;
/// The demo's excerpts are as long as the worker's.
const SNIPPET_MAX: usize = 120;

/// Marks the messages that are saved. `by_index`: the demo's messages have no ids and
/// are known by their place (`demo:3`). True if a mark changed.
pub(super) fn flag(messages: &mut [Message], saved: &HashSet<&str>, by_index: bool) -> bool {
    let mut changed = false;
    for (index, message) in messages.iter_mut().enumerate() {
        let is_saved = match message.event_id() {
            Some(id) => saved.contains(id),
            None if !message.id.is_empty() => saved.contains(message.id.as_str()),
            None => by_index && saved.contains(demo_key(index).as_str()),
        };
        changed |= message.saved != is_saved;
        message.saved = is_saved;
    }
    changed
}

fn demo_key(index: usize) -> String {
    format!("demo:{index}")
}

/// A message of an ignored user shows nothing of them, as in the pinned list.
fn hide_ignored(mut details: SavedDetails, ignored: &std::collections::BTreeSet<String>) -> SavedDetails {
    if details
        .sender_id
        .as_deref()
        .is_some_and(|id| ignored.contains(id))
    {
        details.sender = "Ignored user".to_owned();
        details.sender_id = None;
        details.snippet = "Message from an ignored user".to_owned();
    }
    details
}

impl App {
    // ----- the list and its file -----------------------------------------------------

    /// Reads the saved messages of the profile. Called once at start; the demo keeps
    /// its list in memory.
    pub fn load_saved(&mut self, file: std::path::PathBuf) {
        let now = self.saved.clock.now_secs();
        let (store, problem) = SavedStore::load(file, now);
        self.saved.store = store;
        let missed = self.saved.store.due(now).count();
        if let Some(problem) = problem {
            tracing::warn!("the saved messages file could not be read in full");
            self.show_toast(problem.message());
        } else if missed == 1 {
            self.show_toast("A reminder came due while matrix-rat was not running");
        } else if missed > 1 {
            self.show_toast(format!(
                "{missed} reminders came due while matrix-rat was not running"
            ));
        }
    }

    fn persist_saved(&mut self) {
        if let Err(err) = self.saved.store.write() {
            tracing::warn!(why = %crate::logsafe::kind(&err), "could not write the saved messages");
            self.show_toast("Could not write the saved messages file");
        }
    }

    /// How many reminders are due and not gone to yet, for the status line.
    pub fn reminders_due(&self) -> usize {
        self.saved.store.due(self.saved.clock.now_secs()).count()
    }

    pub fn saved_room_label(&self, room_id: &str) -> RoomLabel {
        match self.rooms.iter().find(|room| room.id == room_id) {
            Some(room) => RoomLabel::Name(room.name.clone()),
            None if self.matrix.is_some() && !self.rooms_received => RoomLabel::Unknown,
            None => RoomLabel::Gone,
        }
    }

    /// Under what the message at `index` of the open conversation is saved: its event
    /// id; in the demo, its id or its place in the room. `None` for what cannot be
    /// saved: a line that is no message, one that is not sent yet, a deleted one
    /// (unless it is saved, so that it can be unsaved).
    pub(super) fn saved_key(&self, index: usize) -> Option<String> {
        let message = self.messages().get(index)?;
        if message.is_system_or_marker()
            || message.state != SendState::Sent
            || (message.kind == MsgKind::Redacted && !message.saved)
            || self.room()?.is_pending()
        {
            return None;
        }
        if let Some(id) = message.event_id() {
            return Some(id.to_owned());
        }
        if self.matrix.is_some() {
            return None;
        }
        if !message.id.is_empty() {
            return Some(message.id.clone());
        }
        // The demo's thread view counts its messages differently.
        (!self.thread_open()).then(|| demo_key(index))
    }

    /// The selected message as an entry for the list.
    fn selected_as_saved(&mut self) -> Option<SavedItem> {
        let Some(index) = self.timeline.selected else {
            self.show_toast("No message is selected");
            return None;
        };
        let (Some(event_id), Some(room_id)) = (self.saved_key(index), self.room_id()) else {
            self.show_toast("This message cannot be saved");
            return None;
        };
        Some(SavedItem {
            room_id,
            event_id,
            thread: self.threads.open_root().map(str::to_owned),
            saved_at: self.saved.clock.now_secs(),
            remind_at: None,
            done: false,
        })
    }

    /// Saves the selected message for later, or takes it off the list.
    pub(super) fn toggle_saved(&mut self) {
        let Some(item) = self.selected_as_saved() else {
            return;
        };
        let room_id = item.room_id.clone();
        if self.saved.store.remove(&room_id, &item.event_id) {
            self.show_toast("Removed from the saved messages");
        } else if let Err(full) = self.saved.store.add(item) {
            self.show_toast(full.message());
            return;
        } else {
            let hint = match self.keymap.label_for(Action::OpenSaved) {
                Some(key) => format!("Saved for later ({key} shows the list)"),
                None => "Saved for later".to_owned(),
            };
            self.show_toast(hint);
        }
        self.persist_saved();
        self.sync_saved_flags(&room_id);
    }

    /// Marks the saved messages among those loaded of a room, and among those put
    /// aside while a thread of it is open. Called when the room's messages arrive and
    /// when the list changes.
    pub(super) fn sync_saved_flags(&mut self, room_id: &str) {
        let ids: Vec<String> = self
            .saved
            .store
            .in_room(room_id)
            .map(str::to_owned)
            .collect();
        let saved: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let demo = self.matrix.is_none();
        let open_here = self.room_id().as_deref() == Some(room_id);
        let thread_here = open_here && self.threads.open.is_some();
        let mut changed = false;
        if let Some(room) = self.rooms.iter_mut().find(|room| room.id == room_id) {
            // With a thread open, the room's list holds the thread's messages.
            changed = flag(&mut room.messages, &saved, demo && !thread_here);
        }
        if thread_here && let Some(thread) = &mut self.threads.open {
            thread.apply_saved(&saved, demo);
        }
        if changed && open_here {
            self.timeline_rev += 1;
        }
    }

    // ----- the popup -----------------------------------------------------------------

    /// The saved messages of all rooms. A due reminder is selected, if there is one.
    pub(super) fn open_saved(&mut self) {
        let now = self.saved.clock.now_secs();
        let mut popup = SavedPopup::list();
        popup.selected = self
            .saved
            .store
            .items()
            .iter()
            .position(|item| matches!(item.reminder(now), Reminder::Due(_)))
            .unwrap_or(0);
        self.open_modal(Modal::Saved(popup));
        self.saved_fetch_visible();
    }

    /// The reminder form for the selected message. A message that is not saved yet is
    /// saved once a time is chosen.
    pub(super) fn open_reminder_for_selected(&mut self) {
        let Some(item) = self.selected_as_saved() else {
            return;
        };
        let known = self.saved.store.get(&item.room_id, &item.event_id);
        let has_reminder = known.is_some_and(|known| known.remind_at.is_some());
        let unsaved = known.is_none();
        if unsaved && self.saved.store.len() >= crate::saved::MAX_SAVED {
            self.show_toast(crate::saved::Full.message());
            return;
        }
        let mut form = RemindForm::new(item.room_id.clone(), item.event_id.clone(), has_reminder);
        form.unsaved = unsaved.then_some(item);
        self.open_modal(Modal::Saved(SavedPopup::form(form)));
    }

    /// Asks the worker about the rows on screen that it has not told about yet (the
    /// demo looks at its own messages).
    pub(super) fn saved_fetch_visible(&mut self) {
        let Some(Modal::Saved(popup)) = &self.modal else {
            return;
        };
        if !matches!(popup.stage, Stage::List) {
            return;
        }
        let items = self.saved.store.items();
        let shown = if popup.page == 0 {
            FETCH_FIRST
        } else {
            popup.page + FETCH_AHEAD
        };
        let rows = popup.scroll..popup.scroll + shown;
        // The selected row may not be in view yet: the next draw brings it there.
        let selected = Some(popup.selected).filter(|index| !rows.contains(index));
        let wanted: Vec<SavedItem> = rows
            .chain(selected)
            .filter_map(|index| items.get(index))
            .filter(|item| popup.details_of(&item.room_id, &item.event_id).is_none())
            .filter(|item| {
                !popup
                    .asked
                    .contains(&(item.room_id.clone(), item.event_id.clone()))
            })
            .take(SAVED_MAX_FETCH)
            .cloned()
            .collect();
        if wanted.is_empty() {
            return;
        }
        if self.matrix.is_none() {
            let found: Vec<SavedDetails> = wanted
                .iter()
                .map(|item| self.demo_saved_details(item))
                .collect();
            if let Some(Modal::Saved(popup)) = &mut self.modal {
                for details in found {
                    popup.put_details(details);
                }
            }
            return;
        }
        // Before the session is up there is nobody to ask; the tick asks again.
        if self.user_id.is_none() || self.logged_out {
            return;
        }
        let keys: Vec<(String, String)> = wanted
            .into_iter()
            .map(|item| (item.room_id, item.event_id))
            .collect();
        if let Some(Modal::Saved(popup)) = &mut self.modal {
            popup.asked.extend(keys.iter().cloned());
        }
        self.saved.next_request += 1;
        self.send_matrix(MatrixCmd::Saved(SavedCmd::Fetch {
            request: self.saved.next_request,
            items: keys,
        }));
    }

    /// What the demo knows about a saved message: it is among the room's messages.
    fn demo_saved_details(&self, item: &SavedItem) -> SavedDetails {
        let mut details = SavedDetails {
            room_id: item.room_id.clone(),
            event_id: item.event_id.clone(),
            sender: String::new(),
            sender_id: None,
            snippet: String::new(),
            text: SavedText::Unavailable,
        };
        let Some(room) = self.rooms.iter().find(|room| room.id == item.room_id) else {
            details.text = SavedText::RoomGone;
            return details;
        };
        let found = find_target_message(&room.messages, &item.event_id, true, false)
            .and_then(|index| room.messages.get(index));
        if let Some(message) = found {
            details.sender = message.sender.clone();
            details.sender_id = message.sender_id.clone();
            if message.kind == MsgKind::Redacted {
                details.text = SavedText::Redacted;
            } else {
                details.snippet = truncate(sanitize_line(&message.body).trim(), SNIPPET_MAX);
                details.text = SavedText::Text;
            }
        }
        hide_ignored(details, &self.ignored_users)
    }

    pub(super) fn on_saved_event(&mut self, event: SavedEvent) {
        let SavedEvent::Details { request, items } = event;
        // For a reminder that waited for its message's text.
        if let Some(at) = self
            .saved
            .waiting
            .iter()
            .position(|(asked, ..)| *asked == request)
        {
            let (_, item, _) = self.saved.waiting.remove(at);
            let details = items
                .into_iter()
                .find(|found| item.is(&found.room_id, &found.event_id))
                .map(|found| hide_ignored(found, &self.ignored_users));
            self.notify_reminder(&item, details.as_ref());
            return;
        }
        let ignored = &self.ignored_users;
        if let Some(Modal::Saved(popup)) = &mut self.modal {
            for details in items {
                popup.put_details(hide_ignored(details, ignored));
            }
        }
    }

    pub(super) fn on_saved_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let len = self.saved.store.len();
        let Some(Modal::Saved(popup)) = &mut self.modal else {
            return;
        };
        if let Stage::Remind(form) = &mut popup.stage {
            let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match code {
                KeyCode::Esc => self.saved_escape(),
                KeyCode::Enter => self.accept_reminder(),
                KeyCode::Up | KeyCode::BackTab => form.step(-1),
                KeyCode::Down | KeyCode::Tab => form.step(1),
                // Typing anywhere in the form types a time.
                KeyCode::Char(_) if plain && form.selected != RemindForm::TYPED => {
                    form.select(RemindForm::TYPED);
                    form.input.handle_key(code, mods, EditKeys::FULL);
                }
                _ if form.selected == RemindForm::TYPED => {
                    if form.input.handle_key(code, mods, EditKeys::FULL).changed() {
                        form.error = None;
                    }
                }
                _ => {}
            }
            return;
        }
        let page = popup.page.max(1) as isize;
        match (code, mods) {
            (KeyCode::Esc, _) => self.modal = None,
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => popup.step(-1, len),
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => popup.step(1, len),
            (KeyCode::PageUp, _) => popup.step(-page, len),
            (KeyCode::PageDown, _) => popup.step(page, len),
            (KeyCode::Home, _) => popup.step(-(len as isize), len),
            (KeyCode::End, _) => popup.step(len as isize, len),
            (KeyCode::Enter, _) => self.activate_saved(None),
            (KeyCode::Delete, _) => self.remove_saved(None),
            (KeyCode::Char('r'), KeyModifiers::NONE) => self.open_saved_form(None),
            _ => {}
        }
        self.saved_fetch_visible();
    }

    pub(super) fn on_saved_paste(&mut self, first_line: &str) {
        if let Some(Modal::Saved(popup)) = &mut self.modal
            && let Stage::Remind(form) = &mut popup.stage
        {
            form.select(RemindForm::TYPED);
            form.input.paste(first_line);
        }
    }

    pub(super) fn click_saved_input(&mut self, column: usize) {
        if let Some(Modal::Saved(popup)) = &mut self.modal
            && let Stage::Remind(form) = &mut popup.stage
        {
            form.select(RemindForm::TYPED);
            form.input.place(column, false);
        }
    }

    pub(super) fn scroll_saved(&mut self, delta: isize) {
        if let Some(Modal::Saved(popup)) = &mut self.modal {
            match &mut popup.stage {
                Stage::List => popup.scroll_by(delta.signum()),
                Stage::Remind(form) => form.step(delta.signum()),
            }
        }
        self.saved_fetch_visible();
    }

    /// Esc, or a click beside the popup: from the form back to the list it came from,
    /// else closed.
    pub(super) fn saved_escape(&mut self) {
        if let Some(Modal::Saved(popup)) = &mut self.modal
            && matches!(popup.stage, Stage::Remind(_))
            && !popup.form_only
        {
            popup.stage = Stage::List;
            return;
        }
        self.modal = None;
    }

    /// The entry at `index` of the list, or the selected one (which a click also
    /// selects).
    fn saved_at(&mut self, index: Option<usize>) -> Option<SavedItem> {
        let len = self.saved.store.len();
        let Some(Modal::Saved(popup)) = &mut self.modal else {
            return None;
        };
        if let Some(index) = index {
            popup.select(index, len);
        }
        self.saved.store.items().get(popup.selected).cloned()
    }

    /// Enter or a click on a row: goes to the message.
    pub(super) fn activate_saved(&mut self, index: Option<usize>) {
        let Some(item) = self.saved_at(index) else {
            return;
        };
        self.modal = None;
        self.go_to_saved(&item);
    }

    /// Del or a click on "remove": the message is saved no more.
    pub(super) fn remove_saved(&mut self, index: Option<usize>) {
        let Some(item) = self.saved_at(index) else {
            return;
        };
        if self.saved.store.remove(&item.room_id, &item.event_id) {
            self.persist_saved();
            self.sync_saved_flags(&item.room_id);
            self.show_toast("Removed from the saved messages");
        }
        self.saved_fetch_visible();
    }

    /// `r` or a click on "remind": the reminder form for that entry.
    pub(super) fn open_saved_form(&mut self, index: Option<usize>) {
        let Some(item) = self.saved_at(index) else {
            return;
        };
        if let Some(Modal::Saved(popup)) = &mut self.modal {
            popup.stage = Stage::Remind(RemindForm::new(
                item.room_id,
                item.event_id,
                item.remind_at.is_some(),
            ));
        }
    }

    /// A click on a row of the form: a fixed choice (or "no reminder") is taken at
    /// once; the typed time gets the cursor.
    pub(super) fn click_reminder_choice(&mut self, row: usize) {
        if let Some(Modal::Saved(popup)) = &mut self.modal
            && let Stage::Remind(form) = &mut popup.stage
        {
            form.select(row);
            if form.selected == row && row != RemindForm::TYPED {
                self.accept_reminder();
            }
        }
    }

    /// Enter in the form: sets the reminder to the chosen or typed time, or takes it
    /// away. A time that is not understood stays in the field, with the reason.
    fn accept_reminder(&mut self) {
        let clock = self.saved.clock;
        let now = clock.now_local();
        let Some(Modal::Saved(popup)) = &mut self.modal else {
            return;
        };
        let Stage::Remind(form) = &mut popup.stage else {
            return;
        };
        let at = if form.selected == RemindForm::CLEAR {
            None
        } else {
            let local = match Preset::ALL.get(form.selected) {
                Some(preset) => preset.at(now).ok_or(crate::saved::WhenError::TooFar),
                None => parse_when(form.input.as_str(), now),
            };
            let local = match local {
                Ok(local) => local,
                Err(why) => {
                    form.error = Some(why.message());
                    return;
                }
            };
            let Some(secs) = clock.secs_of(local) else {
                form.error =
                    Some("That time does not exist on that day (the clocks change)".to_owned());
                return;
            };
            Some((secs, local))
        };
        let (room_id, event_id) = (form.room_id.clone(), form.event_id.clone());
        if let Some(item) = form.unsaved.take()
            && let Err(full) = self.saved.store.add(item)
        {
            form.error = Some(full.message());
            return;
        }
        self.saved
            .store
            .set_reminder(&room_id, &event_id, at.map(|(secs, _)| secs));
        self.saved_escape();
        self.persist_saved();
        self.sync_saved_flags(&room_id);
        self.show_toast(match at {
            Some((_, local)) => format!("Reminder set for {}", when_label(local, now)),
            None => "Reminder removed".to_owned(),
        });
    }

    // ----- going to a saved message --------------------------------------------------

    /// Goes to the message, in its room (and thread). A reminder that was due is done
    /// with that.
    fn go_to_saved(&mut self, item: &SavedItem) {
        let now = self.saved.clock.now_secs();
        if self.saved.store.visited(&item.room_id, &item.event_id, now) {
            self.persist_saved();
        }
        self.saved.clicks.remove(&item.room_id);
        self.jump_to_event(
            item.room_id.clone(),
            item.event_id.clone(),
            item.thread.clone(),
        );
    }

    /// A click on "reminder due" in the status line: with one due, to its message;
    /// with several, to the list.
    pub(super) fn click_due_reminder(&mut self) {
        let now = self.saved.clock.now_secs();
        let mut due = self.saved.store.due(now);
        match (due.next().cloned(), due.next().is_some()) {
            (Some(item), false) => {
                self.modal = None;
                self.go_to_saved(&item);
            }
            (Some(_), true) => self.open_saved(),
            (None, _) => {}
        }
    }

    /// A click on the desktop notification of `room_id`: true if that was a reminder,
    /// and its message is where we went.
    pub(super) fn reminder_clicked(&mut self, room_id: &str) -> bool {
        let Some(item) = self.saved.clicks.remove(room_id) else {
            return false;
        };
        if !self.saved.store.contains(&item.room_id, &item.event_id) {
            return false;
        }
        self.go_to_saved(&item);
        true
    }

    // ----- reminders coming due ------------------------------------------------------

    /// Called a few times a second: tells of the reminders that came due, sends out
    /// the notifications that waited `wait` for their text in vain, and fills the
    /// rows of the open list.
    pub(super) fn saved_tick(&mut self, wait: Duration) {
        let now = self.saved.clock.now_secs();
        for item in self.saved.store.take_newly_due(now) {
            self.reminder_due(item);
        }
        if !self.saved.waiting.is_empty() {
            let (late, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.saved.waiting)
                .into_iter()
                .partition(|(_, _, since)| since.elapsed() >= wait);
            self.saved.waiting = waiting;
            for (_, item, _) in late {
                self.notify_reminder(&item, None);
            }
        }
        self.saved_fetch_visible();
    }

    /// A reminder came due: said in the app at once, and on the desktop unless
    /// notifications are off or held back.
    fn reminder_due(&mut self, item: SavedItem) {
        self.dirty = true;
        self.show_toast("A reminder is due");
        if !self.reminders_notify() {
            return;
        }
        // With previews off nothing of the message is shown, so nothing is asked.
        if self.notification_preview {
            if self.matrix.is_none() {
                let details = self.demo_saved_details(&item);
                self.notify_reminder(&item, Some(&details));
                return;
            }
            if self.user_id.is_some() {
                self.saved.next_request += 1;
                let request = self.saved.next_request;
                self.send_matrix(MatrixCmd::Saved(SavedCmd::Fetch {
                    request,
                    items: vec![(item.room_id.clone(), item.event_id.clone())],
                }));
                self.saved.waiting.push((request, item, Instant::now()));
                return;
            }
        }
        self.notify_reminder(&item, None);
    }

    fn reminders_notify(&self) -> bool {
        self.notify_mode != crate::notify::NotifyMode::Off
            && !self.do_not_disturb
            && !self.logged_out
    }

    /// Hands the reminder's notification to the event loop, with the message's text
    /// where it is known and may be shown.
    fn notify_reminder(&mut self, item: &SavedItem, details: Option<&SavedDetails>) {
        // Do not disturb may have been turned on while the text was waited for.
        if !self.reminders_notify() {
            return;
        }
        let room = match self.saved_room_label(&item.room_id) {
            RoomLabel::Name(name) => name,
            RoomLabel::Unknown | RoomLabel::Gone => String::new(),
        };
        let (sender, body) = match details {
            Some(found) if found.text == SavedText::Text && self.notification_preview => {
                (found.sender.clone(), found.snippet.clone())
            }
            _ => (String::new(), String::new()),
        };
        self.saved.notices.push(Notice {
            room_id: item.room_id.clone(),
            room,
            sender,
            body,
            highlight: false,
            thread: item.thread.clone(),
        });
        self.saved
            .clicks
            .insert(item.room_id.clone(), item.clone());
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Pane;
    use crate::config::Config;
    use crate::matrix::{MatrixEvent, MatrixHandle};
    use crate::model::{ItemRef, Room, RoomKind};
    use crate::notify::NotifyMode;
    use crate::saved::Clock;
    use crate::saved::tests::friday;
    use crate::ui;

    const ROOM: &str = "!garden:example.org";

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(col: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn find(text: &str, needle: &str) -> (u16, u16) {
        for (row, line) in text.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                // The backend prints a quote before each row.
                let col = crate::text::width(&line[..byte]).saturating_sub(1);
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{text}");
    }

    fn toast(app: &App) -> &str {
        app.toast
            .as_ref()
            .map(|(text, _)| text.as_str())
            .unwrap_or("")
    }

    fn message(n: usize, sender: &str, body: &str) -> Message {
        Message {
            id: format!("m{n}"),
            item: Some(ItemRef::Event(format!("$e{n}"))),
            sender: sender.to_owned(),
            sender_id: Some(format!("@{}:example.org", sender.to_lowercase())),
            body: body.to_owned(),
            ..Message::default()
        }
    }

    /// Logged in through a fake worker, with one room of three messages open, on an
    /// 80x24 screen; the clock stands at Friday 14:30.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.saved.clock = Clock::Fixed(friday());
        let mut room = Room::new(ROOM, "Garden", RoomKind::Room);
        room.messages = vec![
            message(0, "Ann", "water the tomatoes"),
            message(1, "Bob", "the shed key is under the pot"),
            message(2, "Ann", "see you on Sunday"),
        ];
        app.rooms = vec![room, Room::new("!other:example.org", "Other", RoomKind::Room)];
        app.active = 0;
        app.rooms_received = true;
        app.notify_mode = NotifyMode::Desktop;
        app.focus = Pane::Timeline;
        app.timeline.selected = Some(1);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        screen(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    fn fetches(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<(u64, Vec<(String, String)>)> {
        let mut out = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::Saved(SavedCmd::Fetch { request, items }) = command {
                out.push((request, items));
            }
        }
        out
    }

    fn text_of(event: &str, sender: &str, snippet: &str) -> SavedDetails {
        SavedDetails {
            room_id: ROOM.into(),
            event_id: event.into(),
            sender: sender.into(),
            sender_id: Some(format!("@{}:example.org", sender.to_lowercase())),
            snippet: snippet.into(),
            text: SavedText::Text,
        }
    }

    fn answer(app: &mut App, request: u64, items: Vec<SavedDetails>) {
        app.on_matrix(MatrixEvent::Saved(SavedEvent::Details { request, items }));
    }

    fn popup(app: &App) -> &SavedPopup {
        match &app.modal {
            Some(Modal::Saved(popup)) => popup,
            other => panic!("no saved popup: {other:?}"),
        }
    }

    fn form(app: &App) -> &RemindForm {
        match &popup(app).stage {
            Stage::Remind(form) => form,
            Stage::List => panic!("the list, not the form"),
        }
    }

    /// Moves the clock on by `secs`.
    fn later(app: &mut App, secs: i64) {
        app.saved.clock = Clock::Fixed(friday() + chrono::Duration::seconds(secs));
    }

    fn saved_ids(app: &App) -> Vec<&str> {
        app.saved
            .store
            .items()
            .iter()
            .map(|item| item.event_id.as_str())
            .collect()
    }

    #[test]
    fn the_key_saves_the_selected_message_and_marks_it_and_again_unsaves_it() {
        let (mut app, mut terminal, _commands) = connected();
        app.on_event(key(KeyCode::Char('b')));
        assert_eq!(saved_ids(&app), ["$e1"]);
        assert_eq!(app.saved.store.items()[0].room_id, ROOM);
        assert_eq!(
            app.saved.store.items()[0].saved_at,
            friday().timestamp()
        );
        assert!(toast(&app).contains("Saved for later (B"), "{}", toast(&app));
        assert!(app.rooms[0].messages[1].saved);
        assert!(!app.rooms[0].messages[0].saved);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("under the pot (saved)"), "{text}");

        app.on_event(key(KeyCode::Char('b')));
        assert!(saved_ids(&app).is_empty());
        assert!(toast(&app).contains("Removed"), "{}", toast(&app));
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("(saved)"), "{text}");
    }

    #[test]
    fn the_mark_comes_back_when_the_messages_of_the_room_arrive_again() {
        let (mut app, _terminal, _commands) = connected();
        app.perform(Action::SaveForLater);
        let fresh = vec![
            message(0, "Ann", "water the tomatoes"),
            message(1, "Bob", "the shed key is under the pot"),
        ];
        app.rooms[0].messages = fresh;
        assert!(!app.rooms[0].messages[1].saved);
        app.sync_saved_flags(ROOM);
        assert!(app.rooms[0].messages[1].saved);
    }

    #[test]
    fn what_is_no_sent_message_cannot_be_saved() {
        let (mut app, _terminal, _commands) = connected();
        app.rooms[0].messages[1].state = SendState::Sending;
        app.perform(Action::SaveForLater);
        assert!(saved_ids(&app).is_empty());
        assert!(toast(&app).contains("cannot be saved"), "{}", toast(&app));
        app.rooms[0].messages[1].state = SendState::Sent;
        app.rooms[0].messages[1].kind = MsgKind::Redacted;
        app.perform(Action::SaveForLater);
        assert!(saved_ids(&app).is_empty());
        app.timeline.selected = None;
        app.perform(Action::SaveForLater);
        assert!(toast(&app).contains("No message"), "{}", toast(&app));
    }

    #[test]
    fn the_menu_and_the_palette_offer_saving_and_the_reminder() {
        let (mut app, mut terminal, _commands) = connected();
        let labels = |app: &App| -> Vec<String> {
            match &app.modal {
                Some(Modal::Menu(menu)) => menu.items.iter().map(|it| it.label.clone()).collect(),
                other => panic!("no menu: {other:?}"),
            }
        };
        app.perform(Action::ContextMenu);
        let offered = labels(&app);
        assert!(offered.contains(&"Save for later".to_owned()), "{offered:?}");
        assert!(offered.contains(&"Remind me…".to_owned()), "{offered:?}");
        // Chosen with the keyboard: down to the entry, Enter.
        let at = offered.iter().position(|l| l == "Save for later").unwrap();
        for _ in 0..at {
            app.on_event(key(KeyCode::Down));
        }
        app.on_event(key(KeyCode::Enter));
        screen(&mut app, &mut terminal);
        assert_eq!(saved_ids(&app), ["$e1"]);
        // Saved: the menu offers to take it off the list.
        app.perform(Action::ContextMenu);
        let offered = labels(&app);
        assert!(offered.contains(&"Remove from saved".to_owned()), "{offered:?}");
        app.modal = None;

        let entries = crate::palette::Palette::open(app.palette_context()).entries;
        let titled = |title: &str| {
            entries
                .iter()
                .find(|entry| entry.title == title)
                .unwrap_or_else(|| panic!("no palette entry {title:?}"))
        };
        assert!(titled("Remove message from saved").enabled);
        assert!(titled("Remind me of this message…").enabled);
        assert!(titled("Saved messages and reminders").enabled);
        // Without a chat the list is still there; the message entries are not.
        app.timeline.selected = None;
        let entries = crate::palette::Palette::open(app.palette_context()).entries;
        let entry = |title: &str| entries.iter().find(|entry| entry.title == title).unwrap();
        assert!(!entry("Save message for later").enabled);
        assert!(entry("Saved messages and reminders").enabled);
    }

    #[test]
    fn the_list_shows_room_sender_text_and_times_newest_saved_first() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::SaveForLater);
        later(&mut app, 60);
        app.timeline.selected = Some(2);
        app.perform(Action::SaveForLater);
        app.saved.store.set_reminder(ROOM, "$e1", Some(friday().timestamp() + 7200));
        assert_eq!(saved_ids(&app), ["$e2", "$e1"]);

        app.on_event(key(KeyCode::Char('B')));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Saved messages"), "{text}");
        // Nothing of the messages is kept by the app: the worker is asked.
        let asked = fetches(&mut commands);
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].1,
            [
                (ROOM.to_owned(), "$e2".to_owned()),
                (ROOM.to_owned(), "$e1".to_owned())
            ]
        );
        assert!(text.contains("Garden"), "{text}");
        assert!(text.contains("saved today 14:31"), "{text}");
        assert!(text.contains("saved today 14:30"), "{text}");
        assert!(text.contains("today 16:30"), "{text}");
        assert!(popup(&app).details.is_empty());
        // Asked once, however often it is drawn or ticked.
        app.saved_tick(TEXT_WAIT);
        assert!(fetches(&mut commands).is_empty());

        answer(
            &mut app,
            asked[0].0,
            vec![
                text_of("$e2", "Ann", "see you on Sunday"),
                text_of("$e1", "Bob", "the shed key is under the pot"),
            ],
        );
        let text = screen(&mut app, &mut terminal);
        let first = find(&text, "see you on Sunday").1;
        let second = find(&text, "the shed key is under the pot").1;
        assert!(first < second, "{text}");
        assert!(text.contains("Garden · Ann"), "{text}");
        assert!(text.contains("Garden · Bob"), "{text}");
    }

    #[test]
    fn enter_goes_to_the_message_through_the_jump_to_a_message_that_is_not_loaded() {
        let (mut app, mut terminal, mut commands) = connected();
        app.saved
            .store
            .add(SavedItem {
                room_id: "!other:example.org".into(),
                event_id: "$far".into(),
                thread: Some("$root".into()),
                saved_at: 5,
                remind_at: None,
                done: false,
            })
            .unwrap();
        app.perform(Action::OpenSaved);
        screen(&mut app, &mut terminal);
        while commands.try_recv().is_ok() {}
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let mut opened = Vec::new();
        while let Ok(command) = commands.try_recv() {
            opened.push(command.describe());
        }
        assert!(
            opened
                .iter()
                .any(|shown| shown.starts_with("OpenEvent(!other:example.org")),
            "{opened:?}"
        );
        let pending = app.pending_jump.as_ref().expect("a jump under way");
        assert_eq!(pending.event_id, "$far");
        assert_eq!(pending.thread_root.as_deref(), Some("$root"));
    }

    #[test]
    fn a_click_on_a_row_goes_to_a_loaded_message_at_once() {
        let (mut app, mut terminal, mut commands) = connected();
        app.timeline.selected = Some(0);
        app.perform(Action::SaveForLater);
        app.timeline.selected = Some(2);
        app.perform(Action::OpenSaved);
        let asked = fetches(&mut commands);
        answer(
            &mut app,
            asked[0].0,
            vec![text_of("$e0", "Ann", "water the tomatoes")],
        );
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "water the tomatoes");
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
        assert_eq!(app.timeline.selected, Some(0));
        assert_eq!(app.focus, Pane::Timeline);
    }

    #[test]
    fn del_and_the_remove_mark_take_a_message_off_the_list() {
        let (mut app, mut terminal, _commands) = connected();
        for index in 0..3 {
            app.timeline.selected = Some(index);
            app.perform(Action::SaveForLater);
        }
        app.perform(Action::OpenSaved);
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(saved_ids(&app), ["$e2", "$e0"]);
        assert!(!app.rooms[0].messages[1].saved);
        assert!(matches!(app.modal, Some(Modal::Saved(_))));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, " remove ");
        app.on_event(click(col + 1, row));
        assert_eq!(saved_ids(&app), ["$e0"]);
        app.on_event(key(KeyCode::Delete));
        assert!(saved_ids(&app).is_empty());
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("No saved messages"), "{text}");
        // Nothing to act on: nothing happens.
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Char('r')));
        assert!(matches!(popup(&app).stage, Stage::List));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_room_that_was_left_and_a_deleted_message_are_shown_as_such_and_can_be_removed() {
        let (mut app, mut terminal, mut commands) = connected();
        for (room, event) in [("!left:example.org", "$gone"), (ROOM, "$deleted")] {
            app.saved
                .store
                .add(SavedItem {
                    room_id: room.into(),
                    event_id: event.into(),
                    thread: None,
                    saved_at: 5,
                    remind_at: None,
                    done: false,
                })
                .unwrap();
        }
        app.perform(Action::OpenSaved);
        let asked = fetches(&mut commands);
        let mut deleted = text_of("$deleted", "Bob", "");
        deleted.text = SavedText::Redacted;
        let mut gone = text_of("$gone", "", "");
        gone.room_id = "!left:example.org".into();
        gone.sender_id = None;
        gone.text = SavedText::RoomGone;
        answer(&mut app, asked[0].0, vec![deleted, gone]);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("message deleted"), "{text}");
        assert!(text.contains("a room you left"), "{text}");
        // Going there says why not, and the entry stays to be removed.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(toast(&app).contains("no longer available"), "{}", toast(&app));
        app.perform(Action::OpenSaved);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert!(saved_ids(&app).is_empty());
    }

    #[test]
    fn a_message_of_an_ignored_user_shows_nothing_of_them() {
        let (mut app, mut terminal, mut commands) = connected();
        app.ignored_users.insert("@bob:example.org".into());
        app.perform(Action::SaveForLater);
        app.perform(Action::OpenSaved);
        let asked = fetches(&mut commands);
        answer(
            &mut app,
            asked[0].0,
            vec![text_of("$e1", "Bob", "the shed key is under the pot")],
        );
        let text = screen(&mut app, &mut terminal);
        // The popup covers the room's messages.
        assert!(text.contains("Ignored user"), "{text}");
        assert!(!text.contains("Garden · Bob"), "{text}");
    }

    #[test]
    fn a_full_list_says_so_and_takes_no_more() {
        let (mut app, _terminal, _commands) = connected();
        for n in 0..crate::saved::MAX_SAVED {
            app.saved
                .store
                .add(SavedItem {
                    room_id: ROOM.into(),
                    event_id: format!("$x{n}"),
                    thread: None,
                    saved_at: 1,
                    remind_at: None,
                    done: false,
                })
                .unwrap();
        }
        app.perform(Action::SaveForLater);
        assert!(toast(&app).contains("full (500"), "{}", toast(&app));
        assert!(!app.saved.store.contains(ROOM, "$e1"));
        app.perform(Action::RemindMe);
        assert!(app.modal.is_none());
        assert!(toast(&app).contains("full (500"), "{}", toast(&app));
    }

    #[test]
    fn a_fixed_choice_sets_the_reminder_and_saves_the_message_with_it() {
        let (mut app, mut terminal, _commands) = connected();
        app.perform(Action::RemindMe);
        let text = screen(&mut app, &mut terminal);
        for choice in Preset::ALL {
            assert!(text.contains(choice.label()), "{text}");
        }
        assert!(!text.contains("no reminder"), "{text}");
        // Nothing is saved before a time is chosen.
        assert!(saved_ids(&app).is_empty());
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(saved_ids(&app), ["$e1"]);
        assert_eq!(
            app.saved.store.items()[0].remind_at,
            Some(friday().timestamp() + 3600)
        );
        assert_eq!(toast(&app), "Reminder set for today 15:30");
        assert!(app.rooms[0].messages[1].saved);
    }

    #[test]
    fn leaving_the_form_saves_nothing() {
        let (mut app, _terminal, _commands) = connected();
        app.perform(Action::RemindMe);
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(saved_ids(&app).is_empty());
    }

    #[test]
    fn a_typed_time_is_taken_and_one_that_is_not_understood_stays_with_its_reason() {
        let (mut app, mut terminal, _commands) = connected();
        app.perform(Action::RemindMe);
        // Typing anywhere goes to the field.
        type_text(&mut app, "soon");
        assert_eq!(form(&app).selected, RemindForm::TYPED);
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Not understood"), "{text}");
        assert!(saved_ids(&app).is_empty());
        for _ in 0..4 {
            app.on_event(key(KeyCode::Backspace));
        }
        assert_eq!(form(&app).error, None);
        type_text(&mut app, "tomorrow 8:15");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let at = friday().timestamp() + (17 * 60 + 45) * 60;
        assert_eq!(app.saved.store.items()[0].remind_at, Some(at));
        assert_eq!(toast(&app), "Reminder set for tomorrow 08:15");
    }

    #[test]
    fn a_time_can_be_pasted_and_a_click_chooses() {
        let (mut app, mut terminal, _commands) = connected();
        app.perform(Action::RemindMe);
        app.on_event(Event::Paste("2h\nmore".into()));
        assert_eq!(form(&app).input.as_str(), "2h");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            app.saved.store.items()[0].remind_at,
            Some(friday().timestamp() + 7200)
        );
        // From the list: the form has a row that takes the reminder away.
        app.perform(Action::OpenSaved);
        app.on_event(key(KeyCode::Char('r')));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "in 3 hours");
        app.on_event(click(col + 1, row));
        assert!(matches!(popup(&app).stage, Stage::List));
        assert_eq!(
            app.saved.store.items()[0].remind_at,
            Some(friday().timestamp() + 3 * 3600)
        );
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, " remind ");
        app.on_event(click(col + 1, row));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "no reminder");
        app.on_event(click(col + 1, row));
        assert_eq!(app.saved.store.items()[0].remind_at, None);
        assert_eq!(toast(&app), "Reminder removed");
        // Esc in the form goes back to the list, then closes.
        app.on_event(key(KeyCode::Char('r')));
        assert!(matches!(popup(&app).stage, Stage::Remind(_)));
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(popup(&app).stage, Stage::List));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    /// The selected message saved, with a reminder in an hour.
    fn with_reminder() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (mut app, terminal, mut commands) = connected();
        app.perform(Action::SaveForLater);
        app.saved
            .store
            .set_reminder(ROOM, "$e1", Some(friday().timestamp() + 3600));
        app.toast = None;
        while commands.try_recv().is_ok() {}
        (app, terminal, commands)
    }

    #[test]
    fn a_due_reminder_notifies_with_the_text_and_shows_in_the_status_line() {
        let (mut app, mut terminal, mut commands) = with_reminder();
        later(&mut app, 3599);
        app.saved_tick(TEXT_WAIT);
        assert!(fetches(&mut commands).is_empty());
        assert_eq!(app.reminders_due(), 0);

        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        assert_eq!(toast(&app), "A reminder is due");
        assert_eq!(app.reminders_due(), 1);
        // The text is asked for; the notification waits for it.
        let asked = fetches(&mut commands);
        assert_eq!(asked[0].1, [(ROOM.to_owned(), "$e1".to_owned())]);
        assert!(app.saved.notices.is_empty());
        answer(
            &mut app,
            asked[0].0,
            vec![text_of("$e1", "Bob", "the shed key is under the pot")],
        );
        assert_eq!(app.saved.notices.len(), 1);
        let (title, text) = crate::notify::reminder_title_and_text(&app.saved.notices[0], true);
        assert_eq!(title, "Reminder: Garden");
        assert_eq!(text, "Bob: the shed key is under the pot");
        // Once.
        app.saved_tick(TEXT_WAIT);
        assert_eq!(app.saved.notices.len(), 1);
        assert!(fetches(&mut commands).is_empty());

        app.toast = None;
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("1 reminder due"), "{text}");
        // In the list it is marked due.
        app.perform(Action::OpenSaved);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("due since today 15:30"), "{text}");
    }

    #[test]
    fn with_previews_off_the_notification_holds_nothing_of_the_message() {
        let (mut app, _terminal, mut commands) = with_reminder();
        app.notification_preview = false;
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        // Not even asked for.
        assert!(fetches(&mut commands).is_empty());
        assert_eq!(app.saved.notices.len(), 1);
        let notice = &app.saved.notices[0];
        assert!(notice.body.is_empty() && notice.sender.is_empty());
        let ready = app.take_notices_with_icons();
        assert!(!ready.preview);
        assert_eq!(ready.reminders.len(), 1);
        let (title, text) = crate::notify::reminder_title_and_text(&ready.reminders[0].0, false);
        assert_eq!((title.as_str(), text.as_str()), ("matrix-rat", "A reminder is due"));
        assert!(app.saved.notices.is_empty());
    }

    #[test]
    fn do_not_disturb_and_notifications_off_hold_the_notification_back_but_not_the_entry() {
        for quiet in [true, false] {
            let (mut app, mut terminal, mut commands) = with_reminder();
            if quiet {
                app.do_not_disturb = true;
            } else {
                app.notify_mode = NotifyMode::Off;
            }
            later(&mut app, 4000);
            app.saved_tick(TEXT_WAIT);
            assert!(fetches(&mut commands).is_empty());
            assert!(app.saved.notices.is_empty());
            assert_eq!(app.reminders_due(), 1);
            app.toast = None;
            let text = screen(&mut app, &mut terminal);
            assert!(text.contains("1 reminder due"), "{text}");
        }
    }

    #[test]
    fn do_not_disturb_turned_on_while_the_text_is_awaited_holds_the_notification_back() {
        let (mut app, _terminal, mut commands) = with_reminder();
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        let asked = fetches(&mut commands);
        app.do_not_disturb = true;
        answer(&mut app, asked[0].0, vec![text_of("$e1", "Bob", "text")]);
        assert!(app.saved.notices.is_empty());
    }

    #[test]
    fn a_text_that_does_not_come_does_not_hold_the_notification_back_for_ever() {
        let (mut app, _terminal, mut commands) = with_reminder();
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        assert_eq!(fetches(&mut commands).len(), 1);
        assert!(app.saved.notices.is_empty());
        // The wait is over.
        app.saved_tick(Duration::ZERO);
        assert_eq!(app.saved.notices.len(), 1);
        assert!(app.saved.notices[0].body.is_empty());
        let (title, text) = crate::notify::reminder_title_and_text(&app.saved.notices[0], true);
        assert_eq!(title, "Reminder: Garden");
        assert_eq!(text, "A saved message is due");
        // A late answer does not notify again.
        answer(&mut app, 1, vec![text_of("$e1", "Bob", "late")]);
        assert_eq!(app.saved.notices.len(), 1);
    }

    #[test]
    fn a_message_that_cannot_be_read_gives_no_text_to_the_notification() {
        let (mut app, _terminal, mut commands) = with_reminder();
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        let asked = fetches(&mut commands);
        let mut deleted = text_of("$e1", "Bob", "[message deleted]");
        deleted.text = SavedText::Redacted;
        answer(&mut app, asked[0].0, vec![deleted]);
        assert!(app.saved.notices[0].body.is_empty());
        assert!(app.saved.notices[0].sender.is_empty());
    }

    #[test]
    fn a_click_on_the_status_line_goes_to_the_message_and_the_reminder_is_done() {
        let (mut app, mut terminal, _commands) = with_reminder();
        app.timeline.selected = Some(2);
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        app.toast = None;
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "1 reminder due");
        app.on_event(click(col + 2, row));
        assert_eq!(app.timeline.selected, Some(1));
        assert_eq!(app.reminders_due(), 0);
        assert!(app.saved.store.items()[0].done);
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("reminder due"), "{text}");
        // It stays in the list, as done, until removed.
        app.perform(Action::OpenSaved);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("done"), "{text}");
        assert_eq!(saved_ids(&app), ["$e1"]);
    }

    #[test]
    fn with_several_due_the_status_line_opens_the_list_at_a_due_one() {
        let (mut app, mut terminal, _commands) = with_reminder();
        for (index, at) in [(0, 100), (2, 5000)] {
            app.timeline.selected = Some(index);
            app.perform(Action::SaveForLater);
            let event = format!("$e{index}");
            app.saved
                .store
                .set_reminder(ROOM, &event, Some(friday().timestamp() + at));
        }
        // Newest saved first: $e2 (waits), $e0 (due), $e1 (due).
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        app.toast = None;
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "2 reminders due");
        app.on_event(click(col + 2, row));
        assert_eq!(popup(&app).selected, 1);
    }

    #[test]
    fn a_click_on_the_reminders_notification_goes_to_its_message() {
        let (mut app, _terminal, mut commands) = with_reminder();
        app.notification_preview = false;
        app.timeline.selected = Some(2);
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        assert_eq!(app.saved.notices.len(), 1);
        while commands.try_recv().is_ok() {}
        app.on_matrix(MatrixEvent::NoticeClicked {
            room_id: ROOM.into(),
        });
        assert_eq!(app.timeline.selected, Some(1));
        assert!(app.saved.store.items()[0].done);
        // The next click on a notification of the room is an ordinary one.
        app.timeline.selected = Some(2);
        app.on_matrix(MatrixEvent::NoticeClicked {
            room_id: ROOM.into(),
        });
        assert_eq!(app.timeline.selected, Some(2));
    }

    #[test]
    fn a_message_notification_after_the_reminder_takes_the_click_for_itself() {
        let (mut app, _terminal, _commands) = with_reminder();
        app.notification_preview = false;
        app.window_focused = false;
        app.timeline.selected = Some(2);
        later(&mut app, 3600);
        app.saved_tick(TEXT_WAIT);
        app.on_matrix(MatrixEvent::Notice(Notice {
            room_id: ROOM.into(),
            room: "Garden".into(),
            sender: "Ann".into(),
            body: "news".into(),
            highlight: false,
            thread: None,
        }));
        app.on_matrix(MatrixEvent::NoticeClicked {
            room_id: ROOM.into(),
        });
        assert_eq!(app.timeline.selected, Some(2));
        assert!(!app.saved.store.items()[0].done);
    }

    #[test]
    fn reminders_missed_while_the_app_was_away_are_due_at_the_next_start_without_a_notification() {
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-app-saved-missed-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("saved.json");
        {
            let (mut app, _terminal, _commands) = connected();
            app.load_saved(file.clone());
            app.perform(Action::RemindMe);
            app.on_event(key(KeyCode::Enter));
            assert_eq!(saved_ids(&app), ["$e1"]);
        }
        let (mut app, mut terminal, mut commands) = connected();
        later(&mut app, 86_400);
        app.load_saved(file);
        assert!(toast(&app).contains("came due while"), "{}", toast(&app));
        app.sync_saved_flags(ROOM);
        assert!(app.rooms[0].messages[1].saved);
        app.saved_tick(TEXT_WAIT);
        assert!(app.saved.notices.is_empty());
        assert!(fetches(&mut commands).is_empty());
        app.toast = None;
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("1 reminder due"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_file_does_not_stop_the_app_and_is_told() {
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-app-saved-damaged-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("saved.json");
        std::fs::write(&file, "{{{").unwrap();
        let (mut app, _terminal, _commands) = connected();
        app.load_saved(file.clone());
        assert!(toast(&app).contains("damaged"), "{}", toast(&app));
        app.perform(Action::SaveForLater);
        assert_eq!(saved_ids(&app), ["$e1"]);
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_popup_works_on_a_small_screen_and_scrolls() {
        let (mut app, _terminal, mut commands) = connected();
        for n in 0..30 {
            app.saved
                .store
                .add(SavedItem {
                    room_id: ROOM.into(),
                    event_id: format!("$x{n}"),
                    thread: None,
                    saved_at: n,
                    remind_at: None,
                    done: false,
                })
                .unwrap();
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.perform(Action::OpenSaved);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Enter go to"), "{text}");
        let first = fetches(&mut commands);
        assert_eq!(first[0].1.len(), FETCH_FIRST);
        app.on_event(key(KeyCode::End));
        screen(&mut app, &mut terminal);
        assert_eq!(popup(&app).selected, 29);
        assert!(popup(&app).scroll > 0);
        // The rows that came into view are asked about, and only those.
        app.saved_tick(TEXT_WAIT);
        let more: Vec<(String, String)> = fetches(&mut commands)
            .into_iter()
            .flat_map(|(_, items)| items)
            .collect();
        assert!(more.contains(&(ROOM.to_owned(), "$x0".to_owned())), "{more:?}");
        assert!(more.len() < 30 - FETCH_FIRST, "{}", more.len());
        // The form fits too, with its hint.
        app.on_event(key(KeyCode::Char('r')));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("in a week"), "{text}");
        assert!(text.contains("At"), "{text}");
    }

    #[test]
    fn the_demo_saves_and_lists_without_a_worker() {
        let mut app = App::new(&Config::default(), std::path::PathBuf::new(), None);
        app.saved.clock = Clock::Fixed(friday());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        screen(&mut app, &mut terminal);
        app.focus = Pane::Timeline;
        let index = (0..app.messages().len())
            .rev()
            .find(|i| app.saved_key(*i).is_some() && !app.messages()[*i].body.is_empty())
            .expect("a message to save");
        app.timeline.selected = Some(index);
        app.perform(Action::SaveForLater);
        assert!(app.messages()[index].saved);
        app.timeline.selected = Some(0);
        app.perform(Action::OpenSaved);
        let details = popup(&app).details.clone();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].text, SavedText::Text);
        assert!(!details[0].snippet.is_empty());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.timeline.selected, Some(index));
    }
}
