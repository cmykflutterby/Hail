use anyhow::Result;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
mod uevent;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::OsEventSource;
pub use uevent::is_block_uevent;
#[cfg(target_os = "windows")]
pub use windows::OsEventSource;

/// A blocking source of "something may have changed" signals.
pub trait ChangeSource {
    /// Blocks until a change is signalled (`Ok(true)`) or `timeout` elapses (`Ok(false)`).
    /// `None` waits indefinitely. Bursts of OS notifications are coalesced into one signal.
    fn wait(&mut self, timeout: Option<Duration>) -> Result<bool>;
}

/// Turns OS change signals into delayed rescans.
///
/// OS notifications usually arrive before the volume is mounted (or before a drive
/// letter is assigned), so each signal schedules `RESCANS` rescans spaced `settle` apart.
/// Rescans are snapshot diffs, so repeated scans never duplicate events.
pub struct RescanSchedule {
    settle: Duration,
    due: Option<Instant>,
    remaining: u32,
}

impl RescanSchedule {
    pub const RESCANS: u32 = 3;

    pub fn new(settle: Duration) -> Self {
        Self {
            settle,
            due: None,
            remaining: 0,
        }
    }

    /// Records a change signal. A pending rescan keeps its deadline so a stream of
    /// notifications cannot postpone scanning indefinitely.
    pub fn signal(&mut self, now: Instant) {
        self.remaining = Self::RESCANS;
        if self.due.is_none() {
            self.due = Some(now + self.settle);
        }
    }

    /// How long to wait before the next rescan; `None` means idle until the next signal.
    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Returns true when a rescan is due now and advances the schedule.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.due {
            Some(due) if now >= due => {
                self.remaining = self.remaining.saturating_sub(1);
                self.due = (self.remaining > 0).then(|| now + self.settle);
                true
            }
            _ => false,
        }
    }
}
