use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use kiln_harness::api::{SandboxApi, TonicKilnd};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::compaction::{
    COMPACTION_HEADROOM_TOKENS, CompactionPolicy, MAX_COMPACT_AT, MIN_COMPACT_AT,
};
use kiln_harness::engine::{
    CheckOutcome, Harness, HarnessConfig, HarnessError, MAX_OUTPUT_TOKENS_PER_REQUEST,
    MAX_SANDBOX_IDLE_TIMEOUT, MAX_SANDBOX_LIFETIME_MARGIN, MAX_SESSION_TIMEOUT, MAX_STEPS,
    MAX_TASK_TEXT_BYTES, MAX_TOOL_CALLS, MAX_TOOL_CALLS_PER_TURN, MAX_TOOL_TIMEOUT,
    MAX_TOTAL_MODEL_OUTPUT_BYTES, MAX_TOTAL_TOKENS, MAX_TRUNCATION_RECOVERIES,
    MIN_SANDBOX_IDLE_TIMEOUT, MIN_SANDBOX_LIFETIME_MARGIN, SessionOutcome, SessionTask,
};
use kiln_harness::eval;
use kiln_harness::files::read_regular_file;
use kiln_harness::journal::{
    JournalEvent, JournalLimits, JsonlJournal, LoadedJournal, open_journal_source, read_journal,
    read_open_journal,
};
use kiln_harness::live::TerminalLive;
use kiln_harness::pricing::{self, ModelPricing};
use kiln_harness::provider::{
    CompactionResponse, ModelRequest, ModelResponse, Provider, ProviderError, ProviderInfo,
    ScriptedProvider, Usage,
};
use kiln_harness::providers::anthropic::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use kiln_harness::providers::{
    AnthropicConfig, AnthropicProvider, Effort, OpenAiCompatConfig, OpenAiCompatProvider,
    SecretString, default_system_prompt_for,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{ToolLimits, ToolSet};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Provider-neutral AI harness over kilnd.v1")]
struct Cli {
    /// kilnd's Unix socket (unused by `journal verify`, `eval compare` and
    /// `eval cost`).
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
        /// Print the model's command output to stderr as it arrives, each
        /// line prefixed with its call and stream, every byte that is not
        /// printable ASCII, a newline or a tab shown as `\xNN`.
        #[arg(long)]
        live: bool,
    },
    /// Run one task against a model server and kilnd.
    Run(Box<RunArgs>),
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
        /// Print the model's command output to stderr as it arrives, each
        /// line prefixed with its call and stream, every byte that is not
        /// printable ASCII, a newline or a tab shown as `\xNN`.
        #[arg(long)]
        live: bool,
    },
    /// Inspect session journals offline.
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
    /// Run, compare, and self-test the eval suite.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ProviderKind {
    /// An OpenAI-compatible `/v1/chat/completions` server (MLX, Ollama,
    /// llama.cpp, vLLM).
    #[value(name = "openai-compat")]
    OpenAiCompat,
    /// Anthropic's Messages API.
    #[value(name = "anthropic")]
    Anthropic,
}

/// `--tool-set`.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ToolSetArg {
    #[value(name = "base")]
    Base,
    #[value(name = "coding")]
    Coding,
}

impl From<ToolSetArg> for ToolSet {
    fn from(arg: ToolSetArg) -> Self {
        match arg {
            ToolSetArg::Base => Self::Base,
            ToolSetArg::Coding => Self::CodingV1,
        }
    }
}

/// The provider and limit flags `run` and `eval run` share.
#[derive(clap::Args)]
struct ModelArgs {
    #[arg(long, value_enum)]
    provider: ProviderKind,
    /// API root, such as `http://192.168.1.20:11434/v1`. Required for
    /// openai-compat; anthropic defaults to `https://api.anthropic.com`.
    #[arg(long)]
    base_url: Option<String>,
    /// Model name as the server knows it. Required for openai-compat;
    /// anthropic defaults to `claude-opus-5-5`.
    #[arg(long)]
    model: Option<String>,
    /// Environment variable holding the API key; the key is never a flag.
    /// Without it, anthropic needs a `--base-url` naming a host-side proxy
    /// that adds the key.
    #[arg(long, value_name = "VAR")]
    api_key_env: Option<String>,
    /// Sampling temperature, 0.0 to 2.0 (openai-compat; default 0).
    #[arg(long)]
    temperature: Option<f32>,
    /// Sampling seed, forwarded when set (openai-compat).
    #[arg(long)]
    seed: Option<u64>,
    /// Effort: low, medium, high, xhigh, or max (anthropic; default high).
    #[arg(long, value_name = "LEVEL", value_parser = parse_effort)]
    effort: Option<Effort>,
    /// Send `fallbacks: "default"`, letting the API rerun a declined
    /// request on another model (anthropic; off by default, so every turn is
    /// served by the model asked for).
    #[arg(long)]
    fallbacks: bool,
    /// Mark every tool schema `strict` (anthropic).
    #[arg(long)]
    strict_tools: bool,
    /// Keep thinking only for the last N assistant turns, clearing older
    /// thinking server-side (anthropic; off by default). Each tool-results
    /// message then ends with a short text block so the API can count turns.
    #[arg(long, value_name = "N")]
    clear_thinking: Option<u32>,
    /// Summarize the whole history, then go on from the summary, before a
    /// turn whose prompt is estimated over TOKENS (anthropic; 20000 to
    /// 1000000; off by default). The estimate is the last response's usage
    /// plus the tool results since, so it is a heuristic, not a guarantee.
    /// Not with --clear-thinking or --fallbacks.
    #[arg(long, value_name = "TOKENS")]
    compact_at: Option<u64>,
    #[arg(
        long,
        value_enum,
        default_value_t = ToolSetArg::Base,
        help = "Tools the model gets: base (exec, read_file, write_file, list_dir, \
                workspace_diff) or coding (adds edit_file, search and line ranges in read_file)"
    )]
    tool_set: ToolSetArg,
    /// Model turns per session.
    #[arg(long, default_value_t = 40)]
    max_steps: u32,
    /// Tool calls per session.
    #[arg(long, default_value_t = 120)]
    max_tool_calls: u32,
    /// Tool calls in one model turn.
    #[arg(long, default_value_t = 8)]
    max_tool_calls_per_turn: u32,
    /// Wall-clock limit for the session, including sandbox creation.
    #[arg(long, default_value_t = 900)]
    session_timeout_secs: u64,
    /// Wall-clock limit for each tool call's sandbox executions together
    /// (`read_file` runs two); response-loss recovery may add up to 1 s.
    /// When kilnd supports expiry, this plus 1 s of recovery plus a third
    /// of the idle timeout must fit in the idle timeout: about 5 s at most
    /// for a 10 s idle timeout, 79 s for the default 120 s.
    #[arg(long, default_value_t = 30)]
    tool_timeout_secs: u64,
    /// Idle time after which kilnd may reclaim the sandbox, when it supports
    /// expiry. The harness renews it during model calls. It must exceed the
    /// tool timeout plus 1 s by half again: 30 s tools need at least about
    /// 47 s.
    #[arg(long, default_value_t = 120)]
    sandbox_idle_timeout_secs: u64,
    /// Added to the session and cleanup windows to form the sandbox's
    /// maximum lifetime, when kilnd supports expiry.
    #[arg(long, default_value_t = 60)]
    sandbox_lifetime_margin_secs: u64,
    /// Output tokens per model request, hidden reasoning included.
    #[arg(long, default_value_t = 16_384)]
    max_output_tokens: u32,
    /// Responses cut off at --max-output-tokens that the session discards
    /// and goes on from, 0 to 16 per session; each uses a step. 0 makes
    /// every such response fatal.
    #[arg(long, default_value_t = 2)]
    max_truncation_recoveries: u32,
    /// Input tokens per session, cached input included.
    #[arg(long, default_value_t = 2_000_000)]
    max_total_input_tokens: u64,
    /// Output tokens per session, hidden reasoning included.
    #[arg(long, default_value_t = 600_000)]
    max_total_output_tokens: u64,
    /// Bytes of model text and tool-call arguments per session, counted
    /// locally whatever the server reports as usage.
    #[arg(long, default_value_t = 8 << 20)]
    max_total_output_bytes: u64,
}

