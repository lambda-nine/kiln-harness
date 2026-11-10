use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kiln_harness::api::{SandboxApi, TonicKilnd};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, LoadedJournal, read_journal};
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
    let api = connect(socket).await?;
    let (audit, journal) = open_sinks(audit_path, journal_path)?;
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
    let recorded =
        read_journal(from).with_context(|| format!("read journal {}", from.display()))?;
    let provider = ReplayProvider::from_journal(&recorded)
        .with_context(|| format!("load recorded turns from {}", from.display()))?;
    let Some(expected) = provider.recorded_final_answer().map(str::to_string) else {
        bail!(
            "{} has no recorded final answer to reproduce (outcome: {})",
            from.display(),
            provider.recorded_outcome().unwrap_or("incomplete")
        );
    };
    let task = provider.task();
    let mut config = HarnessConfig::default();
    config.apply_limits(&provider.limits());

    let api = connect(socket).await?;
    let (audit, journal) = open_sinks(audit_path, journal_path)?;
    let mut harness = Harness::new(api.clone(), provider, audit, journal, config);
    let result = harness.run(&task, interrupt()).await;
    check_health(&api).await?;
    println!("journal: {}", journal_path.display());
    match result {
        Ok(outcome) if outcome.final_answer == expected => {
            print_outcome(&outcome);
            println!("reproduced");
            Ok(ExitCode::SUCCESS)
        }
        Ok(outcome) => {
            print_outcome(&outcome);
            println!("diverged: final answer differs from the recording");
            Ok(ExitCode::FAILURE)
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

fn open_sinks(audit_path: &Path, journal_path: &Path) -> Result<(JsonlAudit, JsonlJournal)> {
    let audit = JsonlAudit::open(audit_path)
        .with_context(|| format!("open audit log {}", audit_path.display()))?;
    let journal = JsonlJournal::open(journal_path)
        .with_context(|| format!("create journal {}", journal_path.display()))?;
    Ok((audit, journal))
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
