//! GUI-free state of "Export recording as MKV": the folder picker, the export
//! dialog (name, folder, checks), the success toast and the running job.
//! The egui layer in `ui` only draws this.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use iptv::export::{self, ExportJob, JobState, Progress};
use iptv::recordings::Recording;

/// A tiny folder browser: the drive list, or one folder with its subfolders.
#[derive(Debug, Clone)]
pub struct FolderPicker {
    /// `None` = the drive list.
    location: Option<PathBuf>,
    /// Display name of each row (drive path or subfolder name).
    names: Vec<String>,
    /// Full path of each row, parallel to `names`.
    paths: Vec<PathBuf>,
}

impl FolderPicker {
    /// Start in `dir`; `None` or a folder that is gone = the drive list.
    pub fn open_at(dir: Option<&Path>) -> FolderPicker {
        let mut p = FolderPicker { location: None, names: Vec::new(), paths: Vec::new() };
        match dir {
            Some(d) if d.is_dir() => p.go(Some(d.to_path_buf())),
            _ => p.go(None),
        }
        p
    }

    fn go(&mut self, to: Option<PathBuf>) {
        let rows: Vec<(String, PathBuf)> = match &to {
            None => export::drives().into_iter().map(|d| (d.display().to_string(), d)).collect(),
            Some(dir) => {
                let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        (!name.starts_with('.')).then(|| (name, e.path()))
                    })
                    .collect();
                v.sort_by_key(|(n, _)| n.to_lowercase());
                v
            }
        };
        (self.names, self.paths) = rows.into_iter().unzip();
        self.location = to;
    }

    /// The folder shown now; `None` = the drive list.
    pub fn location(&self) -> Option<&Path> {
        self.location.as_deref()
    }

    /// Display names of the rows (drive paths or subfolder names).
    pub fn rows(&self) -> &[String] {
        &self.names
    }

    /// Open row `row` (ignored when out of range).
    pub fn enter(&mut self, row: usize) {
        if let Some(path) = self.paths.get(row).cloned() {
            self.go(Some(path));
        }
    }

    /// Go to the parent folder; from a drive root, to the drive list.
    pub fn up(&mut self) {
        if let Some(loc) = &self.location {
            let parent = loc.parent().map(Path::to_path_buf);
            self.go(parent);
        }
    }

    /// The current folder; `None` on the drive list.
    pub fn choose(&self) -> Option<PathBuf> {
        self.location.clone()
    }
}

/// What to export where; produced by a successful `ExportDialog::submit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRequest {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub folder: PathBuf,
    /// The recording's estimated length (for trusting the probed duration).
    pub length_hint: Option<Duration>,
}

/// Result of pressing Export in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submit {
    /// A check failed; show the text in the dialog.
    Invalid(String),
    /// The destination exists: ask before replacing it.
    AskReplace(ExportRequest),
    Start(ExportRequest),
}

/// The export dialog for one recording.
#[derive(Debug)]
pub struct ExportDialog {
    rec: Recording,
    folder: Option<PathBuf>,
    /// The file name as typed (cleaned on submit).
    pub name: String,
    pub picker: Option<FolderPicker>,
    pub replace_question: Option<ExportRequest>,
}

impl ExportDialog {
    /// Folder = the remembered one if it still exists, else Downloads.
    pub fn new(rec: Recording, remembered: Option<&str>) -> ExportDialog {
        let name = export::default_file_name(&rec.title);
        ExportDialog { folder: export::default_dir(remembered), rec, name, picker: None, replace_question: None }
    }

    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn rec(&self) -> &Recording {
        &self.rec
    }

    /// Open the folder picker at the current folder.
    pub fn open_picker(&mut self) {
        self.picker = Some(FolderPicker::open_at(self.folder.as_deref()));
    }

    /// Take the picker's current folder (nothing on the drive list) and close it.
    pub fn use_picked(&mut self) {
        if let Some(p) = self.picker.take() {
            if let Some(dir) = p.choose() {
                self.folder = Some(dir);
            }
        }
    }

