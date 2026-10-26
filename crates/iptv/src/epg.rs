//! EPG spike: programme guide data from the provider's Xtream API.
//!
//! Two sources, to compare their richness: the whole XMLTV file (`xmltv.php`,
//! downloaded once) and `get_short_epg` (one small request per channel).
//! Credentials come from the playlist URL and never appear in `Debug` output
//! or error text.

use std::collections::HashMap;

use serde::Deserialize;

/// One programme; times are unix seconds (UTC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Programme {
    pub start: i64,
    pub stop: i64,
    pub title: String,
    pub desc: String,
}

/// Xtream account derived from the playlist URL (`?username=..&password=..`
/// or `/live/<user>/<pass>/<id>` style).
#[derive(Clone)]
pub struct Account {
    base: String,
    user: String,
    pass: String,
}

/// Never prints the host or the credentials.
impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").finish_non_exhaustive()
    }
}

impl Account {
    pub fn from_playlist_url(url: &str) -> Option<Account> {
        let u = reqwest::Url::parse(url).ok()?;
        let host = u.host_str()?;
        let base = match u.port() {
            Some(p) => format!("{}://{host}:{p}", u.scheme()),
            None => format!("{}://{host}", u.scheme()),
        };
        let q: HashMap<_, _> = u.query_pairs().collect();
        if let (Some(user), Some(pass)) = (q.get("username"), q.get("password")) {
            return Some(Account { base, user: user.to_string(), pass: pass.to_string() });
        }
        let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
        let i = segs.iter().position(|s| matches!(*s, "live" | "movie" | "series"))?;
        let (user, pass) = (segs.get(i + 1)?, segs.get(i + 2)?);
        Some(Account { base, user: user.to_string(), pass: pass.to_string() })
    }

    fn client(user_agent: &str, secs: u64) -> Result<reqwest::blocking::Client, String> {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(secs))
            .user_agent(user_agent)
            .build()
            .map_err(|e| format!("http client: {}", e.without_url()))
    }

    fn get(&self, path: &str, extra: &[(&str, &str)], user_agent: &str, secs: u64) -> Result<Vec<u8>, String> {
        let mut url = reqwest::Url::parse(&format!("{}/{path}", self.base)).map_err(|_| "bad url".to_string())?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("username", &self.user).append_pair("password", &self.pass);
            for (k, v) in extra {
                q.append_pair(k, v);
            }
        }
        let resp = Self::client(user_agent, secs)?
            .get(url)
            .send()
            .map_err(|e| format!("request failed: {}", e.without_url()))?;
        if !resp.status().is_success() {
            return Err(format!("server returned HTTP {}", resp.status().as_u16()));
        }
        resp.bytes().map(|b| b.to_vec()).map_err(|e| format!("reading response: {}", e.without_url()))
    }

    /// stream id -> `epg_channel_id` (only streams that have one).
    pub fn fetch_epg_ids(&self, user_agent: &str) -> Result<HashMap<String, String>, String> {
        #[derive(Deserialize)]
        struct Row {
            stream_id: serde_json::Value,
            epg_channel_id: Option<String>,
        }
        let body = self.get("player_api.php", &[("action", "get_live_streams")], user_agent, 60)?;
        let rows: Vec<Row> = serde_json::from_slice(&body).map_err(|_| "live stream list is not valid JSON".to_string())?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id = match r.stream_id {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s,
                    _ => return None,
                };
                r.epg_channel_id.filter(|e| !e.trim().is_empty()).map(|e| (id, e))
            })
            .collect())
    }

    /// The whole guide, channel id (lowercased) -> programmes sorted by start.
    pub fn fetch_xmltv(&self, user_agent: &str) -> Result<HashMap<String, Vec<Programme>>, String> {
        let body = self.get("xmltv.php", &[], user_agent, 120)?;
        Ok(parse_xmltv(&String::from_utf8_lossy(&body)))
    }

    /// `get_short_epg` for one stream.
    pub fn fetch_short(&self, stream_id: &str, limit: u32, user_agent: &str) -> Result<Vec<Programme>, String> {
        let limit = limit.to_string();
        let body = self.get(
            "player_api.php",
            &[("action", "get_short_epg"), ("stream_id", stream_id), ("limit", &limit)],
            user_agent,
            15,
        )?;
        Ok(parse_short_epg(&body))
    }
}

/// Programme airing at `now` and the ones after it (`list` sorted by start).
pub fn now_and_next(list: &[Programme], now: i64) -> &[Programme] {
    let i = list.iter().position(|p| p.stop > now).unwrap_or(list.len());
    &list[i..]
}

