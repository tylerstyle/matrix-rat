//! Poll creation, editing, voting, and `/poll` command handling.

use crossterm::event::{KeyCode, KeyModifiers};

use super::{App, ComposeMode, Pane};
use crate::lineedit::{EditKeys, EditOutcome};
use crate::matrix::MatrixCmd;
use crate::modal::{Modal, PollFormPopup, PollFormSlot, PollVotePopup};
use crate::model::{ItemRef, Message, MsgKind, PollAnswer, PollInfo, SendState};

/// Extracts the optional question argument from a `/poll` command, or `None` if `text`
/// is not a `/poll` command.
pub(super) fn poll_command_argument(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let rest = trimmed.strip_prefix("/poll")?;
    if rest.is_empty() {
        return Some("");
    }
    if rest.starts_with(char::is_whitespace) {
        return Some(rest.trim());
    }
    None
}

impl App {
    /// Handles `/poll [question]` on Enter in the composer. Returns `true` if handled.
    pub(super) fn poll_command(&mut self) -> bool {
        let text = self.composer.text().to_owned();
        let Some(arg) = poll_command_argument(&text) else {
            return false;
        };
        if let Some(hint) = self
            .room()
            .filter(|room| room.is_pending())
            .map(|room| room.kind.pending_hint())
        {
            self.show_toast(hint);
            return true;
        }
        let Some(room_id) = self.room_id() else {
            return true;
        };
        self.composer.set_text("");
        self.sync_completion();
        self.open_modal(Modal::PollForm(PollFormPopup::create(room_id, arg)));
        true
    }

