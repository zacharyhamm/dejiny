//! Durable delivery queue: one row per (command, node), deleted only after
//! the peer acknowledges the batch. Retries are driven by whatever flush
//! runs next (every `dejiny store`, `dejiny record`, or a manual
//! `dejiny sync flush`); receiver-side dedupe makes redelivery idempotent.

use crate::config::SyncConfig;
use crate::sync::proto::CmdMsg;
use rusqlite::Connection;

/// Outbox rows older than this are dropped: a node unreachable for this long
/// is treated as dead rather than growing the queue forever.
const PRUNE_AFTER_SECS: f64 = 30.0 * 24.0 * 3600.0;
/// Cap on the exponential per-node retry backoff.
const MAX_BACKOFF_SECS: f64 = 600.0;

pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn enqueue(conn: &Connection, cfg: &SyncConfig, command_id: i64) -> anyhow::Result<()> {
    let now = unix_now();
    for node in &cfg.nodes {
        conn.execute(
            "INSERT OR IGNORE INTO sync_outbox (command_id, node, created) VALUES (?1, ?2, ?3)",
            rusqlite::params![command_id, node.name, now],
        )?;
    }
    Ok(())
}

pub fn prune(conn: &Connection, cfg: &SyncConfig) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM sync_outbox WHERE created < ?1",
        [unix_now() - PRUNE_AFTER_SECS],
    )?;
    let placeholders = vec!["?"; cfg.nodes.len()].join(", ");
    let names: Vec<&str> = cfg.nodes.iter().map(|n| n.name.as_str()).collect();
    conn.execute(
        &format!("DELETE FROM sync_outbox WHERE node NOT IN ({placeholders})"),
        rusqlite::params_from_iter(names),
    )?;
    conn.execute(
        "DELETE FROM sync_outbox WHERE command_id NOT IN (SELECT id FROM commands)",
        [],
    )?;
    Ok(())
}

