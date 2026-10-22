//! Skip-intro flow (pure). See the module tests for the sequence.

use iptv::intro::{find_intro, find_intro_pair, is_blank, scan_times, Prints, INTRO_STEP_SECS};

pub const REFINE_SECS: f64 = 24.0;
pub const MAX_EXTEND_SECS: f64 = 20.0;
/// Reference samples kept around its intro span when it is stored.
pub const REF_MARGIN_SECS: f64 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Which {
    Own,
    Reference,
    Refine,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Scan { job: u64, which: Which, times: Vec<f64> },
    SaveReference(Prints),
}

/// Frame times (1 s steps) for the refinement scan: from the last matched
/// grid time `REFINE_SECS` on, never beyond `duration - 3`.
pub fn refine_times(span_end: f64, duration: f64) -> Vec<f64> {
    let first = (span_end - INTRO_STEP_SECS / 2.0).floor();
    let limit = (first + REFINE_SECS).min(duration - 3.0);
    let mut v = Vec::new();
    let mut t = first.max(0.0);
    while t <= limit {
        v.push(t);
        t += 1.0;
    }
    v
}

/// The reference samples kept as the show's reference: only its intro
/// (`start..end`) plus `REF_MARGIN_SECS` on both sides, in time order. A whole
/// scan would let that episode match itself beyond its intro later.
pub fn intro_window(mut prints: Prints, start: f64, end: f64) -> Prints {
    prints.retain(|(t, _)| *t >= start - REF_MARGIN_SECS && *t <= end + REF_MARGIN_SECS);
    prints.sort_by(|a, b| a.0.total_cmp(&b.0));
    prints
}

/// Where to jump to: the first non-blank frame after the intro's end card
/// (a run of blank frames), if within `MAX_EXTEND_SECS` of `span_end`; else
/// `span_end`. Based on the fingerprints (flat = card), not the credits
/// classifier: a dim opening shot reads as "credits" but is not a card.
pub fn skip_target(span_end: f64, prints: &Prints) -> f64 {
    let from = span_end - INTRO_STEP_SECS / 2.0;
    let mut card = false;
    for (t, fp) in prints.iter().filter(|(t, _)| *t >= from) {
        let Some(fp) = fp else { continue };
        if is_blank(fp) {
            card = true;
        } else if card {
            return if *t - span_end <= MAX_EXTEND_SECS { *t } else { span_end };
        }
    }
    span_end
}

/// What an Enter/Esc press does to the "Skip intro" prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntroKey {
    /// Not ours: the key keeps its usual meaning.
    Ignore,
    /// Enter: skip to the target (consumes Enter and Esc this press).
    Accept,
    /// Esc: hide the prompt for this episode (nothing else this press).
    Dismiss,
}

/// When the prompt owns Enter/Esc: it shows (`offering`, no seek drag), the
/// "Next episode" offer does not (that one wins the keys), not typing, the
/// subtitle menu is closed. Enter only in the player view (also fullscreen)
/// with no tab button focused; in the browse view Enter keeps its list
/// meaning and the button is clicked instead.
#[derive(Debug, Clone, Copy)]
pub struct KeyGate {
    pub offering: bool,
    pub next_offer: bool,
    pub typing: bool,
    pub sub_menu: bool,
    pub player_view: bool,
    pub tab_focused: bool,
}

impl KeyGate {
    pub fn key(&self, enter: bool, esc: bool) -> IntroKey {
        if !self.offering || self.next_offer || self.typing || self.sub_menu {
            IntroKey::Ignore
        } else if enter && self.player_view && !self.tab_focused {
            IntroKey::Accept
        } else if esc {
            IntroKey::Dismiss
        } else {
            IntroKey::Ignore
        }
    }
}

#[derive(Debug)]
enum Phase {
    Off,
    Waiting,
    /// A scan is in flight; only a result for `job` counts.
    Scanning { job: u64, which: Which },
    Ready { start: f64, target: f64, done: bool },
}

