//! Seek-bar preview textures: owns the [`ThumbService`], the per-video cache
//! of frames (as egui textures) and the request bookkeeping.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use eframe::egui;

use crate::scan_dump::kind_letter;
use crate::state::credits::FrameKind;
use crate::thumbs::{bucket_of, nearest, Reply, ThumbService};

/// Most previews kept per video (240x135 RGBA = 130 KB each).
const MAX_KEPT: usize = 300;

/// Progress of the tail scan of the current video.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanState {
    NotStarted,
    Running,
    /// `(time, kind)` per sampled time, in the order asked (`None` = no frame).
    Done(Vec<(f64, Option<FrameKind>)>),
    /// The second source cannot be opened: no scan for this video.
    Unavailable,
}

pub struct Previews {
    svc: ThumbService,
    /// Changes with every video, so late replies of an earlier one are ignored.
    gen: u64,
    /// The service was asked to open the current video.
    opened: bool,
    /// Opening failed (e.g. a second connection is refused): no more requests.
    unavailable: bool,
    tex: HashMap<i64, egui::TextureHandle>,
    order: VecDeque<i64>,
    /// Buckets whose frame could not be taken.
    bad: HashSet<i64>,
    wanted: Option<i64>,
    scan: ScanState,
    /// Folder the running scan saves its frames in (printed when it finishes).
    scan_dump: Option<PathBuf>,
}

impl Previews {
    pub fn new(user_agent: String, ctx: egui::Context) -> Previews {
        Previews {
            svc: ThumbService::new(user_agent, move || ctx.request_repaint()),
            gen: 0,
            opened: false,
            unavailable: false,
            tex: HashMap::new(),
            order: VecDeque::new(),
            bad: HashSet::new(),
            wanted: None,
            scan: ScanState::NotStarted,
            scan_dump: None,
        }
    }

    /// Another video starts, or playback stops: drop the source and the frames.
    pub fn reset(&mut self) {
        if self.opened {
            self.svc.close();
        }
        self.gen += 1;
        self.opened = false;
        self.unavailable = false;
        self.tex.clear();
        self.order.clear();
        self.bad.clear();
        self.wanted = None;
        self.scan = ScanState::NotStarted;
        self.scan_dump = None;
    }

    /// Adopt finished frames. Every frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        for reply in self.svc.poll() {
            match reply {
                Reply::OpenFailed { gen } if gen == self.gen => self.unavailable = true,
                Reply::Scanned { gen, samples } if gen == self.gen && self.scan == ScanState::Running => {
                    // One letter per sample (C credits, B black, X content, ? no frame): tuning aid.
                    let kinds: String = samples.iter().map(|(_, k)| kind_letter(*k)).collect();
                    let from = samples.first().map_or(0.0, |s| s.0);
                    eprintln!("tail scan: {} samples from {:.0} s: {kinds}", samples.len(), from);
                    if let Some(dir) = self.scan_dump.take() {
                        eprintln!("tail scan frames: {}", dir.display());
                    }
                    self.scan = ScanState::Done(samples);
                }
                Reply::Closed { gen } if gen == self.gen => {
                    // The worker closed the idle connection after the scan: the
                    // next hover opens it again (the scan result is kept; a
                    // request it dropped is asked again).
                    self.opened = false;
                    self.wanted = None;
                }
                Reply::Failed { gen, bucket } if gen == self.gen => {
                    self.bad.insert(bucket);
                }
                Reply::Frame { gen, bucket, thumb } if gen == self.gen => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [thumb.width as usize, thumb.height as usize],
                        &thumb.rgba,
                    );
                    let tex = ctx.load_texture(format!("preview{}", bucket), image, egui::TextureOptions::LINEAR);
                    if self.tex.insert(bucket, tex).is_none() {
                        self.order.push_back(bucket);
                    }
                    while self.order.len() > MAX_KEPT {
                        if let Some(old) = self.order.pop_front() {
                            self.tex.remove(&old);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Start the tail scan of the current video at `times` (opens the second
    /// source if needed). Nothing happens when that source is unavailable.
    /// `duration` = the video's (for the refinement pass); `dump` = folder
    /// to save the sampled frames in (`--console` only).
    pub fn start_scan(&mut self, url: &str, times: Vec<f64>, duration: f64, dump: Option<PathBuf>) {
        if self.unavailable {
            return;
        }
        self.ensure_open(url);
        self.scan_dump = dump.clone();
        self.svc.scan(self.gen, times, duration, dump);
        self.scan = ScanState::Running;
    }

    /// Where the tail scan of the current video stands (see [`Previews::poll`]).
    pub fn scan_result(&mut self) -> ScanState {
        if self.unavailable {
            ScanState::Unavailable
        } else {
            self.scan.clone()
        }
    }

    fn ensure_open(&mut self, url: &str) {
        if !self.opened {
            self.svc.open(self.gen, url);
            self.opened = true;
        }
    }

    /// The preview to show at `secs` (the nearest one taken so far), asking
    /// for the exact one. The first call per video opens the second source.
    pub fn at(&mut self, url: &str, secs: f64) -> Option<&egui::TextureHandle> {
        if self.unavailable {
            return None;
        }
        self.ensure_open(url);
        let bucket = bucket_of(secs);
        if !self.tex.contains_key(&bucket) && !self.bad.contains(&bucket) && self.wanted != Some(bucket) {
            self.svc.want(self.gen, bucket);
            self.wanted = Some(bucket);
        }
        nearest(self.tex.keys().copied(), bucket).and_then(|b| self.tex.get(&b))
    }
}
