//! Native "open file" dialog (comdlg32 `GetOpenFileNameW`) for choosing .srt files.
//! Declared by hand with `extern "system"` (no extra crate). The paths chosen are
//! never logged.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

/// Splits an `OFN_ALLOWMULTISELECT | OFN_EXPLORER` result buffer. One file: the
/// full path, then NUL NUL. Several: the directory, then each file name, each
/// NUL terminated, ended by an empty string.
pub fn parse_multiselect(buf: &[u16]) -> Vec<PathBuf> {
    let mut parts: Vec<String> = Vec::new();
    for seg in buf.split(|&c| c == 0) {
        if seg.is_empty() {
            break;
        }
        parts.push(String::from_utf16_lossy(seg));
    }
    match parts.len() {
        0 => Vec::new(),
        1 => vec![PathBuf::from(&parts[0])],
        _ => {
            let dir = PathBuf::from(&parts[0]);
            parts[1..].iter().map(|n| dir.join(n)).collect()
        }
    }
}

#[repr(C)]
struct OpenFileNameW {
    l_struct_size: u32,
    hwnd_owner: isize,
    h_instance: isize,
    lpstr_filter: *const u16,
    lpstr_custom_filter: *mut u16,
    n_max_cust_filter: u32,
    n_filter_index: u32,
    lpstr_file: *mut u16,
    n_max_file: u32,
    lpstr_file_title: *mut u16,
    n_max_file_title: u32,
    lpstr_initial_dir: *const u16,
    lpstr_title: *const u16,
    flags: u32,
    n_file_offset: u16,
    n_file_extension: u16,
    lpstr_def_ext: *const u16,
    l_cust_data: isize,
    lpfn_hook: isize,
    lp_template_name: *const u16,
    pv_reserved: *mut core::ffi::c_void,
    dw_reserved: u32,
    flags_ex: u32,
}

#[link(name = "comdlg32")]
extern "system" {
    fn GetOpenFileNameW(ofn: *mut OpenFileNameW) -> i32;
}

const OFN_ALLOWMULTISELECT: u32 = 0x0000_0200;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
const OFN_EXPLORER: u32 = 0x0008_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn pick_blocking() -> Vec<PathBuf> {
    let mut buf = vec![0u16; 32768];
    // "Subtitles (*.srt)\0*.srt\0\0"
    let mut filter: Vec<u16> = "Subtitles (*.srt)".encode_utf16().collect();
    filter.push(0);
    filter.extend("*.srt".encode_utf16());
    filter.push(0);
    filter.push(0);
    let title = wide("Choose subtitle files");
    let mut ofn = OpenFileNameW {
        l_struct_size: std::mem::size_of::<OpenFileNameW>() as u32,
        hwnd_owner: 0,
        h_instance: 0,
        lpstr_filter: filter.as_ptr(),
        lpstr_custom_filter: std::ptr::null_mut(),
        n_max_cust_filter: 0,
        n_filter_index: 1,
        lpstr_file: buf.as_mut_ptr(),
        n_max_file: buf.len() as u32,
        lpstr_file_title: std::ptr::null_mut(),
        n_max_file_title: 0,
        lpstr_initial_dir: std::ptr::null(),
        lpstr_title: title.as_ptr(),
        flags: OFN_EXPLORER | OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        n_file_offset: 0,
        n_file_extension: 0,
        lpstr_def_ext: std::ptr::null(),
        l_cust_data: 0,
        lpfn_hook: 0,
        lp_template_name: std::ptr::null(),
        pv_reserved: std::ptr::null_mut(),
        dw_reserved: 0,
        flags_ex: 0,
    };
    // SAFETY: `ofn` and every buffer it points to outlive the call; the struct
    // layout follows the Windows SDK's OPENFILENAMEW (x64).
    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return Vec::new();
    }
    parse_multiselect(&buf)
}

/// Opens the dialog on a thread; the receiver gets the chosen files (empty on
/// cancel or error) once.
pub fn pick_srt_files() -> Receiver<Vec<PathBuf>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(pick_blocking());
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(parts: &[&str]) -> Vec<u16> {
        let mut v = Vec::new();
        for p in parts {
            v.extend(p.encode_utf16());
            v.push(0);
        }
        v.push(0);
        v
    }

    #[test]
    fn multiselect_single_path() {
        assert_eq!(parse_multiselect(&w(&["C:\\subs\\Pilot.en.srt"])), vec![PathBuf::from("C:\\subs\\Pilot.en.srt")]);
    }

    #[test]
    fn multiselect_directory_plus_names() {
        assert_eq!(
            parse_multiselect(&w(&["C:\\subs", "a.srt", "b.srt"])),
            vec![PathBuf::from("C:\\subs").join("a.srt"), PathBuf::from("C:\\subs").join("b.srt")]
        );
    }

    #[test]
    fn multiselect_empty_is_empty() {
        assert!(parse_multiselect(&[]).is_empty());
        assert!(parse_multiselect(&[0, 0]).is_empty());
    }
}
