//! Favorites tab state and the favorite toggle (toggle + persist).

use iptv::favorites::Favorites;
use iptv::library::Library;
use iptv::playlist::favorite_key;

use super::Tab;

/// One row of the Favorites tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FavRow {
    pub key: String,
    pub label: String,
    /// Entry in the current library; `None` = unavailable (greyed, not playable).
    pub entry: Option<usize>,
}

impl FavRow {
    pub fn playable(&self) -> bool {
        self.entry.is_some()
    }
}

/// Rows for `favs` resolved against `lib` (all unavailable without a library).
pub fn favorite_rows(lib: Option<&Library>, favs: &Favorites) -> Vec<FavRow> {
    favs.list()
        .iter()
        .map(|f| FavRow {
            key: f.key.clone(),
            label: f.label.clone(),
            entry: lib.and_then(|l| l.find_by_key(&f.key)),
        })
        .collect()
}

/// Toggle the favorite for library entry `entry` and save. Returns whether it
/// is now a favorite. On a save error the toggle is undone (memory matches
/// disk) and the error text is returned.
pub fn toggle_favorite(favs: &mut Favorites, lib: &Library, entry: usize) -> Result<bool, String> {
    let e = lib.entry(entry);
    toggle_key(favs, &favorite_key(e), &e.name)
}

/// [`toggle_favorite`] by key (e.g. removing an unavailable favorite).
pub fn toggle_key(favs: &mut Favorites, key: &str, label: &str) -> Result<bool, String> {
    favs.toggle(key, label);
    match favs.save() {
        Ok(()) => Ok(favs.contains(key)),
        Err(err) => {
            favs.toggle(key, label);
            Err(format!("Could not save favorites: {err:#}"))
        }
    }
}

/// A favorite toggle target: (favorite key, label).
pub type FavTarget = (String, String);

/// What `F` toggles. In the Favorites tab the row under the cursor wins (so
/// an unavailable favorite can be removed while something plays); in other
/// tabs the playing channel, else the selected one.
pub fn toggle_target(
    tab: Tab,
    fav_row: Option<&FavRow>,
    playing: Option<FavTarget>,
    selected: Option<FavTarget>,
) -> Option<FavTarget> {
    match (tab, fav_row) {
        (Tab::Favorites, Some(r)) => Some((r.key.clone(), r.label.clone())),
        _ => playing.or(selected),
    }
}

/// Favorites tab: rows plus a selected row.
#[derive(Debug, Default)]
pub struct FavBrowser {
    rows: Vec<FavRow>,
    cursor: Option<usize>,
}

