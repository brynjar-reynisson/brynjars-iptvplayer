use std::path::{Path, PathBuf};

/// User-Agent sent on playlist downloads and stream requests when
/// `config.toml` sets none. Some providers block requests without a
/// User-Agent (or with an unknown one), e.g. with HTTP 454; a common player
/// UA is accepted.
pub const DEFAULT_USER_AGENT: &str = "VLC/3.0.20 LibVLC/3.0.20";

#[derive(Clone)]
pub struct Config {
    pub playlist_url: String,
    /// Optional `user_agent` key, trimmed; blank/missing = [`DEFAULT_USER_AGENT`].
    pub user_agent: String,
}

/// Never prints the URL (it carries the account credentials).
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("playlist_url", &"<redacted>")
            .field("user_agent", &self.user_agent)
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
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => anyhow::bail!("{} is missing the required field `playlist_url`", path.display()),
    };
    let user_agent = parse_user_agent(value.get("user_agent"), &path)?;
    Ok(Config { playlist_url, user_agent })
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
}
