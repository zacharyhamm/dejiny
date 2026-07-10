//! Integration tests for cross-instance history sync: two dejiny "nodes",
//! each with its own XDG_DATA_HOME / XDG_CONFIG_HOME, talking over loopback.

use rusqlite::Connection;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn dejiny_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dejiny"))
}

/// One simulated machine: isolated data + config roots.
struct TestNode {
    _tmp: TempDir,
    data_root: PathBuf,
    config_root: PathBuf,
}

impl TestNode {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let data_root = tmp.path().join("data");
        let config_root = tmp.path().join("config");
        std::fs::create_dir_all(&data_root).unwrap();
        std::fs::create_dir_all(&config_root).unwrap();
        Self {
            _tmp: tmp,
            data_root,
            config_root,
        }
    }

    fn write_config(&self, key: &str, listen: &str, nodes: &[(&str, &str)]) {
        let dir = self.config_root.join("dejiny");
        std::fs::create_dir_all(&dir).unwrap();
        let mut contents = format!("[sync]\nkey = \"{key}\"\nlisten = \"{listen}\"\n");
        for (name, addr) in nodes {
            contents.push_str(&format!("\n[[sync.nodes]]\nname = \"{name}\"\naddr = \"{addr}\"\n"));
        }
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        // Keep the key-file permission warning out of test output
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(dejiny_bin());
        cmd.args(args)
            .env("XDG_DATA_HOME", &self.data_root)
            .env("XDG_CONFIG_HOME", &self.config_root)
            .env("DEJINY_NO_SUMMARY", "1")
            .stdin(Stdio::null());
        cmd
    }

    /// Run a query against this node's DB, retrying while a concurrent
    /// dejiny process (detached store child, listener) holds the write lock.
    fn with_db<T>(&self, f: impl Fn(&Connection) -> Result<T, rusqlite::Error>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last_err;
        loop {
            match dejiny::db::open_db_at(&self.data_root.join("dejiny")) {
                Ok(conn) => {
                    let _ = conn.busy_timeout(Duration::from_secs(5));
                    match f(&conn) {
                        Ok(v) => return v,
                        Err(e) => last_err = e.to_string(),
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
            assert!(Instant::now() < deadline, "db access kept failing: {last_err}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pid_file(&self) -> PathBuf {
        self.data_root.join("dejiny").join("sync.pid")
    }

    fn store(&self, command: &str, start: f64) {
        let status = self
            .cmd(&[
                "store",
                "--command",
                command,
                "--exit-code",
                "0",
                "--start",
                &start.to_string(),
                "--end",
                &(start + 1.0).to_string(),
                "--cwd",
                "/tmp",
            ])
            .status()
            .unwrap();
        assert!(status.success(), "dejiny store failed");
    }

    fn command_count(&self, command: &str) -> i64 {
        self.with_db(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM commands WHERE command = ?1",
                [command],
                |r| r.get(0),
            )
        })
    }

    fn outbox_state(&self) -> (i64, i64) {
        self.with_db(|conn| {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(MAX(attempts), 0) FROM sync_outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
    }
}

/// Kills the foreground listener child on drop so failed tests don't leak it.
struct ListenerGuard(Child);

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn a foreground `dejiny sync listen` and return its bound port,
/// parsed from the startup line (configs use port 0 for a free port).
fn spawn_listener(node: &TestNode) -> (ListenerGuard, u16) {
    let mut child = node
        .cmd(&["sync", "listen"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let port = parse_port(&line);
    (ListenerGuard(child), port)
}

fn parse_port(listening_line: &str) -> u16 {
    listening_line
        .trim()
        .rsplit(':')
        .next()
        .and_then(|p| p.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in listener output: {listening_line:?}"))
}

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Reserve a free loopback port by binding and immediately releasing it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn store_syncs_to_peer_and_redelivery_is_idempotent() {
    let receiver = TestNode::new();
    let sender = TestNode::new();
    receiver.write_config("shared-key", "127.0.0.1:0", &[("sender", "127.0.0.1:1")]);
    let (_listener, port) = spawn_listener(&receiver);
    sender.write_config(
        "shared-key",
        "127.0.0.1:0",
        &[("receiver", &format!("127.0.0.1:{port}"))],
    );

    let cmd_text = "echo sync-delivery-test";
    sender.store(cmd_text, 1720000100.25);

    // `store` forks a detached child that inserts and flushes, so poll.
    wait_for("command to reach peer", Duration::from_secs(10), || {
        receiver.command_count(cmd_text) == 1
    });

    let (command, exit_code, start, cwd, host): (String, i32, f64, String, String) = receiver
        .with_db(|conn| {
            conn.query_row(
                "SELECT command, exit_code, start, cwd, hostname FROM commands WHERE command = ?1",
                [cmd_text],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        });
    assert_eq!(command, cmd_text);
    assert_eq!(exit_code, 0);
    assert_eq!(start, 1720000100.25);
    assert_eq!(cwd, "/tmp");
    let local_host = hostname::get().unwrap().to_string_lossy().into_owned();
    assert_eq!(host, local_host, "receiver should record the origin host");

    wait_for("sender outbox to drain", Duration::from_secs(10), || {
        sender.outbox_state().0 == 0
    });

    // Redeliver the same command by re-enqueueing it manually; the peer's
    // (command, start, hostname) dedupe must absorb it.
    sender.with_db(|conn| {
        conn.execute(
            "INSERT INTO sync_outbox (command_id, node, created)
             SELECT id, 'receiver', start FROM commands WHERE command = ?1",
            [cmd_text],
        )
    });
    let status = sender.cmd(&["sync", "flush"]).stdout(Stdio::null()).status().unwrap();
    assert!(status.success());
    assert_eq!(sender.outbox_state().0, 0, "redelivered outbox row should be drained");
    assert_eq!(receiver.command_count(cmd_text), 1, "dedupe must keep exactly one row");
}

#[test]
fn wrong_key_is_rejected_and_row_stays_queued() {
    let receiver = TestNode::new();
    let sender = TestNode::new();
    receiver.write_config("right-key", "127.0.0.1:0", &[("sender", "127.0.0.1:1")]);
    let (_listener, port) = spawn_listener(&receiver);
    sender.write_config(
        "wrong-key",
        "127.0.0.1:0",
        &[("receiver", &format!("127.0.0.1:{port}"))],
    );

    let cmd_text = "echo wrong-key-test";
    sender.store(cmd_text, 1720000200.5);

    // The detached flush must have tried and failed at least once.
    wait_for("failed delivery attempt", Duration::from_secs(10), || {
        let (pending, attempts) = sender.outbox_state();
        pending == 1 && attempts >= 1
    });
    assert_eq!(
        receiver.command_count(cmd_text),
        0,
        "command signed with the wrong key must not be accepted"
    );
}

#[test]
fn outbox_persists_until_listener_appears() {
    let receiver = TestNode::new();
    let sender = TestNode::new();
    let port = free_port();
    receiver.write_config(
        "shared-key",
        &format!("127.0.0.1:{port}"),
        &[("sender", "127.0.0.1:1")],
    );
    sender.write_config(
        "shared-key",
        "127.0.0.1:0",
        &[("receiver", &format!("127.0.0.1:{port}"))],
    );

    // No listener yet: the command must stay queued.
    let cmd_text = "echo durable-outbox-test";
    sender.store(cmd_text, 1720000300.75);
    wait_for("failed delivery attempt", Duration::from_secs(10), || {
        let (pending, attempts) = sender.outbox_state();
        pending == 1 && attempts >= 1
    });

    // Listener comes up; a manual flush overrides the backoff and delivers.
    let (_listener, bound_port) = spawn_listener(&receiver);
    assert_eq!(bound_port, port);
    let status = sender.cmd(&["sync", "flush"]).stdout(Stdio::null()).status().unwrap();
    assert!(status.success());

    assert_eq!(receiver.command_count(cmd_text), 1);
    assert_eq!(sender.outbox_state().0, 0);
}

#[test]
fn daemon_lifecycle() {
    let receiver = TestNode::new();
    let sender = TestNode::new();
    let port = free_port();
    receiver.write_config(
        "shared-key",
        &format!("127.0.0.1:{port}"),
        &[("sender", "127.0.0.1:1")],
    );
    sender.write_config(
        "shared-key",
        "127.0.0.1:0",
        &[("receiver", &format!("127.0.0.1:{port}"))],
    );

    // Start: parent binds, reports, exits 0; daemon keeps running.
    let output = receiver
        .cmd(&["sync", "listen", "--daemon"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success(), "daemon start failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("(daemon)"), "unexpected startup output: {stdout}");
    assert_eq!(parse_port(&stdout), port);

    wait_for("PID file", Duration::from_secs(5), || {
        read_pid(&receiver.pid_file()).is_some()
    });
    let pid = read_pid(&receiver.pid_file()).unwrap();
    assert!(pid_alive(pid), "daemon process should be alive");

    // Guard: whatever happens below, don't leak the daemon.
    struct DaemonGuard(i32);
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.0),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _guard = DaemonGuard(pid);

    // A second --daemon must refuse while the first is alive.
    let second = receiver
        .cmd(&["sync", "listen", "--daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!second.status.success(), "second daemon start should fail");
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // The daemon actually receives commands.
    let cmd_text = "echo daemon-delivery-test";
    sender.store(cmd_text, 1720000400.0);
    wait_for("command to reach daemon", Duration::from_secs(10), || {
        receiver.command_count(cmd_text) == 1
    });

    // Stop: process exits, PID file removed.
    let stop = receiver
        .cmd(&["sync", "stop"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(stop.status.success(), "sync stop failed: {stop:?}");
    assert!(String::from_utf8_lossy(&stop.stdout).contains("stopped"));
    assert!(!pid_alive(pid), "daemon should be dead after stop");
    assert!(!receiver.pid_file().exists(), "PID file should be removed");

    // Stopping again is a friendly no-op.
    let again = receiver
        .cmd(&["sync", "stop"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("not running"));
}
