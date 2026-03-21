use dejiny::db::open_db_at;
use dejiny::format::build_recording;
use rusqlite::Connection;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn dejiny_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dejiny"))
}

/// Open (or create) the dejiny database at the given directory path.
fn open_test_db(dir: &Path) -> Connection {
    open_db_at(dir).expect("failed to open test database")
}

/// Insert a synthetic recording into the test database.
/// Returns the command_id.
fn insert_synthetic_recording(
    conn: &Connection,
    command: &str,
    cols: u16,
    rows: u16,
    events: &[(u64, &[u8])],
) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    conn.execute(
        "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
         VALUES (?1, 0, ?2, ?2, '/tmp', 'test')",
        rusqlite::params![command, now],
    )
    .unwrap();
    let command_id = conn.last_insert_rowid();

    let recording = build_recording(cols, rows, events);
    let compressed = zstd::encode_all(&recording[..], 3).unwrap();
    conn.execute(
        "INSERT INTO recording_chunks (command_id, seq, data) VALUES (?1, 0, ?2)",
        rusqlite::params![command_id, compressed],
    )
    .unwrap();

    command_id
}

/// Insert a synthetic multi-chunk recording into the test database.
fn insert_synthetic_multi_chunk(conn: &Connection, command: &str, chunks: &[Vec<u8>]) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    conn.execute(
        "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
         VALUES (?1, 0, ?2, ?2, '/tmp', 'test')",
        rusqlite::params![command, now],
    )
    .unwrap();
    let command_id = conn.last_insert_rowid();

    for (seq, chunk) in chunks.iter().enumerate() {
        let compressed = zstd::encode_all(&chunk[..], 3).unwrap();
        conn.execute(
            "INSERT INTO recording_chunks (command_id, seq, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![command_id, seq as i64, compressed],
        )
        .unwrap();
    }

    command_id
}

/// Run `dejiny replay` with piped I/O (no PTY needed for replay).
fn replay_command(data_dir: &Path, id: i64, speed: f64) -> Output {
    Command::new(dejiny_bin())
        .args(["replay", &id.to_string(), "--speed", &speed.to_string()])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny replay")
}

/// Run `dejiny replay --text` with piped I/O.
fn replay_text_command(data_dir: &Path, id: i64) -> Output {
    Command::new(dejiny_bin())
        .args(["replay", &id.to_string(), "--text"])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny replay --text")
}

/// Run `dejiny replay` (latest) with piped I/O.
fn replay_latest(data_dir: &Path, speed: f64) -> Output {
    Command::new(dejiny_bin())
        .args(["replay", "--speed", &speed.to_string()])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny replay")
}

/// Insert a synthetic recording with custom metadata.
fn insert_synthetic_recording_with_meta(
    conn: &Connection,
    command: &str,
    exit_code: i32,
    cwd: &str,
    start: f64,
    end: f64,
    cols: u16,
    rows: u16,
    events: &[(u64, &[u8])],
) -> i64 {
    conn.execute(
        "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
         VALUES (?1, ?2, ?3, ?4, ?5, 'test')",
        rusqlite::params![command, exit_code, start, end, cwd],
    )
    .unwrap();
    let command_id = conn.last_insert_rowid();

    let recording = build_recording(cols, rows, events);
    let compressed = zstd::encode_all(&recording[..], 3).unwrap();
    conn.execute(
        "INSERT INTO recording_chunks (command_id, seq, data) VALUES (?1, 0, ?2)",
        rusqlite::params![command_id, compressed],
    )
    .unwrap();

    command_id
}

// ---------------------------------------------------------------------------
// Replay tests — synthetic recordings
// ---------------------------------------------------------------------------

/// Helper: set up a temp dir with dejiny subdir structure for XDG_DATA_HOME.
/// Returns (TempDir, data_dir_path) where data_dir_path = tmp/dejiny.
fn setup_replay_env() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("dejiny");
    (tmp, data_dir)
}

#[test]
fn replay_simple_text() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "echo hello", 80, 24, &[(0, b"hello\r\n")]);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).contains("hello"), true);
}

#[test]
fn replay_multiple_events() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(
        &conn,
        "test",
        80,
        24,
        &[
            (0, b"line1\r\n"),
            (100_000, b"line2\r\n"),
            (200_000, b"line3\r\n"),
        ],
    );
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("line1"));
    assert!(stdout.contains("line2"));
    assert!(stdout.contains("line3"));
}

