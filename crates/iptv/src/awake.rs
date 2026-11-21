//! Keeps the computer from sleeping while a value is alive.

/// Keeps the computer from sleeping while alive.
pub struct KeepAwake;

impl KeepAwake {
    pub fn new() -> KeepAwake {
        #[cfg(windows)]
        unsafe {
            SetThreadExecutionState(0x8000_0001); // ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        }
        KeepAwake
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
