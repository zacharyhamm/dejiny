use crate::db::open_db;
use rusqlite::TransactionBehavior;
use std::fs;
use std::path::PathBuf;

/// Import shell history from zsh and/or bash history files into the dejiny database.
pub fn import(zsh_path: Option<PathBuf>, bash_path: Option<PathBuf>, dry_run: bool) {
    let zsh_file = zsh_path.or_else(default_zsh_history);
    let bash_file = bash_path.or_else(default_bash_history);

    if zsh_file.is_none() && bash_file.is_none() {
        eprintln!("dejiny: no history files found to import");
        return;
    }

    let mut entries: Vec<ImportEntry> = Vec::new();

    if let Some(path) = &zsh_file {
        match parse_zsh_history(path) {
            Ok(parsed) => {
                eprintln!(
                    "Parsed {} entries from zsh history: {}",
                    parsed.len(),
                    path.display()
                );
                entries.extend(parsed);
            }
            Err(e) => eprintln!("dejiny: failed to read zsh history {}: {e}", path.display()),
        }
    }

    if let Some(path) = &bash_file {
        match parse_bash_history(path) {
            Ok(parsed) => {
                eprintln!(
                    "Parsed {} entries from bash history: {}",
                    parsed.len(),
                    path.display()
                );
                entries.extend(parsed);
            }
            Err(e) => eprintln!(
                "dejiny: failed to read bash history {}: {e}",
                path.display()
            ),
        }
    }

    if entries.is_empty() {
        eprintln!("dejiny: no entries found to import");
        return;
    }

    // Sort by timestamp so entries are inserted in chronological order
    entries.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if dry_run {
        eprintln!("Dry run — would import {} entries:", entries.len());
        for (i, e) in entries.iter().enumerate().take(20) {
            let ts = format_timestamp(e.start);
            eprintln!("  [{:>5}] {} | {}", i + 1, ts, e.command);
        }
        if entries.len() > 20 {
            eprintln!("  ... and {} more", entries.len() - 20);
        }
        return;
    }

    let mut conn = match open_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dejiny: failed to open database: {e}");
            return;
        }
    };

    // Use a longer busy timeout for import since it holds the lock for a while
    // and the shell hook may be competing for writes.
    if let Err(e) = conn.busy_timeout(std::time::Duration::from_secs(5)) {
        eprintln!("dejiny: failed to set busy timeout: {e}");
        return;
    }

    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".into());

    let mut imported = 0u64;
    let mut skipped = 0u64;

    // Use an IMMEDIATE transaction to acquire the write lock upfront rather than
    // failing with "database is locked" on individual inserts when the shell hook
    // is competing for writes.
    let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("dejiny: failed to begin transaction: {e}");
            return;
        }
    };

    for entry in &entries {
        // Skip if an identical command+start already exists (avoid duplicates)
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM commands WHERE command = ?1 AND start = ?2)",
                rusqlite::params![entry.command, entry.start],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if exists {
            skipped += 1;
            continue;
        }

        if let Err(e) = tx.execute(
            "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                entry.command,
                entry.exit_code,
                entry.start,
                entry.end,
                entry.cwd,
                hostname,
            ],
        ) {
            eprintln!("dejiny: failed to insert entry: {e}");
            continue;
        }
        imported += 1;
    }

    if let Err(e) = tx.commit() {
        eprintln!("dejiny: failed to commit transaction: {e}");
        return;
    }

    eprintln!("Imported {imported} entries ({skipped} duplicates skipped)");
}

struct ImportEntry {
    command: String,
    exit_code: i32,
    start: f64,
    end: f64,
    cwd: String,
}

fn default_zsh_history() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(&home).join(".zsh_history");
    path.exists().then_some(path)
}

fn default_bash_history() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(&home).join(".bash_history");
    path.exists().then_some(path)
}

/// Parse zsh history file.
///
/// Zsh extended history format: `: <timestamp>:<duration>;<command>`
/// Plain format: just the command text, one per line.
/// Multi-line commands have continuation lines starting with a backslash at the end of the
/// previous line.
fn parse_zsh_history(path: &PathBuf) -> anyhow::Result<Vec<ImportEntry>> {
    let data = fs::read(path)?;
    // Zsh history can contain invalid UTF-8, so use lossy conversion
    let content = String::from_utf8_lossy(&data);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());

    let mut entries = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if let Some(rest) = line.strip_prefix(": ") {
            // Extended history format: `: timestamp:duration;command`
            if let Some((meta, cmd_start)) = rest.split_once(';') {
                let parts: Vec<&str> = meta.splitn(2, ':').collect();
                let timestamp: f64 = parts[0].trim().parse().unwrap_or(0.0);
                let duration: f64 = parts
                    .get(1)
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0.0);

                // Handle multi-line commands (lines ending with \)
                let mut command = cmd_start.to_string();
                while command.ends_with('\\') && i + 1 < lines.len() {
                    command.pop(); // remove trailing backslash
                    i += 1;
                    command.push('\n');
                    command.push_str(lines[i]);
                }

                let command = command.trim().to_string();
                if !command.is_empty() {
                    entries.push(ImportEntry {
                        command,
                        exit_code: 0,
                        start: timestamp,
                        end: timestamp + duration,
                        cwd: home.clone(),
                    });
                }
            }
        } else if !line.trim().is_empty() {
            // Plain format — no timestamp available, use 0
            // Handle multi-line commands
            let mut command = line.to_string();
            while command.ends_with('\\') && i + 1 < lines.len() {
                command.pop();
                i += 1;
                command.push('\n');
                command.push_str(lines[i]);
            }

            let command = command.trim().to_string();
            if !command.is_empty() {
                entries.push(ImportEntry {
                    command,
                    exit_code: 0,
                    start: 0.0,
                    end: 0.0,
                    cwd: home.clone(),
                });
            }
        }

        i += 1;
    }

    Ok(entries)
}

/// Parse bash history file.
///
/// Two formats:
/// - With timestamps: `#<timestamp>` line followed by command line
/// - Without timestamps: plain command lines
fn parse_bash_history(path: &PathBuf) -> anyhow::Result<Vec<ImportEntry>> {
    let data = fs::read(path)?;
    let content = String::from_utf8_lossy(&data);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());

    let mut entries = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if let Some(ts_str) = line.strip_prefix('#') {
            // Timestamp line — next line is the command
            if let Ok(timestamp) = ts_str.trim().parse::<f64>() {
                if i + 1 < lines.len() {
                    i += 1;
                    let command = lines[i].trim().to_string();
                    if !command.is_empty() && !command.starts_with('#') {
                        entries.push(ImportEntry {
                            command,
                            exit_code: 0,
                            start: timestamp,
                            end: timestamp,
                            cwd: home.clone(),
                        });
                    }
                }
            } else {
                // Not a valid timestamp, treat as a regular command (e.g., comment in script)
                // Skip it
            }
        } else if !line.trim().is_empty() {
            // Plain command, no timestamp
            entries.push(ImportEntry {
                command: line.trim().to_string(),
                exit_code: 0,
                start: 0.0,
                end: 0.0,
                cwd: home.clone(),
            });
        }

        i += 1;
    }

    Ok(entries)
}

fn format_timestamp(ts: f64) -> String {
    if ts == 0.0 {
        return "unknown time         ".to_string();
    }
    // Use libc to format the timestamp without pulling in chrono
    let secs = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}
