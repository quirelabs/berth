//! Schema and writes. One SQLite file, WAL mode, opened once per process.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{CHUNK_TARGET_BYTES, Result, ZSTD_LEVEL, ansi};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY,
  host_alias TEXT NOT NULL,
  tmux_name TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at INTEGER
);

CREATE TABLE IF NOT EXISTS chunks (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  offset_ms INTEGER NOT NULL,
  data BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_session_offset ON chunks(session_id, offset_ms);

CREATE TABLE IF NOT EXISTS commands (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  offset_ms INTEGER NOT NULL,
  cwd TEXT,
  command TEXT NOT NULL,
  exit_code INTEGER
);
CREATE INDEX IF NOT EXISTS commands_session ON commands(session_id, offset_ms);

CREATE VIRTUAL TABLE IF NOT EXISTS commands_fts
  USING fts5(command, content='commands', content_rowid='id');

CREATE TRIGGER IF NOT EXISTS commands_ai AFTER INSERT ON commands BEGIN
  INSERT INTO commands_fts(rowid, command) VALUES (new.id, new.command);
END;
CREATE TRIGGER IF NOT EXISTS commands_ad AFTER DELETE ON commands BEGIN
  INSERT INTO commands_fts(commands_fts, rowid, command) VALUES('delete', old.id, old.command);
END;
CREATE TRIGGER IF NOT EXISTS commands_au AFTER UPDATE ON commands BEGIN
  INSERT INTO commands_fts(commands_fts, rowid, command) VALUES('delete', old.id, old.command);
  INSERT INTO commands_fts(rowid, command) VALUES (new.id, new.command);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS output_fts
  USING fts5(text, session_id UNINDEXED, offset_ms UNINDEXED);
"#;

pub struct Timeline {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: i64,
    pub host_alias: String,
    pub tmux_name: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRow {
    pub id: i64,
    pub session_id: i64,
    pub offset_ms: i64,
    pub cwd: Option<String>,
    pub command: String,
    pub exit_code: Option<i64>,
}

impl Timeline {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn start_session(&self, host_alias: &str, tmux_name: &str, started_at: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (host_alias, tmux_name, started_at) VALUES (?1, ?2, ?3)",
            params![host_alias, tmux_name, started_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn end_session(&self, session_id: i64, ended_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE id = ?1",
            params![session_id, ended_at],
        )?;
        Ok(())
    }

    pub fn session(&self, id: i64) -> Result<Option<SessionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, host_alias, tmux_name, started_at, ended_at FROM sessions WHERE id = ?1",
                params![id],
                |r| {
                    Ok(SessionRow {
                        id: r.get(0)?,
                        host_alias: r.get(1)?,
                        tmux_name: r.get(2)?,
                        started_at: r.get(3)?,
                        ended_at: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Store one chunk of raw PTY output and index its visible text.
    pub fn write_chunk(&self, session_id: i64, offset_ms: i64, raw: &[u8]) -> Result<i64> {
        let compressed = zstd::encode_all(raw, ZSTD_LEVEL)?;
        self.conn.execute(
            "INSERT INTO chunks (session_id, offset_ms, data) VALUES (?1, ?2, ?3)",
            params![session_id, offset_ms, compressed],
        )?;
        let chunk_id = self.conn.last_insert_rowid();
        self.index_output(session_id, offset_ms, raw)?;
        Ok(chunk_id)
    }

    fn index_output(&self, session_id: i64, offset_ms: i64, raw: &[u8]) -> Result<()> {
        let text = ansi::strip(&String::from_utf8_lossy(raw));
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO output_fts (text, session_id, offset_ms) VALUES (?1, ?2, ?3)",
        )?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            stmt.execute(params![line, session_id, offset_ms])?;
        }
        Ok(())
    }

    pub fn read_chunk(&self, chunk_id: i64) -> Result<Option<Vec<u8>>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM chunks WHERE id = ?1",
                params![chunk_id],
                |r| r.get(0),
            )
            .optional()?;
        match blob {
            Some(b) => Ok(Some(zstd::decode_all(b.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Every chunk for a session in order, for replay.
    pub fn replay(&self, session_id: i64) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT offset_ms, data FROM chunks WHERE session_id = ?1 ORDER BY offset_ms, id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (offset, blob) = row?;
            out.push((offset, zstd::decode_all(blob.as_slice())?));
        }
        Ok(out)
    }

    pub fn record_command(
        &self,
        session_id: i64,
        offset_ms: i64,
        cwd: Option<&str>,
        command: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO commands (session_id, offset_ms, cwd, command) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, offset_ms, cwd, command],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_exit_code(&self, command_id: i64, exit_code: i32) -> Result<()> {
        self.conn.execute(
            "UPDATE commands SET exit_code = ?2 WHERE id = ?1",
            params![command_id, exit_code],
        )?;
        Ok(())
    }

    /// Total size of stored chunk data, used by retention.
    pub fn chunk_bytes(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM chunks",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }
}

/// A buffer that accumulates PTY output and flushes on size.
#[derive(Debug, Default)]
pub struct ChunkBuffer {
    buf: Vec<u8>,
    first_offset_ms: Option<i64>,
}

impl ChunkBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append output. Returns a chunk when the buffer reaches its target.
    pub fn push(&mut self, offset_ms: i64, data: &[u8]) -> Option<(i64, Vec<u8>)> {
        if self.buf.is_empty() {
            self.first_offset_ms = Some(offset_ms);
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK_TARGET_BYTES {
            self.take()
        } else {
            None
        }
    }

    /// Flush whatever is buffered, e.g. when a session ends.
    pub fn take(&mut self) -> Option<(i64, Vec<u8>)> {
        if self.buf.is_empty() {
            return None;
        }
        let offset = self.first_offset_ms.take().unwrap_or(0);
        Some((offset, std::mem::take(&mut self.buf)))
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline() -> Timeline {
        Timeline::open_in_memory().unwrap()
    }

    #[test]
    fn schema_applies_cleanly_twice() {
        let t = timeline();
        t.conn().execute_batch(SCHEMA).unwrap();
    }

    #[test]
    fn session_round_trip() {
        let t = timeline();
        let id = t.start_session("web-1", "berth/main", 1_000).unwrap();
        let s = t.session(id).unwrap().unwrap();
        assert_eq!(s.host_alias, "web-1");
        assert_eq!(s.ended_at, None);
        t.end_session(id, 2_000).unwrap();
        assert_eq!(t.session(id).unwrap().unwrap().ended_at, Some(2_000));
    }

    #[test]
    fn chunks_round_trip_through_compression() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        let raw = b"hello world\n".repeat(1000);
        let cid = t.write_chunk(sid, 0, &raw).unwrap();
        assert_eq!(t.read_chunk(cid).unwrap().unwrap(), raw);
    }

    #[test]
    fn compression_actually_shrinks_terminal_output() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        let raw = b"the quick brown fox\n".repeat(4000);
        t.write_chunk(sid, 0, &raw).unwrap();
        let stored = t.chunk_bytes().unwrap();
        assert!(
            stored < raw.len() as u64 / 10,
            "stored {stored} vs raw {}",
            raw.len()
        );
    }

    #[test]
    fn replay_returns_chunks_in_order() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        t.write_chunk(sid, 200, b"second").unwrap();
        t.write_chunk(sid, 100, b"first").unwrap();
        let chunks = t.replay(sid).unwrap();
        assert_eq!(chunks[0], (100, b"first".to_vec()));
        assert_eq!(chunks[1], (200, b"second".to_vec()));
    }

    #[test]
    fn missing_chunk_is_none() {
        assert!(timeline().read_chunk(999).unwrap().is_none());
    }

    #[test]
    fn commands_reach_the_fts_index() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        t.record_command(sid, 0, Some("/srv"), "systemctl restart nginx")
            .unwrap();
        let n: i64 = t
            .conn()
            .query_row(
                "SELECT count(*) FROM commands_fts WHERE commands_fts MATCH 'nginx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn deleting_a_command_clears_its_index_entry() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        let cid = t.record_command(sid, 0, None, "rare_token_xyz").unwrap();
        t.conn()
            .execute("DELETE FROM commands WHERE id = ?1", params![cid])
            .unwrap();
        let n: i64 = t
            .conn()
            .query_row(
                "SELECT count(*) FROM commands_fts WHERE commands_fts MATCH 'rare_token_xyz'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn output_indexing_strips_escape_sequences() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        t.write_chunk(sid, 0, b"\x1b[31mfailed to bind\x1b[0m\n")
            .unwrap();
        let text: String = t
            .conn()
            .query_row("SELECT text FROM output_fts LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(text, "failed to bind");
    }

    #[test]
    fn exit_code_updates() {
        let t = timeline();
        let sid = t.start_session("h", "berth/main", 0).unwrap();
        let cid = t.record_command(sid, 0, None, "false").unwrap();
        t.set_exit_code(cid, 1).unwrap();
        let code: i64 = t
            .conn()
            .query_row(
                "SELECT exit_code FROM commands WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn chunk_buffer_flushes_at_target_size() {
        let mut b = ChunkBuffer::new();
        assert!(b.push(0, &[0u8; 1024]).is_none());
        let flushed = b.push(10, &vec![0u8; CHUNK_TARGET_BYTES]);
        let (offset, data) = flushed.expect("should flush");
        assert_eq!(offset, 0, "offset is that of the first byte in the chunk");
        assert_eq!(data.len(), 1024 + CHUNK_TARGET_BYTES);
        assert!(b.is_empty());
    }

    #[test]
    fn chunk_buffer_take_on_empty_is_none() {
        assert!(ChunkBuffer::new().take().is_none());
    }
}
