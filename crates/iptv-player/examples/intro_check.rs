//! Headless intro detection check on two cached playlist entries:
//! `cargo run --release -p iptv-player --example intro_check -- --data-dir <dir>
//! --name <text in entry A's name> --ref <text in entry B's name>`
//! Scans both with `scan_times` through the app's `IntroScanner` (a pool of up
//! to `SCAN_SOURCES` parallel provider connections per scan), runs
//! `find_intro_pair` and prints both spans, the per-sample letters and the
//! timings. Close the app first. Prints entry names and timings only, never a URL.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use iptv::intro::{find_intro, find_intro_pair, scan_times, INTRO_SCAN_SECS};
use iptv_player::state::skip_intro::{intro_window, refine_times, skip_target};
use iptv_player::intro_scan::{IntroScanner, JobResult};
use iptv_player::scan_dump::kind_letter;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn run(scanner: &IntroScanner, job: u64, url: &str, times: Vec<f64>) -> anyhow::Result<JobResult> {
    scanner.scan(job, url, times, None);
    let t0 = Instant::now();
    loop {
        if let Some(r) = scanner.poll().into_iter().find(|r| r.job == job) {
            if let Some(e) = &r.error {
                anyhow::bail!("could not open the source: {e}");
            }
            println!("job {job} done in {:.1} s", t0.elapsed().as_secs_f64());
            return Ok(r);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(arg(&args, "--data-dir").ok_or_else(|| anyhow::anyhow!("--data-dir <dir> missing"))?);
    let a = arg(&args, "--name").ok_or_else(|| anyhow::anyhow!("--name <text> missing"))?;
    let b = arg(&args, "--ref").ok_or_else(|| anyhow::anyhow!("--ref <text> missing"))?;

    let (lib, _) = iptv::fetch::load_cached(&dir, &[]).ok_or_else(|| anyhow::anyhow!("no cache in the data dir"))?;
    let find = |s: &str| {
        (0..lib.entry_count())
            .find(|&i| lib.entry(i).name.contains(s))
            .ok_or_else(|| anyhow::anyhow!("entry not in the playlist: {s}"))
    };
    let (ea, eb) = (lib.entry(find(&a)?), lib.entry(find(&b)?));
    println!("A: {}\nB: {}", ea.name, eb.name);

    let ua = iptv::config::load(&dir).map_or_else(|_| iptv::config::DEFAULT_USER_AGENT.to_string(), |c| c.user_agent);
    let scanner = IntroScanner::new(ua, || {});
    let t0 = Instant::now();
    let times = scan_times(INTRO_SCAN_SECS + 3.0);
    let ra = run(&scanner, 1, &ea.url, times.clone())?;
    let rb = run(&scanner, 2, &eb.url, times)?;

    let letters = |r: &JobResult| r.kinds.iter().map(|(_, k)| kind_letter(*k)).collect::<String>();
    println!("A: {}\nB: {}", letters(&ra), letters(&rb));
    match find_intro_pair(&ra.prints, &rb.prints) {
        Some((a, b)) => {
            println!("intro span in A: {:.1} .. {:.1} s", a.start, a.end);
            // The skip target: a 1 s refine scan of A from the span end.
            let rr = run(&scanner, 3, &ea.url, refine_times(a.end, 3000.0))?;
            println!("refine frames: {}", rr.kinds.iter().map(|(_, k)| kind_letter(*k)).collect::<String>());
            println!("skip target in A: {:.1} s (span end {:.1} s)", skip_target(a.end, &rr.prints), a.end);
            println!("intro span in B: {:.1} .. {:.1} s", b.start, b.end);
            // What the app stores for B, and what B later finds against it.
            let stored = intro_window(rb.prints.clone(), b.start, b.end);
            match find_intro(&rb.prints, &stored) {
                Some(s) => println!("B against its stored intro ({} samples): {:.1} .. {:.1} s", stored.len(), s.start, s.end),
                None => println!("B against its stored intro: no match"),
            }
        }
        None => println!("no intro found"),
    }
    println!("whole check: {:.1} s", t0.elapsed().as_secs_f64());
    Ok(())
}
