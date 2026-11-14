//! The [`ToolSet::CodingV1`] tools: `read_file` with line ranges,
//! `edit_file` and `search`.
//!
//! Each reaches the workspace only through fixed argv the harness builds,
//! with the model's text as exactly one argv entry (a validated path or a
//! search pattern) or as stdin (an edited file's new bytes). Everything
//! else happens on the host, bounded: a ranged read scans at most
//! [`MAX_SCAN_BYTES`], an edit makes at most [`MAX_REPLACEMENTS`]
//! replacements and computes the edited size before allocating it, and a
//! search keeps at most `max_results` matches.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{
    DEFAULT_SEARCH_RESULTS, ERROR_STDERR_BYTES, EarlierExecutions, MAX_EDITS, MAX_LINE_COUNT,
    MAX_PATTERN_BYTES, MAX_REPLACEMENTS, MAX_SCAN_BYTES, MAX_SEARCH_RESULTS, MAX_START_LINE,
    RESULT_RESERVE_BYTES, SEARCH_TEXT_BYTES, ToolError, ToolLimits, ToolPlan, ToolRun, ToolSession,
    WORKSPACE, bounded_text, combine_execution_error, complete, evidence, execution_error,
    execution_error_with_stderr, merge_evidence, parse_arguments, refused_after, tool_error_run,
    with_executions, workspace_path, write_argv,
};
use crate::api::{Execution, ExecutionState, SandboxApi};
use crate::provider::ToolDefinition;

/// The most files one `grep` execution searches.
const GREP_CHUNK_FILES: usize = 256;

/// Replace `read_file` in the base tools with the coding one, then append
/// `edit_file` and `search` after `workspace_diff`.
pub(super) fn extend_definitions(tools: &mut Vec<ToolDefinition>, limits: &ToolLimits) {
    let file_kib = limits.max_file_bytes / 1024;
    if let Some(read) = tools.iter_mut().find(|tool| tool.name == "read_file") {
        *read = ToolDefinition {
            name: "read_file".to_string(),
            description: "Read one /workspace file, given as a relative path (notes.txt) or \
                          under /workspace/. Without start_line or line_count it reads a bounded \
                          prefix; with either it reads whole lines from start_line (default 1), \
                          at most line_count (default and maximum 5000), within the first 1 MiB \
                          of the file."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1},
                    "start_line": {"type": "integer", "minimum": 1},
                    "line_count": {"type": "integer", "minimum": 1}
                }
            }),
        };
    }
    tools.push(ToolDefinition {
        name: "edit_file".to_string(),
        description: format!(
            "Replace exact text in one /workspace file of at most {file_kib} KiB (path as for \
             read_file). Each old_text must occur exactly once in the current file unless \
             replace_all is true, which replaces every non-overlapping occurrence. Edits are \
             matched against the file before any of them applies, may not overlap, and apply \
             together or not at all. Matching is byte-exact: no whitespace or line-ending \
             normalization."
        ),
        input_schema: json!({
            "type": "object",
            "required": ["path", "edits"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_EDITS,
                    "items": {
                        "type": "object",
                        "required": ["old_text", "new_text"],
                        "additionalProperties": false,
                        "properties": {
                            "old_text": {"type": "string"},
                            "new_text": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        }
                    }
                }
            }
        }),
    });
    tools.push(ToolDefinition {
        name: "search".to_string(),
        description: "Search the regular files under one /workspace path (default the whole \
                      workspace; a file or a directory, as for list_dir) for lines containing \
                      pattern: a literal string, or a POSIX extended regular expression when \
                      regex is true. Results are path:line:text lines in path order, at most \
                      max_results (default 50). Files whose names contain ':' or are not plain \
                      UTF-8 are skipped and counted."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["pattern"],
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "regex": {"type": "boolean"},
                "ignore_case": {"type": "boolean"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS}
            }
        }),
    });
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingReadFileArgs {
    path: String,
    max_bytes: Option<usize>,
    start_line: Option<u32>,
    line_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArgs {
    path: String,
    edits: Vec<EditArgs>,
}

/// One `edit_file` edit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditArgs {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    #[serde(default)]
    pub(crate) replace_all: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    pattern: String,
    path: Option<String>,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    ignore_case: bool,
    max_results: Option<u32>,
}

/// A `search` call that passed every host-side check.
pub(super) struct SearchPlan {
    root: String,
    pattern: String,
    regex: bool,
    ignore_case: bool,
    max_results: u32,
}

