/// Replace credentials in a URL with `***`.
///
/// Handles Xtream-style paths (`/USER/PASS/ID`, `/movie|series|live/USER/PASS/ID.ext`)
/// and `username=` / `password=` query values. Anything else is returned unchanged.
pub fn redact_url(s: &str) -> String {
    let (before_frag, frag) = match s.find('#') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let (base, query) = match before_frag.find('?') {
        Some(i) => (&before_frag[..i], Some(&before_frag[i + 1..])),
        None => (before_frag, None),
    };
    let mut out = redact_path(base);
    if let Some(q) = query {
        out.push('?');
        let parts: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) if k.eq_ignore_ascii_case("username") || k.eq_ignore_ascii_case("password") => {
                    format!("{k}=***")
                }
                _ => kv.to_string(),
            })
            .collect();
        out.push_str(&parts.join("&"));
    }
    out.push_str(frag);
    out
}

fn redact_path(base: &str) -> String {
    let (prefix, rest) = match base.find("://") {
        Some(i) => {
            let after = &base[i + 3..];
            match after.find('/') {
                Some(j) => (&base[..i + 3 + j], &after[j..]),
                None => return base.to_string(),
            }
        }
        None => return base.to_string(),
    };
    let mut segs: Vec<String> = rest.split('/').skip(1).map(str::to_string).collect();
    let kind_prefix = matches!(segs.first().map(String::as_str), Some("movie" | "series" | "live"));
    let cred_at = if kind_prefix && segs.len() == 4 {
        Some(1)
    } else if segs.len() == 3 && stem_is_numeric(&segs[2]) {
        Some(0)
    } else {
        None
    };
    if let Some(i) = cred_at {
        segs[i] = "***".into();
        segs[i + 1] = "***".into();
    }
    format!("{prefix}/{}", segs.join("/"))
}

fn stem_is_numeric(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or("");
    !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_xtream_live_path() {
        assert_eq!(redact_url("http://h:8080/USER/PASS/123"), "http://h:8080/***/***/123");
    }

    #[test]
    fn redacts_movie_and_series_paths() {
        assert_eq!(redact_url("http://h:8080/movie/USER/PASS/9.mkv"), "http://h:8080/movie/***/***/9.mkv");
        assert_eq!(redact_url("http://h:8080/series/USER/PASS/9.mkv"), "http://h:8080/series/***/***/9.mkv");
    }

    #[test]
    fn redacts_query_credentials() {
        assert_eq!(
            redact_url("http://h/get.php?username=a&password=b&type=m3u_plus"),
            "http://h/get.php?username=***&password=***&type=m3u_plus"
        );
        assert_eq!(redact_url("http://h/get.php?username=a&password=b"), "http://h/get.php?username=***&password=***");
    }

    #[test]
    fn leaves_non_credential_url_unchanged() {
        for u in ["http://h/a/b", "http://h/", "http://h", "https://example.com/path/to/file.m3u", "not a url"] {
            assert_eq!(redact_url(u), u);
        }
    }
}
