//! The content-bearing session journal.
//!
//! The audit log is metadata-only. The journal is its deliberate counterpart:
//! it records the task, every model response, and every tool call and result
//! so a session can be replayed. It is a trusted host-side file and must never
//! be placed in or mounted into a sandbox.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

// The harness reaches kilnd over a Unix socket and is Unix-only.
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;

use rustix::fs::{Mode, OFlags};

use crate::provider::{
    Message, ModelRequest, ModelResponse, ProviderErrorKind, ProviderInfo, ToolCall,
    ToolDefinition, ToolResult, Usage,
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
    #[error("journal path {0:?} does not name a file")]
    InvalidPath(PathBuf),
    #[error("journal directory {path:?} is unsafe: {reason}")]
    UnsafeDirectory { path: PathBuf, reason: &'static str },
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
        let name = path
            .file_name()
            .ok_or_else(|| JournalError::InvalidPath(path.to_path_buf()))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = open_verified_directory(&resolved_directory(parent)?)?;
        let fd = rustix::fs::openat(
            &directory,
            name,
            OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?;
        Ok(Self {
            file: File::from(fd),
            chain: Chain::default(),
        })
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

/// An absolute starting path for the descriptor walk: the deepest existing
/// ancestor, canonicalized once, followed by the components that do not exist
/// yet. Those name nothing on disk, so `.` and `..` among them resolve
/// lexically. The walk itself re-resolves every component without following
/// links, so this path is only a starting point.
fn resolved_directory(parent: &Path) -> Result<PathBuf, JournalError> {
    let absolute = std::path::absolute(parent)?;
    let mut existing = absolute.clone();
    let mut rest = Vec::new();
    let mut resolved = loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            break canonical;
        }
        let Some(last) = existing.components().next_back() else {
            return Err(JournalError::InvalidPath(parent.to_path_buf()));
        };
        rest.push(last.as_os_str().to_os_string());
        if !existing.pop() {
            return Err(JournalError::InvalidPath(parent.to_path_buf()));
        }
    };
    for component in rest.iter().rev() {
        match Path::new(component).components().next() {
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            _ => resolved.push(component),
        }
    }
    Ok(resolved)
}

/// Walk `directory` from `/` through pinned descriptors, creating missing
/// components, and return the verified final directory.
fn open_verified_directory(directory: &Path) -> Result<OwnedFd, JournalError> {
    let euid = rustix::process::geteuid().as_raw();
    let walk = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut current = rustix::fs::open("/", walk, Mode::empty()).map_err(std::io::Error::from)?;
    let mut shown = PathBuf::from("/");
    let components = directory
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    check_directory(&current, &shown, euid, components.is_empty())?;
    for (position, name) in components.iter().enumerate() {
        shown.push(name);
        let next = match rustix::fs::openat(&current, *name, walk, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => {
                match rustix::fs::mkdirat(&current, *name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(std::io::Error::from(error).into()),
                }
                rustix::fs::openat(&current, *name, walk, Mode::empty())
                    .map_err(|error| directory_error(&shown, error))?
            }
            Err(error) => return Err(directory_error(&shown, error)),
        };
        check_directory(&next, &shown, euid, position + 1 == components.len())?;
        current = next;
    }
    Ok(current)
}

fn directory_error(path: &Path, error: rustix::io::Errno) -> JournalError {
    if matches!(error, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) {
        JournalError::UnsafeDirectory {
            path: path.to_path_buf(),
            reason: "not a directory or a symlink",
        }
    } else {
        std::io::Error::from(error).into()
    }
}

const GROUP_OR_WORLD_WRITABLE: u32 = 0o022;
const STICKY: u32 = 0o1000;

/// Apply the ownership and mode rules to one open directory.
fn check_directory(
    directory: &OwnedFd,
    path: &Path,
    euid: u32,
    is_final: bool,
) -> Result<(), JournalError> {
    let unsafe_directory = |reason| JournalError::UnsafeDirectory {
        path: path.to_path_buf(),
        reason,
    };
    let stat = rustix::fs::fstat(directory).map_err(std::io::Error::from)?;
    let mode = u32::from(stat.st_mode);
    let owner = stat.st_uid;
    if is_final {
        if owner != euid {
            return Err(unsafe_directory("not owned by the current user"));
        }
        if mode & GROUP_OR_WORLD_WRITABLE != 0 {
            return Err(unsafe_directory("group- or world-writable"));
        }
    } else {
        if owner != 0 && owner != euid {
            return Err(unsafe_directory(
                "ancestor not owned by root or the current user",
            ));
        }
        if mode & GROUP_OR_WORLD_WRITABLE != 0 && mode & STICKY == 0 {
            return Err(unsafe_directory(
                "ancestor is group- or world-writable without the sticky bit",
            ));
        }
    }
    Ok(())
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
    // O_NONBLOCK keeps a FIFO or device from blocking the open itself;
    // fstat on the descriptor then decides, so no path can be swapped in
    // between. Reads are blocking again once the file is known to be regular.
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::LOOP => JournalError::NotRegularFile,
        error => std::io::Error::from(error).into(),
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(JournalError::NotRegularFile);
    }
    let flags = rustix::fs::fcntl_getfl(&fd).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK).map_err(std::io::Error::from)?;
    Ok(File::from(fd))
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
    context: Option<SessionContext>,
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
    results: Vec<ToolResult>,
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
            JournalEvent::SandboxReady { .. } => {
                if self.sandbox_ready {
                    return Err("second sandbox_ready");
                }
                if self.last_turn > 0 {
                    return Err("sandbox_ready after a model request");
                }
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
                self.usage = self.usage + response.usage;
                self.last_response = Some(RespondedTurn {
                    turn: *turn,
                    text: response.text.clone(),
                    calls,
                    results: Vec::new(),
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
                Some(Message::Assistant { text, tool_calls }),
                Some(Message::ToolResults { results }),
            ) = (assistant, results)
            {
                self.last_response = Some(RespondedTurn {
                    turn: last_turn,
                    text,
                    calls: tool_calls,
                    results,
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
