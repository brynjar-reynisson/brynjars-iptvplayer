//! What is playing, and the list Up/Down walks through while playing.
//!
//! The list is a snapshot taken when playback started (the visible list at
//! that moment) together with the library it indexes into, so a background
//! refresh swapping the library never invalidates it.

use std::sync::Arc;

use iptv::library::Library;
use iptv::playlist::{favorite_key, Entry, Kind};

use super::nav::ChannelList;
use super::Tab;

#[derive(Debug)]
pub struct Playback {
    lib: Arc<Library>,
    list: ChannelList,
    /// Tab the list came from.
    pub tab: Tab,
}

impl Playback {
    /// Play `entry`, with `list` as the Up/Down list (the entry is selected
    /// in it; if not listed, the list is just `[entry]`).
    pub fn new(lib: Arc<Library>, list: Vec<usize>, entry: usize, tab: Tab) -> Playback {
        let mut l = ChannelList::new(list);
        if !l.select_entry(entry) {
            l = ChannelList::new(vec![entry]);
            l.set_cursor(0);
        }
        Playback { lib, list: l, tab }
    }

    pub fn library(&self) -> &Arc<Library> {
        &self.lib
    }

    pub fn entry_index(&self) -> usize {
        self.list.current().expect("playback always has a selected entry")
    }

    pub fn entry(&self) -> &Entry {
        self.lib.entry(self.entry_index())
    }

    pub fn key(&self) -> String {
        favorite_key(self.entry())
    }

    pub fn is_live(&self) -> bool {
        self.entry().kind == Kind::Live
    }

    /// Next/previous entry in the snapshot list (wrapping).
    pub fn step(&mut self, delta: i32) -> usize {
        self.list.step(delta);
        self.entry_index()
    }

    /// True if this playback indexes into `lib` (same library instance).
    pub fn same_library(&self, lib: &Arc<Library>) -> bool {
        Arc::ptr_eq(&self.lib, lib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::live::tests::sample_lib;

    #[test]
    fn steps_through_snapshot_and_wraps() {
        let lib = Arc::new(sample_lib());
        let mut p = Playback::new(lib, vec![3, 4], 4, Tab::Live);
        assert_eq!(p.entry().name, "\u{c5}rets Sport");
        assert_eq!(p.step(1), 3);
        assert_eq!(p.step(1), 4);
        assert_eq!(p.step(-1), 3);
        assert!(p.is_live());
        assert_eq!(p.key(), "live:4");
    }

    #[test]
    fn entry_not_in_list_plays_alone() {
        let lib = Arc::new(sample_lib());
        let mut p = Playback::new(lib, vec![0, 1], 5, Tab::Favorites);
        assert_eq!(p.entry_index(), 5);
        assert_eq!(p.step(1), 5);
    }
}
