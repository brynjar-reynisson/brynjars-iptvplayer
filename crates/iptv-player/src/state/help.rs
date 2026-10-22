//! Content of the help card (keyboard shortcuts and configuration options).
//! Plain data, so it is unit-tested and the egui layer only draws it.
//! Keep in step with the key handling in `ui/mod.rs` and with the README.

/// Tabs of the help card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTab {
    Keys,
    Settings,
}

impl HelpTab {
    pub const ALL: [HelpTab; 2] = [HelpTab::Keys, HelpTab::Settings];

    pub fn label(self) -> &'static str {
        match self {
            HelpTab::Keys => "Keys",
            HelpTab::Settings => "Settings",
        }
    }

    /// The tab after (`forward`) or before this one, wrapping (Tab / Shift+Tab).
    pub fn cycle(self, forward: bool) -> HelpTab {
        let n = Self::ALL.len();
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[if forward { (i + 1) % n } else { (i + n - 1) % n }]
    }

    pub fn sections(self) -> &'static [Section] {
        match self {
            HelpTab::Keys => KEYS,
            HelpTab::Settings => SETTINGS,
        }
    }
}

/// One line: a key (or option name) and what it does.
pub struct Row {
    pub name: &'static str,
    pub what: &'static str,
}

pub struct Section {
    pub title: &'static str,
    pub rows: &'static [Row],
}

const fn row(name: &'static str, what: &'static str) -> Row {
    Row { name, what }
}

pub const KEYS: &[Section] = &[
    Section {
        title: "Everywhere",
        rows: &[
            row("F1  ?", "Show / hide this help (? when not typing); Tab / Shift+Tab switch its tabs"),
            row("F5", "Refresh the playlist"),
            row("Ctrl+F  /", "Focus the channel filter (Movies/Series: the search box)"),
            row("F11  Alt+Enter", "Full screen on / off"),
            row("Esc", "Leave the filter box, else stop playback and go back to the list"),
            row("Tab", "Move keyboard focus to the tab buttons; Enter opens the tab"),
            row("F", "Add / remove favorite (playing channel, else the selected row)"),
            row("Shift+P", "Continue the last series you watched"),
        ],
    },
    Section {
        title: "Live",
        rows: &[
            row("Up / Down", "Move the selection; while playing, switch channel (after 300 ms)"),
            row("Enter  Right", "Play the selected channel; Enter on the playing one = full screen"),
            row("Left", "Go to the country list; Up/Down walk countries and their groups"),
            row("Right  Enter", "From the country list: back into the channels, plays the first"),
            row("Esc", "Leave the country list, or cancel a pending channel switch"),
        ],
    },
    Section {
        title: "Movies and Series",
        rows: &[
            row("Up / Down", "Move the cursor (never starts playing by itself)"),
            row("Enter", "Play; in Series open the show, then play an episode"),
            row("Backspace  Left  Esc", "Series: back from the episodes to the shows"),
        ],
    },
    Section {
        title: "While a movie or episode plays",
        rows: &[
            row("Space", "Pause / resume (also live)"),
            row("Left / Right", "Jump 10 seconds back / forward"),
            row("Shift+Right", "Next episode (the >> button)"),
            row("C", "Subtitle menu: Up/Down + Enter pick, Esc closes"),
            row(
                "Enter",
                "\"Skip intro\" button: skip it; \"Next episode in N\" offer: play it now (Esc keeps watching)",
            ),
        ],
    },
];

pub const SETTINGS: &[Section] = &[
    Section {
        title: "config.toml (beside the exe, or in the data directory)",
        rows: &[
            row("playlist_url", "Your M3U playlist address. Required (the app asks for it at startup while empty), keep it private"),
            row("user_agent", "Sent on the playlist download and every stream. Default VLC/3.0.20 LibVLC/3.0.20"),
            row("top_countries", "Live countries listed first, comma separated, e.g. \"Iceland, United Kingdom\" (after F5 or restart)"),
            row("theme", "\"black\" (default: black background, white text) or \"standard\" (egui dark look); after a restart"),
        ],
    },
    Section {
        title: "settings.json (edit while the app is closed)",
        rows: &[
            row("autoplay_next", "false turns off the \"Next episode\" offer and autoplay. Default true"),
            row("hidden_groups", "Set from the Groups menu on Movies/Series, not by hand"),
        ],
    },
    Section {
        title: "Where the data lives",
        rows: &[
            row("--data-dir <path>", "Folder for config, cache, favorites and resume points"),
            row("IPTV_DATA_DIR", "Same, as an environment variable (the flag wins). Default: the exe's folder"),
            row("--playlist-file <m3u>", "Read a local playlist file instead of downloading it"),
            row("--console", "Print status lines to the terminal the app was started from"),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_wraps_both_ways() {
        assert_eq!(HelpTab::Keys.cycle(true), HelpTab::Settings);
        assert_eq!(HelpTab::Settings.cycle(true), HelpTab::Keys);
        assert_eq!(HelpTab::Keys.cycle(false), HelpTab::Settings);
        assert_eq!(HelpTab::Settings.cycle(false), HelpTab::Keys);
    }

    #[test]
    fn every_section_has_rows_and_text() {
        for tab in HelpTab::ALL {
            assert!(!tab.sections().is_empty());
            for s in tab.sections() {
                assert!(!s.title.is_empty() && !s.rows.is_empty(), "{}", s.title);
                for r in s.rows {
                    assert!(!r.name.is_empty() && !r.what.is_empty(), "{}", r.name);
                }
            }
        }
    }

    #[test]
    fn no_key_is_listed_twice_in_a_section() {
        for tab in HelpTab::ALL {
            for s in tab.sections() {
                let mut names: Vec<_> = s.rows.iter().map(|r| r.name).collect();
                names.sort();
                let n = names.len();
                names.dedup();
                assert_eq!(n, names.len(), "duplicate in {}", s.title);
            }
        }
    }

    #[test]
    fn help_never_shows_credentials() {
        for tab in HelpTab::ALL {
            for s in tab.sections() {
                for r in s.rows {
                    assert!(!r.what.contains("://") && !r.name.contains("://"), "{}", r.name);
                }
            }
        }
    }
}