#[test]
fn replay_ansi_escapes() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    // Red text: ESC[31m hello ESC[0m
    let id =
        insert_synthetic_recording(&conn, "color", 80, 24, &[(0, b"\x1b[31mhello\x1b[0m\r\n")]);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
    let stdout = out.stdout;
    // Should contain the ANSI escape and the text
    assert!(stdout.windows(5).any(|w| w == b"hello"));
    assert!(stdout.windows(5).any(|w| w == b"\x1b[31m"));
}

#[test]
fn replay_empty_recording() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "empty", 80, 24, &[]);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
}

#[test]
fn replay_nonexistent_id() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    drop(conn);

    let out = replay_command(&data_dir, 99999, 0.0);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no recording"));
}

#[test]
fn replay_latest_resolves() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let _id1 = insert_synthetic_recording(&conn, "first", 80, 24, &[(0, b"first\r\n")]);
    let _id2 = insert_synthetic_recording(&conn, "second", 80, 24, &[(0, b"second\r\n")]);
    drop(conn);

    let out = replay_latest(&data_dir, 0.0);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Latest should be the second recording
    assert!(stdout.contains("second"));
}

#[test]
fn replay_escape_reset() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    // Recording that sets bold and red
    let id = insert_synthetic_recording(&conn, "style", 80, 24, &[(0, b"\x1b[1;31mbold red text")]);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
    let stdout = out.stdout;
    // Output should end with reset escape sequence (from reset_escape_state)
    // Check for SGR reset \x1b[0m somewhere near the end
    assert!(stdout.windows(4).any(|w| w == b"\x1b[0m"));
}

#[test]
fn replay_large_recording() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    // 1000 events with 100 bytes each
    let data = vec![b'A'; 100];
    let events: Vec<(u64, &[u8])> = (0..1000).map(|i| (i * 1000, data.as_slice())).collect();
    let id = insert_synthetic_recording(&conn, "large", 80, 24, &events);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
    // Should have replayed all the data (minus escape reset overhead)
    let stdout_len = out.stdout.len();
    assert!(
        stdout_len >= 100_000,
        "expected >= 100000 bytes, got {stdout_len}"
    );
}

#[test]
fn replay_binary_data() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    // All byte values 0x00-0xFF
    let data: Vec<u8> = (0..=255).collect();
    let id = insert_synthetic_recording(&conn, "binary", 80, 24, &[(0, &data)]);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
    // The binary data should be present in stdout (before escape reset suffix)
    for byte in 0..=255u8 {
        assert!(
            out.stdout.contains(&byte),
            "missing byte 0x{byte:02x} in output"
        );
    }
}

#[test]
fn replay_multi_chunk() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);

    // Chunk 1: header + event
    let chunk1 = build_recording(80, 24, &[(0, b"chunk1 ")]);
    // Chunk 2: just an event (no header — concatenated after decompression)
    let mut chunk2 = Vec::new();
    let ts: u64 = 100_000;
    chunk2.extend_from_slice(&ts.to_le_bytes());
    chunk2.extend_from_slice(&(6u32).to_le_bytes());
    chunk2.extend_from_slice(b"chunk2");

    let id = insert_synthetic_multi_chunk(&conn, "multi", &[chunk1, chunk2]);
    drop(conn);

    let out = replay_command(&data_dir, id, 0.0);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("chunk1"), "missing chunk1 in output");
    assert!(stdout.contains("chunk2"), "missing chunk2 in output");
}

// ---------------------------------------------------------------------------
// --text mode tests
// ---------------------------------------------------------------------------

#[test]
fn text_simple_output() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "echo hello", 80, 24, &[(0, b"hello\r\n")]);
    drop(conn);

    let out = replay_text_command(&data_dir, id);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hello"), "expected 'hello' in text output");
    // Should NOT contain ANSI reset sequences that normal replay adds
    assert!(
        !stdout.contains("\x1b["),
        "text output should not contain ANSI escapes"
    );
}

