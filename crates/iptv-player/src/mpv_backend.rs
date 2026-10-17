//! libmpv playback backend for [`iptv::supervisor::Supervisor`].
//!
//! Hand-written safe wrapper over `libmpv2-sys` (the high-level `libmpv2`
//! crate drops `playlist_entry_id` from START_FILE/END_FILE events and ties
//! the render context to a borrow of the core, which does not fit a
//! long-lived backend + `Send` paint callback; see AGENTS.md).
//!
//! # Generations
//! Every `load` runs `loadfile <url> replace -1 [start=<secs>]` through
//! `mpv_command_ret`, whose result carries the new `playlist_entry_id`. That
//! id is mapped to the supervisor's generation. START_FILE/END_FILE carry the
//! entry id, so they are attributed exactly; property changes and log
//! messages carry no id and are attributed to the entry between its
//! START_FILE and END_FILE (mpv delivers events in order). An END_FILE for a
//! replaced or stopped entry has reason `stop` and produces nothing.
//!
//! # Events
//! - `Started`: first PLAYBACK_RESTART of the entry (after the initial
//!   `start=` seek, so no Position(0) clobbers a resume point).
//! - `Position`/`Duration`: observed `time-pos`/`duration`.
//! - `Buffering(b)`: `paused-for-cache && !pause && !seeking`, emitted on
//!   change only: a user pause or user seek is never reported as buffering.
//! - END_FILE: [`map_end_file`]: eof -> `Ended` (or `Error(Auth)` if the
//!   stream logged an auth/connection-limit error), error -> one
//!   `Error(class)` classified from the scraped ffmpeg/stream log lines,
//!   stop/quit/redirect -> nothing.
//!
//! # Credentials
//! mpv logs URLs. Only warn+ messages are requested, `msg-level=all=warn`,
//! terminal and log file stay off, and every message is passed through
//! [`scrub`] (known URLs and anything that looks like `scheme://...` are
//! redacted with `iptv::redact::redact_url`) before it is stored.

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use eframe::glow::{self, HasContext};
use iptv::redact::redact_url;
use iptv::supervisor::{Backend, ErrorClass, PlayerEvent};
use libmpv2_sys as sys;

/// mpv options applied before `mpv_initialize`.
pub const BASE_OPTIONS: &[(&str, &str)] = &[
    // Credentials: no terminal, no log file, only warnings and errors.
    ("terminal", "no"),
    ("msg-level", "all=warn"),
    ("config", "no"),
    ("load-scripts", "no"),
    // The built-in ytdl hook would spawn youtube-dl for every http URL.
    ("ytdl", "no"),
    ("osc", "no"),
    ("input-default-bindings", "no"),
    ("input-vo-keyboard", "no"),
    ("idle", "yes"),
    ("keep-open", "no"),
    // First line of defence; the supervisor is the second.
    ("stream-lavf-o", "reconnect=1,reconnect_streamed=1,reconnect_delay_max=3"),
    ("network-timeout", "10"),
    ("cache", "yes"),
    ("demuxer-max-bytes", "64MiB"),
    ("demuxer-max-back-bytes", "16MiB"),
    ("hwdec", "auto-safe"),
    // Do not let mpv_render_context_render block the UI thread.
    ("video-timing-offset", "0"),
];

const OBS_TIME_POS: u64 = 1;
const OBS_DURATION: u64 = 2;
const OBS_PAUSED_FOR_CACHE: u64 = 3;
const OBS_PAUSE: u64 = 4;
const OBS_SEEKING: u64 = 5;

/// Log lines kept per entry for error classification.
const MAX_ENTRY_ERRORS: usize = 12;
/// Scrubbed log lines kept for display.
const MAX_LOG: usize = 64;

/// mpv's `end-file` reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Eof,
    Stop,
    Quit,
    Error,
    Redirect,
    Unknown(u32),
}

impl EndReason {
    pub fn from_raw(r: u32) -> EndReason {
        match r {
            sys::mpv_end_file_reason_MPV_END_FILE_REASON_EOF => EndReason::Eof,
            sys::mpv_end_file_reason_MPV_END_FILE_REASON_STOP => EndReason::Stop,
            sys::mpv_end_file_reason_MPV_END_FILE_REASON_QUIT => EndReason::Quit,
            sys::mpv_end_file_reason_MPV_END_FILE_REASON_ERROR => EndReason::Error,
            sys::mpv_end_file_reason_MPV_END_FILE_REASON_REDIRECT => EndReason::Redirect,
            other => EndReason::Unknown(other),
        }
    }
}

