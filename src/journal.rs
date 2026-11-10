//! The content-bearing session journal.
//!
//! The audit log is metadata-only. The journal is its deliberate counterpart:
//! it records the task, every model response, and every tool call and result
//! so a session can be replayed. It is a trusted host-side file and must never
//! be placed in or mounted into a sandbox.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::provider::{
    ModelRequest, ModelResponse, ProviderErrorKind, ProviderInfo, ToolCall, ToolDefinition, Usage,
};

pub const JOURNAL_SCHEMA: u32 = 1;
/// Largest journal file `read_journal` accepts and a sink will write.
pub const MAX_JOURNAL_BYTES: u64 = 64 << 20;
/// Largest single record line, excluding its newline.
pub const MAX_JOURNAL_LINE_BYTES: usize = 8 << 20;
/// `prev_sha256` of the first record.
pub const GENESIS_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JournalEvent {
    SessionStarted {
        schema: u32,
        harness_version: String,
        provider: ProviderInfo,
        system: String,
        task: String,
        tools: Vec<ToolDefinition>,
        limits: SessionLimits,
    },
    SandboxReady {
        sandbox_id: String,
        baseline_file_count: usize,
    },
    ModelRequested {
        turn: u32,
        request_sha256: String,
        message_count: usize,
    },
    ModelResponded {
        turn: u32,
        response: ModelResponse,
        latency_ms: u64,
    },
    ModelFailed {
        turn: u32,
        kind: ProviderErrorKind,
        message: String,
    },
    ToolExecuted {
        turn: u32,
        index: u32,
        audit_call_id: String,
        call: ToolCall,
        ok: bool,
        result: Value,
        execution_ids: Vec<String>,
    },
    SessionEnded {
        outcome: String,
        final_answer: Option<String>,
        steps: u32,
        tool_calls: u32,
        usage: Usage,
        sandbox_deleted: bool,
    },
}

/// Every limit that can change model-visible behavior, recorded so replay
/// runs under exactly the same ones. Lifecycle cleanup windows are not
/// model-visible and are not recorded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionLimits {
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_tool_calls_per_turn: u32,
    pub session_timeout_ms: u64,
    pub max_output_tokens_per_request: u32,
    pub max_total_input_tokens: u64,
    pub max_total_output_tokens: u64,
    pub max_model_arguments_bytes: usize,
    pub max_final_answer_bytes: usize,
    pub tool_limits: RecordedToolLimits,
}

/// The recorded form of [`crate::tools::ToolLimits`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordedToolLimits {
    pub timeout_ms: u64,
    pub max_argv_bytes: usize,
    pub max_stdin_bytes: usize,
    pub max_file_bytes: usize,
    pub max_result_bytes: usize,
    pub max_manifest_bytes: usize,
    pub max_manifest_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalRecord {
    /// Starts at 0 and is contiguous.
    pub sequence: u64,
    pub unix_ms: u64,
    pub session_id: String,
    /// SHA-256 hex of the previous record's exact line bytes, without the
    /// newline; [`GENESIS_SHA256`] for sequence 0.
    pub prev_sha256: String,
    #[serde(flatten)]
    pub event: JournalEvent,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("journal path is not a regular file")]
    NotRegularFile,
    #[error("journal exceeds {limit} bytes")]
    FileTooLarge { limit: u64 },
    #[error("journal line {line} exceeds {limit} bytes")]
    LineTooLarge { line: u64, limit: usize },
    #[error("journal line {line} is empty")]
    EmptyLine { line: u64 },
    #[error("journal line {line} has no terminating newline")]
    UnterminatedLine { line: u64 },
    #[error("journal line {line} is not a valid record: {source}")]
    InvalidRecord {
        line: u64,
        #[source]
        source: serde_json::Error,
    },
    #[error("journal is empty")]
    Empty,
    #[error("journal record {sequence} is not session_started where one is required")]
    MissingSessionStarted { sequence: u64 },
    #[error("journal record {sequence} starts a second session")]
    DuplicateSessionStarted { sequence: u64 },
    #[error("journal schema {found} is unsupported; expected {expected}")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("journal record {sequence} has session ID {found:?}, expected {expected:?}")]
    SessionMismatch {
        sequence: u64,
        found: String,
        expected: String,
    },
    #[error("journal record at line {line} has sequence {found}, expected {expected}")]
    SequenceGap {
        line: u64,
        found: u64,
        expected: u64,
    },
    #[error("journal record {sequence} does not chain to the previous record")]
    ChainMismatch { sequence: u64 },
    #[error("journal record {sequence} follows session_ended")]
    RecordAfterEnd { sequence: u64 },
    #[error("journal sink is closed after a failed write or a finished session")]
    Closed,
}

pub trait JournalSink: Send {
    /// Append one record to the session journal.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be encoded, would exceed a
    /// journal limit, breaks the single-session record order, or cannot be
    /// persisted.
    fn append(&mut self, session_id: &str, event: JournalEvent) -> Result<(), JournalError>;
}