/// True if the node's last failed attempt is recent enough that this flush
/// should leave it alone: waits min(2^attempts, 600) seconds between tries.
pub fn in_backoff(conn: &Connection, node: &str) -> anyhow::Result<bool> {
    let (attempts, last_attempt): (i64, Option<f64>) = conn.query_row(
        "SELECT COALESCE(MAX(attempts), 0), MAX(last_attempt) FROM sync_outbox WHERE node = ?1",
        [node],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(last) = last_attempt else {
        return Ok(false);
    };
    let wait = f64::powi(2.0, attempts.min(32) as i32).min(MAX_BACKOFF_SECS);
    Ok(unix_now() - last < wait)
}

/// Pending commands for a node, oldest first, joined to their history rows.
pub fn pending(conn: &Connection, node: &str) -> anyhow::Result<Vec<(i64, CmdMsg)>> {
    let mut stmt = conn.prepare(
        "SELECT o.id, c.command, c.exit_code, c.start, c.end, c.cwd
         FROM sync_outbox o JOIN commands c ON c.id = o.command_id
         WHERE o.node = ?1
         ORDER BY o.id",
    )?;
    let rows = stmt.query_map([node], |row| {
        Ok((
            row.get(0)?,
            CmdMsg {
                command: row.get(1)?,
                exit_code: row.get(2)?,
                start: row.get(3)?,
                end: row.get(4)?,
                cwd: row.get(5)?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn mark_attempt(conn: &Connection, node: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE sync_outbox SET attempts = attempts + 1, last_attempt = ?1 WHERE node = ?2",
        rusqlite::params![unix_now(), node],
    )?;
    Ok(())
}

pub fn delete_delivered(conn: &mut Connection, outbox_ids: &[i64]) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare("DELETE FROM sync_outbox WHERE id = ?1")?;
        for id in outbox_ids {
            stmt.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub struct NodeStatus {
    pub pending: i64,
    pub attempts: i64,
    pub last_attempt: Option<f64>,
}

pub fn node_status(conn: &Connection, node: &str) -> anyhow::Result<NodeStatus> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(attempts), 0), MAX(last_attempt)
         FROM sync_outbox WHERE node = ?1",
        [node],
        |row| {
            Ok(NodeStatus {
                pending: row.get(0)?,
                attempts: row.get(1)?,
                last_attempt: row.get(2)?,
            })
        },
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Node;
    use crate::db::open_db_at;

    fn test_cfg(names: &[&str]) -> SyncConfig {
        SyncConfig {
            key: "k".into(),
            listen: "127.0.0.1:0".into(),
            nodes: names
                .iter()
                .map(|n| Node {
                    name: n.to_string(),
                    addr: format!("{n}.example:28657"),
                })
                .collect(),
        }
    }

    fn test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = open_db_at(dir.path()).unwrap();
        (dir, conn)
    }

    fn insert_command(conn: &Connection, command: &str, start: f64) -> i64 {
        conn.execute(
            "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
             VALUES (?1, 0, ?2, ?2, '/tmp', 'testhost')",
            rusqlite::params![command, start],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn enqueue_one_row_per_node() {
        let (_dir, conn) = test_db();
        let cfg = test_cfg(&["a", "b"]);
        let id = insert_command(&conn, "ls", 1.0);
        enqueue(&conn, &cfg, id).unwrap();
        assert_eq!(pending(&conn, "a").unwrap().len(), 1);
        assert_eq!(pending(&conn, "b").unwrap().len(), 1);
        assert_eq!(pending(&conn, "a").unwrap()[0].1.command, "ls");
    }

    #[test]
    fn enqueue_is_idempotent() {
        let (_dir, conn) = test_db();
        let cfg = test_cfg(&["a"]);
        let id = insert_command(&conn, "ls", 1.0);
        enqueue(&conn, &cfg, id).unwrap();
        enqueue(&conn, &cfg, id).unwrap();
        assert_eq!(pending(&conn, "a").unwrap().len(), 1);
    }

    #[test]
    fn prune_removes_stale_orphaned_and_unknown_nodes() {
        let (_dir, conn) = test_db();
        let cfg = test_cfg(&["a"]);
        let id = insert_command(&conn, "ls", 1.0);

        // Too old
        conn.execute(
            "INSERT INTO sync_outbox (command_id, node, created) VALUES (?1, 'a', ?2)",
            rusqlite::params![id, unix_now() - PRUNE_AFTER_SECS - 1.0],
        )
        .unwrap();
        // Node no longer configured
        conn.execute(
            "INSERT INTO sync_outbox (command_id, node, created) VALUES (?1, 'gone', ?2)",
            rusqlite::params![id, unix_now()],
        )
        .unwrap();
        // Command no longer exists
        conn.execute(
            "INSERT INTO sync_outbox (command_id, node, created) VALUES (999999, 'a', ?1)",
            rusqlite::params![unix_now()],
        )
        .unwrap();

        prune(&conn, &cfg).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn backoff_after_failed_attempt() {
        let (_dir, conn) = test_db();
        let cfg = test_cfg(&["a"]);
        let id = insert_command(&conn, "ls", 1.0);
        enqueue(&conn, &cfg, id).unwrap();

        assert!(!in_backoff(&conn, "a").unwrap());
        mark_attempt(&conn, "a").unwrap();
        assert!(in_backoff(&conn, "a").unwrap());
    }

    #[test]
    fn delete_delivered_drains_queue() {
        let (_dir, mut conn) = test_db();
        let cfg = test_cfg(&["a"]);
        let id = insert_command(&conn, "ls", 1.0);
        enqueue(&conn, &cfg, id).unwrap();
        let ids: Vec<i64> = pending(&conn, "a").unwrap().iter().map(|(i, _)| *i).collect();
        delete_delivered(&mut conn, &ids).unwrap();
        assert!(pending(&conn, "a").unwrap().is_empty());
    }
}
