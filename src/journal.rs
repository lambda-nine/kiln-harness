//! The content-bearing session journal.
//!
//! The audit log is metadata-only. The journal is its deliberate counterpart:
//! it records the task, every model response, and every tool call and result
//! so a session can be replayed. It is a trusted host-side file and must never
//! be placed in or mounted into a sandbox.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

// The harness reaches kilnd over a Unix socket and is Unix-only.
use std::os::unix::fs::MetadataExt;

pub use crate::outputs::DirectoryProblem;
use crate::outputs::{Existing, OutputError, Rollback, describe_unsafe};
use crate::provider::{
    Message, ModelRequest, ModelResponse, ProviderContent, ProviderErrorKind, ProviderInfo,
    ToolCall, ToolDefinition, ToolResult, Usage,
};
use crate::tools::sanitize_call;

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
        /// The expiry the harness sent kilnd, when the daemon supports it.
        /// Not model-visible, so not in `SessionLimits`; skipped when absent
        /// so journals without it are byte-identical to before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expiry_lifetime_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expiry_idle_ms: Option<u64>,
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
        /// `cleanup_failed` whenever sandbox deletion could not be confirmed
        /// after work; otherwise the same as `work_outcome`.
        outcome: String,
        /// How the work itself ended, before cleanup.
        work_outcome: String,
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
    /// Absent from journals written before this limit existed; they load
    /// with the engine's default.
    #[serde(default = "default_max_total_model_output_bytes")]
    pub max_total_model_output_bytes: u64,
    pub max_model_arguments_bytes: usize,
    pub max_final_answer_bytes: usize,
    pub tool_limits: RecordedToolLimits,
}

