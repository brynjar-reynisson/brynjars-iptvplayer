//! Link libmpv from `<repo>/libmpv/mpv.lib` and put `libmpv-2.dll` next to the
//! built binaries (exe, test and example executables).
//!
//! Both files are gitignored; see AGENTS.md for how to obtain/generate them.
//! A missing folder only produces a warning here (the link step then fails
//! with "cannot open input file 'mpv.lib'").

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const DLL: &str = "libmpv-2.dll";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/iptv-player -> repo root
    let repo = manifest.parent().and_then(Path::parent).expect("repo root");
    let libdir = repo.join("libmpv");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", libdir.join("mpv.lib").display());
    println!("cargo:rerun-if-changed={}", libdir.join(DLL).display());

    if !libdir.join("mpv.lib").exists() {
        println!(
            "cargo:warning=libmpv not found: expected {} (see AGENTS.md, section libmpv). Linking will fail.",
            libdir.join("mpv.lib").display()
        );
    }
    println!("cargo:rustc-link-search=native={}", libdir.display());

    let dll = libdir.join(DLL);
    if !dll.exists() {
        println!(
            "cargo:warning={} not found in {}; copy it beside iptv-player.exe before running (see AGENTS.md).",
            DLL,
            libdir.display()
        );
        return;
    }
    // OUT_DIR = <target>/<profile>/build/<pkg>-<hash>/out
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out.ancestors().nth(3) else { return };
    for dir in [profile_dir.to_path_buf(), profile_dir.join("deps"), profile_dir.join("examples")] {
        if let Err(e) = place_dll(&dll, &dir) {
            println!("cargo:warning=could not copy {} to {}: {e}", DLL, dir.display());
        }
    }
}

/// Hard-link (cheap, the DLL is >100 MB) or copy `dll` into `dir` unless an
/// identical-size file is already there.
fn place_dll(dll: &Path, dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let dst = dir.join(DLL);
    let src_len = fs::metadata(dll)?.len();
    if fs::metadata(&dst).map(|m| m.len() == src_len).unwrap_or(false) {
        return Ok(());
    }
    let _ = fs::remove_file(&dst);
    if fs::hard_link(dll, &dst).is_err() {
        fs::copy(dll, &dst)?;
    }
    Ok(())
}
