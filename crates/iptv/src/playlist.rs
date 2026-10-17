use serde::{Deserialize, Serialize};
use std::io::BufRead;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Live,
    Movie,
    Series,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Live => "live",
            Kind::Movie => "movie",
            Kind::Series => "series",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: Kind,
    pub id: String,
    pub name: String,
    pub group: String,
    pub logo: String,
    pub url: String,
}

/// Last URL path segment without extension (query/fragment ignored).
pub fn stream_id(url: &str) -> Option<String> {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let mut path = &url[..end];
    if let Some(p) = path.find("://") {
        let rest = &path[p + 3..];
        path = match rest.find('/') {
            Some(i) => &rest[i..],
            None => return None,
        };
    }
    let seg = path.rsplit('/').next()?;
    let seg = match seg.rfind('.') {
        Some(i) if i > 0 => &seg[..i],
        _ => seg,
    };
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub fn favorite_key(e: &Entry) -> String {
    if !e.id.is_empty() {
        format!("{}:{}", e.kind.as_str(), e.id)
    } else {
        format!("{}:name:{}|{}", e.kind.as_str(), normalize(&e.name), e.group)
    }
}

fn kind_of(url: &str) -> Kind {
    if url.contains("/movie/") {
        Kind::Movie
    } else if url.contains("/series/") {
        Kind::Series
    } else {
        Kind::Live
    }
}

struct Pending {
    name: String,
    group: String,
    logo: String,
}

/// Parse the part of an `#EXTINF:` line after the prefix. None for separators.
fn parse_extinf(s: &str) -> Option<Pending> {
    let b = s.as_bytes();
    let mut in_quote = false;
    let mut quote_start = 0;
    let mut first_comma: Option<usize> = None;
    let mut tvg_name = "";
    let mut group = "";
    let mut logo = "";
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'"' => {
                if in_quote {
                    in_quote = false;
                    if quote_start > 0 && b[quote_start - 1] == b'=' {
                        let kend = quote_start - 1;
                        let kstart = b[..kend]
                            .iter()
                            .rposition(|x| x.is_ascii_whitespace())
                            .map_or(0, |p| p + 1);
                        let key = &s[kstart..kend];
                        let val = &s[quote_start + 1..i];
                        if key.eq_ignore_ascii_case("tvg-name") {
                            tvg_name = val;
                        } else if key.eq_ignore_ascii_case("group-title") {
                            group = val;
                        } else if key.eq_ignore_ascii_case("tvg-logo") {
                            logo = val;
                        }
                    }
                } else {
                    in_quote = true;
                    quote_start = i;
                }
            }
            b',' if !in_quote => {
                first_comma = Some(i);
                break;
            }
            _ => {}
        }
    }
    let disp = first_comma.map_or("", |i| s[i + 1..].trim());
    let name = if disp.is_empty() { tvg_name.trim() } else { disp };
    if name.starts_with("=====") || tvg_name.trim().starts_with("=====") {
        return None;
    }
    Some(Pending {
        name: name.to_string(),
        group: group.trim().to_string(),
        logo: logo.trim().to_string(),
    })
}

