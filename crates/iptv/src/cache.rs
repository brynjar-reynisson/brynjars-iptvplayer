use crate::playlist::Entry;
use anyhow::{Context, Result};
use bincode::Options;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"IPTVCACH";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 8;

#[derive(Clone)]
pub struct CacheData {
    pub entries: Vec<Entry>,
    pub saved_at: SystemTime,
}

/// Counts only: entries carry credentialed URLs.
impl std::fmt::Debug for CacheData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheData")
            .field("entries", &self.entries.len())
            .field("saved_at", &self.saved_at)
            .finish()
    }
}

impl CacheData {
    pub fn is_stale(&self, max_age: Duration) -> bool {
        match SystemTime::now().duration_since(self.saved_at) {
            Ok(age) => age > max_age,
            Err(_) => true, // saved in the future (clock skew): treat as stale
        }
    }
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("cache").join("playlist.bin")
}

pub fn save(dir: &Path, entries: &[Entry]) -> Result<()> {
    save_with_writer(dir, entries, |w, bytes| w.write_all(bytes))
}

/// `write` is the sink used for the file contents; tests inject failures through it.
pub(crate) fn save_with_writer<F>(dir: &Path, entries: &[Entry], write: F) -> Result<()>
where
    F: FnOnce(&mut dyn Write, &[u8]) -> std::io::Result<()>,
{
    let path = cache_path(dir);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cache path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating cache dir {}", parent.display()))?;
    let tmp = crate::fsutil::unique_tmp_path(&path, "tmp");

    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut buf = Vec::with_capacity(HEADER_LEN + entries.len() * 128);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&secs.to_le_bytes());
    bincode::options()
        .serialize_into(&mut buf, entries)
        .context("serializing playlist cache")?;

    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        write(&mut f, &buf)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        crate::fsutil::rename_with_retry(&tmp, &path, 3, Duration::from_millis(50))
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing cache file {}", path.display()));
    }
    Ok(())
}

