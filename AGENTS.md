# AGENTS.md

Rust Windows desktop IPTV player (browse + play in one window, with persistent stream retry). Design: `docs/superpowers/specs/2026-10-17-iptv-player-design.md`.

## Rules
- Playlist URL/credentials live only in gitignored `config.toml`. Never commit it, the sample playlists, or the cache; never print credentialed URLs in logs/errors.
- Branches: `feature/...`, `bugfix/...`. Use `git -C <path>`. Update this file and docs when committing a feature branch.
- Add new source files to git.
- Sample playlist (outside repo, contains credentials): `<samples dir>\`. Reference APK in `<apk dir>\` is a WebView+ExoPlayer shell; nothing to port.

## Build / run
- Toolchain: Rust stable, MSVC (`rustup default stable-x86_64-pc-windows-msvc`; needs VS Build Tools with the C++ workload). Run cargo from a normal shell with `%USERPROFILE%\.cargo\bin` on PATH.
- Workspace: `crates/iptv` (library: parsing, config, redaction, ...) and `crates/iptv-player` (egui binary).
- Series: `iptv::series` groups Kind::Series entries into shows/seasons (strips 2-letter language prefixes like `NC - `/`SE:`/`Vod:`, keeps year in show name, unparsed go to `(Unsorted)`); exposed via `Library::shows()`.
- Playback retry: `iptv::supervisor::Supervisor<B: Backend>` is a pure state machine (time passed in as `Instant`). Every `Backend::load` (play or retry) gets a new generation; the backend must tag events with it, stale ones are ignored. Backoff 0.5/1/2/3 s (auth 3/6/12/15 s), reset after 10 s stable playing; pre-Started timeout 30 s VOD / 15 s live, +15 s per failed attempt (cap 120 s, extended by progress events, absolute cap 180 s after load); post-Started Buffering(true) stall watchdog 15 s (never report user pause/seek as Buffering); VOD `Ended` within 5 s of duration = `Finished`, else resume at last position. Never put the URL in its Debug/logs.
- `cargo test -p iptv` runs unit tests. `cargo run -p iptv-player` runs the app.
- Config: copy `config.example.toml` to `config.toml` (gitignored) beside the exe (or in `--data-dir` / `IPTV_DATA_DIR`) and set `playlist_url`. Lookup order: CLI, env, exe directory. Always print URLs via `iptv::redact::redact_url`.
- Dependencies are added per task; eframe, reqwest and libmpv2 arrive with the tasks that use them.

## libmpv (Task 10 links it)
- `*.dll` and `/libmpv/` are gitignored. Get the x86_64 dev package from https://github.com/shinchiro/mpv-winbuild-cmake/releases (`mpv-dev-x86_64-<date>-git-<hash>.7z`, e.g. `gh release download <tag> -R shinchiro/mpv-winbuild-cmake -p 'mpv-dev-x86_64-2*'`) and extract with 7-Zip.
- Put `libmpv-2.dll` in `./libmpv/`; at runtime it must sit beside `iptv-player.exe` (copy it to `target/debug` / `target/release`, or the release folder).
- The `libmpv2` crate links `mpv.lib` (MSVC import library). The package only ships a MinGW `libmpv.dll.a`, so generate it from the DLL: list exports with `dumpbin /exports libmpv-2.dll`, write `mpv.def` (`LIBRARY libmpv-2.dll` + `EXPORTS` + the `mpv_*` names), then `lib /def:mpv.def /out:libmpv\mpv.lib /machine:x64` from a VS developer prompt. Point the linker at it with `RUSTFLAGS=-L native=<repo>\libmpv` or a build script. Not yet verified with the crate (Task 10).