    /// Close the picker, keeping the folder.
    pub fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Run the checks in order: folder known, file name, source, free space,
    /// then an existing destination asks before replacing.
    pub fn submit(&mut self, now: SystemTime, being_recorded: bool, free: Option<u64>) -> Submit {
        self.replace_question = None;
        let Some(folder) = self.folder.clone() else {
            return Submit::Invalid("Pick a folder to save to.".to_string());
        };
        let file = match export::clean_file_name(&self.name) {
            Ok(f) => f,
            Err(m) => return Submit::Invalid(m),
        };
        // The list is a snapshot (and directory listings lag for a file another
        // process holds open): look at the file itself now.
        let (modified, size) = match std::fs::metadata(&self.rec.path) {
            Ok(m) => (m.modified().unwrap_or(self.rec.modified), m.len()),
            Err(_) => return Submit::Invalid("The recording is gone: it was moved or deleted.".to_string()),
        };
        if let Err(m) = export::check_source(modified, now, being_recorded) {
            return Submit::Invalid(m);
        }
        if let Err(m) = export::check_space(size, free) {
            return Submit::Invalid(m);
        }
        let dest = export::dest_path(&folder, &file);
        let req = ExportRequest { src: self.rec.path.clone(), dest, folder, length_hint: self.rec.length() };
        if req.dest.exists() {
            self.replace_question = Some(req.clone());
            Submit::AskReplace(req)
        } else {
            Submit::Start(req)
        }
    }

    /// The user agreed to replace the existing file.
    pub fn confirm_replace(&mut self) -> Option<ExportRequest> {
        self.replace_question.take()
    }
}

/// How long the success toast stays.
pub const TOAST_SECS: u64 = 6;

/// A short-lived message.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    since: Instant,
}

impl Toast {
    pub fn new(text: String, now: Instant) -> Toast {
        Toast { text, since: now }
    }

    pub fn visible(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) < Duration::from_secs(TOAST_SECS)
    }
}

/// `Exported "<file name>"`, plus ` (without subtitles)`.
pub fn done_text(dest: &Path, without_subtitles: bool) -> String {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let extra = if without_subtitles { " (without subtitles)" } else { "" };
    format!("Exported \"{name}\"{extra}")
}

/// The running export (at most one) and the toast of the last success.
#[derive(Default)]
pub struct Exports {
    job: Option<ExportJob>,
    pub toast: Option<Toast>,
}

impl Exports {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Start `req` with `ffmpeg`; ignored while another export runs.
    pub fn start(&mut self, ffmpeg: PathBuf, req: ExportRequest) {
        if self.job.is_none() {
            self.job = Some(ExportJob::start_with_hint(ffmpeg, req.src, req.dest, req.length_hint));
        }
    }

    /// A finished job is reported once (and dropped): the message to show.
    /// Success also sets `toast`.
    pub fn poll(&mut self, now: Instant) -> Option<String> {
        let job = self.job.as_mut()?;
        let msg = match job.poll() {
            JobState::Running(_) => return None,
            JobState::Done { dest, without_subtitles } => {
                let text = done_text(&dest, without_subtitles);
                self.toast = Some(Toast::new(text.clone(), now));
                text
            }
            JobState::Failed(m) => format!("Export failed: {m}"),
            JobState::Cancelled => "Export cancelled".to_string(),
        };
        self.job = None;
        Some(msg)
    }

    pub fn progress(&mut self) -> Option<Progress> {
        match self.job.as_mut()?.poll() {
            JobState::Running(p) => Some(p),
            _ => None,
        }
    }

    pub fn cancel(&self) {
        if let Some(j) = &self.job {
            j.cancel();
        }
    }

