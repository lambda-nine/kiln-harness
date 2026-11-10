use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kiln_harness::api::{SandboxApi, TonicKilnd};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask};
use kiln_harness::journal::{
    JournalEvent, JournalLimits, JsonlJournal, LoadedJournal, open_journal_source, read_journal,
    read_open_journal,
};
use kiln_harness::provider::{ScriptedProvider, Usage};
use kiln_harness::replay::ReplayProvider;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Provider-neutral AI harness over kilnd.v1")]
struct Cli {
    /// kilnd's Unix socket (unused by `journal verify`).
    #[arg(long, default_value = "/run/kilnd/kilnd.sock", global = true)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the deterministic no-model-API vertical slice.
    Demo {
        /// Append-only metadata audit destination.
        #[arg(long, default_value = "audit/demo.jsonl")]
        audit: PathBuf,
        /// New session journal; defaults to `journal/demo-<unix_ms>.jsonl`.
        #[arg(long)]
        journal: Option<PathBuf>,
    },
    /// Replay a recorded session against kilnd and compare its final answer.
    Replay {
        /// Journal of the session to replay.
        #[arg(long)]
        from: PathBuf,
        /// Append-only metadata audit destination.
        #[arg(long, default_value = "audit/replay.jsonl")]
        audit: PathBuf,
        /// New journal for the replay; defaults to `journal/replay-<unix_ms>.jsonl`.
        #[arg(long)]
        journal: Option<PathBuf>,
    },
    /// Inspect session journals offline.
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
}

#[derive(Subcommand)]
enum JournalCommand {
    /// Verify a journal's limits, order, and hash chain without contacting kilnd.
    Verify { path: PathBuf },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Demo { audit, journal } => {
            let journal = journal.unwrap_or_else(|| default_journal("demo"));
            demo(cli.socket, &audit, &journal).await
        }
        Command::Replay {
            from,
            audit,
            journal,
        } => {
            let journal = journal.unwrap_or_else(|| default_journal("replay"));
            replay(cli.socket, &from, &audit, &journal).await
        }
        Command::Journal {
            command: JournalCommand::Verify { path },
        } => Ok(verify(&path)),
    }
}

async fn demo(socket: PathBuf, audit_path: &Path, journal_path: &Path) -> Result<ExitCode> {
    refuse_aliases(&[
        ("--audit", FileIdentity::of_path(audit_path)?),
        ("--journal", FileIdentity::of_path(journal_path)?),
    ])?;
    let (audit, journal) = open_sinks(None, audit_path, journal_path)?;
    let api = connect(socket).await?;
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        audit,
        journal,
        HarnessConfig::default(),
    );
    let outcome = harness.run(&SessionTask::demo(), interrupt()).await?;
    check_health(&api).await?;
    print_outcome(&outcome);
    println!("audit: {}", audit_path.display());
    println!("journal: {}", journal_path.display());
    Ok(ExitCode::SUCCESS)
}

