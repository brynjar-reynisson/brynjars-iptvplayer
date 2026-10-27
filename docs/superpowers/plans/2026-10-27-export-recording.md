# Export recording as MKV Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An "Export" button on each Recordings row opens an in-app, theme-aware dialog that remuxes the `.ts` recording into a `.mkv` (stream copy, no re-encode) in a remembered folder, with background progress, Cancel and a self-vanishing "done" popup.

**Architecture:** Pure/library parts in `crates/iptv` (`iptv::export`: names, ffmpeg command, progress parsing, checks, the background job; `iptv::settings`: remembered folder). GUI-free dialog/picker state in `crates/iptv-player/src/state/export.rs`. Thin egui layer in `crates/iptv-player/src/ui/export.rs`, wired into `App` in `ui/mod.rs`. The job writes `<name>.mkv.part` and renames to `.mkv` only on success.

**Tech Stack:** Rust, egui 0.37 (`egui::Modal`), `ffmpeg`/`ffprobe` found via `rec_merge::find_ffmpeg`, `extern "system"` declarations for Win32 (no new crates), `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-10-27-export-recording-design.md`

## Global Constraints

- Branch `feature/export-recording` (already created). New source files are `git add`ed. Commit messages end with the two attribution lines from the session reminder. Update `AGENTS.md`, `README.md`, `docs/follow-ups.md` in the last task (feature-branch rule).
- Output format is MKV only; stream copy (`-c copy`), never re-encode; the original `.ts` is never modified or deleted.
- Streams: `-map 0:v -map 0:a? -map 0:s?`; timestamps: `-fflags +genpts -avoid_negative_ts make_zero`; output written as `<name>.mkv.part` with `-f matroska`, renamed to `.mkv` only on success. On failure/cancel the `.part` is deleted and an existing `.mkv` of that name is untouched.
- If ffmpeg fails with subtitles mapped, retry once without `-map 0:s?` and report `without_subtitles = true` (popup says "exported without subtitles").
- Refuse: file still being recorded by the app, or last write within the last 10 s (`RECENT_WRITE_SECS`); free space in the target folder < source size + 1 GiB (`SPACE_MARGIN`), or free space unreadable; ffmpeg missing (Export disabled with a message).
- One export at a time. Cancel button on the progress line. App exit kills ffmpeg at once and deletes the `.part`.
- Remembered folder: `settings.json` optional field `export_dir` (version stays 1), saved when the export starts; first time (or if the folder no longer exists) Downloads (known-folder API, fallback `%USERPROFILE%\Downloads`).
- File name starts as the recording title + `.mkv`; unsafe characters are replaced, `.mkv` added if missing. Existing destination file: ask before replacing.
- Done popup: non-blocking toast, vanishes by itself after `TOAST_SECS` = 6 (my call for "a few seconds").
- Never print provider URLs (not involved here). Windows only; Win32 via `extern "system"`. Some files are committed with CRLF: keep each file's existing line endings when editing (do not use `sed -i` in Git Bash on them; check with `git ls-files --eol <file>`).
- Colours: dialog/picker/toast use the egui visuals already set by `ui::theme::apply` (no hard-coded colours except what the Recordings list already uses), so black and standard themes both work.

## Review Focus

1. A file name with illegal characters, a Windows reserved name (`CON`, `NUL`...), only dots/spaces, or no extension → sanitised or rejected, never a path escape (`..\x`, `C:\x`). Pinned in Task 1.
2. Source written to a few seconds ago by the headless recorder (app idle) → refused as "still being recorded". Pinned in Task 1.
3. Free space unreadable or just under size + 1 GiB → refused with a clear message. Pinned in Task 1.
4. Remembered folder deleted / on a disconnected drive → falls back to Downloads and shows that folder. Pinned in Tasks 1 and 3.
5. ffmpeg fails, or the user cancels, while replacing an existing `.mkv` → no `.part` left, the old `.mkv` still intact, no half-written `.mkv` that looks finished. Pinned in Task 2.

---

### Task 1: `iptv::export` pure helpers and the remembered folder

**Files:**
- Create: `crates/iptv/src/export.rs`
- Modify: `crates/iptv/src/lib.rs` (add `pub mod export;`), `crates/iptv/src/settings.rs`
- Test: unit tests inside both files

