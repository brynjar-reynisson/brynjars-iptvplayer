//! GUI-free state of "Export recording as MKV": the folder tree, the export
//! dialog (name, folder, checks), the success toast and the running job.
//! The egui layer in `ui` only draws this.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use iptv::export::{self, ExportJob, JobState, Progress};
use iptv::recordings::Recording;

/// One visible row of the folder tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: PathBuf,
    /// Drive path (`C:\`) or folder name.
    pub name: String,
    pub depth: usize,
    pub expanded: bool,
}

/// Comparison key of a path: case-insensitive, no trailing separator.
fn key_of(p: &Path) -> String {
    p.to_string_lossy().to_lowercase().trim_end_matches(['\\', '/']).to_string()
}

/// An expandable folder tree: the drives at the top, subfolders read when a
/// folder is first expanded. At most one folder is selected.
#[derive(Debug, Clone)]
pub struct FolderTree {
    roots: Vec<PathBuf>,
    /// Keys of the expanded folders.
    expanded: HashSet<String>,
    /// Sorted subfolders per folder key, read on first expansion.
    children: HashMap<String, Vec<(String, PathBuf)>>,
    rows: Vec<TreeRow>,
    selected: Option<PathBuf>,
}

impl FolderTree {
    /// The drives, expanded down to `current` (which is selected). A folder
    /// that is gone, or `None`: nothing selected, drives collapsed.
    pub fn open_at(current: Option<&Path>) -> FolderTree {
        FolderTree::with_roots(export::drives(), current)
    }

    /// Like `open_at` with explicit top-level folders (tests).
    pub fn with_roots(roots: Vec<PathBuf>, current: Option<&Path>) -> FolderTree {
        let mut t = FolderTree {
            roots,
            expanded: HashSet::new(),
            children: HashMap::new(),
            rows: Vec::new(),
            selected: None,
        };
        if let Some(cur) = current.filter(|c| c.is_dir()) {
            let under_root = t.roots.iter().any(|r| cur.ancestors().any(|a| key_of(a) == key_of(r)));
            if under_root {
                for a in cur.ancestors().skip(1) {
                    t.expanded.insert(key_of(a));
                }
                t.rebuild();
                let k = key_of(cur);
                t.selected = t.rows.iter().find(|r| key_of(&r.path) == k).map(|r| r.path.clone());
                return t;
            }
        }
        t.rebuild();
        t
    }

