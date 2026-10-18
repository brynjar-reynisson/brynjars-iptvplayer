//! Resume points for movies and episodes (`positions.json` beside
//! `favorites.json`): where playback stopped, by favorite key. Same safety
//! rules as settings: atomic writes, and an unreadable file is moved aside to
//! `positions.json.bad[.N]`, never deleted or overwritten.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const FORMAT_VERSION: u64 = 1;
/// Positions before this are not worth resuming (forgotten).
const MIN_RESUME_SECS: f64 = 15.0;
/// Within this many seconds of the end, or past [`DONE_FRACTION`], counts as watched.
const END_MARGIN_SECS: f64 = 60.0;
const DONE_FRACTION: f64 = 0.95;
/// Resume this much before the saved position, for context.
const REWIND_SECS: f64 = 3.0;
/// Most recent entries kept.
const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    /// Seconds into the movie/episode.
    pos: f64,
    /// Total length, if known.
    #[serde(default)]
    dur: Option<f64>,
    /// Unix seconds of the last update (oldest are dropped first).
    at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PositionsFile {
    version: u64,
    #[serde(default)]
    items: BTreeMap<String, Item>,
}

#[derive(Debug)]
pub struct Positions {
    dir: PathBuf,
    items: BTreeMap<String, Item>,
    dirty: bool,
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("positions.json")
}

fn parse(bytes: &[u8]) -> Option<BTreeMap<String, Item>> {
    let f: PositionsFile = serde_json::from_slice(bytes).ok()?;
    (f.version == FORMAT_VERSION).then_some(f.items)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Positions {
    pub fn load(dir: &Path) -> Positions {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> Positions {
        let path = path_in(dir);
        let mut p = Positions { dir: dir.to_path_buf(), items: BTreeMap::new(), dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return p,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Some(items) => {
                    p.items = items;
                    false
                }
                None => true,
            },
        };
        if unreadable {
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = mover(&path, &bad) {
                p.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        p
    }

    /// Note where `key` is now. Too early or (nearly) finished positions
    /// forget the entry instead. In memory; call [`save_if_dirty`](Self::save_if_dirty).
    pub fn record(&mut self, key: &str, pos: f64, dur: Option<f64>) {
        let dur = dur.filter(|d| d.is_finite() && *d > 0.0);
        let finished = dur.is_some_and(|d| pos >= d - END_MARGIN_SECS || pos >= d * DONE_FRACTION);
        if !pos.is_finite() || pos < MIN_RESUME_SECS || finished {
            self.forget(key);
            return;
        }
        if self.items.get(key).is_some_and(|i| (i.pos - pos).abs() < 1.0 && i.dur == dur) {
            return;
        }
        self.items.insert(key.to_string(), Item { pos, dur, at: now_secs() });
        self.dirty = true;
        if self.items.len() > MAX_ENTRIES {
            let oldest = self.items.iter().min_by_key(|(_, i)| i.at).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.items.remove(&k);
            }
        }
    }

    pub fn forget(&mut self, key: &str) {
        if self.items.remove(key).is_some() {
            self.dirty = true;
        }
    }

    /// Where to start `key`: the saved position, a little rewound.
    pub fn resume_for(&self, key: &str) -> Option<f64> {
        self.items.get(key).map(|i| (i.pos - REWIND_SECS).max(0.0))
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(msg) = &self.blocked {
            return Err(anyhow!("{msg}"));
        }
        let path = path_in(&self.dir);
        let json = serde_json::to_vec_pretty(&PositionsFile { version: FORMAT_VERSION, items: self.items.clone() })
            .context("serializing positions")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_resumes_and_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Positions::load(d.path());
        assert_eq!(p.resume_for("movie:1"), None);
        p.record("movie:1", 600.0, Some(5400.0));
        assert_eq!(p.resume_for("movie:1"), Some(597.0));
        p.save_if_dirty().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path_in(d.path())).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(Positions::load(d.path()).resume_for("movie:1"), Some(597.0));
    }

    #[test]
    fn early_and_finished_positions_are_forgotten() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Positions::load(d.path());
        p.record("a", 600.0, Some(5400.0));
        p.record("a", 5.0, Some(5400.0)); // restarted
        assert_eq!(p.resume_for("a"), None);
        p.record("b", 600.0, Some(5400.0));
        p.record("b", 5350.0, Some(5400.0)); // last minute
        assert_eq!(p.resume_for("b"), None);
        p.record("c", 570.0, Some(600.0)); // 95 %
        assert_eq!(p.resume_for("c"), None);
        p.record("d", 3000.0, None); // unknown length is kept
        assert_eq!(p.resume_for("d"), Some(2997.0));
        p.record("e", f64::NAN, None);
        assert_eq!(p.resume_for("e"), None);
    }

    #[test]
    fn unchanged_position_is_not_dirty_and_entries_are_capped() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Positions::load(d.path());
        p.record("a", 100.0, Some(900.0));
        p.save_if_dirty().unwrap();
        p.record("a", 100.4, Some(900.0));
        assert!(!p.dirty);
        for i in 0..(MAX_ENTRIES + 5) {
            p.record(&format!("k{i}"), 100.0, None);
        }
        assert_eq!(p.items.len(), MAX_ENTRIES);
    }

    #[test]
    fn corrupt_file_moved_aside_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut p = Positions::load(d.path());
        assert!(!f.exists());
        p.record("a", 100.0, None);
        p.save_if_dirty().unwrap();
        assert_eq!(std::fs::read(d.path().join("positions.json.bad")).unwrap(), b"{oops");
        std::fs::write(&f, br#"{"version":9,"items":{}}"#).unwrap();
        let _ = Positions::load(d.path());
        assert!(d.path().join("positions.json.bad.1").exists());
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut p = Positions::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        p.record("a", 100.0, None);
        assert!(p.save_if_dirty().unwrap_err().to_string().contains("positions.json"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{oops");
    }
}
