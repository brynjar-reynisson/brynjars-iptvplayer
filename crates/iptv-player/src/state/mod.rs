//! GUI-free application state (selection, navigation, filtering, favorites,
//! startup/refresh). The egui layer in `ui` stays a thin view over this.

pub mod autoplay;
pub mod credits;
pub mod cursor;
pub mod data;
pub mod favs;
pub mod help;
pub mod live;
pub mod nav;
pub mod next;
pub mod playback;
pub mod seek;
pub mod titles;
pub mod vod;
pub mod zap;

use std::time::Duration;

use iptv::supervisor::{RetryKind, Status};

/// Top-level tabs. Movies/Series are filled in by Task 12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Live,
    Favorites,
    Movies,
    Series,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Live, Tab::Favorites, Tab::Movies, Tab::Series];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Live => "Live",
            Tab::Favorites => "Favorites",
            Tab::Movies => "Movies",
            Tab::Series => "Series",
        }
    }
}

/// Enter on a keyboard-focused tab button: the tab to open and whether its
/// search box gets the focus (Movies/Series only). `None` = no tab has focus.
pub fn tab_enter(focused: Option<Tab>) -> Option<(Tab, bool)> {
    focused.map(|t| (t, matches!(t, Tab::Movies | Tab::Series)))
}

/// `tab_enter`, but only while the tab bar is drawn (Browse view, not
/// fullscreen): `focused_tab` is stale otherwise.
pub fn tab_enter_visible(focused: Option<Tab>, tab_bar_visible: bool) -> Option<(Tab, bool)> {
    if tab_bar_visible {
        tab_enter(focused)
    } else {
        None
    }
}

/// Player overlay text for a supervisor status ("" while playing normally).
pub fn status_text(s: Status, retry_in: Option<Duration>) -> String {
    let wait = retry_in.map(|d| format!(" (next try in {:.1} s)", d.as_secs_f32())).unwrap_or_default();
    match s {
        Status::Idle => String::new(),
        Status::Playing => String::new(),
        Status::Buffering => "Buffering…".into(),
        Status::Reconnecting { attempt, kind: RetryKind::Network } => format!("Reconnecting… (attempt {attempt}){wait}"),
        Status::Reconnecting { attempt, kind: RetryKind::Auth } => {
            format!("Account rejected or too many connections — retrying (attempt {attempt}){wait}")
        }
        Status::Reconnecting { attempt, kind: RetryKind::Unavailable } => {
            format!("Channel unavailable (off-air?) — retrying (attempt {attempt}){wait}")
        }
        Status::Finished => "Finished".into(),
    }
}

/// Human-readable byte count ("12.3 MB").
pub fn format_bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / MB)
    } else {
        format!("{} KB", n / 1024)
    }
}

/// Seconds as `hh:mm:ss` (negative/NaN clamp to 0).
pub fn format_hms(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 { secs as u64 } else { 0 };
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Last line of defence before any error/warning text reaches the screen:
/// redacts anything URL-shaped (messages from `fetch` are already scrubbed).
pub fn safe_text(msg: &str) -> String {
    crate::mpv_backend::scrub(msg, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_enter_opens_focused_tab() {
        assert_eq!(tab_enter(Some(Tab::Movies)), Some((Tab::Movies, true)));
        assert_eq!(tab_enter(Some(Tab::Series)), Some((Tab::Series, true)));
        assert_eq!(tab_enter(Some(Tab::Live)), Some((Tab::Live, false)));
        assert_eq!(tab_enter(Some(Tab::Favorites)), Some((Tab::Favorites, false)));
        assert_eq!(tab_enter(None), None);
    }

    #[test]
    fn tab_enter_ignored_when_tab_bar_hidden() {
        assert_eq!(tab_enter_visible(Some(Tab::Movies), false), None);
        assert_eq!(tab_enter_visible(Some(Tab::Movies), true), Some((Tab::Movies, true)));
    }

    #[test]
    fn hms() {
        assert_eq!(format_hms(0.0), "00:00:00");
        assert_eq!(format_hms(3725.9), "01:02:05");
        assert_eq!(format_hms(-4.0), "00:00:00");
        assert_eq!(format_hms(f64::NAN), "00:00:00");
    }

    #[test]
    fn status_texts() {
        assert_eq!(status_text(Status::Playing, None), "");
        assert_eq!(
            status_text(Status::Reconnecting { attempt: 3, kind: RetryKind::Network }, Some(Duration::from_millis(1500))),
            "Reconnecting… (attempt 3) (next try in 1.5 s)"
        );
        assert!(status_text(Status::Reconnecting { attempt: 1, kind: RetryKind::Auth }, None).starts_with("Account rejected"));
        assert_eq!(
            status_text(Status::Reconnecting { attempt: 2, kind: RetryKind::Unavailable }, Some(Duration::from_secs(6))),
            "Channel unavailable (off-air?) — retrying (attempt 2) (next try in 6.0 s)"
        );
    }

    #[test]
    fn safe_text_redacts_urls() {
        let s = safe_text("failed: http://h.invalid:8080/SECRETUSER/SECRETPASS/1.ts timed out");
        assert!(!s.contains("SECRETUSER") && !s.contains("SECRETPASS"), "{s}");
        assert!(s.contains("timed out"));
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512 * 1024), "512 KB");
        assert_eq!(format_bytes(80 * 1024 * 1024), "80.0 MB");
    }
}
