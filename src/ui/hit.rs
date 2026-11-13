//! Mouse hit-testing. While drawing, every clickable thing registers its rectangle;
//! mouse events are then resolved against what was actually on screen.

use ratatui::layout::{Position, Rect};

use crate::action::{Action, Pane};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Help,
    Settings,
    Theme,
    CancelCompose,
    JumpToNewest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitId {
    Pane(Pane),
    /// Index into `App::rooms`.
    Room(usize),
    RoomFilter,
    /// Index into the active room's messages.
    Message(usize),
    /// Message index and reaction index.
    Reaction(usize, usize),
    /// Message index and link index within it.
    Link(usize, usize),
    ComposerText,
    /// A text field or button slot in a form.
    Field(usize),
    /// Index into the emoji picker's results.
    Emoji(usize),
    /// A row of the settings menu, and the step to change it by (0: just activate).
    Setting(usize, i8),
    Splitter,
    Button(Button),
    /// A clickable hint that performs an action, like the ones in the status line.
    Do(Action),
    /// The dimmed area around an overlay; clicking it closes the overlay.
    Backdrop,
    Overlay,
    /// The picture of the message at this index.
    Image(usize),
}

#[derive(Debug, Default)]
pub struct HitMap {
    entries: Vec<(Rect, HitId)>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Later registrations lie on top of earlier ones.
    pub fn add(&mut self, rect: Rect, id: HitId) {
        if rect.width > 0 && rect.height > 0 {
            self.entries.push((rect, id));
        }
    }

    pub fn at(&self, column: u16, row: u16) -> Option<HitId> {
        let position = Position::new(column, row);
        self.entries
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, id)| *id)
    }

    /// The pane under the pointer, looking through whatever lies on top of it.
    pub fn pane_at(&self, column: u16, row: u16) -> Option<Pane> {
        let position = Position::new(column, row);
        self.entries.iter().rev().find_map(|(rect, id)| match id {
            HitId::Pane(pane) if rect.contains(position) => Some(*pane),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topmost_wins_and_empty_rects_are_ignored() {
        let mut map = HitMap::default();
        map.add(Rect::new(0, 0, 10, 10), HitId::Pane(Pane::Timeline));
        map.add(Rect::new(2, 2, 3, 1), HitId::Message(4));
        map.add(Rect::new(0, 0, 0, 5), HitId::Splitter);
        assert_eq!(map.at(3, 2), Some(HitId::Message(4)));
        assert_eq!(map.at(5, 2), Some(HitId::Pane(Pane::Timeline)));
        assert_eq!(map.at(0, 0), Some(HitId::Pane(Pane::Timeline)));
        assert_eq!(map.at(10, 10), None);
        assert_eq!(map.pane_at(3, 2), Some(Pane::Timeline));
        map.clear();
        assert_eq!(map.at(3, 2), None);
    }
}
