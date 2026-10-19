//! Episode titles from TVmaze: name cleaning, response parsing/matching and
//! an on-disk cache with TTLs (`episode-titles.json`). Pure: no HTTP here, and
//! `now` (unix seconds) is always passed in. Same safety rules as settings:
//! atomic writes, an unreadable file is moved aside to `.bad[.N]`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u64 = 1;
const HIT_TTL: u64 = 30 * 24 * 3600;
const MISS_TTL: u64 = 24 * 3600;
const ERROR_TTL: u64 = 3600;
const MAX_ENTRIES: usize = 500;

/// Episode titles by (season, episode).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Titles(BTreeMap<(u32, u32), String>);

impl Titles {
    pub fn get(&self, season: u32, episode: u32) -> Option<&str> {
        self.0.get(&(season, episode)).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn to_map(&self) -> BTreeMap<String, String> {
        self.0.iter().map(|((s, e), t)| (format!("{s}:{e}"), t.clone())).collect()
    }

    fn from_map(m: &BTreeMap<String, String>) -> Titles {
        let mut out = BTreeMap::new();
        for (k, t) in m {
            if let Some((s, e)) = k.split_once(':') {
                if let (Ok(s), Ok(e)) = (s.parse(), e.parse()) {
                    out.insert((s, e), t.clone());
                }
            }
        }
        Titles(out)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub name: String,
    pub year: Option<u32>,
}

/// Show name as used in the playlist -> search name + optional year.
/// Drops trailing `(YYYY)` (into `year`) and a country tag like `(US)`/`(UK)`.
pub fn clean_query(show: &str) -> Query {
    let mut name = show.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut year = None;
    loop {
        let Some(open) = name.rfind('(') else { break };
        if !name.ends_with(')') {
            break;
        }
        let inner = &name[open + 1..name.len() - 1];
        if inner.len() == 4 && inner.chars().all(|c| c.is_ascii_digit()) && year.is_none() {
            year = inner.parse().ok();
        } else if (2..=3).contains(&inner.len()) && inner.chars().all(|c| c.is_ascii_alphabetic()) {
            // country tag
        } else {
            break;
        }
        name = name[..open].trim_end().to_string();
    }
    Query { name, year }
}

pub fn cache_key(q: &Query) -> String {
    let n = crate::library::normalize(&q.name);
    match q.year {
        Some(y) => format!("{n}|{y}"),
        None => n,
    }
}

#[derive(Deserialize)]
struct ApiShow {
    name: Option<String>,
    premiered: Option<String>,
    #[serde(rename = "_embedded")]
    embedded: Option<ApiEmbedded>,
}

#[derive(Deserialize)]
struct ApiEmbedded {
    #[serde(default)]
    episodes: Vec<ApiEpisode>,
}

#[derive(Deserialize)]
struct ApiEpisode {
    season: Option<u32>,
    number: Option<u32>,
    name: Option<String>,
}

/// Parse a TVmaze `singlesearch/shows?embed=episodes` body. `Ok(None)` = the
/// show does not match the query (name, or year when both are known).
pub fn parse_response(json: &[u8], q: &Query) -> Result<Option<Titles>> {
    let show: ApiShow = serde_json::from_slice(json).context("invalid TVmaze response")?;
    let Some(name) = show.name else { return Ok(None) };
    if crate::library::normalize(&name) != crate::library::normalize(&q.name) {
        return Ok(None);
    }
    if let (Some(want), Some(p)) = (q.year, show.premiered.as_deref()) {
        let got: Option<u32> = p.get(..4).and_then(|y| y.parse().ok());
        if got.is_some_and(|g| g.abs_diff(want) > 1) {
            return Ok(None);
        }
    }
    let mut t = BTreeMap::new();
    for e in show.embedded.map(|e| e.episodes).unwrap_or_default() {
        if let (Some(s), Some(n), Some(title)) = (e.season, e.number, e.name) {
            let title = title.trim();
            if !title.is_empty() {
                t.insert((s, n), title.to_string());
            }
        }
    }
    Ok(Some(Titles(t)))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    Hit(Titles),
    Miss,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Hit,
    Miss,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    at: u64,
    #[serde(rename = "status")]
    kind: Kind,
    #[serde(default)]
    titles: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u64,
    #[serde(default, rename = "shows")]
    items: BTreeMap<String, Entry>,
}

#[derive(Debug)]
pub struct TitleCache {
    dir: PathBuf,
    items: BTreeMap<String, Entry>,
    dirty: bool,
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("episode-titles.json")
}

fn parse_file(bytes: &[u8]) -> Option<BTreeMap<String, Entry>> {
    let f: CacheFile = serde_json::from_slice(bytes).ok()?;
    (f.version == FORMAT_VERSION).then_some(f.items)
}

impl TitleCache {
    pub fn load(dir: &Path) -> TitleCache {
        let path = path_in(dir);
        let mut c = TitleCache { dir: dir.to_path_buf(), items: BTreeMap::new(), dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return c,
            Err(_) => true,
            Ok(bytes) => match parse_file(&bytes) {
                Some(items) => {
                    c.items = items;
                    false
                }
                None => true,
            },
        };
        if unreadable {
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = std::fs::rename(&path, &bad) {
                c.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        c
    }

    pub fn get(&self, key: &str, now: u64) -> Lookup {
        let Some(e) = self.items.get(key) else { return Lookup::Unknown };
        let ttl = match e.kind {
            Kind::Hit => HIT_TTL,
            Kind::Miss => MISS_TTL,
            Kind::Error => ERROR_TTL,
        };
        if now.saturating_sub(e.at) >= ttl {
            return Lookup::Unknown;
        }
        match e.kind {
            Kind::Hit => Lookup::Hit(Titles::from_map(&e.titles)),
            Kind::Miss | Kind::Error => Lookup::Miss,
        }
    }

    pub fn put_hit(&mut self, key: &str, now: u64, titles: &Titles) {
        self.put(key, Entry { at: now, kind: Kind::Hit, titles: titles.to_map() });
    }

    pub fn put_miss(&mut self, key: &str, now: u64) {
        self.put(key, Entry { at: now, kind: Kind::Miss, titles: BTreeMap::new() });
    }

    pub fn put_error(&mut self, key: &str, now: u64) {
        self.put(key, Entry { at: now, kind: Kind::Error, titles: BTreeMap::new() });
    }

    fn put(&mut self, key: &str, e: Entry) {
        self.items.insert(key.to_string(), e);
        self.dirty = true;
        while self.items.len() > MAX_ENTRIES {
            let oldest = self.items.iter().min_by_key(|(_, i)| i.at).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.items.remove(&k),
                None => break,
            };
        }
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(msg) = &self.blocked {
            return Err(anyhow!("{msg}"));
        }
        let path = path_in(&self.dir);
        let json = serde_json::to_vec(&CacheFile { version: FORMAT_VERSION, items: self.items.clone() })
            .context("serializing episode titles")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    fn q(name: &str, year: Option<u32>) -> Query {
        Query { name: name.into(), year }
    }

    const JSON: &str = r#"{"name":"Fake Show","premiered":"2019-05-01","_embedded":{"episodes":[
        {"season":1,"number":2,"name":"The Visit"},{"season":1,"number":null,"name":"Special"}]}}"#;

    #[test]
    fn clean_query_strips_year_and_country() {
        assert_eq!(clean_query("Fake Show (2019)"), q("Fake Show", Some(2019)));
        assert_eq!(clean_query("Fake Show (US)"), q("Fake Show", None));
        assert_eq!(clean_query("Ófærð"), q("Ófærð", None));
        assert_eq!(clean_query("  Fake   Show "), q("Fake Show", None));
    }

    #[test]
    fn parse_matches_name_and_year() {
        let t = parse_response(JSON.as_bytes(), &q("fake show", None)).unwrap().unwrap();
        assert_eq!(t.get(1, 2), Some("The Visit"));
        assert_eq!(t.len(), 1);
        assert!(parse_response(JSON.as_bytes(), &q("Fake Show", Some(2019))).unwrap().is_some());
        assert!(parse_response(JSON.as_bytes(), &q("Fake Show", Some(2005))).unwrap().is_none());
        assert!(parse_response(JSON.as_bytes(), &q("Other Show", None)).unwrap().is_none());
        for (want, ok) in [(2017, false), (2018, true), (2020, true), (2021, false)] {
            let r = parse_response(JSON.replace("2019-05-01", &format!("{want}-05-01")).as_bytes(), &q("Fake Show", Some(2019)));
            assert_eq!(r.unwrap().is_some(), ok, "premiered {want}");
        }
        let np = JSON.replace(r#""2019-05-01""#, "null");
        assert!(parse_response(np.as_bytes(), &q("Fake Show", Some(2005))).unwrap().is_some());
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_response(b"<html>", &q("Fake Show", None)).is_err());
    }

    #[test]
    fn cache_round_trip_and_ttl() {
        let d = tempfile::tempdir().unwrap();
        let mut c = TitleCache::load(d.path());
        let t = parse_response(JSON.as_bytes(), &q("Fake Show", None)).unwrap().unwrap();
        c.put_hit("h", 0, &t);
        c.put_miss("m", 0);
        c.put_error("e", 0);
        assert_eq!(c.get("h", 29 * DAY), Lookup::Hit(t.clone()));
        assert_eq!(c.get("h", 31 * DAY), Lookup::Unknown);
        assert_eq!(c.get("m", 23 * 3600), Lookup::Miss);
        assert_eq!(c.get("m", 25 * 3600), Lookup::Unknown);
        assert_eq!(c.get("e", 59 * 60), Lookup::Miss);
        assert_eq!(c.get("e", 61 * 60), Lookup::Unknown);
        assert_eq!(c.get("absent", 0), Lookup::Unknown);
        c.save_if_dirty().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path_in(d.path())).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["shows"]["h"]["status"], "hit");
        assert_eq!(v["shows"]["h"]["titles"]["1:2"], "The Visit");
        assert_eq!(TitleCache::load(d.path()).get("h", DAY), Lookup::Hit(t));
    }

    #[test]
    fn cache_unreadable_file_is_moved_aside() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let c = TitleCache::load(d.path());
        assert_eq!(c.get("x", 0), Lookup::Unknown);
        assert!(!f.exists());
        assert!(d.path().join("episode-titles.json.bad").exists());
        std::fs::write(&f, br#"{"version":9,"shows":{}}"#).unwrap();
        let _ = TitleCache::load(d.path());
        assert!(d.path().join("episode-titles.json.bad.1").exists());
    }

    #[test]
    fn cache_caps_entries() {
        let d = tempfile::tempdir().unwrap();
        let mut c = TitleCache::load(d.path());
        for i in 0..=MAX_ENTRIES as u64 {
            c.put_miss(&format!("k{i}"), i);
        }
        assert_eq!(c.items.len(), 500);
        assert_eq!(c.get("k0", 1), Lookup::Unknown);
        assert_eq!(c.get("k500", 501), Lookup::Miss);
    }
}