/// Hash-chain and ordering state shared by every sink, so writers refuse
/// exactly the journals that `read_journal` would reject.
#[derive(Debug, Default)]
struct Chain {
    next_sequence: u64,
    prev_sha256: Option<String>,
    session_id: Option<String>,
    bytes: u64,
    closed: bool,
}

struct Encoded {
    record: JournalRecord,
    line: String,
}

impl Chain {
    fn encode(&self, session_id: &str, event: JournalEvent) -> Result<Encoded, JournalError> {
        if self.closed {
            return Err(JournalError::Closed);
        }
        let sequence = self.next_sequence;
        let starting = matches!(event, JournalEvent::SessionStarted { .. });
        if sequence == 0 && !starting {
            return Err(JournalError::MissingSessionStarted { sequence });
        }
        if sequence > 0 && starting {
            return Err(JournalError::DuplicateSessionStarted { sequence });
        }
        if let Some(expected) = &self.session_id
            && expected != session_id
        {
            return Err(JournalError::SessionMismatch {
                sequence,
                found: session_id.to_string(),
                expected: expected.clone(),
            });
        }
        let record = JournalRecord {
            sequence,
            unix_ms: unix_ms()?,
            session_id: session_id.to_string(),
            prev_sha256: self
                .prev_sha256
                .clone()
                .unwrap_or_else(|| GENESIS_SHA256.to_string()),
            event,
        };
        let line = serde_json::to_string(&record)?;
        if line.len() > MAX_JOURNAL_LINE_BYTES {
            return Err(JournalError::LineTooLarge {
                line: sequence.saturating_add(1),
                limit: MAX_JOURNAL_LINE_BYTES,
            });
        }
        let total = self
            .bytes
            .saturating_add(line.len() as u64)
            .saturating_add(1);
        if total > MAX_JOURNAL_BYTES {
            return Err(JournalError::FileTooLarge {
                limit: MAX_JOURNAL_BYTES,
            });
        }
        Ok(Encoded { record, line })
    }

    fn commit(&mut self, encoded: &Encoded) {
        self.next_sequence = encoded.record.sequence + 1;
        self.prev_sha256 = Some(sha256_hex(encoded.line.as_bytes()));
        self.session_id = Some(encoded.record.session_id.clone());
        self.bytes = self
            .bytes
            .saturating_add(encoded.line.len() as u64)
            .saturating_add(1);
        self.closed = matches!(encoded.record.event, JournalEvent::SessionEnded { .. });
    }
}

/// A one-session JSONL journal on the host filesystem.
pub struct JsonlJournal {
    file: File,
    chain: Chain,
}

impl JsonlJournal {
    /// Create a new journal file with mode 0600, creating missing parents.
    ///
    /// # Errors
    ///
    /// Returns an error if the path already exists or cannot be created.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        // create_new refuses existing files and symlinks.
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        Ok(Self {
            file,
            chain: Chain::default(),
        })
    }
}

impl JournalSink for JsonlJournal {
    fn append(&mut self, session_id: &str, event: JournalEvent) -> Result<(), JournalError> {
        let encoded = self.chain.encode(session_id, event)?;
        let mut bytes = Vec::with_capacity(encoded.line.len() + 1);
        bytes.extend_from_slice(encoded.line.as_bytes());
        bytes.push(b'\n');
        let written = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.sync_data());
        if let Err(error) = written {
            // A partial line may be on disk; nothing may follow it.
            self.chain.closed = true;
            return Err(error.into());
        }
        self.chain.commit(&encoded);
        Ok(())
    }
}

/// An in-memory journal for tests. `lines` holds the exact encoded records.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    pub records: Vec<JournalRecord>,
    pub lines: Vec<String>,
    chain: Chain,
}

