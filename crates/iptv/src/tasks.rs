//! Windows Task Scheduler glue: one task per schedule that starts
//! `iptv-player.exe --data-dir <dir> --record <id>` at the start time on the
//! chosen weekdays, whether or not the app is open. Only the id is on the
//! command line (no URL, no credentials). Tasks run as the current user,
//! only while logged on (no password), wake the computer, and start late if
//! the start was missed (the recorder then records the rest of the window).

use anyhow::{anyhow, bail, Result};
use std::path::Path;
use std::process::Command;

use crate::schedule::{Schedule, DAY_NAMES};

/// Task folder + name.
pub fn task_name(id: &str) -> String {
    format!("IPTV Player\\Record {id}")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The task definition (Task Scheduler XML) for `s`.
pub fn task_xml(exe: &Path, data_dir: &Path, s: &Schedule) -> String {
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    let days: String = (0..7).filter(|d| s.days & (1 << d) != 0).map(|d| format!("<{0} />", DAYS[d])).collect();
    let limit = s.length_minutes() as u32 + 60;
    let args = format!("--data-dir \"{}\" --record {}", data_dir.display(), s.id);
    debug_assert_eq!(DAY_NAMES.len(), DAYS.len());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{desc}</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2026-01-01T{start}:00</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByWeek>
        <DaysOfWeek>{days}</DaysOfWeek>
        <WeeksInterval>1</WeeksInterval>
      </ScheduleByWeek>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <WakeToRun>true</WakeToRun>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <ExecutionTimeLimit>PT{h}H{m}M</ExecutionTimeLimit>
    <Enabled>{enabled}</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        desc = esc(&format!("IPTV Player scheduled recording: {}", s.name)),
        start = crate::schedule::fmt_minutes(s.start),
        days = days,
        h = limit / 60,
        m = limit % 60,
        enabled = s.enabled,
        exe = esc(&exe.display().to_string()),
        args = esc(&args),
    )
}

/// UTF-16 LE with a byte order mark: what `schtasks /XML` expects.
fn utf16(s: &str) -> Vec<u8> {
    let mut out = vec![0xff, 0xfe];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

fn schtasks(args: &[&str]) -> Result<(bool, String)> {
    let mut cmd = Command::new("schtasks");
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| anyhow!("could not run schtasks: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text.trim().to_string()))
}

/// Create or replace the task of `s`.
pub fn install(exe: &Path, data_dir: &Path, s: &Schedule) -> Result<()> {
    let xml = data_dir.join(format!("schedule-task-{}.xml", s.id));
    std::fs::write(&xml, utf16(&task_xml(exe, data_dir, s)))
        .map_err(|e| anyhow!("writing the task definition: {e}"))?;
    let name = task_name(&s.id);
    let r = schtasks(&["/Create", "/TN", &name, "/XML", &xml.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&xml);
    let (ok, text) = r?;
    if !ok {
        bail!("Task Scheduler refused the task: {text}");
    }
    Ok(())
}

/// Delete the task of schedule `id` (already gone = fine).
pub fn remove(id: &str) -> Result<()> {
    let name = task_name(id);
    let (ok, text) = schtasks(&["/Delete", "/TN", &name, "/F"])?;
    if ok || !exists(id) {
        Ok(())
    } else {
        bail!("could not delete the task: {text}")
    }
}

/// The task of schedule `id` exists.
pub fn exists(id: &str) -> bool {
    schtasks(&["/Query", "/TN", &task_name(id)]).is_ok_and(|(ok, _)| ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Schedule {
        Schedule {
            id: "s1".into(),
            name: "Sýn <Sport> & co".into(),
            key: "live:1".into(),
            days: 1 << 6,
            start: 18 * 60 + 30,
            end: 19 * 60 + 45,
            enabled: true,
        }
    }

    #[test]
    fn xml_has_the_trigger_the_action_and_no_credentials() {
        let xml = task_xml(Path::new("C:\\Apps\\iptv player\\iptv-player.exe"), Path::new("C:\\Apps\\iptv player"), &s());
        assert!(xml.contains("<StartBoundary>2026-01-01T18:30:00</StartBoundary>"));
        assert!(xml.contains("<DaysOfWeek><Sunday /></DaysOfWeek>"));
        assert!(xml.contains("<Command>C:\\Apps\\iptv player\\iptv-player.exe</Command>"));
        assert!(xml.contains("--data-dir &quot;C:\\Apps\\iptv player&quot; --record s1"));
        assert!(xml.contains("<WakeToRun>true</WakeToRun>") && xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT2H15M</ExecutionTimeLimit>"), "75 min + 60 min");
        assert!(xml.contains("Sýn &lt;Sport&gt; &amp; co"), "the name is escaped");
        assert!(xml.contains("<Enabled>true</Enabled>"));
        let mut off = s();
        off.enabled = false;
        assert!(task_xml(Path::new("a.exe"), Path::new("."), &off).contains("<Enabled>false</Enabled>\n  </Settings>"));
    }

    /// Registers a real task and deletes it again: `cargo test -p iptv -- --ignored real_task`.
    #[test]
    #[ignore]
    fn real_task_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let mut s = s();
        s.id = "test-roundtrip".into();
        s.name = "Round trip".into();
        let exe = std::env::current_exe().unwrap();
        install(&exe, d.path(), &s).unwrap();
        assert!(exists(&s.id));
        s.enabled = false;
        install(&exe, d.path(), &s).unwrap(); // replaces it
        remove(&s.id).unwrap();
        assert!(!exists(&s.id));
        remove(&s.id).unwrap(); // already gone is fine
    }

    #[test]
    fn utf16_has_a_bom() {
        assert_eq!(utf16("a"), vec![0xff, 0xfe, b'a', 0]);
        assert_eq!(task_name("s2"), "IPTV Player\\Record s2");
    }
}
