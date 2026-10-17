//! A list of library entry indices with a cursor (one row selected).

/// Entry indices (into a `Library`) in display order, plus the selected row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelList {
    items: Vec<usize>,
    cursor: Option<usize>,
}

impl ChannelList {
    /// New list with no row selected.
    pub fn new(items: Vec<usize>) -> ChannelList {
        ChannelList { items, cursor: None }
    }

    pub fn items(&self) -> &[usize] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Selected row (position in the list).
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Entry index of the selected row.
    pub fn current(&self) -> Option<usize> {
        self.cursor.and_then(|c| self.items.get(c).copied())
    }

    /// Select row `pos` (clamped to the list; no-op on an empty list).
    pub fn set_cursor(&mut self, pos: usize) {
        if self.items.is_empty() {
            self.cursor = None;
        } else {
            self.cursor = Some(pos.min(self.items.len() - 1));
        }
    }

    /// Select the row showing `entry`; false (cursor cleared) if not listed.
    pub fn select_entry(&mut self, entry: usize) -> bool {
        self.cursor = self.items.iter().position(|&e| e == entry);
        self.cursor.is_some()
    }

    /// Move the cursor by `delta` rows, wrapping around at both ends, and
    /// return the newly selected entry. With nothing selected, a step down
    /// selects the first row and a step up the last.
    pub fn step(&mut self, delta: i32) -> Option<usize> {
        let n = self.items.len() as i64;
        if n == 0 {
            return None;
        }
        let next = match self.cursor {
            Some(c) => (c as i64 + delta as i64).rem_euclid(n),
            None if delta >= 0 => 0,
            None => n - 1,
        };
        self.cursor = Some(next as usize);
        self.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_wraps_both_ways() {
        let mut l = ChannelList::new(vec![10, 20, 30]);
        assert_eq!(l.current(), None);
        assert_eq!(l.step(1), Some(10));
        assert_eq!(l.step(1), Some(20));
        assert_eq!(l.step(1), Some(30));
        assert_eq!(l.step(1), Some(10), "wraps at the end");
        assert_eq!(l.step(-1), Some(30), "wraps at the start");
        let mut l = ChannelList::new(vec![10, 20, 30]);
        assert_eq!(l.step(-1), Some(30), "up with nothing selected = last");
    }

    #[test]
    fn empty_list_never_selects() {
        let mut l = ChannelList::new(vec![]);
        assert_eq!(l.step(1), None);
        l.set_cursor(3);
        assert_eq!(l.cursor(), None);
    }

    #[test]
    fn select_entry_and_clamp() {
        let mut l = ChannelList::new(vec![5, 6, 7]);
        assert!(l.select_entry(6));
        assert_eq!(l.cursor(), Some(1));
        assert!(!l.select_entry(99));
        assert_eq!(l.cursor(), None);
        l.set_cursor(100);
        assert_eq!(l.current(), Some(7));
    }
}
