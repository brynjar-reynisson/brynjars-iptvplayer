//! Phone sharing: pure state machine deciding when the ffmpeg HLS pipe and
//! the mpv `stream-record` tap start and stop. No I/O; time is passed in.
//!
//! `App` builds a [`ShareInput`] each frame and acts on the [`ShareStep`]:
//! `StartPipe` = spawn ffmpeg, `StartTap` = point `stream-record` at the tcp
//! tap, `StopPipe` = kill ffmpeg and clear `stream-record`. `Stop` is never
//! returned by `Sharer`: App decides itself to give up (and shows the message)
//! when the playback is no longer a live channel.

use std::time::{Duration, Instant};

use iptv::share_pipe::PIPE_WAIT;

/// No segment this long after the tap started = restart the pipe.
pub const TAP_TIMEOUT: Duration = Duration::from_secs(15);
/// Pause between stopping a pipe and starting the next one.
pub const RESTART_GAP: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareInput {
    /// A live channel is playing (not reconnecting/buffering).
    pub live_playing: bool,
    /// Supervisor generation of the current load.
    pub generation: u32,
    /// The ffmpeg process is still running.
    pub pipe_alive: bool,
    /// Monotonic count of segments the pipe has written
    /// (`iptv::share_pipe::segment_count`). Only growth after the tap started
    /// counts: after a restart the old playlist is still there.
    pub segments: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareStep {
    Nothing,
    StartPipe { restart: bool },
    StartTap,
    StopPipe,
    /// Give up sharing with a message. Decided by `App`, never by `Sharer`.
    Stop(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No pipe; may start once `not_before` has passed and a channel plays.
    Idle,
    /// Pipe started, tap not yet.
    Starting,
    /// Tap started.
    Tapping,
}

#[derive(Debug)]
pub struct Sharer {
    phase: Phase,
    /// Some pipe was started this session (later ones are restarts).
    started_before: bool,
    not_before: Option<Instant>,
    generation: u32,
    /// Start of the current wait (pipe start, tap start or last segment growth).
    since: Instant,
    /// Segment counter when the tap started.
    baseline: u64,
    /// Highest segment counter seen since the tap started.
    last_seen: u64,
    /// Not playing on the previous poll: timers restart when playing resumes.
    paused: bool,
    live: bool,
}

impl Default for Sharer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sharer {
    pub fn new() -> Self {
        Sharer {
            phase: Phase::Idle,
            started_before: false,
            not_before: None,
            generation: 0,
            since: Instant::now(),
            baseline: 0,
            last_seen: 0,
            paused: false,
            live: false,
        }
    }

    /// Call every frame. A reconnect (`live_playing` false) pauses everything:
    /// nothing starts, nothing is torn down, timers do not run. Live = the
    /// segment counter grew after the tap started.
    pub fn poll(&mut self, now: Instant, i: ShareInput) -> ShareStep {
        if !i.live_playing {
            self.paused = true;
            return ShareStep::Nothing;
        }
        if self.paused {
            self.paused = false;
            if self.phase == Phase::Tapping {
                self.since = now; // the segment timeout counts playing time only
            }
        }
        match self.phase {
            Phase::Idle => {
                if self.not_before.is_some_and(|t| now < t) {
                    return ShareStep::Nothing;
                }
                self.not_before = None;
                self.phase = Phase::Starting;
                self.generation = i.generation;
                self.since = now;
                let restart = self.started_before;
                self.started_before = true;
                ShareStep::StartPipe { restart }
            }
            Phase::Starting | Phase::Tapping => {
                if !i.pipe_alive || i.generation != self.generation {
                    return self.stop_pipe(now);
                }
                if self.phase == Phase::Starting {
                    if now.saturating_duration_since(self.since) >= PIPE_WAIT {
                        self.phase = Phase::Tapping;
                        self.since = now;
                        self.baseline = i.segments;
                        self.last_seen = i.segments;
                        self.live = false;
                        return ShareStep::StartTap;
                    }
                    return ShareStep::Nothing;
                }
                if i.segments > self.last_seen {
                    self.last_seen = i.segments;
                    self.since = now;
                }
                self.live = self.last_seen > self.baseline;
                // No growth for TAP_TIMEOUT (never started, or stalled): restart.
                if now.saturating_duration_since(self.since) >= TAP_TIMEOUT {
                    return self.stop_pipe(now);
                }
                ShareStep::Nothing
            }
        }
    }

    fn stop_pipe(&mut self, now: Instant) -> ShareStep {
        self.phase = Phase::Idle;
        self.live = false;
        self.not_before = Some(now + RESTART_GAP);
        ShareStep::StopPipe
    }

    pub fn state_name(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "waiting",
            Phase::Starting => "starting",
            Phase::Tapping if self.live => "live",
            Phase::Tapping => "tapping",
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }
}

/// True when nothing listens on `127.0.0.1:port` (binds and drops at once).
pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Pipe restarts allowed within `RESTART_WINDOW` before sharing gives up.
pub const MAX_RESTARTS: usize = 5;
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Counts restarts of a pipe that never produced segments.
#[derive(Debug, Default)]
pub struct RestartBudget {
    times: Vec<Instant>,
}

impl RestartBudget {
    /// Record a restart; true = too many within the window.
    pub fn record(&mut self, now: Instant) -> bool {
        self.times.retain(|t| now.saturating_duration_since(*t) < RESTART_WINDOW);
        self.times.push(now);
        self.times.len() >= MAX_RESTARTS
    }

    /// A restart of the pipe at player generation `gen`; `prev_gen` is the
    /// generation of the previous start. The same generation means the pipe
    /// died or stalled (counts); a new one is a channel switch or reconnect
    /// (does not count, clears the history). True = too many failures.
    pub fn on_restart(&mut self, now: Instant, prev_gen: Option<u32>, gen: u32) -> bool {
        if prev_gen == Some(gen) {
            self.record(now)
        } else {
            self.reset();
            false
        }
    }

    /// Segments appeared: the pipe works, forget earlier failures.
    pub fn reset(&mut self) {
        self.times.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(playing: bool, generation: u32, alive: bool, seg: u64) -> ShareInput {
        ShareInput { live_playing: playing, generation, pipe_alive: alive, segments: seg }
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Sharer already tapping at generation 1; returns the time of StartTap.
    fn tapping(t0: Instant) -> (Sharer, Instant) {
        let mut s = Sharer::new();
        assert_eq!(s.poll(t0, inp(true, 1, false, 0)), ShareStep::StartPipe { restart: false });
        let t1 = t0 + PIPE_WAIT;
        assert_eq!(s.poll(t1, inp(true, 1, true, 0)), ShareStep::StartTap);
        (s, t1)
    }

    #[test]
    fn first_pipe_is_not_a_restart() {
        let mut s = Sharer::new();
        let t = Instant::now();
        assert_eq!(s.poll(t, inp(false, 1, false, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t, inp(true, 1, false, 0)), ShareStep::StartPipe { restart: false });
        assert_eq!(s.state_name(), "starting");
    }

    #[test]
    fn tap_starts_after_the_pipe_wait() {
        let mut s = Sharer::new();
        let t = Instant::now();
        s.poll(t, inp(true, 1, false, 0));
        assert_eq!(s.poll(t + PIPE_WAIT - ms(1), inp(true, 1, true, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t + PIPE_WAIT, inp(true, 1, true, 0)), ShareStep::StartTap);
        assert_eq!(s.state_name(), "tapping");
        assert_eq!(s.poll(t + PIPE_WAIT + ms(10), inp(true, 1, true, 0)), ShareStep::Nothing);
    }

    #[test]
    fn generation_change_restarts_with_append() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        let t2 = t1 + ms(500);
        assert_eq!(s.poll(t2, inp(true, 2, true, 5)), ShareStep::StopPipe);
        assert_eq!(s.poll(t2 + RESTART_GAP - ms(1), inp(true, 2, false, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t2 + RESTART_GAP, inp(true, 2, false, 0)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn dead_pipe_restarts_after_the_gap() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        let t2 = t1 + ms(100);
        assert_eq!(s.poll(t2, inp(true, 1, false, 0)), ShareStep::StopPipe);
        assert_eq!(s.poll(t2 + ms(1999), inp(true, 1, false, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t2 + RESTART_GAP, inp(true, 1, false, 0)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn dead_pipe_before_the_tap_also_restarts() {
        let t = Instant::now();
        let mut s = Sharer::new();
        s.poll(t, inp(true, 1, false, 0));
        assert_eq!(s.poll(t + ms(100), inp(true, 1, false, 0)), ShareStep::StopPipe);
    }

    #[test]
    fn no_segments_for_15s_restarts() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + TAP_TIMEOUT - ms(1), inp(true, 1, true, 0)), ShareStep::Nothing);
        let t2 = t1 + TAP_TIMEOUT;
        assert_eq!(s.poll(t2, inp(true, 1, true, 0)), ShareStep::StopPipe);
        assert_eq!(s.poll(t2 + RESTART_GAP, inp(true, 1, false, 0)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn growth_after_tap_makes_it_live() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert!(!s.is_live());
        assert_eq!(s.poll(t1 + ms(3000), inp(true, 1, true, 1)), ShareStep::Nothing);
        assert!(s.is_live());
        assert_eq!(s.state_name(), "live");
        // keeps growing: never times out
        for k in 2..100u64 {
            assert_eq!(s.poll(t1 + ms(3000 + k * 2000), inp(true, 1, true, k)), ShareStep::Nothing);
        }
        assert_eq!(s.state_name(), "live");
    }

    #[test]
    fn stale_playlist_after_restart_does_not_count_as_flowing() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        s.poll(t1 + ms(2000), inp(true, 1, true, 10));
        assert!(s.is_live());
        let t2 = t1 + ms(2500);
        assert_eq!(s.poll(t2, inp(true, 2, true, 10)), ShareStep::StopPipe);
        let t3 = t2 + RESTART_GAP;
        assert_eq!(s.poll(t3, inp(true, 2, false, 10)), ShareStep::StartPipe { restart: true });
        let t4 = t3 + PIPE_WAIT;
        assert_eq!(s.poll(t4, inp(true, 2, true, 10)), ShareStep::StartTap);
        // old playlist stays, count constant: never live
        for k in 1..15u64 {
            assert_eq!(s.poll(t4 + Duration::from_secs(k), inp(true, 2, true, 10)), ShareStep::Nothing);
            assert!(!s.is_live());
        }
        assert_eq!(s.poll(t4 + TAP_TIMEOUT, inp(true, 2, true, 10)), ShareStep::StopPipe);
        assert!(!s.is_live());
    }

    #[test]
    fn live_stream_that_stops_growing_restarts_after_tap_timeout() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        let t2 = t1 + ms(2000);
        s.poll(t2, inp(true, 1, true, 3));
        assert!(s.is_live());
        assert_eq!(s.poll(t2 + TAP_TIMEOUT - ms(1), inp(true, 1, true, 3)), ShareStep::Nothing);
        let t3 = t2 + TAP_TIMEOUT;
        assert_eq!(s.poll(t3, inp(true, 1, true, 3)), ShareStep::StopPipe);
        assert!(!s.is_live());
        assert_eq!(s.poll(t3 + RESTART_GAP, inp(true, 1, false, 3)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn reconnect_pauses_without_stopping() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        s.poll(t1 + ms(500), inp(true, 1, true, 5));
        // reconnecting for 60 s: nothing happens, even with a dead pipe
        for k in 0..60u64 {
            assert_eq!(s.poll(t1 + ms(1000 + k * 1000), inp(false, 1, k % 2 == 0, 5)), ShareStep::Nothing);
        }
        // playing again, same generation, pipe alive: carries on
        assert_eq!(s.poll(t1 + Duration::from_secs(62), inp(true, 1, true, 5)), ShareStep::Nothing);
        assert_eq!(s.state_name(), "live");
    }

    #[test]
    fn pause_before_the_tap_resumes_with_start_tap() {
        let t = Instant::now();
        let mut s = Sharer::new();
        s.poll(t, inp(true, 1, false, 0));
        assert_eq!(s.poll(t + Duration::from_secs(30), inp(false, 1, true, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t + Duration::from_secs(31), inp(true, 1, true, 0)), ShareStep::StartTap);
        // pipe died meanwhile: stop instead
        let mut s = Sharer::new();
        s.poll(t, inp(true, 1, false, 0));
        s.poll(t + Duration::from_secs(30), inp(false, 1, false, 0));
        assert_eq!(s.poll(t + Duration::from_secs(31), inp(true, 1, false, 0)), ShareStep::StopPipe);
    }

    #[test]
    fn generation_change_during_reconnect_forces_restart_when_playing() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + ms(1000), inp(false, 2, true, 5)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + ms(5000), inp(true, 2, true, 5)), ShareStep::StopPipe);
    }

    #[test]
    fn timeout_does_not_run_while_not_playing() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + Duration::from_secs(40), inp(false, 1, true, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + Duration::from_secs(41), inp(true, 1, true, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + Duration::from_secs(41) + TAP_TIMEOUT, inp(true, 1, true, 0)), ShareStep::StopPipe);
    }

    #[test]
    fn nothing_starts_while_not_playing_after_a_stop() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1, inp(true, 1, false, 0)), ShareStep::StopPipe);
        assert_eq!(s.poll(t1 + Duration::from_secs(10), inp(false, 1, false, 0)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + Duration::from_secs(11), inp(true, 1, false, 0)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn never_two_pipes() {
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..50 {
            let mut s = Sharer::new();
            let mut now = Instant::now();
            let mut pipes: i32 = 0;
            let mut gen = 1u32;
            let mut segs = 0u64;
            for _ in 0..2000 {
                now += ms(rnd(3000));
                if rnd(5) == 0 {
                    gen += 1;
                }
                // a real pipe only reports alive when one runs
                let alive = pipes == 1 && rnd(10) != 0;
                if rnd(3) != 0 {
                    segs += rnd(3); // grows or stalls
                }
                let i = inp(rnd(4) != 0, gen, alive, segs);
                match s.poll(now, i) {
                    ShareStep::StartPipe { .. } => pipes += 1,
                    ShareStep::StopPipe => pipes -= 1,
                    _ => {}
                }
                assert!((0..=1).contains(&pipes), "pipes = {pipes}");
            }
        }
    }

    #[test]
    fn is_live_follows_segments() {
        let t = Instant::now();
        let mut s = Sharer::new();
        assert!(!s.is_live());
        s.poll(t, inp(true, 1, false, 0));
        s.poll(t + PIPE_WAIT, inp(true, 1, true, 0));
        assert!(!s.is_live());
        s.poll(t + PIPE_WAIT + ms(500), inp(true, 1, true, 1));
        assert!(s.is_live());
        assert_eq!(s.state_name(), "live");
        s.poll(t + PIPE_WAIT + ms(600), inp(true, 2, true, 5)); // StopPipe
        assert!(!s.is_live());
    }

    #[test]
    fn restarts_count_only_for_the_same_generation() {
        let t = Instant::now();
        let at = |k: u64| t + Duration::from_secs(k);
        // Zapping: a new generation each time never trips.
        let mut b = RestartBudget::default();
        for k in 0..20u32 {
            assert!(!b.on_restart(at(k as u64), Some(k), k + 1));
        }
        // The same generation 5 times trips on the 5th.
        let mut b = RestartBudget::default();
        for k in 0..MAX_RESTARTS - 1 {
            assert!(!b.on_restart(at(k as u64 * 3), Some(7), 7));
        }
        assert!(b.on_restart(at(13), Some(7), 7));
        // A zap in between clears the failures.
        let mut b = RestartBudget::default();
        for k in 0..MAX_RESTARTS - 1 {
            assert!(!b.on_restart(at(k as u64), Some(7), 7));
        }
        assert!(!b.on_restart(at(5), Some(7), 8));
        for k in 0..MAX_RESTARTS - 1 {
            assert!(!b.on_restart(at(6 + k as u64), Some(8), 8));
        }
        assert!(b.on_restart(at(11), Some(8), 8));
    }

    #[test]
    fn port_free_sees_a_held_port() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!port_free(port));
        drop(l);
        assert!(port_free(port));
    }

    #[test]
    fn restart_budget_trips_on_fast_repeats_only() {
        let t = Instant::now();
        let mut b = RestartBudget::default();
        for k in 0..MAX_RESTARTS - 1 {
            assert!(!b.record(t + Duration::from_secs(k as u64 * 3)));
        }
        assert!(b.record(t + Duration::from_secs(13)));
        let mut slow = RestartBudget::default();
        for k in 0..20 {
            assert!(!slow.record(t + Duration::from_secs(k * 30)));
        }
    }
}
