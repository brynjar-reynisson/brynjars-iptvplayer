//! The "new schedule" form of the Recordings tab (no GUI types).

use iptv::schedule::{parse_hhmm, Schedule};

/// What the user has filled in so far.
#[derive(Debug, Clone, Default)]
pub struct SchedForm {
    /// Channel name and favorite key.
    pub channel: Option<(String, String)>,
    /// Weekdays, bit 0 = Monday.
    pub days: u8,
    pub start: String,
    pub end: String,
}

/// A validated form.
#[derive(Debug, PartialEq, Eq)]
pub struct NewSchedule {
    pub name: String,
    pub key: String,
    pub days: u8,
    pub start: u16,
    pub end: u16,
}

impl SchedForm {
    pub fn toggle_day(&mut self, day: usize) {
        if day < 7 {
            self.days ^= 1 << day;
        }
    }

    pub fn has_day(&self, day: usize) -> bool {
        self.days & (1 << day) != 0
    }

    /// The schedule to add, or what is wrong with the form.
    pub fn build(&self) -> Result<NewSchedule, String> {
        let (name, key) = self.channel.clone().ok_or("Select a channel in the Live tab, then press \"Use selected channel\".")?;
        let start = parse_hhmm(&self.start).ok_or("The start time must look like 18:30.")?;
        let end = parse_hhmm(&self.end).ok_or("The end time must look like 19:45.")?;
        let probe = Schedule { id: String::new(), name: name.clone(), key: key.clone(), days: self.days, start, end, enabled: true };
        probe.validate().map_err(str::to_string)?;
        Ok(NewSchedule { name, key, days: self.days, start, end })
    }

    /// Back to empty after a schedule was added (the channel stays: adding
    /// the same channel on other days is common).
    pub fn clear_times(&mut self) {
        self.days = 0;
        self.start.clear();
        self.end.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> SchedForm {
        SchedForm {
            channel: Some(("Syn Sport".into(), "live:1".into())),
            days: 1 << 6,
            start: "18:30".into(),
            end: "19:45".into(),
        }
    }

    #[test]
    fn a_complete_form_builds() {
        assert_eq!(
            filled().build().unwrap(),
            NewSchedule { name: "Syn Sport".into(), key: "live:1".into(), days: 64, start: 1110, end: 1185 }
        );
    }

    #[test]
    fn each_missing_piece_has_its_own_message() {
        let mut f = filled();
        f.channel = None;
        assert!(f.build().unwrap_err().contains("channel"));
        let mut f = filled();
        f.start = "6pm".into();
        assert!(f.build().unwrap_err().contains("start time"));
        let mut f = filled();
        f.end = "".into();
        assert!(f.build().unwrap_err().contains("end time"));
        let mut f = filled();
        f.days = 0;
        assert!(f.build().unwrap_err().contains("weekday"));
        let mut f = filled();
        f.end = "18:30".into();
        assert!(f.build().unwrap_err().contains("differ"));
    }

    #[test]
    fn days_toggle_and_clear() {
        let mut f = SchedForm::default();
        f.toggle_day(6);
        f.toggle_day(0);
        assert!(f.has_day(6) && f.has_day(0) && !f.has_day(3));
        f.toggle_day(6);
        assert!(!f.has_day(6));
        f.toggle_day(9);
        assert_eq!(f.days, 1);
        f.channel = Some(("A".into(), "k".into()));
        f.clear_times();
        assert_eq!(f.days, 0);
        assert!(f.channel.is_some());
    }
}
