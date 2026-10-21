//! Which episode follows the one that is playing (autoplay next episode).

use std::collections::HashSet;
use std::sync::{Arc, Weak};

use iptv::library::Library;
use iptv::playlist::Kind;
use iptv::recent_series::Recent;
use iptv::series::UNSORTED;

use super::vod::{episode_rows, EpisodeRow, Hidden};

/// What a Shift+Right press does.
#[derive(Debug, PartialEq, Eq)]
pub enum ShiftRight {
    /// Not ours (typing, a tab button has focus, the subtitle menu is open):
    /// leave the key event alone.
    Ignore,
    /// Consumed, nothing happens (no next episode, live, nothing playing): it
    /// must not fall through to the Right-arrow seek or browse handlers, which
    /// ignore extra Shift.
    Swallow,
    /// Consumed: start the next episode (like the ">>" button).
    PlayNext,
}

/// Shift+Right = next episode, with the guards of the other player keys.
pub fn shift_right(typing: bool, tab_focused: bool, sub_menu: bool, has_next: bool) -> ShiftRight {
    if typing || tab_focused || sub_menu {
        ShiftRight::Ignore
    } else if has_next {
        ShiftRight::PlayNext
    } else {
        ShiftRight::Swallow
    }
}

/// Playing entry, library, hidden groups.
type CacheKey = (Option<usize>, Weak<Library>, Weak<HashSet<String>>);

/// `next_episode` of the playing entry, recomputed only when the playing
/// entry, the library (by `Arc` identity: a refresh swaps it) or the hidden
/// groups (by `Arc` identity: a change replaces the set) change, since
/// `next_episode` scans every show and the ">>" button asks every frame.
/// Holds `Weak`s: never keeps an old library alive, and a freed allocation
/// cannot be reused while the `Weak` exists, so identity is never confused.
#[derive(Default)]
pub struct NextCache {
    key: Option<CacheKey>,
    next: Option<usize>,
    computed: u32,
}

impl NextCache {
    /// The episode after `entry` in `lib` (`None` for live channels, movies,
    /// `(Unsorted)`, the last episode or an entry no longer in the playlist).
    pub fn get(&mut self, lib: &Arc<Library>, entry: Option<usize>, hidden: &Hidden) -> Option<usize> {
        let hit = self.key.as_ref().is_some_and(|(e, l, h)| {
            *e == entry && Weak::ptr_eq(l, &Arc::downgrade(lib)) && Weak::ptr_eq(h, &Arc::downgrade(hidden))
        });
        if !hit {
            self.computed += 1;
            self.next = entry
                .filter(|&e| lib.entry(e).kind == Kind::Series)
                .and_then(|e| next_episode(lib, e, hidden));
            self.key = Some((entry, Arc::downgrade(lib), Arc::downgrade(hidden)));
        }
        self.next
    }

    /// How often `get` recomputed (tests).
    pub fn computed(&self) -> u32 {
        self.computed
    }
}

/// The entry of the episode after `entry` in its show's visible rows (crossing
/// season headers). `None` if `entry` is in no show, in `(Unsorted)`, in a
/// hidden group, or the last episode.
pub fn next_episode(lib: &Library, entry: usize, hidden: &HashSet<String>) -> Option<usize> {
    if hidden.contains(&lib.entry(entry).group) {
        return None;
    }
    let show = show_of(lib, entry)?;
    let rows = episode_rows(lib, show, hidden);
    let at = rows.iter().position(|r| *r == EpisodeRow::Episode(entry))?;
    rows[at + 1..].iter().find_map(|r| match r {
        EpisodeRow::Episode(e) => Some(*e),
        EpisodeRow::Season(_) => None,
    })
}

/// The show (index in `Library::shows`) that lists episode `entry`, never
/// `(Unsorted)`.
pub fn show_of(lib: &Library, entry: usize) -> Option<usize> {
    lib.shows()
        .iter()
        .position(|s| s.name != UNSORTED && s.seasons.iter().any(|se| se.episodes.contains(&entry)))
}

/// The episodes of `show` in visible groups, in row order (the Up/Down list
/// of a `Playback` that plays one of them).
pub fn show_episodes(lib: &Library, show: usize, hidden: &HashSet<String>) -> Vec<usize> {
    episode_rows(lib, show, hidden)
        .into_iter()
        .filter_map(|r| match r {
            EpisodeRow::Episode(e) => Some(e),
            EpisodeRow::Season(_) => None,
        })
        .collect()
}

