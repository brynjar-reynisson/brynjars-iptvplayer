//! Retry supervisor: a pure state machine that keeps a stream playing until the
//! user picks something else.
//!
//! It knows nothing about mpv or the clock: the caller passes `now` into every
//! call, and playback is driven through the [`Backend`] trait. The UI thread
//! calls [`Supervisor::tick`] every frame and forwards player events through
//! [`Supervisor::on_event`].
//!
//! # Generations
//! Every `Backend::load` gets a fresh generation number (wrapping `u32`): user
//! `play()` calls *and* automatic retries. The backend tags each event it emits
//! with the generation of the load that caused it, and events whose generation
//! is not the current one are dropped. So late events from a previous channel,
//! or from a failed retry attempt, can never cause another retry. `stop()` also
//! moves to a new generation so nothing from the stopped load is acted on.
//!
//! Within one generation, the first failure (Error, Ended-as-drop, stall)
//! marks the generation dead; every later event of that generation is
//! ignored, so duplicate errors or Error-then-Ended schedule exactly one retry.
//!
//! # Backoff
//! Network/other failures: 0.5 s, 1 s, 2 s, then 3 s forever.
//! Auth failures (401/403/458/459, "too many connections", "unauthorized"):
//! 3 s, 6 s, 12 s, then 15 s forever — never faster than the network cap.
//! Unavailable channels (404/407/410, ffmpeg's generic "4XX Client Error";
//! e.g. an off-air event channel answering 407): 3 s, 6 s, then 10 s
//! forever, so the channel is picked up when it goes live without hammering
//! the provider.
//! The attempt counter (saturating) resets after `Started` followed by
//! [`STABLE_RESET`] of uninterrupted playing (no buffering). One counter
//! serves all tables, so network failures followed by an auth failure jump
//! straight to the auth delay for that attempt number.
//!
//! # Watchdogs
//! Before `Started`: a growing start timeout ([`START_TIMEOUT_BASE`] for VOD,
//! [`START_TIMEOUT_BASE_LIVE`] for live, + step per failed attempt, capped at
//! [`START_TIMEOUT_MAX`]), extended by progress events but never beyond
//! [`START_ABSOLUTE_MAX`] after the load. After `Started`: `Buffering(true)`
//! longer than [`STALL_TIMEOUT`] (armed at `Started` if buffering was already
//! in progress).
//! Pause and user seeks must not be reported as `Buffering(true)`; a paused
//! stream stays `Playing` (Task 10 contract).
//!
//! # Credentials
//! The URL is stored only to reload it; it is never logged and the manual
//! `Debug` impl does not print it.

use std::fmt;
use std::time::{Duration, Instant};

/// The player the supervisor drives (libmpv in the app, a fake in tests).
pub trait Backend {
    /// Load `url`, optionally starting at `start_at` seconds. Every event the
    /// backend later emits for this load must carry `generation`.
    fn load(&mut self, url: &str, start_at: Option<f64>, generation: u32);
    fn stop(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Network,
    /// Account rejected or connection limit reached.
    Auth,
    /// The server answered but has nothing to play right now (HTTP 404/407/
    /// 410 or another unlisted 4xx): typically an off-air event channel.
    Unavailable,
    Other,
}

impl ErrorClass {
    /// Classify a player/HTTP error message (case-insensitive).
    ///
    /// Auth (checked first) is only the whole-token statuses 401/403/458/459
    /// and account / connection-limit wordings. Unavailable is 404/407/410,
    /// "not found" (except host lookups) and ffmpeg's catch-all "Server
    /// returned 4XX Client Error, but not one of 40{0,1,3,4}" (how lavf words
    /// e.g. a 407; mpv's libcurl backend reports the code itself). Timeouts,
    /// refused connections, DNS, TLS and 5xx failures are Network.
    pub fn from_message(msg: &str) -> ErrorClass {
        let m = msg.to_ascii_lowercase();
        const AUTH_CODES: [&str; 4] = ["401", "403", "458", "459"];
        const AUTH_WORDS: [&str; 12] = [
            "too many connections",
            "too many open",
            "too many streams",
            "max connections",
            "maximum connections",
            "connection limit",
            "simultaneous",
            "unauthorized",
            "unauthorised",
            "forbidden",
            "authorization failed",
            "authorisation failed",
        ];
        if AUTH_CODES.iter().any(|c| contains_code(&m, c))
            || AUTH_WORDS.iter().any(|w| m.contains(w))
        {
            return ErrorClass::Auth;
        }
        const UNAVAILABLE_CODES: [&str; 3] = ["404", "407", "410"];
        if UNAVAILABLE_CODES.iter().any(|c| contains_code(&m, c))
            || m.contains("4xx client error")
            || m.contains("proxy authentication required")
            || (m.contains("not found") && !m.contains("host"))
        {
            return ErrorClass::Unavailable;
        }
        const NET_WORDS: [&str; 18] = [
            "http", "network", "connection", "connect", "timed out", "timeout", "refused",
            "reset", "unreachable", "resolve", "dns", "tls", "ssl", "socket", "i/o error",
            "not found", "server returned", "unknown host",
        ];
        if NET_WORDS.iter().any(|w| m.contains(w)) {
            return ErrorClass::Network;
        }
        ErrorClass::Other
    }
}

/// Why the supervisor is reconnecting (picks the backoff table and the
/// message shown to the user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// Network/other errors, drops, stalls and start timeouts.
    Network,
    /// Account rejected or connection limit reached.
    Auth,
    /// Channel unavailable (off-air?).
    Unavailable,
}

impl From<ErrorClass> for RetryKind {
    fn from(c: ErrorClass) -> RetryKind {
        match c {
            ErrorClass::Auth => RetryKind::Auth,
            ErrorClass::Unavailable => RetryKind::Unavailable,
            ErrorClass::Network | ErrorClass::Other => RetryKind::Network,
        }
    }
}