/// Unix seconds now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Seconds the local clock is ahead of UTC, from the wall clock vs. unix time
/// (rounded to 15 minutes, within -12 h..=+14 h).
pub fn local_offset_secs() -> i64 {
    let l = crate::recordings::local_now();
    let local = i64::from(l.hour) * 3600 + i64::from(l.minute) * 60 + i64::from(l.second);
    let utc = unix_now().rem_euclid(86400);
    let mut d = (local - utc).rem_euclid(86400);
    if d > 14 * 3600 {
        d -= 86400;
    }
    (d + 450).div_euclid(900) * 900
}

/// `HH:MM` of a unix time shifted by `offset` seconds.
pub fn clock(ts: i64, offset: i64) -> String {
    let t = (ts + offset).rem_euclid(86400);
    format!("{:02}:{:02}", t / 3600, t / 60 % 60)
}

/// Which source a guide came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Xmltv,
    Short,
}

/// How rich a list of upcoming programmes is: (with description, total), over
/// the first `n`.
fn richness(list: &[Programme], n: usize) -> (usize, usize) {
    let l = &list[..list.len().min(n)];
    (l.iter().filter(|p| !p.desc.is_empty()).count(), l.len())
}

/// The richer of the two guides for `now` (both already from `now` on); a tie
/// goes to XMLTV. `short` None = not answered yet.
pub fn richer<'a>(xmltv: &'a [Programme], short: Option<&'a [Programme]>, n: usize) -> (Source, &'a [Programme]) {
    match short {
        Some(s) if richness(s, n) > richness(xmltv, n) => (Source::Short, s),
        _ => (Source::Xmltv, xmltv),
    }
}

pub fn parse_short_epg(body: &[u8]) -> Vec<Programme> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else { return Vec::new() };
    let num = |e: &serde_json::Value, k: &str| match e.get(k)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let text = |e: &serde_json::Value, k: &str| {
        e.get(k)
            .and_then(|s| s.as_str())
            .and_then(base64_decode)
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default()
    };
    let mut out: Vec<Programme> = v
        .get("epg_listings")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Programme {
                start: num(e, "start_timestamp")?,
                stop: num(e, "stop_timestamp")?,
                title: text(e, "title"),
                desc: text(e, "description"),
            })
        })
        .collect();
    out.sort_by_key(|p| p.start);
    out
}

pub fn parse_xmltv(xml: &str) -> HashMap<String, Vec<Programme>> {
    let mut out: HashMap<String, Vec<Programme>> = HashMap::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<programme ") {
        rest = &rest[i + "<programme".len()..];
        let Some(open_end) = rest.find('>') else { break };
        let open = &rest[..open_end];
        let (body, next) = if open.ends_with('/') {
            ("", &rest[open_end + 1..])
        } else {
            let after = &rest[open_end + 1..];
            match after.find("</programme>") {
                Some(j) => (&after[..j], &after[j..]),
                None => break,
            }
        };
        rest = next;
        let (Some(start), Some(stop), Some(ch)) =
            (attr(open, "start").and_then(xmltv_time), attr(open, "stop").and_then(xmltv_time), attr(open, "channel"))
        else {
            continue;
        };
        out.entry(unescape(&ch).to_lowercase()).or_default().push(Programme {
            start,
            stop,
            title: inner(body, "title").map(|t| unescape(&t)).unwrap_or_default(),
            desc: inner(body, "desc").map(|t| unescape(&t)).unwrap_or_default(),
        });
    }
    for v in out.values_mut() {
        v.sort_by_key(|p| p.start);
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&pat) {
        let at = from + i;
        // Whole attribute name only (not `xstart=`).
        if at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace() {
            let v = &tag[at + pat.len()..];
            return v.find('"').map(|e| v[..e].to_string());
        }
        from = at + pat.len();
    }
    None
}