/// Skip-intro flow. Pure: the App performs the scans and stores references.
///
/// `arm` -> `update(duration)` asks for the Own scan -> `on_result` either asks
/// for the Refine scan (stored reference) or for a Reference scan of the
/// neighbour episode (first time; own prints are kept) -> `SaveReference`
/// (the neighbour's intro window only)
/// (the Refine scan is returned by the next `update`) -> Refine result = Ready.
#[derive(Debug)]
pub struct SkipIntro {
    phase: Phase,
    next_job: u64,
    stored: Option<Prints>,
    has_neighbour: bool,
    own: Option<Prints>,
    /// Refine scan queued behind a `SaveReference`.
    pending: Option<Action>,
    /// Intro span in the own timeline (start, end) once found.
    span: Option<(f64, f64)>,
}

impl Default for SkipIntro {
    fn default() -> Self {
        Self::new()
    }
}

impl SkipIntro {
    pub fn new() -> SkipIntro {
        SkipIntro { phase: Phase::Off, next_job: 0, stored: None, has_neighbour: false, own: None, pending: None, span: None }
    }

    /// An episode starts. Neither a stored reference nor a neighbour = Off.
    pub fn arm(&mut self, stored: Option<Prints>, has_neighbour: bool) {
        let next_job = self.next_job; // job ids stay unique across episodes
        *self = SkipIntro::new();
        self.next_job = next_job;
        if stored.is_some() || has_neighbour {
            self.stored = stored;
            self.has_neighbour = has_neighbour;
            self.phase = Phase::Waiting;
        }
    }

    pub fn disarm(&mut self) {
        self.arm(None, false);
    }

    fn scan(&mut self, which: Which, times: Vec<f64>) -> Action {
        self.next_job += 1;
        self.phase = Phase::Scanning { job: self.next_job, which };
        Action::Scan { job: self.next_job, which, times }
    }

    /// Call every frame with the playing duration; returns the next scan to start.
    pub fn update(&mut self, duration: Option<f64>) -> Action {
        if let Some(a) = self.pending.take() {
            return a;
        }
        if !matches!(self.phase, Phase::Waiting) {
            return Action::None;
        }
        let Some(d) = duration else { return Action::None };
        let times = scan_times(d);
        if times.is_empty() {
            self.phase = Phase::Off;
            return Action::None;
        }
        self.scan(Which::Own, times)
    }

    fn off(&mut self) -> Action {
        self.phase = Phase::Off;
        self.pending = None;
        Action::None
    }

    pub fn on_result(&mut self, job: u64, prints: Prints, duration: f64) -> Action {
        let which = match self.phase {
            Phase::Scanning { job: j, which } if j == job => which,
            _ => return Action::None,
        };
        match which {
            Which::Own => {
                if let Some(stored) = self.stored.take() {
                    match find_intro(&prints, &stored) {
                        Some(span) => self.start_refine(span.start, span.end, duration),
                        None => self.off(),
                    }
                } else if self.has_neighbour {
                    self.own = Some(prints);
                    self.scan(Which::Reference, scan_times(duration))
                } else {
                    self.off()
                }
            }
            Which::Reference => {
                let own = self.own.take().unwrap_or_default();
                match find_intro_pair(&own, &prints) {
                    Some((span, ref_span)) => {
                        // The Refine scan is queued; the next `update` returns it.
                        let scan = self.start_refine(span.start, span.end, duration);
                        self.pending = Some(scan);
                        Action::SaveReference(intro_window(prints, ref_span.start, ref_span.end))
                    }
                    None => self.off(),
                }
            }
            Which::Refine => {
                let (start, end) = self.span.unwrap_or((0.0, 0.0));
                self.phase = Phase::Ready { start, target: skip_target(end, &prints), done: false };
                Action::None
            }
        }
    }