/// What the "continue" button plays: `show` (index in `Library::shows`) and
/// the episode `entry` to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinueTarget {
    pub show: usize,
    pub entry: usize,
}

/// Resolve a recent-series record against `lib`: the recorded episode, or the
/// one after it when it reached its credits (the last episode stays itself).
/// `None` when the show or the episode is not (visibly) in the playlist.
pub fn continue_target(lib: &Library, recent: &Recent, hidden: &HashSet<String>) -> Option<ContinueTarget> {
    let show = lib.shows().iter().position(|s| s.name == recent.show && s.name != UNSORTED)?;
    let entry = lib.find_by_key(&recent.key)?;
    if !show_episodes(lib, show, hidden).contains(&entry) {
        return None;
    }
    let entry = if recent.credits { next_episode(lib, entry, hidden).unwrap_or(entry) } else { entry };
    Some(ContinueTarget { show, entry })
}

#[cfg(test)]
mod tests {
    use super::super::vod::tests::{vod, vod_lib};
    use super::*;
    use iptv::playlist::{favorite_key, Kind};

    fn hide(g: &[&str]) -> HashSet<String> {
        g.iter().map(|s| s.to_string()).collect()
    }

    fn recent(lib: &Library, show: &str, entry: usize, credits: bool) -> Recent {
        Recent { show: show.into(), key: favorite_key(lib.entry(entry)), credits }
    }

    #[test]
    fn continue_resumes_the_recorded_episode() {
        let lib = vod_lib();
        let t = continue_target(&lib, &recent(&lib, "Quiet Harbor", 4, false), &HashSet::new()).unwrap();
        assert_eq!(t.entry, 4);
        assert_eq!(lib.shows()[t.show].name, "Quiet Harbor");
    }

    #[test]
    fn continue_after_credits_plays_the_next_episode_or_stays_on_the_last() {
        let lib = vod_lib();
        let h = HashSet::new();
        assert_eq!(continue_target(&lib, &recent(&lib, "Quiet Harbor", 4, true), &h).unwrap().entry, 3);
        assert_eq!(continue_target(&lib, &recent(&lib, "Quiet Harbor", 3, true), &h).unwrap().entry, 5, "next season");
        assert_eq!(continue_target(&lib, &recent(&lib, "Quiet Harbor", 5, true), &h).unwrap().entry, 5);
    }

    #[test]
    fn continue_is_none_when_show_or_episode_is_gone_or_hidden() {
        let lib = vod_lib();
        let h = HashSet::new();
        assert!(continue_target(&lib, &recent(&lib, "No Such Show", 4, false), &h).is_none());
        let mut gone = recent(&lib, "Quiet Harbor", 4, false);
        gone.key = "series:999".into();
        assert!(continue_target(&lib, &gone, &h).is_none());
        assert!(continue_target(&lib, &recent(&lib, "Quiet Harbor", 4, false), &hide(&["Series: Testland"])).is_none());
        assert!(continue_target(&lib, &recent(&lib, "(Unsorted)", 7, false), &h).is_none());
    }

    // vod_lib: Quiet Harbor S01E01 = 4, S01E02 = 3, S02E01 = 5; 7 is (Unsorted); 0 is a movie.

    #[test]
    fn next_within_season() {
        assert_eq!(next_episode(&vod_lib(), 4, &HashSet::new()), Some(3));
    }

    #[test]
    fn last_of_season_goes_to_first_of_next_season() {
        assert_eq!(next_episode(&vod_lib(), 3, &HashSet::new()), Some(5));
    }

    #[test]
    fn last_of_show_is_none() {
        assert_eq!(next_episode(&vod_lib(), 5, &HashSet::new()), None);
    }

    #[test]
    fn unsorted_is_none() {
        assert_eq!(next_episode(&vod_lib(), 7, &HashSet::new()), None);
    }

    #[test]
    fn movie_entry_is_none() {
        assert_eq!(next_episode(&vod_lib(), 0, &HashSet::new()), None);
    }

