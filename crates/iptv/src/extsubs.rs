//! Imported (external) subtitle files: copies kept in `<data dir>/subtitles/`
//! and an index `subtitles.json` (`{"version":1,"next_id":N,"items":{key:[..]}}`)
//! beside `favorites.json`. Same safety rules as settings: atomic writes, and
//! an unreadable index is moved aside to `subtitles.json.bad[.N]`, never
//! deleted or overwritten.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u64 = 1;
/// Largest .srt accepted (bytes).
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Most imported files kept in total.
pub const MAX_FILES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtSub {
    /// Canonical 3-letter language code.
    pub lang: String,
    /// File name of the copy inside the `subtitles` folder.
    pub file: String,
    /// Seconds added to every cue (negative = earlier).
    #[serde(default)]
    pub delay: f64,
    /// The delay was set by hand (not measured).
    #[serde(default)]
    pub manual: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u64,
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    items: BTreeMap<String, Vec<ExtSub>>,
}

#[derive(Debug)]
pub struct ExtSubs {
    dir: PathBuf,
    next_id: u64,
    items: BTreeMap<String, Vec<ExtSub>>,
    dirty: bool,
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("subtitles.json")
}

fn parse(bytes: &[u8]) -> Option<IndexFile> {
    let f: IndexFile = serde_json::from_slice(bytes).ok()?;
    (f.version == FORMAT_VERSION).then_some(f)
}

impl ExtSubs {
    pub fn load(dir: &Path) -> ExtSubs {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> ExtSubs {
        let path = path_in(dir);
        let mut s = ExtSubs { dir: dir.to_path_buf(), next_id: 1, items: BTreeMap::new(), dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return s,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Some(f) => {
                    s.items = f.items;
                    s.next_id = f.next_id.max(1);
                    false
                }
                None => true,
            },
        };
        if unreadable {
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = mover(&path, &bad) {
                s.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        s
    }

    fn folder(&self) -> PathBuf {
        self.dir.join("subtitles")
    }

    pub fn for_key(&self, key: &str) -> &[ExtSub] {
        self.items.get(key).map_or(&[], Vec::as_slice)
    }

    fn total(&self) -> usize {
        self.items.values().map(Vec::len).sum()
    }

    /// Store `bytes` (an .srt in any common encoding) as the `lang` subtitle
    /// of `key`, replacing the same key+language. In memory plus the copy on
    /// disk; call [`save_if_dirty`](Self::save_if_dirty) for the index.
    pub fn add(&mut self, key: &str, lang: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_FILE_BYTES {
            bail!("subtitle file is too large ({} bytes, max {MAX_FILE_BYTES})", bytes.len());
        }
        let text = crate::subsync::decode_srt(bytes);
        if crate::subsync::cue_starts(&text).is_empty() {
            bail!("no subtitle cues found in the file");
        }
        let replaces = self.for_key(key).iter().any(|s| s.lang == lang);
        if !replaces && self.total() >= MAX_FILES {
            bail!("too many imported subtitles (max {MAX_FILES})");
        }
        let folder = self.folder();
        std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
        let file = format!("{:08x}.srt", self.next_id);
        let path = folder.join(&file);
        crate::fsutil::atomic_write(&path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
        self.next_id += 1;
        let entry = ExtSub { lang: lang.to_string(), file, delay: 0.0, manual: false };
        let list = self.items.entry(key.to_string()).or_default();
        let old = match list.iter().position(|s| s.lang == lang) {
            Some(i) => Some(std::mem::replace(&mut list[i], entry)),
            None => {
                list.push(entry);
                None
            }
        };
        if let Some(old) = old {
            let _ = std::fs::remove_file(folder.join(old.file));
        }
        self.dirty = true;
        Ok(())
    }

    pub fn remove(&mut self, key: &str, lang: &str) {
        let Some(list) = self.items.get_mut(key) else { return };
        let Some(i) = list.iter().position(|s| s.lang == lang) else { return };
        let old = list.remove(i);
        if list.is_empty() {
            self.items.remove(key);
        }
        let _ = std::fs::remove_file(self.folder().join(old.file));
        self.dirty = true;
    }

    pub fn set_delay(&mut self, key: &str, lang: &str, delay: f64, manual: bool) {
        if !delay.is_finite() {
            return;
        }
        if let Some(s) = self.items.get_mut(key).and_then(|l| l.iter_mut().find(|s| s.lang == lang)) {
            if s.delay != delay || s.manual != manual {
                s.delay = delay;
                s.manual = manual;
                self.dirty = true;
            }
        }
    }

    pub fn path_of(&self, s: &ExtSub) -> PathBuf {
        self.folder().join(&s.file)
    }

    /// Cue start times of the stored copy (empty if it is missing/unreadable).
    pub fn cue_starts_of(&self, s: &ExtSub) -> Vec<f64> {
        match std::fs::read(self.path_of(s)) {
            Ok(b) => crate::subsync::cue_starts(&crate::subsync::decode_srt(&b)),
            Err(_) => Vec::new(),
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
        let json = serde_json::to_vec_pretty(&IndexFile {
            version: FORMAT_VERSION,
            next_id: self.next_id,
            items: self.items.clone(),
        })
        .context("serializing subtitles index")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srt(n: usize) -> Vec<u8> {
        let mut s = String::new();
        for i in 0..n {
            s.push_str(&format!("{}\n00:00:{:02},000 --> 00:00:{:02},500\nLine {i}\n\n", i + 1, i + 1, i + 1));
        }
        s.into_bytes()
    }

    #[test]
    fn add_roundtrips_and_survives_reload() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ExtSubs::load(d.path());
        assert!(s.for_key("series:1").is_empty());
        s.add("series:1", "eng", &srt(3)).unwrap();
        s.save_if_dirty().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path_in(d.path())).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["next_id"], 2);
        let r = ExtSubs::load(d.path());
        let list = r.for_key("series:1");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].lang, "eng");
        assert_eq!(list[0].file, "00000001.srt");
        assert_eq!(r.cue_starts_of(&list[0]), vec![1.0, 2.0, 3.0]);
        assert!(r.path_of(&list[0]).exists());
    }

