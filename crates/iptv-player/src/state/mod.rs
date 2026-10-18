//! GUI-free application state (selection, navigation, filtering, favorites,
//! startup/refresh). The egui layer in `ui` stays a thin view over this.

pub mod data;
pub mod favs;
pub mod live;
pub mod nav;
pub mod playback;
pub mod vod;
pub mod zap;

use std::time::Duration;

use iptv::supervisor::Status;

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

/// Player overlay text for a supervisor status ("" while playing normally).
pub fn status_text(s: Status, retry_in: Option<Duration>) -> String {
    let wait = retry_in.map(|d| format!(" (next try in {:.1} s)", d.as_secs_f32())).unwrap_or_default();
    match s {
        Status::Idle => String::new(),
        Status::Playing => String::new(),
        Status::Buffering => "Buffering…".into(),
        Status::Reconnecting { attempt, auth: false } => format!("Reconnecting… (attempt {attempt}){wait}"),
        Status::Reconnecting { attempt, auth: true } => {
            format!("Account rejected or too many connections — retrying (attempt {attempt}){wait}")
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

/// Last line of defence before any error/warning text reaches the screen:
/// redacts anything URL-shaped (messages from `fetch` are already scrubbed).
pub fn safe_text(msg: &str) -> String {
    crate::mpv_backend::scrub(msg, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_texts() {
        assert_eq!(status_text(Status::Playing, None), "");
        assert_eq!(
            status_text(Status::Reconnecting { attempt: 3, auth: false }, Some(Duration::from_millis(1500))),
            "Reconnecting… (attempt 3) (next try in 1.5 s)"
        );
        assert!(status_text(Status::Reconnecting { attempt: 1, auth: true }, None).starts_with("Account rejected"));
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
