use crate::db::open_db;

pub fn add(pattern: &str) {
    if let Err(e) = regex::Regex::new(pattern) {
        eprintln!("dejiny: invalid regex: {e}");
        std::process::exit(1);
    }
    let conn = open_db().expect("failed to open database");
    match conn.execute(
        "INSERT OR IGNORE INTO summary_blacklist (pattern) VALUES (?1)",
        rusqlite::params![pattern],
    ) {
        Ok(_) => println!("Added blacklist pattern: {pattern}"),
        Err(e) => {
            eprintln!("dejiny: failed to add pattern: {e}");
            std::process::exit(1);
        }
    }
}

pub fn remove(pattern: &str) {
    let conn = open_db().expect("failed to open database");
    match conn.execute(
        "DELETE FROM summary_blacklist WHERE pattern = ?1",
        rusqlite::params![pattern],
    ) {
        Ok(0) => eprintln!("dejiny: pattern not found: {pattern}"),
        Ok(_) => println!("Removed blacklist pattern: {pattern}"),
        Err(e) => {
            eprintln!("dejiny: failed to remove pattern: {e}");
            std::process::exit(1);
        }
    }
}

pub fn list() {
    let conn = open_db().expect("failed to open database");
    let mut stmt = conn
        .prepare("SELECT pattern FROM summary_blacklist ORDER BY id")
        .expect("failed to query blacklist");
    let patterns = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("failed to read patterns");
    for p in patterns.flatten() {
        println!("{p}");
    }
}
