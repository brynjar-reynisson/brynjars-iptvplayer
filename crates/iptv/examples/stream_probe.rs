//! Dev probe (prints no URL): `stream_probe <data dir> <favorite key> [secs]`
//! opens the channel's stream like the scheduled recorder does and prints the
//! response headers (names and values, no URL) and how long / how much arrives.
use std::io::Read;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (dir, key) = (std::path::PathBuf::from(&a[0]), &a[1]);
    let secs: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let cfg = iptv::config::load(&dir)?;
    let (lib, _) = iptv::fetch::load_cached(&dir, &[]).ok_or_else(|| anyhow::anyhow!("no cache"))?;
    let e = lib.find_by_key(key).ok_or_else(|| anyhow::anyhow!("no such channel"))?;
    let url = lib.entry(e).url.clone();
    println!("url ends with: .{}", url.rsplit('.').next().unwrap_or("?").chars().take(4).collect::<String>());
    let client = reqwest::blocking::Client::builder()
        .user_agent(cfg.user_agent)
        .connect_timeout(Duration::from_secs(15))
        .timeout(None::<Duration>)
        .build()?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let mut r = client.get(&url).send().map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
        println!("HTTP {} {:?}", r.status().as_u16(), r.version());
        for (k, v) in r.headers() {
            let k = k.as_str();
            if !matches!(k, "location" | "set-cookie") {
                println!("  {k}: {}", v.to_str().unwrap_or("?"));
            }
        }
        let start = Instant::now();
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        let end = loop {
            match r.read(&mut buf) {
                Ok(0) => break "eof".to_string(),
                Ok(n) => total += n as u64,
                Err(e) => break format!("error: {e}"),
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                break "probe time over".into();
            }
        };
        println!("-> {:.1} MB in {:.1} s, then {end}", total as f64 / 1e6, start.elapsed().as_secs_f64());
    }
    Ok(())
}
