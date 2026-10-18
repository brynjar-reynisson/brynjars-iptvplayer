//! IPTV Player: browse the playlist and play it in one window.
//!
//! Usage: `iptv-player [--data-dir <path>] [--playlist-file <path.m3u>]`
//! - `--data-dir`: where `config.toml`, `cache/`, `favorites.json` and
//!   `settings.json` live
//!   (default: `IPTV_DATA_DIR`, then the exe's directory).
//! - `--playlist-file` (dev): parse a local `.m3u` instead of downloading
//!   `playlist_url`; the result is still cached under the data dir.
//!
//! Release builds use the Windows GUI subsystem (no console window when
//! started from Explorer). With `--console`, the process attaches to the
//! terminal that started it so status/timing lines on stderr show there.

// Release: no console window. Debug builds keep the console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Instant;

use iptv_player::ui::{run, Options};

const USAGE: &str = "usage: iptv-player [--console] [--data-dir <path>] [--playlist-file <path.m3u>]";

/// GUI-subsystem builds start without a console: borrow the parent's (the
/// terminal that started us), if any, so stdout/stderr reach it. Redirected
/// handles (pipes/files) are inherited as usual and keep working.
#[cfg(windows)]
fn attach_parent_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX; // (DWORD)-1
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // SAFETY: plain Win32 call with a constant argument; failure (no parent
    // console, or already attached) is harmless and ignored.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn main() -> anyhow::Result<()> {
    let t0 = Instant::now();
    // Opt-in: attached output lands after the shell has printed its prompt,
    // which looks like a hung command prompt until Ctrl+C.
    if std::env::args().any(|a| a == "--console") {
        attach_parent_console();
    }
    let mut data_dir = None;
    let mut playlist_file = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?)),
            "--playlist-file" => {
                playlist_file = Some(PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?))
            }
            "--console" => {}
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
