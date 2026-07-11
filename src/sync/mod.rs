//! Cross-instance history sync: broadcast newly stored commands to the
//! nodes listed in the config file, and receive theirs via `dejiny sync
//! listen`. History only — recordings and summaries never leave the machine.

mod listen;
mod outbox;
mod proto;

pub use outbox::enqueue;

use crate::config::{Node, SyncConfig};
use crate::db::{log_error, open_db};
use rusqlite::Connection;
use std::io::{BufRead, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Flushes always run detached from the shell, so waiting out a competing
/// writer (shell hook, listener, another flush) beats erroring at 500ms.
const FLUSH_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FlushStats {
    pub delivered: u64,
    pub failed_nodes: Vec<String>,
    pub deferred_nodes: Vec<String>,
    pub quarantined_entries: Vec<(String, i64)>,
}

/// Push all pending outbox entries to their nodes. Per-node failures are
/// recorded for retry and never propagated — a dead peer must not fail the
/// command that triggered the flush. `force` retries nodes still in backoff
/// (used by the manual `dejiny sync flush`).
pub fn flush_outbox(
    conn: &mut Connection,
    cfg: &SyncConfig,
    force: bool,
) -> anyhow::Result<FlushStats> {
    conn.busy_timeout(FLUSH_BUSY_TIMEOUT)?;
    outbox::prune(conn, cfg)?;
    let mut stats = FlushStats {
        delivered: 0,
        failed_nodes: Vec::new(),
        deferred_nodes: Vec::new(),
        quarantined_entries: Vec::new(),
    };
    for node in &cfg.nodes {
        let pending = outbox::pending(conn, &node.name)?;
        if pending.is_empty() {
            continue;
        }
        if !force && outbox::in_backoff(conn, &node.name)? {
            stats.deferred_nodes.push(node.name.clone());
            continue;
        }
        match send_to_node(conn, node, cfg.key.as_bytes(), &mut stats) {
            Ok(()) => {}
            Err(e) => {
                log::debug!("sync flush: node {} ({}): {e}", node.name, node.addr);
                outbox::mark_attempt(conn, &node.name)?;
                stats.failed_nodes.push(node.name.clone());
            }
        }
    }
    Ok(stats)
}

/// Send pending commands to one node in count- and byte-bounded frames,
/// deleting each frame's outbox rows only after the peer acknowledges it.
fn send_to_node(
    conn: &mut Connection,
    node: &Node,
    key: &[u8],
    stats: &mut FlushStats,
) -> anyhow::Result<()> {
    let mut connection: Option<(TcpStream, std::io::BufReader<TcpStream>)> = None;

    loop {
        let pending = outbox::pending(conn, &node.name)?;
        if pending.is_empty() {
            break;
        }

        let (count, line) = match largest_encodable_prefix(key, &pending) {
            Ok(frame) => frame,
            Err(e) => {
                let outbox_id = pending[0].0;
                let message = format!("cannot encode command for sync: {e}");
                outbox::mark_permanent_error(conn, outbox_id, &message)?;
                log_error(&format!(
                    "sync flush: node {} outbox entry {outbox_id}: {message}",
                    node.name
                ));
                stats
                    .quarantined_entries
                    .push((node.name.clone(), outbox_id));
                continue;
            }
        };

        if connection.is_none() {
            let stream = connect(&node.addr)?;
            stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
            stream.set_read_timeout(Some(ACK_TIMEOUT))?;
            let reader = std::io::BufReader::new(stream.try_clone()?);
            connection = Some((stream, reader));
        }
        let (stream, reader) = connection.as_mut().expect("connection initialized");
        stream.write_all(line.as_bytes())?;

        let mut ack = String::new();
        (&mut *reader).take(16).read_line(&mut ack)?;
        if ack.trim_end() != "ok" {
            anyhow::bail!("no acknowledgement (got {ack:?})");
        }

        let ids: Vec<i64> = pending[..count].iter().map(|(id, _)| *id).collect();
        outbox::delete_delivered(conn, &ids)?;
        stats.delivered += count as u64;
    }
    Ok(())
}

/// Return the largest prefix whose authenticated JSON frame fits the wire
/// limit. The outbox query already caps the candidate page at MAX_BATCH.
fn largest_encodable_prefix(
    key: &[u8],
    pending: &[(i64, proto::CmdMsg)],
) -> anyhow::Result<(usize, String)> {
    debug_assert!(!pending.is_empty());
    let encode = |count: usize| {
        proto::encode(
            key,
            &proto::Envelope {
                v: proto::PROTO_VERSION,
                cmds: pending[..count]
                    .iter()
                    .map(|(_, cmd)| cmd.clone())
                    .collect(),
            },
        )
    };

    if let Ok(frame) = encode(pending.len()) {
        return Ok((pending.len(), frame));
    }

    let first = encode(1)?;
    let mut best = (1, first);
    let mut low = 2;
    let mut high = pending.len().saturating_sub(1);
    while low <= high {
        let middle = low + (high - low) / 2;
        match encode(middle) {
            Ok(frame) => {
                best = (middle, frame);
                low = middle + 1;
            }
            Err(_) => high = middle - 1,
        }
    }
    Ok(best)
}

fn connect(addr: &str) -> anyhow::Result<TcpStream> {
    let mut last_err = anyhow::anyhow!("{addr}: no addresses resolved");
    for sock_addr in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = anyhow::anyhow!("{addr}: {e}"),
        }
    }
    Err(last_err)
}

