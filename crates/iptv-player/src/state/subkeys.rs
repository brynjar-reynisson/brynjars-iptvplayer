//! Keys and notices for imported subtitles: Z / X nudge the timing.

/// How much one Z / X press moves the subtitles, in seconds.
pub const NUDGE_STEP: f64 = 0.1;

/// Shown when automatic matching could not decide.
pub const UNSURE_NOTICE: &str = "Could not match the subtitles automatically; adjust with Z / X";

/// Z = earlier (-0.1 s), X = later (+0.1 s). `None` = not ours: the key events
/// are left alone (typing, a tab button has focus, the subtitle menu or help is
/// open, or no movie/episode plays). `earlier` wins over `later` if both are set.
pub fn nudge(
    typing: bool,
    tab_focused: bool,
    sub_menu: bool,
    help_open: bool,
    vod_playing: bool,
    earlier: bool,
    later: bool,
) -> Option<f64> {
    if typing || tab_focused || sub_menu || help_open || !vod_playing {
        return None;
    }
    if earlier {
        Some(-NUDGE_STEP)
    } else if later {
        Some(NUDGE_STEP)
    } else {
        None
    }
}

/// `+0.5` / `-0.2` / `+0.0`: one decimal, explicit sign (no `-0.0`).
pub fn signed_secs(delay: f64) -> String {
    let t = (delay * 10.0).round() / 10.0;
    if t == 0.0 {
        "+0.0".to_string()
    } else {
        format!("{t:+.1}")
    }
}

/// Notice after a nudge; `delay` is the track's new total delay.
pub fn nudged_notice(delay: f64) -> String {
    format!("Subtitles {} s", signed_secs(delay))
}

/// Notice after automatic sync applied `delay`.
pub fn applied_notice(delay: f64) -> String {
    format!("Subtitles shifted {} s (automatic)", signed_secs(delay))
}

/// Notice after removing an imported track (`name` = language name or code).
pub fn removed_notice(name: &str) -> String {
    format!("Removed {name} subtitles")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z_and_x_only_while_a_video_plays() {
        assert_eq!(nudge(false, false, false, false, true, true, false), Some(-0.1));
        assert_eq!(nudge(false, false, false, false, true, false, true), Some(0.1));
        assert_eq!(nudge(false, false, false, false, true, false, false), None);
        assert_eq!(nudge(false, false, false, false, false, true, false), None);
        assert_eq!(nudge(false, false, false, false, false, false, true), None);
    }

    #[test]
    fn blocked_while_typing_menu_help_or_tab_focus() {
        assert_eq!(nudge(true, false, false, false, true, true, false), None);
        assert_eq!(nudge(false, true, false, false, true, true, false), None);
        assert_eq!(nudge(false, false, true, false, true, false, true), None);
        assert_eq!(nudge(false, false, false, true, true, false, true), None);
    }

    #[test]
    fn notices_have_one_decimal_and_a_sign() {
        assert_eq!(nudged_notice(0.5), "Subtitles +0.5 s");
        assert_eq!(nudged_notice(-0.2), "Subtitles -0.2 s");
        assert_eq!(nudged_notice(0.30000000000000004), "Subtitles +0.3 s");
        assert_eq!(nudged_notice(-0.01), "Subtitles +0.0 s");
        assert_eq!(applied_notice(0.4), "Subtitles shifted +0.4 s (automatic)");
        assert_eq!(removed_notice("English"), "Removed English subtitles");
    }
}