/// Map an mpv `end-file` to the supervisor event (pure).
///
/// `error_text` is the (scrubbed) ffmpeg/stream log text collected for the
/// entry plus mpv's generic error string. A stop/quit (user stop or
/// replacement by a new `loadfile`) and a playlist redirect produce nothing.
pub fn map_end_file(reason: EndReason, error_text: &str) -> Option<PlayerEvent> {
    match reason {
        EndReason::Error => Some(PlayerEvent::Error(ErrorClass::from_message(error_text))),
        // ffmpeg's reconnect gives up with EOF; if the server rejected the
        // account meanwhile, back off like an auth failure.
        EndReason::Eof | EndReason::Unknown(_) => {
            if !error_text.is_empty() && ErrorClass::from_message(error_text) == ErrorClass::Auth {
                Some(PlayerEvent::Error(ErrorClass::Auth))
            } else {
                Some(PlayerEvent::Ended)
            }
        }
        EndReason::Stop | EndReason::Quit | EndReason::Redirect => None,
    }
}

/// True for log prefixes that describe the stream/network (scraped for error
/// classification). Decoder/output prefixes are excluded: their messages
/// contain arbitrary numbers ("MB 401") that must not look like HTTP codes.
pub fn is_stream_log_prefix(prefix: &str) -> bool {
    let p = prefix.to_ascii_lowercase();
    if p == "ffmpeg" || p.starts_with("ffmpeg/demuxer") || p.starts_with("ffmpeg/http")
        || p.starts_with("ffmpeg/tcp") || p.starts_with("ffmpeg/tls")
    {
        return true;
    }
    ["stream", "lavf", "curl", "demux", "cplayer", "file", "http", "tcp", "tls", "network"]
        .iter()
        .any(|k| p == *k || p.starts_with(&format!("{k}/")) || p.starts_with(&format!("{k}_")))
}

