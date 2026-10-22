//! Per-show intro references (`intros.json` beside `recent-series.json`):
//! the fingerprints of a show's first-seen intro region, keyed by the
//! lowercased show name. Same safety rules as settings: atomic writes, and an
//! unreadable file is moved aside to `intros.json.bad[.N]`, never deleted or
//! overwritten.

use crate::intro::{Fingerprint, Prints};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u64 = 1;
/// Shows kept (oldest insertion dropped first).
pub const MAX_SHOWS: usize = 200;

#[derive(Debug, Serialize, Deserialize)]
struct Item {
    show: String,
    prints: Vec<(f64, Option<String>)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IntrosFile {
    version: u64,
    #[serde(default)]
    items: Vec<Item>,
}

pub struct Intros {
    dir: PathBuf,
    /// Oldest insertion first.
    items: Vec<(String, Prints)>,
    dirty: bool,
    blocked: Option<String>,
}

impl std::fmt::Debug for Intros {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Intros({} shows)", self.items.len())
    }
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("intros.json")
}

fn key(show: &str) -> String {
    show.to_lowercase()
}

fn parse(bytes: &[u8]) -> Option<Vec<(String, Prints)>> {
    let f: IntrosFile = serde_json::from_slice(bytes).ok()?;
    if f.version != FORMAT_VERSION {
        return None;
    }
    let mut out = Vec::new();
    for it in f.items {
        let mut prints: Prints = Vec::new();
        for (t, hex) in it.prints {
            let fp = match hex {
                None => None,
                Some(h) => Some(Fingerprint::from_hex(&h)?),
            };
            prints.push((t, fp));
        }
        out.push((it.show, prints));
    }
    Some(out)
}

impl Intros {
    pub fn load(dir: &Path) -> Intros {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> Intros {
        let path = path_in(dir);
        let mut r = Intros { dir: dir.to_path_buf(), items: Vec::new(), dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return r,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Some(mut items) => {
                    if items.len() > MAX_SHOWS {
                        items.drain(..items.len() - MAX_SHOWS);
                    }
                    r.items = items;
                    false
                }
                None => true,
            },
        };
        if unreadable {
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = mover(&path, &bad) {
                r.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        r
    }

    pub fn get(&self, show: &str) -> Option<&Prints> {
        let k = key(show);
        self.items.iter().find(|(s, _)| *s == k).map(|(_, p)| p)
    }

    /// Replaces the show's reference (it becomes the newest). In memory; call
    /// [`save_if_dirty`](Self::save_if_dirty).
    pub fn put(&mut self, show: &str, prints: Prints) {
        let k = key(show);
        self.items.retain(|(s, _)| *s != k);
        self.items.push((k, prints));
        if self.items.len() > MAX_SHOWS {
            self.items.remove(0);
        }
        self.dirty = true;
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(msg) = &self.blocked {
            return Err(anyhow!("{msg}"));
        }
        let path = path_in(&self.dir);
        let items: Vec<Item> = self
            .items
            .iter()
            .map(|(s, p)| Item {
                show: s.clone(),
                prints: p.iter().map(|(t, fp)| (*t, fp.as_ref().map(|f| f.to_hex()))).collect(),
            })
            .collect();
        let json = serde_json::to_vec_pretty(&IntrosFile { version: FORMAT_VERSION, items })
            .context("serializing intros")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(b: u8) -> Fingerprint {
        Fingerprint([b; 144])
    }

    #[test]
    fn round_trips_prints_including_none() {
        let d = tempfile::tempdir().unwrap();
        let mut r = Intros::load(d.path());
        let prints: Prints = vec![(0.0, Some(fp(7))), (4.0, None), (8.5, Some(fp(200)))];
        r.put("The Show", prints.clone());
        r.save_if_dirty().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path_in(d.path())).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["items"][0]["show"], "the show");
        let r = Intros::load(d.path());
        assert_eq!(r.get("The Show"), Some(&prints));
    }

    #[test]
    fn show_key_is_case_insensitive() {
        let d = tempfile::tempdir().unwrap();
        let mut r = Intros::load(d.path());
        r.put("MiXed Case", vec![(0.0, None)]);
        assert!(r.get("mixed case").is_some());
        assert!(r.get("MIXED CASE").is_some());
        assert!(r.get("other").is_none());
    }

    #[test]
    fn put_replaces_and_keeps_at_most_200() {
        let d = tempfile::tempdir().unwrap();
        let mut r = Intros::load(d.path());
        r.put("a", vec![(0.0, None)]);
        r.put("A", vec![(1.0, None)]);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.get("a"), Some(&vec![(1.0, None)]));
        for i in 0..(MAX_SHOWS + 5) {
            r.put(&format!("s{i}"), vec![]);
        }
        assert_eq!(r.items.len(), MAX_SHOWS);
        assert!(r.get("a").is_none());
        assert!(r.get("s0").is_none());
        assert!(r.get(&format!("s{}", MAX_SHOWS + 4)).is_some());
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_load_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut r = Intros::load(d.path());
        assert!(!f.exists());
        assert!(r.items.is_empty());
        r.put("a", vec![]);
        r.save_if_dirty().unwrap();
        assert_eq!(std::fs::read(d.path().join("intros.json.bad")).unwrap(), b"{oops");
    }

    #[test]
    fn unknown_version_is_moved_aside() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, br#"{"version":9,"items":[]}"#).unwrap();
        let r = Intros::load(d.path());
        assert!(!f.exists());
        assert!(r.items.is_empty());
        assert!(d.path().join("intros.json.bad").exists());
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut r = Intros::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        r.put("a", vec![]);
        assert!(r.save_if_dirty().unwrap_err().to_string().contains("intros.json"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{oops");
    }

    #[test]
    fn missing_file_loads_empty_and_save_if_dirty_writes_nothing_when_clean() {
        let d = tempfile::tempdir().unwrap();
        let mut r = Intros::load(d.path());
        assert!(r.items.is_empty());
        r.save_if_dirty().unwrap();
        assert!(!path_in(d.path()).exists());
        assert_eq!(format!("{r:?}"), "Intros(0 shows)");
    }
}