/// Text of the first `<name ...>text</name>` in `body`.
fn inner(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let mut from = 0;
    loop {
        let i = from + body[from..].find(&open)?;
        let after = &body[i + open.len()..];
        // `<title>` or `<title lang="..">`, not `<titles>`.
        if after.starts_with('>') || after.starts_with(' ') {
            let gt = after.find('>')?;
            let text = &after[gt + 1..];
            let end = text.find(&format!("</{name}>"))?;
            return Some(text[..end].trim().to_string());
        }
        from = i + open.len();
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&s| s <= 8) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `20261025230500 +0200` (offset optional, default UTC) -> unix seconds.
fn xmltv_time(s: String) -> Option<i64> {
    let s = s.trim();
    let digits = s.get(..14)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| digits[r].parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(4..6)?, n(6..8)?, n(8..10)?, n(10..12)?, n(12..14)?);
    let mut t = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se;
    let off = s[14..].trim();
    if off.len() == 5 && (off.starts_with('+') || off.starts_with('-')) {
        let oh: i64 = off[1..3].parse().ok()?;
        let om: i64 = off[3..5].parse().ok()?;
        let secs = oh * 3600 + om * 60;
        t -= if off.starts_with('-') { -secs } else { secs };
    }
    Some(t)
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_from_query_and_path_urls() {
        let a = Account::from_playlist_url("http://h.invalid:8080/get.php?username=u1&password=p1&type=m3u_plus").unwrap();
        assert_eq!((a.base.as_str(), a.user.as_str(), a.pass.as_str()), ("http://h.invalid:8080", "u1", "p1"));
        let b = Account::from_playlist_url("http://h.invalid/live/u2/p2/55.ts").unwrap();
        assert_eq!((b.user.as_str(), b.pass.as_str()), ("u2", "p2"));
        assert!(Account::from_playlist_url("http://h.invalid/list.m3u").is_none());
        assert!(!format!("{a:?}").contains("u1"));
    }

    #[test]
    fn xmltv_times() {
        assert_eq!(xmltv_time("19700101000000 +0000".into()), Some(0));
        assert_eq!(xmltv_time("20000101000000".into()), Some(946_684_800));
        assert_eq!(xmltv_time("20000101020000 +0200".into()), Some(946_684_800));
        assert_eq!(xmltv_time("20000101000000 -0100".into()), Some(946_688_400));
        assert_eq!(xmltv_time("garbage".into()), None);
    }

    #[test]
    fn xmltv_programmes() {
        let xml = r#"<tv><channel id="A.se"><display-name>A</display-name></channel>
<programme start="20000101000000 +0000" stop="20000101010000 +0000" channel="A.se"><title lang="sv">Tom &amp; Jerry</title><desc>Cat &#38; mouse</desc></programme>
<programme start="20000101010000 +0000" stop="20000101020000 +0000" channel="A.se"><title>Next</title></programme>
<programme start="20000101000000 +0000" stop="20000101010000 +0000" channel="B"/>
</tv>"#;
        let g = parse_xmltv(xml);
        let a = &g["a.se"];
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].title, "Tom & Jerry");
        assert_eq!(a[0].desc, "Cat & mouse");
        assert_eq!(a[1].desc, "");
        assert_eq!((a[0].start, a[0].stop), (946_684_800, 946_688_400));
        assert_eq!(g["b"][0].title, "");
    }

    #[test]
    fn short_epg_decodes_base64() {
        // "Kråkflickan" / "Hej" in base64.
        let body = br#"{"epg_listings":[{"title":"S3LDpWtmbGlja2Fu","description":"SGVq","start_timestamp":"200","stop_timestamp":"300"},
{"title":"QQ==","description":"","start_timestamp":100,"stop_timestamp":"200"}]}"#;
        let l = parse_short_epg(body);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].start, l[0].title.as_str()), (100, "A"));
        assert_eq!((l[1].title.as_str(), l[1].desc.as_str()), ("Kråkflickan", "Hej"));
        assert!(parse_short_epg(b"nonsense").is_empty());
    }

    #[test]
    fn richer_prefers_descriptions_then_count_and_ties_to_xmltv() {
        let p = |d: &str| Programme { start: 0, stop: 1, title: "t".into(), desc: d.into() };
        let x = vec![p(""), p("")];
        let s = vec![p("d"), p("")];
        assert_eq!(richer(&x, Some(&s), 4).0, Source::Short);
        assert_eq!(richer(&s, Some(&x), 4).0, Source::Xmltv);
        assert_eq!(richer(&x, Some(&x), 4).0, Source::Xmltv);
        assert_eq!(richer(&[], Some(&x), 4).0, Source::Short);
        assert_eq!(richer(&x, None, 4).0, Source::Xmltv);
    }

    #[test]
    fn now_and_next_skips_finished() {
        let p = |s, e| Programme { start: s, stop: e, title: String::new(), desc: String::new() };
        let l = vec![p(0, 10), p(10, 20), p(20, 30)];
        assert_eq!(now_and_next(&l, 15).len(), 2);
        assert_eq!(now_and_next(&l, 30).len(), 0);
    }
}