fn default_max_total_model_output_bytes() -> u64 {
    crate::engine::HarnessConfig::default().max_total_model_output_bytes
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

impl From<OutputError> for JournalError {
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
    #[error("journal record {sequence} breaks the session grammar: {reason}")]
    Grammar { sequence: u64, reason: String },
    #[error("journal path {0:?} does not name a file")]
    InvalidPath(PathBuf),
    #[error("{}", describe_unsafe("journal", "--journal", path, *mode, *problem))]
    UnsafeDirectory {
        path: PathBuf,
        /// The directory's mode from `fstat`, when it could be opened.
        mode: Option<u32>,
        problem: DirectoryProblem,
    },
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
    grammar: Grammar,
}

struct Encoded {
    record: JournalRecord,
    line: String,
}

impl Chain {
    /// Encode one record. The grammar is fed last, after every other check,
    /// and `Grammar::feed` changes nothing when it fails, so a refused record
    /// leaves the chain exactly as it was.
    fn encode(&mut self, session_id: &str, event: JournalEvent) -> Result<Encoded, JournalError> {
        if self.closed {
            return Err(JournalError::Closed);
        }
        let sequence = self.next_sequence;
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
        let line =
            encode_bounded(&record, MAX_JOURNAL_LINE_BYTES)?.ok_or(JournalError::LineTooLarge {
                line: sequence.saturating_add(1),
                limit: MAX_JOURNAL_LINE_BYTES,
            })?;
        let total = self
            .bytes
            .saturating_add(line.len() as u64)
            .saturating_add(1);
        if total > MAX_JOURNAL_BYTES {
            return Err(JournalError::FileTooLarge {
                limit: MAX_JOURNAL_BYTES,
            });
        }
        self.grammar.feed(sequence, &record.event)?;
        Ok(Encoded { record, line })
    }

    fn commit(&mut self, encoded: Encoded) -> JournalRecord {
        self.next_sequence = encoded.record.sequence + 1;
        self.prev_sha256 = Some(sha256_hex(encoded.line.as_bytes()));
        self.session_id = Some(encoded.record.session_id.clone());
        self.bytes = self
            .bytes
            .saturating_add(encoded.line.len() as u64)
            .saturating_add(1);
        self.closed = matches!(encoded.record.event, JournalEvent::SessionEnded { .. });
        encoded.record
    }
}

/// A one-session JSONL journal on the host filesystem.
pub struct JsonlJournal {
    file: File,
    chain: Chain,
    /// What `open` created, kept until the first record is written.
    rollback: Option<Rollback>,
}

impl JsonlJournal {
    /// Create a new journal file with mode 0600, creating missing parents.
    ///
    /// The directory is reached through pinned descriptors: starting at `/`,
    /// each component of the resolved parent is opened relative to the
    /// previous directory with `O_DIRECTORY | O_NOFOLLOW` (and created with
    /// mode 0700 when missing), then checked with `fstat`. As with OpenSSH
    /// `StrictModes`, every directory must be owned by root or the effective
    /// user and be sticky if group- or world-writable, and the final one must
    /// be owned by the effective user and writable by no one else. The file is
    /// created relative to that verified descriptor with
    /// `O_CREAT | O_EXCL | O_NOFOLLOW`, so a parent swapped by path after a
    /// check cannot redirect it.
    ///
    /// # Errors
    ///
    /// Returns an error if the path already exists, a directory is unsafe or
    /// a symlink, or the file cannot be created.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let (file, rollback) = crate::outputs::open_output(path, Existing::Refuse)?;
        Ok(Self {
            file,
            chain: Chain::default(),
            rollback: Some(rollback),
        })
    }

    /// Remove the journal file and any directories `open` created, through
    /// the pinned directory descriptors, if nothing has been written yet.
    /// Used when a caller refuses the journal after opening it.
    pub fn discard(self) {
        if let Some(rollback) = self.rollback {
            drop(self.file);
            rollback.undo();
        }
    }

    /// The device and inode of the open journal file.
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptor cannot be queried.
    pub fn file_identity(&self) -> Result<(u64, u64), JournalError> {
        let metadata = self.file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
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
        self.chain.commit(encoded);
        self.rollback = None;
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
        let line = encoded.line.clone();
        self.records.push(self.chain.commit(encoded));
        self.lines.push(line);
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
    let file = open_journal_source(path)?;
    read_open_journal(&file, limits)
}

/// Open an existing journal read-only, refusing anything but a regular file.
///
/// # Errors
///
/// Returns an error if the path is missing, not a regular file, or cannot
/// be opened.
pub fn open_journal_source(path: &Path) -> Result<File, JournalError> {
    crate::files::open_regular_file(path).map_err(|error| match error {
        crate::files::InputFileError::Io(error) => error.into(),
        crate::files::InputFileError::NotRegularFile
        | crate::files::InputFileError::TooLarge { .. } => JournalError::NotRegularFile,
    })
}

/// Read and verify a journal from an already open handle, from its start.
///
/// # Errors
///
/// See [`read_journal`].
pub fn read_open_journal(
    file: &File,
    limits: JournalLimits,
) -> Result<LoadedJournal, JournalError> {
    use std::io::{Seek, SeekFrom};
    if !file.metadata()?.is_file() {
        return Err(JournalError::NotRegularFile);
    }
    let mut reader = file;
    reader.seek(SeekFrom::Start(0))?;
    // Never trust the metadata length: read at most one byte past the limit.
    let mut bytes = Vec::new();
    reader
        .take(limits.max_file_bytes.saturating_add(1))
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
    let mut grammar = Grammar::default();
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
        if let Some(first) = records.first()
            && first.session_id != record.session_id
        {
            return Err(JournalError::SessionMismatch {
                sequence: record.sequence,
                found: record.session_id,
                expected: first.session_id.clone(),
            });
        }
        grammar.feed(record.sequence, &record.event)?;
        prev_sha256 = sha256_hex(line);
        records.push(record);
    }
    let complete = records
        .last()
        .is_some_and(|last| matches!(last.event, JournalEvent::SessionEnded { .. }));
    Ok(LoadedJournal { records, complete })
}

