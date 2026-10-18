use crate::playlist::{favorite_key, Entry, Kind};
use crate::series::{Builder, Show};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Country {
    pub name: String,
    pub channels: Vec<usize>,
}

pub struct Library {
    entries: Vec<Entry>,
    countries: Vec<Country>,
    movies: Vec<usize>,
    by_key: HashMap<String, usize>,
    shows: Vec<Show>,
    live_idx: NameIndex,
    movie_idx: NameIndex,
    show_idx: NameIndex,
}

/// Counts only: entries carry credentialed URLs.
impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library")
            .field("entries", &self.entries.len())
            .field("countries", &self.countries.len())
            .field("movies", &self.movies.len())
            .field("shows", &self.shows.len())
            .finish_non_exhaustive()
    }
}

/// Precomputed normalised names, parallel to `ids` (entry index or show index).
#[derive(Debug, Default)]
struct NameIndex {
    ids: Vec<usize>,
    names: Vec<String>,
}

impl NameIndex {
    fn push(&mut self, id: usize, name: &str) {
        self.ids.push(id);
        self.names.push(normalize(name));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Live,
    Movies,
    Series,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHit {
    pub entry: Option<usize>,
    pub show: Option<usize>,
}

fn fold_char(c: char, out: &mut String) {
    let r = match c {
        '\u{e0}'..='\u{e5}' | '\u{c0}'..='\u{c5}' | '\u{100}'..='\u{105}' => 'a',
        '\u{e7}' | '\u{c7}' | '\u{106}'..='\u{10d}' => 'c',
        '\u{10e}'..='\u{111}' | '\u{f0}' | '\u{d0}' => 'd',
        '\u{e8}'..='\u{eb}' | '\u{c8}'..='\u{cb}' | '\u{112}'..='\u{11b}' => 'e',
        '\u{ec}'..='\u{ef}' | '\u{cc}'..='\u{cf}' | '\u{128}'..='\u{131}' => 'i',
        '\u{139}'..='\u{142}' => 'l',
        '\u{f1}' | '\u{d1}' | '\u{143}'..='\u{148}' => 'n',
        '\u{f2}'..='\u{f6}' | '\u{d2}'..='\u{d6}' | '\u{f8}' | '\u{d8}' | '\u{14c}'..='\u{151}' => 'o',
        '\u{154}'..='\u{159}' => 'r',
        '\u{15a}'..='\u{161}' => 's',
        '\u{162}'..='\u{167}' => 't',
        '\u{f9}'..='\u{fc}' | '\u{d9}'..='\u{dc}' | '\u{168}'..='\u{173}' => 'u',
        '\u{fd}' | '\u{ff}' | '\u{dd}' | '\u{176}'..='\u{178}' => 'y',
        '\u{179}'..='\u{17e}' => 'z',
        '\u{e6}' | '\u{c6}' => {
            out.push_str("ae");
            return;
        }
        '\u{153}' | '\u{152}' => {
            out.push_str("oe");
            return;
        }
        '\u{df}' => {
            out.push_str("ss");
            return;
        }
        _ => {
            out.extend(c.to_lowercase());
            return;
        }
    };
    out.push(r);
}

/// Lowercase and fold common Latin diacritics to base letters (no dependency).
/// Case- and diacritic-insensitive form used by search (exposed so the UI can
/// filter a sub-list with the same rules).
pub fn normalize(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        fold_char(c, &mut out);
    }
    out
}

const OTHER: &str = "Other";

impl Library {
    pub fn build(entries: Vec<Entry>) -> Library {
        Self::build_with_top(entries, None)
    }

