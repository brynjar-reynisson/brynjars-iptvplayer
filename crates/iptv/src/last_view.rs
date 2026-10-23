//! Where the app was left (`last-view.json` beside `positions.json`): the live
//! channel that played, or the series episode that was open, by favorite key.
//! The next start goes back there. Same safety rules as settings: atomic
//! writes, and an unreadable file is moved aside to `last-view.json.bad[.N]`,
//! never deleted or overwritten.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LastKind {
    /// A live channel: starts playing again.
    Live,
    /// A series episode: selected in its show, not played.
    Episode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Last {
    pub kind: LastKind,
    /// Favorite key of the channel / episode.
    pub key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LastFile {
    version: u64,
    #[serde(default)]
    last: Option<Last>,
}

#[derive(Debug)]
pub struct LastView {
    dir: PathBuf,
    last: Option<Last>,
    dirty: bool,
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("last-view.json")
}

fn parse(bytes: &[u8]) -> Option<Option<Last>> {
    let f: LastFile = serde_json::from_slice(bytes).ok()?;
    (f.version == FORMAT_VERSION).then_some(f.last)
}

impl LastView {
    pub fn load(dir: &Path) -> LastView {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> LastView {
        let path = path_in(dir);
        let mut v = LastView { dir: dir.to_path_buf(), last: None, dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return v,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Some(last) => {
                    v.last = last;
                    false
                }
                None => true,
            },
        };
        if unreadable {
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = mover(&path, &bad) {
                v.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        v
    }

    pub fn get(&self) -> Option<&Last> {
        self.last.as_ref()
    }

    /// In memory; call [`save_if_dirty`](Self::save_if_dirty).
    pub fn set(&mut self, kind: LastKind, key: &str) {
        let new = Some(Last { kind, key: key.to_string() });
        if self.last != new {
            self.last = new;
            self.dirty = true;
        }
    }

    /// Forget a live channel (it was stopped on purpose); an episode stays.
    pub fn clear_live(&mut self) {
        if self.last.as_ref().is_some_and(|l| l.kind == LastKind::Live) {
            self.last = None;
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
        let json = serde_json::to_vec_pretty(&LastFile { version: FORMAT_VERSION, last: self.last.clone() })
            .context("serializing last view")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_unchanged_set_is_not_dirty() {
        let d = tempfile::tempdir().unwrap();
        let mut v = LastView::load(d.path());
        assert!(v.get().is_none());
        v.set(LastKind::Live, "live:1");
        v.save_if_dirty().unwrap();
        let mut v = LastView::load(d.path());
        assert_eq!(v.get(), Some(&Last { kind: LastKind::Live, key: "live:1".into() }));
        v.set(LastKind::Live, "live:1");
        assert!(!v.dirty);
        v.set(LastKind::Episode, "series:5");
        assert_eq!(v.get().unwrap().kind, LastKind::Episode);
    }

    #[test]
    fn stopping_live_clears_only_live() {
        let d = tempfile::tempdir().unwrap();
        let mut v = LastView::load(d.path());
        v.set(LastKind::Episode, "series:5");
        v.clear_live();
        assert!(v.get().is_some());
        v.set(LastKind::Live, "live:1");
        v.clear_live();
        assert!(v.get().is_none());
    }

    #[test]
    fn corrupt_file_moved_aside_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut v = LastView::load(d.path());
        assert!(!f.exists());
        v.set(LastKind::Live, "live:1");
        v.save_if_dirty().unwrap();
        assert_eq!(std::fs::read(d.path().join("last-view.json.bad")).unwrap(), b"{oops");
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut v = LastView::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        v.set(LastKind::Live, "live:1");
        assert!(v.save_if_dirty().unwrap_err().to_string().contains("last-view.json"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{oops");
    }
}
