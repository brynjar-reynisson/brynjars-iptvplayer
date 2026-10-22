//! Headless intro capture: saves the frames of the first minutes of one
//! cached playlist entry (like the in-app `--console` intro scan dump, but at
//! any times and step, and without playing it), so it needs no waiting:
//! `cargo run --release -p iptv-player --example head_frames -- --data-dir <dir>
//! (--key <favorite key, e.g. series:142253> | --name <text in the entry name>) --out <folder> [--from 0] [--to 300] [--step 1]`
//! Files are `<secs>s_<letter>.png` (see `scan_dump::head_file_name`). Uses one
//! provider connection; close the app first. Prints the entry's name and
//! timings only, never the URL.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use iptv_player::scan_dump::{kind_letter, write_head_frame};
use iptv_player::state::credits::classify;
use iptv_player::thumb_mpv::ThumbMpv;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(arg(&args, "--data-dir").ok_or_else(|| anyhow::anyhow!("--data-dir <dir> missing"))?);
    let out = PathBuf::from(arg(&args, "--out").ok_or_else(|| anyhow::anyhow!("--out <folder> missing"))?);
    let num = |name: &str, default: f64| arg(&args, name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let (from, to, step) = (num("--from", 0.0), num("--to", 300.0), num("--step", 1.0).max(0.1));

    let (lib, _) = iptv::fetch::load_cached(&dir, &[]).ok_or_else(|| anyhow::anyhow!("no cache in the data dir"))?;
    let idx = match (arg(&args, "--key"), arg(&args, "--name")) {
        (Some(key), _) => lib.find_by_key(&key),
        (None, Some(name)) => (0..lib.entry_count()).find(|&i| lib.entry(i).name.contains(&name)),
        _ => anyhow::bail!("--key <favorite key> or --name <text in the entry name> missing"),
    }
    .ok_or_else(|| anyhow::anyhow!("entry not in the playlist"))?;
    let entry = lib.entry(idx);
    println!("entry: {}", entry.name);

    let ua = iptv::config::load(&dir).map_or_else(|_| iptv::config::DEFAULT_USER_AGENT.to_string(), |c| c.user_agent);
    let t0 = Instant::now();
    let mpv = ThumbMpv::new(&ua, 240)?;
    mpv.open(&entry.url, Duration::from_secs(30))?;
    println!("opened in {:.1} s", t0.elapsed().as_secs_f64());

    let t1 = Instant::now();
    let (mut n, mut letters) = (0, String::new());
    let mut t = from;
    while t <= to {
        match mpv.frame_at(t, Duration::from_secs(15)) {
            Ok(th) => {
                let kind = classify(&th.rgba, th.width, th.height);
                letters.push(kind_letter(Some(kind)));
                if let Err(e) = write_head_frame(&out, t, kind, &th) {
                    anyhow::bail!("could not save a frame: {e}");
                }
                n += 1;
            }
            Err(e) => {
                letters.push('?');
                eprintln!("{t:.0} s: {e}");
            }
        }
        t += step;
    }
    println!("{n} frames in {:.1} s ({:.2} s each): {letters}", t1.elapsed().as_secs_f64(), t1.elapsed().as_secs_f64() / n.max(1) as f64);
    Ok(())
}
