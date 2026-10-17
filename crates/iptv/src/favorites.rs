use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// Duplicated minimally from cache.rs (unique tmp name per save) to avoid restructuring it.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_LABEL_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteItem {
    pub key: String,
    pub label: String,
}

/// Favorites store: keys + labels only, never stream URLs or credentials.
#[derive(Debug, Clone)]
pub struct Favorites {
    dir: PathBuf,
    items: Vec<FavoriteItem>,
}

fn path_in(dir: &Path) -> PathBuf {
    dir.join("favorites.json")
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl Favorites {
    /// Missing -> empty. Corrupt -> empty, and the bad file is moved to `favorites.json.bad`.
    pub fn load(dir: &Path) -> Favorites {
        let path = path_in(dir);
        let items = match std::fs::read(&path) {
            Err(_) => Vec::new(),
            Ok(bytes) => match serde_json::from_slice::<Vec<FavoriteItem>>(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    let bad = dir.join("favorites.json.bad");
                    let _ = std::fs::remove_file(&bad);
                    let _ = std::fs::rename(&path, &bad);
                    Vec::new()
                }
            },
        };
        Favorites { dir: dir.to_path_buf(), items }
    }

    /// Adds (at the end) or removes. Empty/blank keys are ignored.
    pub fn toggle(&mut self, key: &str, label: &str) {
        if key.trim().is_empty() {
            return;
        }
        if let Some(i) = self.items.iter().position(|f| f.key == key) {
            self.items.remove(i);
        } else {
            self.items.push(FavoriteItem {
                key: key.to_string(),
                label: truncate_chars(label, MAX_LABEL_CHARS),
            });
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.items.iter().any(|f| f.key == key)
    }

    pub fn list(&self) -> &[FavoriteItem] {
        &self.items
    }

    /// Atomic write: unique tmp file, fsync, rename (retried on transient Windows failures).
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating dir {}", self.dir.display()))?;
        let path = path_in(&self.dir);
        let tmp = self.dir.join(format!(
            "favorites.json.{}.{}.tmp",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let json = serde_json::to_vec_pretty(&self.items).context("serializing favorites")?;
        let result = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.flush()?;
            f.sync_all()?;
            drop(f);
            let mut last = None;
            for attempt in 0..3 {
                match std::fs::rename(&tmp, &path) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        last = Some(e);
                        if attempt < 2 {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                    }
                }
            }
            Err(last.unwrap())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("writing {}", path.display()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::{favorite_key, parse};

    #[test]
    fn toggle_adds_then_removes() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("live:1", "One");
        f.toggle("live:2", "Two");
        assert!(f.contains("live:1"));
        assert_eq!(f.list().iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), ["live:1", "live:2"]);
        f.toggle("live:1", "One");
        assert!(!f.contains("live:1"));
        assert_eq!(f.list().len(), 1);
    }

    #[test]
    fn roundtrip_through_disk() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("live:1", "Канал Ünï 日本語 📺");
        f.toggle("movie:2", "Two");
        f.save().unwrap();
        let g = Favorites::load(d.path());
        assert_eq!(g.list(), f.list());
        assert_eq!(g.list()[0].label, "Канал Ünï 日本語 📺");
        for e in std::fs::read_dir(d.path()).unwrap() {
            assert!(!e.unwrap().file_name().to_string_lossy().ends_with(".tmp"));
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        assert!(Favorites::load(d.path()).list().is_empty());
    }

    #[test]
    fn corrupt_file_yields_empty_and_keeps_backup() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("favorites.json"), b"{not json").unwrap();
        let f = Favorites::load(d.path());
        assert!(f.list().is_empty());
        assert!(!d.path().join("favorites.json").exists());
        assert_eq!(std::fs::read(d.path().join("favorites.json.bad")).unwrap(), b"{not json");
        // a second corrupt file overwrites the old .bad
        std::fs::write(d.path().join("favorites.json"), b"also bad").unwrap();
        let _ = Favorites::load(d.path());
        assert_eq!(std::fs::read(d.path().join("favorites.json.bad")).unwrap(), b"also bad");
        // saving afterwards does not touch the backup
        let mut f = f;
        f.toggle("live:1", "x");
        f.save().unwrap();
        assert_eq!(std::fs::read(d.path().join("favorites.json.bad")).unwrap(), b"also bad");
    }

    #[test]
    fn empty_key_ignored() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("", "x");
        f.toggle("   ", "x");
        assert!(f.list().is_empty());
        f.toggle("live:1", "x");
        assert_eq!(f.list().len(), 1);
    }

    #[test]
    fn long_label_truncated_on_char_boundary() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("live:1", &"é日".repeat(300));
        assert_eq!(f.list()[0].label.chars().count(), 200);
        f.save().unwrap();
        assert_eq!(Favorites::load(d.path()).list()[0].label.chars().count(), 200);
    }

    #[test]
    fn rapid_double_toggle_returns_to_original() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("live:1", "a");
        f.toggle("live:1", "a");
        assert!(f.list().is_empty());
        f.toggle("live:1", "a");
        f.toggle("live:1", "a");
        f.toggle("live:1", "a");
        assert_eq!(f.list().len(), 1);
    }

    #[test]
    fn save_twice_replaces() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("live:1", "a");
        f.save().unwrap();
        f.toggle("live:1", "a");
        f.save().unwrap();
        assert!(Favorites::load(d.path()).list().is_empty());
    }

    #[test]
    fn key_and_file_contain_no_credentials() {
        let m3u = "#EXTM3U\n#EXTINF:-1 group-title=\"G\",Fake\nhttp://host.invalid:8080/USER/PASS/123.ts\n#EXTINF:-1 group-title=\"G\",NoId\nhttp://host.invalid:8080\n";
        let entries = parse(std::io::Cursor::new(m3u));
        assert_eq!(entries.len(), 2);
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        for e in &entries {
            let k = favorite_key(e);
            assert!(!k.contains("USER") && !k.contains("PASS"), "{k}");
            f.toggle(&k, &e.name);
        }
        assert_eq!(f.list().len(), 2);
        f.save().unwrap();
        let raw = std::fs::read_to_string(d.path().join("favorites.json")).unwrap();
        assert!(!raw.contains("USER") && !raw.contains("PASS") && !raw.contains("host.invalid"));
    }
}