    pub(super) fn open_poll_create(&mut self, initial_question: &str) {
        if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.show_toast("Finish or cancel the edit first");
            return;
        }
        if let Some(hint) = self
            .room()
            .filter(|room| room.is_pending())
            .map(|room| room.kind.pending_hint())
        {
            self.show_toast(hint);
            return;
        }
        let Some(room_id) = self.room_id() else {
            return;
        };
        self.open_modal(Modal::PollForm(PollFormPopup::create(
            room_id,
            initial_question,
        )));
    }

    pub(super) fn open_poll_edit(&mut self, index: usize) {
        let Some(room_id) = self.room_id() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if !message.editable() {
            self.show_toast("A poll can only be edited before anyone votes");
            return;
        }
        let Some(poll) = message.poll.clone() else {
            return;
        };
        let target = message
            .item
            .clone()
            .unwrap_or_else(|| ItemRef::Event(message.id.clone()));
        self.open_modal(Modal::PollForm(PollFormPopup::edit(room_id, target, &poll)));
    }

    pub(super) fn open_poll_vote(&mut self, index: usize) {
        let Some(room_id) = self.room_id() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if message.kind == MsgKind::Redacted || message.state != SendState::Sent {
            return;
        }
        let Some(poll) = message.poll.clone() else {
            return;
        };
        if poll.ended {
            self.show_toast("This poll has ended");
            return;
        }
        let poll_event_id = match &message.item {
            Some(ItemRef::Event(id)) => id.clone(),
            Some(ItemRef::Transaction(_)) => {
                self.show_toast("Wait until the poll is sent");
                return;
            }
            None => message.id.clone(),
        };
        if poll_event_id.is_empty() {
            return;
        }
        self.open_modal(Modal::PollVote(PollVotePopup::from_poll(
            room_id,
            poll_event_id,
            &poll,
        )));
    }

    pub(super) fn end_poll(&mut self, index: usize) {
        let Some(room_id) = self.room_id() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if !message.own || message.kind == MsgKind::Redacted || message.state != SendState::Sent {
            return;
        }
        let Some(poll) = &message.poll else {
            return;
        };
        if poll.ended {
            return;
        }
        let poll_event_id = match &message.item {
            Some(ItemRef::Event(id)) => id.clone(),
            _ => message.id.clone(),
        };
        if poll_event_id.is_empty() {
            return;
        }
        if let Some(messages) = self.messages_mut()
            && let Some(msg) = messages.get_mut(index)
            && let Some(p) = &mut msg.poll
        {
            p.ended = true;
            p.editable = false;
            self.timeline_rev = self.timeline_rev.wrapping_add(1);
        }
        self.send_matrix(MatrixCmd::EndPoll {
            room_id,
            poll_event_id,
        });
    }

    pub(super) fn click_timeline_poll_answer(&mut self, msg_idx: usize, ans_idx: usize) {
        self.cancel_pending_jump();
        self.set_focus(Pane::Timeline);
        if self.selectable(msg_idx) {
            self.timeline.selected = Some(msg_idx);
        }
        let Some(message) = self.messages().get(msg_idx) else {
            return;
        };
        if message.kind == MsgKind::Redacted || message.state != SendState::Sent {
            return;
        }
        let Some(poll) = &message.poll else {
            return;
        };
        if poll.ended {
            return;
        }
        let poll_event_id = match &message.item {
            Some(ItemRef::Event(id)) => id.clone(),
            Some(ItemRef::Transaction(_)) => {
                self.show_toast("Wait until the poll is sent");
                return;
            }
            None => message.id.clone(),
        };
        let next_ids = match poll.toggled_selection(ans_idx) {
            Ok(ids) => ids,
            Err(max) => {
                self.show_toast(format!("Choose at most {max} answers"));
                return;
            }
        };
        self.apply_poll_vote_by_event(&poll_event_id, next_ids);
    }

    fn apply_poll_vote_by_event(&mut self, poll_event_id: &str, next_ids: Vec<String>) {
        if let Some(messages) = self.messages_mut()
            && let Some(msg) = messages.iter_mut().find(|m| {
                matches!(&m.item, Some(ItemRef::Event(id)) if id == poll_event_id)
                    || (m.item.is_none() && m.id == poll_event_id)
            })
            && let Some(poll) = &mut msg.poll
            && !poll.ended
        {
            let had_any = poll.answers.iter().any(|a| a.own);
            let has_any = !next_ids.is_empty();
            for ans in &mut poll.answers {
                let want = next_ids.iter().any(|id| id == &ans.id);
                if want && !ans.own {
                    ans.own = true;
                    ans.votes = ans.votes.saturating_add(1);
                } else if !want && ans.own {
                    ans.own = false;
                    ans.votes = ans.votes.saturating_sub(1);
                }
            }
            if !had_any && has_any {
                poll.total_voters = poll.total_voters.saturating_add(1);
            } else if had_any && !has_any {
                poll.total_voters = poll.total_voters.saturating_sub(1);
            }
            if has_any {
                poll.editable = false;
            }
            self.timeline_rev = self.timeline_rev.wrapping_add(1);
        }
        if let Some(room_id) = self.room_id()
            && !poll_event_id.is_empty()
        {
            self.send_matrix(MatrixCmd::VotePoll {
                room_id,
                poll_event_id: poll_event_id.to_owned(),
                answers: next_ids,
            });
        }
    }

    pub(super) fn on_poll_form_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::PollForm(form)) = &mut self.modal else {
            return;
        };
        let slot = form.current_slot();
        match (code, mods) {
            (KeyCode::Esc, _) => self.modal = None,
            (KeyCode::Tab, KeyModifiers::NONE) | (KeyCode::Down, KeyModifiers::NONE) => {
                form.step(1);
            }
            (KeyCode::BackTab, _) | (KeyCode::Up, KeyModifiers::NONE) => {
                form.step(-1);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => match slot {
                PollFormSlot::Question => form.step(1),
                PollFormSlot::Answer(i) => {
                    if i + 1 < form.answers.len() {
                        form.step(1);
                    } else {
                        self.submit_poll_form();
                    }
                }
                PollFormSlot::AddAnswer => {
                    form.add_answer();
                }
                PollFormSlot::Kind => form.flip_kind(),
                PollFormSlot::MaxSelections => form.step_max(1),
                PollFormSlot::Submit => self.submit_poll_form(),
                PollFormSlot::Cancel => self.modal = None,
            },
            (KeyCode::Char(' '), KeyModifiers::NONE)
                if matches!(
                    slot,
                    PollFormSlot::AddAnswer
                        | PollFormSlot::Kind
                        | PollFormSlot::Submit
                        | PollFormSlot::Cancel
                ) =>
            {
                match slot {
                    PollFormSlot::AddAnswer => {
                        form.add_answer();
                    }
                    PollFormSlot::Kind => form.flip_kind(),
                    PollFormSlot::Submit => self.submit_poll_form(),
                    PollFormSlot::Cancel => self.modal = None,
                    _ => {}
                }
            }
            (KeyCode::Left, KeyModifiers::NONE) if slot == PollFormSlot::Kind => {
                form.flip_kind();
            }
            (KeyCode::Right, KeyModifiers::NONE) if slot == PollFormSlot::Kind => {
                form.flip_kind();
            }
            (KeyCode::Left | KeyCode::Char('-'), _) if slot == PollFormSlot::MaxSelections => {
                form.step_max(-1);
            }
            (KeyCode::Right | KeyCode::Char('+' | '='), _)
                if slot == PollFormSlot::MaxSelections =>
            {
                form.step_max(1);
            }
            (KeyCode::Delete, KeyModifiers::CONTROL) | (KeyCode::Backspace, KeyModifiers::ALT) => {
                if let PollFormSlot::Answer(i) = slot
                    && form.answers.get(i).is_some_and(|e| e.is_empty())
                {
                    form.remove_answer(i);
                    return;
                }
                if let Some(input) = form.active_input_mut()
                    && input.handle_key(code, mods, EditKeys::FULL) == EditOutcome::Changed
                {
                    form.error = None;
                }
            }
            _ => {
                if let Some(input) = form.active_input_mut()
                    && input.handle_key(code, mods, EditKeys::FULL) == EditOutcome::Changed
                {
                    form.error = None;
                }
            }
        }
    }

    pub(super) fn on_poll_form_paste(&mut self, first_line: &str) {
        if let Some(Modal::PollForm(form)) = &mut self.modal
            && let Some(input) = form.active_input_mut()
        {
            input.paste(first_line);
            form.error = None;
        }
    }

    pub(super) fn click_poll_form_slot(&mut self, slot_idx: usize) {
        let Some(Modal::PollForm(form)) = &mut self.modal else {
            return;
        };
        let slots = form.slots();
        let Some(&slot) = slots.get(slot_idx) else {
            return;
        };
        form.focus = slot_idx;
        match slot {
            PollFormSlot::Question | PollFormSlot::Answer(_) | PollFormSlot::MaxSelections => {}
            PollFormSlot::AddAnswer => {
                form.add_answer();
            }
            PollFormSlot::Kind => form.flip_kind(),
            PollFormSlot::Submit => self.submit_poll_form(),
            PollFormSlot::Cancel => self.modal = None,
        }
    }

    pub(super) fn remove_poll_answer(&mut self, index: usize) {
        if let Some(Modal::PollForm(form)) = &mut self.modal {
            form.remove_answer(index);
        }
    }

    pub(super) fn step_poll_max(&mut self, delta: i8) {
        if let Some(Modal::PollForm(form)) = &mut self.modal {
            if let Some(pos) = form
                .slots()
                .iter()
                .position(|s| *s == PollFormSlot::MaxSelections)
            {
                form.focus = pos;
            }
            form.step_max(delta);
        }
    }

    pub(super) fn submit_poll_form(&mut self) {
        let Some(Modal::PollForm(form)) = &mut self.modal else {
            return;
        };
        let (question, answers, disclosed, max_selections) = match form.validated() {
            Ok(valid) => valid,
            Err((slot, err)) => {
                form.focus = slot;
                form.error = Some(err);
                return;
            }
        };
        let room_id = form.room_id.clone();
        let editing = form.editing.clone();
        self.modal = None;
        // The room may have gone from the list while the form was open.
        if self.room_id().as_deref() != Some(room_id.as_str()) {
            self.show_toast("That room is not open any more");
            return;
        }

        if let Some(item) = editing {
            if let Some(messages) = self.messages_mut()
                && let Some(target) = messages.iter_mut().find(|m| {
                    m.item.as_ref() == Some(&item)
                        || matches!(&item, ItemRef::Event(id) if m.item.is_none() && &m.id == id)
                })
            {
                let poll_answers: Vec<PollAnswer> = answers
                    .iter()
                    .enumerate()
                    .map(|(i, text)| PollAnswer {
                        id: format!("a{}", i + 1),
                        text: text.clone(),
                        votes: 0,
                        own: false,
                    })
                    .collect();
                target.body = format!("📊 {question} ({})", answers.join(" / "));
                target.edited = true;
                target.poll = Some(PollInfo {
                    question: question.clone(),
                    disclosed,
                    max_selections: u64::from(max_selections),
                    answers: poll_answers,
                    total_voters: 0,
                    ended: false,
                    editable: true,
                });
                self.timeline_rev = self.timeline_rev.wrapping_add(1);
            }
            self.send_matrix(MatrixCmd::EditPoll {
                room_id,
                item,
                question,
                answers,
                disclosed,
                max_selections,
            });
            return;
        }

        if self.matrix.is_none() {
            let poll_answers: Vec<PollAnswer> = answers
                .iter()
                .enumerate()
                .map(|(i, text)| PollAnswer {
                    id: format!("a{}", i + 1),
                    text: text.clone(),
                    votes: 0,
                    own: false,
                })
                .collect();
            let id = format!("$demo-poll-{}", self.timeline_rev.wrapping_add(1));
            self.push_message(Message {
                id: id.clone(),
                item: Some(ItemRef::Event(id)),
                sender: crate::model::OWN_NAME.to_owned(),
                own: true,
                day: "Today".to_owned(),
                time: chrono::Local::now().format("%H:%M").to_string(),
                body: format!("📊 {question} ({})", answers.join(" / ")),
                poll: Some(PollInfo {
                    question,
                    disclosed,
                    max_selections: u64::from(max_selections),
                    answers: poll_answers,
                    total_voters: 0,
                    ended: false,
                    editable: true,
                }),
                ..Message::default()
            });
            self.timeline.scroll = 0;
            return;
        }

        self.send_matrix(MatrixCmd::SendPoll {
            room_id,
            question,
            answers,
            disclosed,
            max_selections,
        });
        self.timeline.scroll = 0;
    }

    pub(super) fn on_poll_vote_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::PollVote(popup)) = &mut self.modal else {
            return;
        };
        match (code, mods) {
            (KeyCode::Esc | KeyCode::Char('q'), KeyModifiers::NONE) => self.modal = None,
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::BackTab, _) => {
                popup.step(-1);
            }
            (KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab, KeyModifiers::NONE) => {
                popup.step(1);
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                let cursor = popup.cursor;
                popup.toggle_at(cursor);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if !popup.is_multi()
                    && let Some(ans) = popup.answers.get(popup.cursor)
                {
                    popup.selected_ids = vec![ans.id.clone()];
                }
                self.submit_poll_vote();
            }
            _ => {}
        }
    }

    pub(super) fn click_poll_vote_answer(&mut self, index: usize) {
        if let Some(Modal::PollVote(popup)) = &mut self.modal
            && index < popup.answers.len()
        {
            popup.cursor = index;
            popup.toggle_at(index);
        }
    }

    pub(super) fn submit_poll_vote(&mut self) {
        let Some(Modal::PollVote(popup)) = self.modal.take() else {
            return;
        };
        if self.room_id().as_deref() != Some(popup.room_id.as_str()) {
            return;
        }
        let next = popup.ordered_selection();
        self.apply_poll_vote_by_event(&popup.poll_event_id, next);
    }

    pub(super) fn scroll_poll_modal(&mut self, delta: isize) {
        match &mut self.modal {
            Some(Modal::PollForm(form)) => {
                let max = form.slots().len().saturating_sub(3);
                form.scroll = (form.scroll as isize + delta).clamp(0, max as isize) as usize;
            }
            Some(Modal::PollVote(popup)) => {
                popup.step(delta.signum());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::action::Action;
    use crate::config::Config;
    use crate::matrix::MatrixHandle;

    fn test_app() -> (App, tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>) {
        let (handle, rx) = MatrixHandle::for_test();
        let mut app = App::new(&Config::default(), PathBuf::new(), Some(handle));
        app.rooms = vec![crate::model::Room::new(
            "!room:example.org",
            "general",
            crate::model::RoomKind::Room,
        )];
        app.active = 0;
        (app, rx)
    }

    #[test]
    fn poll_command_create_vote_edit_and_end_round_trip() {
        let (mut app, mut rx) = test_app();

        // `/poll` with question argument opens PollFormPopup pre-populated and focused on Answer 1.
        app.composer.set_text("/poll Best lunch spot?");
        app.perform(Action::Send);
        assert!(app.composer.is_empty());
        let Some(Modal::PollForm(form)) = &mut app.modal else {
            panic!("expected PollForm modal");
        };
        let dbg = format!("{form:?}");
        assert!(
            !dbg.contains("Best lunch spot?"),
            "form debug leaked: {dbg}"
        );
        assert_eq!(form.question.as_str(), "Best lunch spot?");
        assert_eq!(form.focus, 1);

        // Fill in 3 answers, switch to undisclosed + max 2 selections, and submit.
        form.answers[0].set("Tacos".to_owned());
        form.answers[1].set("Ramen".to_owned());
        assert!(form.add_answer());
        form.answers[2].set("Pizza".to_owned());
        form.flip_kind();
        form.step_max(1);
        app.submit_poll_form();
        assert!(app.modal.is_none());

        match rx.try_recv().expect("SendPoll command") {
            MatrixCmd::SendPoll {
                room_id,
                question,
                answers,
                disclosed,
                max_selections,
            } => {
                assert_eq!(room_id, "!room:example.org");
                assert_eq!(question, "Best lunch spot?");
                assert_eq!(answers, vec!["Tacos", "Ramen", "Pizza"]);
                assert!(!disclosed);
                assert_eq!(max_selections, 2);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Put an editable own poll in the room timeline and edit it via Action::EditLast.
        app.rooms[0].messages.push(Message {
            id: "$poll1".into(),
            item: Some(ItemRef::Event("$poll1".into())),
            sender: "you".into(),
            own: true,
            body: "📊 Best lunch spot?".into(),
            poll: Some(PollInfo {
                question: "Best lunch spot?".into(),
                disclosed: false,
                max_selections: 2,
                answers: vec![
                    PollAnswer {
                        id: "a1".into(),
                        text: "Tacos".into(),
                        votes: 0,
                        own: false,
                    },
                    PollAnswer {
                        id: "a2".into(),
                        text: "Ramen".into(),
                        votes: 0,
                        own: false,
                    },
                ],
                total_voters: 0,
                ended: false,
                editable: true,
            }),
            ..Message::default()
        });
        app.perform(Action::EditLast);
        let Some(Modal::PollForm(edit_form)) = &mut app.modal else {
            panic!("expected PollForm edit modal");
        };
        edit_form.question.set("Updated lunch?".to_owned());
        app.submit_poll_form();
        match rx.try_recv().expect("EditPoll command") {
            MatrixCmd::EditPoll { item, question, .. } => {
                assert_eq!(item, ItemRef::Event("$poll1".into()));
                assert_eq!(question, "Updated lunch?");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Direct timeline click on answer 0 votes immediately; clicking answer 1 adds a second vote;
        // once someone voted, `editable()` is false so Edit is rejected.
        app.click_timeline_poll_answer(0, 0);
        match rx.try_recv().expect("VotePoll command") {
            MatrixCmd::VotePoll {
                poll_event_id,
                answers,
                ..
            } => {
                assert_eq!(poll_event_id, "$poll1");
                assert_eq!(answers, vec!["a1"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(!app.rooms[0].messages[0].editable());

        // Enter on the selected poll opens PollVotePopup; toggling answer 1 and pressing Enter submits both.
        app.open_poll_vote(0);
        if let Some(Modal::PollVote(v)) = &app.modal {
            let vdbg = format!("{v:?}");
            assert!(!vdbg.contains("Updated lunch?"));
            assert!(!vdbg.contains("Tacos"));
        } else {
            panic!("expected PollVotePopup");
        }
        app.on_poll_vote_key(KeyCode::Down, KeyModifiers::NONE);
        app.on_poll_vote_key(KeyCode::Char(' '), KeyModifiers::NONE);
        app.on_poll_vote_key(KeyCode::Enter, KeyModifiers::NONE);
        match rx.try_recv().expect("VotePoll multi command") {
            MatrixCmd::VotePoll { answers, .. } => {
                assert_eq!(answers, vec!["a1", "a2"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Ending our own poll sends EndPoll and marks the poll ended locally.
        app.end_poll(0);
        assert!(app.rooms[0].messages[0].poll.as_ref().unwrap().ended);
        match rx.try_recv().expect("EndPoll command") {
            MatrixCmd::EndPoll { poll_event_id, .. } => {
                assert_eq!(poll_event_id, "$poll1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