async fn replay(
    socket: PathBuf,
    from: &Path,
    audit_path: &Path,
    journal_path: &Path,
) -> Result<ExitCode> {
    // Hold the source open read-only for the whole replay and refuse any
    // output path that names the same file, before kilnd is contacted.
    let source =
        open_journal_source(from).with_context(|| format!("open journal {}", from.display()))?;
    refuse_aliases(&[
        ("--from", FileIdentity::of_file(from, &source)?),
        ("--audit", FileIdentity::of_path(audit_path)?),
        ("--journal", FileIdentity::of_path(journal_path)?),
    ])?;
    let recorded = read_open_journal(&source, JournalLimits::default())
        .with_context(|| format!("read journal {}", from.display()))?;
    let provider = ReplayProvider::from_journal(&recorded)
        .with_context(|| format!("load recorded turns from {}", from.display()))?;
    let recorded_outcome = provider.recorded_outcome().unwrap_or("incomplete");
    let (Some(expected), "completed") = (
        provider.recorded_final_answer().map(str::to_string),
        recorded_outcome,
    ) else {
        bail!(
            "{} did not record a completed session to reproduce (outcome: {recorded_outcome})",
            from.display(),
        );
    };
    let task = provider.task();
    let mut config = HarnessConfig::default();
    config.apply_limits(&provider.limits());

    let (audit, journal) = open_sinks(Some(&source), audit_path, journal_path)?;
    let api = connect(socket).await?;
    let mut harness = Harness::new(api.clone(), provider, audit, journal, config);
    let result = harness.run(&task, interrupt()).await;
    check_health(&api).await?;
    println!("journal: {}", journal_path.display());
    match result {
        Ok(outcome) => {
            print_outcome(&outcome);
            let replay = harness.provider();
            // Reproduction needs the same answer, no divergence, and every
            // recorded turn consumed.
            if outcome.final_answer != expected {
                println!("diverged: final answer differs from the recording");
                Ok(ExitCode::FAILURE)
            } else if let Some(turn) = replay.diverged_at() {
                println!("diverged at turn {turn}");
                Ok(ExitCode::FAILURE)
            } else if !replay.exhausted() {
                println!("diverged: recorded turns were left unreplayed");
                Ok(ExitCode::FAILURE)
            } else {
                println!("reproduced");
                Ok(ExitCode::SUCCESS)
            }
        }
        Err(error) => {
            match harness.provider().diverged_at() {
                Some(turn) => println!("diverged at turn {turn}"),
                None => println!("replay failed: {error}"),
            }
            if matches!(
                error,
                HarnessError::Cleanup(_) | HarnessError::CreateUncertain(_)
            ) {
                return Err(error.into());
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// What a command-line path refers to: the file's device and inode when it
/// exists, and its canonical location either way.
struct FileIdentity {
    inode: Option<(u64, u64)>,
    canonical: PathBuf,
}

impl FileIdentity {
    fn of_file(path: &Path, file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            inode: Some((metadata.dev(), metadata.ino())),
            canonical: std::fs::canonicalize(path)?,
        })
    }

    fn of_path(path: &Path) -> Result<Self> {
        if let Ok(metadata) = std::fs::metadata(path) {
            return Ok(Self {
                inode: Some((metadata.dev(), metadata.ino())),
                canonical: std::fs::canonicalize(path)?,
            });
        }
        Ok(Self {
            inode: None,
            canonical: canonical_new_path(path)?,
        })
    }

    fn same_file(&self, other: &Self) -> bool {
        matches!((self.inode, other.inode), (Some(left), Some(right)) if left == right)
            || self.canonical == other.canonical
    }
}

/// Canonicalize the deepest existing ancestor of a path that does not exist
/// yet, then apply the remaining components. Those name nothing on disk yet,
/// so `.` and `..` among them resolve lexically.
fn canonical_new_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut existing = absolute.clone();
    let mut rest = Vec::new();
    let mut resolved = loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            break canonical;
        }
        let Some(last) = existing.components().next_back() else {
            bail!("cannot resolve {}", path.display());
        };
        rest.push(last.as_os_str().to_os_string());
        if !existing.pop() {
            bail!("cannot resolve {}", path.display());
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

fn refuse_aliases(paths: &[(&str, FileIdentity)]) -> Result<()> {
    for (index, (left_flag, left)) in paths.iter().enumerate() {
        for (right_flag, right) in &paths[index + 1..] {
            if left.same_file(right) {
                bail!("{left_flag} and {right_flag} refer to the same file");
            }
        }
    }
    Ok(())
}

fn verify(path: &Path) -> ExitCode {
    let journal = match read_journal(path) {
        Ok(journal) => journal,
        Err(error) => {
            eprintln!("invalid: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = ReplayProvider::from_journal(&journal) {
        eprintln!("invalid: {error}");
        return ExitCode::FAILURE;
    }
    let summary = summarize(&journal);
    println!("session: {}", journal.session_id());
    println!("records: {}", journal.records.len());
    match &summary.outcome {
        Some(outcome) => println!("status: complete ({outcome})"),
        None => println!("status: incomplete"),
    }
    // Deletion is confirmed only by a complete journal that says so.
    println!(
        "sandbox deleted: {}",
        if summary.sandbox_deleted { "yes" } else { "no" }
    );
    println!("turns: {}", summary.turns);
    println!("tool calls: {}", summary.tool_calls);
    println!(
        "usage: input {}, output {}, cache read {}, cache creation {}",
        summary.usage.input_tokens,
        summary.usage.output_tokens,
        summary.usage.cache_read_input_tokens,
        summary.usage.cache_creation_input_tokens
    );
    ExitCode::SUCCESS
}

struct Summary {
    outcome: Option<String>,
    sandbox_deleted: bool,
    turns: u32,
    tool_calls: u32,
    usage: Usage,
}

fn summarize(journal: &LoadedJournal) -> Summary {
    let mut summary = Summary {
        outcome: None,
        sandbox_deleted: false,
        turns: 0,
        tool_calls: 0,
        usage: Usage::default(),
    };
    for record in &journal.records {
        match &record.event {
            JournalEvent::ModelRequested { .. } => summary.turns += 1,
            JournalEvent::ModelResponded { response, .. } => {
                summary.usage = summary.usage + response.usage;
            }
            JournalEvent::ToolExecuted { .. } => summary.tool_calls += 1,
            JournalEvent::SessionEnded {
                outcome,
                sandbox_deleted,
                ..
            } => {
                summary.outcome = Some(outcome.clone());
                summary.sandbox_deleted = *sandbox_deleted;
            }
            _ => {}
        }
    }
    summary
}

async fn connect(socket: PathBuf) -> Result<TonicKilnd> {
    let api = TonicKilnd::connect(socket).await?;
    let health = api.health().await?;
    if !health.serving {
        bail!("kilnd is not serving: {}", health.detail);
    }
    Ok(api)
}

async fn check_health(api: &TonicKilnd) -> Result<()> {
    let health = api.health().await?;
    if !health.serving {
        bail!(
            "kilnd became unhealthy after the session: {}",
            health.detail
        );
    }
    Ok(())
}

/// Open both outputs without writing, then refuse if any two of the source
/// and outputs are the same file by `fstat` on their open descriptors. Path
/// checks earlier are only diagnostics: a path can be relinked after them,
/// but an open descriptor cannot change which file it names.
fn open_sinks(
    source: Option<&File>,
    audit_path: &Path,
    journal_path: &Path,
) -> Result<(JsonlAudit, JsonlJournal)> {
    let audit = JsonlAudit::open(audit_path)
        .with_context(|| format!("open audit log {}", audit_path.display()))?;
    let journal = match JsonlJournal::open(journal_path) {
        Ok(journal) => journal,
        Err(error) => {
            if audit.created() {
                discard_if_new_and_empty(audit_path, audit.file_identity().ok());
            }
            return Err(anyhow::Error::new(error)
                .context(format!("create journal {}", journal_path.display())));
        }
    };
    let audit_identity = audit.file_identity()?;
    let journal_identity = journal.file_identity()?;
    let source_identity = source
        .map(|file| file.metadata().map(|meta| (meta.dev(), meta.ino())))
        .transpose()?;
    let clash = if source_identity == Some(audit_identity) {
        Some("--from and --audit")
    } else if source_identity == Some(journal_identity) {
        Some("--from and --journal")
    } else if audit_identity == journal_identity {
        Some("--audit and --journal")
    } else {
        None
    };
    if let Some(flags) = clash {
        if audit.created() {
            discard_if_new_and_empty(audit_path, Some(audit_identity));
        }
        discard_if_new_and_empty(journal_path, Some(journal_identity));
        bail!("{flags} refer to the same file");
    }
    Ok((audit, journal))
}

/// Remove a file this invocation created, but only while the path still
/// names that same, still empty, file.
fn discard_if_new_and_empty(path: &Path, identity: Option<(u64, u64)>) {
    let Some(identity) = identity else {
        return;
    };
    if std::fs::symlink_metadata(path)
        .is_ok_and(|meta| (meta.dev(), meta.ino()) == identity && meta.len() == 0)
    {
        let _ = std::fs::remove_file(path);
    }
}

fn interrupt() -> CancellationToken {
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    cancel
}

fn default_journal(kind: &str) -> PathBuf {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    PathBuf::from(format!("journal/{kind}-{unix_ms}.jsonl"))
}

fn print_outcome(outcome: &SessionOutcome) {
    println!("{}", outcome.final_answer);
    println!("session: {}", outcome.session_id);
    println!("sandbox: {} (deleted)", outcome.sandbox_id);
    println!(
        "steps: {}, tool calls: {}",
        outcome.steps, outcome.tool_calls
    );
    println!(
        "usage: input {}, output {}",
        outcome.usage.input_tokens, outcome.usage.output_tokens
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory with mode 0700 whatever the umask.
    fn private_tempdir() -> std::io::Result<tempfile::TempDir> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    #[test]
    fn descriptor_check_refuses_an_audit_that_is_the_source() {
        let temp = private_tempdir().expect("tempdir");
        let source_path = temp.path().join("source.jsonl");
        std::fs::write(&source_path, b"recorded\n").expect("source");
        let source = File::open(&source_path).expect("open source");
        // A hard link made after any path check names the same inode.
        let audit_path = temp.path().join("audit.jsonl");
        std::fs::hard_link(&source_path, &audit_path).expect("hard link");
        let journal_path = temp.path().join("replay.jsonl");

        let error = open_sinks(Some(&source), &audit_path, &journal_path)
            .err()
            .expect("aliased outputs are refused");

        assert!(error.to_string().contains("--from and --audit"), "{error}");
        assert_eq!(std::fs::read(&source_path).expect("read"), b"recorded\n");
        assert!(audit_path.exists(), "a pre-existing file is never removed");
        assert!(!journal_path.exists(), "the new empty journal is removed");
    }

    #[test]
    fn one_new_path_for_both_outputs_is_refused_and_cleaned_up() {
        let temp = private_tempdir().expect("tempdir");
        let path = temp.path().join("both.jsonl");

        let error = open_sinks(None, &path, &path)
            .err()
            .expect("one file cannot be both outputs");

        assert!(error.to_string().contains("create journal"), "{error}");
        assert!(!path.exists(), "the audit this call created is removed");
    }

    #[test]
    fn distinct_outputs_open_without_writing() {
        let temp = private_tempdir().expect("tempdir");
        let source_path = temp.path().join("source.jsonl");
        std::fs::write(&source_path, b"recorded\n").expect("source");
        let source = File::open(&source_path).expect("open source");
        let audit_path = temp.path().join("audit.jsonl");
        let journal_path = temp.path().join("replay.jsonl");

        let (audit, journal) =
            open_sinks(Some(&source), &audit_path, &journal_path).expect("distinct outputs");

        assert!(audit.created());
        assert_ne!(
            audit.file_identity().expect("audit identity"),
            journal.file_identity().expect("journal identity")
        );
        assert_eq!(std::fs::metadata(&audit_path).expect("audit").len(), 0);
        assert_eq!(std::fs::metadata(&journal_path).expect("journal").len(), 0);
    }
}