#[allow(clippy::result_large_err)]
impl<A: SandboxApi> ToolSession<'_, A> {
    /// A coding `read_file`: without a line field it is the base read.
    pub(super) fn plan_coding_read(&self, arguments: Value) -> Result<ToolPlan, ToolRun> {
        let args: CodingReadFileArgs = parse_arguments(arguments)?;
        let path = workspace_path(&args.path, false)
            .map_err(|message| ToolRun::rejected("invalid_path", message))?;
        if args.start_line.is_none() && args.line_count.is_none() {
            let max_bytes = self.read_max_bytes(args.max_bytes)?;
            return Ok(ToolPlan::Read { path, max_bytes });
        }
        let start = args.start_line.unwrap_or(1);
        if !(1..=MAX_START_LINE).contains(&start) {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                format!("start_line must be 1 to {MAX_START_LINE}"),
            ));
        }
        let count = args.line_count.unwrap_or(MAX_LINE_COUNT);
        if !(1..=MAX_LINE_COUNT).contains(&count) {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                format!("line_count must be 1 to {MAX_LINE_COUNT}"),
            ));
        }
        let max_bytes = self.read_max_bytes(args.max_bytes)?;
        Ok(ToolPlan::ReadLines {
            path,
            start,
            count,
            max_bytes,
        })
    }

    pub(super) fn plan_edit(&self, arguments: Value) -> Result<ToolPlan, ToolRun> {
        let args: EditFileArgs = parse_arguments(arguments)?;
        let path = workspace_path(&args.path, false)
            .map_err(|message| ToolRun::rejected("invalid_path", message))?;
        if args.edits.is_empty() || args.edits.len() > MAX_EDITS {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                format!("edits must hold 1 to {MAX_EDITS} edits"),
            ));
        }
        for (index, edit) in args.edits.iter().enumerate() {
            let problem = if edit.old_text.is_empty() {
                Some("old_text is empty")
            } else if edit.old_text == edit.new_text {
                Some("new_text equals old_text")
            } else if edit.old_text.len() > self.limits.max_file_bytes
                || edit.new_text.len() > self.limits.max_file_bytes
            {
                Some("text exceeds the per-file limit")
            } else {
                None
            };
            if let Some(problem) = problem {
                return Err(ToolRun::rejected(
                    "invalid_arguments",
                    format!("edit {index}: {problem}"),
                ));
            }
        }
        Ok(ToolPlan::Edit {
            path,
            edits: args.edits,
        })
    }

    pub(super) fn plan_search(arguments: Value) -> Result<SearchPlan, ToolRun> {
        let args: SearchArgs = parse_arguments(arguments)?;
        if args.pattern.is_empty()
            || args.pattern.len() > MAX_PATTERN_BYTES
            || args.pattern.contains(['\n', '\0'])
        {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                format!("pattern must be 1 to {MAX_PATTERN_BYTES} bytes with no newline or NUL"),
            ));
        }
        let root = workspace_path(args.path.as_deref().unwrap_or("."), true)
            .map_err(|message| ToolRun::rejected("invalid_path", message))?;
        let max_results = args.max_results.unwrap_or(DEFAULT_SEARCH_RESULTS);
        if !(1..=MAX_SEARCH_RESULTS).contains(&max_results) {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                format!("max_results must be 1 to {MAX_SEARCH_RESULTS}"),
            ));
        }
        Ok(SearchPlan {
            root,
            pattern: args.pattern,
            regex: args.regex,
            ignore_case: args.ignore_case,
            max_results,
        })
    }

    /// Run one fixed argv for a coding tool, under the tool timeout, after
    /// `earlier`, the executions this call already settled. A failure keeps
    /// them first in its IDs and evidence. When the failed execution's
    /// outcome is unknown (its RPC failed, or the deadline passed), the run
    /// also says so, so response-loss recovery still looks for it; when the
    /// sandbox was already unusable nothing was sent, and there is nothing
    /// to look for. A settled execution is also recorded outside the call's
    /// future ([`ToolSession::record_settled`]), so a call dropped by
    /// cancellation keeps it too.
    async fn run_fixed(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        deadline: Instant,
        earlier: &[&Execution],
    ) -> Result<Execution, ToolRun> {
        let unusable_before = self.unusable().is_some();
        let error = match self.run(argv, stdin, self.limits.timeout, deadline).await {
            Ok(execution) => {
                self.record_settled(&execution);
                return Ok(execution);
            }
            Err(error) => error,
        };
        let unresolved = matches!(error, ToolError::Deadline)
            || (matches!(error, ToolError::Api(_)) && !unusable_before);
        let mut run = tool_error_run(error, "kilnd_error", self.typed_reasons);
        if earlier.is_empty() {
            return Err(run);
        }
        if unresolved {
            run.earlier = Some(EarlierExecutions::of(earlier));
        }
        with_executions(&mut run, earlier);
        Err(run)
    }

    /// Whole lines `start ..` of `path`, at most `count`, from its first
    /// [`MAX_SCAN_BYTES`]: `stat` for the size, then `head`, then slicing
    /// on the host, byte for byte.
    pub(super) async fn read_lines(
        &self,
        path: String,
        start: u32,
        count: u32,
        max_bytes: usize,
        deadline: Instant,
    ) -> ToolRun {
        let sized = match self
            .run_fixed(stat_argv(&path), Vec::new(), deadline, &[])
            .await
        {
            Ok(execution) => execution,
            Err(run) => return run,
        };
        if !sized.succeeded() {
            return execution_error(&sized, "stat_failed");
        }
        let Some(size) = std::str::from_utf8(&sized.stdout.data)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        else {
            return execution_error(&sized, "invalid_stat_output");
        };
        let read = match self
            .run_fixed(
                head_argv(MAX_SCAN_BYTES, path),
                Vec::new(),
                deadline,
                &[&sized],
            )
            .await
        {
            Ok(execution) => execution,
            Err(run) => return run,
        };
        if !read.succeeded() {
            return combine_execution_error(&sized, &read, "read_failed");
        }
        let eof_known = complete(&read.stdout) && read.stdout.data.len() as u64 == size;
        let budget = max_bytes.min(
            self.limits
                .max_result_bytes
                .saturating_sub(RESULT_RESERVE_BYTES),
        );
        let Some(slice) = slice_lines(&read.stdout.data, eof_known, start, count, budget) else {
            return refused_after(
                &[&sized, &read],
                "beyond_scan_limit",
                format!(
                    "line ranges cover the first {} KiB of a file; use exec for later lines",
                    MAX_SCAN_BYTES / 1024
                ),
            );
        };
        let truncated = slice.next_line.is_some() || slice.line_truncated;
        let mut run = ToolRun {
            ok: true,
            result: json!({
                "ok": true,
                "content": slice.content,
                "utf8": slice.utf8,
                "file_bytes": size,
                "start_line": start,
                "returned_lines": slice.returned,
                "next_line": slice.next_line,
                "line_truncated": slice.line_truncated,
                "truncated": truncated
            }),
            execution_ids: vec![sized.id.clone(), read.id.clone()],
            stdout: merge_evidence(evidence(&sized.stdout), evidence(&read.stdout)),
            stderr: merge_evidence(evidence(&sized.stderr), evidence(&read.stderr)),
            stop: None,
            live: None,
            earlier: None,
        };
        run.stdout.truncated |= truncated;
        run
    }

    /// Read the whole file, apply every edit on the host, and write the
    /// result back with `dd`, or refuse and write nothing.
    pub(super) async fn edit_file(
        &self,
        path: String,
        edits: Vec<EditArgs>,
        deadline: Instant,
    ) -> ToolRun {
        let max_file_bytes = self.limits.max_file_bytes;
        let file_kib = max_file_bytes / 1024;
        let read = match self
            .run_fixed(
                head_argv(max_file_bytes.saturating_add(1), path.clone()),
                Vec::new(),
                deadline,
                &[],
            )
            .await
        {
            Ok(execution) => execution,
            Err(run) => return run,
        };
        if !read.succeeded() {
            return execution_error_with_stderr(&read, "read_failed");
        }
        // Only every byte of the file may become an edit's input: a prefix
        // written back would cut the file.
        if !complete(&read.stdout) {
            return refused_after(
                &[&read],
                "read_incomplete",
                "kilnd did not return the whole file; nothing was written".to_string(),
            );
        }
        if read.stdout.data.len() > max_file_bytes {
            return refused_after(
                &[&read],
                "file_too_large",
                format!("the file exceeds {file_kib} KiB; use exec"),
            );
        }
        let Ok(content) = std::str::from_utf8(&read.stdout.data) else {
            return refused_after(
                &[&read],
                "not_utf8",
                "edit_file edits UTF-8 text files only".to_string(),
            );
        };
        let max_out = max_file_bytes.min(self.limits.max_stdin_bytes);
        let (edited, replacements) = match apply_edits(content, &edits, max_out) {
            Ok(edited) => edited,
            Err(refusal) => {
                let (kind, message) = refusal.describe(max_out);
                return refused_after(&[&read], kind, message);
            }
        };
        let file_bytes = edited.len();
        let write = match self
            .run_fixed(write_argv(&path), edited.into_bytes(), deadline, &[&read])
            .await
        {
            Ok(execution) => execution,
            Err(run) => return run,
        };
        if !write.succeeded() {
            let mut run = execution_error_with_stderr(&write, "write_failed");
            with_executions(&mut run, &[&read]);
            return run;
        }
        let mut run = ToolRun::from_execution(
            &write,
            json!({"ok": true, "replacements": replacements, "file_bytes": file_bytes}),
            true,
        );
        with_executions(&mut run, &[&read]);
        run
    }

    /// List the regular files under the plan's root, then `grep` them in
    /// argv chunks in sorted order, keeping at most `max_results` matches.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn search(&self, plan: SearchPlan, deadline: Instant) -> ToolRun {
        let find = match self
            .run_fixed(find_argv(&plan.root), Vec::new(), deadline, &[])
            .await
        {
            Ok(execution) => execution,
            Err(run) => return run,
        };
        if !find.succeeded() {
            return execution_error_with_stderr(&find, "search_failed");
        }
        let too_many =
            |find: &Execution| refused_after(&[find], "too_many_files", TOO_MANY_FILES.to_string());
        if !complete(&find.stdout) || find.stdout.data.len() > self.limits.max_manifest_bytes {
            return too_many(&find);
        }
        let mut files = BTreeSet::new();
        let mut skipped_files = 0u64;
        for raw in find.stdout.data.split(|byte| *byte == 0) {
            if raw.is_empty() {
                continue;
            }
            if !raw.starts_with(b"/workspace/") {
                return refused_after(
                    &[&find],
                    "search_failed",
                    "file list escaped /workspace".to_string(),
                );
            }
            match std::str::from_utf8(raw) {
                Ok(path) if searchable(path) => {
                    files.insert(path.to_string());
                }
                _ => skipped_files += 1,
            }
            if files.len() > self.limits.max_manifest_entries {
                return too_many(&find);
            }
        }
        let prefix = grep_prefix(&plan);
        let (chunks, unfit) = grep_chunks(&files, &prefix, self.limits.max_argv_bytes);
        skipped_files += unfit;
        let mut executions = vec![find];
        let mut found = Found::default();
        let mut errors: Option<String> = None;
        let mut stopped_early = false;
        for (index, chunk) in chunks.iter().enumerate() {
            if found.lines.len() >= plan.max_results as usize {
                stopped_early = true;
                break;
            }
            let mut argv = prefix.clone();
            argv.extend(chunk.iter().map(|file| (*file).clone()));
            let prior = executions.iter().collect::<Vec<_>>();
            let grep = match self.run_fixed(argv, Vec::new(), deadline, &prior).await {
                Ok(execution) => execution,
                Err(run) => return run,
            };
            let failed = |grep: &Execution| {
                let mut run = execution_error_with_stderr(grep, "search_failed");
                with_executions(&mut run, &prior);
                run
            };
            if grep.state != ExecutionState::Exited {
                return failed(&grep);
            }
            match grep.exit_code {
                Some(0 | 1) => {}
                Some(2) if !grep.stdout.data.is_empty() => {
                    errors.get_or_insert_with(|| {
                        bounded_text(&grep.stderr.data, ERROR_STDERR_BYTES).text
                    });
                }
                _ => return failed(&grep),
            }
            let chunk_files = chunk.iter().map(|file| file.as_str()).collect();
            let whole = complete(&grep.stdout);
            found.parse(&grep.stdout.data, whole, &chunk_files, plan.max_results);
            executions.push(grep);
            if !whole {
                stopped_early = index + 1 < chunks.len();
                break;
            }
        }
        // The matches share the result with everything else in it.
        let budget = self
            .limits
            .max_result_bytes
            .saturating_sub(RESULT_RESERVE_BYTES)
            .saturating_sub(errors.as_deref().map_or(0, encoded_len));
        let mut matches = String::new();
        let mut used = 2;
        let mut returned = 0u32;
        for line in &found.lines {
            let cost = encoded_len(line) - 2;
            if used + cost > budget {
                break;
            }
            used += cost;
            matches.push_str(line);
            returned += 1;
        }
        let truncated = found.truncated || stopped_early || found.seen > u64::from(returned);
        let mut result = json!({
            "ok": true,
            "matches": matches,
            "returned": returned,
            "seen": found.seen,
            "truncated": truncated,
            "skipped_files": skipped_files,
            "unparsed": found.unparsed,
            "partial": errors.is_some()
        });
        if let (Some(errors), Value::Object(result)) = (errors, &mut result) {
            result.insert("errors".to_string(), Value::String(errors));
        }
        let mut run = ToolRun {
            ok: true,
            result,
            execution_ids: Vec::new(),
            stdout: super::empty_evidence(),
            stderr: super::empty_evidence(),
            stop: None,
            live: None,
            earlier: None,
        };
        with_executions(&mut run, &executions.iter().collect::<Vec<_>>());
        run.stdout.truncated |= truncated;
        run
    }
}

