//! Startup / library / refresh state machine. Pure: the GUI performs the
//! returned [`Action`]s (background threads) and feeds results back in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use iptv::fetch::{is_shrink_refusal, RefreshEvent};
use iptv::library::Library;

/// A cache older than this triggers a background refresh at startup.
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether a playlist source (config URL or `--playlist-file`) is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    Available,
    /// No usable `config.toml`: `config_path` is where it is expected,
    /// `detail` the (URL-free) reason.
    Missing { config_path: PathBuf, detail: String },
}

/// What the main area shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Reading the cache, or (no cache) waiting for the first download.
    Loading,
    /// A library is available.
    Ready,
    /// No cache and no config: show setup instructions.
    NoConfig { config_path: PathBuf, detail: String },
    /// No cache and the first refresh failed.
    RefreshFailed { msg: String, can_force: bool },
}

/// Background refresh status (status line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshState {
    Idle,
    Running { bytes: u64, forced: bool },
    /// Last refresh failed (old data kept). `can_force`: the failure was the
    /// "much smaller playlist" safety refusal, so "Force refresh" can help.
    Failed { msg: String, can_force: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Load the cache + build the library on a background thread, then call
    /// [`DataState::on_cache_loaded`].
    LoadCache,
    /// Spawn a `Refresher` (forced = skip the shrink check) and feed its
    /// events to [`DataState::on_refresh_event`].
    StartRefresh { force: bool },
}

pub struct DataState {
    source: SourceStatus,
    phase: Phase,
    refresh: RefreshState,
    library: Option<Arc<Library>>,
    warning: Option<String>,
    /// A refresh was started this session (user or automatic): the
    /// stale-cache refresh must not start a second one.
    refresh_requested: bool,
}

/// Kinds and counts only: messages and the library never reach Debug output.
impl std::fmt::Debug for DataState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match self.source {
            SourceStatus::Available => "Available",
            SourceStatus::Missing { .. } => "Missing",
        };
        let phase = match self.phase {
            Phase::Loading => "Loading",
            Phase::Ready => "Ready",
            Phase::NoConfig { .. } => "NoConfig",
            Phase::RefreshFailed { .. } => "RefreshFailed",
        };
        let refresh = match self.refresh {
            RefreshState::Idle => "Idle",
            RefreshState::Running { .. } => "Running",
            RefreshState::Failed { .. } => "Failed",
        };
        f.debug_struct("DataState")
            .field("source", &source)
            .field("phase", &phase)
            .field("refresh", &refresh)
            .field("library_entries", &self.library.as_ref().map(|l| l.entry_count()))
            .field("warning", &self.warning.is_some())
            .field("refresh_requested", &self.refresh_requested)
            .finish()
    }
}

