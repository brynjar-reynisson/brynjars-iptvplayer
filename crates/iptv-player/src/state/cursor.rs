//! Mouse cursor auto-hide in fullscreen (pure rule; the UI sends the command).

use std::time::Duration;

/// Fullscreen without mouse movement for this long hides the cursor.
pub const HIDE_AFTER: Duration = Duration::from_secs(3);

/// The cursor is hidden only in fullscreen, once it has been still for
/// [`HIDE_AFTER`]. Any movement shows it again at once.
pub fn hidden(fullscreen: bool, still_for: Duration) -> bool {
    fullscreen && still_for >= HIDE_AFTER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_only_in_fullscreen_after_the_delay() {
        assert!(!hidden(true, Duration::ZERO));
        assert!(!hidden(true, HIDE_AFTER - Duration::from_millis(1)));
        assert!(hidden(true, HIDE_AFTER));
        assert!(hidden(true, Duration::from_secs(60)));
        assert!(!hidden(false, Duration::from_secs(60)));
    }
}
