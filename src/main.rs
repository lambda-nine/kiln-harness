use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kiln_harness::api::{SandboxApi, TonicKilnd};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::engine::{Harness, HarnessConfig};
use kiln_harness::provider::ScriptedProvider;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Provider-neutral AI harness over kilnd.v1")]
struct Cli {
    /// kilnd's Unix socket.
    #[arg(long, default_value = "/run/kilnd/kilnd.sock", global = true)]
    socket: PathBuf,
    /// Append-only JSONL audit destination.
    #[arg(long, default_value = "audit/demo.jsonl", global = true)]
    audit: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the deterministic no-model-API vertical slice.
    Demo,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Demo => demo(cli.socket, cli.audit).await,
    }
}

async fn demo(socket: PathBuf, audit_path: PathBuf) -> Result<()> {
    let api = TonicKilnd::connect(socket).await?;
    let before = api.health().await?;
    if !before.serving {
        bail!("kilnd is not serving: {}", before.detail);
    }
    let verifier = api.clone();
    let audit = JsonlAudit::open(&audit_path)
        .with_context(|| format!("open audit log {}", audit_path.display()))?;
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });

    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        audit,
        HarnessConfig::default(),
    );
    let outcome = harness.run(cancel).await?;
    let after = verifier.health().await?;
    if !after.serving {
        bail!("kilnd became unhealthy after the demo: {}", after.detail);
    }

    println!("{}", outcome.final_answer);
    println!("session: {}", outcome.session_id);
    println!("sandbox: {} (deleted)", outcome.sandbox_id);
    println!(
        "steps: {}, tool calls: {}",
        outcome.steps, outcome.tool_calls
    );
    println!("audit: {}", audit_path.display());
    Ok(())
}
