use std::path::{Path, PathBuf};

/// User-Agent sent on playlist downloads and stream requests when
/// `config.toml` sets none. Some providers block requests without a
/// User-Agent (or with an unknown one), e.g. with HTTP 454; a common player
/// UA is accepted.
pub const DEFAULT_USER_AGENT: &str = "VLC/3.0.20 LibVLC/3.0.20";

/// The `config.toml` written on first run (same text as `config.example.toml`,
/// empty `playlist_url`).
pub const DEFAULT_CONFIG: &str = include_str!("../../../config.example.toml");

/// Host of the placeholder URL older example files shipped with: counts as "not set".
const PLACEHOLDER_HOST: &str = "example.invalid";

/// `config.toml` parses but has no usable `playlist_url` (missing, blank or the
/// example placeholder). The app answers this with the URL prompt; any other
/// load error is a broken file. Never carries the URL.
#[derive(Debug)]
pub struct UrlUnset {
    path: PathBuf,
}

impl std::fmt::Display for UrlUnset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is missing the required field `playlist_url`", self.path.display())
    }
}

impl std::error::Error for UrlUnset {}

#[derive(Clone)]
pub struct Config {
    pub playlist_url: String,
    /// Optional `user_agent` key, trimmed; blank/missing = [`DEFAULT_USER_AGENT`].
    pub user_agent: String,
    /// Optional `top_countries` key, a comma-separated list (`"Iceland, UK"`);
    /// blank/missing = none. Those live countries are listed first, in this
    /// order, the rest alphabetically. The old single `top_country` key is
    /// still read when `top_countries` is absent.
    pub top_countries: Vec<String>,
}

/// Never prints the URL (it carries the account credentials).
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("playlist_url", &"<redacted>")
            .field("user_agent", &self.user_agent)
            .field("top_countries", &self.top_countries)
            .finish()
    }
}

/// `value` of the optional `user_agent` key -> the UA to send.
fn parse_user_agent(value: Option<&toml::Value>, path: &Path) -> anyhow::Result<String> {
    let ua = match value {
        None => return Ok(DEFAULT_USER_AGENT.to_string()),
        Some(v) => v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("{}: `user_agent` must be a string", path.display()))?
            .trim(),
    };
    if ua.is_empty() {
        return Ok(DEFAULT_USER_AGENT.to_string());
    }
    if ua.chars().any(char::is_control) {
        anyhow::bail!("{}: `user_agent` must not contain control characters or line breaks", path.display());
    }
    Ok(ua.to_string())
}

/// Data directory: CLI `--data-dir`, then env `IPTV_DATA_DIR`, then the exe's directory.
pub fn data_dir(cli: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli {
        return p;
    }
    if let Some(p) = std::env::var_os("IPTV_DATA_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Load `config.toml` from `dir`. Errors name the file/field, never the URL.
pub fn load(dir: &Path) -> anyhow::Result<Config> {
    let path = dir.join("config.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {}", path.display(), e.kind()))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|_| anyhow::anyhow!("{} is not valid TOML", path.display()))?;
    let playlist_url = match value.get("playlist_url").and_then(|v| v.as_str()) {
        Some(u) if !u.trim().is_empty() && !is_placeholder(u) => u.to_string(),
        _ => return Err(UrlUnset { path }.into()),
    };
    let user_agent = parse_user_agent(value.get("user_agent"), &path)?;
    let top_countries = {
        let key = if value.get("top_countries").is_some() { "top_countries" } else { "top_country" };
        match value.get(key) {
            None => Vec::new(),
            Some(v) => v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{}: `{key}` must be a string", path.display()))?
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        }
    };
    Ok(Config { playlist_url, user_agent, top_countries })
}

fn is_placeholder(url: &str) -> bool {
    url.to_ascii_lowercase().contains(PLACEHOLDER_HOST)
}

