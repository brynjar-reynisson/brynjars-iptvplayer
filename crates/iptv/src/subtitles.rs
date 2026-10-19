//! Subtitle tracks and the remembered per-show choice (pure; the player reads
//! the track list from mpv and applies the result).

use serde::{Deserialize, Serialize};

/// One subtitle track of the playing file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubTrack {
    /// mpv track id (what `sid` takes).
    pub id: i64,
    pub lang: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
}

/// What the user picked. Stored per show, matched against later episodes'
/// tracks (ids differ between files, so language/title/forced identify it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubChoice {
    Off,
    Track { lang: Option<String>, title: Option<String>, forced: bool },
}

impl SubChoice {
    pub fn of(track: &SubTrack) -> SubChoice {
        SubChoice::Track { lang: track.lang.clone(), title: track.title.clone(), forced: track.forced }
    }
}

fn eq_opt(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
        (None, None) => true,
        _ => false,
    }
}

/// What to do for `want` given the tracks of a file: `Some(None)` = subtitles
/// off, `Some(Some(id))` = select that track, `None` = no match (leave
/// mpv's own default alone). The language must match; the title (+4) and the
/// forced flag (+2) only rank candidates; the first of equals wins.
pub fn pick(tracks: &[SubTrack], want: &SubChoice) -> Option<Option<i64>> {
    let SubChoice::Track { lang, title, forced } = want else { return Some(None) };
    let mut best: Option<(u32, i64)> = None;
    for t in tracks {
        let same_lang = match (&t.lang, lang) {
            (Some(_), Some(_)) => eq_opt(&t.lang, lang),
            // Neither names a language: only the title can identify it.
            (None, None) => title.is_some() && eq_opt(&t.title, title),
            _ => false,
        };
        if !same_lang {
            continue;
        }
        let score = 4 * u32::from(eq_opt(&t.title, title)) + 2 * u32::from(t.forced == *forced);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, t.id));
        }
    }
    best.map(|(_, id)| Some(id))
}

/// Menu text: language name (or code), then the track title if it adds
/// something, then "forced".
pub fn label(t: &SubTrack) -> String {
    let lang = t.lang.as_deref().map(|l| language_name(l).map_or_else(|| l.to_string(), str::to_string));
    let mut s = lang.clone().unwrap_or_else(|| format!("Track {}", t.id));
    if let Some(title) = t.title.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        if lang.as_deref().is_none_or(|l| !l.eq_ignore_ascii_case(title)) {
            s.push_str(" - ");
            s.push_str(title);
        }
    }
    if t.forced {
        s.push_str(" (forced)");
    }
    s
}

/// English name of an ISO 639 code (2 or 3 letters) for common languages.
pub fn language_name(code: &str) -> Option<&'static str> {
    Some(match code.trim().to_ascii_lowercase().as_str() {
        "en" | "eng" => "English",
        "is" | "isl" | "ice" => "Icelandic",
        "da" | "dan" => "Danish",
        "sv" | "swe" => "Swedish",
        "no" | "nor" | "nb" | "nob" => "Norwegian",
        "fi" | "fin" => "Finnish",
        "de" | "deu" | "ger" => "German",
        "fr" | "fra" | "fre" => "French",
        "es" | "spa" => "Spanish",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "nl" | "nld" | "dut" => "Dutch",
        "pl" | "pol" => "Polish",
        "ru" | "rus" => "Russian",
        "tr" | "tur" => "Turkish",
        "ar" | "ara" => "Arabic",
        "he" | "heb" => "Hebrew",
        "ja" | "jpn" => "Japanese",
        "ko" | "kor" => "Korean",
        "zh" | "zho" | "chi" => "Chinese",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: i64, lang: Option<&str>, title: Option<&str>, forced: bool) -> SubTrack {
        SubTrack { id, lang: lang.map(Into::into), title: title.map(Into::into), forced }
    }

    #[test]
    fn off_always_matches() {
        assert_eq!(pick(&[], &SubChoice::Off), Some(None));
    }

    #[test]
    fn matches_language_across_files_with_different_ids() {
        let want = SubChoice::of(&t(2, Some("eng"), Some("SDH"), false));
        let next = [t(1, Some("is"), None, false), t(3, Some("ENG"), Some("SDH"), false)];
        assert_eq!(pick(&next, &want), Some(Some(3)));
    }

    #[test]
    fn title_and_forced_rank_same_language_candidates() {
        let want = SubChoice::of(&t(1, Some("en"), Some("SDH"), false));
        let tracks = [t(1, Some("en"), None, true), t(2, Some("en"), None, false), t(3, Some("en"), Some("SDH"), false)];
        assert_eq!(pick(&tracks, &want), Some(Some(3)));
        let want = SubChoice::of(&t(1, Some("en"), None, false));
        assert_eq!(pick(&tracks, &want), Some(Some(2)));
        let only_forced = [t(1, Some("en"), None, true)];
        assert_eq!(pick(&only_forced, &want), Some(Some(1)));
    }

    #[test]
    fn no_language_match_leaves_default() {
        let want = SubChoice::of(&t(1, Some("is"), None, false));
        assert_eq!(pick(&[t(1, Some("en"), None, false)], &want), None);
        assert_eq!(pick(&[], &want), None);
    }

    #[test]
    fn tracks_without_language_match_by_title_only() {
        let want = SubChoice::of(&t(1, None, Some("Full"), false));
        assert_eq!(pick(&[t(4, None, Some("full"), false)], &want), Some(Some(4)));
        assert_eq!(pick(&[t(4, None, Some("Other"), false)], &want), None);
        let untitled = SubChoice::of(&t(1, None, None, false));
        assert_eq!(pick(&[t(4, None, None, false)], &untitled), None);
    }

    #[test]
    fn labels() {
        assert_eq!(label(&t(1, Some("eng"), None, false)), "English");
        assert_eq!(label(&t(1, Some("en"), Some("SDH"), false)), "English - SDH");
        assert_eq!(label(&t(1, Some("en"), Some("English"), true)), "English (forced)");
        assert_eq!(label(&t(1, Some("xx"), None, false)), "xx");
        assert_eq!(label(&t(7, None, None, false)), "Track 7");
    }

    #[test]
    fn choice_roundtrips_as_json() {
        for c in [SubChoice::Off, SubChoice::of(&t(1, Some("en"), None, true))] {
            let j = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<SubChoice>(&j).unwrap(), c);
        }
    }
}
