use std::path::{Path, PathBuf};

#[derive(Clone, serde::Deserialize)]
pub struct Config {
    pub playlist_url: String,
}

/// Never prints the URL (it carries the account credentials).
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config").field("playlist_url", &"<redacted>").finish()
    }
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
    match value.get("playlist_url").and_then(|v| v.as_str()) {
        Some(u) if !u.trim().is_empty() => Ok(Config { playlist_url: u.to_string() }),
        _ => anyhow::bail!("{} is missing the required field `playlist_url`", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_playlist_url() {
        let c = Config { playlist_url: "http://host.invalid/get.php?username=USER&password=PASS".into() };
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
