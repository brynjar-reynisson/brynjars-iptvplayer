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
//! - END_FILE: [`map_end_file`]: eof -> `Ended` (or `Error(Auth)` /
//!   `Error(Unavailable)` if the stream logged an auth/connection-limit or
//!   404/407/410 error), error -> one
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
///
/// URLs and decimal numbers are neutralised first ([`neutralize_for_classification`])
/// so a stream id like `/401.ts` or a timestamp like `458.200` never reads as
/// an HTTP status.
pub fn map_end_file(reason: EndReason, error_text: &str) -> Option<PlayerEvent> {
    let text = neutralize_for_classification(error_text);
    match reason {
        EndReason::Error => Some(PlayerEvent::Error(ErrorClass::from_message(&text))),
        // A reconnect that gives up ends with EOF; if the server rejected the
        // account or took the channel off air meanwhile, back off (and
        // report) like that failure.
        EndReason::Eof | EndReason::Unknown(_) => {
            let class = if text.trim().is_empty() { ErrorClass::Other } else { ErrorClass::from_message(&text) };
            match class {
                ErrorClass::Auth | ErrorClass::Unavailable => Some(PlayerEvent::Error(class)),
                ErrorClass::Network | ErrorClass::Other => Some(PlayerEvent::Ended),
            }
        }
        EndReason::Stop | EndReason::Quit | EndReason::Redirect => None,
    }
}

