//! A second, invisible libmpv instance that opens the same movie/episode and
//! returns small still frames at given times (seek-bar previews).
//!
//! It is a separate connection to the provider: use it from one worker thread
//! only, and never log the URL (nothing here prints it; mpv messages are off).

use std::ffi::{c_char, c_int, CStr, CString};
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use libmpv2_sys as sys;

/// A decoded still frame, RGBA, row-major without padding.
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Thumb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thumb({}x{})", self.width, self.height)
    }
}

pub struct ThumbMpv {
    handle: *mut sys::mpv_handle,
}

// The handle is only used from the thread that owns the `ThumbMpv`.
unsafe impl Send for ThumbMpv {}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("string contains NUL"))
}

fn err(code: c_int) -> String {
    unsafe { CStr::from_ptr(sys::mpv_error_string(code)).to_string_lossy().into_owned() }
}

impl ThumbMpv {
    /// `width`: thumbnail width in pixels (height follows the aspect ratio).
    pub fn new(user_agent: &str, width: u32) -> Result<ThumbMpv> {
        let handle = unsafe { sys::mpv_create() };
        if handle.is_null() {
            bail!("mpv_create failed");
        }
        let vf = format!("scale=w={width}:h=-2,format=bgra");
        let opts: [(&str, &str); 19] = [
            ("terminal", "no"),
            ("msg-level", "all=no"),
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("osc", "no"),
            ("vo", "null"),
            ("ao", "null"),
            ("aid", "no"),
            ("sid", "no"),
            ("hwdec", "no"),
            ("idle", "yes"),
            ("pause", "yes"),
            ("keep-open", "yes"),
            ("cache", "no"),
            ("demuxer-max-bytes", "16MiB"),
            ("network-timeout", "10"),
            ("vf", &vf),
            ("user-agent", if user_agent.trim().is_empty() { iptv::config::DEFAULT_USER_AGENT } else { user_agent }),
        ];
        for (k, v) in opts {
            let (ck, cv) = (cstr(k)?, cstr(v)?);
            let r = unsafe { sys::mpv_set_option_string(handle, ck.as_ptr(), cv.as_ptr()) };
            if r < 0 {
                unsafe { sys::mpv_terminate_destroy(handle) };
                bail!("thumbnail mpv option {k}: {}", err(r));
            }
        }
        let r = unsafe { sys::mpv_initialize(handle) };
        if r < 0 {
            unsafe { sys::mpv_terminate_destroy(handle) };
            bail!("thumbnail mpv_initialize: {}", err(r));
        }
        Ok(ThumbMpv { handle })
    }

    fn command(&self, args: &[&str]) -> Result<()> {
        let c: Vec<CString> = args.iter().map(|a| cstr(a)).collect::<Result<_>>()?;
        let mut argv: Vec<*const c_char> = c.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        let r = unsafe { sys::mpv_command(self.handle, argv.as_mut_ptr()) };
        if r < 0 {
            bail!("mpv {}: {}", args[0], err(r));
        }
        Ok(())
    }

    /// Wait for event `want` (true) or a failure (false), up to `timeout`.
    fn wait_for(&self, want: sys::mpv_event_id, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let ev = unsafe { &*sys::mpv_wait_event(self.handle, left.as_secs_f64().min(0.25)) };
            match ev.event_id {
                id if id == want => return true,
                sys::mpv_event_id_MPV_EVENT_END_FILE | sys::mpv_event_id_MPV_EVENT_SHUTDOWN => return false,
                _ => {}
            }
        }
    }

    /// Open `url` (paused, no audio). `Ok` once the first frame is ready.
    pub fn open(&self, url: &str, timeout: Duration) -> Result<()> {
        self.command(&["loadfile", url, "replace"])?;
        if self.wait_for(sys::mpv_event_id_MPV_EVENT_PLAYBACK_RESTART, timeout) {
            Ok(())
        } else {
            Err(anyhow!("thumbnail source did not open in {timeout:?}"))
        }
    }

    /// Frame near `secs`: a keyframe seek (fast, not frame-exact).
    pub fn frame_at(&self, secs: f64, timeout: Duration) -> Result<Thumb> {
        self.command(&["seek", &format!("{secs:.3}"), "absolute+keyframes"])?;
        if !self.wait_for(sys::mpv_event_id_MPV_EVENT_PLAYBACK_RESTART, timeout) {
            bail!("thumbnail seek to {secs:.0}s timed out");
        }
        self.screenshot()
    }

    fn screenshot(&self) -> Result<Thumb> {
        let c: Vec<CString> = ["screenshot-raw", "video"].iter().map(|a| cstr(a)).collect::<Result<_>>()?;
        let mut argv: Vec<*const c_char> = c.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        let mut res: sys::mpv_node = unsafe { std::mem::zeroed() };
        let r = unsafe { sys::mpv_command_ret(self.handle, argv.as_mut_ptr(), &mut res) };
        if r < 0 {
            bail!("screenshot-raw: {}", err(r));
        }
        let out = unsafe { read_screenshot(&res) };
        unsafe { sys::mpv_free_node_contents(&mut res) };
        out.ok_or_else(|| anyhow!("screenshot-raw returned no usable image"))
    }
}

impl Drop for ThumbMpv {
    fn drop(&mut self) {
        unsafe { sys::mpv_terminate_destroy(self.handle) };
    }
}

/// Picks `w`, `h`, `stride` and `data` out of the `screenshot-raw` node map
/// (`bgr0`/`bgra` pixels) and returns tightly packed RGBA.
unsafe fn read_screenshot(node: &sys::mpv_node) -> Option<Thumb> {
    if node.format != sys::mpv_format_MPV_FORMAT_NODE_MAP {
        return None;
    }
    let (mut w, mut h, mut stride, mut data) = (None, None, None, None);
    unsafe {
        let list = &*node.u.list;
        for i in 0..list.num.max(0) as usize {
            let key = CStr::from_ptr(*list.keys.add(i)).to_bytes();
            let v = &*list.values.add(i);
            match key {
                b"w" if v.format == sys::mpv_format_MPV_FORMAT_INT64 => w = Some(v.u.int64 as usize),
                b"h" if v.format == sys::mpv_format_MPV_FORMAT_INT64 => h = Some(v.u.int64 as usize),
                b"stride" if v.format == sys::mpv_format_MPV_FORMAT_INT64 => stride = Some(v.u.int64 as usize),
                b"data" if v.format == sys::mpv_format_MPV_FORMAT_BYTE_ARRAY && !v.u.ba.is_null() => {
                    let ba = &*v.u.ba;
                    data = Some(std::slice::from_raw_parts(ba.data as *const u8, ba.size));
                }
                _ => {}
            }
        }
    }
    let (w, h, stride, data) = (w?, h?, stride?, data?);
    if w == 0 || h == 0 || stride < w * 4 || data.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for px in data[y * stride..y * stride + w * 4].chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]); // BGRA/BGR0 -> RGBA
        }
    }
    Some(Thumb { width: w as u32, height: h as u32, rgba })
}
