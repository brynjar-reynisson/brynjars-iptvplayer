//! IPTV Player: browse the playlist and play it in one window.
//!
//! Usage: `iptv-player [--data-dir <path>] [--playlist-file <path.m3u>]`
//! - `--data-dir`: where `config.toml`, `cache/` and `favorites.json` live
//!   (default: `IPTV_DATA_DIR`, then the exe's directory).
//! - `--playlist-file` (dev): parse a local `.m3u` instead of downloading
//!   `playlist_url`; the result is still cached under the data dir.

use std::path::PathBuf;
use std::time::Instant;

use iptv_player::ui::{run, Options};

const USAGE: &str = "usage: iptv-player [--data-dir <path>] [--playlist-file <path.m3u>]";

fn main() -> anyhow::Result<()> {
    let t0 = Instant::now();
    let mut data_dir = None;
    let mut playlist_file = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?)),
            "--playlist-file" => {
                playlist_file = Some(PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?))
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => anyhow::bail!("{USAGE}"),
        }
    }
    let data_dir = iptv::config::data_dir(data_dir);
    eprintln!("data dir: {}", data_dir.display());
    run(Options { data_dir, playlist_file, t0 })
}
