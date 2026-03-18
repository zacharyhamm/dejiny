use crate::db::open_db;
use crate::format::RecordingHeader;
use crate::terminal::RawModeGuard;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::forkpty;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, sigaction};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

const CHUNK_FLUSH_THRESHOLD: usize = 1_048_576;
const ZSTD_LEVEL: i32 = 3;
const SIGTSTP_TIMEOUT_MS: u64 = 100;

/// Write end of the self-pipe used to wake poll() on SIGCHLD.
static SIGCHLD_PIPE_WR: AtomicI32 = AtomicI32::new(-1);

/// Write end of the self-pipe used to wake poll() on SIGWINCH.
static SIGWINCH_PIPE_WR: AtomicI32 = AtomicI32::new(-1);

/// Check whether `data` contains a Ctrl+Z keystroke in any encoding:
///  - Legacy: raw 0x1A byte
///  - Kitty keyboard protocol: CSI 122 ; <modifiers> [:<event-type>] u
///    where 122 = 'z' and the Ctrl bit (bit 2) is set in (modifiers − 1).
fn contains_ctrl_z(data: &[u8]) -> bool {
    if data.contains(&0x1A) {
        return true;
    }
    let needle = b"\x1b[122;";
    for i in 0..data.len().saturating_sub(needle.len()) {
        if !data[i..].starts_with(needle) {
            continue;
        }
        let rest = &data[i + needle.len()..];
        let mut j = 0;
        let mut modval: u32 = 0;
        while j < rest.len() && rest[j].is_ascii_digit() {
            modval = modval * 10 + (rest[j] - b'0') as u32;
            j += 1;
        }
        if j == 0 || modval < 1 || (modval - 1) & 4 == 0 {
            continue; // no digits, or Ctrl bit not set
        }
        // 'u' → press (event-type omitted)
        if j < rest.len() && rest[j] == b'u' {
            return true;
        }
        // ':<event-type>u' → only accept press (1) or repeat (2)
        if j < rest.len() && rest[j] == b':' {
            j += 1;
            let mut etype: u32 = 0;
            while j < rest.len() && rest[j].is_ascii_digit() {
                etype = etype * 10 + (rest[j] - b'0') as u32;
                j += 1;
            }
            if j < rest.len() && rest[j] == b'u' && etype != 3 {
                return true;
            }
        }
    }
    false
}

struct Recording {
    buf: Vec<u8>,
    start: std::time::Instant,
}

impl Recording {
    fn new(cols: u16, rows: u16) -> Self {
        let mut buf = Vec::with_capacity(64 * 1024);
        let header = RecordingHeader { cols, rows };
        buf.extend_from_slice(&header.encode());
        Self {
            buf,
            start: std::time::Instant::now(),
        }
    }

    fn append(&mut self, data: &[u8]) {
        let ts = self.start.elapsed().as_micros() as u64;
        self.buf.extend_from_slice(&ts.to_le_bytes());
        self.buf
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(data);
    }

