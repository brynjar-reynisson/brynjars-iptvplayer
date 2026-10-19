//! Live tab browsing state: selected country, filter text, filtered channel
//! list with a cursor. No GUI types.

use iptv::library::{normalize, Library};
use iptv::playlist::favorite_key;

use super::nav::ChannelList;

/// Entries of `channels` whose (normalized) name contains every
/// whitespace-separated term of `query` (case- and diacritic-insensitive,
/// same rules as `Library::search`). An empty query keeps everything, in order.
pub fn filter_channels(lib: &Library, channels: &[usize], query: &str) -> Vec<usize> {
    let q = normalize(query);
    let terms: Vec<&str> = q.split_whitespace().collect();
    if terms.is_empty() {
        return channels.to_vec();
    }
    channels
        .iter()
        .copied()
        .filter(|&i| {
            let name = normalize(&lib.entry(i).name);
            terms.iter().all(|t| name.contains(t))
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct LiveBrowser {
    country: Option<usize>,
    filter: String,
    list: ChannelList,
    /// Keyboard focus is on the country list (Up/Down change country).
    country_focus: bool,
}

impl LiveBrowser {
    pub fn new() -> LiveBrowser {
        LiveBrowser::default()
    }

    /// Selected country (index into `Library::countries`).
    pub fn country(&self) -> Option<usize> {
        self.country
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The filtered channel list of the selected country.
    pub fn list(&self) -> &ChannelList {
        &self.list
    }

    pub fn list_mut(&mut self) -> &mut ChannelList {
        &mut self.list
    }

    /// Adopt a (new or refreshed) library: keep the selected country by name
    /// and the selected channel by favorite key when they still exist,
    /// otherwise select the first country.
    pub fn set_library(&mut self, old: Option<&Library>, new: &Library) {
        let old_country = old.zip(self.country).and_then(|(l, c)| l.countries().get(c)).map(|c| c.name.clone());
        let old_key = old.zip(self.list.current()).map(|(l, e)| favorite_key(l.entry(e)));
        let country = old_country
            .and_then(|n| new.countries().iter().position(|c| c.name == n))
            .or(if new.countries().is_empty() { None } else { Some(0) });
        self.country = country;
        self.rebuild(new);
        if let Some(i) = old_key.and_then(|k| new.find_by_key(&k)) {
            self.list.select_entry(i);
        }
    }

    /// Select country `i` (the filter text is kept).
    pub fn select_country(&mut self, lib: &Library, i: usize) {
        if i < lib.countries().len() && self.country != Some(i) {
            self.country = Some(i);
            self.rebuild(lib);
        }
    }

    /// Move to the next/previous country (wrapping); the filter is kept.
    pub fn step_country(&mut self, lib: &Library, delta: i32) {
        let n = lib.countries().len() as i64;
        if n == 0 {
            return;
        }
        let next = match self.country {
            Some(c) => (c as i64 + delta as i64).rem_euclid(n),
            None if delta >= 0 => 0,
            None => n - 1,
        };
        self.select_country(lib, next as usize);
    }

    /// Keyboard focus: country list (true) or channel list (false).
    pub fn countries_focused(&self) -> bool {
        self.country_focus
    }

    pub fn focus_countries(&mut self) {
        self.country_focus = true;
    }

    /// Back to the channel list; selects its first channel if none is selected.
    pub fn focus_channels(&mut self) {
        self.country_focus = false;
        if self.list.cursor().is_none() {
            self.list.step(1);
        }
    }

    /// Change the filter text; keeps the selected channel if still listed.
    pub fn set_filter(&mut self, lib: &Library, text: &str) {
        if self.filter == text {
            return;
        }
        self.filter = text.to_string();
        let keep = self.list.current();
        self.rebuild(lib);
        if let Some(e) = keep {
            self.list.select_entry(e);
        }
    }

    fn rebuild(&mut self, lib: &Library) {
        let items = match self.country.and_then(|c| lib.countries().get(c)) {
            Some(c) => filter_channels(lib, &c.channels, &self.filter),
            None => Vec::new(),
        };
        self.list = ChannelList::new(items);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use iptv::playlist::{Entry, Kind};

    pub(crate) fn live(id: &str, name: &str, group: &str) -> Entry {
        Entry {
            kind: Kind::Live,
            id: id.into(),
            name: name.into(),
            group: group.into(),
            logo: String::new(),
            url: format!("http://host.invalid/USER/PASS/{id}"),
        }
    }

    pub(crate) fn sample_lib() -> Library {
        // Sweden pinned first (countries are otherwise alphabetical).
        Library::build_with_top(
            vec![
                live("1", "SVT1 HD", "Sweden - Nationella"),
                live("2", "SVT2 HD", "Sweden - Nationella"),
                live("3", "NRK1", "Norway - Main"),
                live("4", "TV4 Sport", "Sweden - Sport"),
                live("5", "\u{c5}rets Sport", "Sweden - Sport"),
                live("6", "Kanal 5", "Sweden - Other"),
            ],
            &["Sweden".into()],
        )
    }

    #[test]
    fn first_country_selected_on_library() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        assert_eq!(b.country(), Some(0));
        assert_eq!(b.list().items(), &[0, 1, 3, 4, 5]);
    }

    #[test]
    fn filter_is_diacritic_insensitive_within_country() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        b.set_filter(&lib, "sport");
        assert_eq!(b.list().items(), &[3, 4]);
        b.set_filter(&lib, "arets");
        assert_eq!(b.list().items(), &[4]);
        b.set_filter(&lib, "nrk"); // Norway channel, not in Sweden
        assert!(b.list().is_empty());
        b.set_filter(&lib, "");
        assert_eq!(b.list().len(), 5);
    }

    #[test]
    fn next_prev_in_filtered_list() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        b.set_filter(&lib, "svt");
        let l = b.list_mut();
        assert_eq!(l.step(1), Some(0));
        assert_eq!(l.step(1), Some(1));
        assert_eq!(l.step(1), Some(0), "wraps within the filtered list only");
        assert_eq!(l.step(-1), Some(1));
    }

    #[test]
    fn filter_change_keeps_selected_channel() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        b.list_mut().select_entry(4);
        b.set_filter(&lib, "sport");
        assert_eq!(b.list().current(), Some(4));
        b.set_filter(&lib, "svt");
        assert_eq!(b.list().current(), None);
    }

    #[test]
    fn library_swap_keeps_country_and_channel() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        b.select_country(&lib, 1); // Norway
        b.list_mut().select_entry(2);
        // Refreshed playlist: Norway moved, NRK1 has a new index.
        let new = Library::build(vec![
            live("7", "New", "Denmark - X"),
            live("3", "NRK1", "Norway - Main"),
        ]);
        b.set_library(Some(&lib), &new);
        assert_eq!(new.countries()[b.country().unwrap()].name, "Norway");
        assert_eq!(b.list().current(), Some(1));
    }

    #[test]
    fn step_country_wraps_and_keeps_filter() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        b.set_filter(&lib, "nrk");
        b.step_country(&lib, 1);
        assert_eq!(lib.countries()[b.country().unwrap()].name, "Norway");
        assert_eq!(b.list().items(), &[2], "filter kept across countries");
        b.step_country(&lib, 1);
        assert_eq!(b.country(), Some(0), "wraps forward");
        b.step_country(&lib, -1);
        assert_eq!(lib.countries()[b.country().unwrap()].name, "Norway", "wraps back");
    }

    #[test]
    fn focus_channels_selects_first_when_nothing_selected() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        assert!(!b.countries_focused());
        b.focus_countries();
        assert!(b.countries_focused());
        b.step_country(&lib, 1);
        assert_eq!(b.list().cursor(), None);
        b.focus_channels();
        assert!(!b.countries_focused());
        assert_eq!(b.list().cursor(), Some(0));
        // An existing selection is kept (Sweden has several channels).
        b.focus_countries();
        b.step_country(&lib, -1);
        b.focus_channels();
        b.list_mut().step(1);
        b.focus_countries();
        b.focus_channels();
        assert_eq!(b.list().cursor(), Some(1));
    }
}
