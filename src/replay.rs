use crate::db::{load_command_meta, load_recording, open_db};
use crate::format::RecEvent;
use crate::terminal::{RawModeGuard, reset_escape_state};
use crate::util::{clean_text, format_duration};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::io::Write;
use std::os::fd::AsFd;

const SEEK_DELTA_US: u64 = 5_000_000; // 5 seconds
const DRAIN_TIMEOUT_MS: u8 = 50;

fn text_impl(id: i64) -> anyhow::Result<()> {
    let conn = open_db()?;
    let rec = load_recording(&conn, id)?;
    let meta = load_command_meta(&conn, id);

    let text = clean_text(&rec.concatenate_event_data());

    let mut stdout = std::io::stdout();

    // Print metadata header
    if let Some(meta) = &meta {
        writeln!(stdout, "# Command: {}", meta.command)?;
        writeln!(stdout, "# Directory: {}", meta.cwd)?;
        writeln!(stdout, "# Exit Code: {}", meta.exit_code)?;
        let duration = meta.end - meta.start;
        writeln!(stdout, "# Duration: {}", format_duration(duration))?;
    }
    writeln!(
        stdout,
        "# Terminal: {}x{}",
        rec.header.cols, rec.header.rows
    )?;

    if !text.is_empty() {
        writeln!(stdout)?;
        write!(stdout, "{text}")?;
        // Ensure trailing newline
        if !text.ends_with('\n') {
            writeln!(stdout)?;
        }
    }

    Ok(())
}

enum ControlKey {
    Quit,
    Pause,
    Forward,
    Backward,
}

enum KeyAction {
    Quit,
    Resume,
    SeekForward,
    SeekBackward,
}

struct ReplayState<'a> {
    events: &'a [RecEvent],
    recording: &'a [u8],
    idx: usize,
    prev_ts: u64,
}

impl<'a> ReplayState<'a> {
    fn seek_forward(&mut self, stdout: &mut impl Write) {
        if self.idx < self.events.len() {
            self.prev_ts = self.prev_ts.saturating_add(SEEK_DELTA_US);
            while self.idx < self.events.len() && self.events[self.idx].ts_us <= self.prev_ts {
                let e = &self.events[self.idx];
                let _ = stdout.write_all(&self.recording[e.offset..e.offset + e.length]);
                self.idx += 1;
            }
            let _ = stdout.flush();
        }
    }

    fn seek_backward(&mut self, stdout: &mut impl Write) {
        let target_ts = self.prev_ts.saturating_sub(SEEK_DELTA_US);
        let target_idx = self.events[..self.idx].partition_point(|e| e.ts_us <= target_ts);
        reset_escape_state(stdout);
        let _ = stdout.write_all(b"\x1b[2J\x1b[H");
        for i in 0..target_idx {
            let e = &self.events[i];
            let _ = stdout.write_all(&self.recording[e.offset..e.offset + e.length]);
        }
        let _ = stdout.flush();
        self.idx = target_idx;
        self.prev_ts = if self.idx > 0 {
            self.events[self.idx - 1].ts_us
        } else {
            0
        };
    }
}

/// Map a control key to a `KeyAction`. For `Pause`, blocks until the user
/// chooses to resume, quit, or seek.
fn handle_key(key: ControlKey, stdin: &std::io::Stdin) -> KeyAction {
    match key {
        ControlKey::Quit => KeyAction::Quit,
        ControlKey::Forward => KeyAction::SeekForward,
        ControlKey::Backward => KeyAction::SeekBackward,
        ControlKey::Pause => {
            eprint!(
                "\r\x1b[K[paused \u{2014} space to resume, \u{2190}/\u{2192} to seek, q to quit]"
            );
            let _ = std::io::stderr().flush();
            loop {
                let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(n) if n > 0 => {}
                    _ => continue,
                }
                if let Some(key) = read_control_key(stdin) {
                    eprint!("\r\x1b[K");
                    let _ = std::io::stderr().flush();
                    return match key {
                        ControlKey::Pause => KeyAction::Resume,
                        ControlKey::Quit => KeyAction::Quit,
                        ControlKey::Forward => KeyAction::SeekForward,
                        ControlKey::Backward => KeyAction::SeekBackward,
                    };
                }
            }
        }
    }
}

pub fn replay(id: Option<i64>, speed: f64, text: bool) {
    let id = match id {
        Some(id) => id,
        None => match resolve_latest_recording() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("dejiny: {e}");
                std::process::exit(1);
            }
        },
    };
    let result = if text {
        text_impl(id)
    } else {
        replay_impl(id, speed)
    };
    if let Err(e) = result {
        eprintln!("dejiny: replay failed: {e}");
        std::process::exit(1);
    }
}