    /// For app exit: kill ffmpeg and remove the `.part` before returning.
    pub fn kill_blocking(&mut self) {
        if let Some(mut j) = self.job.take() {
            j.kill_blocking();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::export;
    use iptv::recordings::Recording;
    use std::time::{Duration, Instant, SystemTime};

    const GIB: u64 = 1 << 30;

    fn rec_at(path: &Path, title: &str, age_secs: u64) -> Recording {
        let now = SystemTime::now();
        Recording {
            path: path.to_path_buf(),
            title: title.to_string(),
            size: 1000,
            created: now - Duration::from_secs(age_secs + 60),
            modified: now - Duration::from_secs(age_secs),
        }
    }

    /// Make the file look `secs` old (submit re-reads the real metadata).
    fn age(path: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    fn dialog(dir: &Path, title: &str) -> ExportDialog {
        ExportDialog::new(rec_at(Path::new("src.ts"), title, 3600), Some(dir.to_str().unwrap()))
    }

    #[test]
    fn picker_walks_folders_and_up_to_the_drive_list() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["b", "A", ".hidden"] {
            std::fs::create_dir(tmp.path().join(d)).unwrap();
        }
        std::fs::write(tmp.path().join("file.txt"), b"x").unwrap();
        let mut p = FolderPicker::open_at(Some(tmp.path()));
        assert_eq!(p.rows(), ["A", "b"]);
        assert_eq!(p.location(), Some(tmp.path()));
        p.enter(0);
        assert_eq!(p.choose(), Some(tmp.path().join("A")));
        p.up();
        assert_eq!(p.location(), Some(tmp.path()));

        let none = FolderPicker::open_at(None);
        assert_eq!(none.location(), None);
        assert_eq!(none.choose(), None);

        let missing = FolderPicker::open_at(Some(&tmp.path().join("nope")));
        assert_eq!(missing.location(), None);
    }

    #[test]
    fn picker_up_from_a_drive_root_shows_the_drive_list() {
        let c = Path::new("C:\\");
        if !c.exists() {
            return;
        }
        let mut p = FolderPicker::open_at(Some(c));
        assert_eq!(p.location(), Some(c));
        p.up();
        assert_eq!(p.location(), None);
        let i = p.rows().iter().position(|r| r.eq_ignore_ascii_case("C:\\")).expect("C: listed");
        p.enter(i);
        assert_eq!(p.location(), Some(c));
    }

    #[test]
    fn dialog_starts_in_the_remembered_folder_or_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dialog(tmp.path(), "News");
        assert_eq!(d.folder(), Some(tmp.path()));
        assert_eq!(d.name, "News.mkv");
        let gone = tmp.path().join("gone");
        let d = ExportDialog::new(rec_at(Path::new("s.ts"), "X", 3600), Some(gone.to_str().unwrap()));
        assert_eq!(d.folder().map(Path::to_path_buf), export::downloads_dir());
    }

    #[test]
    fn submit_checks_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.ts");
        std::fs::write(&src, b"x").unwrap();
        age(&src, 3600);
        let out = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let mut d = ExportDialog::new(rec_at(&src, "Show", 3600), Some(out.path().to_str().unwrap()));

        match d.submit(now, true, Some(100 * GIB)) {
            Submit::Invalid(m) => assert!(m.contains("still being recorded"), "{m}"),
            _ => panic!("expected Invalid"),
        }
        assert!(matches!(d.submit(now, false, None), Submit::Invalid(_)));
        d.name = String::new();
        assert!(matches!(d.submit(now, false, Some(100 * GIB)), Submit::Invalid(_)));
        d.name = "???".to_string();
        match d.submit(now, false, Some(100 * GIB)) {
            Submit::Start(r) => {
                assert_eq!(r.src, src);
                assert_eq!(r.folder, out.path());
                assert_eq!(r.dest, out.path().join("___.mkv"));
            }
            _ => panic!("expected Start"),
        }
        d.name = "Taken".to_string();
        std::fs::write(out.path().join("Taken.mkv"), b"old").unwrap();
        let req = match d.submit(now, false, Some(100 * GIB)) {
            Submit::AskReplace(r) => r,
            _ => panic!("expected AskReplace"),
        };
        assert!(d.replace_question.is_some());
        let again = d.confirm_replace().unwrap();
        assert_eq!(again.dest, req.dest);
        assert!(d.replace_question.is_none());
        assert!(d.confirm_replace().is_none());
    }

