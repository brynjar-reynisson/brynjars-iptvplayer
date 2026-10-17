//! What is playing, and the list Up/Down walks through while playing.
//!
//! The list is a snapshot taken when playback started (the visible list at
//! that moment) together with the library it indexes into. When a refresh
//! swaps the library, [`Playback::rebase`] re-points everything at the new
//! one by favorite key and drops the old `Arc<Library>`. A channel that
//! vanished keeps playing (the stream is already loaded) but has no entry
//! index any more: it is "not in playlist".

use std::sync::Arc;

use iptv::library::Library;
use iptv::playlist::{favorite_key, Kind};

use super::nav::ChannelList;
use super::Tab;

#[derive(Clone)]
pub struct Playback {
    lib: Arc<Library>,
    list: ChannelList,
    /// Entry in `lib` that is playing; `None` once a refresh removed it.
    entry: Option<usize>,
    name: String,
    key: String,
    /// Stream URL (credentials): never printed (manual Debug below).
    url: String,
    live: bool,
    /// Tab the list came from.
    pub tab: Tab,
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playback")
            .field("entry", &self.entry)
            .field("key", &self.key)
            .field("list_len", &self.list.len())
            .field("tab", &self.tab)
            .finish_non_exhaustive()
    }
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
        let mut p = Playback {
            lib,
            list: l,
            entry: None,
            name: String::new(),
            key: String::new(),
            url: String::new(),
            live: true,
            tab,
        };
        p.set_entry(entry);
        p
    }

    fn set_entry(&mut self, entry: usize) {
        let e = self.lib.entry(entry);
        self.entry = Some(entry);
        self.name = e.name.clone();
        self.key = favorite_key(e);
        self.url = e.url.clone();
        self.live = e.kind == Kind::Live;
    }

    pub fn library(&self) -> &Arc<Library> {
        &self.lib
    }

    /// Playing entry in [`Playback::library`]; `None` = not in the playlist
    /// any more (still playing).
    pub fn entry_index(&self) -> Option<usize> {
        self.entry
    }

    pub fn in_playlist(&self) -> bool {
        self.entry.is_some()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Stream URL, only for handing to the player.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    /// Next/previous entry in the snapshot list (wrapping); `None` only if
    /// the list is empty.
    pub fn step(&mut self, delta: i32) -> Option<usize> {
        let e = self.list.step(delta)?;
        self.set_entry(e);
        Some(e)
    }

    /// True if this playback indexes into `lib` (same library instance).
    pub fn same_library(&self, lib: &Arc<Library>) -> bool {
        Arc::ptr_eq(&self.lib, lib)
    }

    /// Re-point at a refreshed library (by favorite key) and release the old
    /// one. Snapshot entries that vanished are dropped from the list; if the
    /// playing channel vanished it keeps playing as "not in playlist".
    pub fn rebase(&mut self, new: Arc<Library>) {
        let items: Vec<usize> =
            self.list.items().iter().filter_map(|&e| new.find_by_key(&favorite_key(self.lib.entry(e)))).collect();
        self.list = ChannelList::new(items);
        // Assign before select/set_entry: the old Arc is dropped here.
        self.lib = new;
        match self.lib.find_by_key(&self.key) {
            Some(e) => {
                self.list.select_entry(e);
                self.set_entry(e);
            }
            // Keep name/key/url (the stream is still loaded in mpv).
            None => self.entry = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::live::tests::{live, sample_lib};

    #[test]
    fn steps_through_snapshot_and_wraps() {
        let lib = Arc::new(sample_lib());
        let mut p = Playback::new(lib, vec![3, 4], 4, Tab::Live);
        assert_eq!(p.name(), "\u{c5}rets Sport");
        assert_eq!(p.step(1), Some(3));
        assert_eq!(p.step(1), Some(4));
        assert_eq!(p.step(-1), Some(3));
        assert!(p.is_live());
        assert_eq!(p.key(), "live:4");
    }

    #[test]
    fn entry_not_in_list_plays_alone() {
        let lib = Arc::new(sample_lib());
        let mut p = Playback::new(lib, vec![0, 1], 5, Tab::Favorites);
        assert_eq!(p.entry_index(), Some(5));
        assert_eq!(p.step(1), Some(5));
    }

    #[test]
    fn debug_has_no_url() {
        let p = Playback::new(Arc::new(sample_lib()), vec![0], 0, Tab::Live);
        let s = format!("{p:?}");
        assert!(!s.contains("USER") && !s.contains("host.invalid"), "{s}");
    }

    #[test]
    fn rebase_follows_the_channel_and_releases_the_old_library() {
        let old = Arc::new(sample_lib());
        let mut p = Playback::new(old.clone(), vec![0, 1, 3], 1, Tab::Live); // SVT2 HD
        // Refreshed playlist: order changed, entry "4" (TV4 Sport) gone.
        let new = Arc::new(Library::build(vec![
            live("9", "New One", "Sweden - Nationella"),
            live("2", "SVT2 HD", "Sweden - Nationella"),
            live("1", "SVT1 HD", "Sweden - Nationella"),
        ]));
        p.rebase(new.clone());
        assert!(p.same_library(&new));
        assert_eq!(Arc::strong_count(&old), 1, "old library released");
        assert_eq!(p.entry_index(), Some(1), "same channel, new index");
        assert!(p.in_playlist());
        assert_eq!(p.name(), "SVT2 HD");
        // Snapshot list remapped (vanished entry dropped): [SVT1 HD, SVT2 HD].
        assert_eq!(p.step(1), Some(2));
        assert_eq!(p.step(1), Some(1));
    }

    #[test]
    fn rebase_keeps_a_vanished_channel_playing_without_an_index() {
        let mut p = Playback::new(Arc::new(sample_lib()), vec![0, 3, 4], 3, Tab::Live); // TV4 Sport
        let new = Arc::new(Library::build(vec![
            live("1", "SVT1 HD", "Sweden - Nationella"),
            live("5", "\u{c5}rets Sport", "Sweden - Sport"),
        ]));
        p.rebase(new);
        assert_eq!(p.entry_index(), None, "no dangling index");
        assert!(!p.in_playlist());
        assert_eq!(p.name(), "TV4 Sport", "still shows what is playing");
        assert_eq!(p.key(), "live:4");
        assert!(p.url().ends_with("/4"), "stream info kept");
        // Up/Down continues in the remapped list [SVT1 HD, Arets Sport].
        assert_eq!(p.step(1), Some(0));
        assert!(p.in_playlist());
    }
}