    fn start_refine(&mut self, start: f64, end: f64, duration: f64) -> Action {
        self.span = Some((start, end));
        let times = refine_times(end, duration);
        self.scan(Which::Refine, times)
    }

    /// A scan failed (current job only) -> Off.
    pub fn on_failed(&mut self, job: u64) {
        if matches!(self.phase, Phase::Scanning { job: j, .. } if j == job) {
            self.off();
        }
    }

    /// Show the "Skip intro" prompt at this playback position?
    pub fn offering(&self, position: f64) -> bool {
        matches!(self.phase, Phase::Ready { start, target, done: false } if position >= start && position < target - 1.0)
    }

    /// The intro (start, end) in the playing episode once it is ready.
    pub fn span(&self) -> Option<(f64, f64)> {
        self.span.filter(|_| matches!(self.phase, Phase::Ready { .. }))
    }

    pub fn target(&self) -> Option<f64> {
        match self.phase {
            Phase::Ready { target, .. } => Some(target),
            _ => None,
        }
    }

    pub fn dismiss(&mut self) {
        if let Phase::Ready { done, .. } = &mut self.phase {
            *done = true;
        }
    }

    pub fn accept(&mut self) {
        self.dismiss();
    }

    pub fn state_name(&self) -> &'static str {
        match self.phase {
            Phase::Off => "off",
            Phase::Waiting => "waiting",
            Phase::Scanning { .. } => "scanning",
            Phase::Ready { done: false, .. } => "ready",
            Phase::Ready { done: true, .. } => "done",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::intro::Fingerprint;

    fn fp(seed: u8) -> Fingerprint {
        let mut x = (seed as u32).wrapping_mul(2654435761).wrapping_add(12345);
        let mut out = [0u8; 144];
        for b in out.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 24) as u8;
        }
        Fingerprint(out)
    }
    fn black() -> Fingerprint {
        Fingerprint([2; 144])
    }
    /// black x3, `intro_len` intro frames (seeds 1..), then unique scenes from `scene`.
    fn episode(intro_len: u8, scene: u8, total: usize) -> Prints {
        (0..total)
            .map(|i| {
                let f = if i < 3 {
                    black()
                } else if i < 3 + intro_len as usize {
                    fp((i - 3) as u8 + 1)
                } else {
                    fp(scene.wrapping_add(i as u8))
                };
                (i as f64 * 4.0, Some(f))
            })
            .collect()
    }
    fn unrelated(base: u8) -> Prints {
        (0..40).map(|i| (i as f64 * 4.0, Some(fp(base.wrapping_add(i as u8))))).collect()
    }
    // intro frames 3..23 -> span start 12, end 12 + 19*4 + 2 = 90
    const START: f64 = 12.0;
    const END: f64 = 90.0;

    fn job_of(a: &Action) -> u64 {
        match a {
            Action::Scan { job, .. } => *job,
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    /// Blank (flat) and non-blank (range 81) frames, like the real E04 data.
    fn flat() -> Option<Fingerprint> {
        let mut a = [0u8; 144];
        a[1] = 3;
        Some(Fingerprint(a))
    }
    fn busy() -> Option<Fingerprint> {
        let mut a = [18u8; 144];
        a[1] = 99;
        a[2] = 18;
        Some(Fingerprint(a))
    }
    /// One print per entry: `b` busy, `.` blank, `x` failed; 1 s apart from `start`.
    fn seq(start: f64, pat: &str) -> Prints {
        pat.chars()
            .enumerate()
            .map(|(i, c)| (start + i as f64, match c { 'b' => busy(), '.' => flat(), _ => None }))
            .collect()
    }

    #[test]
    fn arm_without_reference_or_neighbour_is_off() {
        let mut s = SkipIntro::new();
        s.arm(None, false);
        assert_eq!(s.state_name(), "off");
        assert_eq!(s.update(Some(2700.0)), Action::None);
    }

    #[test]
    fn update_requests_the_own_scan_once_the_duration_is_known() {
        let mut s = SkipIntro::new();
        s.arm(None, true);
        assert_eq!(s.update(None), Action::None);
        let a = s.update(Some(2700.0));
        match &a {
            Action::Scan { which: Which::Own, times, .. } => assert_eq!(times, &scan_times(2700.0)),
            other => panic!("{other:?}"),
        }
        assert_eq!(s.update(Some(2700.0)), Action::None);
    }

    #[test]
    fn update_with_short_duration_turns_off() {
        let mut s = SkipIntro::new();
        s.arm(None, true);
        assert_eq!(s.update(Some(20.0)), Action::None);
        assert_eq!(s.state_name(), "off");
    }

    #[test]
    fn own_scan_with_stored_reference_that_matches_asks_for_refinement() {
        let mut s = SkipIntro::new();
        s.arm(Some(episode(20, 150, 40)), false);
        let job = job_of(&s.update(Some(2700.0)));
        let a = s.on_result(job, episode(20, 100, 40), 2700.0);
        match a {
            Action::Scan { which: Which::Refine, times, job: j } => {
                assert_ne!(j, job);
                assert_eq!(times, refine_times(END, 2700.0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn own_scan_without_match_turns_off() {
        let mut s = SkipIntro::new();
        s.arm(Some(unrelated(200)), false);
        let job = job_of(&s.update(Some(2700.0)));
        assert_eq!(s.on_result(job, unrelated(100), 2700.0), Action::None);
        assert_eq!(s.state_name(), "off");
    }

    #[test]
    fn first_time_flow_scans_the_neighbour_then_saves_the_reference_then_refines() {
        let mut s = SkipIntro::new();
        s.arm(None, true);
        let own_job = job_of(&s.update(Some(2700.0)));
        let a = s.on_result(own_job, episode(20, 100, 40), 2700.0);
        let (ref_job, times) = match a {
            Action::Scan { job, which: Which::Reference, times } => (job, times),
            other => panic!("{other:?}"),
        };
        assert_eq!(times, scan_times(2700.0));
        let reference = episode(20, 150, 40);
        let a = s.on_result(ref_job, reference.clone(), 2700.0);
        // Only the reference's intro (12..90) plus REF_MARGIN_SECS: 4..96.
        let window: Prints = reference.iter().filter(|(t, _)| (4.0..=98.0).contains(t)).cloned().collect();
        assert_eq!(window.first().map(|p| p.0), Some(4.0));
        assert_eq!(window.last().map(|p| p.0), Some(96.0));
        assert_eq!(a, Action::SaveReference(window));
        match s.update(Some(2700.0)) {
            Action::Scan { which: Which::Refine, times, .. } => assert_eq!(times, refine_times(END, 2700.0)),
            other => panic!("{other:?}"),
        }
        assert_eq!(s.update(Some(2700.0)), Action::None);
    }

    #[test]
    fn save_reference_keeps_only_the_margin_window_with_an_offset() {
        let mut s = SkipIntro::new();
        s.arm(None, true);
        let own_job = job_of(&s.update(Some(2700.0)));
        let ref_job = job_of(&s.on_result(own_job, episode(20, 100, 40), 2700.0));
        // The neighbour's intro starts 20 s later (5 more black frames).
        let mut reference: Prints = (0..5).map(|i| (i as f64 * 4.0, Some(black()))).collect();
        reference.extend(episode(20, 150, 40).into_iter().map(|(t, f)| (t + 20.0, f)));
        let Action::SaveReference(saved) = s.on_result(ref_job, reference, 2700.0) else { panic!() };
        // Reference span 32..110 -> window 24..118: samples 24..116.
        let times: Vec<f64> = saved.iter().map(|p| p.0).collect();
        assert_eq!(times.first(), Some(&24.0));
        assert_eq!(times.last(), Some(&116.0));
        assert!(times.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn an_episode_against_its_own_stored_reference_finds_the_intro_not_the_story() {
        // E05 was the neighbour when E04 played: its intro window was stored.
        let e05 = episode(20, 150, 60);
        let mut s = SkipIntro::new();
        s.arm(None, true);
        let j = job_of(&s.update(Some(2700.0)));
        let j = job_of(&s.on_result(j, episode(20, 100, 60), 2700.0));
        let Action::SaveReference(stored) = s.on_result(j, e05.clone(), 2700.0) else { panic!() };
        // Now E05 itself plays.
        let mut s = SkipIntro::new();
        s.arm(Some(stored), false);
        let j = job_of(&s.update(Some(2700.0)));
        let refine = job_of(&s.on_result(j, e05, 2700.0));
        s.on_result(refine, seq(88.0, "bb.....bbbb"), 2700.0);
        let (start, end) = s.span().unwrap();
        assert_eq!(start, START);
        assert!(end >= END && end <= END + REF_MARGIN_SECS, "end {end}");
    }

    #[test]
    fn no_common_run_in_the_reference_turns_off() {
        let mut s = SkipIntro::new();
        s.arm(None, true);
        let j = job_of(&s.update(Some(2700.0)));
        let j = job_of(&s.on_result(j, unrelated(100), 2700.0));
        assert_eq!(s.on_result(j, unrelated(200), 2700.0), Action::None);
        assert_eq!(s.state_name(), "off");
        assert_eq!(s.update(Some(2700.0)), Action::None);
    }

    #[test]
    fn stale_job_results_are_ignored() {
        let mut s = SkipIntro::new();
        s.arm(Some(episode(20, 150, 40)), false);
        let job = job_of(&s.update(Some(2700.0)));
        assert_eq!(s.on_result(job + 99, episode(20, 100, 40), 2700.0), Action::None);
        assert_eq!(s.state_name(), "scanning");
        s.on_failed(job + 99);
        assert_eq!(s.state_name(), "scanning");
        let refine = job_of(&s.on_result(job, episode(20, 100, 40), 2700.0));
        // the Own job is stale now
        assert_eq!(s.on_result(job, episode(20, 100, 40), 2700.0), Action::None);
        s.on_failed(job);
        assert_eq!(s.state_name(), "scanning");
        s.on_failed(refine);
        assert_eq!(s.state_name(), "off");
    }

    fn ready() -> SkipIntro {
        let mut s = SkipIntro::new();
        s.arm(Some(episode(20, 150, 40)), false);
        let job = job_of(&s.update(Some(2700.0)));
        let refine = job_of(&s.on_result(job, episode(20, 100, 40), 2700.0));
        // intro tail 88-89, card 90-94, first scene from 95
        assert_eq!(s.on_result(refine, seq(88.0, "bb.....bbbb"), 2700.0), Action::None);
        s
    }

    #[test]
    fn refine_result_makes_it_ready_and_offering_inside_the_intro() {
        let s = ready();
        assert_eq!(s.state_name(), "ready");
        assert_eq!(s.target(), Some(95.0));
        assert!(!s.offering(START - 1.0));
        assert!(s.offering(START));
        assert!(s.offering(50.0));
    }

    #[test]
    fn not_offering_past_the_target_or_when_resumed_after_it() {
        let s = ready();
        assert!(s.offering(93.9));
        assert!(!s.offering(94.0));
        assert!(!s.offering(300.0));
    }

    #[test]
    fn dismiss_and_accept_stop_the_offer() {
        let mut s = ready();
        s.dismiss();
        assert!(!s.offering(50.0));
        let mut s = ready();
        s.accept();
        assert!(!s.offering(50.0));
    }

    #[test]
    fn skip_target_is_the_first_non_blank_frame_after_the_card() {
        // E04-like: intro tail, 5 flat frames, then a dim shot, then brighter.
        let p = seq(86.0, "bbbb.....bbbbbbb");
        assert_eq!(skip_target(90.0, &p), 95.0);
    }

    #[test]
    fn skip_target_no_card_gives_span_end() {
        assert_eq!(skip_target(90.0, &seq(86.0, "bbbbbbbbbb")), 90.0);
        assert_eq!(skip_target(90.0, &Vec::new()), 90.0);
        // a card but nothing after it
        assert_eq!(skip_target(90.0, &seq(86.0, "bbbb....")), 90.0);
    }

    #[test]
    fn skip_target_card_beyond_the_cap_gives_span_end() {
        let p = seq(88.0, &format!("bb{}b", ".".repeat(25)));
        assert_eq!(skip_target(90.0, &p), 90.0);
    }

    #[test]
    fn skip_target_ignores_failed_frames() {
        let p = seq(86.0, "bbbb.x.xbbb");
        assert_eq!(skip_target(90.0, &p), 94.0);
        // a failed frame alone is not a card
        assert_eq!(skip_target(90.0, &seq(86.0, "bbbbxbbb")), 90.0);
    }

    #[test]
    fn skip_target_longer_card_like_e03() {
        let p = seq(86.0, &format!("bbbb{}bbb", ".".repeat(12)));
        assert_eq!(skip_target(90.0, &p), 102.0);
    }

    #[test]
    fn skip_target_frames_before_the_span_end_are_ignored() {
        // a blank frame well before span_end (earlier black in the intro) is no card
        let p = seq(60.0, ".bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(skip_target(90.0, &p), 90.0);
    }

    #[test]
    fn span_is_known_once_ready() {
        let mut s = SkipIntro::new();
        assert_eq!(s.span(), None);
        s.arm(None, true);
        assert_eq!(s.span(), None);
        assert_eq!(ready().span(), Some((START, END)));
    }

    fn gate() -> KeyGate {
        KeyGate { offering: true, next_offer: false, typing: false, sub_menu: false, player_view: true, tab_focused: false }
    }

    #[test]
    fn enter_accepts_and_esc_dismisses_in_the_player_view() {
        assert_eq!(gate().key(true, false), IntroKey::Accept);
        assert_eq!(gate().key(false, true), IntroKey::Dismiss);
        assert_eq!(gate().key(true, true), IntroKey::Accept, "Enter wins, Esc is still consumed");
        assert_eq!(gate().key(false, false), IntroKey::Ignore);
    }

    #[test]
    fn enter_only_in_the_player_view_without_a_focused_tab_button() {
        assert_eq!(KeyGate { player_view: false, ..gate() }.key(true, false), IntroKey::Ignore);
        assert_eq!(KeyGate { player_view: false, ..gate() }.key(false, true), IntroKey::Dismiss, "Esc in browse");
        assert_eq!(KeyGate { tab_focused: true, ..gate() }.key(true, false), IntroKey::Ignore);
    }

    #[test]
    fn keys_are_not_ours_when_hidden_typing_in_the_sub_menu_or_next_offer_shows() {
        for g in [
            KeyGate { offering: false, ..gate() },
            KeyGate { next_offer: true, ..gate() },
            KeyGate { typing: true, ..gate() },
            KeyGate { sub_menu: true, ..gate() },
        ] {
            assert_eq!(g.key(true, false), IntroKey::Ignore, "{g:?}");
            assert_eq!(g.key(false, true), IntroKey::Ignore, "{g:?}");
        }
    }

    #[test]
    fn refine_times_are_one_second_steps_and_respect_duration() {
        let t = refine_times(90.0, 2700.0);
        assert_eq!(t.first(), Some(&88.0));
        assert_eq!(t.last(), Some(&112.0));
        assert_eq!(t.len(), 25);
        assert!(t.windows(2).all(|w| w[1] - w[0] == 1.0));
        let t = refine_times(90.0, 100.0);
        assert_eq!(t.last(), Some(&97.0));
        assert!(refine_times(90.0, 80.0).is_empty());
    }
}