/// Work outcomes the engine writes in `session_ended`.
const WORK_OUTCOMES: [&str; 5] = [
    "completed",
    "canceled",
    "deadline",
    "failed",
    "create_failed",
];
const CLEANUP_FAILED: &str = "cleanup_failed";

/// Expiry is not model-visible and feeds no request hash. The engine sends
/// both fields or neither, and never a zero.
fn check_expiry(lifetime_ms: Option<u64>, idle_ms: Option<u64>) -> Result<(), &'static str> {
    match (lifetime_ms, idle_ms) {
        (None, None) => Ok(()),
        (Some(lifetime), Some(idle)) if lifetime > 0 && idle > 0 => Ok(()),
        _ => Err("sandbox_ready expiry needs both fields, each positive"),
    }
}

/// The event grammar of one session journal.
///
/// Writers feed every event before appending it and readers feed every
/// event they load, so both accept exactly the same journals:
///
/// - `session_started` comes first, once, with schema 1.
/// - `sandbox_ready` appears at most once, before any model request; no
///   request is allowed without it. Its optional expiry fields come as a
///   pair of positive values or not at all.
/// - `model_requested` turns run 1, 2, … and each is answered by exactly one
///   `model_responded` or `model_failed` for that turn before the next
///   request. Nothing but `session_ended` follows a failure or a response
///   without tool calls.
/// - After a response with n tool calls, up to n `tool_executed` records
///   follow for that turn with indexes 1, 2, … and the response's call IDs
///   in order. Fewer than n only when the session then ends.
/// - `session_ended` matches what came before: its tool-call count, last
///   turn, and usage sum; `completed` work needs a final response without
///   tool calls whose text is the final answer; a completed outcome needs
///   confirmed deletion; `cleanup_failed` means deletion was not confirmed;
///   and a request may be left unanswered only when the work did not
///   complete.
#[derive(Debug, Clone, Default)]
pub struct Grammar {
    phase: Phase,
    context: Option<SessionContext>,
    sandbox_ready: bool,
    last_turn: u32,
    pending: Option<u32>,
    last_response: Option<RespondedTurn>,
    turn_failed: bool,
    tool_calls: u32,
    usage: Usage,
    output_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Empty,
    Open,
    Ended,
}

/// What `session_started` fixes for the whole session, and the message
/// history rebuilt from the records so far, exactly as the engine builds it.
#[derive(Debug, Clone)]
struct SessionContext {
    system: String,
    tools: Vec<ToolDefinition>,
    limits: SessionLimits,
    messages: Vec<Message>,
}

/// The last model response: its text, its calls after sanitization, and the
/// results recorded for them so far.
#[derive(Debug, Clone)]
struct RespondedTurn {
    turn: u32,
    text: Option<String>,
    calls: Vec<ToolCall>,
    /// Echoed into the history exactly as recorded, as the engine does.
    provider_content: Option<ProviderContent>,
    results: Vec<ToolResult>,
    /// Whether the engine would go on from this response: run its calls, or
    /// accept it as the final answer. See [`engine_accepts`].
    accepted: bool,
}

/// Mirror of the engine's checks on one response, in the engine's order.
/// A refused response is still journaled (the engine records it before
/// refusing it), but nothing may follow from it except `session_ended`.
fn engine_accepts(
    response: &ModelResponse,
    usage_after: Usage,
    output_bytes_after: u64,
    limits: &SessionLimits,
) -> bool {
    use crate::provider::StopReason;
    let within_budget = response.usage.output_tokens
        <= u64::from(limits.max_output_tokens_per_request)
        && usage_after.total_input_tokens() <= limits.max_total_input_tokens
        && usage_after.output_tokens <= limits.max_total_output_tokens
        && output_bytes_after <= limits.max_total_model_output_bytes;
    if !within_budget
        || matches!(
            response.stop_reason,
            StopReason::MaxTokens | StopReason::Refusal
        )
        || (response.stop_reason == StopReason::ToolUse && response.tool_calls.is_empty())
    {
        return false;
    }
    let text_bytes = response.text.as_ref().map_or(0, String::len);
    if response.tool_calls.is_empty() {
        return text_bytes > 0 && text_bytes <= limits.max_final_answer_bytes;
    }
    let mut call_ids = std::collections::BTreeSet::new();
    response.tool_calls.len() <= limits.max_tool_calls_per_turn as usize
        && response.tool_calls.iter().all(|call| {
            crate::engine::call_id_is_valid(&call.call_id) && call_ids.insert(&call.call_id)
        })
        && text_bytes <= limits.max_final_answer_bytes
}