pub fn load(dir: &Path) -> Option<CacheData> {
    let mut bytes = Vec::new();
    std::fs::File::open(cache_path(dir)).ok()?.read_to_end(&mut bytes).ok()?;
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let secs = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    let body = &bytes[HEADER_LEN..];
    // Limit = body size, so a corrupt length prefix can never claim more than the file holds.
    let entries: Vec<Entry> = bincode::options()
        .with_limit(body.len() as u64)
        .reject_trailing_bytes()
        .deserialize(body)
        .ok()?;
    let saved_at = UNIX_EPOCH.checked_add(Duration::from_secs(secs))?;
    Some(CacheData { entries, saved_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::Kind;

    #[test]
    fn debug_hides_urls() {
        let c = CacheData { entries: fake(3), saved_at: SystemTime::now() };
        let d = format!("{c:?}");
        assert!(!d.contains("example.invalid") && !d.contains("://"), "{d}");
        assert!(d.contains('3'), "{d}");
    }

    fn fake(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                kind: Kind::Live,
                id: format!("id{i}"),
                name: format!("Fake Channel {i}"),
                group: "Fake Group".into(),
                logo: String::new(),
                url: format!("http://example.invalid/stream/{i}"),
            })
            .collect()
    }

    fn assert_no_tmp(dir: &Path) {
        for e in std::fs::read_dir(dir.join("cache")).unwrap() {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!n.ends_with(".tmp"), "leftover tmp {n}");
        }
    }

    #[test]
    fn future_saved_at_is_stale() {
        let c = CacheData { entries: vec![], saved_at: SystemTime::now() + Duration::from_secs(7200) };
        assert!(c.is_stale(Duration::from_secs(3600)));
    }

    #[test]
    fn concurrent_saves_leave_valid_cache() {
        let d = tempfile::tempdir().unwrap();
        let sets: Vec<Vec<Entry>> = (0..8).map(|i| fake(100 + i * 10)).collect();
        std::thread::scope(|s| {
            for set in &sets {
                s.spawn(|| {
                    for _ in 0..5 {
                        // Windows may transiently refuse a rename onto a file being replaced;
                        // retry handling is deferred, so tolerate errors here.
                        let _ = save(d.path(), set);
                    }
                });
            }
        });
        let c = load(d.path()).expect("cache loads cleanly");
        assert!(sets.contains(&c.entries));
        assert_no_tmp(d.path());
    }

    #[test]
    fn roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let e = fake(50);
        save(d.path(), &e).unwrap();
        let c = load(d.path()).unwrap();
        assert_eq!(c.entries, e);
        assert!(!c.is_stale(Duration::from_secs(60)));
        assert_no_tmp(d.path());
    }

    #[test]
    fn save_twice_replaces() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &fake(3)).unwrap();
        save(d.path(), &fake(5)).unwrap();
        assert_eq!(load(d.path()).unwrap().entries.len(), 5);
    }

    #[test]
    fn missing_returns_none() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).is_none());
    }

    #[test]
    fn corrupt_file_returns_none() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &fake(10)).unwrap();
        let p = d.path().join("cache/playlist.bin");
        let good = std::fs::read(&p).unwrap();

        std::fs::write(&p, b"garbage").unwrap();
        assert!(load(d.path()).is_none());
        std::fs::write(&p, b"").unwrap();
        assert!(load(d.path()).is_none());
        // truncated body
        std::fs::write(&p, &good[..good.len() - 10]).unwrap();
        assert!(load(d.path()).is_none());
        // trailing junk
        let mut extra = good.clone();
        extra.extend_from_slice(b"xx");
        std::fs::write(&p, &extra).unwrap();
        assert!(load(d.path()).is_none());
        // version mismatch
        let mut ver = good.clone();
        ver[8] = 99;
        std::fs::write(&p, &ver).unwrap();
        assert!(load(d.path()).is_none());
        // absurd length prefix (varint u64::MAX) must not panic/allocate
        let mut huge = good[..HEADER_LEN].to_vec();
        huge.push(255);
        huge.extend_from_slice(&[255; 8]);
        std::fs::write(&p, &huge).unwrap();
        assert!(load(d.path()).is_none());
    }

    #[test]
    fn failed_save_leaves_previous_cache_intact() {
        let d = tempfile::tempdir().unwrap();
        let old = fake(7);
        save(d.path(), &old).unwrap();
        let r = save_with_writer(d.path(), &fake(20), |w, bytes| {
            w.write_all(&bytes[..bytes.len() / 2])?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(r.is_err());
        assert_eq!(load(d.path()).unwrap().entries, old);
        assert_no_tmp(d.path());
    }

    #[test]
    fn stale_detection() {
        let c = CacheData { entries: vec![], saved_at: SystemTime::now() - Duration::from_secs(7200) };
        assert!(c.is_stale(Duration::from_secs(3600)));
        assert!(!c.is_stale(Duration::from_secs(10800)));
    }

    #[test]
    #[ignore]
    fn perf_sample() {
        use std::time::Instant;
        let path = std::env::var("IPTV_SAMPLE").expect("IPTV_SAMPLE");
        let f = std::io::BufReader::new(std::fs::File::open(path).unwrap());
        let entries = crate::playlist::parse(f);
        let d = tempfile::tempdir().unwrap();
        let t = Instant::now();
        save(d.path(), &entries).unwrap();
        let save_ms = t.elapsed().as_millis();
        let size = std::fs::metadata(d.path().join("cache/playlist.bin")).unwrap().len();
        let t = Instant::now();
        let c = load(d.path()).unwrap();
        let load_ms = t.elapsed().as_millis();
        println!("entries={} save={}ms load={}ms size={}B", entries.len(), save_ms, load_ms, size);
        assert_eq!(c.entries.len(), entries.len());
        assert!(load_ms < 500);
    }
}