    /// Countries are listed alphabetically (case/diacritic-insensitive),
    /// "Other" last; `top_country` (matched the same way) goes first.
    pub fn build_with_top(entries: Vec<Entry>, top_country: Option<&str>) -> Library {
        let mut countries: Vec<Country> = Vec::new();
        let mut pos: HashMap<String, usize> = HashMap::new();
        let mut other: Vec<usize> = Vec::new();
        let mut movies = Vec::new();
        let mut series = Builder::new();
        let mut live_idx = NameIndex::default();
        let mut movie_idx = NameIndex::default();
        let mut by_key = HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            by_key.entry(favorite_key(e)).or_insert(i);
            match e.kind {
                Kind::Live => {
                    live_idx.push(i, &e.name);
                    // An empty country (" - X") and a real "Other - X" join
                    // the catch-all "Other" (always last, never duplicated).
                    match e.group.split_once(" - ").filter(|(c, _)| !c.trim().is_empty() && *c != OTHER) {
                        Some((c, _)) => {
                            let p = *pos.entry(c.to_string()).or_insert_with(|| {
                                countries.push(Country { name: c.to_string(), channels: Vec::new() });
                                countries.len() - 1
                            });
                            countries[p].channels.push(i);
                        }
                        None => other.push(i),
                    }
                }
                Kind::Movie => {
                    movies.push(i);
                    movie_idx.push(i, &e.name);
                }
                Kind::Series => series.add(i, &e.name),
            }
        }
        countries.sort_by_cached_key(|c| normalize(&c.name));
        if let Some(top) = top_country.map(|t| normalize(t.trim())).filter(|t| !t.is_empty()) {
            if let Some(p) = countries.iter().position(|c| normalize(&c.name) == top) {
                countries[..=p].rotate_right(1);
            }
        }
        if !other.is_empty() {
            countries.push(Country { name: OTHER.to_string(), channels: other });
        }
        let shows = series.finish();
        let mut show_idx = NameIndex::default();
        for (i, s) in shows.iter().enumerate() {
            show_idx.push(i, &s.name);
        }
        Library { entries, countries, movies, by_key, shows, live_idx, movie_idx, show_idx }
    }

    /// Case- and diacritic-insensitive substring search; every whitespace-separated
    /// term must match. Hits are in file order (shows in first-seen order) and stop
    /// at `limit`. Series hits carry `show`, Live/Movies hits carry `entry`.
    pub fn search(&self, scope: SearchScope, query: &str, limit: usize) -> Vec<SearchHit> {
        let q = normalize(query);
        let terms: Vec<&str> = q.split_whitespace().collect();
        let mut hits = Vec::new();
        if terms.is_empty() || limit == 0 {
            return hits;
        }
        let idx = match scope {
            SearchScope::Live => &self.live_idx,
            SearchScope::Movies => &self.movie_idx,
            SearchScope::Series => &self.show_idx,
        };
        for (id, name) in idx.ids.iter().zip(&idx.names) {
            if terms.iter().all(|t| name.contains(t)) {
                hits.push(match scope {
                    SearchScope::Series => SearchHit { entry: None, show: Some(*id) },
                    _ => SearchHit { entry: Some(*id), show: None },
                });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        hits
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn countries(&self) -> &[Country] {
        &self.countries
    }

    pub fn shows(&self) -> &[Show] {
        &self.shows
    }

    pub fn entry(&self, idx: usize) -> &Entry {
        &self.entries[idx]
    }

    pub fn movies(&self) -> &[usize] {
        &self.movies
    }

    pub fn find_by_key(&self, key: &str) -> Option<usize> {
        self.by_key.get(key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: Kind, id: &str, name: &str, group: &str) -> Entry {
        Entry {
            kind,
            id: id.into(),
            name: name.into(),
            group: group.into(),
            logo: String::new(),
            url: format!("http://host.invalid/USER/PASS/{id}"),
        }
    }

    #[test]
    fn merges_groups_with_same_country_prefix() {
        let lib = Library::build(vec![
            e(Kind::Live, "1", "A", "Sweden - Nationella"),
            e(Kind::Live, "2", "B", "Norway - News"),
            e(Kind::Live, "3", "C", "Sweden - Sport"),
        ]);
        let c = lib.countries();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "Norway");
        assert_eq!(c[0].channels, vec![1]);
        assert_eq!(c[1].name, "Sweden");
        assert_eq!(c[1].channels, vec![0, 2]);
    }

    #[test]
    fn countries_alphabetical_with_configurable_top_other_last() {
        let es = || {
            vec![
                e(Kind::Live, "1", "A", "Sweden - X"),
                e(Kind::Live, "2", "B", "Iceland - X"),
                e(Kind::Live, "3", "C", "österreich - X"),
                e(Kind::Live, "4", "D", "Misc"),
                e(Kind::Live, "5", "E", "Norway - X"),
            ]
        };
        let names = |l: &Library| l.countries().iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&Library::build(es())), ["Iceland", "Norway", "österreich", "Sweden", "Other"]);
        let top = Library::build_with_top(es(), Some(" sweden "));
        assert_eq!(names(&top), ["Sweden", "Iceland", "Norway", "österreich", "Other"]);
        assert_eq!(top.countries()[0].channels, vec![0]);
        // Unknown or blank top country changes nothing.
        assert_eq!(names(&Library::build_with_top(es(), Some("Atlantis"))), names(&Library::build(es())));
        assert_eq!(names(&Library::build_with_top(es(), Some(""))), names(&Library::build(es())));
        // "Other" stays last even if named as top.
        assert_eq!(names(&Library::build_with_top(es(), Some("Other"))).last().unwrap(), "Other");
    }

    #[test]
    fn group_without_dash_goes_to_other_last() {
        let lib = Library::build(vec![
            e(Kind::Live, "1", "A", "Misc"),
            e(Kind::Live, "2", "B", "Sweden - X"),
            e(Kind::Movie, "3", "M", "Action"),
        ]);
        let c = lib.countries();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "Sweden");
        assert_eq!(c[1].name, "Other");
        assert_eq!(c[1].channels, vec![0]);
        assert_eq!(lib.movies(), &[2]);
    }