    fn take_buffer(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

fn flush_chunk(
    conn: &rusqlite::Connection,
    recording: &mut Recording,
    command_id: i64,
    chunk_seq: &mut i64,
) -> bool {
    let chunk_data = recording.take_buffer();
    match zstd::encode_all(&chunk_data[..], ZSTD_LEVEL) {
        Ok(compressed) => match conn.execute(
            "INSERT INTO recording_chunks (command_id, seq, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![command_id, *chunk_seq, compressed],
        ) {
            Ok(_) => {
                *chunk_seq += 1;
                true
            }
            Err(e) => {
                eprintln!("\r\ndejiny: recording flush failed, stopping capture: {e}\r");
                false
            }
        },
        Err(e) => {
            eprintln!("\r\ndejiny: recording compress failed, stopping capture: {e}\r");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{self, EVENT_HEADER_SIZE, HEADER_SIZE};

    // --- contains_ctrl_z tests ---

    #[test]
    fn ctrl_z_legacy_byte() {
        assert!(contains_ctrl_z(&[0x1A]));
    }

    #[test]
    fn ctrl_z_no_match() {
        assert!(!contains_ctrl_z(b"hello world"));
    }

    #[test]
    fn ctrl_z_kitty_press() {
        // CSI 122 ; 5 u → press with Ctrl (modifiers=5, (5-1)&4 = 4 ≠ 0)
        assert!(contains_ctrl_z(b"\x1b[122;5u"));
    }

    #[test]
    fn ctrl_z_kitty_repeat() {
        // CSI 122 ; 5 : 2 u → repeat event with Ctrl
        assert!(contains_ctrl_z(b"\x1b[122;5:2u"));
    }

    #[test]
    fn ctrl_z_kitty_release_rejected() {
        // CSI 122 ; 5 : 3 u → release event (event-type 3), should NOT match
        assert!(!contains_ctrl_z(b"\x1b[122;5:3u"));
    }

    #[test]
    fn ctrl_z_no_ctrl_bit() {
        // CSI 122 ; 2 u → Shift only (modifiers=2, (2-1)&4 = 0)
        assert!(!contains_ctrl_z(b"\x1b[122;2u"));
    }

    #[test]
    fn ctrl_z_in_stream() {
        let mut data = Vec::new();
        data.extend_from_slice(b"some output before ");
        data.push(0x1A);
        data.extend_from_slice(b" and after");
        assert!(contains_ctrl_z(&data));
    }

    #[test]
    fn ctrl_z_kitty_in_stream() {
        let mut data = Vec::new();
        data.extend_from_slice(b"prefix");
        data.extend_from_slice(b"\x1b[122;5u");
        data.extend_from_slice(b"suffix");
        assert!(contains_ctrl_z(&data));
    }

    #[test]
    fn ctrl_z_truncated_sequence() {
        // Truncated: missing 'u' terminator
        assert!(!contains_ctrl_z(b"\x1b[122;5"));
    }

    #[test]
    fn ctrl_z_empty() {
        assert!(!contains_ctrl_z(&[]));
    }

    // --- Recording struct tests ---

    #[test]
    fn recording_new_contains_header() {
        let rec = Recording::new(80, 24);
        assert_eq!(rec.len(), HEADER_SIZE);
        let header = format::RecordingHeader::decode(&rec.buf).unwrap();
        assert_eq!(header.cols, 80);
        assert_eq!(header.rows, 24);
    }

    #[test]
    fn recording_append_single() {
        let mut rec = Recording::new(80, 24);
        let data = b"hello";
        rec.append(data);
        assert_eq!(rec.len(), HEADER_SIZE + EVENT_HEADER_SIZE + data.len());
    }

    #[test]
    fn recording_append_preserves_order() {
        let mut rec = Recording::new(80, 24);
        rec.append(b"first");
        rec.append(b"second");
        let events = format::parse_events(&rec.buf).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].ts_us <= events[1].ts_us);
        assert_eq!(
            &rec.buf[events[0].offset..events[0].offset + events[0].length],
            b"first"
        );
        assert_eq!(
            &rec.buf[events[1].offset..events[1].offset + events[1].length],
            b"second"
        );
    }

    #[test]
    fn recording_take_buffer() {
        let mut rec = Recording::new(80, 24);
        rec.append(b"data");
        let buf = rec.take_buffer();
        assert!(!buf.is_empty());
        assert!(rec.is_empty());
        assert_eq!(rec.len(), 0);
    }

    #[test]
    fn recording_take_buffer_then_append() {
        let mut rec = Recording::new(80, 24);
        rec.append(b"chunk1");
        let _ = rec.take_buffer();
        rec.append(b"chunk2");
        // After take, append should work but buffer has no header
        assert_eq!(rec.len(), EVENT_HEADER_SIZE + 6);
    }

    #[test]
    fn recording_len_and_is_empty() {
        let rec = Recording::new(80, 24);
        assert_eq!(rec.len(), HEADER_SIZE);
        assert!(!rec.is_empty());
    }
}

pub fn record(command: &[String]) {
    match record_impl(command) {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("dejiny: record failed: {e}");
            std::process::exit(1);
        }
    }
}

fn record_impl(command: &[String]) -> anyhow::Result<i32> {
    let cmd_str = command.join(" ");

    // Ensure stdin is a terminal
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        anyhow::bail!("record requires a terminal (stdin is not a tty)");
    }

