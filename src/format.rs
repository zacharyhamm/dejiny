//! Shared types and constants for the binary recording format.
//!
//! Layout:
//!   Header: [cols: u16 LE] [rows: u16 LE]   (4 bytes)
//!   Events: [ts_us: u64 LE] [len: u32 LE] [data: len bytes]  (12 + len bytes each)

pub const HEADER_SIZE: usize = 4;
pub const EVENT_HEADER_SIZE: usize = 12; // u64 timestamp + u32 length

#[derive(Debug)]
pub struct RecordingHeader {
    pub cols: u16,
    pub rows: u16,
}

impl RecordingHeader {
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..2].copy_from_slice(&self.cols.to_le_bytes());
        buf[2..4].copy_from_slice(&self.rows.to_le_bytes());
        buf
    }

    pub fn decode(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() < HEADER_SIZE {
            anyhow::bail!("recording is empty or corrupt");
        }
        Ok(Self {
            cols: u16::from_le_bytes([data[0], data[1]]),
            rows: u16::from_le_bytes([data[2], data[3]]),
        })
    }
}

#[derive(Debug)]
pub struct RecEvent {
    pub offset: usize,
    pub ts_us: u64,
    pub length: usize,
}

pub fn parse_events(recording: &[u8]) -> anyhow::Result<Vec<RecEvent>> {
    let mut events = Vec::new();
    let mut scan = HEADER_SIZE;
    while scan + EVENT_HEADER_SIZE <= recording.len() {
        let ts_us = u64::from_le_bytes(recording[scan..scan + 8].try_into().unwrap());
        let length =
            u32::from_le_bytes(recording[scan + 8..scan + 12].try_into().unwrap()) as usize;
        scan += EVENT_HEADER_SIZE;
        if scan + length > recording.len() {
            anyhow::bail!("recording truncated");
        }
        events.push(RecEvent {
            offset: scan,
            ts_us,
            length,
        });
        scan += length;
    }
    Ok(events)
}

pub fn build_recording(cols: u16, rows: u16, events: &[(u64, &[u8])]) -> Vec<u8> {
    let header = RecordingHeader { cols, rows };
    let mut buf = Vec::new();
    buf.extend_from_slice(&header.encode());
    for (ts_us, data) in events {
        buf.extend_from_slice(&ts_us.to_le_bytes());
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(data);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        for (cols, rows) in [(80, 24), (132, 43)] {
            let h = RecordingHeader { cols, rows };
            let encoded = h.encode();
            let decoded = RecordingHeader::decode(&encoded).unwrap();
            assert_eq!(decoded.cols, cols);
            assert_eq!(decoded.rows, rows);
        }
    }

    #[test]
    fn header_roundtrip_extremes() {
        for (cols, rows) in [(0, 0), (1, 1), (u16::MAX, u16::MAX)] {
            let h = RecordingHeader { cols, rows };
            let decoded = RecordingHeader::decode(&h.encode()).unwrap();
            assert_eq!(decoded.cols, cols);
            assert_eq!(decoded.rows, rows);
        }
    }

    #[test]
    fn header_decode_too_short() {
        let err = RecordingHeader::decode(&[0u8; 2]).unwrap_err();
        assert!(err.to_string().contains("empty or corrupt"));
    }

    #[test]
    fn header_decode_exact_size() {
        let data = [80u8, 0, 24, 0]; // 80 cols, 24 rows in LE
        let h = RecordingHeader::decode(&data).unwrap();
        assert_eq!(h.cols, 80);
        assert_eq!(h.rows, 24);
    }

    #[test]
    fn parse_events_empty_recording() {
        let rec = build_recording(80, 24, &[]);
        let events = parse_events(&rec).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parse_events_single() {
        let data = b"hello";
        let rec = build_recording(80, 24, &[(1000, data)]);
        let events = parse_events(&rec).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts_us, 1000);
        assert_eq!(events[0].length, 5);
        assert_eq!(events[0].offset, HEADER_SIZE + EVENT_HEADER_SIZE);
        assert_eq!(
            &rec[events[0].offset..events[0].offset + events[0].length],
            b"hello"
        );
    }

    #[test]
    fn parse_events_multiple() {
        let rec = build_recording(80, 24, &[(1000, b"aaa"), (2000, b"bb"), (3000, b"c")]);
        let events = parse_events(&rec).unwrap();
        assert_eq!(events.len(), 3);
        assert!(events[0].ts_us < events[1].ts_us);
        assert!(events[1].ts_us < events[2].ts_us);
        // Verify contiguous offsets
        assert_eq!(
            events[1].offset,
            events[0].offset + events[0].length + EVENT_HEADER_SIZE
        );
    }

    #[test]
    fn parse_events_truncated() {
        let mut rec = build_recording(80, 24, &[(1000, b"hello world")]);
        rec.truncate(rec.len() - 3); // cut into the data
        let err = parse_events(&rec).unwrap_err();
        assert!(err.to_string().contains("truncated"));
    }

    #[test]
    fn parse_events_partial_event_header() {
        let header = RecordingHeader { cols: 80, rows: 24 };
        let mut buf = Vec::new();
        buf.extend_from_slice(&header.encode());
        buf.extend_from_slice(&[0u8; 8]); // only 8 bytes, need 12 for event header
        let events = parse_events(&buf).unwrap();
        assert!(events.is_empty());
    }
}
