//! Dev check of the gap cards: `merge_demo <dir>` builds two small clips with a
//! 12 s gap and a 1.5 s gap-free overlap boundary, joins them with `rec_merge::finalize`
//! and leaves the result in `<dir>` (inspect it with a player). Needs ffmpeg.
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use iptv::recordings::PieceInfo;

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: merge_demo <dir>"));
    std::fs::create_dir_all(&dir)?;
    let base = "Demo - 2026-10-25 18-30";
    let ff = iptv::rec_merge::find_ffmpeg().expect("ffmpeg");
    // `merge_demo <dir> existing`: <dir> already holds `<base>.piece-*.ts` files (e.g. cut
    // from a real recording); journal them 8 s apart and join.
    if std::env::args().nth(2).as_deref() == Some("existing") {
        let mut names: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| iptv::rec_merge::base_of(n) == Some(base))
            .collect();
        names.sort();
        for (i, n) in names.iter().enumerate() {
            let begin = i as u64 * 20_000;
            iptv::rec_merge::journal_append(&dir, base, &PieceInfo { file: n.clone(), begin, end: begin + 12_000 })?;
        }
        let o = iptv::rec_merge::finalize(&dir, base, Some(&ff), Duration::ZERO, &mut |m| println!("log: {m}"));
        println!("{o:?}");
        return Ok(());
    }
    for (n, (size, rate, secs)) in [("rec1", ("640x360", 25, 4)), ("app2", ("640x360", 25, 4)), ("rec3", ("640x360", 25, 3))] {
        let out = dir.join(format!("{base}.piece-{n}.ts"));
        let st = Command::new(&ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={size}:rate={rate}:duration={secs}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency={}:sample_rate=48000:duration={secs}", if n == "app2" { 880 } else { 440 }))
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac", "-f", "mpegts"])
            .arg(&out)
            .status()?;
        anyhow::ensure!(st.success());
    }
    // rec1 0-4 s, app2 16-20 s (a 12 s gap), rec3 21-24 s (a 1 s gap: no card).
    for (n, b, e) in [("rec1", 0, 4_000), ("app2", 16_000, 20_000), ("rec3", 21_000, 24_000)] {
        iptv::rec_merge::journal_append(&dir, base, &PieceInfo { file: format!("{base}.piece-{n}.ts"), begin: b, end: e })?;
    }
    let o = iptv::rec_merge::finalize(&dir, base, Some(&ff), Duration::ZERO, &mut |m| println!("log: {m}"));
    println!("{o:?}");
    Ok(())
}
