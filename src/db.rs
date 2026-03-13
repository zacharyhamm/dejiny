use crate::format::{RecEvent, RecordingHeader, parse_events};
use rusqlite::Connection;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const SQLITE_BUSY_TIMEOUT_MS: u64 = 500;
const COMMAND_LOAD_LIMIT: u32 = 50000;

pub fn history_path() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".local/share")
        });
    data_dir.join("dejiny")
}

pub fn log_error(msg: &str) {
    let log_path = history_path().join("error.log");
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

pub fn open_db() -> anyhow::Result<Connection> {
    let dir = history_path();
    open_db_at(&dir)
}

pub fn open_db_at(dir: &std::path::Path) -> anyhow::Result<Connection> {
    fs::create_dir_all(dir)?;

    let db_path = dir.join("history.db");
    let conn = Connection::open(&db_path)?;

    conn.busy_timeout(std::time::Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS commands (
            id        INTEGER PRIMARY KEY,
            command   TEXT NOT NULL,
            exit_code INTEGER NOT NULL,
            start     REAL NOT NULL,
            end       REAL NOT NULL,
            cwd       TEXT NOT NULL,
            hostname  TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_cmd_start ON commands(command, start)",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS recording_chunks (
            command_id INTEGER NOT NULL,
            seq        INTEGER NOT NULL,
            data       BLOB NOT NULL,
            PRIMARY KEY (command_id, seq)
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS summary_blacklist (
            id      INTEGER PRIMARY KEY,
            pattern TEXT NOT NULL UNIQUE
        )",
        [],
    )?;

    // Migration: add summary column (silently ignore if already exists)
    let _ = conn.execute("ALTER TABLE commands ADD COLUMN summary TEXT", []);

    Ok(conn)
}

/// Returns true if the command matches any regex pattern in the summary_blacklist table.
pub fn is_command_blacklisted(conn: &Connection, command: &str) -> bool {
    let mut stmt = match conn.prepare("SELECT pattern FROM summary_blacklist") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let patterns = match stmt.query_map([], |row| row.get::<_, String>(0)) {
        Ok(rows) => rows,
        Err(_) => return false,
    };
    for pat in patterns.flatten() {
        if let Ok(re) = regex::Regex::new(&pat)
            && re.is_match(command)
        {
            return true;
        }
    }
    false
}

#[derive(Clone)]
pub struct HistoryEntry {
    pub id: i64,
    pub command: String,
    pub exit_code: i32,
    pub start: f64,
    pub cwd: String,
    pub has_recording: bool,
    pub summary: Option<String>,
}

impl AsRef<str> for HistoryEntry {
    fn as_ref(&self) -> &str {
        &self.command
    }
}

pub struct LoadedRecording {
    pub header: RecordingHeader,
    pub data: Vec<u8>,
    pub events: Vec<RecEvent>,
}

impl LoadedRecording {
    pub fn concatenate_event_data(&self) -> Vec<u8> {
        let mut raw = Vec::new();
        for event in &self.events {
            raw.extend_from_slice(&self.data[event.offset..event.offset + event.length]);
        }
        raw
    }
}

pub fn load_recording(conn: &Connection, id: i64) -> anyhow::Result<LoadedRecording> {
    let mut chunk_stmt =
        conn.prepare("SELECT data FROM recording_chunks WHERE command_id = ?1 ORDER BY seq")?;
    let chunks: Vec<Vec<u8>> = chunk_stmt
        .query_map([id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    if chunks.is_empty() {
        anyhow::bail!("no recording for this command");
    }
    let data = {
        let mut buf = Vec::new();
        for chunk in &chunks {
            let decompressed = zstd::decode_all(&chunk[..])?;
            buf.extend_from_slice(&decompressed);
        }
        buf
    };

    let header = RecordingHeader::decode(&data)?;
    let events = parse_events(&data)?;

    Ok(LoadedRecording {
        header,
        data,
        events,
    })
}

pub struct CommandMeta {
    pub command: String,
    pub exit_code: i32,
    pub start: f64,
    pub end: f64,
    pub cwd: String,
}

pub fn load_command_meta(conn: &Connection, id: i64) -> Option<CommandMeta> {
    conn.query_row(
        "SELECT command, exit_code, start, end, cwd FROM commands WHERE id = ?1",
        [id],
        |row| {
            Ok(CommandMeta {
                command: row.get(0)?,
                exit_code: row.get(1)?,
                start: row.get(2)?,
                end: row.get(3)?,
                cwd: row.get(4)?,
            })
        },
    )
    .ok()
}

pub fn load_commands() -> anyhow::Result<Vec<HistoryEntry>> {
    let conn = open_db()?;
    let query = format!(
        "SELECT id, command, exit_code, start, cwd,
                EXISTS (SELECT 1 FROM recording_chunks WHERE command_id = commands.id) as has_recording,
                summary
         FROM commands
         ORDER BY start DESC
         LIMIT {COMMAND_LOAD_LIMIT}"
    );
    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map([], |row| {
        Ok(HistoryEntry {
            id: row.get(0)?,
            command: row.get(1)?,
            exit_code: row.get(2)?,
            start: row.get(3)?,
            cwd: row.get(4)?,
            has_recording: row.get(5)?,
            summary: row.get(6)?,
        })
    })?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}