/// Writes the default `config.toml` into `dir` if there is none (never
/// overwrites). Returns whether it was created.
pub fn ensure_default(dir: &Path) -> std::io::Result<bool> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    match std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join("config.toml")) {
        Ok(mut f) => {
            f.write_all(DEFAULT_CONFIG.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Checks a pasted playlist URL: trimmed, `http(s)://` plus a host, no
/// whitespace or control characters. The error text never contains the input.
pub fn validate_playlist_url(input: &str) -> Result<String, &'static str> {
    let url = input.trim();
    if url.is_empty() {
        return Err("Paste the playlist URL from your provider.");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The URL must not contain spaces or line breaks.");
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://"));
    match rest {
        Some(r) if r.chars().next().is_some_and(|c| c.is_alphanumeric()) => {}
        _ => return Err("The URL must start with http:// or https://."),
    }
    if is_placeholder(url) {
        return Err("That is the example address; use the URL from your provider.");
    }
    Ok(url.to_string())
}

/// Sets `playlist_url` in `dir/config.toml`, changing only that line (comments
/// and the other keys stay; a missing file is created from the default).
/// Errors name the file, never the URL.
pub fn set_playlist_url(dir: &Path, url: &str) -> anyhow::Result<()> {
    let url = validate_playlist_url(url).map_err(|m| anyhow::anyhow!("{m}"))?;
    let path = dir.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_string(),
        Err(e) => anyhow::bail!("cannot read {}: {}", path.display(), e.kind()),
    };
    let value = toml::Value::String(url).to_string();
    let mut out = String::with_capacity(text.len() + value.len() + 20);
    let mut done = false;
    for line in text.split_inclusive('\n') {
        let is_key = line
            .trim_start()
            .strip_prefix("playlist_url")
            .is_some_and(|r| r.trim_start().starts_with('='));
        if is_key && !done {
            let eol = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            out.push_str(&format!("playlist_url = {value}{eol}"));
            done = true;
        } else {
            out.push_str(line);
        }
    }
    if !done {
        out.insert_str(0, &format!("playlist_url = {value}\n"));
    }
    crate::fsutil::atomic_write(&path, out.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {}", path.display(), e.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL_LINE: &str = "playlist_url = \"http://host.invalid/get.php?username=USER&password=PASS\"\n";

    fn load_with(extra: &str) -> anyhow::Result<Config> {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), format!("{URL_LINE}{extra}")).unwrap();
        load(d.path())
    }

    #[test]
    fn user_agent_defaults_to_vlc() {
        assert_eq!(DEFAULT_USER_AGENT, "VLC/3.0.20 LibVLC/3.0.20");
        assert_eq!(load_with("").unwrap().user_agent, DEFAULT_USER_AGENT);
    }

    #[test]
    fn user_agent_override_and_blank() {
        assert_eq!(load_with("user_agent = \"MyPlayer/1.0 (Windows)\"\n").unwrap().user_agent, "MyPlayer/1.0 (Windows)");
        assert_eq!(load_with("user_agent = \"  Trim/2  \"\n").unwrap().user_agent, "Trim/2");
        assert_eq!(load_with("user_agent = \"\"\n").unwrap().user_agent, DEFAULT_USER_AGENT);
        assert_eq!(load_with("user_agent = \"   \"\n").unwrap().user_agent, DEFAULT_USER_AGENT);
    }

    #[test]
    fn top_countries_is_an_optional_trimmed_comma_list() {
        assert!(load_with("").unwrap().top_countries.is_empty());
        assert_eq!(load_with("top_countries = \"  Iceland \"\n").unwrap().top_countries, ["Iceland"]);
        assert_eq!(load_with("top_countries = \"Iceland, UK ,, \"\n").unwrap().top_countries, ["Iceland", "UK"]);
        assert!(load_with("top_countries = \"  \"\n").unwrap().top_countries.is_empty());
        assert!(format!("{:#}", load_with("top_countries = 5\n").unwrap_err()).contains("top_countries"));
        // Old single-country key still works; the new key wins when both are set.
        assert_eq!(load_with("top_country = \"Iceland\"\n").unwrap().top_countries, ["Iceland"]);
        assert_eq!(load_with("top_country = \"X\"\ntop_countries = \"Y\"\n").unwrap().top_countries, ["Y"]);
        assert!(format!("{:#}", load_with("top_country = 5\n").unwrap_err()).contains("top_country"));
    }

    #[test]
    fn invalid_user_agent_is_rejected_without_url() {
        for extra in ["user_agent = \"bad\\nagent\"\n", "user_agent = \"tab\\there\"\n", "user_agent = 5\n"] {
            let err = format!("{:#}", load_with(extra).unwrap_err());
            assert!(err.contains("user_agent"), "{err}");
            assert!(!err.contains("host.invalid") && !err.contains("USER") && !err.contains("PASS"), "{err}");
        }
    }

    #[test]
    fn debug_hides_playlist_url() {
        let c = Config {
            playlist_url: "http://host.invalid/get.php?username=USER&password=PASS".into(),
            user_agent: DEFAULT_USER_AGENT.into(),
            top_countries: Vec::new(),
        };
        let d = format!("{c:?}");
        assert!(!d.contains("host.invalid") && !d.contains("USER") && !d.contains("PASS") && !d.contains("://"), "{d}");
    }

    #[test]
    fn cli_overrides_env_overrides_exe_dir() {
        // Single test touches the env var to avoid races between tests.
        std::env::set_var("IPTV_DATA_DIR", "/from/env");
        assert_eq!(data_dir(Some(PathBuf::from("/from/cli"))), PathBuf::from("/from/cli"));
        assert_eq!(data_dir(None), PathBuf::from("/from/env"));
        std::env::remove_var("IPTV_DATA_DIR");
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(data_dir(None), exe_dir);
    }

    #[test]
    fn load_reports_missing_file_without_url() {
        let d = tempfile::tempdir().unwrap();
        let err = format!("{:#}", load(d.path()).unwrap_err());
        assert!(err.contains("config.toml"), "{err}");
        assert!(!err.contains("http"), "{err}");
    }

    #[test]
    fn load_reads_url_and_reports_missing_field() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "playlist_url = \"http://h/x\"\n").unwrap();
        assert_eq!(load(d.path()).unwrap().playlist_url, "http://h/x");
        std::fs::write(d.path().join("config.toml"), "other = \"http://secret/USER/PASS\"\n").unwrap();
        let err = format!("{:#}", load(d.path()).unwrap_err());
        assert!(err.contains("playlist_url"), "{err}");
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn default_config_parses_with_empty_url_and_default_agent() {
        let v: toml::Value = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(v.get("playlist_url").and_then(|u| u.as_str()), Some(""));
        assert_eq!(v.get("user_agent").and_then(|u| u.as_str()), Some(DEFAULT_USER_AGENT));
        assert!(v.get("top_countries").is_none());
    }

    #[test]
    fn ensure_default_creates_once_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("nested");
        assert!(ensure_default(&dir).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("config.toml")).unwrap(), DEFAULT_CONFIG);
        std::fs::write(dir.join("config.toml"), "playlist_url = \"http://h/x\"\n").unwrap();
        assert!(!ensure_default(&dir).unwrap());
        assert_eq!(load(&dir).unwrap().playlist_url, "http://h/x");
    }

    #[test]
    fn blank_and_placeholder_urls_are_unset() {
        let d = tempfile::tempdir().unwrap();
        let bodies = [
            "playlist_url = \"\"\n",
            "playlist_url = \"  \"\n",
            "playlist_url = \"http://example.invalid/get.php?username=USER&password=PASS\"\n",
            "x = 1\n",
        ];
        for body in bodies {
            std::fs::write(d.path().join("config.toml"), body).unwrap();
            let err = load(d.path()).unwrap_err();
            assert!(err.downcast_ref::<UrlUnset>().is_some(), "{body}");
        }
        // A broken file is not "unset".
        std::fs::write(d.path().join("config.toml"), "playlist_url = [").unwrap();
        assert!(load(d.path()).unwrap_err().downcast_ref::<UrlUnset>().is_none());
        std::fs::write(d.path().join("config.toml"), "playlist_url = \"http://h/x\"\nuser_agent = 5\n").unwrap();
        assert!(load(d.path()).unwrap_err().downcast_ref::<UrlUnset>().is_none());
    }

    #[test]
    fn validate_playlist_url_cases() {
        let ok = validate_playlist_url("  http://h.example/get.php?username=u&password=p \n").unwrap();
        assert_eq!(ok, "http://h.example/get.php?username=u&password=p");
        assert!(validate_playlist_url("HTTPS://h.example/x").is_ok());
        for bad in ["", "   ", "h.example/x", "ftp://h/x", "http://", "http:// h/x", "http://h/a b", "http://example.invalid/x"] {
            assert!(validate_playlist_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn set_playlist_url_changes_only_that_line() {
        let d = tempfile::tempdir().unwrap();
        ensure_default(d.path()).unwrap();
        set_playlist_url(d.path(), "http://h.example/get.php?username=u&password=p").unwrap();
        let text = std::fs::read_to_string(d.path().join("config.toml")).unwrap();
        let want = DEFAULT_CONFIG.replacen(
            "playlist_url = \"\"",
            "playlist_url = \"http://h.example/get.php?username=u&password=p\"",
            1,
        );
        assert_eq!(text, want);
        let c = load(d.path()).unwrap();
        assert_eq!(c.playlist_url, "http://h.example/get.php?username=u&password=p");
        assert_eq!(c.user_agent, DEFAULT_USER_AGENT);
    }

    #[test]
    fn set_playlist_url_handles_crlf_missing_key_and_missing_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "# note\r\nplaylist_url   =  \"\"\r\nuser_agent = \"X/1\"\r\n").unwrap();
        set_playlist_url(d.path(), "http://h.example/a").unwrap();
        let want = "# note\r\nplaylist_url = \"http://h.example/a\"\r\nuser_agent = \"X/1\"\r\n";
        assert_eq!(std::fs::read_to_string(&p).unwrap(), want);
        std::fs::write(&p, "user_agent = \"X/1\"\n").unwrap();
        set_playlist_url(d.path(), "http://h.example/b").unwrap();
        let c = load(d.path()).unwrap();
        assert_eq!((c.playlist_url.as_str(), c.user_agent.as_str()), ("http://h.example/b", "X/1"));
        std::fs::remove_file(&p).unwrap();
        set_playlist_url(d.path(), "http://h.example/c").unwrap();
        assert_eq!(load(d.path()).unwrap().playlist_url, "http://h.example/c");
    }

    #[test]
    fn set_playlist_url_rejects_bad_input_without_echoing_it() {
        let d = tempfile::tempdir().unwrap();
        let err = format!("{:#}", set_playlist_url(d.path(), "nonsense SECRET").unwrap_err());
        assert!(!err.contains("SECRET"), "{err}");
        assert!(!d.path().join("config.toml").exists());
    }
}