    // Get terminal size
    let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut winsize) };

    // forkpty — creates PTY pair and forks
    let fork_result = unsafe { forkpty(Some(&winsize), None)? };

    match fork_result {
        nix::pty::ForkptyResult::Child => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let c_shell = std::ffi::CString::new(shell.as_str())?;
            let c_flag = std::ffi::CString::new("-c")?;
            let c_cmd = std::ffi::CString::new(cmd_str.as_str())?;
            nix::unistd::execvp(&c_shell, &[&c_shell, &c_flag, &c_cmd])?;
            unreachable!();
        }
        nix::pty::ForkptyResult::Parent { child, master } => {
            run_recording_session(child, master, winsize, &cmd_str)
        }
    }
}

fn run_recording_session(
    child: Pid,
    master_fd: OwnedFd,
    winsize: libc::winsize,
    cmd_str: &str,
) -> anyhow::Result<i32> {
    log::debug!(
        "child pid={}, master fd={}",
        child.as_raw(),
        master_fd.as_raw_fd()
    );

    // Log real terminal flags BEFORE raw mode
    {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) };
        log::debug!(
            "termios BEFORE raw: rc={rc} c_lflag=0x{:x} ISIG={} ICANON={}",
            t.c_lflag,
            (t.c_lflag & libc::ISIG as libc::tcflag_t) != 0,
            (t.c_lflag & libc::ICANON as libc::tcflag_t) != 0
        );
    }

    // Self-pipe trick: SIGCHLD handler writes a byte to a pipe so
    // that poll() always wakes up when the child changes state,
    // regardless of signal delivery timing.
    let (sigchld_pipe_rd, sigchld_pipe_wr) = nix::unistd::pipe()?;
    // Set both ends non-blocking so the handler never blocks and
    // drain reads never block.
    unsafe {
        libc::fcntl(sigchld_pipe_rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(sigchld_pipe_wr.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    SIGCHLD_PIPE_WR.store(sigchld_pipe_wr.as_raw_fd(), Ordering::SeqCst);

    extern "C" fn sigchld_handler(_: libc::c_int) {
        let fd = SIGCHLD_PIPE_WR.load(Ordering::Relaxed);
        if fd >= 0 {
            unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
        }
    }
    let sigchld_action = SigAction::new(
        SigHandler::Handler(sigchld_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe { sigaction(Signal::SIGCHLD, &sigchld_action)? };

    // Self-pipe for SIGWINCH so poll() wakes on terminal resize.
    let (sigwinch_pipe_rd, sigwinch_pipe_wr) = nix::unistd::pipe()?;
    unsafe {
        libc::fcntl(sigwinch_pipe_rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(sigwinch_pipe_wr.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    SIGWINCH_PIPE_WR.store(sigwinch_pipe_wr.as_raw_fd(), Ordering::SeqCst);

    extern "C" fn sigwinch_handler(_: libc::c_int) {
        let fd = SIGWINCH_PIPE_WR.load(Ordering::Relaxed);
        if fd >= 0 {
            unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
        }
    }
    let sigwinch_action = SigAction::new(
        SigHandler::Handler(sigwinch_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe { sigaction(Signal::SIGWINCH, &sigwinch_action)? };

    // Ignore SIGTSTP for dejiny itself — the Ctrl+Z byte travels
    // through the PTY to the child; we handle suspension via waitpid.
    let sigtstp_ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGTSTP, &sigtstp_ignore)? };

    // Enter raw mode with signal handler for SIGTERM/SIGHUP
    let _raw_guard = RawModeGuard::enter(true)?;

    // Log real terminal flags AFTER raw mode
    {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) };
        log::debug!(
            "termios AFTER raw: rc={rc} c_lflag=0x{:x} ISIG={} ICANON={}",
            t.c_lflag,
            (t.c_lflag & libc::ISIG as libc::tcflag_t) != 0,
            (t.c_lflag & libc::ICANON as libc::tcflag_t) != 0
        );
        log::debug!(
            "  c_iflag=0x{:x} c_oflag=0x{:x} c_cflag=0x{:x}",
            t.c_iflag,
            t.c_oflag,
            t.c_cflag
        );
    }

    let mut recording = Recording::new(winsize.ws_col, winsize.ws_row);
    let mut buf = [0u8; 4096];

    let start_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let hostname = hostname::get()?.to_string_lossy().into_owned();

    let conn = open_db()?;
    conn.execute(
        "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
         VALUES (?1, -1, ?2, ?2, ?3, ?4)",
        rusqlite::params![cmd_str, start_time, cwd, hostname],
    )?;
    let command_id = conn.last_insert_rowid();
    let mut chunk_seq: i64 = 0;
    let mut recording_failed = false;
    let mut child_exit: Option<i32> = None;

    // Poll loop
    let stdin = std::io::stdin();
    // SAFETY: sigchld_pipe_rd and sigwinch_pipe_rd are valid for the lifetime of this scope.
    let sigchld_pipe_borrowed = unsafe { BorrowedFd::borrow_raw(sigchld_pipe_rd.as_raw_fd()) };
    let sigwinch_pipe_borrowed = unsafe { BorrowedFd::borrow_raw(sigwinch_pipe_rd.as_raw_fd()) };
    let mut drain_buf = [0u8; 64];
    // Tracks a pending suspend: after sending SIGTSTP to a raw-mode
    // child, we give it this long to stop before escalating to SIGSTOP.
    let mut suspend_deadline: Option<std::time::Instant> = None;
    loop {
        let mut fds = [
            PollFd::new(stdin.as_fd(), PollFlags::POLLIN),
            PollFd::new(master_fd.as_fd(), PollFlags::POLLIN),
            PollFd::new(sigchld_pipe_borrowed, PollFlags::POLLIN),
            PollFd::new(sigwinch_pipe_borrowed, PollFlags::POLLIN),
        ];

        let timeout = match suspend_deadline {
            Some(dl) => {
                let remain = dl.saturating_duration_since(std::time::Instant::now());
                if remain.is_zero() {
                    PollTimeout::from(0u16)
                } else {
                    PollTimeout::from(remain.as_millis().min(60_000) as u16)
                }
            }
            None => PollTimeout::NONE,
        };
        match poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }

        // If SIGTSTP didn't stop the child in time, force with SIGSTOP.
        if let Some(dl) = suspend_deadline
            && std::time::Instant::now() >= dl
        {
            let stop_result = kill(child, Signal::SIGSTOP);
            log::debug!(
                "deadline: SIGTSTP timed out, kill(pid={}, SIGSTOP) = {stop_result:?}",
                child.as_raw()
            );
            suspend_deadline = None;
        }

        // Self-pipe readable → SIGCHLD was delivered. Drain and check child.
        if let Some(revents) = fds[2].revents()
            && revents.contains(PollFlags::POLLIN)
        {
            log::debug!("sigchld: pipe readable, draining");
            // Drain all bytes from the pipe.
            while nix::unistd::read(&sigchld_pipe_rd, &mut drain_buf).unwrap_or(0) > 0 {}
            // Check child state.
            let wait_result = waitpid(child, Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG));
            log::debug!("sigchld: waitpid = {wait_result:?}");
            match wait_result {
                Ok(WaitStatus::Stopped(_, sig)) => {
                    log::debug!("sigchld: child stopped by {sig:?}, suspending dejiny");
                    suspend_deadline = None;
                    _raw_guard.suspend();
                    nix::sys::signal::raise(Signal::SIGSTOP).ok();
                    // — dejiny stopped until SIGCONT —
                    log::debug!(
                        "sigchld: dejiny resumed, re-entering raw mode and continuing child"
                    );
                    _raw_guard.resume();
                    let _ = kill(child, Signal::SIGCONT);
                    continue;
                }
                Ok(WaitStatus::Exited(_, code)) => {
                    log::debug!("sigchld: child exited with code {code}");
                    child_exit = Some(code);
                    break;
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    log::debug!("sigchld: child killed by signal {sig:?}");
                    child_exit = Some(128 + sig as i32);
                    break;
                }
                _ => {} // StillAlive — spurious wakeup
            }
        }

        // Self-pipe readable → SIGWINCH was delivered. Propagate new size to child PTY.
        if let Some(revents) = fds[3].revents()
            && revents.contains(PollFlags::POLLIN)
        {
            while nix::unistd::read(&sigwinch_pipe_rd, &mut drain_buf).unwrap_or(0) > 0 {}
            let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 {
                unsafe { libc::ioctl(master_fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
                log::debug!("sigwinch: propagated {}x{} to child PTY", ws.ws_col, ws.ws_row);
            }
        }

        // Check master fd (output from child)
        if let Some(revents) = fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match nix::unistd::read(&master_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        use std::io::Write;
                        let _ = std::io::stdout().write_all(&buf[..n]);
                        let _ = std::io::stdout().flush();
                        if !recording_failed {
                            recording.append(&buf[..n]);
                            if recording.len() >= CHUNK_FLUSH_THRESHOLD
                                && !flush_chunk(&conn, &mut recording, command_id, &mut chunk_seq)
                            {
                                recording_failed = true;
                            }
                        }
                    }
                    Err(nix::errno::Errno::EIO) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if revents.contains(PollFlags::POLLHUP) && !revents.contains(PollFlags::POLLIN) {
                break;
            }
        }

        // Check stdin (input from user)
        if let Some(revents) = fds[0].revents()
            && revents.contains(PollFlags::POLLIN)
        {
            match nix::unistd::read(&stdin, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let has_ctrl_z = contains_ctrl_z(&buf[..n]);
                    if has_ctrl_z {
                        log::debug!(
                            "stdin: Ctrl+Z detected in {n} bytes: {:02x?}",
                            &buf[..n.min(32)]
                        );
                    }
                    // Always forward all bytes to the child.
                    let write_result = nix::unistd::write(&master_fd, &buf[..n]);
                    if has_ctrl_z {
                        log::debug!("stdin: write to master = {write_result:?}");
                    }
                    if suspend_deadline.is_none() && has_ctrl_z {
                        let kill_result = kill(Pid::from_raw(-child.as_raw()), Signal::SIGTSTP);
                        log::debug!(
                            "stdin: kill(pgid={}, SIGTSTP) = {kill_result:?}",
                            -child.as_raw()
                        );
                        suspend_deadline = Some(
                            std::time::Instant::now()
                                + std::time::Duration::from_millis(SIGTSTP_TIMEOUT_MS),
                        );
                        log::debug!("stdin: armed suspend_deadline (100ms)");
                    }
                }
                Err(_) => break,
            }
        }
    }

    // Disarm self-pipes and restore signal dispositions.
    SIGCHLD_PIPE_WR.store(-1, Ordering::SeqCst);
    SIGWINCH_PIPE_WR.store(-1, Ordering::SeqCst);
    drop(sigchld_pipe_wr);
    drop(sigchld_pipe_rd);
    drop(sigwinch_pipe_wr);
    drop(sigwinch_pipe_rd);
    let sig_default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    unsafe {
        sigaction(Signal::SIGCHLD, &sig_default).ok();
        sigaction(Signal::SIGWINCH, &sig_default).ok();
        sigaction(Signal::SIGTSTP, &sig_default).ok();
    }

    // Wait for child exit (may already have been reaped in the EINTR branch)
    let exit_code = if let Some(code) = child_exit {
        code
    } else {
        // Ensure child isn't stopped before we blocking-wait
        let _ = kill(child, Signal::SIGCONT);
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => code,
            Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
            _ => 1,
        }
    };

    // Guard restores terminal and disarms signal handler on drop

    // Flush remaining buffer
    if !recording_failed && !recording.is_empty() {
        flush_chunk(&conn, &mut recording, command_id, &mut chunk_seq);
    }

    // Update command row with real exit code and end time
    let end_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    conn.execute(
        "UPDATE commands SET exit_code = ?1, end = ?2 WHERE id = ?3",
        rusqlite::params![exit_code, end_time, command_id],
    )?;

    crate::summarize::spawn_summarize(command_id);

    Ok(exit_code)
}
