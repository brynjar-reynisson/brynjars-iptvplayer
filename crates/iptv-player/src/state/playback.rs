//! What is playing, and the list Up/Down walks through while playing.
//!
//! The list is a snapshot taken when playback started (the visible list at
//! that moment) together with the library it indexes into. When a refresh
//! swaps the library, [`Playback::rebase`] re-points everything at the new
//! one by favorite key and drops the old `Arc<Library>`. A channel that
//! vanished keeps playing (the stream is already loaded) but has no entry
//! index any more: it is "not in playlist".

use std::sync::Arc;
use std::time::Instant;

use iptv::library::Library;
use iptv::playlist::{favorite_key, Kind};

use super::nav::ChannelList;
use super::zap::Debounce;
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
    pub fn new(lib: Arc<Library>, mut list: Vec<usize>, entry: usize, tab: Tab) -> Playback {
        if lib.entry(entry).kind == Kind::Live {
            // Up/Down while watching live only switches between live channels
            // (a Favorites list can mix in movies/episodes).
            list.retain(|&e| lib.entry(e).kind == Kind::Live);
        }
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

    /// A recording file (not in the playlist): plays like a movie, with
    /// `key` for its resume position. Up/Down and favorites do not apply.
    pub fn local(lib: Arc<Library>, name: &str, key: &str, path: &str) -> Playback {
        Playback {
            lib,
            list: ChannelList::new(Vec::new()),
            entry: None,
            name: name.to_string(),
            key: key.to_string(),
            url: path.to_string(),
            live: false,
            tab: Tab::Recordings,
        }
    }

    /// A recording file, see [`Playback::local`].
    pub fn is_local(&self) -> bool {
        self.tab == Tab::Recordings
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

/// Queue `target` as the debounced channel switch while `loaded` plays.
/// Navigating back to the loaded stream (Down then Up within the window)
/// drops the pending switch instead of reloading what already plays, and a
/// movie/episode target is never auto-played (the cursor just moves; any
/// pending switch is dropped since the selection left it).
pub fn queue_zap(z: &mut Debounce<Playback>, loaded: Option<&Playback>, target: Playback, now: Instant) {
    if !target.is_live() || loaded.is_some_and(|l| l.key() == target.key()) {
        z.cancel();
    } else {
        z.push(target, now);
    }
}

/// After a library swap: re-point the pending zap target; drop it if its
/// channel left the playlist (never start a stream that is no longer listed).
pub fn rebase_pending(z: &mut Debounce<Playback>, new: Arc<Library>) {
    if let Some(p) = z.pending_mut() {
        p.rebase(new);
    }
    if z.pending().is_some_and(|p| !p.in_playlist()) {
        z.cancel();
    }
}

/// What Enter does with the selected entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterAction {
    /// The selection is what is loaded: open the full player view.
    OpenPlayer,
    Play(usize),
    Nothing,
}

/// Enter on the loaded entry opens the player (the caller cancels any
/// pending zap) without reloading; on any other entry it plays it.
pub fn enter_action(selected: Option<usize>, loaded: Option<usize>) -> EnterAction {
    match selected {
        Some(e) if loaded == Some(e) => EnterAction::OpenPlayer,
        Some(e) => EnterAction::Play(e),
        None => EnterAction::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::live::tests::{live, sample_lib};
    use crate::state::zap::ZAP_DELAY;

    #[test]
    fn a_local_recording_is_a_vod_outside_the_playlist() {
        let lib = Arc::new(sample_lib());
        let mut p = Playback::local(lib.clone(), "Game", "rec:Game", "C:/x/Game.ts");
        assert!(p.is_local() && !p.is_live() && !p.in_playlist());
        assert_eq!((p.key(), p.url()), ("rec:Game", "C:/x/Game.ts"));
        p.rebase(lib);
        assert!(!p.in_playlist(), "a library swap does not make it a playlist entry");
    }

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

    fn mixed_lib() -> Library {
        use crate::state::vod::tests::vod;
        Library::build(vec![
            live("1", "SVT1 HD", "Sweden - Nationella"),
            vod(Kind::Movie, "7", "Invented Movie", "Movies: Testland"),
            live("2", "SVT2 HD", "Sweden - Nationella"),
            vod(Kind::Series, "8", "Invented Show S01E01", "Series: Testland"),
        ])
    }

    #[test]
    fn vod_entries_play_as_vod_and_live_lists_skip_them() {
        let lib = Arc::new(mixed_lib());
        // A movie favorite resumes as VOD (is_live derived from Entry.kind).
        let m = Playback::new(lib.clone(), vec![0, 1, 2, 3], 1, Tab::Favorites);
        assert!(!m.is_live());
        assert_eq!(m.key(), "movie:7");
        assert!(!Playback::new(lib.clone(), vec![3], 3, Tab::Series).is_live());
        // Live playback from a mixed favorites list: Up/Down only walks live entries.
        let mut l = Playback::new(lib, vec![0, 1, 2, 3], 0, Tab::Favorites);
        assert_eq!(l.step(1), Some(2));
        assert_eq!(l.step(1), Some(0));
    }

    #[test]
    fn zap_back_to_the_loaded_channel_drops_the_pending_zap() {
        let lib = Arc::new(sample_lib());
        let loaded = Playback::new(lib.clone(), vec![0, 1, 3], 1, Tab::Live);
        let mut z = Debounce::new(ZAP_DELAY);
        let t0 = Instant::now();
        // Down: pending zap to entry 3.
        queue_zap(&mut z, Some(&loaded), Playback::new(lib.clone(), vec![0, 1, 3], 3, Tab::Live), t0);
        assert_eq!(z.pending().and_then(Playback::entry_index), Some(3));
        // Up (back to the loaded channel) within the window: nothing to reload.
        queue_zap(&mut z, Some(&loaded), Playback::new(lib.clone(), vec![0, 1, 3], 1, Tab::Live), t0);
        assert!(z.pending().is_none());
        assert!(z.due(t0 + ZAP_DELAY * 2).is_none());
        // Nothing loaded: always queued.
        queue_zap(&mut z, None, Playback::new(lib, vec![0, 1, 3], 1, Tab::Live), t0);
        assert!(z.pending().is_some());
    }

    #[test]
    fn zap_between_third_party_master_m3u8_channels_is_not_same_key() {
        let m = "#EXTINF:-1 group-title=\"Sweden - X\",Alpha\nhttp://cdn.invalid/a/master.m3u8\n\
#EXTINF:-1 group-title=\"Sweden - X\",Beta\nhttp://cdn.invalid/b/master.m3u8\n";
        let lib = Arc::new(Library::build(iptv::playlist::parse(std::io::Cursor::new(m))));
        let loaded = Playback::new(lib.clone(), vec![0, 1], 0, Tab::Live);
        let mut z = Debounce::new(ZAP_DELAY);
        queue_zap(&mut z, Some(&loaded), Playback::new(lib, vec![0, 1], 1, Tab::Live), Instant::now());
        assert_eq!(z.pending().and_then(Playback::entry_index), Some(1));
    }

    #[test]
    fn vod_targets_never_auto_play() {
        let lib = Arc::new(mixed_lib());
        let loaded = Playback::new(lib.clone(), vec![0, 2], 0, Tab::Favorites);
        let mut z = Debounce::new(ZAP_DELAY);
        let t0 = Instant::now();
        queue_zap(&mut z, Some(&loaded), Playback::new(lib.clone(), vec![0, 1, 2], 2, Tab::Favorites), t0);
        assert!(z.pending().is_some());
        // The cursor moved on to a movie: the stale live target is dropped too.
        queue_zap(&mut z, Some(&loaded), Playback::new(lib, vec![0, 1, 2], 1, Tab::Favorites), t0);
        assert!(z.pending().is_none());
    }

    #[test]
    fn enter_on_the_loaded_entry_opens_the_player_without_reloading() {
        assert_eq!(enter_action(Some(4), Some(4)), EnterAction::OpenPlayer);
        assert_eq!(enter_action(Some(4), Some(3)), EnterAction::Play(4));
        assert_eq!(enter_action(Some(4), None), EnterAction::Play(4));
        assert_eq!(enter_action(None, Some(3)), EnterAction::Nothing);
    }

    #[test]
    fn rebase_drops_a_pending_zap_that_left_the_playlist() {
        let lib = Arc::new(sample_lib());
        let mut z = Debounce::new(ZAP_DELAY);
        let t0 = Instant::now();
        z.push(Playback::new(lib.clone(), vec![0, 3], 3, Tab::Live), t0); // TV4 Sport
        let keep = Arc::new(Library::build(vec![
            live("9", "New", "Sweden - Nationella"),
            live("4", "TV4 Sport", "Sweden - Sport"),
        ]));
        rebase_pending(&mut z, keep.clone());
        assert_eq!(z.pending().and_then(Playback::entry_index), Some(1), "re-pointed");
        let gone = Arc::new(Library::build(vec![live("1", "SVT1 HD", "Sweden - Nationella")]));
        rebase_pending(&mut z, gone);
        assert!(z.pending().is_none(), "target vanished: no zap to a stream that left the playlist");
        assert_eq!(Arc::strong_count(&keep), 1);
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
