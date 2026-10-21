//! The optional `theme` key of `config.toml`. Read on its own (not through
//! `config::load`) so it also works while `playlist_url` is still unset.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Theme {
    /// Black background, white text and outlined buttons (matches the video).
    #[default]
    Black,
    /// egui's standard dark look.
    Standard,
}

impl Theme {
    /// `"black"` / `"standard"` (case and spaces ignored); anything else is `None`.
    pub fn parse(s: &str) -> Option<Theme> {
        match s.trim().to_ascii_lowercase().as_str() {
            "black" => Some(Theme::Black),
            "standard" => Some(Theme::Standard),
            _ => None,
        }
    }
}

/// Theme from `config.toml` in `dir`; a missing file, key or unknown value
/// gives the default.
pub fn load(dir: &Path) -> Theme {
    std::fs::read_to_string(dir.join("config.toml"))
        .ok()
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
        .and_then(|v| v.get("theme").and_then(|t| t.as_str()).and_then(Theme::parse))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(text: &str) -> Theme {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), text).unwrap();
        load(d.path())
    }

    #[test]
    fn theme_key_is_optional_and_lenient() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(load(empty.path()), Theme::Black);
        assert_eq!(with(""), Theme::Black);
        assert_eq!(with("theme = \"standard\"\n"), Theme::Standard);
        assert_eq!(with("theme = \" Standard \"\n"), Theme::Standard);
        assert_eq!(with("theme = \"black\"\n"), Theme::Black);
        assert_eq!(with("theme = \"pink\"\n"), Theme::Black);
        assert_eq!(with("theme = 5\n"), Theme::Black);
        assert_eq!(with("not toml ["), Theme::Black);
    }
}