fn resolve_latest_recording() -> anyhow::Result<i64> {
    let conn = open_db()?;
    conn.query_row(
        "SELECT command_id FROM recording_chunks GROUP BY command_id ORDER BY command_id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .map_err(|_| anyhow::anyhow!("no recordings found"))
}

/// Read one byte from stdin. If it starts an escape sequence, try to parse
/// arrow keys (left/right); drain any remaining bytes. For plain keys,
/// return Quit/Pause if recognized.
fn read_control_key(stdin: &std::io::Stdin) -> Option<ControlKey> {
    let mut byte = [0u8; 1];
    match nix::unistd::read(stdin, &mut byte) {
        Ok(1) => {}
        _ => return None,
    }
    if byte[0] == 0x1b {
        // Try to read CSI sequence: \x1b [ <letter>
        let mut seq = [0u8; 1];
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        let got_bracket = matches!(poll(&mut fds, PollTimeout::from(0u8)), Ok(n) if n > 0)
            && nix::unistd::read(stdin, &mut seq).unwrap_or(0) == 1
            && seq[0] == b'[';

        if got_bracket {
            let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
            if matches!(poll(&mut fds, PollTimeout::from(0u8)), Ok(n) if n > 0)
                && nix::unistd::read(stdin, &mut seq).unwrap_or(0) == 1
            {
                match seq[0] {
                    b'C' => return Some(ControlKey::Forward),
                    b'D' => return Some(ControlKey::Backward),
                    _ => {}
                }
            }
        }

        // Drain remaining escape sequence bytes
        let mut drain = [0u8; 256];
        loop {
            let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(0u8)) {
                Ok(n) if n > 0 => {
                    if nix::unistd::read(stdin, &mut drain).unwrap_or(0) == 0 {
                        break;
                    }
                }
                _ => break,
            }
        }
        return None;
    }
    match byte[0] {
        0x03 | 0x71 => Some(ControlKey::Quit),
        0x20 => Some(ControlKey::Pause),
        _ => None,
    }
}