    #[test]
    fn submit_uses_the_files_real_state_not_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.ts");
        std::fs::write(&src, b"x").unwrap();
        let out = tempfile::tempdir().unwrap();
        let remembered = Some(out.path().to_str().unwrap());
        let now = SystemTime::now();
        // snapshot says old (and tiny), the file was touched just now
        let mut d = ExportDialog::new(rec_at(&src, "Show", 3600), remembered);
        match d.submit(now, false, Some(100 * GIB)) {
            Submit::Invalid(m) => assert!(m.contains("a moment ago"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
        // now it is old, but has grown since the snapshot: the space check uses the real size
        age(&src, 3600);
        std::fs::write(&src, vec![0u8; 5000]).unwrap();
        age(&src, 3600);
        let mut d = ExportDialog::new(rec_at(&src, "Show", 3600), remembered);
        assert!(matches!(d.submit(now, false, Some(GIB + 4999)), Submit::Invalid(_)));
        assert!(matches!(d.submit(now, false, Some(GIB + 5000)), Submit::Start(_)));
        // deleted
        std::fs::remove_file(&src).unwrap();
        match d.submit(now, false, Some(100 * GIB)) {
            Submit::Invalid(m) => assert!(m.contains("gone"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn submit_without_a_folder_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = dialog(tmp.path(), "A");
        d.folder = None;
        assert!(matches!(d.submit(SystemTime::now(), false, Some(100 * GIB)), Submit::Invalid(_)));
    }

    #[test]
    fn use_picked_changes_the_folder_and_cancel_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut d = dialog(tmp.path(), "A");
        d.open_picker();
        assert!(d.picker.is_some());
        d.picker.as_mut().unwrap().enter(0);
        d.close_picker();
        assert!(d.picker.is_none());
        assert_eq!(d.folder(), Some(tmp.path()));
        d.open_picker();
        d.picker.as_mut().unwrap().enter(0);
        d.use_picked();
        assert!(d.picker.is_none());
        assert_eq!(d.folder(), Some(tmp.path().join("sub").as_path()));
        // on the drive list nothing can be chosen: the folder stays
        d.picker = Some(FolderPicker::open_at(None));
        d.use_picked();
        assert_eq!(d.folder(), Some(tmp.path().join("sub").as_path()));
    }

    #[test]
    fn toast_vanishes_after_toast_secs() {
        let t0 = Instant::now();
        let t = Toast::new("hi".to_string(), t0);
        assert!(t.visible(t0 + Duration::from_secs(5)));
        assert!(!t.visible(t0 + Duration::from_secs(TOAST_SECS)));
    }

    #[test]
    fn done_text_mentions_missing_subtitles() {
        let p = Path::new("C:\\x\\Show.mkv");
        assert_eq!(done_text(p, false), "Exported \"Show.mkv\"");
        assert_eq!(done_text(p, true), "Exported \"Show.mkv\" (without subtitles)");
    }

    #[test]
    fn exports_poll_reports_a_finished_job_once() {
        let Some(ffmpeg) = iptv::rec_merge::find_ffmpeg() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in.ts");
        let ok = std::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=10", "-t", "1", "-c:v", "mpeg2video"])
            .arg("-y")
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        let dest = tmp.path().join("out.mkv");
        let mut ex = Exports::default();
        assert!(!ex.busy());
        ex.start(ffmpeg, ExportRequest { src, dest: dest.clone(), folder: tmp.path().to_path_buf(), length_hint: None });
        assert!(ex.busy());
        let t0 = Instant::now();
        let msg = loop {
            if let Some(m) = ex.poll(Instant::now()) {
                break m;
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "export did not finish");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(msg.starts_with("Exported \"out.mkv\""), "{msg}");
        assert!(dest.exists());
        assert!(ex.toast.is_some());
        assert_eq!(ex.poll(Instant::now()), None);
        assert!(!ex.busy());
        assert!(ex.progress().is_none());
    }

    #[test]
    fn exports_poll_reports_failure_without_a_toast() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ex = Exports::default();
        ex.start(
            tmp.path().join("no-such-ffmpeg.exe"),
            ExportRequest { src: tmp.path().join("a.ts"), dest: tmp.path().join("a.mkv"), folder: tmp.path().to_path_buf(), length_hint: None },
        );
        let t0 = Instant::now();
        let msg = loop {
            if let Some(m) = ex.poll(Instant::now()) {
                break m;
            }
            assert!(t0.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(msg.starts_with("Export failed: "), "{msg}");
        assert!(ex.toast.is_none());
        assert!(!ex.busy());
    }
}
