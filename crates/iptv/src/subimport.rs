//! Planning an import of .srt files: reading language and episode from file
//! names, matching files to a show's episodes and storing them in
//! [`ExtSubs`](crate::extsubs::ExtSubs).

use crate::extsubs::ExtSubs;
use crate::subtitles::normalize_lang;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What a file name says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameInfo {
    /// Canonical 3-letter language code.
    pub lang: Option<&'static str>,
    pub season_episode: Option<(u32, u32)>,
}

/// An episode that can receive a subtitle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub key: String,
    pub season: u32,
    pub episode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// (file, target key)
    pub matched: Vec<(PathBuf, String)>,
    pub unmatched: Vec<PathBuf>,
    pub duplicates: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub imported: usize,
    pub message: String,
    /// Files whose name has no language and no default was given.
    pub need_language: Vec<PathBuf>,
}

fn num(s: &str, max: usize) -> Option<u32> {
    if s.is_empty() || s.len() > max || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `S02E06` or `2x06` (case-insensitive) -> (season, episode).
fn marker(token: &str) -> Option<(u32, u32)> {
    let t = token.to_ascii_lowercase();
    if let Some(rest) = t.strip_prefix('s') {
        let (s, e) = rest.split_once('e')?;
        // S02E06E07: the first episode counts.
        let e = e.split('e').next().unwrap_or(e);
        return Some((num(s, 3)?, num(e, 4)?));
    }
    let (s, e) = t.split_once('x')?;
    Some((num(s, 2)?, num(e, 3)?))
}

const CODES: [&str; 20] = [
    "eng", "isl", "dan", "swe", "nor", "fin", "deu", "fra", "spa", "ita", "por", "nld", "pol", "rus", "tur", "ara",
    "heb", "jpn", "kor", "zho",
];

/// "Icelandic" -> `isl` (English language names, case-insensitive).
fn lang_by_name(token: &str) -> Option<&'static str> {
    CODES.into_iter().find(|c| crate::subtitles::language_name(c).is_some_and(|n| n.eq_ignore_ascii_case(token)))
}

/// Language (code or English name; see [`normalize_lang`]) and `S02E06` / `2x06` from a file name.
/// A language token counts only as the last token before the extension or,
/// when an episode marker is present, after the marker (so words of the show
/// name like "is" or "it" are never read as languages).
pub fn parse_name(file_name: &str) -> NameInfo {
    let stem = match file_name.rsplit_once('.') {
        Some((s, ext)) if ext.eq_ignore_ascii_case("srt") => s,
        _ => file_name,
    };
    let tokens: Vec<&str> = stem.split(['.', ' ', '-', '_']).filter(|t| !t.is_empty()).collect();
    let found = tokens.iter().enumerate().find_map(|(i, t)| marker(t).map(|m| (i, m)));
    let candidates: &[&str] = match found {
        Some((i, _)) => &tokens[i + 1..],
        None => tokens.last().map_or(&[], std::slice::from_ref),
    };
    let lang = candidates.iter().rev().find_map(|t| normalize_lang(t).or_else(|| lang_by_name(t)));
    NameInfo { lang, season_episode: found.map(|(_, m)| m) }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Match files to episodes by season/episode. Files are taken in name order;
/// for one episode the first file per language wins, later ones are duplicates.
pub fn plan_for_show(files: &[PathBuf], targets: &[Target]) -> Plan {
    let mut sorted: Vec<&PathBuf> = files.iter().collect();
    sorted.sort_by_key(|p| name_of(p));
    let mut plan = Plan::default();
    let mut seen: HashSet<(String, Option<&'static str>)> = HashSet::new();
    for f in sorted {
        let info = parse_name(&name_of(f));
        let target = info
            .season_episode
            .and_then(|(s, e)| targets.iter().find(|t| t.season == s && t.episode == e));
        match target {
            None => plan.unmatched.push(f.clone()),
            Some(t) => {
                if seen.insert((t.key.clone(), info.lang)) {
                    plan.matched.push((f.clone(), t.key.clone()));
                } else {
                    plan.duplicates.push(f.clone());
                }
            }
        }
    }
    plan
}

/// `.srt` files directly in `dir` (case-insensitive extension), sorted.
pub fn srt_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("srt")))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|p| name_of(p));
    v
}

