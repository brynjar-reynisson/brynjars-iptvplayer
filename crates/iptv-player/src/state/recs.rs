//! Recordings tab state: the list of recording files with a cursor and the
//! "really delete?" question. No GUI types.

use std::path::{Path, PathBuf};

use iptv::recordings::{self, Recording};

#[derive(Debug, Default)]
pub struct RecList {
    items: Vec<Recording>,
    cursor: Option<usize>,
    /// The file the user asked to delete and has not confirmed yet.
    confirm: Option<PathBuf>,
}

impl RecList {
    /// Re-read the folder; the cursor stays on the same file if it is still there.
    pub fn reload(&mut self, dir: &Path) {
        let keep = self.current().map(|r| r.path.clone());
        self.items = recordings::list(dir);
        self.cursor = keep.and_then(|p| self.items.iter().position(|r| r.path == p));
        if let Some(c) = &self.confirm {
            if !self.items.iter().any(|r| &r.path == c) {
                self.confirm = None;
            }
        }
    }

    /// Select the first row when nothing is selected (so Enter and Delete work at once).
    pub fn select_first_if_none(&mut self) {
        if self.cursor.is_none() && !self.items.is_empty() {
            self.cursor = Some(0);
        }
    }

    pub fn items(&self) -> &[Recording] {
        &self.items
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn current(&self) -> Option<&Recording> {
        self.cursor.and_then(|c| self.items.get(c))
    }

    pub fn set_cursor(&mut self, i: usize) {
        if i < self.items.len() {
            self.cursor = Some(i);
            self.confirm = None;
        }
    }

    /// Move the cursor (wrapping); with nothing selected a step down selects
    /// the first row and a step up the last.
    pub fn step(&mut self, delta: i32) {
        let n = self.items.len() as i32;
        if n == 0 {
            return;
        }
        let next = match self.cursor {
            Some(c) => (c as i32 + delta).rem_euclid(n),
            None if delta >= 0 => 0,
            None => n - 1,
        };
        self.cursor = Some(next as usize);
        self.confirm = None;
    }

    /// Ask to delete the selected file. False if nothing is selected.
    pub fn ask_delete(&mut self) -> bool {
        self.confirm = self.current().map(|r| r.path.clone());
        self.confirm.is_some()
    }

    /// The file waiting for a yes/no.
    pub fn pending_delete(&self) -> Option<&Recording> {
        let c = self.confirm.as_ref()?;
        self.items.iter().find(|r| &r.path == c)
    }

    pub fn cancel_delete(&mut self) {
        self.confirm = None;
    }

    /// The user said yes: the file to delete (the question is closed).
    pub fn take_confirmed(&mut self) -> Option<PathBuf> {
        self.confirm.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(names: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for n in names {
            std::fs::write(d.path().join(n), b"x").unwrap();
        }
        d
    }

    #[test]
    fn cursor_wraps_and_survives_a_reload() {
        let d = dir_with(&["A.ts", "B.ts", "C.ts"]);
        let mut l = RecList::default();
        l.reload(d.path());
        assert_eq!(l.items().len(), 3);
        assert!(l.cursor().is_none());
        l.step(-1);
        assert_eq!(l.cursor(), Some(2), "up from nothing = last");
        l.step(1);
        assert_eq!(l.cursor(), Some(0), "wraps");
        let title = l.current().unwrap().title.clone();
        std::fs::write(d.path().join("D.ts"), b"x").unwrap();
        l.reload(d.path());
        assert_eq!(l.current().unwrap().title, title, "same file stays selected");
    }

    #[test]
    fn the_first_row_can_be_selected_when_nothing_is() {
        let d = dir_with(&["A.ts", "B.ts"]);
        let mut l = RecList::default();
        l.select_first_if_none(); // empty list: nothing to select
        assert!(l.cursor().is_none());
        l.reload(d.path());
        l.select_first_if_none();
        assert_eq!(l.cursor(), Some(0));
        l.step(1);
        l.select_first_if_none();
        assert_eq!(l.cursor(), Some(1), "an existing selection is kept");
    }

    #[test]
    fn delete_needs_a_yes_and_moving_cancels_it() {
        let d = dir_with(&["A.ts", "B.ts"]);
        let mut l = RecList::default();
        l.reload(d.path());
        assert!(!l.ask_delete(), "nothing selected");
        l.step(1);
        assert!(l.ask_delete());
        let asked = l.pending_delete().unwrap().title.clone();
        l.step(1);
        assert!(l.pending_delete().is_none(), "moving away cancels the question");
        l.set_cursor(0);
        l.ask_delete();
        l.cancel_delete();
        assert!(l.take_confirmed().is_none());
        l.ask_delete();
        let p = l.take_confirmed().unwrap();
        assert!(p.ends_with(format!("{}.ts", l.current().unwrap().title)));
        assert!(!asked.is_empty());
        std::fs::remove_file(&p).unwrap();
        l.reload(d.path());
        assert_eq!(l.items().len(), 1);
        assert!(l.cursor().is_none(), "the deleted row's selection is gone");
    }
}