#[test]
fn text_strips_ansi() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(
        &conn,
        "color-cmd",
        80,
        24,
        &[(0, b"\x1b[31mred\x1b[0m \x1b[1;32mgreen\x1b[0m\r\n")],
    );
    drop(conn);

    let out = replay_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("red green"),
        "expected 'red green', got: {stdout}"
    );
    assert!(!stdout.contains("\x1b["), "should have no ANSI escapes");
}

#[test]
fn text_metadata_fields() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording_with_meta(
        &conn,
        "ls -la",
        0,
        "/home/user",
        1000.0,
        1002.5,
        120,
        40,
        &[(0, b"file1\r\nfile2\r\n")],
    );
    drop(conn);

    let out = replay_text_command(&data_dir, id);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("# Command: ls -la"),
        "missing command header"
    );
    assert!(
        stdout.contains("# Directory: /home/user"),
        "missing directory header"
    );
    assert!(
        stdout.contains("# Exit Code: 0"),
        "missing exit code header"
    );
    assert!(
        stdout.contains("# Duration: 2.5s"),
        "missing duration header"
    );
    assert!(
        stdout.contains("# Terminal: 120x40"),
        "missing terminal header"
    );
}

#[test]
fn text_nonzero_exit_code() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording_with_meta(
        &conn,
        "false",
        1,
        "/tmp",
        1000.0,
        1000.1,
        80,
        24,
        &[(0, b"")],
    );
    drop(conn);

    let out = replay_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# Exit Code: 1"), "expected exit code 1");
}

#[test]
fn text_empty_recording() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "empty", 80, 24, &[]);
    drop(conn);

    let out = replay_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should still have the terminal size header
    assert!(stdout.contains("# Terminal: 80x24"));
}

#[test]
fn text_complex_escapes() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    // OSC title set + CSI clear line + SGR colors
    let id = insert_synthetic_recording(
        &conn,
        "complex",
        80,
        24,
        &[
            (0, b"\x1b]0;my title\x07"),             // OSC set title
            (100_000, b"\x1b[2Kprompt$ "),           // CSI erase line + prompt
            (200_000, b"\x1b[1;34mblue\x1b[0m\r\n"), // colored output
        ],
    );
    drop(conn);

    let out = replay_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("prompt$ "), "expected prompt text");
    assert!(stdout.contains("blue"), "expected 'blue' text");
    assert!(!stdout.contains("\x1b["), "should have no ANSI escapes");
    assert!(!stdout.contains("\x07"), "should have no BEL character");
}

// ---------------------------------------------------------------------------
// --input mode helpers
// ---------------------------------------------------------------------------

/// Insert a synthetic input recording into the test database.
fn insert_synthetic_input_recording(
    conn: &Connection,
    command_id: i64,
    cols: u16,
    rows: u16,
    events: &[(u64, &[u8])],
) {
    let recording = build_recording(cols, rows, events);
    let compressed = zstd::encode_all(&recording[..], 3).unwrap();
    conn.execute(
        "INSERT INTO input_recording_chunks (command_id, seq, data) VALUES (?1, 0, ?2)",
        rusqlite::params![command_id, compressed],
    )
    .unwrap();
}

/// Run `dejiny replay --input` with piped I/O.
fn replay_input_text_command(data_dir: &Path, id: i64) -> Output {
    Command::new(dejiny_bin())
        .args(["replay", &id.to_string(), "--input"])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny replay --input")
}

// ---------------------------------------------------------------------------
// --input mode tests
// ---------------------------------------------------------------------------

#[test]
fn input_text_simple() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "cat", 80, 24, &[(0, b"echoed\r\n")]);
    insert_synthetic_input_recording(&conn, id, 80, 24, &[(0, b"hello world\n")]);
    drop(conn);

    let out = replay_input_text_command(&data_dir, id);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello world"),
        "expected 'hello world' in input text output"
    );
}

#[test]
fn input_text_strips_ansi() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "test", 80, 24, &[(0, b"output\r\n")]);
    // Simulate arrow keys and other escape sequences in input
    insert_synthetic_input_recording(
        &conn,
        id,
        80,
        24,
        &[(0, b"\x1b[Ahello\x1b[B\x1b[C\x1b[D")],
    );
    drop(conn);

    let out = replay_input_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hello"), "expected 'hello' in input text");
    assert!(
        !stdout.contains("\x1b["),
        "input text should not contain ANSI escapes"
    );
}

