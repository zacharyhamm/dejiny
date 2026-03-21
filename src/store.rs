use crate::db::open_db;

pub fn store(command: &str, exit_code: i32, start: &str, end: &str, cwd: &str) {
    if let Err(e) = store_impl(command, exit_code, start, end, cwd) {
        eprintln!("dejiny: store failed: {e}");
    }
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
