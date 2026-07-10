//! TCP listener that receives history from peer nodes, plus the daemon
//! lifecycle (`--daemon` backgrounding, PID file, `dejiny sync stop`).

use crate::config::SyncConfig;
use crate::db::{history_path, log_error, open_db};
use crate::sync::proto;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::unistd::Pid;
use rusqlite::{Connection, TransactionBehavior};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const ACK_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Matches the import path: receiving writes may compete with the shell
/// hook's store for the WAL write lock.
const RECEIVE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(cfg: &SyncConfig, daemon: bool) -> anyhow::Result<()> {
    if daemon && let Some(pid) = daemon_pid() {
        anyhow::bail!("sync daemon already running (pid {pid})");
    }

    // Bind before daemonizing so port-in-use errors reach the user's terminal.
    let listener = TcpListener::bind(&cfg.listen)
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", cfg.listen))?;
    let addr = listener.local_addr()?;

    if daemon {
        println!("dejiny: listening on {addr} (daemon)");
        daemonize()?;
    } else {
        println!("dejiny: listening on {addr}");
    }

    let key = cfg.key.as_bytes().to_vec();
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let key = key.clone();
                std::thread::spawn(move || {
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|_| "unknown".to_string());
                    if let Err(e) = handle_conn(stream, &key) {
                        log::debug!("sync listen: connection from {peer}: {e}");
                    }
                });
            }
            Err(e) => log_error(&format!("sync listen: accept: {e}")),
        }
    }
    Ok(())
}

fn handle_conn(stream: TcpStream, key: &[u8]) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(ACK_WRITE_TIMEOUT))?;
    let mut conn = open_db()?;
    conn.busy_timeout(RECEIVE_BUSY_TIMEOUT)?;

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    // A connection may carry multiple frames; each is verified, inserted,
    // and acknowledged independently. Any protocol or auth error drops the
    // connection with no reply.
    while let Some(line) = read_bounded_line(&mut reader)? {
        let env = proto::decode_verify(key, &line)?;
        insert_batch(&mut conn, &env)?;
        writer.write_all(b"ok\n")?;
    }
    Ok(())
}

/// Insert received commands, skipping any (command, start, hostname) triple
/// already present so redelivered batches are idempotent.
fn insert_batch(conn: &mut Connection, env: &proto::Envelope) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for cmd in &env.cmds {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM commands
             WHERE command = ?1 AND start = ?2 AND hostname = ?3)",
            rusqlite::params![cmd.command, cmd.start, env.host],
            |row| row.get(0),
        )?;
        if exists {
            continue;
        }
        tx.execute(
            "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![cmd.command, cmd.exit_code, cmd.start, cmd.end, cmd.cwd, env.host],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Read one \n-terminated line, refusing to buffer more than MAX_LINE_BYTES.
/// Returns None on a clean EOF between frames.
fn read_bounded_line(reader: &mut impl BufRead) -> anyhow::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let (consumed, done) = {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                if buf.is_empty() {
                    return Ok(None);
                }
                anyhow::bail!("connection closed mid-frame");
            }
            let newline = chunk.iter().position(|&b| b == b'\n');
            let take = newline.unwrap_or(chunk.len());
            if buf.len() + take > proto::MAX_LINE_BYTES {
                anyhow::bail!("frame exceeds {} bytes", proto::MAX_LINE_BYTES);
            }
            buf.extend_from_slice(&chunk[..take]);
            match newline {
                Some(pos) => (pos + 1, true),
                None => (take, false),
            }
        };
        reader.consume(consumed);
        if done {
            return Ok(Some(String::from_utf8(buf)?));
        }
    }
}

// ---- daemon lifecycle ----

fn pid_path() -> PathBuf {
    history_path().join("sync.pid")
}

/// PID of a live sync daemon, if one is running. A PID file naming a dead
/// process (SIGKILL, crash) is treated as absent.
pub fn daemon_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    alive(pid).then_some(pid)
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
}

// Set once before the SIGTERM handler is installed, read only from it.
static PID_FILE_FOR_HANDLER: OnceLock<CString> = OnceLock::new();

extern "C" fn sigterm_handler(_: libc::c_int) {
    // Only async-signal-safe calls here.
    if let Some(path) = PID_FILE_FOR_HANDLER.get() {
        unsafe { libc::unlink(path.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn daemonize() -> anyhow::Result<()> {
    let _ = std::io::stdout().flush();
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
        }
        if pid > 0 {
            libc::_exit(0); // parent: the bound socket lives on in the child
        }
        libc::setsid();
    }

    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    unsafe {
        libc::dup2(devnull.as_raw_fd(), 0);
        libc::dup2(devnull.as_raw_fd(), 1);
        libc::dup2(devnull.as_raw_fd(), 2);
    }

    let path = pid_path();
    std::fs::create_dir_all(history_path())?;
    std::fs::write(&path, std::process::id().to_string())?;

    let cpath = CString::new(path.into_os_string().into_encoded_bytes())?;
    let _ = PID_FILE_FOR_HANDLER.set(cpath);
    let action = SigAction::new(
        SigHandler::Handler(sigterm_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
    }
    Ok(())
}

pub fn stop() -> anyhow::Result<()> {
    let path = pid_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("dejiny: sync daemon not running");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let pid: i32 = contents
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("malformed PID file {}", path.display()))?;

    if !alive(pid) {
        let _ = std::fs::remove_file(&path);
        println!("dejiny: sync daemon not running");
        return Ok(());
    }

    nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM)?;
    for _ in 0..20 {
        if !alive(pid) {
            let _ = std::fs::remove_file(&path);
            println!("dejiny: sync daemon stopped (pid {pid})");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("sync daemon (pid {pid}) did not exit after SIGTERM");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_line_reads_frames_and_eof() {
        let data = b"first line\nsecond\n";
        let mut reader = BufReader::new(&data[..]);
        assert_eq!(
            read_bounded_line(&mut reader).unwrap().as_deref(),
            Some("first line")
        );
        assert_eq!(
            read_bounded_line(&mut reader).unwrap().as_deref(),
            Some("second")
        );
        assert!(read_bounded_line(&mut reader).unwrap().is_none());
    }

    #[test]
    fn bounded_line_rejects_mid_frame_eof() {
        let data = b"no newline";
        let mut reader = BufReader::new(&data[..]);
        assert!(read_bounded_line(&mut reader).is_err());
    }

    #[test]
    fn bounded_line_rejects_oversized_frame() {
        let data = vec![b'x'; proto::MAX_LINE_BYTES + 2];
        let mut reader = BufReader::new(&data[..]);
        assert!(read_bounded_line(&mut reader).is_err());
    }
}