fn list(label: &str, files: &[PathBuf]) -> String {
    const MAX_NAMES: usize = 5;
    let mut names: Vec<String> = files.iter().take(MAX_NAMES).map(|p| name_of(p)).collect();
    if files.len() > MAX_NAMES {
        names.push("...".into());
    }
    format!(" {label} ({}): {}.", files.len(), names.join(", "))
}

fn build_message(imported: usize, total: usize, parts: &[(&str, &[PathBuf])]) -> String {
    let mut m = format!("Imported {imported} of {total}.");
    for (label, files) in parts {
        if !files.is_empty() {
            m.push_str(&list(label, files));
        }
    }
    m
}

fn store_one(store: &mut ExtSubs, key: &str, lang: &str, file: &Path) -> Result<(), String> {
    let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
    store.add(key, lang, &bytes).map_err(|e| e.to_string())
}

fn resolve_default(default_lang: Option<&str>) -> Option<&'static str> {
    default_lang.and_then(normalize_lang)
}

/// Import the files that match an episode of `targets`. A file with no
/// language in its name (and no valid `default_lang`) is not imported but
/// returned in `need_language`; the caller asks once and calls again.
pub fn import_show(
    store: &mut ExtSubs,
    files: &[PathBuf],
    targets: &[Target],
    default_lang: Option<&str>,
) -> Report {
    let plan = plan_for_show(files, targets);
    let default = resolve_default(default_lang);
    let mut report = Report::default();
    let mut failed = Vec::new();
    let mut duplicates = plan.duplicates.clone();
    let mut used: HashSet<(&str, &str)> = HashSet::new();
    for (file, key) in &plan.matched {
        let Some(lang) = parse_name(&name_of(file)).lang.or(default) else {
            report.need_language.push(file.clone());
            continue;
        };
        if !used.insert((key.as_str(), lang)) {
            duplicates.push(file.clone());
            continue;
        }
        match store_one(store, key, lang, file) {
            Ok(()) => report.imported += 1,
            Err(_) => failed.push(file.clone()),
        }
    }
    report.message = build_message(
        report.imported,
        files.len(),
        &[
            ("Not matched", &plan.unmatched),
            ("Duplicates", &duplicates),
            ("Need a language", &report.need_language),
            ("Failed", &failed),
        ],
    );
    report
}