    #[test]
    fn show_of_and_its_episodes() {
        let lib = vod_lib();
        let show = show_of(&lib, 4).unwrap();
        assert_eq!(lib.shows()[show].name, "Quiet Harbor");
        assert_eq!(show_episodes(&lib, show, &HashSet::new()), vec![4, 3, 5]);
        assert_eq!(show_of(&lib, 7), None, "(Unsorted) is no show");
        assert_eq!(show_of(&lib, 0), None);
    }

    #[test]
    fn skips_hidden_group_episodes() {
        let lib = Library::build(vec![
            vod(Kind::Series, "1", "Quiet Harbor S01E01", "Series: A"),
            vod(Kind::Series, "2", "Quiet Harbor S01E02", "Series: B"),
            vod(Kind::Series, "3", "Quiet Harbor S01E03", "Series: A"),
        ]);
        let h = hide(&["Series: B"]);
        assert_eq!(next_episode(&lib, 0, &HashSet::new()), Some(1));
        assert_eq!(next_episode(&lib, 0, &h), Some(2));
        assert_eq!(next_episode(&lib, 1, &h), None, "a hidden entry has no next");
    }

    fn arcs() -> (Arc<Library>, Hidden) {
        (Arc::new(vod_lib()), Arc::new(HashSet::new()))
    }

    #[test]
    fn cache_hit_does_not_recompute() {
        let (lib, h) = arcs();
        let mut c = NextCache::default();
        assert_eq!(c.get(&lib, Some(4), &h), Some(3));
        assert_eq!(c.get(&lib, Some(4), &h), Some(3));
        assert_eq!(c.computed(), 1);
    }

    #[test]
    fn cache_miss_on_entry_change() {
        let (lib, h) = arcs();
        let mut c = NextCache::default();
        assert_eq!(c.get(&lib, Some(4), &h), Some(3));
        assert_eq!(c.get(&lib, Some(3), &h), Some(5));
        assert_eq!(c.get(&lib, Some(5), &h), None, "last episode");
        assert_eq!(c.get(&lib, None, &h), None, "not in playlist");
        assert_eq!(c.computed(), 4);
    }

    #[test]
    fn cache_miss_on_library_change() {
        let (lib, h) = arcs();
        let mut c = NextCache::default();
        c.get(&lib, Some(4), &h);
        // A refresh: same content, new Arc.
        let swapped = Arc::new(vod_lib());
        assert_eq!(c.get(&swapped, Some(4), &h), Some(3));
        assert_eq!(c.computed(), 2);
        drop(lib);
        assert_eq!(c.get(&swapped, Some(4), &h), Some(3));
        assert_eq!(c.computed(), 2);
    }

    #[test]
    fn cache_miss_on_hidden_change() {
        let lib = Arc::new(Library::build(vec![
            vod(Kind::Series, "1", "Quiet Harbor S01E01", "Series: A"),
            vod(Kind::Series, "2", "Quiet Harbor S01E02", "Series: B"),
            vod(Kind::Series, "3", "Quiet Harbor S01E03", "Series: A"),
        ]));
        let mut c = NextCache::default();
        let none: Hidden = Arc::new(HashSet::new());
        assert_eq!(c.get(&lib, Some(0), &none), Some(1));
        let h: Hidden = Arc::new(hide(&["Series: B"]));
        assert_eq!(c.get(&lib, Some(0), &h), Some(2));
        assert_eq!(c.computed(), 2);
    }

    #[test]
    fn shift_right_decision() {
        assert_eq!(shift_right(false, false, false, true), ShiftRight::PlayNext);
        assert_eq!(shift_right(false, false, false, false), ShiftRight::Swallow, "last episode / live");
        assert_eq!(shift_right(true, false, false, true), ShiftRight::Ignore, "typing");
        assert_eq!(shift_right(false, true, false, true), ShiftRight::Ignore, "tab button focused");
        assert_eq!(shift_right(false, false, true, true), ShiftRight::Ignore, "subtitle menu open");
    }

    #[test]
    fn cache_none_for_non_episodes() {
        let (lib, h) = arcs();
        let mut c = NextCache::default();
        assert_eq!(c.get(&lib, Some(0), &h), None, "movie");
        assert_eq!(c.get(&lib, Some(7), &h), None, "(Unsorted)");
    }
}
