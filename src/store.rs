use crate::db::{log_error, open_db};

pub fn store(command: &str, exit_code: i32, start: &str, end: &str, cwd: &str) {
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            // fork failed — store synchronously as fallback
            if let Err(e) = store_impl(command, exit_code, start, end, cwd) {
                eprintln!("dejiny: store failed: {e}");
            }
            return;
        }
        if pid > 0 {
            return; // parent
        }
        // child — detach from terminal process group so Ctrl+C won't kill us
        libc::setsid();
    }

    if let Err(e) = store_impl(command, exit_code, start, end, cwd) {
        log_error(&e.to_string());
    }
    unsafe { libc::_exit(0) };
}

fn store_impl(
    command: &str,
    exit_code: i32,
    start: &str,
    end: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    let conn = open_db()?;

    let start: f64 = start.parse()?;
    let end: f64 = end.parse()?;
    let hostname = hostname::get()?.to_string_lossy().into_owned();

    conn.execute(
        "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![command, exit_code, start, end, cwd, hostname],
    )?;

    Ok(())
}