#[derive(clap::Args)]
struct RunArgs {
    #[command(flatten)]
    model: ModelArgs,
    /// The task.
    #[arg(
        long,
        conflicts_with = "prompt_file",
        required_unless_present = "prompt_file"
    )]
    prompt: Option<String>,
    /// Read the task from a UTF-8 file.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Read the system prompt from a UTF-8 file instead of the default.
    #[arg(long)]
    system_file: Option<PathBuf>,
    /// Append-only metadata audit destination.
    #[arg(long, default_value = "audit/run.jsonl")]
    audit: PathBuf,
    /// New session journal; defaults to `journal/run-<unix_ms>.jsonl`.
    #[arg(long)]
    journal: Option<PathBuf>,
    /// Print the model's command output to stderr as it arrives, each
    /// line prefixed with its call and stream, every byte that is not
    /// printable ASCII, a newline or a tab shown as `\xNN`.
    #[arg(long)]
    live: bool,
}

#[derive(Subcommand)]
enum EvalCommand {
    /// Run every eval task N times against one provider and write a report.
    Run(Box<EvalRunArgs>),
    /// Compare two eval reports as Markdown on stdout.
    Compare {
        /// Output directory of the base run.
        base: PathBuf,
        /// Output directory of the new run.
        new: PathBuf,
    },
    /// Price a finished eval run from its journals with the current price
    /// table, as Markdown on stdout.
    Cost {
        /// Output directory of the run.
        dir: PathBuf,
    },
    /// Prove each task's check: it fails on the bare seed and passes after
    /// the reference solution, through the real engine with a scripted
    /// model; no network provider is built.
    Selftest {
        /// Directory of task directories.
        #[arg(long, default_value = "evals/tasks")]
        tasks: PathBuf,
        /// Test only this task; repeatable.
        #[arg(long = "task", value_name = "ID")]
        only: Vec<String>,
        /// New directory for audits and journals; must not exist.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(clap::Args)]
struct EvalRunArgs {
    #[command(flatten)]
    model: ModelArgs,
    /// Directory of task directories.
    #[arg(long, default_value = "evals/tasks")]
    tasks: PathBuf,
    /// Run only this task; repeatable.
    #[arg(long = "task", value_name = "ID")]
    only: Vec<String>,
    /// Runs per task, 1 to 100.
    #[arg(long, default_value_t = 1)]
    trials: u32,
    /// Runs at a time, 1 to 8: each holds its own kilnd connection and at
    /// most one command stream, within kilnd's default of 8 streams per user.
    #[arg(long, default_value_t = 1)]
    concurrency: u32,
    /// Longest any run may take, in seconds, sandbox creation and the check
    /// included: each run's session timeout becomes the smaller of this and
    /// the task's own (or --session-timeout-secs). A run that reaches it ends
    /// at the session deadline and fails. Unset, runs keep their own limits.
    #[arg(long)]
    max_run_secs: Option<u64>,
    /// New directory for every run's audit and journal, the results and
    /// the report; must not exist.
    #[arg(long)]
    out: PathBuf,
    /// Free text for the report, at most 128 bytes.
    #[arg(long)]
    label: Option<String>,
    /// Replay each completed run's journal against a fresh sandbox.
    #[arg(long)]
    replay: bool,
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
        Command::Demo {
            audit,
            journal,
            live,
        } => {
            let journal = journal.unwrap_or_else(|| default_journal("demo"));
            demo(cli.socket, &audit, &journal, live).await
        }
        Command::Run(args) => {
            let journal = args
                .journal
                .clone()
                .unwrap_or_else(|| default_journal("run"));
            let audit = args.audit.clone();
            // Everything is checked, and the key read, before any output is
            // opened or kilnd is contacted.
            let prepared = prepare_run(&args)?;
            run(cli.socket, prepared, &audit, &journal, args.live).await
        }
        Command::Replay {
            from,
            audit,
            journal,
            live,
        } => {
            let journal = journal.unwrap_or_else(|| default_journal("replay"));
            replay(cli.socket, &from, &audit, &journal, live).await
        }
        Command::Journal {
            command: JournalCommand::Verify { path },
        } => Ok(verify(&path)),
        Command::Eval {
            command: EvalCommand::Run(args),
        } => eval_run(cli.socket, &args).await,
        Command::Eval {
            command: EvalCommand::Compare { base, new },
        } => eval_compare(&base, &new),
        Command::Eval {
            command: EvalCommand::Cost { dir },
        } => eval_cost(&dir),
        Command::Eval {
            command: EvalCommand::Selftest { tasks, only, out },
        } => eval_selftest(cli.socket, &tasks, &only, &out).await,
    }
}

async fn demo(
    socket: PathBuf,
    audit_path: &Path,
    journal_path: &Path,
    live: bool,
) -> Result<ExitCode> {
    refuse_aliases(&[
        ("--audit", FileIdentity::of_path(audit_path)?),
        ("--journal", FileIdentity::of_path(journal_path)?),
    ])?;
    let (audit, journal) = open_sinks(None, audit_path, journal_path)?;
    let api = connect(socket).await?;
    let mut harness = with_live(
        Harness::new(
            api.clone(),
            ScriptedProvider::demo(),
            audit,
            journal,
            HarnessConfig::default(),
        ),
        live,
    );
    let outcome = harness.run(&SessionTask::demo(), interrupt()).await;
    drain_live(&harness);
    let outcome = outcome?;
    warn_if_unhealthy(&api, HarnessConfig::default().lifecycle_timeout).await;
    print_outcome(&outcome);
    println!("audit: {}", audit_path.display());
    println!("journal: {}", journal_path.display());
    Ok(ExitCode::SUCCESS)
}

struct PreparedRun {
    task: SessionTask,
    config: HarnessConfig,
    provider: RunProvider,
}

/// The provider `run` was asked for.
enum RunProvider {
    OpenAiCompat(OpenAiCompatProvider),
    Anthropic(AnthropicProvider),
}

#[async_trait::async_trait]
impl Provider for RunProvider {
    fn info(&self) -> ProviderInfo {
        match self {
            Self::OpenAiCompat(provider) => provider.info(),
            Self::Anthropic(provider) => provider.info(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        match self {
            Self::OpenAiCompat(provider) => provider.complete(request).await,
            Self::Anthropic(provider) => provider.complete(request).await,
        }
    }

    async fn compact(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        match self {
            Self::OpenAiCompat(provider) => provider.compact(request).await,
            Self::Anthropic(provider) => provider.compact(request).await,
        }
    }
}

fn parse_effort(value: &str) -> Result<Effort, String> {
    value.parse()
}

/// Unsigned limits must be positive and at most their hard ceiling.
fn check_limit<T: Copy + PartialOrd + Default + std::fmt::Display>(
    flag: &str,
    value: T,
    max: T,
) -> Result<()> {
    if value <= T::default() || value > max {
        bail!("{flag} must be between 1 and {max}");
    }
    Ok(())
}

/// A whole number of seconds within `[min, max]`.
fn check_secs(flag: &str, value: u64, min: Duration, max: Duration) -> Result<()> {
    if value < min.as_secs() || value > max.as_secs() {
        bail!(
            "{flag} must be between {} and {}",
            min.as_secs(),
            max.as_secs()
        );
    }
    Ok(())
}

fn prepare_run(args: &RunArgs) -> Result<PreparedRun> {
    let config = model_config(&args.model)?;

    let prompt = match (&args.prompt, &args.prompt_file) {
        (Some(prompt), None) => prompt.clone(),
        (None, Some(path)) => read_text_file("--prompt-file", path)?,
        _ => bail!("give exactly one of --prompt and --prompt-file"),
    };
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    if prompt.len() > MAX_TASK_TEXT_BYTES {
        bail!("the prompt is longer than {MAX_TASK_TEXT_BYTES} bytes");
    }
    let system = match &args.system_file {
        Some(path) => read_text_file("--system-file", path)?,
        None => default_system_prompt_for(&config.tool_limits, config.tool_set),
    };

    let provider = build_provider(&args.model)?;
    Ok(PreparedRun {
        task: SessionTask::new(system, prompt),
        config,
        provider,
    })
}

/// Check the shared provider and limit flags and build the configuration
/// they ask for.
fn model_config(args: &ModelArgs) -> Result<HarnessConfig> {
    check_provider_flags(args)?;
    check_limit("--max-steps", args.max_steps, MAX_STEPS)?;
    check_limit("--max-tool-calls", args.max_tool_calls, MAX_TOOL_CALLS)?;
    check_limit(
        "--max-tool-calls-per-turn",
        args.max_tool_calls_per_turn,
        MAX_TOOL_CALLS_PER_TURN,
    )?;
    check_limit(
        "--session-timeout-secs",
        args.session_timeout_secs,
        MAX_SESSION_TIMEOUT.as_secs(),
    )?;
    check_limit(
        "--tool-timeout-secs",
        args.tool_timeout_secs,
        MAX_TOOL_TIMEOUT.as_secs(),
    )?;
    check_limit(
        "--max-output-tokens",
        args.max_output_tokens,
        MAX_OUTPUT_TOKENS_PER_REQUEST,
    )?;
    if let Some(model) = model_metadata(args)
        && args.max_output_tokens > model.max_output_tokens
    {
        bail!(
            "--max-output-tokens {} exceeds {}'s {}-token output limit",
            args.max_output_tokens,
            model.model,
            model.max_output_tokens
        );
    }
    if args.max_truncation_recoveries > MAX_TRUNCATION_RECOVERIES {
        bail!("--max-truncation-recoveries must be between 0 and {MAX_TRUNCATION_RECOVERIES}");
    }
    check_limit(
        "--max-total-input-tokens",
        args.max_total_input_tokens,
        MAX_TOTAL_TOKENS,
    )?;
    check_limit(
        "--max-total-output-tokens",
        args.max_total_output_tokens,
        MAX_TOTAL_TOKENS,
    )?;
    check_limit(
        "--max-total-output-bytes",
        args.max_total_output_bytes,
        MAX_TOTAL_MODEL_OUTPUT_BYTES,
    )?;
    let compaction = args
        .compact_at
        .map(|compact_at| compaction_policy(args, compact_at))
        .transpose()?;
    check_secs(
        "--sandbox-idle-timeout-secs",
        args.sandbox_idle_timeout_secs,
        MIN_SANDBOX_IDLE_TIMEOUT,
        MAX_SANDBOX_IDLE_TIMEOUT,
    )?;
    check_secs(
        "--sandbox-lifetime-margin-secs",
        args.sandbox_lifetime_margin_secs,
        MIN_SANDBOX_LIFETIME_MARGIN,
        MAX_SANDBOX_LIFETIME_MARGIN,
    )?;
    let config = HarnessConfig {
        max_steps: args.max_steps,
        max_tool_calls: args.max_tool_calls,
        max_tool_calls_per_turn: args.max_tool_calls_per_turn,
        session_timeout: Duration::from_secs(args.session_timeout_secs),
        max_output_tokens_per_request: args.max_output_tokens,
        max_truncation_recoveries: args.max_truncation_recoveries,
        max_total_input_tokens: args.max_total_input_tokens,
        max_total_output_tokens: args.max_total_output_tokens,
        max_total_model_output_bytes: args.max_total_output_bytes,
        tool_limits: ToolLimits {
            timeout: Duration::from_secs(args.tool_timeout_secs),
            ..ToolLimits::default()
        },
        sandbox_idle_timeout: Duration::from_secs(args.sandbox_idle_timeout_secs),
        sandbox_lifetime_margin: Duration::from_secs(args.sandbox_lifetime_margin_secs),
        compaction,
        tool_set: args.tool_set.into(),
        ..HarnessConfig::default()
    };
    config.validate()?;
    Ok(config)
}

/// The policy `--compact-at` asks for, refused in range or in combination
/// as [`check_provider_flags`] refuses foreign flags.
fn compaction_policy(args: &ModelArgs, compact_at: u64) -> Result<CompactionPolicy> {
    if !(MIN_COMPACT_AT..=MAX_COMPACT_AT).contains(&compact_at) {
        bail!("--compact-at must be between {MIN_COMPACT_AT} and {MAX_COMPACT_AT}");
    }
    // The API cannot put compaction and context_management on one request,
    // and summaries alongside thinking clearing are untested.
    if args.clear_thinking.is_some() {
        bail!("--compact-at does not apply with --clear-thinking");
    }
    // Another model could write or read the signed summary, unpriced.
    if args.fallbacks {
        bail!("--compact-at does not apply with --fallbacks");
    }
    if let Some(model) = model_metadata(args) {
        let needed = compact_at
            .saturating_add(u64::from(args.max_output_tokens))
            .saturating_add(COMPACTION_HEADROOM_TOKENS);
        if needed > model.context_window {
            bail!(
                "--compact-at {compact_at} plus --max-output-tokens {} and {} tokens of headroom \
                 exceeds {}'s {}-token context window",
                args.max_output_tokens,
                COMPACTION_HEADROOM_TOKENS,
                model.model,
                model.context_window
            );
        }
    }
    Ok(CompactionPolicy::new(compact_at))
}

/// The table entry for the model the flags ask for: only Anthropic's
/// models are in the table, so `None` for openai-compat whatever its model
/// is called, and for a model the table does not list.
fn model_metadata(args: &ModelArgs) -> Option<&'static ModelPricing> {
    match args.provider {
        ProviderKind::Anthropic => pricing::lookup(args.model.as_deref().unwrap_or(DEFAULT_MODEL)),
        ProviderKind::OpenAiCompat => None,
    }
}

/// The prices `eval run` estimates cost at, or why it cannot: openai-compat
/// has no table, a model the table does not list has no prices, and with
/// `--fallbacks` another model may serve, and bill, a turn.
fn suite_pricing(args: &ModelArgs) -> (Option<&'static ModelPricing>, Option<String>) {
    let unpriced = |reason: String| (None, Some(reason));
    match args.provider {
        ProviderKind::OpenAiCompat => unpriced("openai-compat has no price table".to_string()),
        ProviderKind::Anthropic => match model_metadata(args) {
            None => unpriced(format!(
                "no price table for {}",
                args.model.as_deref().unwrap_or(DEFAULT_MODEL)
            )),
            Some(_) if args.fallbacks => unpriced("--fallbacks may bill another model".to_string()),
            Some(pricing) => (Some(pricing), None),
        },
    }
}

/// The base URL's host alone, for the eval manifest: never a user,
/// password, port, path, query or fragment.
fn base_url_host(args: &ModelArgs) -> Option<String> {
    let base_url = match args.provider {
        ProviderKind::OpenAiCompat => args.base_url.as_deref()?,
        ProviderKind::Anthropic => args.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
    };
    reqwest::Url::parse(base_url)
        .ok()?
        .host_str()
        .map(str::to_string)
}

/// The provider `run` was asked for, from its flags. Errors name flags and
/// variables, never a key.
fn build_provider(args: &ModelArgs) -> Result<RunProvider> {
    Ok(match args.provider {
        ProviderKind::OpenAiCompat => {
            let (Some(base_url), Some(model)) = (&args.base_url, &args.model) else {
                bail!("--provider openai-compat needs --base-url and --model");
            };
            let mut provider_config = OpenAiCompatConfig::new(base_url, model)?;
            provider_config.api_key = args.api_key_env.as_deref().map(read_api_key).transpose()?;
            provider_config.temperature = args.temperature.unwrap_or(0.0);
            provider_config.seed = args.seed;
            RunProvider::OpenAiCompat(OpenAiCompatProvider::new(provider_config)?)
        }
        ProviderKind::Anthropic => {
            // Without a key the harness relies on a credential-injecting
            // proxy; the API itself would only answer 401.
            let mut provider_config = AnthropicConfig::new(
                args.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
                args.model.as_deref().unwrap_or(DEFAULT_MODEL),
                args.api_key_env.as_deref().map(read_api_key).transpose()?,
            )?;
            if provider_config.api_key.is_none() && provider_config.targets_the_api() {
                bail!(
                    "--provider anthropic needs --api-key-env, or --base-url pointing at a \
                     credential-injecting proxy"
                );
            }
            provider_config.effort = args.effort.unwrap_or_default();
            provider_config.fallbacks = args.fallbacks;
            provider_config.strict_tools = args.strict_tools;
            provider_config.clear_thinking = args.clear_thinking;
            RunProvider::Anthropic(AnthropicProvider::new(provider_config)?)
        }
    })
}

/// Refuse flags that mean nothing to the chosen provider, before anything
/// else is checked.
fn check_provider_flags(args: &ModelArgs) -> Result<()> {
    let (name, foreign): (&str, &[(&str, bool)]) = match args.provider {
        ProviderKind::OpenAiCompat => (
            "openai-compat",
            &[
                ("--effort", args.effort.is_some()),
                ("--strict-tools", args.strict_tools),
                ("--fallbacks", args.fallbacks),
                ("--clear-thinking", args.clear_thinking.is_some()),
                ("--compact-at", args.compact_at.is_some()),
            ],
        ),
        ProviderKind::Anthropic => (
            "anthropic",
            &[
                ("--temperature", args.temperature.is_some()),
                ("--seed", args.seed.is_some()),
            ],
        ),
    };
    if let Some((flag, _)) = foreign.iter().find(|(_, given)| *given) {
        bail!("{flag} does not apply to --provider {name}");
    }
    Ok(())
}

/// Read the API key from the named environment variable. Errors name the
/// variable, never its value.
fn read_api_key(var: &str) -> Result<SecretString> {
    let mut bytes = var.bytes();
    let valid_name = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid_name {
        bail!("--api-key-env must name an environment variable ([A-Za-z_][A-Za-z0-9_]*)");
    }
    match std::env::var_os(var) {
        None => bail!("--api-key-env: {var} is not set"),
        Some(value) if value.is_empty() => bail!("--api-key-env: {var} is set but empty"),
        Some(value) => value
            .into_string()
            .map(SecretString::new)
            .map_err(|_| anyhow!("--api-key-env: {var} is not UTF-8")),
    }
}

/// Read a UTF-8 task file of at most `MAX_TASK_TEXT_BYTES`.
///
/// It must be a regular file: a FIFO or device would block before any
/// deadline exists, so it is refused, and so is a symlink as the last path
/// component.
fn read_text_file(flag: &str, path: &Path) -> Result<String> {
    let bytes = read_regular_file(path, MAX_TASK_TEXT_BYTES)
        .map_err(|error| anyhow!("{flag} {} is {error}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| anyhow!("{flag} {} is not UTF-8", path.display()))
}

async fn run(
    socket: PathBuf,
    prepared: PreparedRun,
    audit_path: &Path,
    journal_path: &Path,
    live: bool,
) -> Result<ExitCode> {
    refuse_aliases(&[
        ("--audit", FileIdentity::of_path(audit_path)?),
        ("--journal", FileIdentity::of_path(journal_path)?),
    ])?;
    let (audit, journal) = open_sinks(None, audit_path, journal_path)?;
    let api = match connect(socket).await {
        Ok(api) => api,
        Err(error) => {
            // Nothing has been written; leave no empty outputs behind.
            // The journal was opened last, so it goes first: the audit may
            // have created the directory they share.
            journal.discard();
            audit.discard();
            return Err(error);
        }
    };
    let PreparedRun {
        task,
        config,
        provider,
    } = prepared;
    let lifecycle = config.lifecycle_timeout;
    let mut harness = with_live(
        Harness::new(api.clone(), provider, audit, journal, config),
        live,
    );
    let outcome = harness.run(&task, interrupt()).await;
    drain_live(&harness);
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("journal: {}", journal_path.display());
            return Err(error.into());
        }
    };
    warn_if_unhealthy(&api, lifecycle).await;
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
    live: bool,
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
    if let Some(problem) = provider.compaction_problem() {
        bail!("cannot replay {}: {problem}", from.display());
    }
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
    let config = provider.replay_config(HarnessConfig::default());

    let (audit, journal) = open_sinks(Some(&source), audit_path, journal_path)?;
    let api = connect(socket).await?;
    let lifecycle = config.lifecycle_timeout;
    let mut harness = with_live(
        Harness::new(api.clone(), provider, audit, journal, config),
        live,
    );
    let result = harness.replay(interrupt()).await;
    drain_live(&harness);
    warn_if_unhealthy(&api, lifecycle).await;
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
            } else if replay.check_diverged() {
                println!("diverged at check");
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
    if let Some(tool_set) = &summary.tool_set {
        println!("tool set: {}", terminal_safe(tool_set));
    }
    if summary.compactions > 0 {
        println!("compactions: {}", summary.compactions);
    }
    if summary.seed_files > 0 {
        println!("seed: {} files", summary.seed_files);
    }
    if let Some(check) = &summary.check {
        println!("check: {}", terminal_safe(&check.to_string()));
    }
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
    /// Compactions that returned a summary.
    compactions: u32,
    /// The recorded tool set, when not base.
    tool_set: Option<String>,
    usage: Usage,
    seed_files: usize,
    check: Option<CheckOutcome>,
}

fn summarize(journal: &LoadedJournal) -> Summary {
    let mut summary = Summary {
        outcome: None,
        sandbox_deleted: false,
        turns: 0,
        tool_calls: 0,
        compactions: 0,
        tool_set: None,
        usage: Usage::default(),
        seed_files: 0,
        check: None,
    };
    for record in &journal.records {
        match &record.event {
            JournalEvent::SessionStarted { seed, tool_set, .. } => {
                summary.seed_files = seed.len();
                summary.tool_set.clone_from(tool_set);
            }
            JournalEvent::CheckCompleted { outcome, .. } => summary.check = Some(outcome.clone()),
            JournalEvent::ModelRequested { .. } => summary.turns += 1,
            JournalEvent::ModelResponded { response, .. } => {
                summary.usage = summary.usage + response.usage;
            }
            JournalEvent::CompactionResponded { response, .. } => {
                summary.usage = summary.usage + response.usage;
                summary.compactions += u32::from(response.is_success());
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

/// Connect the transport only. `Harness::run` makes the one Health call a
/// session needs, under its deadline and cancellation.
async fn connect(socket: PathBuf) -> Result<TonicKilnd> {
    Ok(TonicKilnd::connect(socket).await?)
}

/// After a session, report on stderr if kilnd stopped serving, within the
/// lifecycle window and at most 10 seconds. This never changes the
/// session's exit status and never waits longer.
async fn warn_if_unhealthy(api: &TonicKilnd, lifecycle: Duration) {
    let window = lifecycle.min(Duration::from_secs(10));
    match tokio::time::timeout(window, api.health()).await {
        Ok(Ok(health)) if health.serving => {}
        Ok(Ok(health)) => eprintln!(
            "warning: kilnd is not serving after the session: {}",
            terminal_safe(&health.detail)
        ),
        Ok(Err(error)) => eprintln!(
            "warning: kilnd health after the session failed: {}",
            terminal_safe(&error.to_string())
        ),
        Err(_) => eprintln!(
            "warning: kilnd health after the session did not answer within {} ms",
            window.as_millis()
        ),
    }
}

/// Open both outputs without writing, then refuse if any two of the source
/// and outputs are the same file by `fstat` on their open descriptors. Path
/// checks earlier are only diagnostics: a path can be relinked after them,
/// but an open descriptor cannot change which file it names. A refused
/// output is discarded: what its `open` created is removed through the
/// pinned parent descriptors, never by path.
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
            audit.discard();
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
        // Newest first, so a directory the audit created for both is empty
        // by the time the audit's rollback reaches it.
        journal.discard();
        audit.discard();
        bail!("{flags} refer to the same file");
    }
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

/// Give `--live` output still queued, after the session and its cleanup,
/// a short time to print before the command exits.
fn drain_live<A, P, S, J>(harness: &Harness<A, P, S, J>)
where
    A: kiln_harness::api::SandboxApi,
    P: Provider,
    S: kiln_harness::audit::AuditSink,
    J: kiln_harness::journal::JournalSink,
{
    let _ = harness.drain_live_output(Duration::from_secs(2));
}

/// `harness`, printing its streamed command output to stderr when `live`.
fn with_live<P: Provider>(
    harness: Harness<TonicKilnd, P, JsonlAudit, JsonlJournal>,
    live: bool,
) -> Harness<TonicKilnd, P, JsonlAudit, JsonlJournal> {
    if live {
        harness.with_live_output(Arc::new(TerminalLive::stderr()))
    } else {
        harness
    }
}

/// Model text made safe for a terminal: C0 and C1 control characters and
/// DEL, which can carry escape sequences, are shown as `\u{..}`, except for
/// newline and tab. The journal keeps the raw text.
fn terminal_safe(text: &str) -> String {
    use std::fmt::Write as _;
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\n' && character != '\t' {
            let _ = write!(safe, "\\u{{{:x}}}", u32::from(character));
        } else {
            safe.push(character);
        }
    }
    safe
}

async fn eval_run(socket: PathBuf, args: &EvalRunArgs) -> Result<ExitCode> {
    // Everything is checked, every task loaded, and the key read, before
    // any output is made or kilnd is contacted.
    let config = model_config(&args.model)?;
    eval::validate_trials_and_label(args.trials, args.label.as_deref())?;
    eval::validate_concurrency(args.concurrency)?;
    let tasks = eval::load_tasks(&args.tasks, &args.only, &config)?;
    if tasks.is_empty() {
        bail!("{} holds no tasks", args.tasks.display());
    }
    eval::check_run_cap(&tasks, &config, args.max_run_secs)?;
    let provider = build_provider(&args.model)?.info();
    eval::refuse_existing_out(&args.out)?;
    let (pricing, unpriced_reason) = suite_pricing(&args.model);
    let api = connect(socket).await?;
    let options = eval::SuiteOptions {
        out: args.out.clone(),
        trials: args.trials,
        concurrency: args.concurrency,
        max_run_secs: args.max_run_secs,
        effort: match args.model.provider {
            ProviderKind::Anthropic => Some(args.model.effort.unwrap_or_default().to_string()),
            ProviderKind::OpenAiCompat => None,
        },
        clear_thinking: match args.model.provider {
            ProviderKind::Anthropic => Some(
                args.model
                    .clear_thinking
                    .map_or_else(|| "off".to_string(), |turns| format!("keep {turns}")),
            ),
            ProviderKind::OpenAiCompat => None,
        },
        pricing,
        unpriced_reason,
        replay: args.replay,
        label: args.label.clone(),
        base_config: config,
        provider,
        base_url_host: base_url_host(&args.model),
    };
    let cancel = interrupt();
    let printer = ProgressPrinter::start(cancel.clone());
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options,
        || build_provider(&args.model).map_err(|error| error.to_string()),
        &cancel,
        &mut |result| {
            printer.print(format!(
                "{} #{}: {} (check {}){}",
                result.task,
                result.trial,
                result.outcome,
                result.check.as_deref().unwrap_or("not run"),
                result
                    .replay
                    .as_deref()
                    .map_or_else(String::new, |replay| format!(", replay {replay}"))
            ))
        },
    )
    .await;
    // Every progress line is out, or failed, before the summary follows.
    let printer_error = printer.finish();
    let summary = summary?;
    warn_if_unhealthy(&api, options.base_config.lifecycle_timeout).await;
    // No run is in flight now; a closed stdout or stderr only loses text.
    let overall = &summary.report.overall;
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "passed {} of {} runs; macro pass rate {}",
        overall.passed,
        overall.runs,
        overall
            .macro_pass_rate
            .map_or_else(|| "n/a".to_string(), |rate| format!("{:.1}%", rate * 100.0))
    );
    let _ = writeln!(stdout, "report: {}", args.out.join("report.md").display());
    // Both are shown: a printer failure can be what canceled the suite,
    // and its stop reason may then read only `canceled`.
    let mut stderr = std::io::stderr();
    if let Some(error) = &printer_error {
        let _ = writeln!(stderr, "progress output: {error}");
    }
    if let Some(stopped) = &summary.stopped {
        let _ = writeln!(stderr, "stopped early: {stopped}");
    }
    if summary.stopped.is_some() || printer_error.is_some() {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints `eval run`'s progress lines on a thread of its own, so the
/// runtime thread never writes to stdout while runs are in flight: a slow
/// stdout cannot stall them and a closed one cannot panic past their
/// cleanup. The first write error is kept, cancels the suite, and is the
/// answer to every later line.
struct ProgressPrinter {
    lines: Option<std::sync::mpsc::Sender<String>>,
    error: Arc<std::sync::Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProgressPrinter {
    fn start(cancel: CancellationToken) -> Self {
        let (lines, received) = std::sync::mpsc::channel::<String>();
        let error = Arc::new(std::sync::Mutex::new(None));
        let failed = Arc::clone(&error);
        let thread = std::thread::spawn(move || {
            for line in received {
                let mut stdout = std::io::stdout().lock();
                if let Err(written) = writeln!(stdout, "{line}").and_then(|()| stdout.flush()) {
                    *failed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(written.to_string());
                    cancel.cancel();
                    return;
                }
            }
        });
        Self {
            lines: Some(lines),
            error,
            thread: Some(thread),
        }
    }

    fn error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Queue `line`, or say why stdout can take no more.
    fn print(&self, line: String) -> Result<(), String> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        let sent = self.lines.as_ref().map(|lines| lines.send(line));
        match sent {
            Some(Ok(())) => Ok(()),
            _ => Err(self.error().unwrap_or_else(|| "stdout closed".to_string())),
        }
    }

    /// Wait for every queued line, and return the write error, if any.
    fn finish(mut self) -> Option<String> {
        drop(self.lines.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.error()
    }
}

fn eval_compare(base: &Path, new: &Path) -> Result<ExitCode> {
    let base = eval::read_report(base)?;
    let new = eval::read_report(new)?;
    print!("{}", eval::compare(&base, &new));
    Ok(ExitCode::SUCCESS)
}

/// Price a finished run again from its journals. Like `compare`, it reads
/// files only and never contacts kilnd.
fn eval_cost(dir: &Path) -> Result<ExitCode> {
    let manifest = eval::read_manifest(dir)?;
    if manifest.provider != "anthropic" {
        bail!("{} has no price table", terminal_safe(&manifest.provider));
    }
    let Some(pricing) = pricing::lookup(&manifest.model) else {
        bail!("no price table for {}", terminal_safe(&manifest.model));
    };
    let summary = eval::reprice(dir, pricing)?;
    print!("{}", summary.markdown());
    Ok(ExitCode::SUCCESS)
}

async fn eval_selftest(
    socket: PathBuf,
    tasks: &Path,
    only: &[String],
    out: &Path,
) -> Result<ExitCode> {
    let config = eval::selftest_config();
    let root = tasks;
    let tasks = eval::load_tasks(root, only, &config)?;
    if tasks.is_empty() {
        bail!("{} holds no tasks", root.display());
    }
    eval::refuse_existing_out(out)?;
    let api = connect(socket).await?;
    let summary =
        eval::selftest(
            &api,
            &tasks,
            &config,
            out,
            &interrupt(),
            &mut |result| match &result.problem {
                None => println!("ok {}", result.task),
                Some(problem) => println!("FAIL {}: {}", result.task, terminal_safe(problem)),
            },
        )
        .await?;
    warn_if_unhealthy(&api, config.lifecycle_timeout).await;
    if let Some(stopped) = &summary.stopped {
        eprintln!("stopped early: {stopped}");
    }
    Ok(if summary.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_outcome(outcome: &SessionOutcome) {
    println!("{}", terminal_safe(&outcome.final_answer));
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
    if let Some(check) = &outcome.check {
        println!("check: {}", terminal_safe(&check.to_string()));
    }
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

    fn model_args(provider: &str, base_url: Option<&str>) -> ModelArgs {
        let mut argv = vec![
            "kiln-harness",
            "run",
            "--provider",
            provider,
            "--prompt",
            "x",
        ];
        if let Some(base_url) = base_url {
            argv.extend(["--base-url", base_url]);
        }
        match Cli::parse_from(argv).command {
            Command::Run(run) => run.model,
            _ => panic!("parsed as run"),
        }
    }

    #[test]
    fn the_manifest_keeps_only_the_base_url_host() {
        let args = model_args(
            "openai-compat",
            Some("http://user:secret@models.example.com:8443/v1/chat?key=sk-123#frag"),
        );
        assert_eq!(base_url_host(&args).as_deref(), Some("models.example.com"));
        let args = model_args("anthropic", None);
        assert_eq!(base_url_host(&args).as_deref(), Some("api.anthropic.com"));
        let args = model_args("openai-compat", Some("not a url"));
        assert_eq!(base_url_host(&args), None);
    }

    #[test]
    fn truncation_recoveries_default_to_two_and_reach_the_engine() {
        let defaults = model_args("anthropic", None);
        assert_eq!(defaults.max_truncation_recoveries, 2);
        assert_eq!(
            model_config(&defaults)
                .expect("config")
                .max_truncation_recoveries,
            2
        );
        let parse = |command: &str, value: &str| {
            let mut argv = vec!["kiln-harness", command];
            if command == "eval" {
                argv.extend(["run", "--out", "out"]);
            } else {
                argv.extend(["--prompt", "x"]);
            }
            argv.extend([
                "--provider",
                "anthropic",
                "--max-truncation-recoveries",
                value,
            ]);
            match Cli::parse_from(argv).command {
                Command::Run(run) => run.model,
                Command::Eval {
                    command: EvalCommand::Run(run),
                } => run.model,
                _ => panic!("parsed as run or eval run"),
            }
        };
        for command in ["run", "eval"] {
            for value in [0, 5, MAX_TRUNCATION_RECOVERIES] {
                let config = model_config(&parse(command, &value.to_string())).expect("valid");
                assert_eq!(config.max_truncation_recoveries, value, "{command}");
                assert_eq!(config.limits().max_truncation_recoveries, value);
            }
            let error = model_config(&parse(command, "17")).expect_err("over the ceiling");
            assert_eq!(
                error.to_string(),
                "--max-truncation-recoveries must be between 0 and 16"
            );
        }
    }

    #[test]
    fn the_tool_set_flag_reaches_the_config_and_the_default_prompt() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv).map(|cli| cli.command) {
            Ok(Command::Run(run)) => Ok(run),
            Ok(_) => panic!("parsed as run"),
            Err(error) => Err(error),
        };
        let base = [
            "kiln-harness",
            "run",
            "--provider",
            "anthropic",
            "--base-url",
            "http://127.0.0.1:8788",
            "--prompt",
            "x",
        ];
        let run = parse(&base).expect("parses");
        assert_eq!(
            model_config(&run.model).expect("config").tool_set,
            ToolSet::Base
        );
        let prepared = prepare_run(&run).expect("prepared");
        assert_eq!(
            prepared.task.system,
            default_system_prompt_for(&prepared.config.tool_limits, ToolSet::Base)
        );
        let mut coding = base.to_vec();
        coding.extend(["--tool-set", "coding"]);
        let run = parse(&coding).expect("parses");
        let prepared = prepare_run(&run).expect("prepared");
        assert_eq!(prepared.config.tool_set, ToolSet::CodingV1);
        assert!(prepared.task.system.contains("- edit_file: "));
        // A system file still wins.
        let dir = private_tempdir().expect("temp dir");
        let system = dir.path().join("system.md");
        std::fs::write(&system, "Mine.").expect("write");
        let mut with_file = coding.clone();
        with_file.extend(["--system-file", system.to_str().expect("utf-8")]);
        let run = parse(&with_file).expect("parses");
        assert_eq!(prepare_run(&run).expect("prepared").task.system, "Mine.");
        let mut wrong = base.to_vec();
        wrong.extend(["--tool-set", "coding_v1"]);
        assert!(parse(&wrong).is_err());
    }

    fn run_model_args(extra: &[&str]) -> ModelArgs {
        let mut argv = vec!["kiln-harness", "run", "--prompt", "x"];
        argv.extend(extra);
        match Cli::parse_from(argv).command {
            Command::Run(run) => run.model,
            _ => panic!("parsed as run"),
        }
    }

    #[test]
    fn eval_runs_are_priced_only_at_a_listed_model_without_fallbacks() {
        let pricing = |extra: &[&str]| {
            let (pricing, reason) = suite_pricing(&run_model_args(extra));
            (pricing.map(|pricing| pricing.model), reason)
        };
        assert_eq!(
            pricing(&["--provider", "anthropic"]),
            (Some("claude-opus-5-5"), None)
        );
        assert_eq!(
            pricing(&["--provider", "anthropic", "--model", "claude-haiku-5-5"]),
            (Some("claude-haiku-5-5"), None)
        );
        assert_eq!(
            pricing(&["--provider", "anthropic", "--model", "claude-haiku-4-5"]),
            (
                None,
                Some("no price table for claude-haiku-4-5".to_string())
            )
        );
        assert_eq!(
            pricing(&["--provider", "anthropic", "--fallbacks"]),
            (None, Some("--fallbacks may bill another model".to_string()))
        );
        // A served model's name does not make openai-compat priced.
        let compat = [
            "--provider",
            "openai-compat",
            "--base-url",
            "http://127.0.0.1:8080/v1",
            "--model",
            "claude-haiku-5-5",
        ];
        assert_eq!(
            pricing(&compat),
            (None, Some("openai-compat has no price table".to_string()))
        );
        assert_eq!(model_metadata(&run_model_args(&compat)), None);
        // The output limit still holds with --fallbacks: it is the
        // requested model's.
        let error = model_config(&run_model_args(&[
            "--provider",
            "anthropic",
            "--fallbacks",
            "--max-output-tokens",
            "128001",
        ]))
        .expect_err("over the model's limit");
        assert_eq!(
            error.to_string(),
            "--max-output-tokens 128001 exceeds claude-opus-5-5's 128000-token output limit"
        );
    }

    #[test]
    fn final_answers_cannot_drive_the_terminal() {
        let answer = "done\n\tok \u{1b}[2J\u{1b}]0;title\u{7}\r\u{7f}\u{9b}31m \u{e9}";
        assert_eq!(
            terminal_safe(answer),
            "done\n\tok \\u{1b}[2J\\u{1b}]0;title\\u{7}\\u{d}\\u{7f}\\u{9b}31m \u{e9}"
        );
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
        let journal_path = temp.path().join("new/replay.jsonl");

        let error = open_sinks(Some(&source), &audit_path, &journal_path)
            .err()
            .expect("aliased outputs are refused");

        assert!(error.to_string().contains("--from and --audit"), "{error}");
        assert_eq!(std::fs::read(&source_path).expect("read"), b"recorded\n");
        assert!(audit_path.exists(), "a pre-existing file is never removed");
        assert!(!journal_path.exists(), "the new empty journal is removed");
        assert!(
            !temp.path().join("new").exists(),
            "and so is the directory created for it"
        );
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
