//! Seek-bar preview textures: owns the [`ThumbService`], the per-video cache
//! of frames (as egui textures) and the request bookkeeping.

use std::collections::{HashMap, HashSet, VecDeque};

use eframe::egui;

use crate::thumbs::{bucket_of, nearest, Reply, ThumbService};

/// Most previews kept per video (240x135 RGBA = 130 KB each).
const MAX_KEPT: usize = 300;

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
    }

    /// Adopt finished frames. Every frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        for reply in self.svc.poll() {
            match reply {
                Reply::OpenFailed { gen } if gen == self.gen => self.unavailable = true,
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

    /// The preview to show at `secs` (the nearest one taken so far), asking
    /// for the exact one. The first call per video opens the second source.
    pub fn at(&mut self, url: &str, secs: f64) -> Option<&egui::TextureHandle> {
        if self.unavailable {
            return None;
        }
        if !self.opened {
            self.svc.open(self.gen, url);
            self.opened = true;
        }
        let bucket = bucket_of(secs);
        if !self.tex.contains_key(&bucket) && !self.bad.contains(&bucket) && self.wanted != Some(bucket) {
            self.svc.want(self.gen, bucket);
            self.wanted = Some(bucket);
        }
        nearest(self.tex.keys().copied(), bucket).and_then(|b| self.tex.get(&b))
    }
}