impl DataState {
    /// Initial state: loading the cache.
    pub fn new(source: SourceStatus) -> (DataState, Vec<Action>) {
        let s = DataState { source, phase: Phase::Loading, refresh: RefreshState::Idle, library: None, warning: None, refresh_requested: false };
        (s, vec![Action::LoadCache])
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn refresh(&self) -> &RefreshState {
        &self.refresh
    }

    pub fn library(&self) -> Option<&Arc<Library>> {
        self.library.as_ref()
    }

    /// Latest non-fatal refresh warning (already redacted by `fetch`).
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn dismiss_warning(&mut self) {
        self.warning = None;
    }

    /// Re-checked source (e.g. config.toml created while the app runs).
    pub fn set_source(&mut self, source: SourceStatus) {
        self.source = source;
    }

    pub fn source(&self) -> &SourceStatus {
        &self.source
    }

    /// Result of [`Action::LoadCache`]. Returns follow-up actions.
    pub fn on_cache_loaded(&mut self, cached: Option<(Arc<Library>, SystemTime)>, now: SystemTime) -> Vec<Action> {
        let stale = match &cached {
            Some((lib, saved_at)) => {
                // A refresh that finished first already provided newer data.
                if self.library.is_none() {
                    self.library = Some(lib.clone());
                    self.phase = Phase::Ready;
                }
                now.duration_since(*saved_at).map(|age| age > MAX_CACHE_AGE).unwrap_or(true)
            }
            None => true,
        };
        // A refresh already requested this session (still running, done or
        // failed) supersedes the stale-cache one.
        if !stale || self.refresh_requested {
            return vec![];
        }
        match &self.source {
            SourceStatus::Available => self.start(false),
            SourceStatus::Missing { config_path, detail } => {
                if self.library.is_none() {
                    self.phase = Phase::NoConfig { config_path: config_path.clone(), detail: detail.clone() };
                }
                vec![]
            }
        }
    }

    /// Feed a refresher event. Returns true when the library was replaced.
    pub fn on_refresh_event(&mut self, ev: RefreshEvent) -> bool {
        match ev {
            RefreshEvent::Progress(n) => {
                if let RefreshState::Running { bytes, .. } = &mut self.refresh {
                    *bytes = n;
                }
                false
            }
            RefreshEvent::Warning(m) => {
                self.warning = Some(m);
                false
            }
            RefreshEvent::Done(lib) => {
                self.library = Some(lib);
                self.phase = Phase::Ready;
                self.refresh = RefreshState::Idle;
                true
            }
            RefreshEvent::Failed(msg) => {
                let can_force = is_shrink_refusal(&msg);
                if self.library.is_none() {
                    self.phase = Phase::RefreshFailed { msg: msg.clone(), can_force };
                }
                self.refresh = RefreshState::Failed { msg, can_force };
                false
            }
        }
    }

    /// User asked for a refresh (button/F5, or "Force refresh").
    pub fn request_refresh(&mut self, force: bool) -> Vec<Action> {
        if matches!(self.refresh, RefreshState::Running { .. }) {
            return vec![];
        }
        match &self.source {
            SourceStatus::Available => self.start(force),
            SourceStatus::Missing { config_path, detail } => {
                let msg = format!("Cannot refresh: {detail}. Create {} with playlist_url.", config_path.display());
                self.refresh = RefreshState::Failed { msg, can_force: false };
                vec![]
            }
        }
    }

    fn start(&mut self, force: bool) -> Vec<Action> {
        self.refresh_requested = true;
        self.refresh = RefreshState::Running { bytes: 0, forced: force };
        if self.library.is_none() {
            self.phase = Phase::Loading;
        }
        vec![Action::StartRefresh { force }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::live::tests::sample_lib;

    #[test]
    fn debug_hides_urls() {
        let (mut s, _) = avail();
        s.on_cache_loaded(Some((Arc::new(sample_lib()), SystemTime::now())), SystemTime::now());
        s.set_source(SourceStatus::Missing {
            config_path: "C:/x/config.toml".into(),
            detail: "bad http://host.invalid/USER/PASS/1".into(),
        });
        let d = format!("{s:?}");
        assert!(s.library().is_some());
        assert!(!d.contains("://") && !d.contains(".invalid") && !d.contains("USER") && !d.contains("PASS"), "{d}");
    }

    fn avail() -> (DataState, Vec<Action>) {
        DataState::new(SourceStatus::Available)
    }

    fn missing() -> SourceStatus {
        SourceStatus::Missing { config_path: PathBuf::from("C:/x/config.toml"), detail: "cannot read".into() }
    }

    fn lib() -> Arc<Library> {
        Arc::new(sample_lib())
    }

    #[test]
    fn starts_loading_the_cache() {
        let (s, a) = avail();
        assert_eq!(s.phase(), &Phase::Loading);
        assert_eq!(a, vec![Action::LoadCache]);
        assert!(s.library().is_none());
    }

    #[test]
    fn fresh_cache_is_ready_without_refresh() {
        let (mut s, _) = avail();
        let now = SystemTime::now();
        let a = s.on_cache_loaded(Some((lib(), now - Duration::from_secs(3600))), now);
        assert!(a.is_empty());
        assert_eq!(s.phase(), &Phase::Ready);
        assert_eq!(s.refresh(), &RefreshState::Idle);
        assert!(s.library().is_some());
    }

    #[test]
    fn stale_cache_is_ready_and_refreshes_in_background() {
        let (mut s, _) = avail();
        let now = SystemTime::now();
        let a = s.on_cache_loaded(Some((lib(), now - MAX_CACHE_AGE - Duration::from_secs(1))), now);
        assert_eq!(a, vec![Action::StartRefresh { force: false }]);
        assert_eq!(s.phase(), &Phase::Ready);
        assert_eq!(s.refresh(), &RefreshState::Running { bytes: 0, forced: false });
    }

    #[test]
    fn no_cache_downloads_then_ready() {
        let (mut s, _) = avail();
        let a = s.on_cache_loaded(None, SystemTime::now());
        assert_eq!(a, vec![Action::StartRefresh { force: false }]);
        assert_eq!(s.phase(), &Phase::Loading);
        assert!(!s.on_refresh_event(RefreshEvent::Progress(1024)));
        assert_eq!(s.refresh(), &RefreshState::Running { bytes: 1024, forced: false });
        assert!(s.on_refresh_event(RefreshEvent::Done(lib())));
        assert_eq!(s.phase(), &Phase::Ready);
        assert_eq!(s.refresh(), &RefreshState::Idle);
        assert!(s.library().is_some());
    }

    #[test]
    fn no_cache_and_no_config_shows_instructions() {
        let (mut s, _) = DataState::new(missing());
        let a = s.on_cache_loaded(None, SystemTime::now());
        assert!(a.is_empty());
        let Phase::NoConfig { config_path, .. } = s.phase() else { panic!("{:?}", s.phase()) };
        assert!(config_path.ends_with("config.toml"));
        // Refresh cannot start without a source.
        assert!(s.request_refresh(false).is_empty());
    }

    #[test]
    fn cache_without_config_is_ready_but_refresh_reports_missing_config() {
        let (mut s, _) = DataState::new(missing());
        let now = SystemTime::now();
        // Even stale: no source, so no refresh.
        assert!(s.on_cache_loaded(Some((lib(), now - MAX_CACHE_AGE * 2)), now).is_empty());
        assert_eq!(s.phase(), &Phase::Ready);
        assert!(s.request_refresh(false).is_empty());
        let RefreshState::Failed { msg, can_force } = s.refresh() else { panic!("{:?}", s.refresh()) };
        assert!(msg.contains("config.toml") && !can_force, "{msg}");
    }

    #[test]
    fn user_refresh_during_cache_load_suppresses_the_stale_refresh() {
        let now = SystemTime::now();
        let stale = now - MAX_CACHE_AGE * 2;
        // F5 while the cache is still loading, refresh finishes first.
        let (mut s, _) = avail();
        assert_eq!(s.request_refresh(false), vec![Action::StartRefresh { force: false }]);
        let fresh = lib();
        assert!(s.on_refresh_event(RefreshEvent::Done(fresh.clone())));
        assert!(s.on_cache_loaded(Some((lib(), stale)), now).is_empty(), "no second refresh");
        assert!(Arc::ptr_eq(s.library().unwrap(), &fresh), "older cache never replaces refreshed data");
        // F5 while loading, refresh fails before the stale cache arrives:
        // the user already saw that attempt fail; no automatic second one.
        let (mut s, _) = avail();
        s.request_refresh(false);
        s.on_refresh_event(RefreshEvent::Failed("server returned HTTP 500".into()));
        assert!(s.on_cache_loaded(Some((lib(), stale)), now).is_empty());
        assert_eq!(s.phase(), &Phase::Ready, "cached data still shown");
        assert!(matches!(s.refresh(), RefreshState::Failed { .. }), "failure stays visible");
    }

    #[test]
    fn config_added_later_enables_refresh() {
        let (mut s, _) = DataState::new(missing());
        s.on_cache_loaded(None, SystemTime::now());
        s.set_source(SourceStatus::Available);
        assert_eq!(s.request_refresh(false), vec![Action::StartRefresh { force: false }]);
        assert_eq!(s.phase(), &Phase::Loading);
    }

    #[test]
    fn first_refresh_failure_without_cache_is_refresh_failed() {
        let (mut s, _) = avail();
        s.on_cache_loaded(None, SystemTime::now());
        assert!(!s.on_refresh_event(RefreshEvent::Failed("server returned HTTP 500".into())));
        assert_eq!(
            s.phase(),
            &Phase::RefreshFailed { msg: "server returned HTTP 500".into(), can_force: false }
        );
        // Retry from the failed screen.
        assert_eq!(s.request_refresh(false), vec![Action::StartRefresh { force: false }]);
        assert_eq!(s.phase(), &Phase::Loading);
    }

    #[test]
    fn refresh_failure_with_library_keeps_data_and_offers_force() {
        let (mut s, _) = avail();
        let now = SystemTime::now();
        s.on_cache_loaded(Some((lib(), now)), now);
        assert_eq!(s.request_refresh(false), vec![Action::StartRefresh { force: false }]);
        assert!(s.request_refresh(false).is_empty(), "no second refresh while running");
        let msg = "downloaded playlist is much smaller than the cached one (2 vs 10 entries); keeping the old data";
        s.on_refresh_event(RefreshEvent::Failed(msg.into()));
        assert_eq!(s.phase(), &Phase::Ready);
        assert_eq!(s.refresh(), &RefreshState::Failed { msg: msg.into(), can_force: true });
        assert_eq!(s.request_refresh(true), vec![Action::StartRefresh { force: true }]);
        assert_eq!(s.refresh(), &RefreshState::Running { bytes: 0, forced: true });
    }

    #[test]
    fn warning_is_non_fatal() {
        let (mut s, _) = avail();
        s.on_cache_loaded(None, SystemTime::now());
        s.on_refresh_event(RefreshEvent::Warning("could not save playlist cache: disk full".into()));
        assert_eq!(s.warning(), Some("could not save playlist cache: disk full"));
        assert!(matches!(s.refresh(), RefreshState::Running { .. }));
        assert!(s.on_refresh_event(RefreshEvent::Done(lib())));
        assert_eq!(s.phase(), &Phase::Ready);
        assert!(s.warning().is_some(), "warning stays visible until dismissed");
        s.dismiss_warning();
        assert!(s.warning().is_none());
    }
}