fn replay_impl(id: i64, speed: f64) -> anyhow::Result<()> {
    let conn = open_db()?;
    let rec = load_recording(&conn, id)?;

    // Warn on size mismatch
    let mut current_winsize: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut current_winsize) };
    if current_winsize.ws_col != rec.header.cols || current_winsize.ws_row != rec.header.rows {
        eprintln!(
            "dejiny: recording was {}x{}, terminal is {}x{}",
            rec.header.cols, rec.header.rows, current_winsize.ws_col, current_winsize.ws_row
        );
    }

    let events = &rec.events;

    // Put terminal in raw mode during replay so that:
    // 1. Terminal query responses (CPR, DA, OSC, XTGETTCAP) don't get line-buffered
    //    and dumped to the shell after replay ends
    // 2. We can drain them from stdin ourselves
    let _raw_guard = RawModeGuard::enter_if_tty(false)?;
    let is_tty = _raw_guard.is_some();

    let mut stdout = std::io::stdout();
    let stdin = std::io::stdin();
    let mut state = ReplayState {
        events,
        recording: &rec.data,
        idx: 0,
        prev_ts: 0,
    };

    'outer: while state.idx < state.events.len() {
        let event = &state.events[state.idx];
        let delay_us = event.ts_us.saturating_sub(state.prev_ts);
        let mut action: Option<KeyAction> = None;

        if delay_us > 0 && speed > 0.0 {
            let sleep_us = (delay_us as f64 / speed) as u64;
            if is_tty {
                let delay_ms = sleep_us / 1000;
                let capped = delay_ms.min(u16::MAX as u64) as u16;
                if capped > 0 {
                    let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
                    match poll(&mut fds, PollTimeout::from(capped)) {
                        Ok(n) if n > 0 => {
                            if let Some(key) = read_control_key(&stdin) {
                                action = Some(handle_key(key, &stdin));
                            }
                        }
                        _ => {}
                    }
                }
                if !matches!(
                    action,
                    Some(KeyAction::Quit | KeyAction::SeekForward | KeyAction::SeekBackward)
                ) {
                    let remainder = sleep_us % 1000;
                    if remainder > 0 {
                        std::thread::sleep(std::time::Duration::from_micros(remainder));
                    }
                }
            } else {
                std::thread::sleep(std::time::Duration::from_micros(sleep_us));
            }
        }

        match action {
            Some(KeyAction::Quit) => break,
            Some(KeyAction::SeekForward) => {
                state.seek_forward(&mut stdout);
                continue;
            }
            Some(KeyAction::SeekBackward) => {
                state.seek_backward(&mut stdout);
                continue;
            }
            Some(KeyAction::Resume) | None => {}
        }

        state.prev_ts = event.ts_us;
        stdout.write_all(&state.recording[event.offset..event.offset + event.length])?;
        stdout.flush()?;

        // Drain any terminal query responses from stdin, checking for control keys
        if is_tty {
            loop {
                let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::from(0u8)) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Some(key) = read_control_key(&stdin) {
                            match handle_key(key, &stdin) {
                                KeyAction::Quit => break 'outer,
                                KeyAction::SeekForward => {
                                    state.idx += 1;
                                    state.seek_forward(&mut stdout);
                                    continue 'outer;
                                }
                                KeyAction::SeekBackward => {
                                    state.idx += 1;
                                    state.seek_backward(&mut stdout);
                                    continue 'outer;
                                }
                                KeyAction::Resume => {}
                            }
                        }
                    }
                }
            }
        }

        state.idx += 1;
    }

    reset_escape_state(&mut stdout);

    // Final drain — wait a moment for any last responses
    if is_tty {
        let mut drain_buf = [0u8; 1024];
        let mut drain_fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        match poll(&mut drain_fds, PollTimeout::from(DRAIN_TIMEOUT_MS)) {
            Ok(n) if n > 0 => {
                while let Ok(n) = poll(&mut drain_fds, PollTimeout::from(0u8)) {
                    if n == 0 {
                        break;
                    }
                    if nix::unistd::read(&stdin, &mut drain_buf).unwrap_or(0) == 0 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    // Guard restores termios on drop
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{self, build_recording};

    fn make_state<'a>(events: &'a [RecEvent], recording: &'a [u8]) -> ReplayState<'a> {
        ReplayState {
            events,
            recording,
            idx: 0,
            prev_ts: 0,
        }
    }

    #[test]
    fn seek_forward_advances() {
        // 10 events at 1s intervals
        let ev_data: Vec<(u64, &[u8])> = (0..10).map(|i| (i * 1_000_000, b"x" as &[u8])).collect();
        let rec = build_recording(80, 24, &ev_data);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);

        // Seek forward 5s from start (prev_ts=0)
        state.seek_forward(&mut out);
        // SEEK_DELTA_US = 5_000_000, so events at 0,1,2,3,4,5 million should be consumed
        assert_eq!(state.idx, 6);
        assert_eq!(state.prev_ts, SEEK_DELTA_US);
    }

    #[test]
    fn seek_forward_partial() {
        let ev_data: Vec<(u64, &[u8])> = (0..10).map(|i| (i * 1_000_000, b"x" as &[u8])).collect();
        let rec = build_recording(80, 24, &ev_data);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);
        state.idx = 3;
        state.prev_ts = 2_000_000;

        state.seek_forward(&mut out);
        // prev_ts becomes 2M + 5M = 7M, events at 3M,4M,5M,6M,7M should be consumed
        assert_eq!(state.idx, 8);
        assert_eq!(state.prev_ts, 7_000_000);
    }

    #[test]
    fn seek_forward_past_end() {
        let ev_data: Vec<(u64, &[u8])> = (0..3).map(|i| (i * 1_000_000, b"x" as &[u8])).collect();
        let rec = build_recording(80, 24, &ev_data);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);

        state.seek_forward(&mut out);
        // All 3 events consumed (at 0,1,2M, all <= 5M)
        assert_eq!(state.idx, 3);
    }

    #[test]
    fn seek_forward_already_at_end() {
        let rec = build_recording(80, 24, &[(0, b"x")]);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);
        state.idx = events.len(); // already at end

        let prev_idx = state.idx;
        state.seek_forward(&mut out);
        assert_eq!(state.idx, prev_idx);
    }

    #[test]
    fn seek_backward_to_beginning() {
        let ev_data: Vec<(u64, &[u8])> = (0..10).map(|i| (i * 1_000_000, b"x" as &[u8])).collect();
        let rec = build_recording(80, 24, &ev_data);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);
        state.idx = 3;
        state.prev_ts = 2_000_000;

        state.seek_backward(&mut out);
        // target_ts = 2M - 5M = 0 (saturating), partition_point(ts <= 0) = 1 (event at 0)
        // Actually, partition_point finds first index where predicate is false
        // events[..3] timestamps: 0, 1M, 2M → partition_point(ts <= 0) = 1
        // So idx = 1, prev_ts = events[0].ts_us = 0
        assert_eq!(state.idx, 1);
        assert_eq!(state.prev_ts, 0);
    }

    #[test]
    fn seek_backward_at_start() {
        let rec = build_recording(80, 24, &[(0, b"x"), (1_000_000, b"y")]);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);
        // Already at start
        state.idx = 0;
        state.prev_ts = 0;

        state.seek_backward(&mut out);
        assert_eq!(state.idx, 0);
        assert_eq!(state.prev_ts, 0);
    }

    #[test]
    fn seek_backward_middle() {
        let ev_data: Vec<(u64, &[u8])> = (0..20).map(|i| (i * 1_000_000, b"x" as &[u8])).collect();
        let rec = build_recording(80, 24, &ev_data);
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);
        state.idx = 15;
        state.prev_ts = 14_000_000;

        state.seek_backward(&mut out);
        // target_ts = 14M - 5M = 9M
        // partition_point on events[..15] for ts <= 9M → events 0..10 (ts 0..9M inclusive) → idx=10
        assert_eq!(state.idx, 10);
        assert_eq!(state.prev_ts, 9_000_000);
    }

    #[test]
    fn seek_forward_output_content() {
        let rec = build_recording(
            80,
            24,
            &[(0, b"hello"), (1_000_000, b" world"), (2_000_000, b"!")],
        );
        let events = format::parse_events(&rec).unwrap();
        let mut out = Vec::new();
        let mut state = make_state(&events, &rec);

        state.seek_forward(&mut out);
        assert_eq!(out, b"hello world!");
    }

}
