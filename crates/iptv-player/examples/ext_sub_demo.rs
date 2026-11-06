//! Headless check of the external-subtitle wrappers with real libmpv:
//! `cargo run -p iptv-player --example ext_sub_demo`
//! Makes a 20 s local test video with one embedded subrip track plus an external
//! `.srt` (all lines invented), then checks `sub_add`, `sub_delay`, the secondary
//! track's `secondary_sub_start` and that `load` drops added tracks.
//! Needs `ffmpeg` on PATH. Only local temp files; nothing is printed that is a URL.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use iptv::supervisor::{Backend, PlayerEvent};
use iptv_player::mpv_backend::{MpvBackend, MpvOptions};

fn srt(starts: &[f64]) -> String {
    let ts = |t: f64| {
        let ms = (t * 1000.0).round() as u64;
        format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    };
    starts
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}\n{} --> {}\nLine {}\n\n", i + 1, ts(*s), ts(*s + 2.0), i + 1))
        .collect()
}

/// Pump events; returns true once `Started` was seen (within `secs`).
fn pump_until_started(b: &mut MpvBackend, gen: u32, secs: f64) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        for (g, ev) in b.poll_events() {
            if g == gen && ev == PlayerEvent::Started {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn check(ok: bool, what: &str, all: &mut bool) {
    println!("{} {what}", if ok { "ok  " } else { "FAIL" });
    *all &= ok;
}

fn main() -> anyhow::Result<()> {
    if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        println!("ffmpeg not found on PATH: skipping");
        return Ok(());
    }
    let dir = std::env::temp_dir().join("ext-sub-demo");
    std::fs::create_dir_all(&dir)?;
    let (emb, ext, video) = (dir.join("embedded.srt"), dir.join("external.srt"), dir.join("video.mkv"));
    std::fs::write(&emb, srt(&[2.0, 6.0, 11.0, 16.5]))?;
    std::fs::write(&ext, srt(&[4.5, 8.5, 13.5, 19.0]))?;
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=25:duration=20", "-f", "lavfi", "-i", "sine=frequency=440:duration=20", "-i"])
        .arg(&emb)
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-c:s", "srt", "-shortest"])
        .arg(&video)
        .status()?;
    anyhow::ensure!(st.success(), "ffmpeg failed to make the test video");

    let mut all = true;
    let mut b = MpvBackend::new(MpvOptions::default())?;
    check(b.has_secondary_sub_start(), "has_secondary_sub_start", &mut all);
    let vid = video.to_str().unwrap();
    b.load(vid, None, 1);
    check(pump_until_started(&mut b, 1, 15.0), "playing", &mut all);

    let before = b.sub_tracks();
    check(before.len() == 1, &format!("one embedded track before sub_add (got {})", before.len()), &mut all);
    let embedded_id = before.first().map(|t| t.id);
    check(b.sub_add(&ext, "eng"), "sub_add returned true", &mut all);
    check(!b.sub_add(Path::new("Z:/does/not/exist.srt"), "eng"), "sub_add of a missing file returns false", &mut all);
    let after = b.sub_tracks();
    let imported: Vec<_> = after.iter().filter(|t| t.title.as_deref() == Some("Imported")).collect();
    check(after.len() == 2 && imported.len() == 1 && imported[0].lang.as_deref() == Some("eng"), "one new track titled Imported, lang eng", &mut all);
    check(b.current_sub() != imported.first().map(|t| t.id) || b.current_sub().is_none(), "sub_add did not select the track", &mut all);

    b.set_sub_delay(-2.5);
    check((b.sub_delay() + 2.5).abs() < 1e-6, &format!("sub_delay reads back -2.5 (got {})", b.sub_delay()), &mut all);
    b.set_sub_delay(0.0);

    b.set_secondary_sub(embedded_id);
    let mut seen: Vec<f64> = Vec::new();
    let end = Instant::now() + Duration::from_secs(13);
    while Instant::now() < end {
        let _ = b.poll_events();
        if let Some(t) = b.secondary_sub_start() {
            if !seen.iter().any(|s| (s - t).abs() < 0.001) {
                seen.push(t);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    println!("secondary-sub-start values seen: {seen:?}");
    for want in [2.0, 6.0, 11.0] {
        check(seen.iter().any(|s| (s - want).abs() < 0.05), &format!("secondary start {want} seen"), &mut all);
    }

    // Picking the track that is the current reference: the app clears the
    // secondary first (`Player::select_sub`). Informational: what mpv does
    // when it is NOT cleared first.
    if let (Some(x), Some(imp)) = (embedded_id, imported.first().map(|t| t.id)) {
        b.set_sub(Some(imp));
        b.set_secondary_sub(Some(x));
        b.set_sub(Some(x));
        println!(
            "NOTE: selecting sid {x} while it is secondary-sid, without clearing first: sid reads {:?} (imported is {imp})",
            b.current_sub()
        );
        b.set_secondary_sub(None);
        b.set_sub(Some(imp));
        b.set_secondary_sub(Some(x));
        b.set_secondary_sub(None);
        b.set_sub(Some(x));
        check(b.current_sub() == Some(x), "selecting the former reference track works once the secondary is cleared first", &mut all);
    }

    b.set_secondary_sub(None);
    b.load(vid, None, 2);
    check(pump_until_started(&mut b, 2, 15.0), "playing again after second load", &mut all);
    let again = b.sub_tracks();
    check(again.len() == 1 && again[0].title.as_deref() != Some("Imported"), "imported track gone after load", &mut all);

    b.stop();
    println!("RESULT: {}", if all { "ALL CHECKS OK" } else { "FAILED" });
    Ok(())
}
