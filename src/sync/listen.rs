//! TCP listener that receives history from peer nodes, plus the daemon
//! lifecycle (`--daemon` backgrounding, PID file, `dejiny sync stop`).

use crate::config::SyncConfig;
use crate::db::{history_path, log_error, open_db};
use crate::sync::proto;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::unistd::Pid;
use rusqlite::{Connection, TransactionBehavior};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const ACK_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Matches the import path: receiving writes may compete with the shell
/// hook's store for the WAL write lock.
const RECEIVE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 32;

struct ConnectionLimiter {
    active: Mutex<usize>,
    available: Condvar,
}

impl ConnectionLimiter {
    fn acquire(self: &Arc<Self>) -> ConnectionPermit {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= MAX_CONNECTIONS {
            active = self
                .available
                .wait(active)
                .unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        ConnectionPermit(Arc::clone(self))
    }
}

struct ConnectionPermit(Arc<ConnectionLimiter>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
        *active -= 1;
        self.0.available.notify_one();
    }
}

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
    let limiter = Arc::new(ConnectionLimiter {
        active: Mutex::new(0),
        available: Condvar::new(),
    });
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let key = key.clone();
                let permit = limiter.acquire();
                if let Err(e) = std::thread::Builder::new()
                    .name("dejiny-sync-connection".to_string())
                    .spawn(move || {
                        let _permit = permit;
                        let peer = stream
                            .peer_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        if let Err(e) = handle_conn(stream, &key) {
                            log::debug!("sync listen: connection from {peer}: {e}");
                        }
                    })
                {
                    log_error(&format!(
                        "sync listen: failed to spawn connection worker: {e}"
                    ));
                }
            }
            Err(e) => log_error(&format!("sync listen: accept: {e}")),
        }
    }
    Ok(())
}

fn handle_conn(stream: TcpStream, key: &[u8]) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(ACK_WRITE_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut conn: Option<Connection> = None;
    // A connection may carry multiple frames; each is verified, inserted,
    // and acknowledged independently. Any protocol or auth error drops the
    // connection with no reply.
    while let Some(line) = read_bounded_line(&mut reader)? {
        let env = proto::decode_verify(key, &line)?;
        if conn.is_none() {
            let opened = open_db()?;
            opened.busy_timeout(RECEIVE_BUSY_TIMEOUT)?;
            conn = Some(opened);
        }
        insert_batch(conn.as_mut().expect("database initialized"), &env)?;
        writer.write_all(b"ok\n")?;
    }
    Ok(())
}

/// Insert received commands, skipping stable event IDs already present so
/// redelivered batches are idempotent without conflating legitimate events.
fn insert_batch(conn: &mut Connection, env: &proto::Envelope) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for cmd in &env.cmds {
        tx.execute(
            "INSERT INTO commands
                 (command, exit_code, start, end, cwd, hostname, sync_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(sync_id) DO NOTHING",
            rusqlite::params![
                cmd.command,
                cmd.exit_code,
                cmd.start,
                cmd.end,
                cmd.cwd,
                cmd.host,
                cmd.id
            ],
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
    let path = pid_path();
    let pid = parse_pid(&std::fs::read_to_string(&path).ok()?, &path).ok()?;
    if daemon_lock_held(&path).ok()? && alive(pid) {
        Some(pid)
    } else {
        None
    }
}

fn alive(pid: i32) -> bool {
    debug_assert!(pid > 1);
    nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
}

fn parse_pid(contents: &str, path: &std::path::Path) -> anyhow::Result<i32> {
    let pid: i32 = contents
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("malformed PID file {}", path.display()))?;
    if pid <= 1 {
        anyhow::bail!("unsafe PID {pid} in {}", path.display());
    }
    Ok(pid)
}

/// Try to acquire the daemon's advisory lock. `true` means this process now
/// owns the lock; `false` means a live daemon still holds it.
fn try_acquire_pid_lock(file: &std::fs::File) -> std::io::Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    ) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn unlock_pid_file(file: &std::fs::File) {
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

fn daemon_lock_held(path: &std::path::Path) -> std::io::Result<bool> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if try_acquire_pid_lock(&file)? {
        unlock_pid_file(&file);
        Ok(false)
    } else {
        Ok(true)
    }
}

// Set once before the SIGTERM handler is installed, read only from it.
static PID_FILE_FOR_HANDLER: OnceLock<CString> = OnceLock::new();
// The daemon holds this descriptor for its lifetime. The advisory lock lets
// status/stop distinguish our daemon from a stale, reused PID.
static PID_FILE_LOCK: OnceLock<std::fs::File> = OnceLock::new();