impl Grammar {
    /// Advance by one event.
    ///
    /// # Errors
    ///
    /// Returns an error when `event` cannot follow the events fed so far.
    pub fn feed(&mut self, sequence: u64, event: &JournalEvent) -> Result<(), JournalError> {
        if self.phase == Phase::Ended {
            return Err(JournalError::RecordAfterEnd { sequence });
        }
        if self.phase == Phase::Empty {
            let JournalEvent::SessionStarted {
                schema,
                system,
                task,
                tools,
                limits,
                ..
            } = event
            else {
                return Err(JournalError::MissingSessionStarted { sequence });
            };
            if *schema != JOURNAL_SCHEMA {
                return Err(JournalError::UnsupportedSchema {
                    found: *schema,
                    expected: JOURNAL_SCHEMA,
                });
            }
            // Reject a task or limits the engine itself would refuse.
            if let Some(problem) = crate::engine::task_problem(system, task) {
                return Err(JournalError::Grammar {
                    sequence,
                    reason: format!("recorded task is invalid: {problem}"),
                });
            }
            if !crate::engine::limits_are_valid(limits) {
                return Err(JournalError::Grammar {
                    sequence,
                    reason: "recorded limits are not positive or exceed a hard ceiling".to_string(),
                });
            }
            self.context = Some(SessionContext {
                system: system.clone(),
                tools: tools.clone(),
                limits: *limits,
                messages: vec![Message::User { text: task.clone() }],
            });
            self.phase = Phase::Open;
            return Ok(());
        }
        if matches!(event, JournalEvent::SessionStarted { .. }) {
            return Err(JournalError::DuplicateSessionStarted { sequence });
        }
        self.advance(event).map_err(|reason| JournalError::Grammar {
            sequence,
            reason: reason.to_string(),
        })
    }

