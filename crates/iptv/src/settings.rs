//! User settings (`settings.json` beside `favorites.json`): the group titles
//! hidden from Movies/Series results, and the subtitle choice remembered per show. Same safety rules as favorites: atomic
//! writes, and an unreadable file (corrupt, unknown/future version) is moved
//! aside to `settings.json.bad[.N]`, never deleted or overwritten.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::subtitles::SubChoice;

const FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SettingsFile {
    version: u64,
    #[serde(default)]
    hidden_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    sub_prefs: BTreeMap<String, SubChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    autoplay_next: Option<bool>,
}

struct Parsed {
    hidden_groups: Vec<String>,
    sub_prefs: BTreeMap<String, SubChoice>,
    autoplay_next: Option<bool>,
}

/// Most shows with a remembered subtitle choice.
const MAX_SUB_PREFS: usize = 1000;

/// Settings store. Each instance is an in-memory copy; `save` rewrites the
/// whole file.
#[derive(Debug, Clone)]
pub struct Settings {
    dir: PathBuf,
    /// Group titles (e.g. `Movies: Arabic`) hidden from Movies/Series results,
    /// sorted, no duplicates.
    pub hidden_groups: Vec<String>,
    /// Subtitle choice per show (key: lowercased show name).
    sub_prefs: BTreeMap<String, SubChoice>,
    /// Hand-edited `autoplay_next`; `None` = not in the file (default true).
    autoplay_next: Option<bool>,
    /// Set when an unreadable settings.json could not be moved aside; `save`
    /// then refuses so the original is never overwritten.
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

fn parse(bytes: &[u8]) -> Option<Parsed> {
    let f: SettingsFile = serde_json::from_slice(bytes).ok()?;
    if f.version != FORMAT_VERSION {
        return None;
    }
    let mut g = f.hidden_groups;
    g.sort();
    g.dedup();
    Some(Parsed { hidden_groups: g, sub_prefs: f.sub_prefs, autoplay_next: f.autoplay_next })
}

impl Settings {
    /// Missing -> defaults. Unreadable -> defaults, and the file is moved to
    /// the first unused `settings.json.bad[.N]` (if that fails, `save` refuses).
    pub fn load(dir: &Path) -> Settings {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> Settings {
        let path = path_in(dir);
        let mut s = Settings { dir: dir.to_path_buf(), hidden_groups: Vec::new(), sub_prefs: BTreeMap::new(), autoplay_next: None, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return s,
            Err(_) => true,
            Ok(bytes) => match parse(&bytes) {
                Some(p) => {
                    s.hidden_groups = p.hidden_groups;
                    s.sub_prefs = p.sub_prefs;
                    s.autoplay_next = p.autoplay_next;
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

    pub fn is_hidden(&self, group: &str) -> bool {
        self.hidden_groups.binary_search_by(|g| g.as_str().cmp(group)).is_ok()
    }

    /// Hide or show `group` (in memory; call `save`).
    pub fn set_hidden(&mut self, group: &str, hidden: bool) {
        match (self.hidden_groups.binary_search_by(|g| g.as_str().cmp(group)), hidden) {
            (Err(i), true) => self.hidden_groups.insert(i, group.to_string()),
            (Ok(i), false) => {
                self.hidden_groups.remove(i);
            }
            _ => {}
        }
    }

    /// Play the next episode when one finishes (hand-edited `autoplay_next`
    /// in settings.json; default true).
    pub fn autoplay_next(&self) -> bool {
        self.autoplay_next.unwrap_or(true)
    }

    pub fn sub_pref(&self, show: &str) -> Option<&SubChoice> {
        self.sub_prefs.get(&show.to_lowercase())
    }

    /// Remember the subtitle choice for `show` (in memory; call `save`).
    pub fn set_sub_pref(&mut self, show: &str, choice: SubChoice) {
        let key = show.to_lowercase();
        if self.sub_prefs.len() >= MAX_SUB_PREFS && !self.sub_prefs.contains_key(&key) {
            self.sub_prefs.pop_first();
        }
        self.sub_prefs.insert(key, choice);
    }

    /// Atomic write of `{"version":1,"hidden_groups":[...],"sub_prefs":{...}}`.
    pub fn save(&self) -> Result<()> {
        if let Some(msg) = &self.blocked {
            return Err(anyhow!("{msg}"));
        }
        let path = path_in(&self.dir);
        let json = serde_json::to_vec_pretty(&SettingsFile {
            version: FORMAT_VERSION,
            hidden_groups: self.hidden_groups.clone(),
            sub_prefs: self.sub_prefs.clone(),
            autoplay_next: self.autoplay_next,
        })
        .context("serializing settings")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let d = tempfile::tempdir().unwrap();
        let s = Settings::load(d.path());
        assert!(s.hidden_groups.is_empty());
        assert!(!s.is_hidden("Movies: Testland"));
    }

    #[test]
    fn roundtrip_sorted_and_versioned() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Settings::load(d.path());
        s.set_hidden("Movies: Zed", true);
        s.set_hidden("Movies: Alpha", true);
        s.set_hidden("Movies: Alpha", true);
        s.set_hidden("Series: Mid", true);
        s.set_hidden("Series: Mid", false);
        s.save().unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(d.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        let g = Settings::load(d.path());
        assert_eq!(g.hidden_groups, vec!["Movies: Alpha".to_string(), "Movies: Zed".to_string()]);
        assert!(g.is_hidden("Movies: Zed") && !g.is_hidden("Series: Mid"));
        for e in std::fs::read_dir(d.path()).unwrap() {
            assert!(!e.unwrap().file_name().to_string_lossy().ends_with(".tmp"));
        }
    }

    #[test]
    fn sub_prefs_roundtrip_case_insensitive_and_capped() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Settings::load(d.path());
        let c = SubChoice::Track { lang: Some("en".into()), title: None, forced: false };
        s.set_sub_pref("The Show (2019)", c.clone());
        s.set_sub_pref("Other", SubChoice::Off);
        s.save().unwrap();
        let g = Settings::load(d.path());
        assert_eq!(g.sub_pref("the show (2019)"), Some(&c));
        assert_eq!(g.sub_pref("OTHER"), Some(&SubChoice::Off));
        assert_eq!(g.sub_pref("missing"), None);
        let mut s = g;
        for i in 0..MAX_SUB_PREFS + 5 {
            s.set_sub_pref(&format!("show {i:05}"), SubChoice::Off);
        }
        assert_eq!(s.sub_prefs.len(), MAX_SUB_PREFS);
    }

    #[test]
    fn corrupt_file_moved_aside_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, b"{oops").unwrap();
        let s = Settings::load(d.path());
        assert!(s.hidden_groups.is_empty());
        assert!(!p.exists());
        std::fs::write(&p, br#"{"version":7,"hidden_groups":["x"]}"#).unwrap();
        let _ = Settings::load(d.path());
        std::fs::write(&p, br#"["not","an","object"]"#).unwrap();
        let _ = Settings::load(d.path());
        let rd = |n: &str| std::fs::read(d.path().join(n)).unwrap();
        assert_eq!(rd("settings.json.bad"), b"{oops");
        assert_eq!(rd("settings.json.bad.1"), br#"{"version":7,"hidden_groups":["x"]}"#);
        assert_eq!(rd("settings.json.bad.2"), br#"["not","an","object"]"#);
        s.save().unwrap();
        assert_eq!(rd("settings.json.bad"), b"{oops", "backups untouched by save");
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, b"{oops").unwrap();
        let mut s = Settings::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        s.set_hidden("Movies: A", true);
        let err = s.save().unwrap_err().to_string();
        assert!(err.contains("settings.json"), "{err}");
        assert_eq!(std::fs::read(&p).unwrap(), b"{oops");
    }

    #[test]
    fn autoplay_next_defaults_to_true() {
        let d = tempfile::tempdir().unwrap();
        assert!(Settings::load(d.path()).autoplay_next());
    }

    #[test]
    fn autoplay_next_false_roundtrips_and_is_kept_when_saving_other_changes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, br#"{"version":1,"autoplay_next":false}"#).unwrap();
        let mut s = Settings::load(d.path());
        assert!(!s.autoplay_next());
        s.set_sub_pref("Show", SubChoice::Off);
        s.save().unwrap();
        assert!(!Settings::load(d.path()).autoplay_next());
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["autoplay_next"], false);

        std::fs::write(&p, br#"{"version":1}"#).unwrap();
        let mut s = Settings::load(d.path());
        s.set_sub_pref("Show", SubChoice::Off);
        s.save().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(v.get("autoplay_next").is_none());
        assert!(Settings::load(d.path()).autoplay_next());
    }

    #[test]
    fn unknown_fields_of_v1_are_tolerated() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("settings.json"), br#"{"version":1,"hidden_groups":["B","A","B"],"later":true}"#)
            .unwrap();
        assert_eq!(Settings::load(d.path()).hidden_groups, vec!["A".to_string(), "B".to_string()]);
        assert!(!d.path().join("settings.json.bad").exists());
    }
}
