use crate::playlist::{favorite_key, Entry, Kind};
use crate::series::{Builder, Show};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Country {
    pub name: String,
    pub channels: Vec<usize>,
}

#[derive(Debug)]
pub struct Library {
    entries: Vec<Entry>,
    countries: Vec<Country>,
    movies: Vec<usize>,
    by_key: HashMap<String, usize>,
    shows: Vec<Show>,
}

const OTHER: &str = "Other";

impl Library {
    pub fn build(entries: Vec<Entry>) -> Library {
        let mut countries: Vec<Country> = Vec::new();
        let mut pos: HashMap<String, usize> = HashMap::new();
        let mut other: Vec<usize> = Vec::new();
        let mut movies = Vec::new();
        let mut series = Builder::new();
        let mut by_key = HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            by_key.entry(favorite_key(e)).or_insert(i);
            match e.kind {
                Kind::Live => match e.group.split_once(" - ") {
                    Some((c, _)) => {
                        let p = *pos.entry(c.to_string()).or_insert_with(|| {
                            countries.push(Country { name: c.to_string(), channels: Vec::new() });
                            countries.len() - 1
                        });
                        countries[p].channels.push(i);
                    }
                    None => other.push(i),
                },
                Kind::Movie => movies.push(i),
                Kind::Series => series.add(i, &e.name),
            }
        }
        if !other.is_empty() {
            countries.push(Country { name: OTHER.to_string(), channels: other });
        }
        Library { entries, countries, movies, by_key, shows: series.finish() }
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
        assert_eq!(c[0].name, "Sweden");
        assert_eq!(c[0].channels, vec![0, 2]);
        assert_eq!(c[1].name, "Norway");
        assert_eq!(c[1].channels, vec![1]);
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