/// Redact credentials in a free-form log message: exact occurrences of the
/// `known` URLs (and their path+query), then anything shaped `scheme://...`.
pub fn scrub(text: &str, known: &[&str]) -> String {
    let mut s = text.to_string();
    for u in known.iter().filter(|u| !u.is_empty()) {
        let red = redact_url(u);
        s = s.replace(u, &red);
        if let (Some(raw_path), Some(red_path)) = (path_part(u), path_part(&red)) {
            if raw_path != red_path && raw_path.len() > 1 {
                s = s.replace(raw_path, red_path);
            }
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find("://") {
        let start = rest[..i]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
            .last()
            .map_or(i, |(p, _)| p);
        let end = rest[i..]
            .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>' | '`'))
            .map_or(rest.len(), |p| i + p);
        out.push_str(&rest[..start]);
        out.push_str(&redact_url(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The part of a URL after the authority (`/path?query`), if any.
fn path_part(u: &str) -> Option<&str> {
    let after = &u[u.find("://")? + 3..];
    let slash = after.find('/')?;
    Some(&after[slash..])
}

// ---------------------------------------------------------------------------
// Core handle

type Callback = Box<dyn Fn() + Send + Sync>;

unsafe extern "C" fn call_callback(d: *mut c_void) {
    if !d.is_null() {
        // SAFETY: `d` is a `*mut Callback` we leaked and free only after
        // unregistering it.
        unsafe { (*(d as *const Callback))() };
    }
}

/// Owns the `mpv_handle`. Shared by the backend and the renderer so the core
/// is destroyed only after the render context is freed.
struct Core {
    handle: *mut sys::mpv_handle,
    wakeup: Mutex<*mut Callback>,
}

// SAFETY: the mpv client API is thread-safe for a single handle.
unsafe impl Send for Core {}
unsafe impl Sync for Core {}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe {
            sys::mpv_set_wakeup_callback(self.handle, None, ptr::null_mut());
            sys::mpv_terminate_destroy(self.handle);
            let cb = *self.wakeup.lock().unwrap();
            if !cb.is_null() {
                drop(Box::from_raw(cb));
            }
        }
    }
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("string contains NUL"))
}

fn mpv_err(code: c_int) -> String {
    unsafe { CStr::from_ptr(sys::mpv_error_string(code)) }.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Rendering

/// OpenGL setup for in-window rendering (`vo=libmpv`).
pub struct GlInit {
    /// Resolves GL functions (eframe: `CreationContext::get_proc_address`).
    /// Only used while the render context is created.
    pub get_proc_address: Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>,
    /// Called (from an mpv thread) when a new frame should be drawn.
    pub on_frame: Callback,
}

struct GlTarget {
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    w: i32,
    h: i32,
}

struct RenderState {
    ctx: *mut sys::mpv_render_context,
    update_cb: *mut Callback,
    target: Option<GlTarget>,
}

/// The libmpv OpenGL render context. All methods must be called on the thread
/// that owns the current GL context (eframe's UI thread / paint callback).
pub struct MpvRenderer {
    state: Mutex<RenderState>,
    _core: Arc<Core>,
}

// SAFETY: the raw pointers are only used with the GL context current, on the
// UI thread; the Mutex serialises access.
unsafe impl Send for MpvRenderer {}
unsafe impl Sync for MpvRenderer {}

unsafe extern "C" fn gpa_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    if ctx.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: ctx points at the `GlInit::get_proc_address` Arc, alive for the
    // duration of mpv_render_context_create (the only time mpv calls this).
    let f = unsafe { &*(ctx as *const Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>) };
    f(unsafe { CStr::from_ptr(name) }) as *mut c_void
}

impl MpvRenderer {
    fn new(core: Arc<Core>, gl: GlInit) -> Result<MpvRenderer> {
        let mut init = sys::mpv_opengl_init_params {
            get_proc_address: Some(gpa_trampoline),
            get_proc_address_ctx: &gl.get_proc_address as *const _ as *mut c_void,
        };
        let mut params = [
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut _ as *mut c_void,
            },
            sys::mpv_render_param { type_: 0, data: ptr::null_mut() },
        ];
        let mut ctx: *mut sys::mpv_render_context = ptr::null_mut();
        let r = unsafe { sys::mpv_render_context_create(&mut ctx, core.handle, params.as_mut_ptr()) };
        if r < 0 || ctx.is_null() {
            bail!("mpv_render_context_create failed: {}", mpv_err(r));
        }
        let update_cb: *mut Callback = Box::into_raw(Box::new(gl.on_frame));
        unsafe {
            sys::mpv_render_context_set_update_callback(ctx, Some(call_callback), update_cb as *mut c_void);
        }
        Ok(MpvRenderer { state: Mutex::new(RenderState { ctx, update_cb, target: None }), _core: core })
    }

    /// Render the current video frame into framebuffer `fbo` (0 = default)
    /// of size `w`x`h`, flipped for OpenGL's bottom-left origin.
    pub fn paint(&self, w: u32, h: u32, fbo: i32) {
        let st = self.state.lock().unwrap();
        unsafe { render_to(st.ctx, w as i32, h as i32, fbo) };
    }

    /// Paint inside an `egui_glow` paint callback: render into an offscreen
    /// FBO the size of the callback rect, then blit it into the rect. The
    /// FBO avoids mpv drawing over the whole window (it sets its own
    /// viewport to the full target).
    pub fn paint_callback(&self, info: &eframe::egui::PaintCallbackInfo, painter: &eframe::egui_glow::Painter) {
        let vp = info.viewport_in_pixels();
        let (w, h) = (vp.width_px, vp.height_px);
        if w <= 0 || h <= 0 {
            return;
        }
        let gl = painter.gl();
        let mut st = self.state.lock().unwrap();
        if st.ctx.is_null() {
            return;
        }
        unsafe {
            if st.target.as_ref().is_none_or(|t| t.w != w || t.h != h) {
                if let Some(t) = st.target.take() {
                    gl.delete_framebuffer(t.fbo);
                    gl.delete_texture(t.tex);
                }
                match create_target(gl, w, h) {
                    Ok(t) => st.target = Some(t),
                    Err(e) => {
                        eprintln!("video target: {e}");
                        return;
                    }
                }
            }
            let t = st.target.as_ref().unwrap();
            render_to(st.ctx, w, h, t.fbo.0.get() as i32);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(t.fbo));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, painter.intermediate_fbo());
            gl.blit_framebuffer(
                0, 0, w, h,
                vp.left_px, vp.from_bottom_px, vp.left_px + w, vp.from_bottom_px + h,
                glow::COLOR_BUFFER_BIT, glow::NEAREST,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, painter.intermediate_fbo());
        }
    }

    /// Free the render context and GL objects. Call with the GL context
    /// current (eframe `App::on_exit`). Idempotent.
    pub fn destroy(&self, gl: Option<&glow::Context>) {
        let mut st = self.state.lock().unwrap();
        if let (Some(gl), Some(t)) = (gl, st.target.take()) {
            unsafe {
                gl.delete_framebuffer(t.fbo);
                gl.delete_texture(t.tex);
            }
        }
        if !st.ctx.is_null() {
            unsafe { sys::mpv_render_context_free(st.ctx) };
            st.ctx = ptr::null_mut();
        }
        if !st.update_cb.is_null() {
            unsafe { drop(Box::from_raw(st.update_cb)) };
            st.update_cb = ptr::null_mut();
        }
    }
}

impl Drop for MpvRenderer {
    fn drop(&mut self) {
        // Fallback; the GL objects leak if `destroy` was not called.
        self.destroy(None);
    }
}

unsafe fn render_to(ctx: *mut sys::mpv_render_context, w: i32, h: i32, fbo: i32) {
    if ctx.is_null() {
        return;
    }
    let mut target = sys::mpv_opengl_fbo { fbo, w, h, internal_format: 0 };
    let mut flip: c_int = 1;
    let mut params = [
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
            data: &mut target as *mut _ as *mut c_void,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
            data: &mut flip as *mut _ as *mut c_void,
        },
        sys::mpv_render_param { type_: 0, data: ptr::null_mut() },
    ];
    unsafe { sys::mpv_render_context_render(ctx, params.as_mut_ptr()) };
}

unsafe fn create_target(gl: &glow::Context, w: i32, h: i32) -> Result<GlTarget, String> {
    unsafe {
        let tex = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.bind_texture(glow::TEXTURE_2D, None);
        let fbo = gl.create_framebuffer()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffer(fbo);
            gl.delete_texture(tex);
            return Err(format!("framebuffer incomplete: {status:#x}"));
        }
        Ok(GlTarget { fbo, tex, w, h })
    }
}

// ---------------------------------------------------------------------------
// Backend

/// Per-entry state between START_FILE and END_FILE.
struct Current {
    entry: i64,
    generation: u32,
    started: bool,
    buffering: bool,
    errors: Vec<String>,
}

/// An END_FILE waiting for its late log lines.
struct Ending {
    generation: u32,
    reason: EndReason,
    errors: Vec<String>,
    deadline: Instant,
}

/// How long an error/EOF END_FILE waits for late log lines before it is
/// mapped. Well below the shortest retry backoff (500 ms).
pub const END_GRACE: Duration = Duration::from_millis(250);

/// Construction options.
#[derive(Default)]
pub struct MpvOptions {
    /// `Some` = render in-app via the OpenGL render API (`vo=libmpv`);
    /// `None` = headless (`vo=null`, `ao=null`; tests and demos).
    pub gl: Option<GlInit>,
    /// Called (from an mpv thread) when events are pending.
    pub on_wakeup: Option<Callback>,
}

pub struct MpvBackend {
    core: Arc<Core>,
    renderer: Option<Arc<MpvRenderer>>,
    /// playlist_entry_id -> generation, for loads whose END_FILE is pending.
    entries: HashMap<i64, u32>,
    current: Option<Current>,
    /// Ended entries waiting for late log lines (see [`END_GRACE`]).
    ending: Vec<Ending>,
    /// Events produced outside the mpv event queue (failed loadfile).
    pending: Vec<(u32, PlayerEvent)>,
    /// Recently loaded URLs, for scrubbing (old entries can still log).
    urls: VecDeque<String>,
    log: VecDeque<String>,
    pause: bool,
    seeking: bool,
    paused_for_cache: bool,
}

impl std::fmt::Debug for MpvBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvBackend").field("entries", &self.entries.len()).finish_non_exhaustive()
    }
}

impl MpvBackend {
    pub fn new(opts: MpvOptions) -> Result<MpvBackend> {
        let api = unsafe { sys::mpv_client_api_version() } as u64;
        if api >> 16 != 2 {
            bail!("unsupported libmpv client API version {}.{}", api >> 16, api & 0xffff);
        }
        let handle = unsafe { sys::mpv_create() };
        if handle.is_null() {
            bail!("mpv_create failed");
        }
        let core = Arc::new(Core { handle, wakeup: Mutex::new(ptr::null_mut()) });
        let headless = opts.gl.is_none();
        let mut options: Vec<(&str, &str)> = BASE_OPTIONS.to_vec();
        if headless {
            options.extend([("vo", "null"), ("ao", "null")]);
        } else {
            options.push(("vo", "libmpv"));
        }
        for (k, v) in options {
            let (ck, cv) = (cstr(k)?, cstr(v)?);
            let r = unsafe { sys::mpv_set_option_string(handle, ck.as_ptr(), cv.as_ptr()) };
            if r < 0 {
                bail!("mpv option {k}={v}: {}", mpv_err(r));
            }
        }
        let r = unsafe { sys::mpv_initialize(handle) };
        if r < 0 {
            bail!("mpv_initialize: {}", mpv_err(r));
        }
        let level = cstr("warn")?;
        unsafe { sys::mpv_request_log_messages(handle, level.as_ptr()) };
        for (id, name, fmt) in [
            (OBS_TIME_POS, "time-pos", sys::mpv_format_MPV_FORMAT_DOUBLE),
            (OBS_DURATION, "duration", sys::mpv_format_MPV_FORMAT_DOUBLE),
            (OBS_PAUSED_FOR_CACHE, "paused-for-cache", sys::mpv_format_MPV_FORMAT_FLAG),
            (OBS_PAUSE, "pause", sys::mpv_format_MPV_FORMAT_FLAG),
            (OBS_SEEKING, "seeking", sys::mpv_format_MPV_FORMAT_FLAG),
        ] {
            let n = cstr(name)?;
            let r = unsafe { sys::mpv_observe_property(handle, id, n.as_ptr(), fmt) };
            if r < 0 {
                bail!("observe {name}: {}", mpv_err(r));
            }
        }
        if let Some(cb) = opts.on_wakeup {
            let raw: *mut Callback = Box::into_raw(Box::new(cb));
            *core.wakeup.lock().unwrap() = raw;
            unsafe { sys::mpv_set_wakeup_callback(handle, Some(call_callback), raw as *mut c_void) };
        }
        let renderer = match opts.gl {
            Some(gl) => Some(Arc::new(MpvRenderer::new(core.clone(), gl)?)),
            None => None,
        };
        Ok(MpvBackend {
            core,
            renderer,
            entries: HashMap::new(),
            current: None,
            ending: Vec::new(),
            pending: Vec::new(),
            urls: VecDeque::new(),
            log: VecDeque::new(),
            pause: false,
            seeking: false,
            paused_for_cache: false,
        })
    }

    /// mpv version string (e.g. "mpv v0.40.0-...").
    pub fn mpv_version(&self) -> String {
        let name = CString::new("mpv-version").unwrap();
        unsafe {
            let p = sys::mpv_get_property_string(self.core.handle, name.as_ptr());
            if p.is_null() {
                return String::from("?");
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            sys::mpv_free(p as *mut c_void);
            s
        }
    }

    /// The render context (in-app mode only), for use in paint callbacks.
    pub fn renderer(&self) -> Option<Arc<MpvRenderer>> {
        self.renderer.clone()
    }

    /// Render the current frame into `fbo` (see [`MpvRenderer::paint`]).
    pub fn paint(&mut self, w: u32, h: u32, fbo: i32) {
        if let Some(r) = &self.renderer {
            r.paint(w, h, fbo);
        }
    }

    /// Toggle the user pause (never reported as buffering).
    pub fn set_pause(&mut self, pause: bool) {
        let (n, v) = (CString::new("pause").unwrap(), CString::new(if pause { "yes" } else { "no" }).unwrap());
        unsafe { sys::mpv_set_property_string(self.core.handle, n.as_ptr(), v.as_ptr()) };
    }

    /// Drain scrubbed warn/error log lines.
    pub fn take_log(&mut self) -> Vec<String> {
        self.log.drain(..).collect()
    }

    /// Drain mpv's event queue (non-blocking) and map it to supervisor events,
    /// each tagged with the generation of the load that caused it.
    pub fn poll_events(&mut self) -> Vec<(u32, PlayerEvent)> {
        let mut out = std::mem::take(&mut self.pending);
        loop {
            let ev = unsafe { &*sys::mpv_wait_event(self.core.handle, 0.0) };
            match ev.event_id {
                sys::mpv_event_id_MPV_EVENT_NONE => break,
                sys::mpv_event_id_MPV_EVENT_START_FILE => {
                    let d = unsafe { &*(ev.data as *const sys::mpv_event_start_file) };
                    // A new file begins: earlier endings get no more lines.
                    self.flush_endings(None, &mut out);
                    self.current = self.entries.get(&d.playlist_entry_id).map(|&generation| Current {
                        entry: d.playlist_entry_id,
                        generation,
                        started: false,
                        buffering: false,
                        errors: Vec::new(),
                    });
                }
                sys::mpv_event_id_MPV_EVENT_PLAYBACK_RESTART => {
                    if let Some(c) = self.current.as_mut() {
                        if !c.started {
                            c.started = true;
                            out.push((c.generation, PlayerEvent::Started));
                        }
                    }
                }
                sys::mpv_event_id_MPV_EVENT_END_FILE => {
                    let d = unsafe { &*(ev.data as *const sys::mpv_event_end_file) };
                    let generation = self.entries.remove(&d.playlist_entry_id);
                    let mut errors = Vec::new();
                    if self.current.as_ref().is_some_and(|c| c.entry == d.playlist_entry_id) {
                        errors = self.current.take().unwrap().errors;
                    }
                    let reason = EndReason::from_raw(d.reason);
                    if let (Some(generation), Some(_)) = (generation, map_end_file(reason, "")) {
                        if reason == EndReason::Error && d.error < 0 {
                            errors.push(mpv_err(d.error));
                        }
                        // The log lines explaining the failure can arrive
                        // after END_FILE: map it once they had time to come.
                        self.ending.push(Ending {
                            generation,
                            reason,
                            errors,
                            deadline: Instant::now() + END_GRACE,
                        });
                    }
                }
                sys::mpv_event_id_MPV_EVENT_PROPERTY_CHANGE => {
                    let p = unsafe { &*(ev.data as *const sys::mpv_event_property) };
                    self.on_property(ev.reply_userdata, p, &mut out);
                }
                sys::mpv_event_id_MPV_EVENT_LOG_MESSAGE => {
                    let m = unsafe { &*(ev.data as *const sys::mpv_event_log_message) };
                    let s = |p: *const c_char| {
                        if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
                    };
                    let (prefix, level, text) = (s(m.prefix), s(m.level), s(m.text));
                    self.on_log(&prefix, &level, &text);
                }
                _ => {}
            }
        }
        self.flush_endings(Some(Instant::now()), &mut out);
        out
    }

    /// Emit endings whose grace period is over (all of them if `now` is None).
    fn flush_endings(&mut self, now: Option<Instant>, out: &mut Vec<(u32, PlayerEvent)>) {
        let (due, keep): (Vec<Ending>, Vec<Ending>) =
            std::mem::take(&mut self.ending).into_iter().partition(|e| now.is_none_or(|n| n >= e.deadline));
        self.ending = keep;
        for e in due {
            if let Some(ev) = map_end_file(e.reason, &e.errors.join(" | ")) {
                out.push((e.generation, ev));
            }
        }
    }

    fn on_property(&mut self, id: u64, p: &sys::mpv_event_property, out: &mut Vec<(u32, PlayerEvent)>) {
        let double = || (p.format == sys::mpv_format_MPV_FORMAT_DOUBLE && !p.data.is_null())
            .then(|| unsafe { *(p.data as *const f64) });
        let flag = || p.format == sys::mpv_format_MPV_FORMAT_FLAG && !p.data.is_null()
            && unsafe { *(p.data as *const c_int) } != 0;
        match id {
            OBS_TIME_POS => {
                if let (Some(c), Some(v)) = (&self.current, double()) {
                    out.push((c.generation, PlayerEvent::Position(v)));
                }
            }
            OBS_DURATION => {
                if let (Some(c), Some(v)) = (&self.current, double()) {
                    out.push((c.generation, PlayerEvent::Duration(v)));
                }
            }
            OBS_PAUSED_FOR_CACHE => self.paused_for_cache = flag(),
            OBS_PAUSE => self.pause = flag(),
            OBS_SEEKING => self.seeking = flag(),
            _ => return,
        }
        if matches!(id, OBS_PAUSED_FOR_CACHE | OBS_PAUSE | OBS_SEEKING) {
            let buffering = effective_buffering(self.paused_for_cache, self.pause, self.seeking);
            if let Some(c) = self.current.as_mut() {
                if c.buffering != buffering {
                    c.buffering = buffering;
                    out.push((c.generation, PlayerEvent::Buffering(buffering)));
                }
            }
        }
    }

    fn on_log(&mut self, prefix: &str, level: &str, text: &str) {
        let known: Vec<&str> = self.urls.iter().map(String::as_str).collect();
        let clean = scrub(text.trim_end(), &known);
        let prefix = scrub(prefix, &known);
        if self.log.len() >= MAX_LOG {
            self.log.pop_front();
        }
        self.log.push_back(format!("[{prefix}] {level}: {clean}"));
        if is_stream_log_prefix(&prefix) {
            // Between START_FILE and END_FILE lines belong to the playing
            // entry; after END_FILE (no current entry) to the latest ending.
            let errors = match (self.current.as_mut(), self.ending.last_mut()) {
                (Some(c), _) => Some(&mut c.errors),
                (None, Some(e)) => Some(&mut e.errors),
                (None, None) => None,
            };
            if let Some(errors) = errors {
                if errors.len() >= MAX_ENTRY_ERRORS {
                    errors.remove(0);
                }
                errors.push(clean);
            }
        }
    }
}

/// Buffering as reported to the supervisor: cache underrun only, never a
/// user pause or a user seek.
pub fn effective_buffering(paused_for_cache: bool, pause: bool, seeking: bool) -> bool {
    paused_for_cache && !pause && !seeking
}

/// Format the per-file `start` option (seconds, no exponent).
pub fn start_option(start_at: Option<f64>) -> Option<String> {
    start_at.filter(|s| s.is_finite() && *s > 0.0).map(|s| format!("start={s:.3}"))
}

impl Backend for MpvBackend {
    fn load(&mut self, url: &str, start_at: Option<f64>, generation: u32) {
        if !self.urls.iter().any(|u| u == url) {
            if self.urls.len() >= 4 {
                self.urls.pop_front();
            }
            self.urls.push_back(url.to_string());
        }
        let mut args: Vec<CString> = Vec::new();
        let ok = (|| -> Result<()> {
            args.push(cstr("loadfile")?);
            args.push(cstr(url)?);
            args.push(cstr("replace")?);
            args.push(cstr("-1")?);
            if let Some(opt) = start_option(start_at) {
                args.push(cstr(&opt)?);
            }
            Ok(())
        })();
        if ok.is_err() {
            self.pending.push((generation, PlayerEvent::Error(ErrorClass::Other)));
            return;
        }
        let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        let mut res: sys::mpv_node = unsafe { std::mem::zeroed() };
        let r = unsafe { sys::mpv_command_ret(self.core.handle, argv.as_mut_ptr(), &mut res) };
        if r < 0 {
            self.log.push_back(format!("[backend] error: loadfile failed: {}", mpv_err(r)));
            self.pending.push((generation, PlayerEvent::Error(ErrorClass::Other)));
            return;
        }
        let id = unsafe { node_map_int(&res, "playlist_entry_id") };
        unsafe { sys::mpv_free_node_contents(&mut res) };
        match id {
            Some(id) => {
                self.entries.insert(id, generation);
            }
            None => {
                self.log.push_back("[backend] error: loadfile returned no playlist_entry_id".into());
                self.pending.push((generation, PlayerEvent::Error(ErrorClass::Other)));
            }
        }
    }

    fn stop(&mut self) {
        let a = CString::new("stop").unwrap();
        let mut argv = [a.as_ptr(), ptr::null()];
        unsafe { sys::mpv_command(self.core.handle, argv.as_mut_ptr()) };
        // END_FILE(reason=stop) follows and produces nothing.
        self.pending.clear();
        self.ending.clear();
    }
}

/// Read an INT64 value from an mpv node map.
unsafe fn node_map_int(node: &sys::mpv_node, key: &str) -> Option<i64> {
    if node.format != sys::mpv_format_MPV_FORMAT_NODE_MAP {
        return None;
    }
    unsafe {
        let list = node.u.list;
        if list.is_null() {
            return None;
        }
        let list = &*list;
        for i in 0..list.num.max(0) as usize {
            let k = CStr::from_ptr(*list.keys.add(i));
            if k.to_bytes() == key.as_bytes() {
                let v = &*list.values.add(i);
                if v.format == sys::mpv_format_MPV_FORMAT_INT64 {
                    return Some(v.u.int64);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_mapping() {
        use ErrorClass::*;
        let e = |r, t| map_end_file(r, t);
        assert_eq!(e(EndReason::Error, "http: HTTP error 401 Unauthorized | loading failed"), Some(PlayerEvent::Error(Auth)));
        assert_eq!(
            e(EndReason::Error, "Server returned 4XX Client Error, but not one of 40{0,1,3,4}"),
            Some(PlayerEvent::Error(Auth))
        );
        assert_eq!(e(EndReason::Error, "Max connections reached"), Some(PlayerEvent::Error(Auth)));
        assert_eq!(
            e(EndReason::Error, "tcp: Connection to tcp://127.0.0.1:8099 failed: Error number -138 occurred | Failed to open http://127.0.0.1:8099/live. | loading failed"),
            Some(PlayerEvent::Error(Network))
        );
        assert_eq!(e(EndReason::Error, "Connection refused"), Some(PlayerEvent::Error(Network)));
        assert_eq!(e(EndReason::Error, "Connection timed out | loading failed"), Some(PlayerEvent::Error(Network)));
        assert_eq!(e(EndReason::Error, "loading failed"), Some(PlayerEvent::Error(Other)));
        assert_eq!(e(EndReason::Error, ""), Some(PlayerEvent::Error(Other)));
        // Normal EOF.
        assert_eq!(e(EndReason::Eof, ""), Some(PlayerEvent::Ended));
        assert_eq!(e(EndReason::Eof, "Connection reset by peer"), Some(PlayerEvent::Ended));
        // Reconnect gave up after the server rejected the account.
        assert_eq!(e(EndReason::Eof, "HTTP error 458"), Some(PlayerEvent::Error(Auth)));
        assert_eq!(e(EndReason::Unknown(99), ""), Some(PlayerEvent::Ended));
        // User stop / replacement / playlist redirect: nothing.
        assert_eq!(e(EndReason::Stop, "HTTP error 401"), None);
        assert_eq!(e(EndReason::Quit, ""), None);
        assert_eq!(e(EndReason::Redirect, ""), None);
    }

    #[test]
    fn end_reason_from_raw() {
        assert_eq!(EndReason::from_raw(0), EndReason::Eof);
        assert_eq!(EndReason::from_raw(2), EndReason::Stop);
        assert_eq!(EndReason::from_raw(3), EndReason::Quit);
        assert_eq!(EndReason::from_raw(4), EndReason::Error);
        assert_eq!(EndReason::from_raw(5), EndReason::Redirect);
        assert_eq!(EndReason::from_raw(1), EndReason::Unknown(1));
    }

    #[test]
    fn scrub_redacts_known_and_embedded_urls() {
        let url = "http://h.example:8080/live/fakeuser/fakepass/123.ts";
        let msg = format!("Failed to open {url}.");
        let s = scrub(&msg, &[url]);
        assert!(!s.contains("fakeuser") && !s.contains("fakepass"), "{s}");
        assert!(s.contains("http://h.example:8080/live/***/***/123.ts"), "{s}");
        // Unknown URL, quoted, with userinfo and query credentials.
        let s = scrub("Opening 'https://u:p@x.test/get.php?username=a&password=b' now", &[]);
        assert_eq!(s, "Opening 'https://***@x.test/get.php?username=***&password=***' now");
        // Bare path of a known URL (e.g. an HTTP request line).
        let s = scrub("GET /fakeuser/fakepass/9 HTTP/1.1", &["http://h/fakeuser/fakepass/9"]);
        assert!(!s.contains("fakepass"), "{s}");
        // Non-URL text and multibyte characters are untouched.
        assert_eq!(scrub("HTTP error 401 Unauthorized é", &[]), "HTTP error 401 Unauthorized é");
        assert_eq!(scrub("é://x", &[]), "é://x");
    }

    #[test]
    fn stream_prefixes_only() {
        for p in ["ffmpeg", "ffmpeg/demuxer", "stream", "lavf", "curl", "demux", "cplayer", "stream_lavf"] {
            assert!(is_stream_log_prefix(p), "{p}");
        }
        for p in ["ffmpeg/video", "ffmpeg/audio", "vd", "ad", "vo/gpu", "ao/wasapi", "sub", "ytdl_hook"] {
            assert!(!is_stream_log_prefix(p), "{p}");
        }
    }

    #[test]
    fn pause_and_seek_are_not_buffering() {
        assert!(effective_buffering(true, false, false));
        assert!(!effective_buffering(true, true, false), "user pause");
        assert!(!effective_buffering(true, false, true), "user seek");
        assert!(!effective_buffering(false, true, false));
        assert!(!effective_buffering(false, false, false));
    }

    #[test]
    fn start_option_formats() {
        assert_eq!(start_option(None), None);
        assert_eq!(start_option(Some(0.0)), None);
        assert_eq!(start_option(Some(f64::NAN)), None);
        assert_eq!(start_option(Some(1234.5)), Some("start=1234.500".into()));
        assert_eq!(start_option(Some(1e7)), Some("start=10000000.000".into()));
    }
}
