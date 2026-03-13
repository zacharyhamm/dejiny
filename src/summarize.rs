use crate::db::{load_command_meta, load_recording, log_error, open_db};
use crate::util::{clean_text, format_duration};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const MAX_TEXT_BYTES: usize = 200_000;

pub fn spawn_summarize(command_id: i64) {
    if std::env::var_os("DEJINY_NO_SUMMARY").is_some() {
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    let _ = Command::new(exe)
        .args(["summarize", &command_id.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn();
}

pub fn summarize(id: i64) {
    if let Err(e) = summarize_impl(id) {
        log_error(&format!("summarize(id={id}): {e}"));
    }
}

fn summarize_impl(id: i64) -> anyhow::Result<()> {
    let conn = open_db()?;
    let meta = load_command_meta(&conn, id);

    if let Some(ref m) = meta
        && crate::db::is_command_blacklisted(&conn, &m.command)
    {
        return Ok(());
    }

    let rec = load_recording(&conn, id)?;

    let mut text = clean_text(&rec.concatenate_event_data());
    if text.len() > MAX_TEXT_BYTES {
        // Truncate at a char boundary
        let mut end = MAX_TEXT_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n... [truncated]");
    }

    // Build prompt
    let mut prompt = String::from(
        "Summarize this terminal recording in a concise but detailed way. Limit to 5 or 6 sentences. \
         Focus on what command was run, what it did, and whether it succeeded.\n\n",
    );
    if let Some(ref meta) = meta {
        let duration = meta.end - meta.start;
        prompt.push_str(&format!("# Command: {}\n", meta.command));
        prompt.push_str(&format!("# Directory: {}\n", meta.cwd));
        prompt.push_str(&format!("# Exit Code: {}\n", meta.exit_code));
        prompt.push_str(&format!("# Duration: {}\n", format_duration(duration)));
    }
    prompt.push_str(&format!(
        "# Terminal: {}x{}\n",
        rec.header.cols, rec.header.rows
    ));
    if !text.is_empty() {
        prompt.push('\n');
        prompt.push_str(&text);
    }

    // Pipe prompt to claude CLI via stdin, with retry + backoff
    const MAX_RETRIES: u32 = 10;
    let mut last_err = String::new();
    let mut summary = String::new();

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let backoff = Duration::from_secs(1 << attempt.min(6));
            thread::sleep(backoff);
        }

        let child = Command::new("claude")
            .args(["-p", "--model", "sonnet", "--output-format", "text"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("failed to spawn claude: {e}");
                continue;
            }
        };

        if let Some(mut stdin) = child.stdin.take()
            && let Err(e) = stdin.write_all(prompt.as_bytes())
        {
            last_err = format!("failed to write to claude stdin: {e}");
            continue;
        }

        match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if text.is_empty() {
                    last_err = "claude returned empty summary".to_string();
                    continue;
                }
                summary = text;
                break;
            }
            Ok(output) => {
                last_err = format!("claude exited with status {}", output.status);
                continue;
            }
            Err(e) => {
                last_err = format!("failed to wait on claude: {e}");
                continue;
            }
        }
    }

    if summary.is_empty() {
        eprintln!(
            "dejiny: failed to summarize command {id} after {MAX_RETRIES} attempts: {last_err}"
        );
        anyhow::bail!(
            "claude failed after {MAX_RETRIES} attempts: {last_err}"
        );
    }

    conn.execute(
        "UPDATE commands SET summary = ?1 WHERE id = ?2",
        rusqlite::params![summary, id],
    )?;

    Ok(())
}