const TOO_MANY_FILES: &str = "too many files to search; narrow path";

/// The ranged read's size: `-L`, so through a symlink it is the target's,
/// the file `head` reads. The base `read_file` keeps its argv without it.
fn stat_argv(path: &str) -> Vec<String> {
    vec![
        "/bin/stat".to_string(),
        "-L".to_string(),
        "-c".to_string(),
        "%s".to_string(),
        "--".to_string(),
        path.to_string(),
    ]
}

fn head_argv(bytes: usize, path: String) -> Vec<String> {
    vec![
        "/usr/bin/head".to_string(),
        "-c".to_string(),
        bytes.to_string(),
        "--".to_string(),
        path,
    ]
}

/// The workspace manifest's argv, from `root` instead of `/workspace`.
fn find_argv(root: &str) -> Vec<String> {
    debug_assert!(root == WORKSPACE || root.starts_with("/workspace/"));
    vec![
        "/usr/bin/find".to_string(),
        root.to_string(),
        "-xdev".to_string(),
        "-type".to_string(),
        "f".to_string(),
        "-print0".to_string(),
    ]
}

/// `grep`'s argv before the file names: the pattern is the one argv entry
/// after `-e`, so one starting with `-` is still a pattern, and `--` ends
/// the options before the names.
fn grep_prefix(plan: &SearchPlan) -> Vec<String> {
    let mut argv = vec![
        "/bin/grep".to_string(),
        "-n".to_string(),
        "-H".to_string(),
        if plan.regex { "-E" } else { "-F" }.to_string(),
    ];
    if plan.ignore_case {
        argv.push("-i".to_string());
    }
    argv.extend(["-e".to_string(), plan.pattern.clone(), "--".to_string()]);
    argv
}