#[test]
fn input_text_metadata_header() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording_with_meta(
        &conn,
        "vim file.txt",
        0,
        "/home/user",
        1000.0,
        1005.0,
        80,
        24,
        &[(0, b"output")],
    );
    insert_synthetic_input_recording(&conn, id, 80, 24, &[(0, b"isome text\x1b")]);
    drop(conn);

    let out = replay_input_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("# Stream: input"),
        "expected '# Stream: input' header"
    );
    assert!(
        stdout.contains("# Command: vim file.txt"),
        "expected command header"
    );
}

#[test]
fn input_text_missing_recording_error() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    // Insert output recording but no input recording
    let id = insert_synthetic_recording(&conn, "echo test", 80, 24, &[(0, b"test\r\n")]);
    drop(conn);

    let out = replay_input_text_command(&data_dir, id);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no input recording"),
        "expected 'no input recording' error, got: {stderr}"
    );
}

#[test]
fn input_text_mutual_exclusivity() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let _id = insert_synthetic_recording(&conn, "test", 80, 24, &[(0, b"test\r\n")]);
    drop(conn);

    let out = Command::new(dejiny_bin())
        .args(["replay", "1", "--text", "--input"])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny replay --text --input");

    assert!(
        !out.status.success(),
        "--text and --input should be mutually exclusive"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "expected conflict error, got: {stderr}"
    );
}

#[test]
fn input_text_empty_recording() {
    let (_tmp, data_dir) = setup_replay_env();
    let conn = open_test_db(&data_dir);
    let id = insert_synthetic_recording(&conn, "sleep 1", 80, 24, &[(0, b"")]);
    insert_synthetic_input_recording(&conn, id, 80, 24, &[]);
    drop(conn);

    let out = replay_input_text_command(&data_dir, id);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("# Stream: input"),
        "expected '# Stream: input' header"
    );
    assert!(
        stdout.contains("# Terminal: 80x24"),
        "expected terminal header"
    );
}

// ---------------------------------------------------------------------------
// Resize propagation test
// ---------------------------------------------------------------------------

#[test]
fn resize_propagation() {
    use nix::poll::PollTimeout;
    use nix::pty::forkpty;
    use nix::sys::signal::{Signal, kill};
    use nix::sys::wait::{WaitPidFlag, waitpid};

    let tmp = TempDir::new().unwrap();

    // Write helper script that traps SIGWINCH and reports terminal size.
    let script_path = tmp.path().join("resize_test.sh");
    std::fs::write(
        &script_path,
        "#!/bin/bash\ntrap 'echo RESIZED:$(stty size)' WINCH\necho READY:$(stty size)\nsleep 5\n",
    )
    .unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Initial window size: 24 rows x 80 cols
    let initial_ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // forkpty with initial size
    let fork_result = unsafe { forkpty(Some(&initial_ws), None) }.expect("forkpty failed");

    match fork_result {
        nix::pty::ForkptyResult::Child => {
            // Child: exec dejiny record wrapping the test script.
            // Set XDG_DATA_HOME so dejiny stores its DB in the temp dir.
            let xdg_data = tmp.path().to_str().unwrap();
            // SAFETY: we are in the child process after fork, single-threaded.
            unsafe {
                std::env::set_var("XDG_DATA_HOME", xdg_data);
                std::env::set_var("DEJINY_NO_SUMMARIZE", "1");
            }

            let dejiny = PathBuf::from(env!("CARGO_BIN_EXE_dejiny"));
            let c_dejiny = std::ffi::CString::new(dejiny.to_str().unwrap()).unwrap();
            let c_record = std::ffi::CString::new("record").unwrap();
            let c_sep = std::ffi::CString::new("--").unwrap();
            let c_script = std::ffi::CString::new(script_path.to_str().unwrap()).unwrap();
            let _ = nix::unistd::execvp(&c_dejiny, &[&c_dejiny, &c_record, &c_sep, &c_script]);
            std::process::exit(1);
        }
        nix::pty::ForkptyResult::Parent { child, master } => {
            // Set master fd to non-blocking for poll-based reading.
            unsafe {
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
            }

            let mut collected = String::new();
            let mut buf = [0u8; 4096];
            let timeout = PollTimeout::from(5000u16); // 5 seconds

            // Phase 1: Read until we see READY:24 80
            let ready_found =
                poll_until_match(&master, &mut collected, &mut buf, timeout, "READY:24 80");
            assert!(
                ready_found,
                "timed out waiting for READY:24 80, got: {collected}"
            );

            // Phase 2: Change window size to 50x120 on the outer master.
            // TIOCSWINSZ stores the new size; we also send SIGWINCH
            // explicitly to the child process group because on macOS the
            // kernel does not always deliver SIGWINCH via the master fd
            // ioctl alone in non-interactive PTY setups.
            let new_ws = libc::winsize {
                ws_row: 50,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &new_ws);
            }
            // Give the ioctl a moment to take effect, then ensure
            // dejiny receives SIGWINCH so it reads the new size.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = kill(child, Signal::SIGWINCH);

            // Phase 3: Read until we see RESIZED:50 120
            let resized_found =
                poll_until_match(&master, &mut collected, &mut buf, timeout, "RESIZED:50 120");
            assert!(
                resized_found,
                "timed out waiting for RESIZED:50 120, got: {collected}"
            );

            // Cleanup: kill child and wait
            let _ = kill(child, Signal::SIGKILL);
            let _ = waitpid(child, Some(WaitPidFlag::empty()));
        }
    }
}