/// Attach one file (any name) to a movie.
pub fn import_movie(store: &mut ExtSubs, key: &str, file: &Path, default_lang: Option<&str>) -> Report {
    let mut report = Report::default();
    let mut failed = Vec::new();
    match parse_name(&name_of(file)).lang.or(resolve_default(default_lang)) {
        None => report.need_language.push(file.to_path_buf()),
        Some(lang) => match store_one(store, key, lang, file) {
            Ok(()) => report.imported = 1,
            Err(_) => failed.push(file.to_path_buf()),
        },
    }
    report.message =
        build_message(report.imported, 1, &[("Need a language", &report.need_language), ("Failed", &failed)]);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srt() -> Vec<u8> {
        b"1\n00:00:01,000 --> 00:00:02,000\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nWorld\n\n".to_vec()
    }

    fn write(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, srt()).unwrap();
        p
    }

    fn tgt(key: &str, s: u32, e: u32) -> Target {
        Target { key: key.into(), season: s, episode: e }
    }

    #[test]
    fn parse_name_reads_episode_and_language() {
        let i = parse_name("Example.Show.S02E06.en.srt");
        assert_eq!((i.season_episode, i.lang), (Some((2, 6)), Some("eng")));
        let i = parse_name("example show 2x06 Icelandic.srt");
        assert_eq!((i.season_episode, i.lang), (Some((2, 6)), Some("isl")));
        let i = parse_name("example show 2x06 ice.srt");
        assert_eq!((i.season_episode, i.lang), (Some((2, 6)), Some("isl")));
        let i = parse_name("movie.srt");
        assert_eq!((i.season_episode, i.lang), (None, None));
        assert_eq!(parse_name("show.s02e06.srt").season_episode, Some((2, 6)));
        // words of the show name are not languages
        let i = parse_name("What.Is.It.S01E02.srt");
        assert_eq!((i.season_episode, i.lang), (Some((1, 2)), None));
        assert_eq!(parse_name("Is.srt").lang, Some("isl"));
        assert_eq!(parse_name("Is.It.Good.srt").lang, None);
    }

    #[test]
    fn plan_matches_by_episode_and_reports_the_rest() {
        let targets = [tgt("a", 2, 1), tgt("b", 2, 2), tgt("c", 2, 3)];
        let files: Vec<PathBuf> = ["Show.S02E02.srt", "Show.S02E01.srt", "Other.srt", "Show.S02E01.copy.srt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let p = plan_for_show(&files, &targets);
        assert_eq!(p.matched.len(), 2);
        assert_eq!(p.unmatched, vec![PathBuf::from("Other.srt")]);
        assert_eq!(p.duplicates, vec![PathBuf::from("Show.S02E01.srt")]);
        assert!(p.matched.contains(&(PathBuf::from("Show.S02E01.copy.srt"), "a".into())));
    }

    #[test]
    fn import_show_needs_a_language_when_the_name_has_none() {
        let d = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut store = ExtSubs::load(data.path());
        let f1 = write(d.path(), "Show.S01E01.srt");
        let f2 = write(d.path(), "Show.S01E02.is.srt");
        let targets = [tgt("a", 1, 1), tgt("b", 1, 2)];
        let r = import_show(&mut store, &[f1.clone(), f2], &targets, None);
        assert_eq!(r.imported, 1);
        assert_eq!(r.need_language, vec![f1.clone()]);
        assert!(store.for_key("a").is_empty());
        assert_eq!(store.for_key("b")[0].lang, "isl");
        let r = import_show(&mut store, &[f1], &targets, Some("en"));
        assert_eq!(r.imported, 1);
        assert!(r.need_language.is_empty());
        assert_eq!(store.for_key("a")[0].lang, "eng");
        let bad = import_show(&mut store, &[write(d.path(), "Show.S01E01.x.srt")], &targets, Some("klingon"));
        assert_eq!(bad.imported, 0);
        assert_eq!(bad.need_language.len(), 1);
    }

    #[test]
    fn named_and_unnamed_language_files_resolving_to_one_language_are_duplicates() {
        let d = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut store = ExtSubs::load(data.path());
        let a = write(d.path(), "Show.S02E06.en.srt");
        let b = write(d.path(), "Show.S02E06.srt");
        let r = import_show(&mut store, &[a, b], &[tgt("k", 2, 6)], Some("eng"));
        assert_eq!(r.imported, 1);
        assert_eq!(r.message, "Imported 1 of 2. Duplicates (1): Show.S02E06.srt.");
        assert_eq!(store.for_key("k").len(), 1);
    }

    #[test]
    fn import_movie_attaches_any_name() {
        let d = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut store = ExtSubs::load(data.path());
        let f = write(d.path(), "whatever name.srt");
        let r = import_movie(&mut store, "movie:1", &f, None);
        assert_eq!((r.imported, r.need_language.len()), (0, 1));
        let r = import_movie(&mut store, "movie:1", &f, Some("isl"));
        assert_eq!(r.imported, 1);
        assert_eq!(r.message, "Imported 1 of 1.");
        assert_eq!(store.for_key("movie:1")[0].lang, "isl");
    }

    #[test]
    fn message_lists_at_most_five_names() {
        let d = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut store = ExtSubs::load(data.path());
        let mut files = vec![write(d.path(), "Show.S01E01.en.srt")];
        for i in 1..=7 {
            files.push(write(d.path(), &format!("x{i}.srt")));
        }
        let r = import_show(&mut store, &files, &[tgt("a", 1, 1)], None);
        assert_eq!(r.message, "Imported 1 of 8. Not matched (7): x1.srt, x2.srt, x3.srt, x4.srt, x5.srt, ....");
        let one = import_show(&mut store, &files[..1], &[tgt("a", 1, 1)], None);
        assert_eq!(one.message, "Imported 1 of 1.");
    }

    #[test]
    fn srt_files_in_is_flat_sorted_and_case_insensitive() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "b.SRT");
        write(d.path(), "a.srt");
        std::fs::write(d.path().join("c.txt"), b"x").unwrap();
        std::fs::create_dir(d.path().join("sub.srt")).unwrap();
        let names: Vec<String> = srt_files_in(d.path()).iter().map(|p| name_of(p)).collect();
        assert_eq!(names, vec!["a.srt", "b.SRT"]);
    }
}