    /// Apply one event after `session_started`.
    fn advance(&mut self, event: &JournalEvent) -> Result<(), &'static str> {
        match event {
            JournalEvent::SessionStarted { .. } => unreachable!("handled by feed"),
            JournalEvent::SandboxReady {
                expiry_lifetime_ms,
                expiry_idle_ms,
                ..
            } => {
                if self.sandbox_ready {
                    return Err("second sandbox_ready");
                }
                if self.last_turn > 0 {
                    return Err("sandbox_ready after a model request");
                }
                check_expiry(*expiry_lifetime_ms, *expiry_idle_ms)?;
                self.sandbox_ready = true;
            }
            JournalEvent::ModelRequested {
                turn,
                request_sha256,
                message_count,
            } => {
                if !self.sandbox_ready {
                    return Err("model_requested before sandbox_ready");
                }
                if self.pending.is_some() {
                    return Err("model_requested before the previous turn was answered");
                }
                if self.turn_failed {
                    return Err("model_requested after model_failed");
                }
                if let Some(last) = &self.last_response {
                    if !last.accepted {
                        return Err("model_requested after a response the engine refuses");
                    }
                    if last.calls.is_empty() {
                        return Err("model_requested after a final response");
                    }
                    if last.results.len() != last.calls.len() {
                        return Err("model_requested before every tool call of the turn ran");
                    }
                }
                if Some(*turn) != self.last_turn.checked_add(1) {
                    return Err("model_requested turns must run 1, 2, ...");
                }
                let max_steps = self.context.as_ref().map_or(0, |ctx| ctx.limits.max_steps);
                if *turn > max_steps {
                    return Err("model_requested beyond max_steps");
                }
                self.check_request(*turn, request_sha256, *message_count)?;
                self.last_turn = *turn;
                self.pending = Some(*turn);
            }
            JournalEvent::ModelResponded { turn, response, .. } => {
                if self.pending != Some(*turn) {
                    return Err("model_responded without a matching request");
                }
                self.pending = None;
                let context = self.context.as_ref().ok_or("no session context")?;
                // Keep only what the history needs: the text and each call
                // as the model will see it.
                let calls = response
                    .tool_calls
                    .iter()
                    .map(|call| sanitize_call(call, &context.tools, &context.limits))
                    .collect();
                let usage_after = self.usage + response.usage;
                let output_bytes_after = self.output_bytes.saturating_add(response.output_bytes());
                let accepted =
                    engine_accepts(response, usage_after, output_bytes_after, &context.limits);
                self.usage = usage_after;
                self.output_bytes = output_bytes_after;
                self.last_response = Some(RespondedTurn {
                    turn: *turn,
                    text: response.text.clone(),
                    calls,
                    provider_content: response.provider_content.clone(),
                    results: Vec::new(),
                    accepted,
                });
            }
            JournalEvent::ModelFailed { turn, .. } => {
                if self.pending != Some(*turn) {
                    return Err("model_failed without a matching request");
                }
                self.pending = None;
                self.turn_failed = true;
            }
            JournalEvent::ToolExecuted {
                turn,
                index,
                call,
                result,
                ..
            } => self.record_tool(*turn, *index, call, result)?,
            JournalEvent::SessionEnded { .. } => {
                self.check_end(event)?;
                self.phase = Phase::Ended;
            }
        }
        Ok(())
    }

    /// True once `session_ended` has been fed.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.phase == Phase::Ended
    }

    /// Check one `tool_executed` against the last response and keep its
    /// result for the rebuilt history.
    fn record_tool(
        &mut self,
        turn: u32,
        index: u32,
        call: &ToolCall,
        result: &Value,
    ) -> Result<(), &'static str> {
        let Some(last) = self
            .last_response
            .as_mut()
            .filter(|_| self.pending.is_none())
        else {
            return Err("tool_executed without a model response");
        };
        if self.turn_failed || last.turn != turn {
            return Err("tool_executed for a turn other than the last response");
        }
        if !last.accepted {
            return Err("tool_executed after a response the engine refuses");
        }
        let limits = self.context.as_ref().ok_or("no session context")?.limits;
        if index > limits.max_tool_calls_per_turn {
            return Err("tool_executed beyond max_tool_calls_per_turn");
        }
        if self.tool_calls >= limits.max_tool_calls {
            return Err("tool_executed beyond max_tool_calls");
        }
        let executed = last.results.len();
        if u32::try_from(executed + 1).ok() != Some(index) {
            return Err("tool_executed indexes must run 1, 2, ...");
        }
        let Some(expected) = last.calls.get(executed) else {
            return Err("more tool_executed records than tool calls");
        };
        if expected != call {
            return Err("tool_executed call differs from the sanitized response call");
        }
        last.results.push(ToolResult {
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
            result: result.clone(),
        });
        self.tool_calls = self.tool_calls.saturating_add(1);
        Ok(())
    }

    /// Extend the rebuilt history with the last tool turn, as the engine does
    /// before its next request, and require the recorded message count and
    /// request hash to match it. On failure the history is left unchanged.
    fn check_request(
        &mut self,
        turn: u32,
        recorded_sha256: &str,
        message_count: usize,
    ) -> Result<(), &'static str> {
        let context = self.context.as_mut().ok_or("no session context")?;
        // Move the last tool turn into the history; it is rebuilt from the
        // history only if the check fails.
        let extended = self.last_response.take().map(|last| {
            context.messages.push(Message::Assistant {
                text: last.text,
                tool_calls: last.calls,
                provider_content: last.provider_content,
            });
            context.messages.push(Message::ToolResults {
                results: last.results,
            });
            last.turn
        });
        let reason = if context.messages.len() == message_count {
            let expected = request_sha256(&ModelRequest {
                session_id: "",
                turn,
                system: &context.system,
                tools: &context.tools,
                messages: &context.messages,
                max_output_tokens: context.limits.max_output_tokens_per_request,
            });
            (expected != recorded_sha256)
                .then_some("model_requested hash differs from the rebuilt request")
        } else {
            Some("model_requested message_count differs from the rebuilt history")
        };
        let Some(reason) = reason else {
            return Ok(());
        };
        if let Some(last_turn) = extended {
            let results = context.messages.pop();
            let assistant = context.messages.pop();
            if let (
                Some(Message::Assistant {
                    text,
                    tool_calls,
                    provider_content,
                }),
                Some(Message::ToolResults { results }),
            ) = (assistant, results)
            {
                // Only an accepted turn reaches this check.
                self.last_response = Some(RespondedTurn {
                    turn: last_turn,
                    text,
                    calls: tool_calls,
                    provider_content,
                    results,
                    accepted: true,
                });
            }
        }
        Err(reason)
    }

    /// Check `session_ended` against everything fed before it.
    fn check_end(&self, event: &JournalEvent) -> Result<(), &'static str> {
        let JournalEvent::SessionEnded {
            outcome,
            work_outcome,
            final_answer,
            steps,
            tool_calls,
            usage,
            sandbox_deleted,
        } = event
        else {
            return Err("not a session_ended record");
        };
        if !WORK_OUTCOMES.contains(&work_outcome.as_str()) {
            return Err("unknown work_outcome");
        }
        if outcome != work_outcome && outcome != CLEANUP_FAILED {
            return Err("outcome must be work_outcome or cleanup_failed");
        }
        if outcome == CLEANUP_FAILED && *sandbox_deleted {
            return Err("cleanup_failed with confirmed deletion");
        }
        if !*sandbox_deleted && outcome != CLEANUP_FAILED {
            return Err("unconfirmed deletion must end cleanup_failed");
        }
        if *tool_calls != self.tool_calls {
            return Err("tool_calls differs from the tool_executed records");
        }
        if *steps != self.last_turn {
            return Err("steps differs from the last turn");
        }
        if *usage != self.usage {
            return Err("usage differs from the sum of responses");
        }
        if work_outcome == "completed" {
            let final_response = self
                .last_response
                .as_ref()
                .filter(|last| last.turn == self.last_turn && last.calls.is_empty());
            if self.pending.is_some() || self.turn_failed || final_response.is_none() {
                return Err("completed work needs a final response in the last turn");
            }
            if final_response.is_some_and(|last| !last.accepted) {
                return Err("completed after a response the engine refuses");
            }
            if final_answer.is_none()
                || final_response.and_then(|last| last.text.as_ref()) != final_answer.as_ref()
            {
                return Err("final_answer differs from the final response text");
            }
        } else if final_answer.is_some() {
            return Err("final_answer without completed work");
        }
        Ok(())
    }
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

/// Serialize without ever buffering more than `limit` bytes. `Ok(None)`
/// means the encoding exceeded `limit` and serialization stopped early.
fn encode_bounded<T: Serialize>(value: &T, limit: usize) -> Result<Option<String>, JournalError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl Write for Bounded {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if buffer.len() > self.limit - self.bytes.len() {
                self.exceeded = true;
                return Err(std::io::Error::other("journal line limit exceeded"));
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if writer.exceeded {
        return Ok(None);
    }
    result?;
    // serde_json writes only valid UTF-8.
    String::from_utf8(writer.bytes)
        .map(Some)
        .map_err(|error| JournalError::Io(std::io::Error::other(error)))
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
