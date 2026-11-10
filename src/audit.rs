use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::outputs::{DirectoryProblem, Existing, OutputError, Rollback, describe_unsafe};
use crate::provider::{StopReason, Usage};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentSummary {
    pub sha256: String,
    pub bytes: usize,
}

impl ContentSummary {
    #[must_use]
    pub fn bytes(bytes: &[u8]) -> Self {
        Self {
            sha256: sha256(bytes),
            bytes: bytes.len(),
        }
    }

    /// Summarize the JSON encoding without retaining its contents.
    ///
    /// # Errors
    ///
    /// Returns an error if the value cannot be serialized.
    pub fn json(value: &Value) -> Result<Self, AuditError> {
        Self::json_bounded(value, usize::MAX)?.ok_or_else(|| {
            AuditError::Io(std::io::Error::other(
                "unbounded JSON summary exceeded usize",
            ))
        })
    }

    /// Summarize JSON without allocating its encoding, stopping at `limit`.
    ///
    /// # Errors
    ///
    /// Returns an error if the value cannot be serialized. `Ok(None)` means
    /// the encoding exceeded `limit` and serialization stopped early.
    pub fn json_bounded(value: &Value, limit: usize) -> Result<Option<Self>, AuditError> {
        let mut writer = SummaryWriter::new(limit);
        let result = serde_json::to_writer(&mut writer, value);
        if writer.exceeded {
            return Ok(None);
        }
        result?;
        Ok(Some(writer.finish()))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputEvidence {
    pub observed_bytes: u64,
    pub total_bytes_known: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    SessionOpened {
        session_timeout_ms: u64,
        network: String,
    },
    SandboxCreated {
        backend_id: String,
    },
    SandboxRecovered {
        sandbox_ids: Vec<String>,
        deleted: bool,
    },
    WorkspaceBaseline {
        ok: bool,
        execution_ids: Vec<String>,
        file_count: usize,
    },
    /// Metadata for one provider response; its content goes only to the journal.
    ModelResponded {
        turn: u32,
        stop_reason: StopReason,
        tool_calls: usize,
        usage: Usage,
        latency_ms: u64,
    },
    ModelTurn {
        turn: u32,
        action: String,
        /// Host-generated correlation ID, scoped to this session.
        audit_call_id: Option<String>,
        tool: Option<String>,
        arguments: Option<ContentSummary>,
    },
    ToolCompleted {
        turn: u32,
        /// Matches the host-generated ID in the corresponding model turn.
        audit_call_id: String,
        tool: String,
        ok: bool,
        execution_ids: Vec<String>,
        result: ContentSummary,
        stdout: OutputEvidence,
        stderr: OutputEvidence,
    },
    FinalAnswer {
        turn: u32,
        answer: ContentSummary,
    },
    SessionFinished {
        /// `cleanup_failed` whenever sandbox deletion could not be confirmed
        /// after work; otherwise the same as `work_outcome`.
        outcome: String,
        /// How the work itself ended, before cleanup.
        work_outcome: String,
        sandbox_deleted: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditRecord {
    pub sequence: u64,
    pub unix_ms: u64,
    pub session_id: String,
    pub sandbox_id: Option<String>,
    #[serde(flatten)]
    pub event: AuditEvent,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("audit I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("audit serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("audit path {0:?} does not name a file")]
    InvalidPath(std::path::PathBuf),
    #[error("{}", describe_unsafe("audit", "--audit", path, *mode, *problem))]
    UnsafeDirectory {
        path: std::path::PathBuf,
        /// The directory's mode from `fstat`, when it could be read.
        mode: Option<u32>,
        problem: DirectoryProblem,
    },
}

impl From<OutputError> for AuditError {
    fn from(error: OutputError) -> Self {
        match error {
            OutputError::InvalidPath(path) => Self::InvalidPath(path),
            OutputError::UnsafeDirectory {
                path,
                mode,
                problem,
            } => Self::UnsafeDirectory {
                path,
                mode,
                problem,
            },
            OutputError::Io(error) => Self::Io(error),
        }
    }
}

pub trait AuditSink: Send {
    /// Append one immutable event to the sink.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be serialized or persisted.
    fn append(
        &mut self,
        session_id: &str,
        sandbox_id: Option<&str>,
        event: AuditEvent,
    ) -> Result<(), AuditError>;
}

pub struct JsonlAudit {
    writer: BufWriter<File>,
    sequence: u64,
    created: bool,
    /// What `open` created, kept until the first record is written.
    rollback: Option<Rollback>,
}

impl JsonlAudit {
    /// Open or create an append-only JSONL audit file.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent or file cannot be created or opened.
    pub fn open(path: &Path) -> Result<Self, AuditError> {
        let (file, rollback) = crate::outputs::open_output(path, Existing::Append)?;
        Ok(Self {
            writer: BufWriter::new(file),
            sequence: 0,
            created: rollback.created_file(),
            rollback: Some(rollback),
        })
    }

    /// Remove the audit file and any directories `open` created, through
    /// the pinned directory descriptors, if nothing has been written yet. An
    /// existing audit that was only opened for appending is left alone.
    pub fn discard(self) {
        if let Some(rollback) = self.rollback {
            drop(self.writer);
            rollback.undo();
        }
    }

    /// Whether `open` created the file rather than appending to an existing one.
    #[must_use]
    pub fn created(&self) -> bool {
        self.created
    }

    /// The device and inode of the open audit file, from `fstat` on its
    /// descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptor cannot be queried.
    pub fn file_identity(&self) -> Result<(u64, u64), AuditError> {
        use std::os::unix::fs::MetadataExt;
        let metadata = self.writer.get_ref().metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

impl AuditSink for JsonlAudit {
    fn append(
        &mut self,
        session_id: &str,
        sandbox_id: Option<&str>,
        event: AuditEvent,
    ) -> Result<(), AuditError> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| AuditError::Io(std::io::Error::other("audit sequence overflow")))?;
        let unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| AuditError::Clock)?
                .as_millis(),
        )
        .map_err(|_| AuditError::Clock)?;
        let record = AuditRecord {
            sequence: self.sequence,
            unix_ms,
            session_id: session_id.to_string(),
            sandbox_id: sandbox_id.map(str::to_string),
            event,
        };
        serde_json::to_writer(&mut self.writer, &record)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.rollback = None;
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryAudit {
    pub records: Vec<AuditRecord>,
    sequence: u64,
}

impl AuditSink for MemoryAudit {
    fn append(
        &mut self,
        session_id: &str,
        sandbox_id: Option<&str>,
        event: AuditEvent,
    ) -> Result<(), AuditError> {
        self.sequence += 1;
        self.records.push(AuditRecord {
            sequence: self.sequence,
            unix_ms: 0,
            session_id: session_id.to_string(),
            sandbox_id: sandbox_id.map(str::to_string),
            event,
        });
        Ok(())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

struct SummaryWriter {
    hasher: Sha256,
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl SummaryWriter {
    fn new(limit: usize) -> Self {
        Self {
            hasher: Sha256::new(),
            bytes: 0,
            limit,
            exceeded: false,
        }
    }

    fn finish(self) -> ContentSummary {
        let digest = self.hasher.finalize();
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        ContentSummary {
            sha256: encoded,
            bytes: self.bytes,
        }
    }
}

impl Write for SummaryWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self.bytes.checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("JSON summary byte count overflow"));
        };
        if next > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other("JSON summary limit exceeded"));
        }
        self.hasher.update(buffer);
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
