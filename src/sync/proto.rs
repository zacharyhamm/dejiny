//! Wire protocol for history sync.
//!
//! One newline-terminated line per message:
//!
//! ```text
//! <64 hex chars: HMAC-SHA256(key, payload)> <compact JSON payload>\n
//! ```
//!
//! The MAC is computed over exactly the payload bytes as transmitted and is
//! verified (constant-time) before the JSON is parsed, so unauthenticated
//! input is never fed to the parser. There is deliberately no
//! timestamp/freshness check: stable per-command IDs make replayed messages
//! idempotent, and skipping freshness eliminates clock-skew failures between
//! nodes.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const PROTO_VERSION: u32 = 2;
/// Upper bound on a single wire line; the listener aborts reads beyond this.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum commands per message; senders chunk larger backlogs.
pub const MAX_BATCH: usize = 512;

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct Envelope {
    pub v: u32,
    pub cmds: Vec<CmdMsg>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct CmdMsg {
    /// Stable, globally unique delivery identity generated when the command
    /// is first stored. Metadata is deliberately not used for deduplication.
    pub id: String,
    pub host: String,
    pub command: String,
    pub exit_code: i32,
    pub start: f64,
    pub end: f64,
    pub cwd: String,
}

fn mac_hex(key: &[u8], payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

pub fn encode(key: &[u8], env: &Envelope) -> anyhow::Result<String> {
    let payload = serde_json::to_string(env)?;
    let frame = format!("{} {payload}\n", mac_hex(key, payload.as_bytes()));
    if frame.len() - 1 > MAX_LINE_BYTES {
        anyhow::bail!("encoded frame exceeds {MAX_LINE_BYTES} bytes");
    }
    Ok(frame)
}

pub fn decode_verify(key: &[u8], line: &str) -> anyhow::Result<Envelope> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    if line.len() > MAX_LINE_BYTES {
        anyhow::bail!("message exceeds {MAX_LINE_BYTES} bytes");
    }
    let (tag_hex, payload) = line
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("malformed frame: missing MAC separator"))?;
    let tag = hex::decode(tag_hex).map_err(|_| anyhow::anyhow!("malformed frame: bad MAC hex"))?;

    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    mac.verify_slice(&tag)
        .map_err(|_| anyhow::anyhow!("HMAC verification failed"))?;

    let env: Envelope = serde_json::from_str(payload)?;
    if env.v != PROTO_VERSION {
        anyhow::bail!("unsupported protocol version {}", env.v);
    }
    if env.cmds.len() > MAX_BATCH {
        anyhow::bail!("batch of {} exceeds limit of {MAX_BATCH}", env.cmds.len());
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-preshared-key";

    fn sample() -> Envelope {
        Envelope {
            v: PROTO_VERSION,
            cmds: vec![CmdMsg {
                id: "event-1".into(),
                host: "zaphod".into(),
                command: "cargo test".into(),
                exit_code: 0,
                start: 1720000012.4831,
                end: 1720000031.9921,
                cwd: "/home/z/dejiny".into(),
            }],
        }
    }

    #[test]
    fn round_trip() {
        let line = encode(KEY, &sample()).unwrap();
        assert!(line.ends_with('\n'));
        let env = decode_verify(KEY, &line).unwrap();
        assert_eq!(env.cmds[0].host, "zaphod");
        assert_eq!(env.cmds, sample().cmds);
    }

    #[test]
    fn fractional_start_round_trips_exactly() {
        let start = 1720000012.483159;
        let mut env = sample();
        env.cmds[0].start = start;
        let line = encode(KEY, &env).unwrap();
        let decoded = decode_verify(KEY, &line).unwrap();
        assert_eq!(decoded.cmds[0].start.to_bits(), start.to_bits());
    }

    #[test]
    fn multiline_command_stays_one_frame() {
        let mut env = sample();
        env.cmds[0].command = "echo one \\\n  two".into();
        let line = encode(KEY, &env).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        let decoded = decode_verify(KEY, &line).unwrap();
        assert_eq!(decoded.cmds[0].command, "echo one \\\n  two");
    }

    #[test]
    fn tampered_payload_rejected() {
        let line = encode(KEY, &sample()).unwrap();
        let tampered = line.replace("cargo", "corgo");
        assert!(decode_verify(KEY, &tampered).is_err());
    }

    #[test]
    fn tampered_mac_rejected() {
        let line = encode(KEY, &sample()).unwrap();
        let flipped = if line.starts_with('0') { "1" } else { "0" };
        let tampered = format!("{flipped}{}", &line[1..]);
        assert!(decode_verify(KEY, &tampered).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let line = encode(KEY, &sample()).unwrap();
        assert!(decode_verify(b"other-key", &line).is_err());
    }

    #[test]
    fn wrong_version_rejected() {
        let mut env = sample();
        env.v = PROTO_VERSION + 1;
        let line = encode(KEY, &env).unwrap();
        assert!(decode_verify(KEY, &line).is_err());
    }

    #[test]
    fn oversized_batch_rejected() {
        let mut env = sample();
        env.cmds = vec![env.cmds[0].clone(); MAX_BATCH + 1];
        match encode(KEY, &env) {
            Ok(line) => assert!(decode_verify(KEY, &line).is_err()),
            Err(e) => assert!(e.to_string().contains("encoded frame exceeds")),
        }
    }

    #[test]
    fn missing_separator_rejected() {
        assert!(decode_verify(KEY, "deadbeef").is_err());
    }

    #[test]
    fn empty_batch_ok() {
        let mut env = sample();
        env.cmds.clear();
        let line = encode(KEY, &env).unwrap();
        assert!(decode_verify(KEY, &line).unwrap().cmds.is_empty());
    }
}