**Interfaces:**
- Consumes: `recordings::free_bytes`, `rec_merge::find_ffmpeg` (unchanged).
- Produces (all in `iptv::export` unless noted):
  - `pub const RECENT_WRITE_SECS: u64 = 10; pub const SPACE_MARGIN: u64 = 1 << 30; pub const MAX_TRUSTED_SECS: f64 = 86_400.0;`
  - `pub fn clean_file_name(input: &str) -> Result<String, String>`
  - `pub fn default_file_name(title: &str) -> String`
  - `pub fn dest_path(dir: &Path, file_name: &str) -> PathBuf` and `pub fn part_path(dest: &Path) -> PathBuf` (`<dest>.part`)
  - `pub fn check_source(modified: SystemTime, now: SystemTime, being_recorded: bool) -> Result<(), String>`
  - `pub fn check_space(source_size: u64, free: Option<u64>) -> Result<(), String>`
  - `pub fn ffmpeg_args(src: &Path, part: &Path, with_subtitles: bool) -> Vec<OsString>` (everything after the program name)
  - `#[derive(Default, Clone, Copy, PartialEq, Debug)] pub struct Progress { pub out_secs: f64, pub bytes: u64, pub fraction: Option<f32> }`, `pub fn apply_progress_line(line: &str, p: &mut Progress)` (reads `out_time_us=`, `total_size=`), `pub fn fraction(out_secs: f64, duration: Option<f64>) -> Option<f32>`
  - `pub fn downloads_dir() -> Option<PathBuf>`, `pub fn default_dir(remembered: Option<&str>) -> Option<PathBuf>` (remembered if it is an existing directory, else `downloads_dir()`)
  - `pub fn drives_from_mask(mask: u32) -> Vec<PathBuf>` (`C:\` for bit 2...) and `pub fn drives() -> Vec<PathBuf>` (`GetLogicalDrives`)
  - `Settings::export_dir(&self) -> Option<&str>` and `Settings::set_export_dir(&mut self, dir: &str)` in `iptv::settings`.

- [ ] **Step 1: Write failing tests** in `export.rs` (module `tests`): 
  - `clean_file_name`: `"News: One/2.mkv"` → `"News_ One_2.mkv"`; `"a"` → `"a.mkv"`; `"a.MKV"` stays; `"  x.mkv. "` → `"x.mkv"`; `"con"` → Err; `"CON.mkv"` → Err; `""`, `"..."`, `"   "` → Err; `"..\\x"` → `".._x.mkv"`'s separators replaced (result contains none of `\ / : * ? " < > |`); a 300-char name is cut to ≤ 150 chars and still ends with `.mkv`.
  - `default_file_name("News One - 2026-10-25 18-30")` == `"News One - 2026-10-25 18-30.mkv"`; `part_path("D:\\x\\a.mkv")` ends with `a.mkv.part`.
  - `check_source`: `being_recorded = true` → Err containing "still being recorded"; modified 3 s ago → Err; modified 60 s ago → Ok.
  - `check_space(5 GiB, Some(5 GiB + SPACE_MARGIN - 1))` Err; `Some(5 GiB + SPACE_MARGIN)` Ok; `None` Err (message says free space could not be read).
  - `ffmpeg_args` contains, in order, `-fflags +genpts`, `-i <src>`, `-map 0:v`, `-map 0:a?`, `-map 0:s?` (absent when `with_subtitles` is false), `-c copy`, `-avoid_negative_ts make_zero`, `-f matroska`, `-progress pipe:1`, ends with the part path; includes `-nostdin -y -nostats`.
  - `apply_progress_line("out_time_us=2500000", ..)` → `out_secs == 2.5`; `"total_size=1048576"` → `bytes`; `"out_time_us=N/A"` and unknown lines leave `p` unchanged.
  - `fraction(30.0, Some(60.0)) == Some(0.5)`; `fraction(70.0, Some(60.0)) == None`; `fraction(1.0, Some(100_000.0)) == None` (over `MAX_TRUSTED_SECS`); `fraction(1.0, None) == None`; `fraction(1.0, Some(0.0)) == None`.
  - `drives_from_mask(0b1100)` == `["C:\\", "D:\\"]`.
  - `default_dir(Some(<existing temp dir>))` returns it; `default_dir(Some("Z:\\no\\such\\dir"))` equals `downloads_dir()`; `downloads_dir()` returns a path whose last component is `Downloads` (skip assertion if `None`).
  
  In `settings.rs` tests: `export_dir_roundtrips_and_is_kept_with_other_changes` (set, save, load; toggling a hidden group afterwards keeps it; a file without the key loads as `None` and the saved JSON then has no `export_dir` key).
- [ ] **Step 2: Run** `cargo test -p iptv export settings` — Expected: FAIL (items not defined).
- [ ] **Step 3: Implement** the functions above in `export.rs`. Notes: `clean_file_name` replaces `\ / : * ? " < >` `|` and control chars with `_`, trims spaces/dots at both ends, compares the stem case-insensitively against `CON PRN AUX NUL COM1-9 LPT1-9`, appends `.mkv` when the (case-insensitive) extension is missing, truncates the stem so the total is ≤ 150 chars. `downloads_dir` calls `SHGetKnownFolderPath(&FOLDERID_Downloads, 0, null, &mut ptr)` (`shell32`) + `CoTaskMemFree` (`ole32`), declared with `extern "system"` as in `recordings.rs`; fall back to `%USERPROFILE%\Downloads` when the call fails. `drives()` uses `GetLogicalDrives` and `drives_from_mask`. In `settings.rs` add `export_dir: Option<String>` to `SettingsFile` (`#[serde(default, skip_serializing_if = "Option::is_none")]`), `Parsed`, `Settings`, `load_with`, `save`.
- [ ] **Step 4: Run** `cargo test -p iptv export settings` — Expected: PASS.
- [ ] **Step 5: Commit** (`git add crates/iptv/src/export.rs docs/superpowers/specs/2026-10-27-export-recording-design.md docs/superpowers/plans/2026-10-27-export-recording.md`; commit `feat: export helpers (names, checks, ffmpeg args, remembered folder)`).

### Task 2: `ExportJob`, the background remux

**Files:**
- Modify: `crates/iptv/src/export.rs`, `crates/iptv/src/rec_merge.rs` (make `hide_window` and `ffprobe_for` `pub(crate)`)
- Test: tests in `export.rs` (real-ffmpeg tests skip with an early `return` when `rec_merge::find_ffmpeg()` is `None`, as `rec_merge`'s tests do)

**Interfaces:**
- Consumes: Task 1 (`ffmpeg_args`, `part_path`, `Progress`, `apply_progress_line`, `fraction`).
- Produces:
  - `pub enum JobState { Running(Progress), Done { dest: PathBuf, without_subtitles: bool }, Failed(String), Cancelled }`
  - `pub struct ExportJob` with `pub fn start(ffmpeg: PathBuf, src: PathBuf, dest: PathBuf) -> ExportJob`, `pub fn poll(&mut self) -> JobState` (non-blocking, never blocks the UI thread), `pub fn cancel(&self)` (kills ffmpeg now, returns at once; the worker then deletes the `.part` and the state becomes `Cancelled`), `pub fn kill_blocking(&mut self)` (for app exit: kill + delete the `.part`, bounded to a fraction of a second), `pub fn dest(&self) -> &Path`.
  - `pub fn sweep_parts(dir: &Path)` deleting `*.mkv.part` directly in `dir` (best effort).

- [ ] **Step 1: Write failing tests** (create a source with `ffmpeg -f lavfi -i testsrc=size=320x180:rate=25 -f lavfi -i sine -t 3 -c:v mpeg2video -c:a mp2 -f mpegts <tmp>/in.ts`):
  - `export_makes_an_mkv_and_keeps_the_source`: run job to completion by polling (≤ 30 s); `Done`; `dest` exists, `.part` gone, source byte-identical; `ffprobe` of dest reports a video and an audio stream and duration ≈ 3 s (±0.5).
  - `progress_is_reported_while_running`: some poll before `Done` returns `Running` with `bytes > 0` OR the job is so fast that it goes straight to `Done` (accept both; assert the final state).
  - `failure_leaves_no_part_and_keeps_an_existing_mkv` (Review Focus 5): source is a text file renamed `bad.ts`, `dest` pre-exists with content `OLD` → `Failed(msg)`, no `.part`, `dest` still contains `OLD`.
  - `cancel_removes_the_part_and_never_creates_the_mkv`: start on a long source (`-t 600` with lavfi, written with `-c:v mpeg2video`, may take a few seconds; or use a 60 s one), `cancel()` immediately, poll until `Cancelled` (≤ 10 s), assert neither `dest` nor `.part` exist and a pre-existing `dest` is untouched.
  - `retries_without_subtitles_when_they_cannot_be_copied`: pure test of the decision function `fn should_retry_without_subs(stderr: &str, with_subs: bool) -> bool` (true only when `with_subs` and stderr mentions `subtitle` or `codec not currently supported in container`; false otherwise), unit tested with sample stderr lines. (A real DVB-subtitle file is not available: the retry itself is verified manually.)
  - `sweep_parts_removes_only_mkv_part_files`.
- [ ] **Step 2: Run** `cargo test -p iptv export` — Expected: FAIL.
- [ ] **Step 3: Implement.** Worker thread: determine duration once with `ffprobe -v error -show_entries format=duration -of csv=p=0 <src>` (failure = `None`), spawn ffmpeg (`hide_window`, stdout piped for `-progress`, stderr piped and drained on another thread into a bounded buffer), read stdout lines through `apply_progress_line`, set `fraction` via `fraction(...)`, publish through an `Arc<Mutex<..>>` shared with `poll`. The child handle sits in a shared `Mutex<Option<Child>>` so `cancel`/`kill_blocking` can kill it from another thread. On exit status success: `std::fs::rename(part, dest)` (replaces an existing file on Windows) → `Done`. On failure: delete the `.part`; if subtitles were mapped and `should_retry_without_subs(stderr, true)`, rerun once without them (`without_subtitles = true` on success); otherwise `Failed` with the last stderr line(s) after `crate::redact`-style neutral cleaning (no URLs are involved; keep it short, ≤ 200 chars). Cancelled flag set → `Cancelled`, `.part` deleted.
- [ ] **Step 4: Run** `cargo test -p iptv export` — Expected: PASS (real-ffmpeg tests run when ffmpeg is on PATH).
- [ ] **Step 5: Commit** `feat: background MKV remux job with cancel and partial-file cleanup`.

### Task 3: GUI-free dialog, folder picker and export state

**Files:**
- Create: `crates/iptv-player/src/state/export.rs`
- Modify: `crates/iptv-player/src/state/mod.rs` (add `pub mod export;`)
- Test: unit tests in the new file

**Interfaces:**
- Consumes: Task 1 and 2 items; `iptv::recordings::Recording`.
- Produces:
  - `pub struct FolderPicker`: `pub fn open_at(dir: Option<&Path>) -> FolderPicker` (`None` or a missing dir = the drive list), `pub fn location(&self) -> Option<&Path>` (`None` = drive list), `pub fn rows(&self) -> &[String]` (display names: drive paths or subfolder names, sorted case-insensitively, names starting with `.` skipped), `pub fn enter(&mut self, row: usize)`, `pub fn up(&mut self)` (from a drive root go to the drive list), `pub fn choose(&self) -> Option<PathBuf>` (the current folder; `None` on the drive list).
  - `pub struct ExportRequest { pub src: PathBuf, pub dest: PathBuf, pub folder: PathBuf }`
  - `pub enum Submit { Invalid(String), AskReplace(ExportRequest), Start(ExportRequest) }`
  - `pub struct ExportDialog` : `pub fn new(rec: Recording, remembered: Option<&str>) -> ExportDialog` (folder = `export::default_dir(remembered)`; name = `default_file_name(&rec.title)`), pub fields `name: String`, `picker: Option<FolderPicker>`, `replace_question: Option<ExportRequest>`; `pub fn folder(&self) -> Option<&Path>`, `pub fn rec(&self) -> &Recording`, `pub fn open_picker(&mut self)`, `pub fn use_picked(&mut self)` (takes `picker.choose()` as the folder and closes the picker), `pub fn close_picker(&mut self)`, `pub fn submit(&mut self, now: SystemTime, being_recorded: bool, free: Option<u64>) -> Submit` (checks, in order: folder known, name via `clean_file_name`, `check_source`, `check_space`, then existing `dest` → `AskReplace`, else `Start`), `pub fn confirm_replace(&mut self) -> Option<ExportRequest>`.
  - `pub const TOAST_SECS: u64 = 6; pub struct Toast { pub text: String, .. }` with `Toast::new(text, now: Instant)`, `visible(&self, now: Instant) -> bool`.
  - `pub fn done_text(dest: &Path, without_subtitles: bool) -> String` (`Exported "<file name>"`, plus ` (without subtitles)`).
  - `pub struct Exports { job: Option<ExportJob>, pub toast: Option<Toast> }`: `busy() -> bool`, `start(ffmpeg, req)`, `poll(now: Instant) -> Option<String>` (on a finished job returns the message to show: `done_text` or `Export failed: ...` / `Export cancelled`, sets `toast`), `progress() -> Option<Progress>`, `cancel()`, `kill_blocking()`.

- [ ] **Step 1: Write failing tests:**
  - `picker_walks_folders_and_up_to_the_drive_list`: temp dir with subfolders `b`, `A`, `.hidden`; `open_at(Some(tmp))` rows == `["A","b"]` (case-insensitive order, hidden skipped); `enter(0)` moves into `A`, `choose()` == `tmp/A`; `up()` returns to `tmp`; `open_at(None).location()` is `None` and `choose()` is `None`; `up()` from a drive root (use `open_at(Some(Path::new("C:\\")))`) goes to the drive list.
  - `dialog_starts_in_the_remembered_folder_or_downloads` (Review Focus 4): remembered temp dir used; remembered missing dir → `folder() == export::downloads_dir()`.
  - `submit_checks_in_order`: recent write → `Invalid` containing "still being recorded"; unreadable free space → `Invalid`; name `"???"` is sanitised and accepted but `""` → `Invalid`; existing destination → `AskReplace`, then `confirm_replace()` returns the request; otherwise `Start` with `dest == folder/<clean name>`.
  - `use_picked_changes_the_folder_and_cancel_keeps_it`.
  - `toast_vanishes_after_toast_secs`: visible at `t0 + 5 s`, not at `t0 + 6 s`.
  - `done_text_mentions_missing_subtitles`.
  - `exports_poll_reports_a_finished_job_once` (uses a real tiny export when ffmpeg exists, else skipped; asserts the second `poll` returns `None` and `busy()` is false).
- [ ] **Step 2: Run** `cargo test -p iptv-player state::export` — Expected: FAIL.
- [ ] **Step 3: Implement** the types above (no egui types). `submit` uses the Task 1 checks; `Start`/`AskReplace` carry `src = rec.path`.
- [ ] **Step 4: Run** `cargo test -p iptv-player state::export` — Expected: PASS.
- [ ] **Step 5: Commit** (`git add` the new file) `feat: export dialog, folder picker and export state (GUI-free)`.

### Task 4: UI, App wiring, exit handling, docs

**Files:**
- Create: `crates/iptv-player/src/ui/export.rs`
- Modify: `crates/iptv-player/src/ui/mod.rs` (add `mod export;`, App fields, `handle_keys`, panel, `on_exit`), `crates/iptv-player/src/ui/live.rs` (Export cell and `RowAction::Export(usize)`; the `countries` match at `live.rs:224` must list the new variant), `AGENTS.md`, `README.md`, `docs/follow-ups.md`
- Test: pure parts are covered in Task 3; add `cargo test --workspace` plus the manual checks below.

**Interfaces:**
- Consumes: Task 3 (`ExportDialog`, `FolderPicker`, `Exports`, `Submit`, `Toast`), `Settings::{export_dir, set_export_dir, save}`, `rec_merge::find_ffmpeg`.
- Produces: `ui::export::dialog(ctx: &egui::Context, d: &mut ExportDialog, error: Option<&str>) -> DialogAction` with `pub enum DialogAction { None, Export, Cancel }`; `ui::export::progress_line(ui: &mut egui::Ui, p: Option<Progress>) -> bool` (true = Cancel clicked); `ui::export::toast(ctx: &egui::Context, text: &str)`.

- [ ] **Step 1: Row button.** In `live::recordings` draw a plain-text "Export" cell (weak text, white-ish on hover, no symbol fonts) immediately left of the delete "x" cell (about 54 pt wide); hover text `Export as MKV`; click returns `RowAction::Export(i)`. Update the `countries` match.
- [ ] **Step 2: App state.** Add `export_dialog: Option<ExportDialog>`, `exports: Exports`, `export_error: Option<String>`, `ffmpeg: Option<Option<PathBuf>>` (looked up on first use via `find_ffmpeg`, cached). `RowAction::Export(i)` → `open_export(i)`: sets the cursor, refuses with `self.message` when ffmpeg is missing ("Export needs ffmpeg: put ffmpeg.exe beside iptv-player.exe or on the PATH") or when `exports.busy()` ("An export is already running"), else `ExportDialog::new(rec, self.settings.export_dir())`.
- [ ] **Step 3: Dialog UI.** `ui::export::dialog` is an `egui::Modal` like `notice_ui` (width `420.0.min(content width - 60)`): heading "Export recording as MKV"; line `Folder: <path>` + "Change folder" button; `File name:` single-line `TextEdit` (focused when the dialog opens); error line (red text is allowed only if the Recordings list's existing red `(230, 70, 70)` is used); buttons Export / Cancel. While `picker` is `Some`, the same modal shows the picker instead: path line, "Up" button, a `ScrollArea` of rows (click enters), "Use this folder" (disabled on the drive list) and "Back". Enter = Export (or "Use this folder" in the picker), Esc = Cancel (or back from the picker); `handle_keys` returns early while the dialog is open (add next to the `notice` early return at `ui/mod.rs:2297`), and the `AskReplace` question is a second modal state in the same window: "<name> already exists. Replace it?" with Replace / Keep.
- [ ] **Step 4: Starting and polling.** On `DialogAction::Export` call `submit(SystemTime::now(), being_recorded, recordings::free_bytes(&folder))` where `being_recorded` = `self.recorder.as_ref().and_then(|r| r.current()) == Some(&rec.path)`; `Invalid(msg)` → `export_error`; `AskReplace` → show the question; `Start(req)` → `settings.set_export_dir(req.folder)` + `save()` (an error goes to `self.message`), `export::sweep_parts(&req.folder)`, `exports.start(ffmpeg, req)`, close the dialog. Every frame in `App::update`: `exports.poll(Instant::now())` → `self.message` for failures/cancel, and the toast for success; request a repaint every 200 ms while `busy()` or a toast is visible.
- [ ] **Step 5: Progress line and toast.** In the Recordings panel (under the "N recordings · free" row) show while `busy()`: `Exporting <name>  mm:ss  <size>` plus `  NN %` only when `fraction` is `Some`, a thin progress bar when it is, and a Cancel button (`exports.cancel()`). `ui::export::toast` draws a bottom-centre `egui::Area` with `Frame::popup`, visible for `TOAST_SECS`, not blocking input, in the current visuals.
- [ ] **Step 6: Exit.** In `App::on_exit` call `exports.kill_blocking()` before the existing stop/`process::exit` logic (the watchdog thread is unchanged).
- [ ] **Step 7: Verify.** `cargo test --workspace` — Expected: all pass. Manual (use the project skill `.claude/skills/build-and-deploy/SKILL.md` to build the release, with the app closed first): export a short recording and a long 4K one to the Downloads default, check the `.mkv` plays in the player with the same streams; change folder, restart the app, folder remembered; name clash asks; export while recording the same file is refused; Cancel mid-export leaves no files; close the app mid-export leaves no `.part`; black and standard themes (`theme` in `config.toml`); with `ffmpeg.exe` hidden, Export says why. Report which of these were not done.
- [ ] **Step 8: Docs.** `AGENTS.md`: add an "Export" entry (modules, `export_dir` setting, `.mkv.part` rule, 10 s / 1 GiB checks, one job at a time, exit kill). `README.md`: Recordings section gets an Export paragraph (MKV, Plex use, remembered folder, needs ffmpeg). `docs/follow-ups.md`: open items (DVB subtitle retry only verified by reasoning unless tested, no "new folder" in the picker, network paths only as mapped drives). No key bindings are added, so `state/help.rs` stays unchanged.
- [ ] **Step 9: Commit** (`git add crates/iptv-player/src/ui/export.rs` and the modified files) `feat: export recordings as MKV (dialog, folder picker, progress, done popup)`.
