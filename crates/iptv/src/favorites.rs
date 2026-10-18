use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_LABEL_CHARS: usize = 200;
const FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteItem {
    pub key: String,
    pub label: String,
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u64,
    items: &'a [FavoriteItem],
}

/// Favorites store: keys + labels only, never stream URLs or credentials.
///
/// Each instance is an in-memory copy; `save` rewrites the whole file, so with several
/// instances on the same dir the last writer wins.
#[derive(Debug, Clone)]
pub struct Favorites {
    dir: PathBuf,
    items: Vec<FavoriteItem>,
    /// Set when an unreadable favorites.json (corrupt, unknown/future version, read error)
    /// could not be moved aside. `save` then refuses, so the original file is never overwritten.
    blocked: Option<String>,
}

fn path_in(dir: &Path) -> PathBuf {
    dir.join("favorites.json")
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

enum Parsed {
    Items(Vec<FavoriteItem>),
    Unreadable,
}

/// Accepts the legacy bare array and `{"version":1,"items":[...]}`. Anything else
/// (invalid JSON, version > 1, unknown shape) is unreadable.
fn parse(bytes: &[u8]) -> Parsed {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Parsed::Unreadable;
    };
    let arr = match v {
        serde_json::Value::Array(_) => v,
        serde_json::Value::Object(mut o) => {
            if o.get("version").and_then(|x| x.as_u64()) != Some(FORMAT_VERSION) {
                return Parsed::Unreadable;
            }
            match o.remove("items") {
                Some(a) => a,
                None => return Parsed::Unreadable,
            }
        }
        _ => return Parsed::Unreadable,
    };
    match serde_json::from_value::<Vec<FavoriteItem>>(arr) {
        Ok(items) => {
            let mut seen = std::collections::HashSet::new();
            Parsed::Items(items.into_iter().filter(|i| seen.insert(i.key.clone())).collect())
        }
        Err(_) => Parsed::Unreadable,
    }
}

impl Favorites {
    /// Missing -> empty. Unreadable (corrupt, newer version, unknown shape) -> empty, and the
    /// file is moved to the first unused `favorites.json.bad[.N]`. If that move fails, the
    /// instance refuses to `save` so the original is not overwritten.
    pub fn load(dir: &Path) -> Favorites {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> Favorites {
        let path = path_in(dir);
        let mut fav = Favorites { dir: dir.to_path_buf(), items: Vec::new(), blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return fav,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Parsed::Items(items) => {
                    fav.items = items;
                    false
                }
                Parsed::Unreadable => true,
            },
        };
        if unreadable {
            // First unused favorites.json.bad[.N] (never overwrites a backup).
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = mover(&path, &bad) {
                fav.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        fav
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

    /// Atomic write of `{"version":1,"items":[...]}`. Refuses if the original file was
    /// unreadable and could not be moved aside.
    pub fn save(&self) -> Result<()> {
        if let Some(msg) = &self.blocked {
            return Err(anyhow!("{msg}"));
        }
        let path = path_in(&self.dir);
        let json = serde_json::to_vec_pretty(&FileOut { version: FORMAT_VERSION, items: &self.items })
            .context("serializing favorites")?;
        crate::fsutil::atomic_write(&path, &json)
            .with_context(|| format!("writing {}", path.display()))
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
        // later corrupt files never overwrite earlier backups
        std::fs::write(d.path().join("favorites.json"), b"also bad").unwrap();
        let _ = Favorites::load(d.path());
        std::fs::write(d.path().join("favorites.json"), b"third").unwrap();
        let _ = Favorites::load(d.path());
        let rd = |n: &str| std::fs::read(d.path().join(n)).unwrap();
        assert_eq!(rd("favorites.json.bad"), b"{not json");
        assert_eq!(rd("favorites.json.bad.1"), b"also bad");
        assert_eq!(rd("favorites.json.bad.2"), b"third");
        // saving afterwards does not touch the backups
        let mut f = f;
        f.toggle("live:1", "x");
        f.save().unwrap();
        assert_eq!(rd("favorites.json.bad"), b"{not json");
    }

    fn failing_mover(_: &Path, _: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("favorites.json");
        std::fs::write(&p, b"{not json").unwrap();
        let mut f = Favorites::load_with(d.path(), failing_mover);
        f.toggle("live:1", "x");
        let err = f.save().unwrap_err().to_string();
        assert!(err.contains("favorites.json"), "{err}");
        assert_eq!(std::fs::read(&p).unwrap(), b"{not json");
    }

    #[test]
    fn legacy_array_loads_and_saves_as_v1() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("favorites.json");
        std::fs::write(&p, br#"[{"key":"live:1","label":"A"}]"#).unwrap();
        let f = Favorites::load(d.path());
        assert_eq!(f.list().len(), 1);
        f.save().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(Favorites::load(d.path()).list(), f.list());
    }

    #[test]
    fn v1_roundtrip_format() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Favorites::load(d.path());
        f.toggle("live:1", "A");
        f.save().unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(d.path().join("favorites.json")).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["items"][0]["key"], "live:1");
        assert_eq!(Favorites::load(d.path()).list(), f.list());
    }

    #[test]
    fn future_version_preserved_and_save_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("favorites.json");
        let future = br#"{"version":2,"items":[{"key":"live:1","label":"A"}],"extra":1}"#;
        // move succeeds: content preserved as a backup, not discarded
        std::fs::write(&p, future).unwrap();
        let f = Favorites::load(d.path());
        assert!(f.list().is_empty());
        assert_eq!(std::fs::read(d.path().join("favorites.json.bad")).unwrap(), future);
        // move fails: original stays in place and save refuses
        std::fs::write(&p, future).unwrap();
        let f = Favorites::load_with(d.path(), failing_mover);
        assert!(f.save().is_err());
        assert_eq!(std::fs::read(&p).unwrap(), future);
        // unknown shape is treated the same way
        std::fs::write(&p, br#"{"foo":1}"#).unwrap();
        assert!(Favorites::load_with(d.path(), failing_mover).save().is_err());
    }

    #[test]
    fn duplicate_keys_deduped_on_load_keeping_first() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("favorites.json"),
            br#"{"version":1,"items":[{"key":"a","label":"1"},{"key":"b","label":"2"},{"key":"a","label":"3"}]}"#,
        )
        .unwrap();
        let f = Favorites::load(d.path());
        assert_eq!(f.list().len(), 2);
        assert_eq!(f.list()[0].label, "1");
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
