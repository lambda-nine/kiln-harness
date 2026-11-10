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
    #[error("journal record {sequence} breaks the session grammar: {reason}")]
    Grammar { sequence: u64, reason: String },
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
    grammar: Grammar,
}

impl Chain {
    fn encode(&self, session_id: &str, event: JournalEvent) -> Result<Encoded, JournalError> {
        if self.closed {
            return Err(JournalError::Closed);
        }
        let sequence = self.next_sequence;
        let mut grammar = self.grammar.clone();
        grammar.feed(sequence, &event)?;
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
        Ok(Encoded {
            record,
            line,
            grammar,
        })
    }

    fn commit(&mut self, encoded: Encoded) -> JournalRecord {
        self.grammar = encoded.grammar;
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
        self.chain.commit(encoded);
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
    // Refuse FIFOs and devices before open can block on them. The path is a
    // trusted operator path; see the threat model for the remaining race.
    if !std::fs::metadata(path)?.is_file() {
        return Err(JournalError::NotRegularFile);
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(JournalError::NotRegularFile);
    }
    Ok(file)
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

/// The event grammar of one session journal.
///
/// Writers feed every event before appending it and readers feed every
/// event they load, so both accept exactly the same journals:
///
/// - `session_started` comes first, once, with schema 1.
/// - `sandbox_ready` appears at most once, before any model request; no
///   request is allowed without it.
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
    sandbox_ready: bool,
    last_turn: u32,
    pending: Option<u32>,
    last_response: Option<RespondedTurn>,
    turn_failed: bool,
    tool_calls: u32,
    usage: Usage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Empty,
    Open,
    Ended,
}

#[derive(Debug, Clone)]
struct RespondedTurn {
    turn: u32,
    text: Option<String>,
    call_ids: Vec<String>,
    executed: u32,
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
            let JournalEvent::SessionStarted { schema, .. } = event else {
                return Err(JournalError::MissingSessionStarted { sequence });
            };
            if *schema != JOURNAL_SCHEMA {
                return Err(JournalError::UnsupportedSchema {
                    found: *schema,
                    expected: JOURNAL_SCHEMA,
                });
            }
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
            JournalEvent::SandboxReady { .. } => {
                if self.sandbox_ready {
                    return Err("second sandbox_ready");
                }
                if self.last_turn > 0 {
                    return Err("sandbox_ready after a model request");
                }
                self.sandbox_ready = true;
            }
            JournalEvent::ModelRequested { turn, .. } => {
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
                    if last.call_ids.is_empty() {
                        return Err("model_requested after a final response");
                    }
                    if last.executed as usize != last.call_ids.len() {
                        return Err("model_requested before every tool call of the turn ran");
                    }
                }
                if Some(*turn) != self.last_turn.checked_add(1) {
                    return Err("model_requested turns must run 1, 2, ...");
                }
                self.last_turn = *turn;
                self.pending = Some(*turn);
            }
            JournalEvent::ModelResponded { turn, response, .. } => {
                if self.pending != Some(*turn) {
                    return Err("model_responded without a matching request");
                }
                self.pending = None;
                self.usage = self.usage + response.usage;
                self.last_response = Some(RespondedTurn {
                    turn: *turn,
                    text: response.text.clone(),
                    call_ids: response
                        .tool_calls
                        .iter()
                        .map(|call| call.call_id.clone())
                        .collect(),
                    executed: 0,
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
                turn, index, call, ..
            } => {
                let Some(last) = self
                    .last_response
                    .as_mut()
                    .filter(|_| self.pending.is_none())
                else {
                    return Err("tool_executed without a model response");
                };
                if self.turn_failed || last.turn != *turn {
                    return Err("tool_executed for a turn other than the last response");
                }
                if last.executed.checked_add(1) != Some(*index) {
                    return Err("tool_executed indexes must run 1, 2, ...");
                }
                let Some(expected) = last.call_ids.get(last.executed as usize) else {
                    return Err("more tool_executed records than tool calls");
                };
                if *expected != call.call_id {
                    return Err("tool_executed call_id differs from the response");
                }
                last.executed += 1;
                self.tool_calls = self.tool_calls.saturating_add(1);
            }
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
        if outcome == "completed" && !*sandbox_deleted {
            return Err("completed without confirmed deletion");
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
                .filter(|last| last.turn == self.last_turn && last.call_ids.is_empty());
            if self.pending.is_some() || self.turn_failed || final_response.is_none() {
                return Err("completed work needs a final response in the last turn");
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

fn unix_ms() -> Result<u64, JournalError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| JournalError::Clock)?
            .as_millis(),
    )
    .map_err(|_| JournalError::Clock)
}