/// Split `files`, in order, into argv chunks: each holds at most
/// [`GREP_CHUNK_FILES`] names and keeps the whole argv within `max_argv_bytes`.
/// Returns the chunks and how many names fit no chunk even alone.
fn grep_chunks<'f>(
    files: &'f BTreeSet<String>,
    prefix: &[String],
    max_argv_bytes: usize,
) -> (Vec<Vec<&'f String>>, u64) {
    let prefix_bytes = argv_bytes(prefix.iter());
    let mut chunks = Vec::new();
    let mut chunk: Vec<&String> = Vec::new();
    let mut chunk_bytes = prefix_bytes;
    let mut unfit = 0u64;
    for file in files {
        let cost = file.len() + 1;
        if prefix_bytes + cost > max_argv_bytes {
            unfit += 1;
            continue;
        }
        if chunk.len() == GREP_CHUNK_FILES || chunk_bytes + cost > max_argv_bytes {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = prefix_bytes;
        }
        chunk.push(file);
        chunk_bytes += cost;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    (chunks, unfit)
}

fn argv_bytes<'a>(argv: impl Iterator<Item = &'a String>) -> usize {
    argv.map(|arg| arg.len() + 1).sum()
}

/// A file `search` can name in its results: plain UTF-8 with no control
/// character, and no `:`, which separates the fields of grep's output.
fn searchable(path: &str) -> bool {
    !path.contains(':') && !path.chars().any(char::is_control)
}

/// The matches parsed from `grep` output so far.
#[derive(Debug, Default)]
struct Found {
    /// `path:line:text\n`, the path relative to `/workspace`, at most the
    /// call's `max_results` of them.
    lines: Vec<String>,
    /// Every parsed line observed: a lower bound once output was cut.
    seen: u64,
    /// Lines that did not parse as a match in a file of the chunk.
    unparsed: u64,
    /// Output was cut, or a match's text was.
    truncated: bool,
}

impl Found {
    /// Parse one chunk's stdout. Output kilnd did not keep whole loses its
    /// last, possibly partial, line.
    fn parse(&mut self, data: &[u8], whole: bool, chunk: &BTreeSet<&str>, keep: u32) {
        let usable = if whole {
            data
        } else {
            self.truncated = true;
            data.iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(&data[..0], |end| &data[..=end])
        };
        for line in usable.split_inclusive(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            match grep_line(line, chunk) {
                Some((text, cut)) => {
                    self.seen += 1;
                    if self.lines.len() < keep as usize {
                        self.truncated |= cut;
                        self.lines.push(text);
                    }
                }
                None => self.unparsed += 1,
            }
        }
    }
}

/// One line of `grep -n -H` output as `search` returns it, and whether its
/// text was cut: the path, up to the first `:` (names with one are never
/// searched), must be a file of this chunk; then the line number, a `:`,
/// and the text. One trailing `\r` is dropped, invalid UTF-8 and control
/// characters but tab become U+FFFD, and text over [`SEARCH_TEXT_BYTES`]
/// is cut at a character boundary and marked with `…`.
fn grep_line(line: &[u8], chunk: &BTreeSet<&str>) -> Option<(String, bool)> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    let path = std::str::from_utf8(&line[..colon]).ok()?;
    if !chunk.contains(path) {
        return None;
    }
    let relative = path.strip_prefix("/workspace/")?;
    let rest = &line[colon + 1..];
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 || rest.get(digits) != Some(&b':') {
        return None;
    }
    let number = std::str::from_utf8(&rest[..digits]).ok()?;
    let raw = &rest[digits + 1..];
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let mut text: String = String::from_utf8_lossy(raw)
        .chars()
        .map(|character| {
            if character.is_control() && character != '\t' {
                '\u{FFFD}'
            } else {
                character
            }
        })
        .collect();
    let cut = text.len() > SEARCH_TEXT_BYTES;
    if cut {
        let mut end = SEARCH_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    Some((format!("{relative}:{number}:{text}\n"), cut))
}

/// The bytes `serde_json` encodes `character` to inside a string: two for
/// a quote, a backslash and the short escapes, six for any other control
/// character below U+0020, and its UTF-8 length otherwise.
fn json_char_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        character if u32::from(character) < 0x20 => 6,
        character => character.len_utf8(),
    }
}

