# Export recording as MKV (design)

## Intent
Get a recording out of the player as an ordinary file other players can use, mainly so it can be put into a Plex library and watched on other devices. Recordings are raw MPEG-TS (`.ts`), which Plex and many players handle badly (poor seeking, missing duration, subtitles). The export is a lossless repackaging into Matroska (`.mkv`).

## Goals
- An "Export" button on every row of the Recordings list (Files view), next to the delete "x".
- Output is `.mkv` only, made by `ffmpeg` with stream copy (no re-encoding): 4K and every other quality stays exactly as recorded. Streams kept: video, audio (all), subtitles (all). Same mapping idea as `rec_merge` (`-map 0:v -map 0:a? -map 0:s?`, `-c copy`).
- The original `.ts` is never touched or deleted.
- A small export dialog drawn inside the player (egui), in the current colour scheme (black or standard, via `ui::theme`), because Windows' own save dialog cannot follow the player's theme.
- The dialog remembers the last export folder; the first time it is the user's Downloads folder.
- Export runs in the background with a progress line; the user can keep watching/browsing. When it finishes, a "done" popup appears and vanishes by itself after a few seconds.

## Non-goals / anti-goals
- No other formats (MP4, AVI, ...), no re-encoding, no quality or codec options.
- No Windows native file dialog; no new dependency for the dialog.
- No export of a file that is still being recorded, no batch/multi-select export, no uploading to Plex, no renaming/tagging for Plex naming conventions.
- Must never lose or damage the original recording, and never leave a half-written `.mkv` that looks finished (a failed or cancelled export removes its partial output).

## Decided (with the user)
- **Format:** MKV only (chosen because MP4 cannot hold some IPTV audio such as MP2 or some subtitle types; MKV takes whatever the stream has).
- **Dialog = "compact" shape (option B):** title "Export recording as MKV"; a "Folder:" line showing the folder that will be used with a "Change folder" button; a "File name:" text box; "Export" and "Cancel" buttons. "Change folder" opens a folder tree (same black/white look) only on demand: the drives on top, folders expand with a triangle, a click selects one, "Select" uses it and "Cancel" keeps the old folder; it opens expanded to the current folder. No full file browser.
- **Remembered folder:** first use = Downloads; afterwards the last folder an export was started into. Stored with the other preferences in `settings.json` (optional field, so the version stays 1).
- **File name:** starts as the recording's name with the extension changed to `.mkv` (e.g. `News One - 2026-10-25 18-30.mkv`), editable. Unsafe characters are rejected/sanitised; `.mkv` is added if missing.
- **While it runs:** background job with a progress line; playback and browsing continue.
- **Finished:** a "done" popup (non-blocking, it vanishes by itself shortly after export finishes). Nothing more (no "open folder" button).
- **Not allowed:** exporting the file that is still being written by a recording. If a file with the chosen name already exists in the folder, ask before replacing it.

## Decided by me (the designer), after the builder check
These were not discussed with the user; they are sensible defaults.
- **Progress:** elapsed media time and size written, plus a percentage only when the duration is trustworthy (ffprobe/ffmpeg-reported duration under a sanity cap, e.g. 12 h plus the recording's time-stamped length); otherwise no percentage. Never show nonsense such as over 100 %.
- **Timestamps:** plain stream copy with `-fflags +genpts -avoid_negative_ts make_zero`, so recorder pieces with the provider's odd clock (~20 h offset) come out starting at 0. The real-ffmpeg test uses such a file.
- **Subtitles:** map them all; if ffmpeg fails because a subtitle codec cannot go into MKV (DVB bitmap, teletext), retry once without subtitles and say so in the done popup ("exported without subtitles"). Data streams (SCTE-35, timed ID3) are not mapped.
- **Output and cancel:** ffmpeg writes `<name>.mkv.iptv-export.part` and the file is renamed to `.mkv` only on success, so a half-written file never looks finished. The progress line has a Cancel button (stops ffmpeg, deletes the `.iptv-export.part`). On app exit the job is killed at once (inside the exit watchdog) and leftover `*.mkv.iptv-export.part` files in the last export folder are removed at the next start of an export or the app (best effort; the app start sweep runs on a background thread). The suffix is deliberately not plain `.mkv.part`: Firefox and download managers use that for resumable downloads in the same Downloads folder, and the sweep must never delete them.
- **One export at a time:** Export is disabled while one runs (the progress line says why).
- **Still being written:** besides the app's own recording, a file whose last write is within the last 10 s is refused ("still being recorded"), which also covers the headless recorder.
- **Free space:** needs the source size plus 1 GiB free in the target folder; if free space cannot be read, refuse with a clear message (same as the recorder).
- **Folder tree:** drives and expandable subfolders (no "Up", no "new folder"; names starting with a dot skipped); a Downloads folder moved to another drive is found through the known-folder API; network paths work only if they are mapped drive letters.
- **Remembered folder:** saved when the export starts (even if it later fails or is cancelled). If the remembered folder no longer exists, Downloads is used (and shown) instead.

## Constraints
- Windows, Rust, egui (existing app structure: GUI-free state in `src/state/`, thin egui layer in `src/ui/`; pure/library parts in `crates/iptv`).
- `ffmpeg` is found the same way as for joining scheduled recordings (`rec_merge::find_ffmpeg`: beside the exe or on PATH). If it is missing, Export is disabled with a clear message; nothing else breaks.
- Never log or show provider URLs (exports only deal with local files, but keep the rule).
- Needs free disk space about the size of the recording in the target folder; check up front and refuse with a clear message.
- Follow repo rules: new source files added to git, feature branch `feature/...`, docs (`AGENTS.md`, README, `state/help.rs` tables if keys are added, `docs/follow-ups.md`) updated when committing.

## Left to the builder
- Module split and names (suggest a pure part in `crates/iptv`, e.g. `iptv::export`, for name sanitising, command construction, progress parsing and the cancel/cleanup logic, and a GUI-free state in `src/state/` for the dialog and job; thin UI in `ui/live.rs`/a new `ui/export.rs`).
- How progress is measured (e.g. `ffmpeg -progress pipe:1` against the recording's estimated length) and how the progress line looks.
- The folder picker's contents: drives/places list, expandable subfolder tree; how to find the Downloads folder on Windows (known-folder API through an `extern "system"` declaration, no new crate if avoidable).
- Behaviour on app exit during an export (suggest: stop ffmpeg and delete the partial file) and whether Esc/a Cancel button can stop a running export.
- Popup look and duration (a few seconds), error messages for failure, no ffmpeg, no space, name clash.
- Tests (unit tests for the pure parts, a real-ffmpeg test where the repo already does that).

## Checks
Unit tests for name handling, command construction, progress parsing, cleanup of partial output, remembered-folder default and persistence. Manual: export a short recording and a long 4K one, confirm the `.mkv` plays in the player and in Plex with the same streams and quality, export into a changed folder and restart the app (folder remembered), export with a name clash, export while a recording is being written (refused), export with ffmpeg missing, theme black and standard.
