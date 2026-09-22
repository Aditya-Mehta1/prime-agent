//! Reverse JSONL scanner: read newline-delimited JSON records back-to-front.
//!
//! Ported from OpenAI Codex (`codex-rs/rollout/src/reverse_jsonl_scanner.rs`,
//! Apache-2.0): backward 64KB chunks from EOF, seek-based, with an optional
//! bounded record size so one pathological record cannot buffer without
//! limit. The session window loader scans records from the end without
//! reading the whole file.

use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use serde::de::DeserializeOwned;

const READ_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub enum ScanOutcome<T> {
    /// The record was valid JSON and deserialized as the requested type.
    Parsed(T),
    /// The record was not valid JSON for the requested type.
    Rejected(serde_json::Error),
}

/// Read-only scanner for newline-delimited JSON records, starting from the end.
pub struct ReverseJsonlScanner<R> {
    reader: R,
    next_chunk_end: u64,
    chunk_position: usize,
    chunk: Vec<u8>,
    record_reversed: Vec<u8>,
    max_record_bytes: Option<usize>,
    discarding_oversized_record: bool,
}

impl<R> ReverseJsonlScanner<R>
where
    R: Read + Seek,
{
    pub fn new(mut reader: R) -> io::Result<Self> {
        let next_chunk_end = reader.seek(SeekFrom::End(0))?;
        Self::new_at(reader, next_chunk_end)
    }

    /// Creates a reverse scanner whose logical end is the given byte offset.
    ///
    /// This lets callers scan a frozen JSONL prefix without reading records
    /// appended after that prefix was captured.
    pub fn new_at(mut reader: R, end_byte_offset: u64) -> io::Result<Self> {
        let file_len = reader.seek(SeekFrom::End(0))?;
        if end_byte_offset > file_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reverse JSONL scan end is past the file",
            ));
        }
        Ok(Self {
            reader,
            next_chunk_end: end_byte_offset,
            chunk_position: 0,
            chunk: vec![0; READ_CHUNK_SIZE],
            record_reversed: Vec::new(),
            max_record_bytes: None,
            discarding_oversized_record: false,
        })
    }

    /// Skips records larger than the configured limit without buffering or parsing them.
    pub fn with_max_record_bytes(mut self, max_record_bytes: usize) -> Self {
        self.max_record_bytes = Some(max_record_bytes);
        self
    }

    /// Scans the next nonblank record as raw bytes (newline excluded).
    ///
    /// Records over the configured limit (see [`Self::with_max_record_bytes`])
    /// are skipped without buffering, matching [`Self::scan_next`]'s discard
    /// behavior.
    pub fn scan_next_record(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if self.chunk_position == 0 {
                if self.next_chunk_end == 0 {
                    if self.discarding_oversized_record {
                        self.discarding_oversized_record = false;
                        return Ok(None);
                    }
                    return Ok(self.take_pending_record());
                }

                let read_size = usize::try_from(self.next_chunk_end.min(READ_CHUNK_SIZE as u64))
                    .map_err(io::Error::other)?;
                self.next_chunk_end -= read_size as u64;
                self.reader.seek(SeekFrom::Start(self.next_chunk_end))?;
                self.reader.read_exact(&mut self.chunk[..read_size])?;
                self.chunk_position = read_size;
            }

            let chunk = &self.chunk[..self.chunk_position];
            if let Some(newline_position) = chunk.iter().rposition(|byte| *byte == b'\n') {
                let fragment = &chunk[newline_position + 1..];
                if !self.discarding_oversized_record {
                    if self.exceeds_record_limit(fragment.len()) {
                        self.record_reversed.clear();
                        self.discarding_oversized_record = true;
                    } else {
                        self.record_reversed.extend(fragment.iter().rev().copied());
                    }
                }
                self.chunk_position = newline_position;
                if self.discarding_oversized_record {
                    self.discarding_oversized_record = false;
                    continue;
                }
                if let Some(record) = self.take_pending_record() {
                    return Ok(Some(record));
                }
            } else {
                if !self.discarding_oversized_record {
                    if self.exceeds_record_limit(chunk.len()) {
                        self.record_reversed.clear();
                        self.discarding_oversized_record = true;
                    } else {
                        self.record_reversed.extend(chunk.iter().rev().copied());
                    }
                }
                self.chunk_position = 0;
            }
        }
    }

    /// Scans the next nonblank record and deserializes it as `T`.
    ///
    /// I/O failures are returned as [`Err`]. Invalid JSON records are returned
    /// as [`ScanOutcome::Rejected`], and the scanner remains usable.
    pub fn scan_next<T>(&mut self) -> io::Result<Option<ScanOutcome<T>>>
    where
        T: DeserializeOwned,
    {
        Ok(self.scan_next_record()?.map(|record| match serde_json::from_slice::<T>(&record) {
            Ok(value) => ScanOutcome::Parsed(value),
            Err(error) => ScanOutcome::Rejected(error),
        }))
    }

    fn exceeds_record_limit(&self, incoming_bytes: usize) -> bool {
        self.max_record_bytes.is_some_and(|max_record_bytes| {
            self.record_reversed
                .len()
                .saturating_add(incoming_bytes)
                > max_record_bytes
        })
    }

    /// Reverses and returns the pending record; `None` when it is blank.
    fn take_pending_record(&mut self) -> Option<Vec<u8>> {
        if self.record_reversed.iter().all(u8::is_ascii_whitespace) {
            self.record_reversed.clear();
            return None;
        }
        self.record_reversed.reverse();
        Some(std::mem::take(&mut self.record_reversed))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Seek;

    use serde::{Deserialize, Serialize};

    use super::{ReverseJsonlScanner, ScanOutcome, READ_CHUNK_SIZE};

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct TestRecord {
        value: String,
    }

    fn record(value: &str) -> TestRecord {
        TestRecord {
            value: value.to_string(),
        }
    }

    fn parsed<T>(outcome: Option<ScanOutcome<T>>) -> T {
        let Some(ScanOutcome::Parsed(record)) = outcome else {
            panic!("expected parsed record");
        };
        record
    }

    fn assert_records<R>(scanner: &mut ReverseJsonlScanner<R>, expected: &[&str]) -> std::io::Result<()>
    where
        R: Read + Seek,
    {
        for value in expected {
            assert_eq!(parsed(scanner.scan_next::<TestRecord>()?), record(value));
        }
        assert!(scanner.scan_next::<TestRecord>()?.is_none());
        Ok(())
    }

    #[test]
    fn scans_jsonl_records_from_end() -> std::io::Result<()> {
        let input = br#"{"value":"first"}
{"value":"second"}
{"value":"third"}
"#;

        assert_records(
            &mut ReverseJsonlScanner::new(Cursor::new(input))?,
            &["third", "second", "first"],
        )
    }

    #[test]
    fn rejects_invalid_json_and_continues_scanning() -> std::io::Result<()> {
        let input = br#"{"value":"first"}
not-json
{"value":"third"}
"#;
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(input))?;

        assert_eq!(parsed(scanner.scan_next::<TestRecord>()?), record("third"));
        let Some(ScanOutcome::Rejected(error)) = scanner.scan_next::<TestRecord>()? else {
            panic!("expected rejected record");
        };
        assert!(error.is_syntax());
        assert_eq!(parsed(scanner.scan_next::<TestRecord>()?), record("first"));
        Ok(())
    }

    #[test]
    fn skips_records_over_the_configured_limit() -> std::io::Result<()> {
        let oversized = record(&"x".repeat(128));
        let input = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&record("first"))?,
            serde_json::to_string(&oversized)?,
            serde_json::to_string(&record("third"))?
        );
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(input.into_bytes()))?
            .with_max_record_bytes(/*max_record_bytes*/ 32);

        assert_records(&mut scanner, &["third", "first"])
    }

    #[test]
    fn accepts_valid_json_at_eof() -> std::io::Result<()> {
        let input = b"{\"value\":\"first\"}\n{\"value\":\"second\"}";

        assert_records(
            &mut ReverseJsonlScanner::new(Cursor::new(input))?,
            &["second", "first"],
        )
    }

    #[test]
    fn scans_from_a_frozen_prefix_end() -> std::io::Result<()> {
        let prefix = b"{\"value\":\"first\"}\n{\"value\":\"second\"}\n";
        let mut input = prefix.to_vec();
        input.extend_from_slice(b"{\"value\":\"later\"}\n");

        assert_records(
            &mut ReverseJsonlScanner::new_at(Cursor::new(input), prefix.len() as u64)?,
            &["second", "first"],
        )
    }

    #[test]
    fn rejects_invalid_json_at_eof_and_continues_scanning() -> std::io::Result<()> {
        let input = b"{\"value\":\"first\"}\n{\"value\":";
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(input))?;

        let Some(ScanOutcome::Rejected(error)) = scanner.scan_next::<TestRecord>()? else {
            panic!("expected rejected record");
        };
        assert!(error.is_eof());
        assert_eq!(parsed(scanner.scan_next::<TestRecord>()?), record("first"));
        Ok(())
    }

    #[test]
    fn skips_blank_lines_with_or_without_termination() -> std::io::Result<()> {
        let input = b"{\"value\":\"first\"}\r\n\n \t\r";

        assert_records(
            &mut ReverseJsonlScanner::new(Cursor::new(input))?,
            &["first"],
        )
    }

    #[test]
    fn scans_across_read_chunk_boundaries() -> std::io::Result<()> {
        let empty_record_len = serde_json::to_string(&record(""))?.len();
        for distance_from_eof in [
            READ_CHUNK_SIZE - 1,
            READ_CHUNK_SIZE,
            READ_CHUNK_SIZE + 1,
        ] {
            let large_value = "x".repeat(distance_from_eof - empty_record_len - 2);
            let input = format!(
                "{}\n{}\n",
                serde_json::to_string(&record("first"))?,
                serde_json::to_string(&record(&large_value))?
            );
            let mut scanner = ReverseJsonlScanner::new(Cursor::new(input.into_bytes()))?;

            assert_eq!(
                parsed(scanner.scan_next::<TestRecord>()?),
                record(&large_value)
            );
            assert_eq!(parsed(scanner.scan_next::<TestRecord>()?), record("first"));
        }
        Ok(())
    }

    #[test]
    fn scans_record_spanning_three_read_chunks() -> std::io::Result<()> {
        let large_value = "x".repeat(READ_CHUNK_SIZE * 2);
        let input = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&record("first"))?,
            serde_json::to_string(&record(&large_value))?,
            serde_json::to_string(&record("third"))?
        );
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(input.into_bytes()))?;

        assert_records(&mut scanner, &["third", &large_value, "first"])
    }

    #[test]
    fn raw_records_scan_from_end_without_parsing() -> std::io::Result<()> {
        let input = br#"{"a":1}
not-json

{"b":[1,2,3]}
"#;
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(input))?;
        assert_eq!(scanner.scan_next_record()?.as_deref(), Some(&b"{\"b\":[1,2,3]}"[..]));
        assert_eq!(scanner.scan_next_record()?.as_deref(), Some(&b"not-json"[..]));
        assert_eq!(scanner.scan_next_record()?.as_deref(), Some(&b"{\"a\":1}"[..]));
        assert!(scanner.scan_next_record()?.is_none());
        Ok(())
    }

    #[test]
    fn raw_records_skip_oversized_records() -> std::io::Result<()> {
        let input = format!(
            "{{\"value\":\"first\"}}\n{{\"value\":\"{}\"}}\n{{\"value\":\"third\"}}\n",
            "x".repeat(64)
        );
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(input.into_bytes()))?
            .with_max_record_bytes(32);
        assert_eq!(
            scanner.scan_next_record()?.as_deref(),
            Some(&b"{\"value\":\"third\"}"[..])
        );
        assert_eq!(
            scanner.scan_next_record()?.as_deref(),
            Some(&b"{\"value\":\"first\"}"[..])
        );
        assert!(scanner.scan_next_record()?.is_none());
        Ok(())
    }

}