    #[test]
    fn other_country_and_empty_country_merge_into_one_other() {
        let lib = Library::build(vec![
            e(Kind::Live, "1", "A", "Other - Sports"),
            e(Kind::Live, "2", "B", "Misc"),
            e(Kind::Live, "3", "C", "Sweden - X"),
            e(Kind::Live, "4", "D", " - Loose"),
            e(Kind::Live, "5", "E", "A - B - C"),
        ]);
        let names: Vec<&str> = lib.countries().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["A", "Sweden", "Other"]);
        assert_eq!(lib.countries()[2].channels, vec![0, 1, 3]);
        assert_eq!(lib.countries()[0].channels, vec![4]);
    }

    #[test]
    fn debug_hides_urls() {
        let lib = Library::build(vec![
            e(Kind::Live, "1", "A", "Sweden - X"),
            Entry { logo: "http://logo.invalid/x.png".into(), ..e(Kind::Movie, "2", "M", "Action") },
        ]);
        for d in [format!("{lib:?}"), format!("{:?}", lib.entry(0)), format!("{:?}", lib.entry(1))] {
            assert!(!d.contains("host.invalid") && !d.contains("logo.invalid"), "{d}");
            assert!(!d.contains("USER") && !d.contains("PASS") && !d.contains("://"), "{d}");
        }
    }

    #[test]
    fn third_party_master_m3u8_entries_have_distinct_keys() {
        let m = "#EXTINF:-1 group-title=\"Sweden - X\",Alpha\nhttp://cdn.invalid/a/master.m3u8\n\
#EXTINF:-1 group-title=\"Sweden - X\",Beta\nhttp://cdn.invalid/b/master.m3u8\n\
#EXTINF:-1 group-title=\"Sweden - X\",Num\nhttp://host.invalid/USER/PASS/123\n";
        let es = crate::playlist::parse(std::io::Cursor::new(m));
        let lib = Library::build(es);
        for i in 0..3 {
            assert_eq!(lib.find_by_key(&favorite_key(lib.entry(i))), Some(i));
        }
        // A favorite saved by an earlier version (numeric id key) still resolves.
        assert_eq!(lib.find_by_key("live:123"), Some(2));
        assert_eq!(lib.find_by_key("live:master"), None);
    }

    #[test]
    fn find_by_key_roundtrips_favorite_key() {
        let es = vec![
            e(Kind::Live, "1", "A", "Sweden - X"),
            e(Kind::Movie, "2", "M", "Action"),
            e(Kind::Live, "", "No Id", "Other"),
        ];
        let keys: Vec<String> = es.iter().map(favorite_key).collect();
        let lib = Library::build(es);
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(lib.find_by_key(k), Some(i));
            assert_eq!(favorite_key(lib.entry(i)), *k);
        }
        assert_eq!(lib.find_by_key("live:nope"), None);
    }

    #[test]
    fn library_is_send_sync() {
        fn ok<T: Send + Sync>() {}
        ok::<Library>();
    }

    fn lib3() -> Library {
        Library::build(vec![
            e(Kind::Movie, "1", "The Red Falcon", "Action"),
            e(Kind::Movie, "2", "Red Harbor Story", "Drama"),
            e(Kind::Movie, "3", "Le Fran\u{e7}ais Tranquille", "Drama"),
            e(Kind::Live, "4", "SVT \u{c5}rjang", "Sweden - News"),
            e(Kind::Series, "5", "Blue Island S01 E01", "S"),
            e(Kind::Series, "6", "Blue Island S01 E02", "S"),
            e(Kind::Series, "7", "Red Moon S01 E01", "S"),
        ])
    }

    #[test]
    fn all_terms_must_match() {
        let l = lib3();
        let h = l.search(SearchScope::Movies, "red falcon", 10);
        assert_eq!(h, vec![SearchHit { entry: Some(0), show: None }]);
        assert_eq!(l.search(SearchScope::Movies, "red", 10).len(), 2);
        assert!(l.search(SearchScope::Movies, "red zebra", 10).is_empty());
    }

    #[test]
    fn case_and_diacritics_insensitive() {
        let l = lib3();
        assert_eq!(l.search(SearchScope::Movies, "francais", 10)[0].entry, Some(2));
        assert_eq!(l.search(SearchScope::Movies, "FRAN\u{c7}AIS", 10)[0].entry, Some(2));
        assert_eq!(l.search(SearchScope::Live, "arjang", 10)[0].entry, Some(3));
        assert_eq!(l.search(SearchScope::Live, "\u{e5}rjang", 10).len(), 1);
    }

    #[test]
    fn series_hits_are_shows() {
        let l = lib3();
        let h = l.search(SearchScope::Series, "island", 10);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].entry, None);
        assert_eq!(l.shows()[h[0].show.unwrap()].name, "Blue Island");
        assert_eq!(l.search(SearchScope::Series, "red", 10).len(), 1);
    }

    #[test]
    fn empty_query_returns_empty() {
        let l = lib3();
        assert!(l.search(SearchScope::Movies, "", 10).is_empty());
        assert!(l.search(SearchScope::Movies, "  \t ", 10).is_empty());
    }

    #[test]
    fn limit_respected() {
        let l = lib3();
        assert_eq!(l.search(SearchScope::Movies, "r", 1).len(), 1);
        assert!(l.search(SearchScope::Movies, "r", 0).is_empty());
    }

    #[test]
    #[ignore]
    fn sample_search_perf() {
        let Ok(p) = std::env::var("IPTV_SAMPLE") else { return };
        let f = std::io::BufReader::new(std::fs::File::open(p).unwrap());
        let es = crate::playlist::parse(f);
        let t = std::time::Instant::now();
        let lib = Library::build(es);
        println!("build={:?}", t.elapsed());
        let words = [
            "the", "ca", "man", "a", "e", "s", "love", "dark", "night", "king", "war", "life", "star", "big", "new",
            "red", "city", "dead", "last", "first", "home", "world", "blood", "time", "day", "girl", "boy", "house",
            "fire", "dream", "zz", "xq", "qj", "zx", "ab", "co", "de", "la", "le", "un",
        ];
        let mut qs: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        for i in 0..words.len() {
            qs.push(format!("{} {}", words[i], words[(i * 7 + 3) % words.len()]));
        }
        for i in 0..words.len() {
            qs.push(format!("{} {} {}", words[i], words[(i * 5 + 1) % words.len()], words[(i * 11 + 2) % words.len()]));
        }
        qs.push("qqqqzzzz nomatchatall".into());
        qs.push("the the the the".into());
        for c in 'a'..='z' {
            qs.push(c.to_string());
        }
        qs.push("fran\u{e7}ais".into());
        qs.push("\u{e5}".into());
        for (a, b) in [('t', 'h'), ('e', 'r'), ('o', 'n'), ('s', 't'), ('m', 'a')] {
            for c in 'a'..='j' {
                qs.push(format!("{a}{b}{c}"));
            }
        }
        qs.truncate(200);
        let mut times = Vec::new();
        for q in &qs {
            for sc in [SearchScope::Movies, SearchScope::Series] {
                let t = std::time::Instant::now();
                let h = lib.search(sc, q, 500);
                times.push(t.elapsed());
                std::hint::black_box(h);
            }
        }
        times.sort();
        let p95 = times[times.len() * 95 / 100];
        println!("queries={} searches={} max={:?} p95={:?}", qs.len(), times.len(), times.last().unwrap(), p95);
        assert!(*times.last().unwrap() < std::time::Duration::from_millis(50));
    }

    #[test]
    #[ignore]
    fn sample_build_time() {
        let Ok(p) = std::env::var("IPTV_SAMPLE") else { return };
        let f = std::io::BufReader::new(std::fs::File::open(p).unwrap());
        let es = crate::playlist::parse(f);
        let n = es.len();
        let t = std::time::Instant::now();
        let lib = Library::build(es);
        let el = t.elapsed();
        let un = lib.shows().iter().find(|s| s.name == crate::series::UNSORTED).map(|s| s.seasons[0].episodes.len()).unwrap_or(0);
        println!("entries={n} countries={} movies={} shows={} unsorted={un} build={el:?}", lib.countries().len(), lib.movies().len(), lib.shows().len());
    }
}