/// Re-exec ourselves as a detached `dejiny sync flush --auto` so network
/// latency never blocks the caller (used from the foreground `dejiny record`
/// path). `--auto` keeps the retry backoff in effect, unlike a manual flush.
pub fn spawn_flush() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let _ = Command::new(exe)
        .args(["sync", "flush", "--auto"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn require_config() -> SyncConfig {
    match crate::config::require() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("dejiny: {e}");
            std::process::exit(1);
        }
    }
}

pub fn listen_cmd(daemon: bool) {
    let cfg = require_config();
    if let Err(e) = listen::run(&cfg, daemon) {
        eprintln!("dejiny: sync listen: {e}");
        log_error(&format!("sync listen: {e}"));
        std::process::exit(1);
    }
}

pub fn stop_cmd() {
    if let Err(e) = listen::stop() {
        eprintln!("dejiny: sync stop: {e}");
        std::process::exit(1);
    }
}

pub fn flush_cmd(auto: bool) {
    let cfg = require_config();
    let mut conn = match open_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dejiny: failed to open database: {e}");
            std::process::exit(1);
        }
    };
    match flush_outbox(&mut conn, &cfg, !auto) {
        Ok(stats) => {
            println!("delivered {} entries", stats.delivered);
            for node in &stats.deferred_nodes {
                println!("node {node}: in retry backoff, deferred");
            }
            for node in &stats.failed_nodes {
                println!("node {node}: unreachable, will retry");
            }
            for (node, entry) in &stats.quarantined_entries {
                println!("node {node}: outbox entry {entry} is too large and was quarantined");
            }
        }
        Err(e) => {
            eprintln!("dejiny: sync flush: {e}");
            std::process::exit(1);
        }
    }
}

pub fn status_cmd() {
    let cfg = require_config();
    let conn = match open_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dejiny: failed to open database: {e}");
            std::process::exit(1);
        }
    };
    match listen::daemon_pid() {
        Some(pid) => println!("daemon: running (pid {pid})"),
        None => println!("daemon: not running"),
    }
    for node in &cfg.nodes {
        match outbox::node_status(&conn, &node.name) {
            Ok(st) if st.pending == 0 && st.quarantined == 0 => {
                println!("node {} ({}): up to date", node.name, node.addr);
            }
            Ok(st) => {
                let last = st
                    .last_attempt
                    .map(crate::util::format_timestamp)
                    .unwrap_or_else(|| "never".to_string());
                println!(
                    "node {} ({}): {} pending, {} quarantined, {} attempts, last attempt {}",
                    node.name, node.addr, st.pending, st.quarantined, st.attempts, last
                );
            }
            Err(e) => eprintln!("dejiny: node {}: {e}", node.name),
        }
    }
}

pub fn keygen_cmd() {
    let mut buf = [0u8; 32];
    if let Err(e) = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
    {
        eprintln!("dejiny: failed to read /dev/urandom: {e}");
        std::process::exit(1);
    }
    println!("{}", hex::encode(buf));
    eprintln!(
        "Set this as `key` under [sync] in {} on every node, then restrict access:",
        crate::config::config_path().display()
    );
    eprintln!("  chmod 600 {}", crate::config::config_path().display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: usize, command_bytes: usize) -> proto::CmdMsg {
        proto::CmdMsg {
            id: format!("event-{id}"),
            host: "test-host".to_string(),
            command: "x".repeat(command_bytes),
            exit_code: 0,
            start: id as f64,
            end: id as f64 + 1.0,
            cwd: "/tmp".to_string(),
        }
    }

    #[test]
    fn encoded_batch_is_split_by_wire_size() {
        let pending: Vec<_> = (0..proto::MAX_BATCH)
            .map(|id| (id as i64, message(id, 10_000)))
            .collect();
        let (count, frame) = largest_encodable_prefix(b"key", &pending).unwrap();
        assert!(count > 0);
        assert!(count < proto::MAX_BATCH);
        assert!(frame.len() - 1 <= proto::MAX_LINE_BYTES);
    }

    #[test]
    fn individually_oversized_entry_is_reported() {
        let pending = vec![(1, message(1, proto::MAX_LINE_BYTES + 1))];
        assert!(largest_encodable_prefix(b"key", &pending).is_err());
    }
}