impl FavBrowser {
    pub fn rows(&self) -> &[FavRow] {
        &self.rows
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Rebuild the rows (after a toggle or a library swap), keeping the
    /// selection on the same key when possible.
    pub fn reload(&mut self, lib: Option<&Library>, favs: &Favorites) {
        let key = self.cursor.and_then(|c| self.rows.get(c)).map(|r| r.key.clone());
        self.rows = favorite_rows(lib, favs);
        self.cursor = key.and_then(|k| self.rows.iter().position(|r| r.key == k));
    }

    /// Select row `pos` (any row, playable or not).
    pub fn set_cursor(&mut self, pos: usize) {
        self.cursor = if pos < self.rows.len() { Some(pos) } else { None };
    }

    /// Entry to play for the selected row; `None` for an unavailable favorite.
    pub fn play_target(&self) -> Option<usize> {
        self.cursor.and_then(|c| self.rows.get(c)).and_then(|r| r.entry)
    }

    /// Entries of the playable rows, in order (the list Up/Down walks while
    /// playing a favorite).
    pub fn playable_entries(&self) -> Vec<usize> {
        self.rows.iter().filter_map(|r| r.entry).collect()
    }

    /// Move the cursor by `delta` (wrapping), skipping unavailable rows;
    /// returns the selected playable entry (`None` if nothing is playable).
    pub fn step(&mut self, delta: i32) -> Option<usize> {
        let n = self.rows.len() as i64;
        if n == 0 || !self.rows.iter().any(FavRow::playable) {
            return None;
        }
        let dir = if delta >= 0 { 1 } else { -1 };
        let mut pos = match self.cursor {
            Some(c) => c as i64,
            None if dir > 0 => -1,
            None => n,
        };
        let mut moved = 0;
        let want = delta.unsigned_abs().max(1);
        while moved < want {
            pos = (pos + dir).rem_euclid(n);
            if self.rows[pos as usize].playable() {
                moved += 1;
            }
        }
        self.cursor = Some(pos as usize);
        self.play_target()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::live::tests::{live, sample_lib};

    #[test]
    fn toggling_favorite_persists() {
        let d = tempfile::tempdir().unwrap();
        let lib = sample_lib();
        let mut favs = Favorites::load(d.path());
        assert_eq!(toggle_favorite(&mut favs, &lib, 2), Ok(true));
        let reloaded = Favorites::load(d.path());
        assert!(reloaded.contains(&favorite_key(lib.entry(2))));
        assert_eq!(reloaded.list()[0].label, "NRK1");
        assert_eq!(toggle_favorite(&mut favs, &lib, 2), Ok(false));
        assert!(Favorites::load(d.path()).list().is_empty());
    }

    #[test]
    fn save_error_reverts_toggle_and_reports() {
        let d = tempfile::tempdir().unwrap();
        // "Data dir" that is actually a file: every save fails.
        let file = d.path().join("not_a_dir");
        std::fs::write(&file, b"x").unwrap();
        let lib = sample_lib();
        let mut favs = Favorites::load(&file);
        let err = toggle_favorite(&mut favs, &lib, 0).unwrap_err();
        assert!(err.starts_with("Could not save favorites"), "{err}");
        assert!(favs.list().is_empty(), "toggle undone after failed save");
    }

    #[test]
    fn unavailable_favorite_can_be_removed_by_key() {
        let d = tempfile::tempdir().unwrap();
        let mut favs = Favorites::load(d.path());
        favs.toggle("live:999", "Gone Channel");
        assert_eq!(toggle_key(&mut favs, "live:999", "Gone Channel"), Ok(false));
        assert!(Favorites::load(d.path()).list().is_empty());
    }

    #[test]
    fn unavailable_favorite_is_not_playable() {
        let d = tempfile::tempdir().unwrap();
        let lib = sample_lib();
        let mut favs = Favorites::load(d.path());
        favs.toggle("live:999", "Gone Channel");
        favs.toggle(&favorite_key(lib.entry(0)), "SVT1 HD");
        let mut b = FavBrowser::default();
        b.reload(Some(&lib), &favs);
        assert_eq!(b.rows().len(), 2);
        assert!(!b.rows()[0].playable());
        b.set_cursor(0);
        assert_eq!(b.play_target(), None);
        b.set_cursor(1);
        assert_eq!(b.play_target(), Some(0));
        assert_eq!(b.playable_entries(), vec![0]);
        // Without a library nothing is playable.
        b.reload(None, &favs);
        assert!(b.rows().iter().all(|r| !r.playable()));
    }

    #[test]
    fn f_prefers_favorites_row_then_playing_then_selected() {
        let t = |k: &str| Some((k.to_string(), k.to_uppercase()));
        let gone = FavRow { key: "live:999".into(), label: "Gone".into(), entry: None };
        // Favorites tab: the row under the cursor, even while something plays.
        assert_eq!(
            toggle_target(Tab::Favorites, Some(&gone), t("live:1"), t("live:2")),
            Some(("live:999".into(), "Gone".into()))
        );
        // Favorites tab without a row selected: falls back to the playing channel.
        assert_eq!(toggle_target(Tab::Favorites, None, t("live:1"), None), t("live:1"));
        // Other tabs: playing beats selected; selected when nothing plays.
        assert_eq!(toggle_target(Tab::Live, Some(&gone), t("live:1"), t("live:2")), t("live:1"));
        assert_eq!(toggle_target(Tab::Live, None, None, t("live:2")), t("live:2"));
        assert_eq!(toggle_target(Tab::Live, None, None, None), None);
    }

    #[test]
    fn step_skips_unavailable_rows() {
        let d = tempfile::tempdir().unwrap();
        let lib = Library::build(vec![live("1", "A", "X - Y"), live("2", "B", "X - Y")]);
        let mut favs = Favorites::load(d.path());
        favs.toggle("live:1", "A");
        favs.toggle("live:404", "Gone");
        favs.toggle("live:2", "B");
        let mut b = FavBrowser::default();
        b.reload(Some(&lib), &favs);
        assert_eq!(b.step(1), Some(0));
        assert_eq!(b.step(1), Some(1), "skips the unavailable row");
        assert_eq!(b.cursor(), Some(2));
        assert_eq!(b.step(1), Some(0), "wraps");
        assert_eq!(b.step(-1), Some(1));
    }
}