    fn load_children(&mut self, path: &Path) {
        let key = key_of(path);
        if self.children.contains_key(&key) {
            return;
        }
        let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (!name.starts_with('.')).then(|| (name, e.path()))
            })
            .collect();
        v.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        self.children.insert(key, v);
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        let mut stack: Vec<(String, PathBuf, usize)> =
            self.roots.iter().rev().map(|r| (r.display().to_string(), r.clone(), 0)).collect();
        while let Some((name, path, depth)) = stack.pop() {
            let key = key_of(&path);
            let expanded = self.expanded.contains(&key);
            if expanded {
                self.load_children(&path);
                if let Some(kids) = self.children.get(&key) {
                    for (n, p) in kids.iter().rev() {
                        stack.push((n.clone(), p.clone(), depth + 1));
                    }
                }
            }
            rows.push(TreeRow { path, name, depth, expanded });
        }
        self.rows = rows;
    }

    /// The visible rows, top to bottom.
    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&Path> {
        self.selected.as_deref()
    }

    /// Index of the selected folder among the visible rows.
    pub fn selected_row(&self) -> Option<usize> {
        let k = key_of(self.selected.as_deref()?);
        self.rows.iter().position(|r| key_of(&r.path) == k)
    }

    /// Expand or collapse row `row` (ignored when out of range). Collapsing
    /// a folder that holds the selection moves the selection to it.
    pub fn toggle(&mut self, row: usize) {
        let Some(r) = self.rows.get(row).cloned() else { return };
        let k = key_of(&r.path);
        if r.expanded {
            self.expanded.remove(&k);
            if let Some(sel) = &self.selected {
                let s = key_of(sel);
                if s.starts_with(&format!("{k}\\")) || s.starts_with(&format!("{k}/")) {
                    self.selected = Some(r.path.clone());
                }
            }
        } else {
            self.expanded.insert(k);
        }
        self.rebuild();
    }

    /// Select row `row` (ignored when out of range).
    pub fn select(&mut self, row: usize) {
        if let Some(r) = self.rows.get(row) {
            self.selected = Some(r.path.clone());
        }
    }

    /// Move the selection by `delta` rows, clamped; with nothing selected the
    /// first row is selected.
    pub fn step(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i64 - 1;
        let to = match self.selected_row() {
            Some(i) => (i as i64 + delta as i64).clamp(0, last),
            None => 0,
        };
        self.select(to as usize);
    }

    /// Right: expand a collapsed folder, else move to its first child.
    pub fn right(&mut self) {
        let Some(i) = self.selected_row() else { return };
        if !self.rows[i].expanded {
            self.toggle(i);
        } else if self.rows.get(i + 1).is_some_and(|n| n.depth > self.rows[i].depth) {
            self.select(i + 1);
        }
    }

    /// Left: collapse an expanded folder, else move to the parent row.
    pub fn left(&mut self) {
        let Some(i) = self.selected_row() else { return };
        if self.rows[i].expanded {
            self.toggle(i);
        } else {
            let d = self.rows[i].depth;
            if let Some(p) = (0..i).rev().find(|&j| self.rows[j].depth < d) {
                self.select(p);
            }
        }
    }

    /// The selected folder, if any.
    pub fn choose(&self) -> Option<PathBuf> {
        self.selected.clone()
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
    pub picker: Option<FolderTree>,
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
        self.picker = Some(FolderTree::open_at(self.folder.as_deref()));
    }

    /// Take the tree's selected folder (none = keep the current one) and close it.
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

    /// tmp/{A/{x,y}, b, .hidden, file.txt}.
    fn tree_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["b", "A/y", "A/x", ".hidden"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        std::fs::write(tmp.path().join("file.txt"), b"x").unwrap();
        tmp
    }

    fn names(t: &FolderTree) -> Vec<(String, usize)> {
        t.rows().iter().skip(1).map(|r| (r.name.clone(), r.depth)).collect()
    }

    fn cur_name(t: &FolderTree) -> String {
        t.rows()[t.selected_row().unwrap()].name.clone()
    }

    #[test]
    fn tree_expand_and_collapse_show_and_hide_children() {
        let tmp = tree_fixture();
        let mut t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], None);
        assert_eq!(t.rows().len(), 1);
        assert!(!t.rows()[0].expanded);
        assert_eq!(t.selected(), None);
        t.toggle(0);
        assert_eq!(names(&t), [("A".to_string(), 1), ("b".to_string(), 1)]);
        t.toggle(1);
        assert_eq!(
            names(&t),
            [("A".to_string(), 1), ("x".to_string(), 2), ("y".to_string(), 2), ("b".to_string(), 1)]
        );
        assert!(t.rows()[1].expanded);
        t.toggle(1);
        assert_eq!(names(&t), [("A".to_string(), 1), ("b".to_string(), 1)]);
        t.toggle(99);
    }

    #[test]
    fn tree_order_is_case_insensitive_with_a_name_tiebreak() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["b", "B", "a", "C"] {
            // a case-insensitive file system cannot hold both b and B
            let _ = std::fs::create_dir(tmp.path().join(d));
        }
        let mut t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], None);
        t.toggle(0);
        let got: Vec<String> = t.rows().iter().skip(1).map(|r| r.name.clone()).collect();
        let mut want = got.clone();
        want.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
        assert_eq!(got, want);
        assert_eq!(got.first().map(String::as_str), Some("a"));
        assert_eq!(got.last().map(String::as_str), Some("C"));
    }

    #[test]
    fn tree_open_at_expands_the_ancestors_and_selects_the_folder() {
        let tmp = tree_fixture();
        let cur = tmp.path().join("A").join("y");
        let t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], Some(&cur));
        assert_eq!(t.selected(), Some(cur.as_path()));
        let i = t.selected_row().unwrap();
        assert_eq!(t.rows()[i].name, "y");
        assert_eq!(t.rows()[i].depth, 2);
        assert!(t.rows()[0].expanded && t.rows()[1].expanded);
        assert!(!t.rows()[i].expanded);
        assert_eq!(t.choose(), Some(cur));
    }

    #[test]
    fn tree_missing_or_unknown_current_selects_nothing() {
        let tmp = tree_fixture();
        let gone = tmp.path().join("nope");
        let outside = Path::new("Z:\\not\\under\\a\\root");
        for cur in [Some(gone.as_path()), None, Some(outside)] {
            let t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], cur);
            assert_eq!(t.selected(), None);
            assert_eq!(t.selected_row(), None);
            assert_eq!(t.choose(), None);
            assert_eq!(t.rows().len(), 1);
            assert!(!t.rows()[0].expanded);
        }
    }

    #[test]
    fn tree_a_drive_root_can_be_the_current_folder() {
        let c = Path::new("C:\\");
        if !c.exists() {
            return;
        }
        let t = FolderTree::open_at(Some(c));
        assert_eq!(t.selected().map(|p| p.to_string_lossy().to_lowercase()), Some("c:\\".to_string()));
        assert!(t.selected_row().is_some());
        let none = FolderTree::open_at(None);
        assert!(none.rows().iter().all(|r| r.depth == 0 && !r.expanded));
    }

    #[test]
    fn tree_step_clamps_at_the_ends() {
        let tmp = tree_fixture();
        let mut t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], None);
        t.toggle(0);
        t.step(1);
        assert_eq!(t.selected_row(), Some(0));
        t.step(-1);
        assert_eq!(t.selected_row(), Some(0));
        t.step(1);
        t.step(1);
        t.step(1);
        assert_eq!(t.selected_row(), Some(2));
        t.step(5);
        assert_eq!(t.selected_row(), Some(2));
    }

    #[test]
    fn tree_right_and_left_walk_the_tree() {
        let tmp = tree_fixture();
        let mut t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], None);
        t.right();
        t.left();
        assert_eq!(t.selected(), None);
        t.step(1);
        t.right(); // expand the root
        assert_eq!(t.rows().len(), 3);
        t.right(); // already expanded: first child
        assert_eq!(cur_name(&t), "A");
        t.right(); // expand A
        assert_eq!(t.rows().len(), 5);
        t.right(); // into A
        assert_eq!(cur_name(&t), "x");
        t.right(); // x is empty: it expands, nothing to enter
        assert_eq!(cur_name(&t), "x");
        t.left(); // collapse x
        assert_eq!(cur_name(&t), "x");
        t.left(); // collapsed: parent
        assert_eq!(cur_name(&t), "A");
        t.left(); // collapse A
        assert_eq!(t.rows().len(), 3);
        t.left(); // collapsed: parent = the root
        assert_eq!(t.selected_row(), Some(0));
        t.left(); // collapse the root
        assert_eq!(t.rows().len(), 1);
        t.left(); // collapsed drive: nothing to do
        assert_eq!(t.selected_row(), Some(0));
    }

    #[test]
    fn tree_collapsing_an_ancestor_of_the_selection_selects_it() {
        let tmp = tree_fixture();
        let cur = tmp.path().join("A").join("x");
        let mut t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], Some(&cur));
        t.toggle(1);
        assert_eq!(t.selected(), Some(tmp.path().join("A").as_path()));
        assert_eq!(t.selected_row(), Some(1));
    }

    #[test]
    fn tree_hidden_folders_and_files_are_skipped() {
        let tmp = tree_fixture();
        let mut t = FolderTree::with_roots(vec![tmp.path().to_path_buf()], None);
        t.toggle(0);
        assert!(t.rows().iter().all(|r| r.name != ".hidden" && r.name != "file.txt"));
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
        // not ".tmp...": the tree skips folders starting with a dot
        let tmp = tempfile::Builder::new().prefix("tree").tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut d = dialog(tmp.path(), "A");
        d.open_picker();
        assert!(d.picker.is_some());
        assert_eq!(d.picker.as_ref().unwrap().selected(), Some(tmp.path()));
        d.picker.as_mut().unwrap().step(1);
        d.close_picker();
        assert!(d.picker.is_none());
        assert_eq!(d.folder(), Some(tmp.path()));
        d.open_picker();
        let t = d.picker.as_mut().unwrap();
        t.right();
        t.right();
        assert_eq!(t.selected(), Some(tmp.path().join("sub").as_path()));
        d.use_picked();
        assert!(d.picker.is_none());
        assert_eq!(d.folder(), Some(tmp.path().join("sub").as_path()));
        // nothing selected: the folder stays
        d.picker = Some(FolderTree::with_roots(vec![tmp.path().to_path_buf()], None));
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