/// Replace every URL-ish token (contains `://`, or a redacted path `/***`)
/// with `<url>` and every decimal number (`458.200`) with `<num>`, so only
/// the wording and real status codes are left for `ErrorClass::from_message`.
pub fn neutralize_for_classification(text: &str) -> String {
    let is_sep = |c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>' | '`' | '(' | ')' | ',');
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.contains("://") || token.contains("/***") {
            out.push_str("<url>");
        } else {
            out.push_str(&neutralize_decimals(token));
        }
        token.clear();
    };
    for c in text.chars() {
        if is_sep(c) {
            flush(&mut token, &mut out);
            out.push(c);
        } else {
            token.push(c);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// `458.200` -> `<num>`; integers are left alone.
fn neutralize_decimals(token: &str) -> String {
    let b = token.as_bytes();
    let mut out = String::with_capacity(token.len());
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                out.push_str("<num>");
            } else {
                out.push_str(&token[start..i]);
            }
        } else {
            let ch = token[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// True for log prefixes that describe the stream/network (scraped for error
/// classification). Decoder/output/player prefixes (`ffmpeg/video`, `vd`,
/// `cplayer`, ...) are excluded: their messages contain arbitrary numbers
/// ("MB 401", "pts 458.2") that must not look like HTTP codes.
pub fn is_stream_log_prefix(prefix: &str) -> bool {
    let p = prefix.to_ascii_lowercase();
    if p == "ffmpeg" || p.starts_with("ffmpeg/demuxer") || p.starts_with("ffmpeg/http")
        || p.starts_with("ffmpeg/tcp") || p.starts_with("ffmpeg/tls")
    {
        return true;
    }
    ["stream", "lavf", "curl", "demux", "http", "tcp", "tls", "network"]
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
    /// Size whose FBO creation failed (logged once, not retried every frame).
    failed_size: Option<(i32, i32)>,
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
        Ok(MpvRenderer { state: Mutex::new(RenderState { ctx, update_cb, target: None, failed_size: None }), _core: core })
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
                if st.failed_size == Some((w, h)) {
                    return;
                }
                match create_target(gl, w, h) {
                    Ok(t) => {
                        st.target = Some(t);
                        st.failed_size = None;
                    }
                    Err(e) => {
                        // create_target may have left its FBO bound.
                        gl.bind_framebuffer(glow::FRAMEBUFFER, painter.intermediate_fbo());
                        eprintln!("video target {w}x{h}: {e}"); // once per size
                        st.failed_size = Some((w, h));
                        return;
                    }
                }
            }
            let t = st.target.as_ref().unwrap();
            // egui calls us with SCISSOR_TEST on and the scissor box set to the
            // clip rect in *window* coordinates; inside our offscreen FBO
            // (origin 0,0) that would clip mpv's render for any video rect not
            // at the window origin.
            gl.disable(glow::SCISSOR_TEST);
            render_to(st.ctx, w, h, t.fbo.0.get() as i32);
            // State egui does not reset in prepare_painting (it re-sets blend,
            // viewport, program, VAO, scissor enable, sRGB): pixel-store state
            // used by its texture uploads and the bound unpack buffer.
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
            gl.active_texture(glow::TEXTURE0);
            // The blit honours the scissor: clip it to egui's clip rect.
            let clip = info.clip_rect_in_pixels();
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(clip.left_px, clip.from_bottom_px, clip.width_px, clip.height_px);
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
        let leaked = !self.state.lock().map(|s| s.ctx.is_null()).unwrap_or(true);
        if leaked {
            eprintln!("warning: MpvRenderer dropped without destroy(); call it in App::on_exit with the GL context current");
        }
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

/// How long an error/EOF END_FILE waits for late log lines before it is
/// mapped. Well below the shortest retry backoff (500 ms).
pub const END_GRACE: Duration = Duration::from_millis(250);

/// After an entry ends (any reason), stream log lines that arrive within this
/// window after the next START_FILE are taken to be the old entry's late
/// lines and are not attributed to the new entry (mpv delivers log messages
/// only when the regular event queue is empty). Shorter than the minimum
/// retry gap (`END_GRACE` + 500 ms backoff), so automatic retries are never
/// affected; only a user switch / watchdog replacement is.
pub const ATTRIBUTION_WINDOW: Duration = Duration::from_millis(500);

/// Before the first PLAYBACK_RESTART of a load with `start=s`, only
/// `time-pos` values in `[s, s + PRE_RESTART_POS_WINDOW]` are passed on (any
/// other value can be the previous file's). Loads without `start=` pass none.
pub const PRE_RESTART_POS_WINDOW: f64 = 30.0;

/// One mpv event, already decoded and scrubbed, as fed to [`EventMapper`].
#[derive(Debug, Clone, PartialEq)]
pub enum RawEvent {
    StartFile(i64),
    PlaybackRestart,
    /// `error` is mpv's generic error string for reason=error.
    EndFile { entry: i64, reason: EndReason, error: Option<String> },
    /// `None` = property unavailable.
    TimePos(Option<f64>),
    Duration(Option<f64>),
    PausedForCache(bool),
    Pause(bool),
    Seeking(bool),
    /// A scrubbed warn/error log line.
    Log { prefix: String, text: String },
}

/// A load whose END_FILE is still pending.
struct Entry {
    generation: u32,
    start_at: Option<f64>,
}

/// Per-entry state between START_FILE and END_FILE.
struct Current {
    entry: i64,
    generation: u32,
    start_at: Option<f64>,
    started: bool,
    buffering: bool,
    /// Stream log lines of this entry (cleared at PLAYBACK_RESTART, so an
    /// EOF->Auth/Unavailable upgrade only uses lines logged while playing).
    errors: Vec<String>,
    /// Latest duration seen before PLAYBACK_RESTART, emitted at restart.
    pending_duration: Option<f64>,
    /// Stream lines before this instant belong to the previous entry. Runs
    /// until it expires (also through PLAYBACK_RESTART and into the ending).
    quarantine_until: Option<Instant>,
}

/// An END_FILE waiting for its late log lines.
struct Ending {
    generation: u32,
    reason: EndReason,
    errors: Vec<String>,
    deadline: Instant,
    /// Inherited from the entry: lines before this are the previous entry's.
    quarantine_until: Option<Instant>,
}

/// Pure attribution of mpv events to supervisor generations (no FFI, time
/// passed in), so the ordering rules are unit-testable.
#[derive(Default)]
pub struct EventMapper {
    entries: HashMap<i64, Entry>,
    current: Option<Current>,
    ending: Vec<Ending>,
    last_end: Option<Instant>,
    pause: bool,
    seeking: bool,
    paused_for_cache: bool,
}

impl EventMapper {
    /// `loadfile` returned `entry` for this generation.
    pub fn loaded(&mut self, entry: i64, generation: u32, start_at: Option<f64>) {
        self.entries.insert(entry, Entry { generation, start_at });
    }

    /// User stop: nothing pending may be reported any more.
    pub fn stopped(&mut self) {
        self.ending.clear();
    }

    /// Number of loads whose END_FILE is pending (for tests/diagnostics).
    pub fn pending_entries(&self) -> usize {
        self.entries.len()
    }

    pub fn handle(&mut self, ev: RawEvent, now: Instant, out: &mut Vec<(u32, PlayerEvent)>) {
        match ev {
            RawEvent::StartFile(id) => {
                // A new file begins: earlier endings get no more lines.
                self.flush(None, out);
                // Ids grow; older ids that never ended are dead (replaced).
                self.entries.retain(|&k, _| k >= id);
                // Cache/seek state belongs to the previous file.
                self.paused_for_cache = false;
                self.seeking = false;
                let quarantine_until = self.last_end.map(|t| t + ATTRIBUTION_WINDOW).filter(|q| now < *q);
                self.current = self.entries.get(&id).map(|e| Current {
                    entry: id,
                    generation: e.generation,
                    start_at: e.start_at,
                    started: false,
                    buffering: false,
                    errors: Vec::new(),
                    pending_duration: None,
                    quarantine_until,
                });
            }
            RawEvent::PlaybackRestart => {
                if let Some(c) = self.current.as_mut() {
                    if !c.started {
                        c.started = true;
                        c.errors.clear();
                        out.push((c.generation, PlayerEvent::Started));
                        if let Some(d) = c.pending_duration.take() {
                            out.push((c.generation, PlayerEvent::Duration(d)));
                        }
                    }
                }
            }
            RawEvent::EndFile { entry, reason, error } => {
                self.last_end = Some(now);
                let generation = self.entries.remove(&entry).map(|e| e.generation);
                let mut errors = Vec::new();
                let mut quarantine_until = None;
                if self.current.as_ref().is_some_and(|c| c.entry == entry) {
                    let c = self.current.take().unwrap();
                    errors = c.errors;
                    quarantine_until = c.quarantine_until;
                }
                if let (Some(generation), Some(_)) = (generation, map_end_file(reason, "")) {
                    if reason == EndReason::Error {
                        errors.extend(error);
                    }
                    // The log lines explaining the failure can arrive after
                    // END_FILE: map it once they had time to come.
                    self.ending.push(Ending { generation, reason, errors, deadline: now + END_GRACE, quarantine_until });
                }
            }
            RawEvent::TimePos(v) => {
                if let (Some(c), Some(v)) = (&self.current, v) {
                    // Before the first restart a time-pos can still be the
                    // old file's: only accept it if it is just at/after start=.
                    if c.started || c.start_at.is_some_and(|s| v >= s && v <= s + PRE_RESTART_POS_WINDOW) {
                        out.push((c.generation, PlayerEvent::Position(v)));
                    }
                }
            }
            RawEvent::Duration(v) => {
                if let Some(c) = self.current.as_mut() {
                    if c.started {
                        if let Some(v) = v {
                            out.push((c.generation, PlayerEvent::Duration(v)));
                        }
                    } else {
                        c.pending_duration = v;
                    }
                }
            }
            RawEvent::PausedForCache(b) => self.set_flags(Some(b), None, None, out),
            RawEvent::Pause(b) => self.set_flags(None, Some(b), None, out),
            RawEvent::Seeking(b) => self.set_flags(None, None, Some(b), out),
            RawEvent::Log { prefix, text } => {
                if !is_stream_log_prefix(&prefix) {
                    return;
                }
                // Between START_FILE and END_FILE lines belong to the playing
                // entry (unless quarantined: late lines of the previous one);
                // after END_FILE (no current entry) to the latest ending.
                let errors = match (self.current.as_mut(), self.ending.last_mut()) {
                    (Some(c), _) if c.quarantine_until.is_some_and(|q| now < q) => None,
                    (Some(c), _) => Some(&mut c.errors),
                    (None, Some(e)) if e.quarantine_until.is_some_and(|q| now < q) => None,
                    (None, Some(e)) => Some(&mut e.errors),
                    (None, None) => None,
                };
                if let Some(errors) = errors {
                    if errors.len() >= MAX_ENTRY_ERRORS {
                        errors.remove(0);
                    }
                    errors.push(text);
                }
            }
        }
    }

    fn set_flags(&mut self, pfc: Option<bool>, pause: Option<bool>, seeking: Option<bool>, out: &mut Vec<(u32, PlayerEvent)>) {
        self.paused_for_cache = pfc.unwrap_or(self.paused_for_cache);
        self.pause = pause.unwrap_or(self.pause);
        self.seeking = seeking.unwrap_or(self.seeking);
        let buffering = effective_buffering(self.paused_for_cache, self.pause, self.seeking);
        if let Some(c) = self.current.as_mut() {
            if c.buffering != buffering {
                c.buffering = buffering;
                out.push((c.generation, PlayerEvent::Buffering(buffering)));
            }
        }
    }

    /// Emit endings whose grace period is over (all of them if `now` is None).
    pub fn flush(&mut self, now: Option<Instant>, out: &mut Vec<(u32, PlayerEvent)>) {
        let (due, keep): (Vec<Ending>, Vec<Ending>) =
            std::mem::take(&mut self.ending).into_iter().partition(|e| now.is_none_or(|n| n >= e.deadline));
        self.ending = keep;
        for e in due {
            if let Some(ev) = map_end_file(e.reason, &e.errors.join(" | ")) {
                out.push((e.generation, ev));
            }
        }
    }
}

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
    mapper: EventMapper,
    /// Events produced outside the mpv event queue (failed loadfile).
    pending: Vec<(u32, PlayerEvent)>,
    /// Recently loaded URLs, for scrubbing (old entries can still log).
    urls: VecDeque<String>,
    log: VecDeque<String>,
}

impl std::fmt::Debug for MpvBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvBackend").field("entries", &self.mapper.pending_entries()).finish_non_exhaustive()
    }
}

/// Oldest mpv with the `loadfile <url> <flags> <index> <options>` signature.
pub const MIN_MPV_VERSION: (u32, u32) = (0, 38);

/// Parse "mpv v0.41.0-1200-g..." / "mpv 0.38.0" into (major, minor).
pub fn parse_mpv_version(s: &str) -> Option<(u32, u32)> {
    let v = s.split_whitespace().nth(1)?.trim_start_matches('v');
    let mut it = v.split(|c: char| !c.is_ascii_digit());
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
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
        let version = get_string(handle, "mpv-version");
        match parse_mpv_version(&version) {
            Some(v) if v < MIN_MPV_VERSION => bail!(
                "libmpv-2.dll is {version}; need mpv >= {}.{} (loadfile index argument)",
                MIN_MPV_VERSION.0,
                MIN_MPV_VERSION.1
            ),
            Some(_) => {}
            None => eprintln!("warning: cannot parse mpv version {version:?}; assuming >= 0.38"),
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
            mapper: EventMapper::default(),
            pending: Vec::new(),
            urls: VecDeque::new(),
            log: VecDeque::new(),
        })
    }

    /// mpv version string (e.g. "mpv v0.41.0-...").
    pub fn mpv_version(&self) -> String {
        get_string(self.core.handle, "mpv-version")
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

    fn push_log(&mut self, line: String) {
        if self.log.len() >= MAX_LOG {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// Drain mpv's event queue (non-blocking) and map it to supervisor events,
    /// each tagged with the generation of the load that caused it.
    pub fn poll_events(&mut self) -> Vec<(u32, PlayerEvent)> {
        let mut out = std::mem::take(&mut self.pending);
        loop {
            let ev = unsafe { &*sys::mpv_wait_event(self.core.handle, 0.0) };
            let now = Instant::now();
            let raw = match ev.event_id {
                sys::mpv_event_id_MPV_EVENT_NONE => break,
                sys::mpv_event_id_MPV_EVENT_START_FILE => {
                    let d = unsafe { &*(ev.data as *const sys::mpv_event_start_file) };
                    RawEvent::StartFile(d.playlist_entry_id)
                }
                sys::mpv_event_id_MPV_EVENT_PLAYBACK_RESTART => RawEvent::PlaybackRestart,
                sys::mpv_event_id_MPV_EVENT_END_FILE => {
                    let d = unsafe { &*(ev.data as *const sys::mpv_event_end_file) };
                    RawEvent::EndFile {
                        entry: d.playlist_entry_id,
                        reason: EndReason::from_raw(d.reason),
                        error: (d.error < 0).then(|| mpv_err(d.error)),
                    }
                }
                sys::mpv_event_id_MPV_EVENT_PROPERTY_CHANGE => {
                    let p = unsafe { &*(ev.data as *const sys::mpv_event_property) };
                    let double = (p.format == sys::mpv_format_MPV_FORMAT_DOUBLE && !p.data.is_null())
                        .then(|| unsafe { *(p.data as *const f64) });
                    let flag = p.format == sys::mpv_format_MPV_FORMAT_FLAG
                        && !p.data.is_null()
                        && unsafe { *(p.data as *const c_int) } != 0;
                    match ev.reply_userdata {
                        OBS_TIME_POS => RawEvent::TimePos(double),
                        OBS_DURATION => RawEvent::Duration(double),
                        OBS_PAUSED_FOR_CACHE => RawEvent::PausedForCache(flag),
                        OBS_PAUSE => RawEvent::Pause(flag),
                        OBS_SEEKING => RawEvent::Seeking(flag),
                        _ => continue,
                    }
                }
                sys::mpv_event_id_MPV_EVENT_LOG_MESSAGE => {
                    let m = unsafe { &*(ev.data as *const sys::mpv_event_log_message) };
                    let s = |p: *const c_char| {
                        if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
                    };
                    let known: Vec<&str> = self.urls.iter().map(String::as_str).collect();
                    let text = scrub(s(m.text).trim_end(), &known);
                    let prefix = scrub(&s(m.prefix), &known);
                    self.push_log(format!("[{prefix}] {}: {text}", s(m.level)));
                    RawEvent::Log { prefix, text }
                }
                _ => continue,
            };
            self.mapper.handle(raw, now, &mut out);
        }
        self.mapper.flush(Some(Instant::now()), &mut out);
        out
    }
}

fn get_string(handle: *mut sys::mpv_handle, name: &str) -> String {
    let name = CString::new(name).unwrap();
    unsafe {
        let p = sys::mpv_get_property_string(handle, name.as_ptr());
        if p.is_null() {
            return String::from("?");
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        sys::mpv_free(p as *mut c_void);
        s
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
        let start_opt = start_option(start_at);
        let args: Result<Vec<CString>> = ["loadfile", url, "replace", "-1"]
            .into_iter()
            .chain(start_opt.as_deref())
            .map(cstr)
            .collect();
        let Ok(args) = args else {
            self.pending.push((generation, PlayerEvent::Error(ErrorClass::Other)));
            return;
        };
        let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        let mut res: sys::mpv_node = unsafe { std::mem::zeroed() };
        let r = unsafe { sys::mpv_command_ret(self.core.handle, argv.as_mut_ptr(), &mut res) };
        if r < 0 {
            self.push_log(format!("[backend] error: loadfile failed: {}", mpv_err(r)));
            self.pending.push((generation, PlayerEvent::Error(ErrorClass::Other)));
            return;
        }
        let id = unsafe { node_map_int(&res, "playlist_entry_id") };
        unsafe { sys::mpv_free_node_contents(&mut res) };
        match id {
            // Only the start= that was actually passed gates early positions.
            Some(id) => self.mapper.loaded(id, generation, start_opt.and(start_at)),
            None => {
                self.push_log("[backend] error: loadfile returned no playlist_entry_id".into());
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
        self.mapper.stopped();
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
            Some(PlayerEvent::Error(Unavailable))
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
        for p in ["ffmpeg", "ffmpeg/demuxer", "stream", "lavf", "curl", "demux", "stream_lavf"] {
            assert!(is_stream_log_prefix(p), "{p}");
        }
        for p in ["ffmpeg/video", "ffmpeg/audio", "vd", "ad", "vo/gpu", "ao/wasapi", "sub", "ytdl_hook", "cplayer", "file"] {
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

    // ---------- I1: stream ids / timestamps never classify as auth ----------

    #[test]
    fn auth_code_in_stream_id_is_not_auth() {
        use ErrorClass::*;
        // A channel whose id is 401/458 failing with a plain connection error.
        for id in ["401", "403", "458", "459"] {
            let text = format!(
                "error: Could not connect to server | Failed to open http://h:8080/live/***/***/{id}.ts. | loading failed"
            );
            assert_eq!(map_end_file(EndReason::Error, &text), Some(PlayerEvent::Error(Network)), "{id}");
            let eof = format!("transfer failed: Failure when receiving data from the peer http://h/***/***/{id}");
            assert_eq!(map_end_file(EndReason::Eof, &eof), Some(PlayerEvent::Ended), "{id}");
        }
        // Real status codes still are.
        assert_eq!(
            map_end_file(EndReason::Error, "HTTP error 401 | Failed to open http://h/live/***/***/401.ts."),
            Some(PlayerEvent::Error(Auth))
        );
        assert_eq!(map_end_file(EndReason::Error, "HTTP error 458"), Some(PlayerEvent::Error(Auth)));
        // Decimal timestamps are not codes.
        assert_eq!(map_end_file(EndReason::Eof, "mpegts: Invalid timestamp 458.200 (401.5)"), Some(PlayerEvent::Ended));
    }

    #[test]
    fn neutralize_keeps_wording() {
        assert_eq!(
            neutralize_for_classification("Failed to open 'http://h/a/***/***/401.ts'. HTTP error 401 at 12.5s"),
            "Failed to open '<url>'. HTTP error 401 at <num>s"
        );
        assert_eq!(neutralize_for_classification("é 1.2 x"), "é <num> x");
    }

    // ---------- EventMapper ----------

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn log(prefix: &str, text: &str) -> RawEvent {
        RawEvent::Log { prefix: prefix.into(), text: text.into() }
    }

    fn end(entry: i64, reason: EndReason) -> RawEvent {
        let error = (reason == EndReason::Error).then(|| "loading failed".to_string());
        RawEvent::EndFile { entry, reason, error }
    }

    /// Feed (offset ms, event) pairs (flushing as time passes), then flush
    /// well after the last one; return the output.
    fn run(m: &mut EventMapper, base: Instant, evs: Vec<(u64, RawEvent)>) -> Vec<(u32, PlayerEvent)> {
        let mut out = vec![];
        let mut last = 0;
        for (t, e) in evs {
            m.flush(Some(base + ms(t)), &mut out);
            m.handle(e, base + ms(t), &mut out);
            last = t;
        }
        m.flush(Some(base + ms(last + 10_000)), &mut out);
        out
    }

    #[test]
    fn late_auth_lines_of_replaced_entry_do_not_taint_new_entry() {
        // Gen 1 is failing with 458 when the user switches to gen 2; the old
        // lines arrive after START_FILE of the new entry. Gen 2 later ends
        // with EOF: that must be Ended, not Auth.
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![(0, RawEvent::StartFile(1)), (5, log("curl", "HTTP error 458"))]);
        assert!(out.is_empty(), "{out:?}");
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (100, end(1, EndReason::Stop)),
            (100, RawEvent::StartFile(2)),
            (110, log("curl", "HTTP error 458")),
            (120, log("stream", "Failed to open http://h/***/***/9.ts.")),
            (900, RawEvent::PlaybackRestart),
            (60_000, end(2, EndReason::Eof)),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Started), (2, PlayerEvent::Ended)]);
    }

    #[test]
    fn late_auth_lines_of_replaced_entry_do_not_taint_new_load_failure() {
        // Same, but gen 2 fails to load before ever starting: Other, not Auth.
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (50, end(1, EndReason::Stop)),
            (50, RawEvent::StartFile(2)),
            (60, log("curl", "HTTP error 458")),
            (80, end(2, EndReason::Error)),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Error(ErrorClass::Other))]);
    }

    #[test]
    fn retry_lines_are_attributed_to_the_retry() {
        // Automatic retry: START_FILE comes >= 750 ms after the old END_FILE,
        // outside the window, so the retry's own late 401 line is used.
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, end(1, EndReason::Error)),
            (12, log("curl", "HTTP error 401")),
        ]);
        assert_eq!(out, vec![(1, PlayerEvent::Error(ErrorClass::Auth))]);
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (800, RawEvent::StartFile(2)),
            (810, end(2, EndReason::Error)),
            (812, log("curl", "HTTP error 401")),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Error(ErrorClass::Auth))]);
    }

    #[test]
    fn eof_auth_upgrade_only_from_lines_after_restart() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, log("curl", "HTTP error 403")), // load-phase line
            (20, RawEvent::PlaybackRestart),
            (5_000, end(1, EndReason::Eof)),
        ]);
        assert_eq!(out, vec![(1, PlayerEvent::Started), (1, PlayerEvent::Ended)]);
        // While playing, the server cuts the stream off with 458: Auth.
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (10_000, RawEvent::StartFile(2)),
            (10_020, RawEvent::PlaybackRestart),
            (15_000, log("curl", "HTTP error 458")),
            (15_010, end(2, EndReason::Eof)),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Started), (2, PlayerEvent::Error(ErrorClass::Auth))]);
    }

    #[test]
    fn cplayer_timestamp_line_ignored() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, RawEvent::PlaybackRestart),
            (20, log("cplayer", "Invalid video timestamp: 458.200 -> 401")),
            (30, log("ffmpeg/video", "h264: error while decoding MB 401 23")),
            (40, end(1, EndReason::Error)),
        ]);
        assert_eq!(out, vec![(1, PlayerEvent::Started), (1, PlayerEvent::Error(ErrorClass::Other))]);
    }

    #[test]
    fn stale_time_pos_and_duration_of_old_file_ignored() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, RawEvent::PlaybackRestart),
            (20, RawEvent::Duration(Some(3600.0))),
            (30, RawEvent::TimePos(Some(100.0))),
        ]);
        assert_eq!(
            out,
            vec![(1, PlayerEvent::Started), (1, PlayerEvent::Duration(3600.0)), (1, PlayerEvent::Position(100.0))]
        );
        // Resume load of another file at 1234.5; old values trail in.
        m.loaded(2, 2, Some(1234.5));
        let out = run(&mut m, base, vec![
            (40, end(1, EndReason::Stop)),
            (40, RawEvent::StartFile(2)),
            (41, RawEvent::TimePos(Some(100.04))),
            (42, RawEvent::Duration(Some(3600.0))),
            (43, RawEvent::TimePos(None)),
            (44, RawEvent::Duration(Some(1800.0))),
            (45, RawEvent::TimePos(Some(1234.6))),
            (50, RawEvent::PlaybackRestart),
            (60, RawEvent::TimePos(Some(1235.0))),
        ]);
        assert_eq!(
            out,
            vec![
                (2, PlayerEvent::Position(1234.6)),
                (2, PlayerEvent::Started),
                (2, PlayerEvent::Duration(1800.0)),
                (2, PlayerEvent::Position(1235.0)),
            ]
        );
        // Live (no start=): nothing before restart.
        m.loaded(3, 3, None);
        let out = run(&mut m, base, vec![
            (70, end(2, EndReason::Stop)),
            (70, RawEvent::StartFile(3)),
            (71, RawEvent::TimePos(Some(1236.0))),
            (72, RawEvent::Duration(Some(1800.0))),
            (73, RawEvent::Duration(None)),
            (80, RawEvent::PlaybackRestart),
        ]);
        assert_eq!(out, vec![(3, PlayerEvent::Started)]);
    }

    #[test]
    fn stop_and_replace_produce_nothing() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, RawEvent::PlaybackRestart),
            (20, end(1, EndReason::Stop)),
        ]);
        assert_eq!(out, vec![(1, PlayerEvent::Started)]);
        // User stop while an error END_FILE waits for its grace period.
        m.loaded(2, 2, None);
        let mut out = vec![];
        m.handle(RawEvent::StartFile(2), base + ms(100), &mut out);
        m.handle(end(2, EndReason::Error), base + ms(110), &mut out);
        m.stopped();
        m.flush(Some(base + ms(5_000)), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn entries_that_never_started_are_pruned() {
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        m.loaded(2, 2, None);
        m.loaded(3, 3, None);
        let mut out = vec![];
        m.handle(RawEvent::StartFile(3), Instant::now(), &mut out);
        assert_eq!(m.pending_entries(), 1);
        m.handle(end(3, EndReason::Stop), Instant::now(), &mut out);
        assert_eq!(m.pending_entries(), 0);
    }

    #[test]
    fn stale_paused_for_cache_does_not_leak_into_next_file() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, RawEvent::PlaybackRestart),
            (20, RawEvent::PausedForCache(true)),
            (30, end(1, EndReason::Stop)),
            (30, RawEvent::StartFile(2)),
            (40, RawEvent::PlaybackRestart),
            // A pause toggle re-evaluates the flags: must not report buffering.
            (50, RawEvent::Pause(true)),
            (60, RawEvent::Pause(false)),
        ]);
        assert_eq!(
            out,
            vec![(1, PlayerEvent::Started), (1, PlayerEvent::Buffering(true)), (2, PlayerEvent::Started)]
        );
    }

    #[test]
    fn user_pause_or_seek_during_cache_wait_is_not_buffering() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (10, RawEvent::PlaybackRestart),
            (20, RawEvent::PausedForCache(true)),
            (30, RawEvent::Pause(true)),
            (40, RawEvent::Seeking(true)),
            (50, RawEvent::Pause(false)),
            (60, RawEvent::Seeking(false)),
            (70, RawEvent::PausedForCache(false)),
        ]);
        assert_eq!(
            out,
            vec![
                (1, PlayerEvent::Started),
                (1, PlayerEvent::Buffering(true)),
                (1, PlayerEvent::Buffering(false)),
                (1, PlayerEvent::Buffering(true)),
                (1, PlayerEvent::Buffering(false)),
            ]
        );
    }

    // ---------- fix round 2 ----------

    #[test]
    fn late_old_line_after_new_entry_end_is_quarantined() {
        // The old entry's 458 line arrives only after the new entry's own
        // error END_FILE: it must not land in the new entry's ending.
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (50, end(1, EndReason::Stop)),
            (50, RawEvent::StartFile(2)),
            (80, end(2, EndReason::Error)),
            (90, log("curl", "HTTP error 458")),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Error(ErrorClass::Other))]);
    }

    #[test]
    fn quarantine_survives_early_restart() {
        // New entry restarts within the window; an old line arriving after
        // the restart must not later turn its EOF into Auth.
        let base = Instant::now();
        let mut m = EventMapper::default();
        m.loaded(1, 1, None);
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (50, end(1, EndReason::Stop)),
            (50, RawEvent::StartFile(2)),
            (100, RawEvent::PlaybackRestart),
            (200, log("curl", "HTTP error 458")),
            (60_000, end(2, EndReason::Eof)),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Started), (2, PlayerEvent::Ended)]);
        // After the window, lines while playing count again.
        m.loaded(3, 3, None);
        let out = run(&mut m, base, vec![
            (61_000, RawEvent::StartFile(3)),
            (61_020, RawEvent::PlaybackRestart),
            (70_000, log("curl", "HTTP error 458")),
            (70_010, end(3, EndReason::Eof)),
        ]);
        assert_eq!(out, vec![(3, PlayerEvent::Started), (3, PlayerEvent::Error(ErrorClass::Auth))]);
    }

    #[test]
    fn pre_restart_positions_only_near_start() {
        let base = Instant::now();
        let mut m = EventMapper::default();
        // Resume at 100: a stale old-file position of 5000 must not pass.
        m.loaded(1, 1, Some(100.0));
        let out = run(&mut m, base, vec![
            (0, RawEvent::StartFile(1)),
            (1, RawEvent::TimePos(Some(5000.0))),
            (2, RawEvent::TimePos(Some(99.0))),
            (3, RawEvent::TimePos(Some(130.5))),
            (4, RawEvent::TimePos(Some(100.4))),
            (5, RawEvent::PlaybackRestart),
            (6, RawEvent::TimePos(Some(5000.0))), // after restart: real
        ]);
        assert_eq!(
            out,
            vec![(1, PlayerEvent::Position(100.4)), (1, PlayerEvent::Started), (1, PlayerEvent::Position(5000.0))]
        );
        // Live / no start=: nothing before restart, whatever the value.
        m.loaded(2, 2, None);
        let out = run(&mut m, base, vec![
            (2_000, end(1, EndReason::Stop)),
            (2_000, RawEvent::StartFile(2)),
            (2_001, RawEvent::TimePos(Some(0.0))),
            (2_002, RawEvent::TimePos(Some(7.0))),
            (2_010, RawEvent::PlaybackRestart),
        ]);
        assert_eq!(out, vec![(2, PlayerEvent::Started)]);
    }

    #[test]
    fn unavailable_channel_through_map_end_file() {
        use ErrorClass::*;
        // 407 from an off-air channel, as the libcurl backend and lavf word it.
        for text in [
            // Exact lines the libcurl backend logged for a local 407 server.
            "error: HTTP error 407 | error: transfer failed: Failed writing received data to disk/application | error: Failed to open http://127.0.0.1:8097/live/***/***/5.ts. | loading failed",
            "HTTP error 407 | loading failed",
            "curl: HTTP error 407 | Failed to open http://h/live/***/***/12345.ts. | loading failed",
            "Server returned 4XX Client Error, but not one of 40{0,1,3,4} | loading failed",
            "HTTP error 404 | loading failed",
            "HTTP error 410 | loading failed",
        ] {
            assert_eq!(map_end_file(EndReason::Error, text), Some(PlayerEvent::Error(Unavailable)), "{text}");
        }
        // A stream id 407/404 in the (redacted) URL is not a status.
        assert_eq!(
            map_end_file(EndReason::Error, "Could not connect to server | Failed to open http://h/live/***/***/407.ts. | loading failed"),
            Some(PlayerEvent::Error(Network))
        );
        // A reconnect that gave up on a 407 is reported as unavailable too.
        assert_eq!(map_end_file(EndReason::Eof, "HTTP error 407"), Some(PlayerEvent::Error(Unavailable)));
    }

    #[test]
    fn more_auth_wordings_through_map_end_file() {
        use ErrorClass::*;
        for text in [
            "Too many connections",
            "HTTP error 403 Forbidden",
            "curl: (22) The requested URL returned error: 401",
        ] {
            assert_eq!(map_end_file(EndReason::Error, text), Some(PlayerEvent::Error(Auth)), "{text}");
        }
    }

    #[test]
    fn mpv_version_parsing() {
        assert_eq!(parse_mpv_version("mpv v0.41.0-1200-gdde067107"), Some((0, 41)));
        assert_eq!(parse_mpv_version("mpv 0.38.0"), Some((0, 38)));
        assert!(parse_mpv_version("mpv v0.37.0-dirty").unwrap() < MIN_MPV_VERSION);
        assert!(parse_mpv_version("mpv v0.38.0").unwrap() >= MIN_MPV_VERSION);
        assert_eq!(parse_mpv_version("mpv dde067107"), None);
        assert_eq!(parse_mpv_version("?"), None);
    }
}
