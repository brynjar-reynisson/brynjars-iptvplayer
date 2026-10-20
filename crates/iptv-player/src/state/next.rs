//! Which episode follows the one that is playing (autoplay next episode).

use std::collections::HashSet;

use iptv::library::Library;
use iptv::series::UNSORTED;

use super::vod::{episode_rows, EpisodeRow};

/// The entry of the episode after `entry` in its show's visible rows (crossing
/// season headers). `None` if `entry` is in no show, in `(Unsorted)`, in a
/// hidden group, or the last episode.
pub fn next_episode(lib: &Library, entry: usize, hidden: &HashSet<String>) -> Option<usize> {
    if hidden.contains(&lib.entry(entry).group) {
        return None;
    }
    let show = lib
        .shows()
        .iter()
        .position(|s| s.name != UNSORTED && s.seasons.iter().any(|se| se.episodes.contains(&entry)))?;
    let rows = episode_rows(lib, show, hidden);
    let at = rows.iter().position(|r| *r == EpisodeRow::Episode(entry))?;
    rows[at + 1..].iter().find_map(|r| match r {
        EpisodeRow::Episode(e) => Some(*e),
        EpisodeRow::Season(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::vod::tests::{vod, vod_lib};
    use super::*;
    use iptv::playlist::Kind;

    fn hide(g: &[&str]) -> HashSet<String> {
        g.iter().map(|s| s.to_string()).collect()
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
}