/// `serde_json::to_string(text).len()`, without encoding it.
pub(crate) fn encoded_len(text: &str) -> usize {
    2 + text.chars().map(json_char_len).sum::<usize>()
}

/// What a ranged read returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineSlice {
    pub(crate) content: String,
    /// Whether the returned bytes are valid UTF-8 (`content` decodes them
    /// lossily).
    pub(crate) utf8: bool,
    pub(crate) returned: u32,
    /// The first line not returned, when one exists or may exist.
    pub(crate) next_line: Option<u64>,
    /// The only line returned was cut to fit the budget.
    pub(crate) line_truncated: bool,
}

impl LineSlice {
    fn empty() -> Self {
        Self {
            content: String::new(),
            utf8: true,
            returned: 0,
            next_line: None,
            line_truncated: false,
        }
    }
}

/// Whole lines `start ..= start + count - 1` of the scanned `data`, as far
/// as they are known and fit `budget` JSON-encoded bytes (each line counted
/// as its own encoded string). With `eof_known`, `data` is the whole file
/// and a last line without `\n` counts; otherwise only lines that end in
/// `\n` are known. `None` when `start` lies past the known lines of a file
/// that may go on: the caller refuses the range.
pub(crate) fn slice_lines(
    data: &[u8],
    eof_known: bool,
    start: u32,
    count: u32,
    budget: usize,
) -> Option<LineSlice> {
    let known = if eof_known {
        data
    } else {
        data.iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(&data[..0], |end| &data[..=end])
    };
    let mut lines = known.split_inclusive(|byte| *byte == b'\n').peekable();
    for _ in 1..start {
        if lines.next().is_none() {
            return eof_known.then(LineSlice::empty);
        }
    }
    if lines.peek().is_none() {
        return eof_known.then(LineSlice::empty);
    }
    let mut content = String::new();
    let mut utf8 = true;
    let mut used = 0usize;
    let mut returned = 0u32;
    while returned < count {
        let Some(line) = lines.peek() else {
            break;
        };
        let text = String::from_utf8_lossy(line);
        let cost = encoded_len(&text);
        if used + cost > budget {
            break;
        }
        used += cost;
        utf8 &= std::str::from_utf8(line).is_ok();
        content.push_str(&text);
        returned += 1;
        lines.next();
    }
    let line_truncated = returned == 0;
    if line_truncated {
        // The first line alone is over the budget: return what fits of it.
        let line = lines.next().unwrap_or_default();
        (content, utf8) = cut_line(line, budget);
        returned = 1;
    }
    let next = u64::from(start) + u64::from(returned);
    Some(LineSlice {
        content,
        utf8,
        returned,
        next_line: (lines.peek().is_some() || !eof_known).then_some(next),
        line_truncated,
    })
}

/// The start of `line` that fits `budget` encoded bytes, and whether the
/// line is valid UTF-8: cut at a character boundary when it is, otherwise
/// cut raw and decoded lossily, then shortened by whole characters until it
/// fits.
fn cut_line(line: &[u8], budget: usize) -> (String, bool) {
    let (text, utf8) = match std::str::from_utf8(line) {
        Ok(text) => {
            let mut end = budget.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            (text[..end].to_string(), true)
        }
        Err(_) => (
            String::from_utf8_lossy(&line[..budget.min(line.len())]).into_owned(),
            false,
        ),
    };
    let mut text = text;
    let mut length = encoded_len(&text);
    while length > budget {
        let Some(character) = text.pop() else {
            break;
        };
        length -= json_char_len(character);
    }
    (text, utf8)
}

/// Why `apply_edits` refused, with 0-based edit indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditRefusal {
    NoMatch(usize),
    /// The edit and how many occurrences were counted, at most 2.
    NotUnique(usize, usize),
    /// Two edits' matches overlap; the lower index first.
    Overlap(usize, usize),
    TooManyReplacements,
    TooLarge,
}

impl EditRefusal {
    /// The result's `error` kind and message, for an edited file limited to
    /// `max_out` bytes.
    pub(crate) fn describe(&self, max_out: usize) -> (&'static str, String) {
        match self {
            Self::NoMatch(index) => ("no_match", format!("edit {index}: old_text was not found")),
            Self::NotUnique(index, _) => (
                "not_unique",
                format!(
                    "edit {index}: old_text occurs more than once; include more surrounding text \
                     or set replace_all"
                ),
            ),
            Self::Overlap(first, second) => (
                "overlapping_edits",
                format!("edits {first} and {second} overlap"),
            ),
            Self::TooManyReplacements => (
                "too_many_replacements",
                format!("the edits make more than {MAX_REPLACEMENTS} replacements"),
            ),
            Self::TooLarge => (
                "file_too_large",
                format!("the edited file would exceed {} KiB", max_out / 1024),
            ),
        }
    }
}

/// One replacement: `content[start..end]` becomes edit `edit`'s new text.
#[derive(Debug, Clone, Copy)]
struct Interval {
    start: usize,
    end: usize,
    edit: usize,
}

