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
    /// The playlist has segments.
    pub has_segments: bool,
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
    /// Start of the current wait (pipe start or tap start).
    since: Instant,
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
            paused: false,
            live: false,
        }
    }

    /// Call every frame. A reconnect (`live_playing` false) pauses everything:
    /// nothing starts, nothing is torn down, timers do not run.
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
                        return ShareStep::StartTap;
                    }
                    return ShareStep::Nothing;
                }
                if i.has_segments {
                    self.live = true;
                } else if now.saturating_duration_since(self.since) >= TAP_TIMEOUT {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(playing: bool, generation: u32, alive: bool, seg: bool) -> ShareInput {
        ShareInput { live_playing: playing, generation, pipe_alive: alive, has_segments: seg }
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Sharer already tapping at generation 1; returns the time of StartTap.
    fn tapping(t0: Instant) -> (Sharer, Instant) {
        let mut s = Sharer::new();
        assert_eq!(s.poll(t0, inp(true, 1, false, false)), ShareStep::StartPipe { restart: false });
        let t1 = t0 + PIPE_WAIT;
        assert_eq!(s.poll(t1, inp(true, 1, true, false)), ShareStep::StartTap);
        (s, t1)
    }

    #[test]
    fn first_pipe_is_not_a_restart() {
        let mut s = Sharer::new();
        let t = Instant::now();
        assert_eq!(s.poll(t, inp(false, 1, false, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t, inp(true, 1, false, false)), ShareStep::StartPipe { restart: false });
        assert_eq!(s.state_name(), "starting");
    }

    #[test]
    fn tap_starts_after_the_pipe_wait() {
        let mut s = Sharer::new();
        let t = Instant::now();
        s.poll(t, inp(true, 1, false, false));
        assert_eq!(s.poll(t + PIPE_WAIT - ms(1), inp(true, 1, true, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t + PIPE_WAIT, inp(true, 1, true, false)), ShareStep::StartTap);
        assert_eq!(s.state_name(), "tapping");
        assert_eq!(s.poll(t + PIPE_WAIT + ms(10), inp(true, 1, true, false)), ShareStep::Nothing);
    }

    #[test]
    fn generation_change_restarts_with_append() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        let t2 = t1 + ms(500);
        assert_eq!(s.poll(t2, inp(true, 2, true, true)), ShareStep::StopPipe);
        assert_eq!(s.poll(t2 + RESTART_GAP - ms(1), inp(true, 2, false, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t2 + RESTART_GAP, inp(true, 2, false, false)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn dead_pipe_restarts_after_the_gap() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        let t2 = t1 + ms(100);
        assert_eq!(s.poll(t2, inp(true, 1, false, false)), ShareStep::StopPipe);
        assert_eq!(s.poll(t2 + ms(1999), inp(true, 1, false, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t2 + RESTART_GAP, inp(true, 1, false, false)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn dead_pipe_before_the_tap_also_restarts() {
        let t = Instant::now();
        let mut s = Sharer::new();
        s.poll(t, inp(true, 1, false, false));
        assert_eq!(s.poll(t + ms(100), inp(true, 1, false, false)), ShareStep::StopPipe);
    }

    #[test]
    fn no_segments_for_15s_restarts() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + TAP_TIMEOUT - ms(1), inp(true, 1, true, false)), ShareStep::Nothing);
        let t2 = t1 + TAP_TIMEOUT;
        assert_eq!(s.poll(t2, inp(true, 1, true, false)), ShareStep::StopPipe);
        assert_eq!(s.poll(t2 + RESTART_GAP, inp(true, 1, false, false)), ShareStep::StartPipe { restart: true });
    }

    #[test]
    fn segments_stop_the_timeout() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + ms(3000), inp(true, 1, true, true)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + TAP_TIMEOUT * 4, inp(true, 1, true, true)), ShareStep::Nothing);
        assert_eq!(s.state_name(), "live");
    }

    #[test]
    fn reconnect_pauses_without_stopping() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        s.poll(t1 + ms(500), inp(true, 1, true, true));
        // reconnecting for 60 s: nothing happens, even with a dead pipe
        for k in 0..60u64 {
            assert_eq!(s.poll(t1 + ms(1000 + k * 1000), inp(false, 1, k % 2 == 0, true)), ShareStep::Nothing);
        }
        // playing again, same generation, pipe alive: carries on
        assert_eq!(s.poll(t1 + Duration::from_secs(62), inp(true, 1, true, true)), ShareStep::Nothing);
        assert_eq!(s.state_name(), "live");
    }

    #[test]
    fn pause_before_the_tap_resumes_with_start_tap() {
        let t = Instant::now();
        let mut s = Sharer::new();
        s.poll(t, inp(true, 1, false, false));
        assert_eq!(s.poll(t + Duration::from_secs(30), inp(false, 1, true, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t + Duration::from_secs(31), inp(true, 1, true, false)), ShareStep::StartTap);
        // pipe died meanwhile: stop instead
        let mut s = Sharer::new();
        s.poll(t, inp(true, 1, false, false));
        s.poll(t + Duration::from_secs(30), inp(false, 1, false, false));
        assert_eq!(s.poll(t + Duration::from_secs(31), inp(true, 1, false, false)), ShareStep::StopPipe);
    }

    #[test]
    fn generation_change_during_reconnect_forces_restart_when_playing() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + ms(1000), inp(false, 2, true, true)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + ms(5000), inp(true, 2, true, true)), ShareStep::StopPipe);
    }

    #[test]
    fn timeout_does_not_run_while_not_playing() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1 + Duration::from_secs(40), inp(false, 1, true, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + Duration::from_secs(41), inp(true, 1, true, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + Duration::from_secs(41) + TAP_TIMEOUT, inp(true, 1, true, false)), ShareStep::StopPipe);
    }

    #[test]
    fn nothing_starts_while_not_playing_after_a_stop() {
        let t = Instant::now();
        let (mut s, t1) = tapping(t);
        assert_eq!(s.poll(t1, inp(true, 1, false, false)), ShareStep::StopPipe);
        assert_eq!(s.poll(t1 + Duration::from_secs(10), inp(false, 1, false, false)), ShareStep::Nothing);
        assert_eq!(s.poll(t1 + Duration::from_secs(11), inp(true, 1, false, false)), ShareStep::StartPipe { restart: true });
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
            for _ in 0..2000 {
                now += ms(rnd(3000));
                if rnd(20) == 0 {
                    gen += 1;
                }
                // a real pipe only reports alive when one runs
                let alive = pipes == 1 && rnd(10) != 0;
                let i = inp(rnd(4) != 0, gen, alive, rnd(2) == 0);
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
        s.poll(t, inp(true, 1, false, false));
        s.poll(t + PIPE_WAIT, inp(true, 1, true, false));
        assert!(!s.is_live());
        s.poll(t + PIPE_WAIT + ms(500), inp(true, 1, true, true));
        assert!(s.is_live());
        assert_eq!(s.state_name(), "live");
        s.poll(t + PIPE_WAIT + ms(600), inp(true, 2, true, true)); // StopPipe
        assert!(!s.is_live());
    }
}
