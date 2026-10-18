//! Series grouping.
//!
//! Findings from a survey of the real playlist (names only; ~195k series entries):
//! - Every series entry is an episode display name, e.g. `Show (2019) S01 E02`
//!   (note the optional space between season and episode). All but one parse.
//! - ~21% carry a language/group prefix: `NC - `, `NO - `, `SE - `, `DK -`,
//!   `NC:`, `SE:`, `NC :` and `Vod: `. Rule: strip exactly two ASCII uppercase
//!   letters followed by `:` or `-` (with whitespace rules below), plus the
//!   literal `Vod:` and `Series:`. 3+ letter prefixes (e.g. `XYZ:`) are real
//!   titles and are NOT stripped.
//! - A trailing country tag like `(US)`/`(AR)` and a year `(2019)` are kept in
//!   the show name: "Show (2019)" and "Show" are deliberately NOT merged because
//!   they are frequently different shows (remakes). Only case and whitespace
//!   differences merge.
//! - No `1x02` forms were seen, but they are supported.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeRef {
    pub show: String,
    pub season: u32,
    pub episode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Season {
    pub number: Option<u32>,
    pub episodes: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Show {
    pub name: String,
    pub seasons: Vec<Season>,
}

pub const UNSORTED: &str = "(Unsorted)";

fn strip_prefix(s: &str) -> &str {
    let s = s.trim_start();
    for lit in ["Vod:", "Series:"] {
        if let Some(r) = s.strip_prefix(lit) {
            return r.trim_start();
        }
    }
    let b = s.as_bytes();
    if b.len() > 2 && b[0].is_ascii_uppercase() && b[1].is_ascii_uppercase() {
        let rest = &s[2..];
        let t = rest.trim_start();
        if let Some(r) = t.strip_prefix(':') {
            return r.trim_start();
        }
        if let Some(r) = t.strip_prefix('-') {
            if r.starts_with(char::is_whitespace) {
                return r.trim_start();
            }
        }
    }
    s
}

fn digits(b: &[u8], mut i: usize) -> (usize, Option<u32>) {
    let st = i;
    while i < b.len() && b[i].is_ascii_digit() && i - st < 4 {
        i += 1;
    }
    if i == st {
        return (i, None);
    }
    (i, std::str::from_utf8(&b[st..i]).ok().and_then(|d| d.parse().ok()))
}

/// Match `S<d> [ws] E<d>` or `<d>x<d>` at `i`; returns (end, season, episode).
fn marker_at(b: &[u8], i: usize) -> Option<(usize, u32, u32)> {
    if i > 0 && b[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let (end, s, e);
    if b[i] == b's' || b[i] == b'S' {
        let (j, sv) = digits(b, i + 1);
        let mut j2 = j;
        while j2 < b.len() && b[j2] == b' ' {
            j2 += 1;
        }
        if j2 >= b.len() || !(b[j2] == b'e' || b[j2] == b'E') {
            return None;
        }
        let (k, ev) = digits(b, j2 + 1);
        end = k;
        s = sv?;
        e = ev?;
    } else if b[i].is_ascii_digit() {
        let (j, sv) = digits(b, i);
        // NxM style: reject resolutions like 1920x1080 (season <= 99, episode <= 999)
        if sv.map_or(true, |v| v > 99) {
            return None;
        }
        if j >= b.len() || !(b[j] == b'x' || b[j] == b'X') {
            return None;
        }
        let (k, ev) = digits(b, j + 1);
        if ev.map_or(true, |v| v > 999) {
            return None;
        }
        end = k;
        s = sv?;
        e = ev?;
    } else {
        return None;
    }
    if end < b.len() && b[end].is_ascii_alphanumeric() {
        return None;
    }
    Some((end, s, e))
}

/// First valid marker in `name`: (start, end, season, episode).
fn find_marker(name: &str) -> Option<(usize, usize, u32, u32)> {
    let b = name.as_bytes();
    (0..b.len())
        .filter(|&i| name.is_char_boundary(i))
        .find_map(|i| marker_at(b, i).map(|(end, s, e)| (i, end, s, e)))
}

pub fn parse_episode(name: &str) -> Option<EpisodeRef> {
    let name = strip_prefix(name);
    let (i, _, season, episode) = find_marker(name)?;
    let show = name[..i].trim().trim_end_matches([':', '-']).trim();
    if show.is_empty() {
        return None;
    }
    let mut show = show.split_whitespace().collect::<Vec<_>>().join(" ");
    // Reserve the catch-all name: a real show called "(Unsorted)" must not collide.
    if show.eq_ignore_ascii_case(UNSORTED) {
        show.push_str(" (show)");
    }
    Some(EpisodeRef { show, season, episode })
}

/// Short label for an episode row inside its show: `S01E02`, plus whatever
/// follows the marker (an episode title), e.g. `S01E02 - The Visit`. `None`
/// if the name does not parse (callers show the full name).
pub fn episode_label(name: &str) -> Option<String> {
    parse_episode(name)?;
    let name = strip_prefix(name);
    let (_, end, season, episode) = find_marker(name)?;
    let rest = name[end..].trim_start_matches(|c: char| c.is_whitespace() || c == '-' || c == ':' || c == '.').trim_end();
    let mut label = format!("S{season:02}E{episode:02}");
    if !rest.is_empty() {
        label.push_str(" - ");
        label.push_str(rest);
    }
    Some(label)
}

fn key_of(show: &str) -> String {
    show.to_lowercase()
}

pub(crate) struct Builder {
    pos: std::collections::HashMap<String, usize>,
    // per show: name, season -> [(episode, entry idx)]
    shows: Vec<(String, std::collections::BTreeMap<u32, Vec<(u32, usize)>>)>,
    unsorted: Vec<usize>,
}

impl Builder {
    pub fn new() -> Self {
        Builder { pos: Default::default(), shows: Vec::new(), unsorted: Vec::new() }
    }

    pub fn add(&mut self, idx: usize, name: &str) {
        match parse_episode(name) {
            Some(r) => {
                let k = key_of(&r.show);
                let p = match self.pos.get(&k) {
                    Some(&p) => p,
                    None => {
                        self.shows.push((r.show, Default::default()));
                        self.pos.insert(k, self.shows.len() - 1);
                        self.shows.len() - 1
                    }
                };
                self.shows[p].1.entry(r.season).or_default().push((r.episode, idx));
            }
            None => self.unsorted.push(idx),
        }
    }

    pub fn finish(self) -> Vec<Show> {
        let mut shows: Vec<Show> = self
            .shows
            .into_iter()
            .map(|(name, seasons)| Show {
                name,
                seasons: seasons
                    .into_iter()
                    .map(|(n, mut eps)| {
                        eps.sort_by_key(|&(e, i)| (e, i));
                        Season { number: Some(n), episodes: eps.into_iter().map(|(_, i)| i).collect() }
                    })
                    .collect(),
            })
            .collect();
        shows.sort_by_cached_key(|s| s.name.to_lowercase());
        if !self.unsorted.is_empty() {
            shows.push(Show {
                name: UNSORTED.to_string(),
                seasons: vec![Season { number: None, episodes: self.unsorted }],
            });
        }
        shows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::playlist::{Entry, Kind};

    fn r(show: &str, season: u32, episode: u32) -> Option<EpisodeRef> {
        Some(EpisodeRef { show: show.into(), season, episode })
    }

    fn s(id: usize, name: &str) -> Entry {
        Entry {
            kind: Kind::Series,
            id: id.to_string(),
            name: name.into(),
            group: "G".into(),
            logo: String::new(),
            url: format!("http://host.invalid/series/U/P/{id}.mkv"),
        }
    }

    #[test]
    fn parses_sxxexx_variants() {
        assert_eq!(parse_episode("Fake Show S01E02"), r("Fake Show", 1, 2));
        assert_eq!(parse_episode("Fake Show s1e2"), r("Fake Show", 1, 2));
        assert_eq!(parse_episode("Fake Show 1x02"), r("Fake Show", 1, 2));
        assert_eq!(parse_episode("Fake Show S01 E02"), r("Fake Show", 1, 2));
        assert_eq!(parse_episode("Show (2019) S02 E10"), r("Show (2019)", 2, 10));
        assert_eq!(parse_episode("Show - S01E01"), r("Show", 1, 1));
    }

    #[test]
    fn strips_language_prefix_but_not_real_titles() {
        assert_eq!(parse_episode("SE: Fake Show S01E01"), r("Fake Show", 1, 1));
        assert_eq!(parse_episode("NC - Fake Show S01E01"), r("Fake Show", 1, 1));
        assert_eq!(parse_episode("DK - Fake Show S01E01"), r("Fake Show", 1, 1));
        assert_eq!(parse_episode("Vod: Fake Show S01E01"), r("Fake Show", 1, 1));
        assert_eq!(parse_episode("XYZ: Nowhere S01E01"), r("XYZ: Nowhere", 1, 1));
        assert_eq!(parse_episode("Yu-Fake S01E01"), r("Yu-Fake", 1, 1));
    }

    #[test]
    fn first_marker_wins() {
        assert_eq!(parse_episode("Fake Show S01E01 S01E02"), r("Fake Show", 1, 1));
        assert_eq!(parse_episode("Fake Show S01E02 Title 1x03"), r("Fake Show", 1, 2));
    }

    #[test]
    fn resolutions_do_not_parse() {
        assert_eq!(parse_episode("Fake Show 1920x1080"), None);
        assert_eq!(parse_episode("Fake Show 720x480 S01E01"), r("Fake Show 720x480", 1, 1));
    }

    #[test]
    fn show_named_unsorted_does_not_collide() {
        let lib = Library::build(vec![s(1, "(Unsorted) S01E01"), s(2, "junk")]);
        let sh = lib.shows();
        assert_eq!(sh.len(), 2);
        assert_eq!(sh[0].name, "(Unsorted) (show)");
        assert_eq!(sh[1].name, UNSORTED);
        assert_eq!(sh[1].seasons[0].episodes, vec![1]);
    }

    #[test]
    fn episode_labels() {
        assert_eq!(episode_label("Fake Show S01E02").as_deref(), Some("S01E02"));
        assert_eq!(episode_label("SE: Fake Show S1 E2 - The Visit").as_deref(), Some("S01E02 - The Visit"));
        assert_eq!(episode_label("Fake Show 2x10: Finale").as_deref(), Some("S02E10 - Finale"));
        assert_eq!(episode_label("Fake Show (2019) S03E104 Part B").as_deref(), Some("S03E104 - Part B"));
        assert_eq!(episode_label("Weird Name"), None);
    }

    #[test]
    fn non_episode_names_do_not_parse() {
        assert_eq!(parse_episode("Just A Movie Title"), None);
        assert_eq!(parse_episode("S01E02"), None);
        assert_eq!(parse_episode("Show Season 1"), None);
        assert_eq!(parse_episode("Classes01e02x"), None);
    }

    #[test]
    fn unparseable_name_goes_to_unsorted() {
        let lib = Library::build(vec![s(1, "Weird Name"), s(2, "A S01E01")]);
        let sh = lib.shows();
        assert_eq!(sh.len(), 2);
        assert_eq!(sh[0].name, "A");
        assert_eq!(sh[1].name, "(Unsorted)");
        assert_eq!(sh[1].seasons, vec![Season { number: None, episodes: vec![0] }]);
    }

    #[test]
    fn same_show_different_case_merges() {
        let lib = Library::build(vec![
            s(1, "Fake Show S01E01"),
            s(2, "FAKE SHOW  S01E02"),
            s(3, "SE: fake show S01E03"),
            s(4, "Fake Show (2019) S01E01"),
        ]);
        let sh = lib.shows();
        assert_eq!(sh.len(), 2, "year variant stays a separate show");
        assert_eq!(sh[0].name, "Fake Show");
        assert_eq!(sh[0].seasons[0].episodes, vec![0, 1, 2]);
        assert_eq!(sh[1].name, "Fake Show (2019)");
    }

    #[test]
    fn episodes_sorted_within_season() {
        let lib = Library::build(vec![
            s(1, "X S02E01"),
            s(2, "X S01E10"),
            s(3, "X S01E2"),
            s(4, "X S01E01"),
        ]);
        let sh = lib.shows();
        assert_eq!(sh.len(), 1);
        let nums: Vec<_> = sh[0].seasons.iter().map(|x| x.number).collect();
        assert_eq!(nums, vec![Some(1), Some(2)]);
        assert_eq!(sh[0].seasons[0].episodes, vec![3, 2, 1]);
        assert_eq!(sh[0].seasons[1].episodes, vec![0]);
    }

    #[test]
    fn non_series_ignored_and_nothing_vanishes() {
        let mut m = s(1, "Movie S01E01");
        m.kind = Kind::Movie;
        let lib = Library::build(vec![m, s(2, "Q S01E01"), s(3, "junk")]);
        let total: usize = lib.shows().iter().flat_map(|x| &x.seasons).map(|x| x.episodes.len()).sum();
        assert_eq!(total, 2);
    }
}