    #[test]
    fn same_key_and_language_replaces_and_deletes_the_old_copy() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ExtSubs::load(d.path());
        s.add("k", "eng", &srt(2)).unwrap();
        s.set_delay("k", "eng", 1.5, true);
        let old = s.path_of(&s.for_key("k")[0]);
        s.add("k", "eng", &srt(4)).unwrap();
        s.add("k", "isl", &srt(1)).unwrap();
        assert!(!old.exists());
        assert_eq!(s.for_key("k").len(), 2);
        let e = s.for_key("k").iter().find(|x| x.lang == "eng").unwrap().clone();
        assert_eq!((e.delay, e.manual), (0.0, false));
        assert_eq!(s.cue_starts_of(&e).len(), 4);
        s.remove("k", "eng");
        assert_eq!(s.for_key("k").len(), 1);
        assert!(!s.path_of(&e).exists());
    }

    #[test]
    fn refuses_empty_no_cue_and_oversized_files() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ExtSubs::load(d.path());
        assert!(s.add("k", "eng", b"").is_err());
        assert!(s.add("k", "eng", b"just some text\nno cues").is_err());
        let mut big = srt(2);
        big.resize(MAX_FILE_BYTES + 1, b' ');
        assert!(s.add("k", "eng", &big).is_err());
        assert!(s.for_key("k").is_empty());
        assert!(!d.path().join("subtitles").exists());
        s.save_if_dirty().unwrap();
        assert!(!path_in(d.path()).exists());
    }

    #[test]
    fn refuses_more_than_max_files() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ExtSubs::load(d.path());
        s.items.insert(
            "full".into(),
            (0..MAX_FILES)
                .map(|i| ExtSub { lang: format!("l{i}"), file: String::new(), delay: 0.0, manual: false })
                .collect(),
        );
        assert!(s.add("other", "eng", &srt(1)).is_err());
        assert!(s.add("full", "l0", &srt(1)).is_ok());
    }

    #[test]
    fn unreadable_index_is_moved_aside_and_save_refuses() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut s = ExtSubs::load(d.path());
        assert!(!f.exists());
        s.add("k", "eng", &srt(1)).unwrap();
        s.save_if_dirty().unwrap();
        assert_eq!(std::fs::read(d.path().join("subtitles.json.bad")).unwrap(), b"{oops");

        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut s = ExtSubs::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        s.add("k", "eng", &srt(1)).unwrap();
        assert!(s.save_if_dirty().unwrap_err().to_string().contains("subtitles.json"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{oops");
    }

    #[test]
    fn missing_copy_gives_no_cues_and_no_panic() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ExtSubs::load(d.path());
        s.add("k", "eng", &srt(2)).unwrap();
        let e = s.for_key("k")[0].clone();
        std::fs::remove_file(s.path_of(&e)).unwrap();
        assert!(s.cue_starts_of(&e).is_empty());
    }

    #[test]
    fn set_delay_and_manual_flag_persist() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ExtSubs::load(d.path());
        s.add("k", "eng", &srt(2)).unwrap();
        s.set_delay("k", "eng", -2.5, true);
        s.set_delay("k", "nope", 9.0, true);
        s.save_if_dirty().unwrap();
        let r = ExtSubs::load(d.path());
        assert_eq!((r.for_key("k")[0].delay, r.for_key("k")[0].manual), (-2.5, true));
    }
}
