//! Keeps the computer from sleeping while a value is alive.

/// Keeps the computer from sleeping while alive.
pub struct KeepAwake;

impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake::set(false)
    }

    /// Also keeps the display on (ES_DISPLAY_REQUIRED). On Modern Standby
    /// machines the system can suspend when the display turns off or the
    /// session locks unless the display is required too.
    pub fn with_display() -> KeepAwake {
        KeepAwake::set(true)
    }

    fn set(display: bool) -> KeepAwake {
        #[cfg(windows)]
        unsafe {
            SetThreadExecutionState(flags(display));
        }
        let _ = display;
        KeepAwake
    }
}

/// Execution-state flags: ES_CONTINUOUS | ES_SYSTEM_REQUIRED, plus
/// ES_DISPLAY_REQUIRED when `display`.
fn flags(display: bool) -> u32 {
    0x8000_0001 | if display { 0x2 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::flags;

    #[test]
    fn flags_with_and_without_display() {
        assert_eq!(flags(false), 0x8000_0001);
        assert_eq!(flags(true), 0x8000_0003);
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        KeepAwake::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            SetThreadExecutionState(0x8000_0000); // ES_CONTINUOUS
        }
    }
}

#[cfg(windows)]
extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}
