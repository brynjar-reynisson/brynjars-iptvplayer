//! The series watched most recently (`recent-series.json` beside
//! `positions.json`), newest first, one entry per show: which episode (by
//! favorite key) and whether it reached its credits. Same safety rules as
//! settings: atomic writes, and an unreadable file is moved aside to
//! `recent-series.json.bad[.N]`, never deleted or overwritten.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u64 = 1;
/// Shows kept.
pub const MAX_RECENT: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// Show name (`iptv::series::Show::name`).
    pub show: String,
    /// Favorite key of the last episode started.
    pub key: String,
    /// That episode reached its credits (or finished).
    #[serde(default)]
    pub credits: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecentFile {
    version: u64,
    #[serde(default)]
    items: Vec<Recent>,
}

#[derive(Debug)]
pub struct RecentSeries {
    dir: PathBuf,
    items: Vec<Recent>,
    dirty: bool,
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("recent-series.json")
}

fn parse(bytes: &[u8]) -> Option<Vec<Recent>> {
    let f: RecentFile = serde_json::from_slice(bytes).ok()?;
    (f.version == FORMAT_VERSION).then_some(f.items)
}

impl RecentSeries {
    pub fn load(dir: &Path) -> RecentSeries {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> RecentSeries {
        let path = path_in(dir);
        let mut r = RecentSeries { dir: dir.to_path_buf(), items: Vec::new(), dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return r,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Some(mut items) => {
                    items.truncate(MAX_RECENT);
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

    /// Newest first.
    pub fn items(&self) -> &[Recent] {
        &self.items
    }

    pub fn latest(&self) -> Option<&Recent> {
        self.items.first()
    }

    /// An episode of `show` started: the show moves to the top (credits not
    /// reached yet). In memory; call [`save_if_dirty`](Self::save_if_dirty).
    pub fn touch(&mut self, show: &str, key: &str) {
        let fresh = Recent { show: show.to_string(), key: key.to_string(), credits: false };
        if self.items.first() == Some(&fresh) {
            return;
        }
        self.items.retain(|r| r.show != show);
        self.items.insert(0, fresh);
        self.items.truncate(MAX_RECENT);
        self.dirty = true;
    }

    /// The episode `key` of `show` reached its credits (only if it is still
    /// the one recorded for the show).
    pub fn mark_credits(&mut self, show: &str, key: &str) {
        if let Some(r) = self.items.iter_mut().find(|r| r.show == show && r.key == key && !r.credits) {
            r.credits = true;
            self.dirty = true;
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
        let json = serde_json::to_vec_pretty(&RecentFile { version: FORMAT_VERSION, items: self.items.clone() })
            .context("serializing recent series")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_one_entry_per_show_capped() {
        let d = tempfile::tempdir().unwrap();
        let mut r = RecentSeries::load(d.path());
        assert!(r.latest().is_none());
        r.touch("A", "series:1");
        r.touch("B", "series:2");
        r.touch("A", "series:3"); // A again, another episode
        let shows: Vec<&str> = r.items().iter().map(|i| i.show.as_str()).collect();
        assert_eq!(shows, ["A", "B"]);
        assert_eq!(r.latest().unwrap().key, "series:3");
        for i in 0..(MAX_RECENT + 3) {
            r.touch(&format!("S{i}"), "series:9");
        }
        assert_eq!(r.items().len(), MAX_RECENT);
        assert_eq!(r.latest().unwrap().show, format!("S{}", MAX_RECENT + 2));
    }

    #[test]
    fn credits_flag_belongs_to_the_recorded_episode() {
        let d = tempfile::tempdir().unwrap();
        let mut r = RecentSeries::load(d.path());
        r.touch("A", "series:1");
        r.mark_credits("A", "series:2"); // not the recorded episode
        assert!(!r.latest().unwrap().credits);
        r.mark_credits("A", "series:1");
        assert!(r.latest().unwrap().credits);
        r.touch("A", "series:2"); // next episode: flag reset
        assert!(!r.latest().unwrap().credits);
    }

    #[test]
    fn roundtrips_and_unchanged_touch_is_not_dirty() {
        let d = tempfile::tempdir().unwrap();
        let mut r = RecentSeries::load(d.path());
        r.touch("A", "series:1");
        r.mark_credits("A", "series:1");
        r.save_if_dirty().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path_in(d.path())).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        let mut r = RecentSeries::load(d.path());
        assert_eq!(r.items(), [Recent { show: "A".into(), key: "series:1".into(), credits: true }]);
        r.touch("B", "series:2");
        r.save_if_dirty().unwrap();
        r.touch("B", "series:2");
        assert!(!r.dirty);
    }

    #[test]
    fn corrupt_file_moved_aside_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut r = RecentSeries::load(d.path());
        assert!(!f.exists());
        r.touch("A", "series:1");
        r.save_if_dirty().unwrap();
        assert_eq!(std::fs::read(d.path().join("recent-series.json.bad")).unwrap(), b"{oops");
        std::fs::write(&f, br#"{"version":9,"items":[]}"#).unwrap();
        let _ = RecentSeries::load(d.path());
        assert!(d.path().join("recent-series.json.bad.1").exists());
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut r = RecentSeries::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        r.touch("A", "series:1");
        assert!(r.save_if_dirty().unwrap_err().to_string().contains("recent-series.json"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{oops");
    }
}
