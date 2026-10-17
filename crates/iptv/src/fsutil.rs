//! Small shared filesystem helpers: unique temp names, rename with retry, atomic write.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<dest file name>.<pid>.<counter>.<suffix>` in the same directory as `dest`.
/// Unique per call within a process and across processes, so concurrent writers never collide.
pub fn unique_tmp_path(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(
        ".{}.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        suffix
    ));
    dest.with_file_name(name)
}

/// Rename, retrying up to `attempts` times (sleeping `delay` between) because Windows may
/// transiently refuse to replace a file that is momentarily open elsewhere.
pub fn rename_with_retry(from: &Path, to: &Path, attempts: u32, delay: Duration) -> std::io::Result<()> {
    let attempts = attempts.max(1);
    let mut last = None;
    for attempt in 0..attempts {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < attempts {
            std::thread::sleep(delay);
        }
    }
    Err(last.unwrap())
}

/// Atomically replace `path` with `bytes`: unique tmp in the same dir, write, fsync, rename
/// (3 tries). Creates the parent directory. On failure only this call's own tmp is removed.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = unique_tmp_path(path, "tmp");
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        rename_with_retry(&tmp, path, 3, Duration::from_millis(50))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_differ_and_stay_in_dir() {
        let p = Path::new("/a/b/file.json");
        let (a, b) = (unique_tmp_path(p, "tmp"), unique_tmp_path(p, "tmp"));
        assert_ne!(a, b);
        assert_eq!(a.parent(), p.parent());
        assert!(a.to_string_lossy().ends_with(".tmp"));
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("f.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn failed_rename_removes_own_tmp_only() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("target");
        std::fs::create_dir(&p).unwrap(); // renaming a file onto a directory fails
        let other = d.path().join("f.txt.1.1.tmp");
        std::fs::write(&other, b"x").unwrap();
        let other2 = d.path().join("target.1.1.tmp");
        std::fs::write(&other2, b"x").unwrap();
        assert!(atomic_write(&p, b"data").is_err());
        assert!(other.exists() && other2.exists());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 3);
    }
}
