use dejiny::db::open_db_at;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn dejiny_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dejiny"))
}

fn open_test_db(dir: &Path) -> Connection {
    open_db_at(dir).expect("failed to open test database")
}

fn setup_store_env() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("dejiny");
    (tmp, data_dir)
}

fn wait_for_command(conn: &Connection, command: &str, timeout: Duration) -> Option<i64> {
    let deadline = Instant::now() + timeout;
    loop {
        let id = conn
            .query_row(
                "SELECT id FROM commands WHERE command = ?1 ORDER BY id DESC LIMIT 1",
                [command],
                |row| row.get(0),
            )
            .ok();
        if id.is_some() {
            return id;
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn store_internal_inserts_command() {
    let (_tmp, data_dir) = setup_store_env();

    let out = Command::new(dejiny_bin())
        .args([
            "store-internal",
            "--command",
            "echo internal",
            "--exit-code",
            "7",
            "--start",
            "10.25",
            "--end",
            "11.5",
            "--cwd",
            "/tmp/internal",
        ])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny store-internal");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = open_test_db(&data_dir);
    let row = conn
        .query_row(
            "SELECT command, exit_code, start, end, cwd FROM commands ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(row.0, "echo internal");
    assert_eq!(row.1, 7);
    assert_eq!(row.2, 10.25);
    assert_eq!(row.3, 11.5);
    assert_eq!(row.4, "/tmp/internal");
}

#[test]
fn store_launches_detached_helper() {
    let (_tmp, data_dir) = setup_store_env();

    let out = Command::new(dejiny_bin())
        .args([
            "store",
            "--command",
            "echo detached",
            "--exit-code",
            "0",
            "--start",
            "20",
            "--end",
            "21",
            "--cwd",
            "/tmp/detached",
        ])
        .env("XDG_DATA_HOME", data_dir.parent().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run dejiny store");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = open_test_db(&data_dir);
    let id = wait_for_command(&conn, "echo detached", Duration::from_secs(2))
        .expect("detached store helper did not insert command in time");
    let cwd: String = conn
        .query_row("SELECT cwd FROM commands WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cwd, "/tmp/detached");
}