/// Apply `edits` to `content`, all matched against the original, and
/// return the edited text and the number of replacements, or why not.
///
/// Pass 1, per edit in order: a unique edit counts its occurrences at every
/// start position, overlapping ones included, stopping at two; one with
/// `replace_all` takes every non-overlapping one, left to right. Past
/// [`MAX_REPLACEMENTS`] in all is refused. Pass 2 refuses overlapping
/// matches of different edits (touching is allowed). Pass 3 computes the
/// edited size and refuses it over `max_out` before allocating anything.
/// Inserted text is never matched again.
pub(crate) fn apply_edits(
    content: &str,
    edits: &[EditArgs],
    max_out: usize,
) -> Result<(String, usize), EditRefusal> {
    let mut intervals: Vec<Interval> = Vec::new();
    let mut push = |interval: Interval| {
        if intervals.len() >= MAX_REPLACEMENTS {
            return Err(EditRefusal::TooManyReplacements);
        }
        intervals.push(interval);
        Ok(())
    };
    for (index, edit) in edits.iter().enumerate() {
        let old = edit.old_text.as_str();
        let Some(first) = old.chars().next() else {
            return Err(EditRefusal::NoMatch(index));
        };
        if edit.replace_all {
            let mut matched = false;
            for (start, _) in content.match_indices(old) {
                matched = true;
                push(Interval {
                    start,
                    end: start + old.len(),
                    edit: index,
                })?;
            }
            if !matched {
                return Err(EditRefusal::NoMatch(index));
            }
        } else {
            let mut found = None;
            let mut occurrences = 0;
            let mut from = 0;
            while occurrences < 2 {
                let Some(hit) = content[from..].find(old) else {
                    break;
                };
                let at = from + hit;
                found.get_or_insert(at);
                occurrences += 1;
                from = at + first.len_utf8();
            }
            match (occurrences, found) {
                (1, Some(start)) => push(Interval {
                    start,
                    end: start + old.len(),
                    edit: index,
                })?,
                (0, _) => return Err(EditRefusal::NoMatch(index)),
                _ => return Err(EditRefusal::NotUnique(index, occurrences)),
            }
        }
    }
    intervals.sort_by_key(|interval| (interval.start, interval.edit));
    for pair in intervals.windows(2) {
        let (previous, next) = (pair[0], pair[1]);
        if next.start < previous.end {
            return Err(EditRefusal::Overlap(
                previous.edit.min(next.edit),
                previous.edit.max(next.edit),
            ));
        }
    }
    let mut new_len = i128::try_from(content.len()).map_err(|_| EditRefusal::TooLarge)?;
    for interval in &intervals {
        let edit = &edits[interval.edit];
        let change = i128::try_from(edit.new_text.len()).map_err(|_| EditRefusal::TooLarge)?
            - i128::try_from(edit.old_text.len()).map_err(|_| EditRefusal::TooLarge)?;
        new_len = new_len.checked_add(change).ok_or(EditRefusal::TooLarge)?;
    }
    let new_len = usize::try_from(new_len).map_err(|_| EditRefusal::TooLarge)?;
    if new_len > max_out {
        return Err(EditRefusal::TooLarge);
    }
    let mut edited = String::with_capacity(new_len);
    let mut at = 0;
    for interval in &intervals {
        edited.push_str(&content[at..interval.start]);
        edited.push_str(&edits[interval.edit].new_text);
        at = interval.end;
    }
    edited.push_str(&content[at..]);
    Ok((edited, intervals.len()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use super::{
        EditArgs, EditRefusal, LineSlice, apply_edits, encoded_len, grep_line, json_char_len,
        slice_lines,
    };
    use crate::tools::{MAX_REPLACEMENTS, SEARCH_TEXT_BYTES};

    fn edit(old: &str, new: &str) -> EditArgs {
        EditArgs {
            old_text: old.to_string(),
            new_text: new.to_string(),
            replace_all: false,
        }
    }

    fn every(old: &str, new: &str) -> EditArgs {
        EditArgs {
            replace_all: true,
            ..edit(old, new)
        }
    }

    const MAX: usize = 64 << 10;

    #[test]
    fn a_unique_edit_replaces_its_one_occurrence() {
        assert_eq!(
            apply_edits("one\ntwo\nthree\n", &[edit("two", "2")], MAX),
            Ok(("one\n2\nthree\n".to_string(), 1))
        );
    }

    #[test]
    fn missing_and_repeated_text_is_refused() {
        assert_eq!(
            apply_edits("abc", &[edit("x", "y")], MAX),
            Err(EditRefusal::NoMatch(0))
        );
        assert_eq!(
            apply_edits("ab ab", &[edit("ab", "c")], MAX),
            Err(EditRefusal::NotUnique(0, 2))
        );
        // Occurrences overlapping each other count too.
        assert_eq!(
            apply_edits("aaa", &[edit("aa", "b")], MAX),
            Err(EditRefusal::NotUnique(0, 2))
        );
        // Counting stops at two, and is right past a multibyte first char.
        assert_eq!(
            apply_edits("ééé", &[edit("éé", "e")], MAX),
            Err(EditRefusal::NotUnique(0, 2))
        );
        assert_eq!(
            apply_edits("éa éb", &[edit("éb", "x")], MAX),
            Ok(("éa x".to_string(), 1))
        );
    }

    #[test]
    fn replace_all_takes_every_non_overlapping_occurrence() {
        assert_eq!(
            apply_edits("aaaa", &[every("aa", "b")], MAX),
            Ok(("bb".to_string(), 2))
        );
        assert_eq!(
            apply_edits("x y x", &[every("x", "zz")], MAX),
            Ok(("zz y zz".to_string(), 2))
        );
        assert_eq!(
            apply_edits("abc", &[every("x", "y")], MAX),
            Err(EditRefusal::NoMatch(0))
        );
        // Inserted text is never matched again.
        assert_eq!(
            apply_edits("a", &[every("a", "aa")], MAX),
            Ok(("aa".to_string(), 1))
        );
    }

    #[test]
    fn edits_match_the_original_and_may_touch_but_not_overlap() {
        // Edit 1 would match edit 0's output if edits applied in turn.
        assert_eq!(
            apply_edits("ab", &[edit("a", "b"), edit("b", "c")], MAX),
            Ok(("bc".to_string(), 2))
        );
        // Touching intervals: [0, 2) and [2, 4).
        assert_eq!(
            apply_edits("abcd", &[edit("cd", "2"), edit("ab", "1")], MAX),
            Ok(("12".to_string(), 2))
        );
        assert_eq!(
            apply_edits("abcd", &[edit("bc", "x"), edit("abc", "y")], MAX),
            Err(EditRefusal::Overlap(0, 1))
        );
        assert_eq!(
            apply_edits(
                "abcd",
                &[edit("cd", "x"), edit("q", "r"), edit("bc", "y")],
                MAX
            ),
            Err(EditRefusal::NoMatch(1))
        );
        assert_eq!(
            apply_edits(
                "abcd",
                &[edit("cd", "x"), edit("d", "q"), edit("ab", "y")],
                MAX
            ),
            Err(EditRefusal::Overlap(0, 1))
        );
        // The same start, from two edits.
        assert_eq!(
            apply_edits("abc", &[edit("ab", "x"), every("a", "y")], MAX),
            Err(EditRefusal::Overlap(0, 1))
        );
    }

    #[test]
    fn refusals_follow_the_pass_order() {
        // A later edit's match error wins over an earlier overlap.
        assert_eq!(
            apply_edits(
                "abc",
                &[edit("ab", "x"), edit("bc", "y"), edit("q", "r")],
                MAX
            ),
            Err(EditRefusal::NoMatch(2))
        );
        // The first failing edit, in edit order.
        assert_eq!(
            apply_edits("aa b", &[edit("a", "x"), edit("q", "r")], MAX),
            Err(EditRefusal::NotUnique(0, 2))
        );
        // Overlap before size.
        assert_eq!(
            apply_edits("abc", &[edit("ab", &"x".repeat(MAX)), edit("bc", "y")], MAX),
            Err(EditRefusal::Overlap(0, 1))
        );
        assert_eq!(
            apply_edits("abc", &[edit("ab", &"x".repeat(MAX))], MAX),
            Err(EditRefusal::TooLarge)
        );
        // Exactly at the limit is fine.
        assert_eq!(
            apply_edits("abc", &[edit("ab", &"x".repeat(MAX - 1))], MAX)
                .map(|(text, count)| (text.len(), count)),
            Ok((MAX, 1))
        );
    }

    #[test]
    fn a_huge_expansion_is_refused_before_it_is_built() {
        // 10,000 occurrences in about 60 KiB, each to become 60,000 bytes:
        // some 600 MB, refused from the computed size alone.
        let content = "x.....".repeat(MAX_REPLACEMENTS);
        assert!(content.len() <= MAX);
        let started = Instant::now();
        assert_eq!(
            apply_edits(&content, &[every("x", &"y".repeat(60_000))], MAX),
            Err(EditRefusal::TooLarge)
        );
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn dense_replace_all_is_bounded() {
        let content = "a".repeat(MAX);
        assert_eq!(
            apply_edits(&content, &[every("a", "b")], MAX),
            Err(EditRefusal::TooManyReplacements)
        );
        // Exactly the limit is allowed.
        let content = "a".repeat(MAX_REPLACEMENTS);
        assert_eq!(
            apply_edits(&content, &[every("a", "b")], MAX).map(|(_, count)| count),
            Ok(MAX_REPLACEMENTS)
        );
        // Over every edit together.
        assert_eq!(
            apply_edits(&content, &[every("a", "b"), edit("q", "r")], MAX),
            Err(EditRefusal::NoMatch(1))
        );
        let content = format!("{}q", "a".repeat(MAX_REPLACEMENTS));
        assert_eq!(
            apply_edits(&content, &[every("a", "b"), edit("q", "r")], MAX),
            Err(EditRefusal::TooManyReplacements)
        );
    }

    #[test]
    fn refusal_messages_name_the_edits() {
        let messages = [
            (
                EditRefusal::NoMatch(2),
                "no_match",
                "edit 2: old_text was not found",
            ),
            (
                EditRefusal::NotUnique(0, 2),
                "not_unique",
                "edit 0: old_text occurs more than once; include more surrounding text or set \
                 replace_all",
            ),
            (
                EditRefusal::Overlap(1, 3),
                "overlapping_edits",
                "edits 1 and 3 overlap",
            ),
            (
                EditRefusal::TooManyReplacements,
                "too_many_replacements",
                "the edits make more than 10000 replacements",
            ),
            (
                EditRefusal::TooLarge,
                "file_too_large",
                "the edited file would exceed 64 KiB",
            ),
        ];
        for (refusal, kind, message) in messages {
            assert_eq!(refusal.describe(MAX), (kind, message.to_string()));
        }
    }

    #[test]
    fn encoded_length_is_what_serde_json_writes() {
        let mut text: String = (0u8..0x80).map(char::from).collect();
        text.push_str("é€😀\u{2028}\u{7f}\u{FFFD}");
        for character in text.chars() {
            let encoded = serde_json::to_string(&character.to_string()).expect("encode");
            assert_eq!(json_char_len(character) + 2, encoded.len(), "{character:?}");
        }
        assert_eq!(
            encoded_len(&text),
            serde_json::to_string(&text).expect("encode").len()
        );
    }

    fn slice(data: &[u8], eof: bool, start: u32, count: u32) -> Option<LineSlice> {
        slice_lines(data, eof, start, count, 1 << 20)
    }

    #[allow(clippy::unnecessary_wraps)] // compared with slice_lines' answer
    fn lines(content: &str, returned: u32, next_line: Option<u64>) -> Option<LineSlice> {
        Some(LineSlice {
            content: content.to_string(),
            utf8: true,
            returned,
            next_line,
            line_truncated: false,
        })
    }

    #[test]
    fn ranges_take_whole_lines_from_start() {
        let data = b"one\ntwo\nthree\n";
        assert_eq!(slice(data, true, 1, 1), lines("one\n", 1, Some(2)));
        assert_eq!(slice(data, true, 2, 1), lines("two\n", 1, Some(3)));
        // Exactly the rest: nothing follows.
        assert_eq!(slice(data, true, 2, 2), lines("two\nthree\n", 2, None));
        assert_eq!(
            slice(data, true, 1, 5000),
            lines("one\ntwo\nthree\n", 3, None)
        );
        // Past the end of a whole file: no lines, nothing to continue.
        assert_eq!(slice(data, true, 4, 1), lines("", 0, None));
        assert_eq!(slice(data, true, 9_000, 1), lines("", 0, None));
        assert_eq!(slice(b"", true, 1, 1), lines("", 0, None));
    }

    #[test]
    fn a_scan_that_may_go_on_knows_only_whole_lines() {
        // Ending mid-line: the partial line is not known.
        let data = b"one\ntwo\nthr";
        assert_eq!(slice(data, false, 1, 9), lines("one\ntwo\n", 2, Some(3)));
        assert_eq!(slice(data, false, 3, 1), None);
        // Ending exactly at a newline: every line is known, more may follow.
        let data = b"one\ntwo\n";
        assert_eq!(slice(data, false, 2, 1), lines("two\n", 1, Some(3)));
        assert_eq!(slice(data, false, 3, 1), None);
        // No newline at all.
        assert_eq!(slice(b"abc", false, 1, 1), None);
    }

    #[test]
    fn a_whole_file_keeps_its_last_line_without_a_newline() {
        let data = b"one\ntwo";
        assert_eq!(slice(data, true, 2, 1), lines("two", 1, None));
        assert_eq!(slice(data, true, 1, 2), lines("one\ntwo", 2, None));
    }

    #[test]
    fn line_bytes_are_kept_exactly() {
        let data = b"a\r\nb\0c\r\n\r\n";
        assert_eq!(slice(data, true, 1, 3), lines("a\r\nb\0c\r\n\r\n", 3, None));
        let invalid = b"ok\n\xff\xfe\n";
        assert_eq!(
            slice(invalid, true, 1, 2),
            Some(LineSlice {
                content: "ok\n\u{FFFD}\u{FFFD}\n".to_string(),
                utf8: false,
                returned: 2,
                next_line: None,
                line_truncated: false,
            })
        );
    }

    #[test]
    fn the_budget_counts_encoded_bytes() {
        // Each line costs its encoded length, quotes included: "a\"\n" is
        // `"a\"\n"`, 7 bytes.
        let data = b"a\"\nb\\\n\x01\n";
        assert_eq!(slice_lines(data, true, 1, 3, 7), lines("a\"\n", 1, Some(2)));
        assert_eq!(
            slice_lines(data, true, 1, 3, 14),
            lines("a\"\nb\\\n", 2, Some(3))
        );
        // "\u0001\n" is 10 bytes.
        assert_eq!(
            slice_lines(data, true, 1, 3, 23),
            lines("a\"\nb\\\n", 2, Some(3))
        );
        assert_eq!(
            slice_lines(data, true, 1, 3, 24),
            lines("a\"\nb\\\n\x01\n", 3, None)
        );
    }

    #[test]
    fn an_oversized_first_line_is_cut_to_fit() {
        // Valid UTF-8 is cut at a character boundary: "aé" + "é" is 5
        // bytes, and with quotes 7 fits 7, 8 would not.
        let data = "aééé\nnext\n".as_bytes();
        let cut = slice_lines(data, true, 1, 2, 7).expect("a slice");
        assert_eq!(
            cut,
            LineSlice {
                content: "aéé".to_string(),
                utf8: true,
                returned: 1,
                next_line: Some(2),
                line_truncated: true,
            }
        );
        let cut = slice_lines(data, true, 1, 2, 6).expect("a slice");
        assert_eq!(cut.content, "aé");
        // Characters that encode longer are trimmed until the result fits.
        let quotes = slice_lines(b"\"\"\"\"\"\"\n", true, 1, 1, 8).expect("a slice");
        assert_eq!(quotes.content, "\"\"\"");
        assert_eq!(encoded_len(&quotes.content), 8);
        // Invalid UTF-8 is cut raw (9 bytes), decoded (U+FFFD is 3), then
        // trimmed until it fits: `"ab\u{FFFD}cd"` is 9 bytes.
        let invalid = slice_lines(b"ab\xffcdefgh", true, 1, 1, 9).expect("a slice");
        assert_eq!(invalid.content, "ab\u{FFFD}cd");
        assert!(!invalid.utf8);
        assert!(invalid.line_truncated);
        // The only line of a whole file: nothing follows.
        assert_eq!(invalid.next_line, None);
        // A scan that may go on always may continue.
        let open = slice_lines(b"abcdefgh\n", false, 1, 1, 4).expect("a slice");
        assert_eq!((open.content.as_str(), open.next_line), ("ab", Some(2)));
    }

    fn chunk() -> BTreeSet<&'static str> {
        BTreeSet::from(["/workspace/a.txt", "/workspace/lib/b.sh"])
    }

    #[test]
    fn grep_lines_keep_text_after_the_first_two_colons() {
        assert_eq!(
            grep_line(b"/workspace/a.txt:12:key: value:x", &chunk()),
            Some(("a.txt:12:key: value:x\n".to_string(), false))
        );
        assert_eq!(
            grep_line(b"/workspace/lib/b.sh:3:\tx=1\r", &chunk()),
            Some(("lib/b.sh:3:\tx=1\n".to_string(), false))
        );
        // Only one trailing CR goes.
        assert_eq!(
            grep_line(b"/workspace/a.txt:1:x\r\r", &chunk()),
            Some(("a.txt:1:x\u{FFFD}\n".to_string(), false))
        );
    }

    #[test]
    fn grep_lines_from_elsewhere_do_not_parse() {
        for line in [
            &b"/workspace/other.txt:1:x"[..],
            b"/workspace/a.txt",
            b"/workspace/a.txt:x:y",
            b"/workspace/a.txt::y",
            b"/workspace/a.txt:12",
            b"Binary file /workspace/a.txt matches",
            b"a.txt:1:x",
            b"/workspace/a.txt\xff:1:x",
            b"",
        ] {
            assert_eq!(grep_line(line, &chunk()), None, "{line:?}");
        }
    }

    #[test]
    fn grep_text_is_cleaned_and_bounded() {
        assert_eq!(
            grep_line(b"/workspace/a.txt:1:a\x1b[31mb\x7f\xffc", &chunk()),
            Some((
                "a.txt:1:a\u{FFFD}[31mb\u{FFFD}\u{FFFD}c\n".to_string(),
                false
            ))
        );
        // 150 two-byte characters fit 300 bytes; one more is cut.
        let fits = "é".repeat(SEARCH_TEXT_BYTES / 2);
        let line = format!("/workspace/a.txt:1:{fits}");
        assert_eq!(
            grep_line(line.as_bytes(), &chunk()),
            Some((format!("a.txt:1:{fits}\n"), false))
        );
        let long = format!("x{fits}");
        let line = format!("/workspace/a.txt:1:{long}");
        let (text, cut) = grep_line(line.as_bytes(), &chunk()).expect("parses");
        assert!(cut);
        assert_eq!(
            text,
            format!("a.txt:1:x{}…\n", "é".repeat(SEARCH_TEXT_BYTES / 2 - 1))
        );
    }
}
