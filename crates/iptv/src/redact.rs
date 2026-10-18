/// Query keys whose values are known to be harmless; every other value is redacted.
const SAFE_QUERY_KEYS: &[&str] = &["type", "output"];

/// Replace credentials in a URL with `***`. Fails closed: userinfo, credential-looking
/// path segments and every non-allowlisted query value are redacted.
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
                Some((k, _)) => {
                    let dk = percent_decode(k).to_ascii_lowercase();
                    if SAFE_QUERY_KEYS.contains(&dk.as_str()) {
                        kv.to_string()
                    } else {
                        format!("{k}=***")
                    }
                }
                // A bare token (`?TOKEN`) may itself be a credential.
                None if kv.is_empty() => String::new(),
                None => "***".to_string(),
            })
            .collect();
        out.push_str(&parts.join("&"));
    }
    // Fragments can carry tokens too; never keep their content.
    if frag.len() > 1 {
        out.push_str("#***");
    } else {
        out.push_str(frag);
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = |c: u8| (c as char).to_digit(16).unwrap() as u8;
            out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn redact_path(base: &str) -> String {
    let Some(i) = base.find("://") else {
        return base.to_string();
    };
    let scheme_end = i + 3;
    let after = &base[scheme_end..];
    let (authority, rest) = match after.find('/') {
        Some(j) => (&after[..j], &after[j..]),
        None => (after, ""),
    };
    let authority = match authority.rfind('@') {
        Some(at) => format!("***{}", &authority[at..]),
        None => authority.to_string(),
    };
    if rest.is_empty() {
        return format!("{}{}", &base[..scheme_end], authority);
    }
    let mut segs: Vec<String> = rest.split('/').skip(1).map(str::to_string).collect();
    // Ignore trailing empty segments (trailing slash) when positioning.
    let mut n = segs.len();
    while n > 0 && segs[n - 1].is_empty() {
        n -= 1;
    }
    let kind_at = segs[..n]
        .iter()
        .position(|g| ["movie", "series", "live"].contains(&g.to_ascii_lowercase().as_str()));
    let start = match kind_at {
        Some(k) if n >= k + 3 => Some(k + 1),
        Some(_) => None,
        None if n >= 3 => Some(0),
        None => None,
    };
    // Fail closed: every segment from `start` up to (not including) the last
    // one (the id) is redacted, so longer paths (`/auth/U/P/9`) leak nothing.
    if let Some(st) = start {
        for s in &mut segs[st..n - 1] {
            *s = "***".into();
        }
    }
    format!("{}{}/{}", &base[..scheme_end], authority, segs.join("/"))
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
    fn redacts_trailing_slash_and_non_numeric_id() {
        assert_eq!(redact_url("http://h:8080/USER/PASS/123/"), "http://h:8080/***/***/123/");
        assert_eq!(redact_url("http://h/USER/PASS/abc"), "http://h/***/***/abc");
        assert_eq!(redact_url("http://h/USER/PASS/123.ts"), "http://h/***/***/123.ts");
    }

    #[test]
    fn redacts_case_insensitive_kind_and_extra_prefix() {
        assert_eq!(redact_url("http://h/Movie/U/P/9.mkv"), "http://h/Movie/***/***/9.mkv");
        assert_eq!(redact_url("http://h/proxy/movie/U/P/9.mkv"), "http://h/proxy/movie/***/***/9.mkv");
        assert_eq!(redact_url("http://h/live/U/P/5.m3u8"), "http://h/live/***/***/5.m3u8");
    }

    #[test]
    fn redacts_userinfo() {
        assert_eq!(redact_url("http://user:pass@h:80/a"), "http://***@h:80/a");
        assert_eq!(redact_url("http://user:pass@h"), "http://***@h");
    }

    #[test]
    fn redacts_unknown_and_encoded_query_keys() {
        assert_eq!(redact_url("http://h/x?user=a&pass=b&token=c"), "http://h/x?user=***&pass=***&token=***");
        assert_eq!(redact_url("http://h/x?user%6Eame=a&PASSWORD=b"), "http://h/x?user%6Eame=***&PASSWORD=***");
    }

    #[test]
    fn percent_decode_does_not_panic_on_multibyte() {
        assert_eq!(redact_url("http://h/x?%a\u{e9}=x"), "http://h/x?%a\u{e9}=***");
        assert_eq!(redact_url("http://h/x?%\u{e9}=x&%=y&%4=z"), "http://h/x?%\u{e9}=***&%=***&%4=***");
    }

    #[test]
    fn over_redacts_unprefixed_three_segment_url_by_design() {
        // Fail closed: indistinguishable from /USER/PASS/ID, so it is redacted.
        assert_eq!(
            redact_url("https://example.com/path/to/file.m3u"),
            "https://example.com/***/***/file.m3u"
        );
    }

    #[test]
    fn redacts_all_but_last_segment_of_long_paths() {
        // Unprefixed paths with more than 3 segments: everything but the id.
        assert_eq!(redact_url("http://h/auth/U/P/9"), "http://h/***/***/***/9");
        assert_eq!(redact_url("http://h/proxy/U/P/9.ts/"), "http://h/***/***/***/9.ts/");
        // After a kind segment: everything between it and the id.
        assert_eq!(redact_url("http://h/live/U/P/extra/9"), "http://h/live/***/***/***/9");
        // Spec shapes stay exactly as before.
        assert_eq!(redact_url("http://h:8080/U/P/123"), "http://h:8080/***/***/123");
        assert_eq!(redact_url("http://h:8080/movie/U/P/9.mkv"), "http://h:8080/movie/***/***/9.mkv");
        assert_eq!(redact_url("http://h/proxy/movie/U/P/9.mkv"), "http://h/proxy/movie/***/***/9.mkv");
    }

    #[test]
    fn redacts_fragment_and_bare_query_tokens() {
        assert_eq!(redact_url("http://h/a/b#secret"), "http://h/a/b#***");
        assert_eq!(redact_url("http://h/a/b?type=m3u#u=x&p=y"), "http://h/a/b?type=m3u#***");
        assert_eq!(redact_url("http://h/a/b#"), "http://h/a/b#");
        assert_eq!(redact_url("http://h/x?SECRETTOKEN"), "http://h/x?***");
        assert_eq!(redact_url("http://h/x?type=m3u&SECRETTOKEN&"), "http://h/x?type=m3u&***&");
    }

    #[test]
    fn https_with_port() {
        assert_eq!(redact_url("https://h:8443/U/P/7"), "https://h:8443/***/***/7");
    }

    #[test]
    fn leaves_non_credential_url_unchanged() {
        for u in ["http://h/a/b", "http://h/", "http://h", "not a url"] {
            assert_eq!(redact_url(u), u);
        }
    }
}
