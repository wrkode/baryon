//! Parser for the Cloud Hypervisor event monitor stream.
//!
//! With `--event-monitor path=<file>` Cloud Hypervisor appends one
//! pretty-printed JSON object per event, separated by a blank line. The
//! writer ignores write errors, so Baryon uses a file and tails it rather
//! than a pipe. The timestamp is the time since the VMM started.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Elapsed time since VMM start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Timestamp {
    pub secs: u64,
    pub nanos: u32,
}

/// One event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Event {
    pub timestamp: Timestamp,
    pub source: String,
    pub event: String,
    #[serde(default)]
    pub properties: Option<serde_json::Value>,
}

impl Event {
    /// `source.event`, for example `vm.booted`.
    pub fn name(&self) -> String {
        format!("{}.{}", self.source, self.event)
    }
}

/// Incremental decoder. Feed it bytes as they are appended to the file and
/// it yields complete events, keeping any trailing partial object.
#[derive(Debug, Default)]
pub struct EventDecoder {
    pending: Vec<u8>,
}

impl EventDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes and return every complete event decoded so far.
    /// Objects that are complete JSON but do not match the event schema are
    /// skipped and returned as errors so the caller can log them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<Event, serde_json::Error>> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut consumed = 0;
        {
            let mut stream = serde_json::Deserializer::from_slice(&self.pending)
                .into_iter::<serde_json::Value>();
            loop {
                match stream.next() {
                    Some(Ok(value)) => {
                        consumed = stream.byte_offset();
                        out.push(serde_json::from_value::<Event>(value));
                    }
                    // An incomplete trailing object: wait for more bytes.
                    Some(Err(e)) if e.is_eof() => break,
                    Some(Err(e)) => {
                        // Corrupt data. Drop everything buffered so the
                        // decoder can resynchronize on the next object.
                        consumed = self.pending.len();
                        out.push(Err(e));
                        break;
                    }
                    None => {
                        consumed = stream.byte_offset();
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        out
    }
}

/// Tails an event monitor file from a byte offset.
#[derive(Debug)]
pub struct EventFile {
    path: PathBuf,
    offset: u64,
    decoder: EventDecoder,
}

impl EventFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            decoder: EventDecoder::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read everything appended since the last call. A missing file yields
    /// no events.
    pub async fn read_new(&mut self) -> std::io::Result<Vec<Result<Event, serde_json::Error>>> {
        let mut file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let len = file.metadata().await?.len();
        if len < self.offset {
            // Truncated: Cloud Hypervisor reopens the file with truncate on
            // start. Start over.
            self.offset = 0;
            self.decoder = EventDecoder::new();
        }
        file.seek(std::io::SeekFrom::Start(self.offset)).await?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        self.offset += buf.len() as u64;
        Ok(self.decoder.push(&buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "timestamp": {
    "secs": 0,
    "nanos": 29355
  },
  "source": "vmm",
  "event": "starting",
  "properties": null
}

{
  "timestamp": {
    "secs": 0,
    "nanos": 15030421
  },
  "source": "vm",
  "event": "booted",
  "properties": null
}

"#;

    #[test]
    fn decodes_complete_stream() {
        let mut d = EventDecoder::new();
        let events: Vec<Event> = d
            .push(SAMPLE.as_bytes())
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name(), "vmm.starting");
        assert_eq!(events[1].name(), "vm.booted");
        assert_eq!(events[1].timestamp.nanos, 15_030_421);
    }

    #[test]
    fn handles_split_writes() {
        let mut d = EventDecoder::new();
        let bytes = SAMPLE.as_bytes();
        let mut names = Vec::new();
        for chunk in bytes.chunks(7) {
            for e in d.push(chunk) {
                names.push(e.unwrap().name());
            }
        }
        assert_eq!(names, vec!["vmm.starting", "vm.booted"]);
    }

    #[test]
    fn reports_schema_mismatch_and_continues() {
        let mut d = EventDecoder::new();
        let out = d.push(b"{\"unexpected\": 1}\n\n{\"timestamp\":{\"secs\":1,\"nanos\":0},\"source\":\"vm\",\"event\":\"shutdown\"}");
        assert_eq!(out.len(), 2);
        assert!(out[0].is_err());
        assert_eq!(out[1].as_ref().unwrap().name(), "vm.shutdown");
    }

    #[tokio::test]
    async fn tails_file_and_detects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.json");
        let mut tail = EventFile::new(&path);
        assert!(tail.read_new().await.unwrap().is_empty());

        std::fs::write(&path, &SAMPLE[..60]).unwrap();
        assert!(tail.read_new().await.unwrap().is_empty());
        std::fs::write(&path, SAMPLE).unwrap();
        assert_eq!(tail.read_new().await.unwrap().len(), 2);

        // Restart truncates the file.
        std::fs::write(&path, &SAMPLE[..SAMPLE.find("\n\n").unwrap() + 2]).unwrap();
        let again = tail.read_new().await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].as_ref().unwrap().name(), "vmm.starting");
    }
}