/// Poll the master fd until `collected` contains `needle` or timeout expires.
/// Returns true if found, false on timeout.
// ---------------------------------------------------------------------------
// Fast-exit output capture test
// ---------------------------------------------------------------------------

/// Exercises the race between SIGCHLD and PTY output for fast-exiting commands.
/// Runs 20 iterations because the race is non-deterministic.
#[test]
fn fast_exit_captures_output() {
    use nix::poll::PollTimeout;
    use nix::pty::forkpty;

    for iteration in 0..20 {
        let tmp = TempDir::new().unwrap();

        let initial_ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let fork_result = unsafe { forkpty(Some(&initial_ws), None) }.expect("forkpty failed");

        match fork_result {
            nix::pty::ForkptyResult::Child => {
                let xdg_data = tmp.path().to_str().unwrap();
                unsafe {
                    std::env::set_var("XDG_DATA_HOME", xdg_data);
                    std::env::set_var("DEJINY_NO_SUMMARIZE", "1");
                }

                let dejiny = PathBuf::from(env!("CARGO_BIN_EXE_dejiny"));
                let c_dejiny = std::ffi::CString::new(dejiny.to_str().unwrap()).unwrap();
                let c_record = std::ffi::CString::new("record").unwrap();
                let c_sep = std::ffi::CString::new("--").unwrap();
                let c_echo = std::ffi::CString::new("/bin/echo").unwrap();
                let c_hello = std::ffi::CString::new("hello").unwrap();
                let _ = nix::unistd::execvp(
                    &c_dejiny,
                    &[&c_dejiny, &c_record, &c_sep, &c_echo, &c_hello],
                );
                std::process::exit(1);
            }
            nix::pty::ForkptyResult::Parent { child: _, master } => {
                unsafe {
                    libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
                }

                let mut collected = String::new();
                let mut buf = [0u8; 4096];
                let timeout = PollTimeout::from(5000u16);

                let found = poll_until_match(&master, &mut collected, &mut buf, timeout, "hello");
                assert!(
                    found,
                    "iteration {iteration}: timed out waiting for 'hello', got: {collected}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn poll_until_match(
    master: &impl AsRawFd,
    collected: &mut String,
    buf: &mut [u8],
    _timeout: nix::poll::PollTimeout,
    needle: &str,
) -> bool {
    use nix::poll::{PollFd, PollFlags, poll};
    use std::os::fd::BorrowedFd;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        if collected.contains(needle) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let poll_ms = remaining.as_millis().min(5000) as u16;

        let borrowed = unsafe { BorrowedFd::borrow_raw(master.as_raw_fd()) };
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, nix::poll::PollTimeout::from(poll_ms)) {
            Ok(0) => continue, // timeout
            Ok(_) => {
                if let Some(revents) = fds[0].revents() {
                    if revents.contains(PollFlags::POLLIN) {
                        let n = unsafe {
                            libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                        };
                        if n > 0 {
                            collected.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
                        }
                    }
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => panic!("poll failed: {e}"),
        }
    }
}
