//! Mouse cursor auto-hide in fullscreen (pure rule; the UI sends the command).

use std::time::Duration;

/// Fullscreen without mouse movement for this long hides the cursor.
pub const HIDE_AFTER: Duration = Duration::from_secs(3);

/// The cursor is hidden only in fullscreen, once it has been still for
/// [`HIDE_AFTER`]. Any movement shows it again at once.
pub fn hidden(fullscreen: bool, still_for: Duration) -> bool {
    fullscreen && still_for >= HIDE_AFTER
}

/// The title overlay (top left) stays this long in fullscreen after the last
/// mouse movement or title change, a bit longer than the cursor.
pub const TITLE_SHOW: Duration = Duration::from_secs(5);

/// The title is always shown outside fullscreen; in fullscreen only until
/// [`TITLE_SHOW`] has passed since the last movement / title change.
pub fn title_visible(fullscreen: bool, since_activity: Duration) -> bool {
    !fullscreen || since_activity < TITLE_SHOW
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_hides_in_fullscreen_only_and_outlasts_the_cursor() {
        assert!(title_visible(false, Duration::from_secs(600)));
        assert!(title_visible(true, Duration::ZERO));
        assert!(title_visible(true, TITLE_SHOW - Duration::from_millis(1)));
        assert!(!title_visible(true, TITLE_SHOW));
        assert!(TITLE_SHOW > HIDE_AFTER);
    }

    #[test]
    fn hides_only_in_fullscreen_after_the_delay() {
        assert!(!hidden(true, Duration::ZERO));
        assert!(!hidden(true, HIDE_AFTER - Duration::from_millis(1)));
        assert!(hidden(true, HIDE_AFTER));
        assert!(hidden(true, Duration::from_secs(60)));
        assert!(!hidden(false, Duration::from_secs(60)));
    }
}