/// Parse an M3U playlist in a single pass. Never panics on bad input.
pub fn parse<R: BufRead>(mut r: R) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut pending: Option<Pending> = None;
    let mut first = true;
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let mut line: &str = &text;
        if first {
            line = line.trim_start_matches('\u{feff}');
            first = false;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = parse_extinf(rest);
        } else if line.starts_with('#') {
            continue;
        } else if let Some(p) = pending.take() {
            out.push(Entry {
                kind: kind_of(line),
                id: stream_id(line).unwrap_or_default(),
                name: p.name,
                group: p.group,
                logo: p.logo,
                url: line.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn p(s: &str) -> Vec<Entry> {
        parse(Cursor::new(s.as_bytes().to_vec()))
    }

    #[test]
    fn parses_live_movie_series_kinds() {
        let m = "#EXTM3U\n\
#EXTINF:-1 tvg-ID=\"svt1.se\" tvg-name=\"SE: SVT 1 HD\" tvg-logo=\"https://logo.invalid/x.png\" group-title=\"Sweden - Nationella\",SE: SVT 1 HD\n\
http://host.invalid:8080/USER/PASS/123\n\
#EXTINF:-1 tvg-name=\"Film\" group-title=\"Movies\",Film\n\
http://host.invalid:8080/movie/USER/PASS/456.mkv\n\
#EXTINF:-1 tvg-name=\"Show S01E01\" group-title=\"Series\",Show S01E01\n\
http://host.invalid:8080/series/USER/PASS/789.mp4\n";
        let v = p(m);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].kind, Kind::Live);
        assert_eq!(v[0].id, "123");
        assert_eq!(v[0].name, "SE: SVT 1 HD");
        assert_eq!(v[0].group, "Sweden - Nationella");
        assert_eq!(v[0].logo, "https://logo.invalid/x.png");
        assert_eq!(v[0].url, "http://host.invalid:8080/USER/PASS/123");
        assert_eq!(v[1].kind, Kind::Movie);
        assert_eq!(v[1].id, "456");
        assert_eq!(v[2].kind, Kind::Series);
        assert_eq!(v[2].id, "789");
    }

    #[test]
    fn name_with_comma_and_quotes() {
        // comma inside quoted attributes must not split; last unquoted comma starts the name
        let m = "#EXTINF:-1 tvg-name=\"Foo, Bar\" group-title=\"A, B\",Foo Bar\nhttp://host.invalid/USER/PASS/1\n";
        let v = p(m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].group, "A, B");
        assert_eq!(v[0].name, "Foo Bar");
        // empty display name falls back to tvg-name (which contains a comma)
        let m = "#EXTINF:-1 tvg-name=\"Foo, Bar\" group-title=\"A, B\",\nhttp://host.invalid/USER/PASS/2\n";
        let v = p(m);
        assert_eq!(v[0].name, "Foo, Bar");
        assert_eq!(v[0].group, "A, B");
    }

    #[test]
    fn comma_and_quotes_in_display_name_survive() {
        let m = "#EXTINF:-1 group-title=\"G\",Foo, Bar\nhttp://host.invalid/USER/PASS/1\n\
#EXTINF:-1 group-title=\"G\",Foo \"Bar\" Baz\nhttp://host.invalid/USER/PASS/2\n\
#EXTINF:-1 group-title=\"H\",Odd \" quote\nhttp://host.invalid/USER/PASS/3\n";
        let v = p(m);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].name, "Foo, Bar");
        assert_eq!(v[0].group, "G");
        assert_eq!(v[1].name, "Foo \"Bar\" Baz");
        assert_eq!(v[2].name, "Odd \" quote");
        assert_eq!(v[2].group, "H");
    }

    #[test]
    fn stream_id_edge_cases() {
        assert_eq!(stream_id("http://host.invalid"), None);
        assert_eq!(stream_id("http://host.invalid:8080"), None);
        assert_eq!(stream_id("http://host.invalid/"), None);
        assert_eq!(stream_id("http://host.invalid/a/12#frag.x"), Some("12".into()));
        let v = p("#EXTINF:-1 group-title=\"G\",One\n#EXTINF:-1 group-title=\"G\",Two\nhttp://host.invalid/USER/PASS/5\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Two");
        let v = p("#EXTINF:-1 group-title=\"G\",NoPath\nhttp://host.invalid:8080\n");
        assert_eq!(v[0].id, "");
        assert_eq!(favorite_key(&v[0]), "live:name:nopath|G");
    }

    #[test]
    fn drops_separator_entries() {
        let m = "#EXTINF:-1 tvg-name=\"===== SWEDEN ======\" group-title=\"X\",===== SWEDEN ======\nhttp://host.invalid/USER/PASS/1\n\
#EXTINF:-1 tvg-name=\"Real\" group-title=\"X\",Real\nhttp://host.invalid/USER/PASS/2\n";
        let v = p(m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Real");
    }

    #[test]
    fn extinf_without_url_is_skipped() {
        let m = "#EXTINF:-1 tvg-name=\"A\",A\n\n#EXTINF:-1 tvg-name=\"B\",B\nhttp://host.invalid/USER/PASS/2\n#EXTINF:-1 tvg-name=\"C\",C\n";
        let v = p(m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "B");
    }

    #[test]
    fn handles_crlf_and_bom() {
        let m = "\u{feff}#EXTM3U\r\n#EXTINF:-1 tvg-name=\"A\" group-title=\"G\",A\r\nhttp://host.invalid/USER/PASS/1\r\n";
        let v = p(m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "A");
        assert_eq!(v[0].group, "G");
        assert_eq!(v[0].url, "http://host.invalid/USER/PASS/1");
    }

    #[test]
    fn non_ascii_names_preserved() {
        let m = "#EXTINF:-1 tvg-name=\"Ísland ÞÆÖ 日本\" group-title=\"Ísland\",Ísland ÞÆÖ 日本\nhttp://host.invalid/USER/PASS/1\n";
        let v = p(m);
        assert_eq!(v[0].name, "Ísland ÞÆÖ 日本");
        assert_eq!(v[0].group, "Ísland");
        // invalid UTF-8 must not panic
        let bytes = b"#EXTINF:-1,Bad \xff\xfe name\nhttp://host.invalid/USER/PASS/9\n".to_vec();
        let v = parse(Cursor::new(bytes));
        assert_eq!(v.len(), 1);
        assert!(v[0].name.contains('\u{fffd}'));
    }

    #[test]
    fn stream_id_strips_extension() {
        assert_eq!(stream_id("http://host.invalid/movie/USER/PASS/456.mkv"), Some("456".into()));
        assert_eq!(stream_id("http://host.invalid/USER/PASS/123"), Some("123".into()));
        assert_eq!(stream_id("http://host.invalid/a/b.m3u8?x=1"), Some("b".into()));
        assert_eq!(stream_id("http://host.invalid/a/"), None);
    }

    #[test]
    fn favorite_key_falls_back_to_name_group_when_no_id() {
        let e = Entry {
            kind: Kind::Live,
            id: "123".into(),
            name: "x".into(),
            group: "g".into(),
            logo: String::new(),
            url: String::new(),
        };
        assert_eq!(favorite_key(&e), "live:123");
        let e = Entry { id: String::new(), name: "  SE:   SVT  1 ".into(), group: "G".into(), ..e };
        assert_eq!(favorite_key(&e), "live:name:se: svt 1|G");
    }

    #[test]
    #[ignore]
    fn parses_real_sample() {
        let path = std::env::var("IPTV_SAMPLE").expect("IPTV_SAMPLE");
        let f = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path).unwrap());
        let t = std::time::Instant::now();
        let v = parse(f);
        let el = t.elapsed();
        let c = |k| v.iter().filter(|e| e.kind == k).count();
        eprintln!(
            "entries={} live={} movie={} series={} elapsed={:?}",
            v.len(),
            c(Kind::Live),
            c(Kind::Movie),
            c(Kind::Series),
            el
        );
        assert!((250_000..=300_000).contains(&v.len()));
        assert!(c(Kind::Live) >= 25_000);
        assert!(c(Kind::Movie) >= 50_000);
        assert!(c(Kind::Series) >= 150_000);
        assert!(el.as_secs_f64() < 3.0);
    }
}