impl JournalSink for MemoryJournal {
    fn append(&mut self, session_id: &str, event: JournalEvent) -> Result<(), JournalError> {
        let encoded = self.chain.encode(session_id, event)?;
        self.chain.commit(&encoded);
        self.records.push(encoded.record);
        self.lines.push(encoded.line);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedJournal {
    pub records: Vec<JournalRecord>,
    /// True when the last record is `session_ended`.
    pub complete: bool,
}

impl LoadedJournal {
    /// The session ID shared by every record.
    #[must_use]
    pub fn session_id(&self) -> &str {
        self.records
            .first()
            .map_or("", |record| record.session_id.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    pub max_file_bytes: u64,
    pub max_line_bytes: usize,
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_JOURNAL_BYTES,
            max_line_bytes: MAX_JOURNAL_LINE_BYTES,
        }
    }
}

/// Read and verify a journal with the default limits.
///
/// # Errors
///
/// Returns a typed error for I/O failures, limit violations, malformed
/// records, schema or session mismatches, sequence gaps, and broken chains.
pub fn read_journal(path: &Path) -> Result<LoadedJournal, JournalError> {
    read_journal_with_limits(path, JournalLimits::default())
}

/// Read and verify a journal with explicit limits.
///
/// # Errors
///
/// See [`read_journal`].
pub fn read_journal_with_limits(
    path: &Path,
    limits: JournalLimits,
) -> Result<LoadedJournal, JournalError> {
    // Refuse FIFOs and devices before open can block on them.
    if !std::fs::metadata(path)?.is_file() {
        return Err(JournalError::NotRegularFile);
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(JournalError::NotRegularFile);
    }
    // Never trust the metadata length: read at most one byte past the limit.
    let mut bytes = Vec::new();
    file.take(limits.max_file_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(JournalError::FileTooLarge {
            limit: limits.max_file_bytes,
        });
    }
    parse_journal(&bytes, limits.max_line_bytes)
}

fn parse_journal(bytes: &[u8], max_line_bytes: usize) -> Result<LoadedJournal, JournalError> {
    if bytes.is_empty() {
        return Err(JournalError::Empty);
    }
    let mut records: Vec<JournalRecord> = Vec::new();
    let mut prev_sha256 = GENESIS_SHA256.to_string();
    let mut rest = bytes;
    let mut line_number = 0u64;
    while !rest.is_empty() {
        line_number += 1;
        let Some(end) = rest.iter().position(|byte| *byte == b'\n') else {
            if rest.len() > max_line_bytes {
                return Err(JournalError::LineTooLarge {
                    line: line_number,
                    limit: max_line_bytes,
                });
            }
            return Err(JournalError::UnterminatedLine { line: line_number });
        };
        let line = &rest[..end];
        rest = &rest[end + 1..];
        if line.len() > max_line_bytes {
            return Err(JournalError::LineTooLarge {
                line: line_number,
                limit: max_line_bytes,
            });
        }
        if line.is_empty() {
            return Err(JournalError::EmptyLine { line: line_number });
        }
        let record: JournalRecord =
            serde_json::from_slice(line).map_err(|source| JournalError::InvalidRecord {
                line: line_number,
                source,
            })?;
        let expected = records.len() as u64;
        if record.sequence != expected {
            return Err(JournalError::SequenceGap {
                line: line_number,
                found: record.sequence,
                expected,
            });
        }
        if record.prev_sha256 != prev_sha256 {
            return Err(JournalError::ChainMismatch {
                sequence: record.sequence,
            });
        }
        match (records.first(), &record.event) {
            (None, JournalEvent::SessionStarted { schema, .. }) => {
                if *schema != JOURNAL_SCHEMA {
                    return Err(JournalError::UnsupportedSchema {
                        found: *schema,
                        expected: JOURNAL_SCHEMA,
                    });
                }
            }
            (None, _) => {
                return Err(JournalError::MissingSessionStarted {
                    sequence: record.sequence,
                });
            }
            (Some(_), JournalEvent::SessionStarted { .. }) => {
                return Err(JournalError::DuplicateSessionStarted {
                    sequence: record.sequence,
                });
            }
            (Some(first), _) => {
                if first.session_id != record.session_id {
                    return Err(JournalError::SessionMismatch {
                        sequence: record.sequence,
                        found: record.session_id,
                        expected: first.session_id.clone(),
                    });
                }
            }
        }
        if records
            .last()
            .is_some_and(|last| matches!(last.event, JournalEvent::SessionEnded { .. }))
        {
            return Err(JournalError::RecordAfterEnd {
                sequence: record.sequence,
            });
        }
        prev_sha256 = sha256_hex(line);
        records.push(record);
    }
    let complete = records
        .last()
        .is_some_and(|last| matches!(last.event, JournalEvent::SessionEnded { .. }));
    Ok(LoadedJournal { records, complete })
}

/// Version of the request-hash input layout.
pub const REQUEST_HASH_VERSION: u32 = 1;

/// SHA-256 hex of the canonical JSON encoding of a model request's content:
/// `{"v", "system", "tools", "messages", "max_output_tokens"}`.
///
/// `session_id` is deliberately excluded because a replay runs under a fresh
/// session. `turn` is excluded because replay compares it separately.
///
/// The encoding is deterministic because `json!` builds a `serde_json::Map`,
/// which is a sorted `BTreeMap` unless `serde_json`'s `preserve_order`
/// feature is enabled. This crate must never enable it: object keys, including
/// those inside model-supplied tool arguments, would then hash in insertion
/// order and replay would diverge on equal requests.
///
/// # Panics
///
/// Never in practice: encoding an in-memory JSON value cannot fail.
#[must_use]
pub fn request_sha256(request: &ModelRequest<'_>) -> String {
    let request = json!({
        "v": REQUEST_HASH_VERSION,
        "system": request.system,
        "tools": request.tools,
        "messages": request.messages,
        "max_output_tokens": request.max_output_tokens,
    });
    let encoded = serde_json::to_vec(&request).expect("JSON values always serialize");
    sha256_hex(&encoded)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn unix_ms() -> Result<u64, JournalError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| JournalError::Clock)?
            .as_millis(),
    )
    .map_err(|_| JournalError::Clock)
}