extern "C" fn sigterm_handler(_: libc::c_int) {
    // Only async-signal-safe calls here.
    if let Some(path) = PID_FILE_FOR_HANDLER.get() {
        unsafe { libc::unlink(path.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn daemonize() -> anyhow::Result<()> {
    let _ = std::io::stdout().flush();
    let mut pipe_fds = [0; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("pipe failed: {}", std::io::Error::last_os_error());
    }

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
            anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
        }
        if pid > 0 {
            libc::close(pipe_fds[1]);
            let mut ready = std::fs::File::from_raw_fd(pipe_fds[0]);
            let mut message = String::new();
            let status = match ready.read_to_string(&mut message) {
                Ok(_) if message == "ready" => 0,
                Ok(_) => {
                    eprintln!(
                        "dejiny: sync daemon failed to start: {}",
                        message
                            .strip_prefix("error:")
                            .unwrap_or("child exited unexpectedly")
                    );
                    1
                }
                Err(e) => {
                    eprintln!("dejiny: sync daemon failed to start: {e}");
                    1
                }
            };
            libc::_exit(status);
        }
        libc::close(pipe_fds[0]);
    }

    let mut ready = unsafe { std::fs::File::from_raw_fd(pipe_fds[1]) };
    if let Err(error) = daemonize_child() {
        let _ = write!(ready, "error:{error}");
        let _ = ready.flush();
        unsafe { libc::_exit(1) };
    }
    if ready.write_all(b"ready").is_err() || ready.flush().is_err() {
        let _ = std::fs::remove_file(pid_path());
        unsafe { libc::_exit(1) };
    }
    drop(ready);
    Ok(())
}

fn daemonize_child() -> anyhow::Result<()> {
    if unsafe { libc::setsid() } < 0 {
        anyhow::bail!("setsid failed: {}", std::io::Error::last_os_error());
    }
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for fd in 0..=2 {
        if unsafe { libc::dup2(devnull.as_raw_fd(), fd) } < 0 {
            anyhow::bail!("dup2 failed: {}", std::io::Error::last_os_error());
        }
    }

    let path = pid_path();
    std::fs::create_dir_all(history_path())?;
    let mut pid_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    if !try_acquire_pid_lock(&pid_file)? {
        anyhow::bail!("sync daemon PID file is locked by another process");
    }
    pid_file.set_len(0)?;
    write!(pid_file, "{}", std::process::id())?;
    pid_file.flush()?;

    let cpath = CString::new(path.into_os_string().into_encoded_bytes())?;
    PID_FILE_FOR_HANDLER
        .set(cpath)
        .map_err(|_| anyhow::anyhow!("PID path was already initialized"))?;
    let action = SigAction::new(
        SigHandler::Handler(sigterm_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        sigaction(Signal::SIGTERM, &action)?;
    }
    PID_FILE_LOCK
        .set(pid_file)
        .map_err(|_| anyhow::anyhow!("PID lock was already initialized"))?;
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
    let pid = parse_pid(&contents, &path)?;

    if !daemon_lock_held(&path)? || !alive(pid) {
        let _ = std::fs::remove_file(&path);
        println!("dejiny: sync daemon not running");
        return Ok(());
    }

    nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM)?;
    for _ in 0..20 {
        if !daemon_lock_held(&path)? {
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

    fn command(id: &str, host: &str) -> proto::CmdMsg {
        proto::CmdMsg {
            id: id.to_string(),
            host: host.to_string(),
            command: "pwd".to_string(),
            exit_code: 0,
            start: 1_720_000_000.0,
            end: 1_720_000_000.1,
            cwd: "/tmp".to_string(),
        }
    }

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

    #[test]
    fn stable_ids_dedupe_redelivery_without_collapsing_equal_commands() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut conn = crate::db::open_db_at(dir.path()).unwrap();
        let env = proto::Envelope {
            v: proto::PROTO_VERSION,
            cmds: vec![
                command("event-a", "old-host"),
                command("event-b", "new-host"),
            ],
        };

        insert_batch(&mut conn, &env).unwrap();
        insert_batch(&mut conn, &env).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let hosts: String = conn
            .query_row(
                "SELECT group_concat(hostname, ',') FROM commands ORDER BY id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hosts, "old-host,new-host");
    }

    #[test]
    fn special_pids_are_rejected() {
        let path = PathBuf::from("sync.pid");
        assert!(parse_pid("-1", &path).is_err());
        assert!(parse_pid("0", &path).is_err());
        assert!(parse_pid("1", &path).is_err());
        assert_eq!(parse_pid("42", &path).unwrap(), 42);
    }

    #[test]
    fn pid_lock_distinguishes_a_live_owner_from_a_stale_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sync.pid");
        let owner = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(try_acquire_pid_lock(&owner).unwrap());
        assert!(daemon_lock_held(&path).unwrap());
        unlock_pid_file(&owner);
        assert!(!daemon_lock_held(&path).unwrap());
    }
}
