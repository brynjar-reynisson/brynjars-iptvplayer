//! Who holds the provider connection during a scheduled recording. The
//! account allows one connection at a time (a second one makes the provider
//! cut the first, over and over), so while a schedule's window is open either
//! the app records the channel it is playing (mpv's `stream-record`) or the
//! headless recorder ([`crate::rec_job`]) records it, never both. The app
//! claims the recording with `recording-owner.json` (rewritten every
//! [`BEAT`]); the recorder stands by while that claim is fresh and takes over
//! when it is released or goes stale (the app closed or crashed).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The app rewrites its claim this often.
pub const BEAT: Duration = Duration::from_secs(3);
/// A claim older than this is dead.
const FRESH_SECS: u64 = 8;

#[derive(Debug, Serialize, Deserialize)]
struct Owner {
    /// Schedule id.
    id: String,
    pid: u32,
    /// Unix seconds of the last beat.
    at: u64,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("recording-owner.json")
}

fn epoch() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The app (this process) records schedule `id`: write or refresh the claim.
pub fn claim(dir: &Path, id: &str) -> std::io::Result<()> {
    let owner = Owner { id: id.to_string(), pid: std::process::id(), at: epoch() };
    let json = serde_json::to_vec(&owner).map_err(std::io::Error::other)?;
    crate::fsutil::atomic_write(&path_in(dir), &json)
}

/// Give the recording back to the recorder (only this process's own claim).
pub fn release(dir: &Path, id: &str) {
    if let Some(o) = read(dir) {
        if o.id == id && o.pid == std::process::id() {
            let _ = std::fs::remove_file(path_in(dir));
        }
    }
}

/// The file is replaced atomically, so a read can fail for an instant: retry.
fn read(dir: &Path) -> Option<Owner> {
    for _ in 0..3 {
        if let Some(o) = std::fs::read(path_in(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            return Some(o);
        }
        if !path_in(dir).exists() {
            return None;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    None
}

/// The app has a fresh claim on schedule `id`.
pub fn owned_by_app(dir: &Path, id: &str) -> bool {
    owned_at(dir, id, epoch())
}

fn owned_at(dir: &Path, id: &str, now: u64) -> bool {
    read(dir).is_some_and(|o| o.id == id && now.saturating_sub(o.at) < FRESH_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_claim_for_the_same_schedule_counts() {
        let d = tempfile::tempdir().unwrap();
        assert!(!owned_by_app(d.path(), "s1"));
        claim(d.path(), "s1").unwrap();
        assert!(owned_by_app(d.path(), "s1"));
        assert!(!owned_by_app(d.path(), "s2"), "another schedule's claim is not ours");
    }

    #[test]
    fn a_stale_claim_is_dead() {
        let d = tempfile::tempdir().unwrap();
        claim(d.path(), "s1").unwrap();
        assert!(owned_at(d.path(), "s1", epoch() + FRESH_SECS - 1));
        assert!(!owned_at(d.path(), "s1", epoch() + FRESH_SECS + 1));
    }

    #[test]
    fn release_removes_only_our_own_claim() {
        let d = tempfile::tempdir().unwrap();
        claim(d.path(), "s1").unwrap();
        release(d.path(), "s2");
        assert!(owned_by_app(d.path(), "s1"));
        release(d.path(), "s1");
        assert!(!owned_by_app(d.path(), "s1"));
        assert!(!path_in(d.path()).exists());
        // Another process's claim stays.
        let other = Owner { id: "s1".into(), pid: std::process::id() + 1, at: epoch() };
        std::fs::write(path_in(d.path()), serde_json::to_vec(&other).unwrap()).unwrap();
        release(d.path(), "s1");
        assert!(owned_by_app(d.path(), "s1"));
    }

    #[test]
    fn garbage_is_not_a_claim() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(path_in(d.path()), b"{oops").unwrap();
        assert!(!owned_by_app(d.path(), "s1"));
    }
}
