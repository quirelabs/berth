//! Pruning, run once at startup.
//!
//! Two limits apply: an age in days and a total size in bytes. Whichever bites
//! first wins, and the oldest chunks go first.

use rusqlite::params;

use crate::{Result, Timeline};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub retention_days: u32,
    pub max_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            retention_days: 30,
            max_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

impl Policy {
    pub fn from_settings(retention_days: u32, max_gb: f64) -> Self {
        Self {
            retention_days,
            max_bytes: (max_gb.max(0.0) * 1024.0 * 1024.0 * 1024.0) as u64,
        }
    }

    pub fn cutoff(&self, now_secs: i64) -> i64 {
        now_secs.saturating_sub(i64::from(self.retention_days) * 86_400)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    pub chunks: usize,
    pub sessions: usize,
}

/// Apply `policy`, deleting oldest first. FTS rows for pruned chunks go too,
/// so search never returns a hit whose output has been discarded.
pub fn prune(timeline: &Timeline, policy: Policy, now_secs: i64) -> Result<Pruned> {
    let conn = timeline.conn();
    let mut pruned = Pruned::default();

    // Age first: whole sessions that ended before the cutoff.
    let cutoff = policy.cutoff(now_secs);
    let stale: Vec<i64> = conn
        .prepare("SELECT id FROM sessions WHERE COALESCE(ended_at, started_at) < ?1")?
        .query_map(params![cutoff], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for id in stale {
        pruned.chunks += delete_session(timeline, id)?;
        pruned.sessions += 1;
    }

    // Then size: drop oldest sessions until the budget is met.
    while timeline.chunk_bytes()? > policy.max_bytes {
        let oldest: Option<i64> = conn
            .prepare("SELECT session_id FROM chunks ORDER BY offset_ms, id LIMIT 1")?
            .query_map([], |r| r.get(0))?
            .next()
            .transpose()?;
        let Some(id) = oldest else { break };
        pruned.chunks += delete_session(timeline, id)?;
        pruned.sessions += 1;
    }

    Ok(pruned)
}

/// Delete one session and everything that references it. Also the action
/// behind "delete this session's history" in the UI.
pub fn delete_session(timeline: &Timeline, session_id: i64) -> Result<usize> {
    let conn = timeline.conn();
    let chunks = conn.execute(
        "DELETE FROM chunks WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM output_fts WHERE session_id = ?1",
        params![session_id],
    )?;
    // The trigger on `commands` keeps commands_fts in step.
    conn.execute(
        "DELETE FROM commands WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn seeded() -> (Timeline, i64, i64) {
        let t = Timeline::open_in_memory().unwrap();
        let old = t.start_session("h", "berth/old", 0).unwrap();
        t.end_session(old, 0).unwrap();
        t.write_chunk(old, 0, b"old output\n").unwrap();
        t.record_command(old, 0, None, "old_command").unwrap();

        let recent = t.start_session("h", "berth/new", 100 * DAY).unwrap();
        t.end_session(recent, 100 * DAY).unwrap();
        t.write_chunk(recent, 0, b"new output\n").unwrap();
        (t, old, recent)
    }

    #[test]
    fn prunes_sessions_past_the_age_limit() {
        let (t, old, recent) = seeded();
        let policy = Policy {
            retention_days: 30,
            max_bytes: u64::MAX,
        };
        let pruned = prune(&t, policy, 100 * DAY).unwrap();
        assert_eq!(pruned.sessions, 1);
        assert!(t.session(old).unwrap().is_none());
        assert!(t.session(recent).unwrap().is_some());
    }

    #[test]
    fn pruning_clears_the_output_index() {
        let (t, _, _) = seeded();
        let policy = Policy {
            retention_days: 30,
            max_bytes: u64::MAX,
        };
        prune(&t, policy, 100 * DAY).unwrap();
        let n: i64 = t
            .conn()
            .query_row(
                "SELECT count(*) FROM output_fts WHERE output_fts MATCH 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn pruning_clears_the_command_index() {
        let (t, _, _) = seeded();
        let policy = Policy {
            retention_days: 30,
            max_bytes: u64::MAX,
        };
        prune(&t, policy, 100 * DAY).unwrap();
        let n: i64 = t
            .conn()
            .query_row(
                "SELECT count(*) FROM commands_fts WHERE commands_fts MATCH 'old_command'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn size_limit_drops_until_under_budget() {
        let t = Timeline::open_in_memory().unwrap();
        for i in 0..5 {
            let sid = t.start_session("h", "berth/s", i * DAY).unwrap();
            t.end_session(sid, i * DAY).unwrap();
            // Random-ish bytes so zstd cannot collapse them to nothing.
            let data: Vec<u8> = (0..8192u32)
                .map(|n| (n.wrapping_mul(2654435761) >> 13) as u8)
                .collect();
            t.write_chunk(sid, 0, &data).unwrap();
        }
        let before = t.chunk_bytes().unwrap();
        assert!(before > 0);
        let policy = Policy {
            retention_days: 36_500,
            max_bytes: before / 2,
        };
        prune(&t, policy, 5 * DAY).unwrap();
        assert!(t.chunk_bytes().unwrap() <= before / 2);
    }

    #[test]
    fn pruning_an_empty_database_is_a_no_op() {
        let t = Timeline::open_in_memory().unwrap();
        assert_eq!(prune(&t, Policy::default(), 0).unwrap(), Pruned::default());
    }

    #[test]
    fn policy_converts_gigabytes() {
        let p = Policy::from_settings(30, 2.0);
        assert_eq!(p.max_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(Policy::from_settings(30, -1.0).max_bytes, 0);
    }

    #[test]
    fn deleting_one_session_leaves_the_others() {
        let (t, old, recent) = seeded();
        delete_session(&t, old).unwrap();
        assert!(t.session(old).unwrap().is_none());
        assert_eq!(t.replay(recent).unwrap().len(), 1);
    }
}