/// True if `code` occurs in `s` not adjacent to other ASCII digits.
fn contains_code(s: &str, code: &str) -> bool {
    let b = s.as_bytes();
    s.match_indices(code).any(|(i, _)| {
        let before = i == 0 || !b[i - 1].is_ascii_digit();
        let end = i + code.len();
        let after = end >= b.len() || !b[end].is_ascii_digit();
        before && after
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    Started,
    Position(f64),
    Duration(f64),
    Buffering(bool),
    Ended,
    Error(ErrorClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Playing,
    /// Loading (before `Started`) or buffering mid-play.
    Buffering,
    /// A failure happened; a retry is scheduled or in flight. `attempt` is the
    /// number of consecutive failures; `kind` is what the latest one looked
    /// like (network drop, account/connection-limit rejection, or an
    /// unavailable/off-air channel).
    Reconnecting { attempt: u32, kind: RetryKind },
    /// VOD reached its natural end. No autoplay, no retry.
    Finished,
}

/// Network/other backoff delays in ms; the last entry is the cap.
pub const NETWORK_BACKOFF_MS: [u64; 4] = [500, 1_000, 2_000, 3_000];
/// Auth backoff delays in ms; the last entry is the cap (15 s).
pub const AUTH_BACKOFF_MS: [u64; 4] = [3_000, 6_000, 12_000, 15_000];
/// Unavailable-channel backoff delays in ms; the last entry is the cap (10 s).
pub const UNAVAILABLE_BACKOFF_MS: [u64; 3] = [3_000, 6_000, 10_000];
/// Uninterrupted playing after `Started` needed to reset the backoff.
pub const STABLE_RESET: Duration = Duration::from_secs(10);
/// Stall watchdog: `Buffering(true)` *after* `Started` lasting longer than
/// this (strictly) is treated as a drop. Applies to live and VOD.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Pre-`Started` watchdog: a load gets `START_TIMEOUT_BASE` (VOD) or
/// `START_TIMEOUT_BASE_LIVE` (live) + `START_TIMEOUT_STEP` per consecutive
/// failed attempt, capped at `START_TIMEOUT_MAX`, to reach `Started`
/// (strictly-greater comparison).
/// The deadline grows so a slow open (large file on a slow link, resume load
/// needing a long seek) cannot be reloaded forever, and any current-generation
/// progress event before `Started` (`Position`, `Duration`, `Buffering(false)`)
/// pushes the deadline back, since progress proves the load is alive, but
/// never past `START_ABSOLUTE_MAX` after the load began: a load that keeps
/// emitting progress without ever reaching `Started` is still failed.
pub const START_TIMEOUT_BASE: Duration = Duration::from_secs(30);
/// Shorter base for live: a dead live stream should be retried quickly.
pub const START_TIMEOUT_BASE_LIVE: Duration = Duration::from_secs(15);
/// Absolute cap from load to `Started`, regardless of progress events.
pub const START_ABSOLUTE_MAX: Duration = Duration::from_secs(180);
pub const START_TIMEOUT_STEP: Duration = Duration::from_secs(15);
pub const START_TIMEOUT_MAX: Duration = Duration::from_secs(120);

fn start_timeout(attempt: u32, live: bool) -> Duration {
    let base = if live { START_TIMEOUT_BASE_LIVE } else { START_TIMEOUT_BASE };
    START_TIMEOUT_STEP
        .checked_mul(attempt)
        .and_then(|d| d.checked_add(base))
        .map_or(START_TIMEOUT_MAX, |d| d.min(START_TIMEOUT_MAX))
}
/// VOD `Ended` within this many seconds of the duration is a natural finish.
pub const VOD_END_TOLERANCE_SECS: f64 = 5.0;

/// One attempt counter serves all tables: the delay comes from the table of
/// the *latest* failure's kind indexed by the shared count, so e.g. three
/// network failures followed by an auth failure wait the 4th auth delay (15 s).
fn backoff(attempt: u32, kind: RetryKind) -> Duration {
    let table: &[u64] = match kind {
        RetryKind::Network => &NETWORK_BACKOFF_MS,
        RetryKind::Auth => &AUTH_BACKOFF_MS,
        RetryKind::Unavailable => &UNAVAILABLE_BACKOFF_MS,
    };
    let i = (attempt.saturating_sub(1) as usize).min(table.len() - 1);
    Duration::from_millis(table[i])
}

pub struct Supervisor<B: Backend> {
    backend: B,
    /// Only stored to reload it. Never logged, never in `Debug`.
    url: Option<String>,
    is_live: bool,
    generation: u32,
    status: Status,
    /// Consecutive failures since the last stable period / `play()`.
    attempt: u32,
    /// True once the current generation has failed/finished (or nothing is
    /// loaded); all further events of that generation are ignored.
    dead: bool,
    /// `Started` was received for the current generation.
    started: bool,
    retry_at: Option<Instant>,
    /// Last known playback position (VOD resume point); survives retries.
    position: Option<f64>,
    duration: Option<f64>,
    /// When post-`Started` buffering began; drives the stall watchdog.
    stall_since: Option<Instant>,
    /// Pre-`Started` watchdog deadline for the current load.
    start_deadline: Option<Instant>,
    /// When the current load was issued (absolute pre-`Started` cap).
    load_started_at: Option<Instant>,
    /// Last `Buffering` value seen for the current generation.
    buffering: bool,
    /// When uninterrupted playing began; drives the backoff reset.
    stable_since: Option<Instant>,
}

impl<B: Backend> fmt::Debug for Supervisor<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor")
            .field("status", &self.status)
            .field("generation", &self.generation)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> Supervisor<B> {
    pub fn new(backend: B) -> Self {
        Supervisor {
            backend,
            url: None,
            is_live: false,
            generation: 0,
            status: Status::Idle,
            attempt: 0,
            dead: true,
            started: false,
            retry_at: None,
            position: None,
            duration: None,
            stall_since: None,
            start_deadline: None,
            load_started_at: None,
            buffering: false,
            stable_since: None,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Start playing `url` (user action). Cancels any pending retry, resets
    /// backoff and resume position, and loads under a new generation.
    pub fn play(&mut self, url: String, is_live: bool, now: Instant) {
        self.url = Some(url);
        self.is_live = is_live;
        self.attempt = 0;
        self.retry_at = None;
        self.position = None;
        self.duration = None;
        self.status = Status::Buffering;
        self.load(None, now);
    }

    /// Feed a player event tagged with the generation of the load it came from.
    pub fn on_event(&mut self, generation: u32, ev: PlayerEvent, now: Instant) {
        if generation != self.generation || self.dead {
            return;
        }
        match ev {
            PlayerEvent::Started => {
                self.started = true;
                self.start_deadline = None;
                self.load_started_at = None;
                if self.buffering {
                    // Buffering began before Started and is still going:
                    // arm the stall watchdog now.
                    self.stall_since = Some(now);
                    self.stable_since = None;
                    self.status = Status::Buffering;
                } else {
                    self.stall_since = None;
                    self.stable_since = Some(now);
                    self.status = Status::Playing;
                }
            }
            PlayerEvent::Position(p) => {
                if p.is_finite() {
                    self.position = Some(p);
                }
                self.note_progress(now);
                self.check_stable(now);
            }
            PlayerEvent::Duration(d) => {
                if d.is_finite() && d > 0.0 {
                    self.duration = Some(d);
                }
                self.note_progress(now);
            }
            // Task 10 contract: only network/cache buffering is reported as
            // Buffering(true). A user pause or user seek must NOT be reported
            // (a paused stream stays Playing), or a pause > STALL_TIMEOUT
            // would trigger a reconnect.
            PlayerEvent::Buffering(true) => {
                self.buffering = true;
                // Before Started, the start deadline governs instead.
                if self.started {
                    self.stall_since.get_or_insert(now);
                    self.stable_since = None;
                    self.status = Status::Buffering;
                }
            }
            PlayerEvent::Buffering(false) => {
                self.buffering = false;
                if self.started {
                    self.stall_since = None;
                    self.stable_since = Some(now);
                    self.status = Status::Playing;
                } else {
                    self.note_progress(now);
                }
            }
            PlayerEvent::Ended => {
                if !self.is_live && self.natural_end() {
                    self.dead = true;
                    self.status = Status::Finished;
                    self.stall_since = None;
                    self.start_deadline = None;
                    self.stable_since = None;
                } else {
                    self.fail(RetryKind::Network, now);
                }
            }
            PlayerEvent::Error(class) => self.fail(class.into(), now),
        }
    }

    /// Drive timers: fire a due retry, run the stall watchdog, reset backoff
    /// after a stable period. Call every frame.
    pub fn tick(&mut self, now: Instant) {
        if let Some(at) = self.retry_at {
            if now >= at {
                self.retry_at = None;
                let start_at = if self.is_live { None } else { self.position };
                self.load(start_at, now);
            }
            return;
        }
        if self.dead {
            return;
        }
        if let Some(deadline) = self.start_deadline {
            let hard = self.load_started_at.map(|t| t + START_ABSOLUTE_MAX);
            if now > deadline || hard.is_some_and(|h| now > h) {
                self.fail(RetryKind::Network, now);
                return;
            }
        }
        if let Some(since) = self.stall_since {
            if now.saturating_duration_since(since) > STALL_TIMEOUT {
                self.fail(RetryKind::Network, now);
                return;
            }
        }
        self.check_stable(now);
    }

    /// User stop: cancel any pending retry, stop the backend, go Idle.
    pub fn stop(&mut self) {
        self.retry_at = None;
        self.url = None;
        self.dead = true;
        self.started = false;
        self.attempt = 0;
        self.position = None;
        self.duration = None;
        self.stall_since = None;
        self.start_deadline = None;
        self.stable_since = None;
        self.generation = self.generation.wrapping_add(1);
        self.status = Status::Idle;
        self.backend.stop();
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Consecutive failures since the last reset.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Time until the scheduled retry fires (for "retrying in Ns"); `None` if
    /// no retry is scheduled (including while a retry load is in flight).
    pub fn next_retry_in(&self, now: Instant) -> Option<Duration> {
        self.retry_at.map(|at| at.saturating_duration_since(now))
    }

    fn load(&mut self, start_at: Option<f64>, now: Instant) {
        self.generation = self.generation.wrapping_add(1);
        self.dead = false;
        self.started = false;
        self.stable_since = None;
        self.stall_since = None;
        self.start_deadline = Some(now + start_timeout(self.attempt, self.is_live));
        self.load_started_at = Some(now);
        self.buffering = false;
        if let Some(url) = self.url.as_deref() {
            self.backend.load(url, start_at, self.generation);
        }
    }

    /// Progress before `Started` proves the load is alive: push the
    /// pre-start deadline back (never earlier than it already is).
    fn note_progress(&mut self, now: Instant) {
        if let Some(old) = self.start_deadline {
            self.start_deadline = Some(old.max(now + start_timeout(self.attempt, self.is_live)));
        }
    }

    fn natural_end(&self) -> bool {
        match (self.position, self.duration) {
            (Some(p), Some(d)) => p >= d - VOD_END_TOLERANCE_SECS,
            _ => false,
        }
    }

    /// The current generation failed: schedule exactly one retry.
    fn fail(&mut self, kind: RetryKind, now: Instant) {
        self.dead = true;
        self.started = false;
        self.stall_since = None;
        self.start_deadline = None;
        self.stable_since = None;
        self.attempt = self.attempt.saturating_add(1);
        self.retry_at = Some(now + backoff(self.attempt, kind));
        self.status = Status::Reconnecting { attempt: self.attempt, kind };
    }

    fn check_stable(&mut self, now: Instant) {
        if self.status != Status::Playing {
            return;
        }
        if let Some(since) = self.stable_since {
            if now.saturating_duration_since(since) >= STABLE_RESET {
                self.attempt = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Load { url: String, start_at: Option<f64>, generation: u32 },
        Stop,
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Vec<Call>,
    }

    impl Backend for FakeBackend {
        fn load(&mut self, url: &str, start_at: Option<f64>, generation: u32) {
            self.calls.push(Call::Load { url: url.to_string(), start_at, generation });
        }
        fn stop(&mut self) {
            self.calls.push(Call::Stop);
        }
    }

    const URL_A: &str = "http://host/user/pass/1.ts";
    const URL_B: &str = "http://host/user/pass/2.ts";

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn sup() -> Supervisor<FakeBackend> {
        Supervisor::new(FakeBackend::default())
    }

    fn loads(s: &Supervisor<FakeBackend>) -> Vec<(String, Option<f64>, u32)> {
        s.backend()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Load { url, start_at, generation } => {
                    Some((url.clone(), *start_at, *generation))
                }
                _ => None,
            })
            .collect()
    }

    fn load_count(s: &Supervisor<FakeBackend>) -> usize {
        loads(s).len()
    }

    /// Fail the current generation with `ev`, return the delay until the retry
    /// fires, and fire it (advancing `now`).
    fn fail_and_measure(s: &mut Supervisor<FakeBackend>, now: &mut Instant, ev: PlayerEvent) -> Duration {
        let g = s.generation();
        s.on_event(g, ev, *now);
        let d = s.next_retry_in(*now).expect("retry should be scheduled");
        // Not yet due just before the deadline.
        let before = load_count(s);
        if d > ms(1) {
            s.tick(*now + d - ms(1));
            assert_eq!(load_count(s), before, "retry fired early");
        }
        *now += d;
        s.tick(*now);
        assert_eq!(load_count(s), before + 1, "retry did not fire when due");
        d
    }

    // ---------- ErrorClass ----------

    #[test]
    fn error_class_from_message() {
        assert_eq!(ErrorClass::from_message("HTTP error 401 Unauthorized"), ErrorClass::Auth);
        assert_eq!(ErrorClass::from_message("http error 403 Forbidden"), ErrorClass::Auth);
        assert_eq!(ErrorClass::from_message("Server returned 458"), ErrorClass::Auth);
        assert_eq!(ErrorClass::from_message("status 459"), ErrorClass::Auth);
        assert_eq!(ErrorClass::from_message("Too Many Connections"), ErrorClass::Auth);
        assert_eq!(ErrorClass::from_message("UNAUTHORIZED"), ErrorClass::Auth);
        assert_eq!(ErrorClass::from_message("HTTP error 404 Not Found"), ErrorClass::Unavailable);
        assert_eq!(ErrorClass::from_message("Connection refused"), ErrorClass::Network);
        assert_eq!(ErrorClass::from_message("network error"), ErrorClass::Network);
        assert_eq!(ErrorClass::from_message("operation timed out"), ErrorClass::Network);
        assert_eq!(ErrorClass::from_message("HTTP error 503 Service Unavailable"), ErrorClass::Network);
        assert_eq!(ErrorClass::from_message("unrecognized file format"), ErrorClass::Other);
        assert_eq!(ErrorClass::from_message(""), ErrorClass::Other);
        // Codes embedded in longer numbers are not auth codes.
        assert_eq!(ErrorClass::from_message("port 14010 refused"), ErrorClass::Network);
        assert_eq!(ErrorClass::from_message("id 94031"), ErrorClass::Other);
    }

    #[test]
    fn error_class_real_mpv_ffmpeg_wordings() {
        use ErrorClass::*;
        let cases = [
            ("HTTP error 401 Unauthorized", Auth),
            ("HTTP error 403 Forbidden", Auth),
            ("Server returned 4XX Client Error, but not one of 40{0,1,3,4}", Unavailable),
            ("server returned 4xx client error, but not one of 40{0,1,3,4}", Unavailable),
            ("Max connections reached", Auth),
            ("Maximum connections exceeded", Auth),
            ("connection limit reached", Auth),
            ("Too many connections", Auth),
            ("too many open streams", Auth),
            ("Simultaneous streams limit", Auth),
            ("Server returned 401 Unauthorized (authorization failed)", Auth),
            ("Server returned 403 Forbidden (access denied)", Auth),
            ("Server returned 404 Not Found", Unavailable),
            ("Not Found", Unavailable),
            ("Connection refused", Network),
            ("tls handshake failure", Network),
            ("Connection timed out", Network),
            ("Failed to resolve hostname host: Unknown host", Network),
            ("DNS lookup failed", Network),
        ];
        for (msg, want) in cases {
            assert_eq!(ErrorClass::from_message(msg), want, "{msg}");
        }
    }

    #[test]
    fn error_class_unavailable_channel() {
        use ErrorClass::*;
        // Off-air event channels answer HTTP 407 (or 404/410) instantly.
        let cases = [
            ("HTTP error 407", Unavailable),
            ("HTTP error 407 Proxy Authentication Required", Unavailable),
            ("http: HTTP error 407 | loading failed", Unavailable),
            ("The requested URL returned error: 407", Unavailable),
            ("Server returned 4XX Client Error, but not one of 40{0,1,3,4}", Unavailable),
            ("HTTP error 404 Not Found", Unavailable),
            ("HTTP error 410 Gone", Unavailable),
            ("Server returned 404 Not Found", Unavailable),
            // Real auth / connection-limit replies stay Auth.
            ("HTTP error 401", Auth),
            ("HTTP error 403", Auth),
            ("HTTP error 458", Auth),
            ("HTTP error 459", Auth),
            ("max connections reached", Auth),
            ("Maximum connections exceeded", Auth),
            ("too many connections", Auth),
            ("simultaneous streams", Auth),
            ("connection limit", Auth),
            ("unauthorized", Auth),
            ("forbidden", Auth),
            // Network stays network.
            ("HTTP error 503 Service Unavailable", Network),
            ("HTTP error 500", Network),
            ("Could not resolve host: Host not found", Network),
            ("Connection refused", Network),
            // Not a connection limit (was Auth via the old broad "too many").
            ("Too many redirects", Other),
            // Codes inside longer numbers are not statuses.
            ("id 14070", Other),
            ("port 4070 refused", Network),
        ];
        for (msg, want) in cases {
            assert_eq!(ErrorClass::from_message(msg), want, "{msg}");
        }
    }

    #[test]
    fn unavailable_backoff_is_endless_and_capped_at_10s() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let mut d = vec![];
        for _ in 0..50 {
            d.push(fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Unavailable)));
        }
        assert_eq!(&d[..5], &[ms(3000), ms(6000), ms(10000), ms(10000), ms(10000)]);
        assert!(d.iter().all(|x| *x >= ms(3000) && *x <= ms(10000)));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 50, kind: RetryKind::Unavailable });
        assert_eq!(load_count(&s), 51, "never gives up");
    }

    #[test]
    fn unavailable_resets_after_stable_play_and_on_play() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        for _ in 0..4 {
            fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Unavailable));
        }
        // The channel comes on air and plays stably: counter resets.
        s.on_event(s.generation(), PlayerEvent::Started, now);
        now += STABLE_RESET;
        s.tick(now);
        assert_eq!(s.attempt(), 0);
        let d = fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Unavailable));
        assert_eq!(d, ms(3000));
        // A user play() resets too.
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Unavailable));
        s.play(URL_B.to_string(), true, now);
        let d = fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Unavailable));
        assert_eq!(d, ms(3000));
    }

    #[test]
    fn retry_kind_from_error_class() {
        assert_eq!(RetryKind::from(ErrorClass::Auth), RetryKind::Auth);
        assert_eq!(RetryKind::from(ErrorClass::Unavailable), RetryKind::Unavailable);
        assert_eq!(RetryKind::from(ErrorClass::Network), RetryKind::Network);
        assert_eq!(RetryKind::from(ErrorClass::Other), RetryKind::Network);
    }

    // ---------- required tests ----------

    #[test]
    fn live_error_retries_with_backoff_sequence() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        assert_eq!(load_count(&s), 1);
        let mut delays = vec![];
        for _ in 0..6 {
            delays.push(fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network)));
        }
        assert_eq!(delays, vec![ms(500), ms(1000), ms(2000), ms(3000), ms(3000), ms(3000)]);
        // Every retry reloads the same URL, live => no start_at, new generation each time.
        let l = loads(&s);
        assert_eq!(l.len(), 7);
        for (i, (url, start, gen)) in l.iter().enumerate() {
            assert_eq!(url, URL_A);
            assert_eq!(*start, None);
            if i > 0 {
                assert_ne!(*gen, l[i - 1].2, "each load must be a new generation");
            }
        }
        assert_eq!(l.last().unwrap().2, s.generation());
    }

    #[test]
    fn backoff_caps_at_3s_network_15s_auth() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let mut net = vec![];
        for _ in 0..10 {
            net.push(fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network)));
        }
        assert!(net.iter().all(|d| *d <= ms(3000)));
        assert_eq!(*net.last().unwrap(), ms(3000));

        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let mut auth = vec![];
        for _ in 0..10 {
            auth.push(fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Auth)));
        }
        assert_eq!(&auth[..5], &[ms(3000), ms(6000), ms(12000), ms(15000), ms(15000)]);
        assert!(auth.iter().all(|d| *d >= ms(3000) && *d <= ms(15000)));
        assert_eq!(*auth.last().unwrap(), ms(15000));
    }

    #[test]
    fn auth_error_sets_auth_flag() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        s.on_event(s.generation(), PlayerEvent::Error(ErrorClass::Auth), now);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Auth });

        let mut s = sup();
        s.play(URL_A.to_string(), true, now);
        s.on_event(s.generation(), PlayerEvent::Error(ErrorClass::Network), now);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });

        let mut s = sup();
        s.play(URL_A.to_string(), true, now);
        s.on_event(s.generation(), PlayerEvent::Error(ErrorClass::Unavailable), now);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Unavailable });
    }

    #[test]
    fn stale_generation_events_ignored() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let gen_a = s.generation();
        s.play(URL_B.to_string(), true, now);
        let gen_b = s.generation();
        assert_ne!(gen_a, gen_b);
        s.on_event(gen_b, PlayerEvent::Started, now);
        s.on_event(gen_a, PlayerEvent::Error(ErrorClass::Network), now);
        s.on_event(gen_a, PlayerEvent::Ended, now);
        assert_eq!(s.status(), Status::Playing);
        assert_eq!(s.next_retry_in(now), None);
        s.tick(now + Duration::from_secs(60));
        let l = loads(&s);
        assert_eq!(l.len(), 2);
        assert!(l.iter().filter(|(u, _, _)| u == URL_A).count() == 1, "old channel must never be retried");
    }

    #[test]
    fn live_ended_reconnects() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        s.on_event(s.generation(), PlayerEvent::Started, now);
        s.on_event(s.generation(), PlayerEvent::Position(100.0), now);
        s.on_event(s.generation(), PlayerEvent::Duration(100.0), now);
        let d = fail_and_measure(&mut s, &mut now, PlayerEvent::Ended);
        assert_eq!(d, ms(500));
        assert_eq!(loads(&s).last().unwrap(), &(URL_A.to_string(), None, s.generation()));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
    }

    #[test]
    fn vod_end_near_duration_finishes() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Duration(3600.0), now);
        s.on_event(g, PlayerEvent::Position(3596.0), now);
        s.on_event(g, PlayerEvent::Ended, now);
        assert_eq!(s.status(), Status::Finished);
        assert_eq!(s.next_retry_in(now), None);
        s.tick(now + Duration::from_secs(60));
        assert_eq!(load_count(&s), 1);
        // Late events for the finished generation change nothing.
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), now);
        s.tick(now + Duration::from_secs(120));
        assert_eq!(s.status(), Status::Finished);
        assert_eq!(load_count(&s), 1);
    }

    #[test]
    fn vod_drop_resumes_at_last_position() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Duration(3600.0), now);
        s.on_event(g, PlayerEvent::Position(1234.5), now);
        fail_and_measure(&mut s, &mut now, PlayerEvent::Ended);
        assert_eq!(loads(&s).last().unwrap(), &(URL_A.to_string(), Some(1234.5), s.generation()));
        // Retry fails before any Position: still resume at the same spot.
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        assert_eq!(loads(&s).last().unwrap().1, Some(1234.5));
        // Retry plays further, then drops via Error: resume at the newer spot.
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Position(1300.0), now);
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        assert_eq!(loads(&s).last().unwrap().1, Some(1300.0));
    }

    #[test]
    fn vod_unknown_duration_ended_reconnects() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Position(50.0), now);
        fail_and_measure(&mut s, &mut now, PlayerEvent::Ended);
        assert_eq!(loads(&s).last().unwrap().1, Some(50.0));
    }

    #[test]
    fn backoff_resets_after_ten_seconds_stable() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        for _ in 0..4 {
            fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        }
        // Started, but only 9 s of playing: no reset.
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        assert_eq!(s.status(), Status::Playing);
        assert_eq!(s.attempt(), 4);
        s.on_event(g, PlayerEvent::Position(1.0), now + Duration::from_secs(9));
        assert_eq!(s.attempt(), 4);
        now += Duration::from_secs(9);
        let d = fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        assert_eq!(d, ms(3000));
        // Now 10 s stable (via Position events) => reset.
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        now += Duration::from_secs(10);
        s.on_event(g, PlayerEvent::Position(10.0), now);
        assert_eq!(s.attempt(), 0);
        let d = fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        assert_eq!(d, ms(500));
        // Reset also driven by tick alone.
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        now += Duration::from_secs(10);
        s.tick(now);
        assert_eq!(s.attempt(), 0);
    }

    #[test]
    fn buffering_interrupts_stable_period() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Buffering(true), now + Duration::from_secs(5));
        assert_eq!(s.status(), Status::Buffering);
        s.on_event(g, PlayerEvent::Buffering(false), now + Duration::from_secs(8));
        assert_eq!(s.status(), Status::Playing);
        s.tick(now + Duration::from_secs(12));
        assert_eq!(s.attempt(), 2, "stable clock restarts after buffering");
        s.tick(now + Duration::from_secs(18));
        assert_eq!(s.attempt(), 0);
    }

    #[test]
    fn retries_forever() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        for i in 1..=1000u32 {
            fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
            assert_eq!(s.status(), Status::Reconnecting { attempt: i, kind: RetryKind::Network });
        }
        assert_eq!(load_count(&s), 1001);
        assert!(matches!(s.status(), Status::Reconnecting { .. }));
    }

    #[test]
    fn stop_cancels_pending_retry() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), now);
        assert!(s.next_retry_in(now).is_some());
        s.stop();
        assert_eq!(s.status(), Status::Idle);
        assert_eq!(s.next_retry_in(now), None);
        assert_eq!(s.backend().calls.last(), Some(&Call::Stop));
        s.tick(now + Duration::from_secs(60));
        assert_eq!(load_count(&s), 1);
        // Late events from the stopped generation are ignored.
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), now);
        s.on_event(g, PlayerEvent::Started, now);
        s.tick(now + Duration::from_secs(120));
        assert_eq!(s.status(), Status::Idle);
        assert_eq!(load_count(&s), 1);
    }

    // ---------- extra behaviours ----------

    #[test]
    fn duplicate_errors_schedule_one_retry() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), now);
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), now + ms(100));
        s.on_event(g, PlayerEvent::Error(ErrorClass::Auth), now + ms(200));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        assert_eq!(s.next_retry_in(now), Some(ms(500)), "deadline not pushed back");
        s.tick(now + Duration::from_secs(30));
        assert_eq!(load_count(&s), 2);
        s.tick(now + Duration::from_secs(60));
        assert_eq!(load_count(&s), 2);
    }

    #[test]
    fn error_then_ended_same_generation_one_retry() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Duration(100.0), now);
        s.on_event(g, PlayerEvent::Position(99.0), now);
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), now);
        // The Ended that follows an Error must not be read as a natural finish.
        s.on_event(g, PlayerEvent::Ended, now);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        s.tick(now + Duration::from_secs(30));
        assert_eq!(load_count(&s), 2);
    }

    #[test]
    fn late_events_from_failed_attempt_do_not_double_retry() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g1 = s.generation();
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        let g2 = s.generation();
        assert_ne!(g1, g2);
        // A late Ended from attempt 1 arrives while attempt 2 is loading.
        s.on_event(g1, PlayerEvent::Ended, now);
        s.on_event(g1, PlayerEvent::Error(ErrorClass::Auth), now);
        assert_eq!(s.next_retry_in(now), None);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        s.tick(now + Duration::from_secs(5));
        assert_eq!(load_count(&s), 2);
        // Late Started from attempt 1 must not mark attempt 2 as playing.
        s.on_event(g1, PlayerEvent::Started, now);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
    }

    #[test]
    fn started_after_reconnecting_plays() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        assert_eq!(s.status(), Status::Buffering);
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        assert_eq!(s.next_retry_in(now), None, "retry in flight, nothing scheduled");
        s.on_event(s.generation(), PlayerEvent::Started, now);
        assert_eq!(s.status(), Status::Playing);
    }

    #[test]
    fn play_while_reconnecting_cancels_and_resets() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        for _ in 0..3 {
            fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Auth));
        }
        let old = s.generation();
        s.on_event(old, PlayerEvent::Error(ErrorClass::Auth), now);
        assert!(s.next_retry_in(now).is_some());
        s.play(URL_B.to_string(), true, now);
        assert_eq!(s.status(), Status::Buffering);
        assert_eq!(s.attempt(), 0);
        assert_eq!(s.next_retry_in(now), None);
        // Well past the old 15 s auth deadline, but inside the stall window.
        s.tick(now + Duration::from_secs(14));
        let l = loads(&s);
        assert_eq!(l.last().unwrap().0, URL_B);
        assert_eq!(l.iter().filter(|(u, _, _)| u == URL_B).count(), 1);
        // New channel fails: fresh network backoff, old auth state gone.
        s.on_event(s.generation(), PlayerEvent::Error(ErrorClass::Network), now);
        assert_eq!(s.next_retry_in(now), Some(ms(500)));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
    }

    #[test]
    fn play_vod_does_not_inherit_previous_position() {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        s.on_event(s.generation(), PlayerEvent::Position(500.0), now);
        s.play(URL_B.to_string(), false, now);
        fail_and_measure(&mut s, &mut now, PlayerEvent::Error(ErrorClass::Network));
        assert_eq!(loads(&s).last().unwrap(), &(URL_B.to_string(), None, s.generation()));
    }

    #[test]
    fn stall_watchdog_reconnects() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Buffering(true), now);
        // Repeated Buffering(true) does not restart the watchdog.
        s.on_event(g, PlayerEvent::Buffering(true), now + Duration::from_secs(10));
        s.tick(now + STALL_TIMEOUT);
        assert_eq!(s.status(), Status::Buffering);
        s.tick(now + STALL_TIMEOUT + ms(1));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        s.tick(now + STALL_TIMEOUT + ms(501));
        assert_eq!(load_count(&s), 2);
    }

    #[test]
    fn stall_watchdog_cleared_by_buffering_false() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Buffering(true), now);
        s.on_event(g, PlayerEvent::Buffering(false), now + Duration::from_secs(14));
        s.tick(now + Duration::from_secs(40));
        assert_eq!(s.status(), Status::Playing);
        assert_eq!(load_count(&s), 1);
    }

    #[test]
    fn start_watchdog_covers_load_that_never_starts() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        s.tick(now + START_TIMEOUT_BASE_LIVE);
        assert_eq!(s.status(), Status::Buffering);
        s.tick(now + START_TIMEOUT_BASE_LIVE + ms(1));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        // And a retry that hangs is also caught (with the grown timeout).
        let t = now + Duration::from_secs(16);
        s.tick(t);
        assert_eq!(load_count(&s), 2);
        s.tick(t + Duration::from_secs(30));
        assert!(matches!(s.status(), Status::Reconnecting { attempt: 1, .. }));
        s.tick(t + Duration::from_secs(30) + ms(1));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 2, kind: RetryKind::Network });
    }

    #[test]
    fn buffering_before_started_uses_start_timeout_not_stall() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        s.on_event(s.generation(), PlayerEvent::Buffering(true), now);
        s.tick(now + Duration::from_secs(20));
        assert_eq!(s.status(), Status::Buffering);
        assert_eq!(load_count(&s), 1);
    }

    #[test]
    fn slow_open_plays_on_second_attempt() {
        // An open that takes 40 s: the old 15 s watchdog reloaded it forever.
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        now += START_TIMEOUT_BASE + ms(1);
        s.tick(now);
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        now += ms(500);
        s.tick(now);
        assert_eq!(load_count(&s), 2);
        let g = s.generation();
        s.tick(now + Duration::from_secs(40));
        assert_eq!(load_count(&s), 2);
        s.on_event(g, PlayerEvent::Started, now + Duration::from_secs(40));
        assert_eq!(s.status(), Status::Playing);
        s.tick(now + Duration::from_secs(200));
        assert_eq!(s.status(), Status::Playing);
        assert_eq!(load_count(&s), 2);
    }

    #[test]
    fn progress_before_started_extends_start_deadline() {
        for ev in [PlayerEvent::Position(1.0), PlayerEvent::Duration(100.0), PlayerEvent::Buffering(false)] {
            let mut s = sup();
            let now = Instant::now();
            s.play(URL_A.to_string(), false, now);
            let g = s.generation();
            s.on_event(g, ev.clone(), now + Duration::from_secs(25));
            s.tick(now + Duration::from_secs(50));
            assert_eq!(s.status(), Status::Buffering, "{ev:?} should extend the deadline");
            s.tick(now + Duration::from_secs(55) + ms(1));
            assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network }, "{ev:?}");
        }
    }

    #[test]
    fn start_timeout_grows_per_attempt_and_caps() {
        assert_eq!(measure_start_timeouts(false), vec![30, 45, 60, 75, 90, 105, 120, 120, 120]);
    }

    #[test]
    fn live_start_timeout_shorter_base() {
        assert_eq!(measure_start_timeouts(true), vec![15, 30, 45, 60, 75, 90, 105, 120, 120]);
    }

    #[test]
    fn progress_cannot_extend_start_past_absolute_cap() {
        for live in [false, true] {
            let mut s = sup();
            let now = Instant::now();
            s.play(URL_A.to_string(), live, now);
            let g = s.generation();
            let mut failed_at = None;
            for i in 1..=60u64 {
                let t = now + Duration::from_secs(10 * i);
                s.on_event(g, PlayerEvent::Duration(100.0), t);
                s.tick(t);
                if failed_at.is_none() && matches!(s.status(), Status::Reconnecting { .. }) {
                    failed_at = Some(10 * i);
                }
            }
            let failed_at = failed_at.expect("load emitting progress but never Started must fail");
            assert!(failed_at > START_ABSOLUTE_MAX.as_secs() && failed_at <= START_ABSOLUTE_MAX.as_secs() + 10);
            assert!(load_count(&s) >= 2, "the reload must happen");
        }
    }

    #[test]
    fn buffering_in_progress_when_started_arms_stall() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Buffering(true), now);
        s.on_event(g, PlayerEvent::Started, now + Duration::from_secs(2));
        s.tick(now + Duration::from_secs(2) + STALL_TIMEOUT);
        assert!(!matches!(s.status(), Status::Reconnecting { .. }));
        s.tick(now + Duration::from_secs(2) + STALL_TIMEOUT + ms(1));
        assert_eq!(s.status(), Status::Reconnecting { attempt: 1, kind: RetryKind::Network });
        s.tick(now + Duration::from_secs(20));
        assert_eq!(load_count(&s), 2);
    }

    #[test]
    fn buffering_ended_before_started_does_not_arm_stall() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Buffering(true), now);
        s.on_event(g, PlayerEvent::Buffering(false), now + ms(500));
        s.on_event(g, PlayerEvent::Started, now + Duration::from_secs(1));
        s.tick(now + Duration::from_secs(60));
        assert_eq!(s.status(), Status::Playing);
        assert_eq!(load_count(&s), 1);
    }

    fn measure_start_timeouts(live: bool) -> Vec<u64> {
        let mut s = sup();
        let mut now = Instant::now();
        s.play(URL_A.to_string(), live, now);
        let mut timeouts = vec![];
        for _ in 0..9 {
            // Find the exact deadline: still alive at T, failed at T + 1 ms.
            let loaded_at = now;
            let before = s.attempt();
            let mut t = Duration::from_secs(1);
            loop {
                s.tick(loaded_at + t + ms(1));
                if s.attempt() != before {
                    break;
                }
                t += Duration::from_secs(1);
                assert!(t < Duration::from_secs(1000));
            }
            timeouts.push(t.as_secs());
            now = loaded_at + t + ms(1);
            now += s.next_retry_in(now).unwrap();
            s.tick(now);
        }
        timeouts
    }

    #[test]
    fn vod_stall_resumes_at_position() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), false, now);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, now);
        s.on_event(g, PlayerEvent::Position(42.0), now);
        s.on_event(g, PlayerEvent::Buffering(true), now);
        s.tick(now + Duration::from_secs(16));
        s.tick(now + Duration::from_secs(17));
        assert_eq!(loads(&s).last().unwrap().1, Some(42.0));
    }

    #[test]
    fn attempt_counter_saturates() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        s.attempt = u32::MAX - 1;
        s.on_event(s.generation(), PlayerEvent::Error(ErrorClass::Network), now);
        assert_eq!(s.attempt(), u32::MAX);
        s.tick(now + Duration::from_secs(5));
        s.on_event(s.generation(), PlayerEvent::Error(ErrorClass::Network), now + Duration::from_secs(5));
        assert_eq!(s.attempt(), u32::MAX);
        assert_eq!(s.next_retry_in(now + Duration::from_secs(5)), Some(ms(3000)));
    }

    #[test]
    fn generation_wraps() {
        let mut s = sup();
        let now = Instant::now();
        s.generation = u32::MAX;
        s.play(URL_A.to_string(), true, now);
        assert_eq!(s.generation(), 0);
        s.on_event(u32::MAX, PlayerEvent::Error(ErrorClass::Network), now);
        assert_eq!(s.next_retry_in(now), None);
    }

    #[test]
    fn time_going_backwards_does_not_panic() {
        let mut s = sup();
        let base = Instant::now() + Duration::from_secs(1000);
        s.play(URL_A.to_string(), true, base);
        let g = s.generation();
        s.on_event(g, PlayerEvent::Started, base);
        s.on_event(g, PlayerEvent::Position(1.0), base - Duration::from_secs(100));
        s.on_event(g, PlayerEvent::Buffering(true), base);
        s.tick(base - Duration::from_secs(500));
        s.on_event(g, PlayerEvent::Error(ErrorClass::Network), base);
        assert_eq!(s.next_retry_in(base - Duration::from_secs(10)), Some(Duration::from_millis(10_500)));
        s.tick(base - Duration::from_secs(10));
        assert_eq!(load_count(&s), 1);
        s.tick(base + Duration::from_secs(1));
        assert_eq!(load_count(&s), 2);
    }

    #[test]
    fn events_when_idle_are_ignored() {
        let mut s = sup();
        let now = Instant::now();
        s.on_event(0, PlayerEvent::Error(ErrorClass::Network), now);
        s.on_event(0, PlayerEvent::Started, now);
        s.tick(now + Duration::from_secs(60));
        assert_eq!(s.status(), Status::Idle);
        assert!(s.backend().calls.is_empty());
    }

    #[test]
    fn debug_does_not_leak_url() {
        let mut s = sup();
        let now = Instant::now();
        s.play(URL_A.to_string(), true, now);
        let dbg = format!("{:?}", s);
        assert!(!dbg.contains("pass"), "{dbg}");
        assert!(!dbg.contains("host"), "{dbg}");
        assert!(dbg.contains("generation"));
    }
}
