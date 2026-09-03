//! When to run each probe mode.
//!
//! Polling only happens while the server pane is visible; a hidden pane costs
//! the remote nothing. Time is milliseconds so the schedule is testable.

pub const SYS_INTERVAL_MS: u64 = 5_000;
pub const PORTS_INTERVAL_MS: u64 = 15_000;
pub const SESSIONS_INTERVAL_MS: u64 = 15_000;
pub const AGENTS_INTERVAL_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollKind {
    Sys,
    Ports,
    Sessions,
    Agents,
}

impl PollKind {
    pub fn interval_ms(self) -> u64 {
        match self {
            PollKind::Sys => SYS_INTERVAL_MS,
            PollKind::Ports => PORTS_INTERVAL_MS,
            PollKind::Sessions => SESSIONS_INTERVAL_MS,
            PollKind::Agents => AGENTS_INTERVAL_MS,
        }
    }

    pub const ALL: [PollKind; 4] = [
        PollKind::Sys,
        PollKind::Ports,
        PollKind::Sessions,
        PollKind::Agents,
    ];
}

#[derive(Debug, Clone)]
pub struct PollSchedule {
    visible: bool,
    last_run_ms: [Option<u64>; 4],
}

impl Default for PollSchedule {
    fn default() -> Self {
        Self::new()
    }
}

impl PollSchedule {
    pub fn new() -> Self {
        Self {
            visible: false,
            last_run_ms: [None; 4],
        }
    }

    /// Hiding the pane clears the timers, so showing it again refreshes at once
    /// rather than displaying stale numbers.
    pub fn set_visible(&mut self, visible: bool) {
        if self.visible != visible {
            self.visible = visible;
            self.last_run_ms = [None; 4];
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Modes that are due now. Marks them as run.
    pub fn due(&mut self, now_ms: u64) -> Vec<PollKind> {
        if !self.visible {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (i, kind) in PollKind::ALL.into_iter().enumerate() {
            let due = match self.last_run_ms[i] {
                None => true,
                Some(last) => now_ms.saturating_sub(last) >= kind.interval_ms(),
            };
            if due {
                self.last_run_ms[i] = Some(now_ms);
                out.push(kind);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_panes_never_poll() {
        let mut s = PollSchedule::new();
        assert!(s.due(0).is_empty());
        assert!(s.due(1_000_000).is_empty());
    }

    #[test]
    fn showing_polls_everything_at_once() {
        let mut s = PollSchedule::new();
        s.set_visible(true);
        assert_eq!(s.due(0).len(), 4);
    }

    #[test]
    fn sys_polls_three_times_as_often() {
        let mut s = PollSchedule::new();
        s.set_visible(true);
        s.due(0);
        assert_eq!(s.due(5_000), vec![PollKind::Sys]);
        assert_eq!(s.due(10_000), vec![PollKind::Sys]);
        assert_eq!(s.due(15_000).len(), 4);
    }

    #[test]
    fn nothing_is_due_before_its_interval() {
        let mut s = PollSchedule::new();
        s.set_visible(true);
        s.due(0);
        assert!(s.due(4_999).is_empty());
    }

    #[test]
    fn hiding_and_showing_forces_a_refresh() {
        let mut s = PollSchedule::new();
        s.set_visible(true);
        s.due(0);
        s.set_visible(false);
        s.set_visible(true);
        assert_eq!(s.due(1_000).len(), 4);
    }
}
