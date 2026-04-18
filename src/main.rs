mod git;
mod github;
mod progress;
mod provider;
mod review;
mod runlog;
mod shell;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use progress::ProgressReporter;
use provider::{PromptPreamble, Provider, build_provider};
use review::{ReviewOptions, render_markdown, run_review};
use runlog::RunLogger;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderKind {
    Codex,
    Claude,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codex => write!(f, "codex"),
            Self::Claude => write!(f, "claude"),
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "reviewer",
    about = "Worktree-based PR review harness that shells out to Codex or Claude."
)]
struct Args {
    #[arg(long, value_enum)]
    provider: ProviderKind,

    #[arg(long)]
    pr: u64,

    #[arg(long, default_value = ".")]
    repo_path: PathBuf,

    #[arg(long)]
    repo: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    extra_args: Option<String>,

    #[arg(long, default_value_t = 3)]
    max_commits_per_file: usize,

    #[arg(long, default_value_t = 2)]
    max_prs_per_file: usize,

    #[arg(long, default_value_t = 30)]
    pr_scan_limit: usize,

    #[arg(long, default_value_t = 4)]
    parallelism: usize,

    #[arg(long, default_value_t = 600)]
    agent_timeout_secs: u64,

    #[arg(long)]
    keep_worktree: bool,

    #[arg(long)]
    output_markdown: Option<PathBuf>,

    #[arg(long)]
    output_json: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let progress = Arc::new(ProgressReporter::new());
    let repo_path = args
        .repo_path
        .canonicalize()
        .with_context(|| format!("failed to resolve repo path {}", args.repo_path.display()))?;
    let run_logger = Arc::new(RunLogger::create().await?);
    let prompt_preamble = load_prompt_preamble().await?;
    let extra_args = parse_extra_args(args.extra_args.as_deref())?;

    progress.info(
        "run",
        format!("artifacts -> {}", run_logger.root().display()),
    );
    progress.info(
        "run",
        format!(
            "provider={} pr={} repo_path={}",
            args.provider,
            args.pr,
            repo_path.display()
        ),
    );

    match &prompt_preamble {
        Some(preamble) => progress.info(
            "config",
            format!(
                "loaded reviewer instructions from {}",
                preamble.path.display()
            ),
        ),
        None => progress.info(
            "config",
            "no ~/.reviewer.md found; using built-in prompts only",
        ),
    }

    if extra_args.is_empty() {
        progress.info("config", "no provider extra args configured");
    } else {
        progress.info(
            "config",
            format!("provider extra args: {}", extra_args.join(" ")),
        );
    }

    let provider = build_provider(
        args.provider.into(),
        args.model.clone(),
        args.agent_timeout_secs,
        run_logger.clone(),
        progress.clone(),
        prompt_preamble,
        extra_args,
    );

    let repo_name = match &args.repo {
        Some(repo) => repo.clone(),
        None => github::resolve_repo_name(&repo_path).await?,
    };

    let options = ReviewOptions {
        pr_number: args.pr,
        repo_name,
        repo_path,
        max_commits_per_file: args.max_commits_per_file,
        max_prs_per_file: args.max_prs_per_file,
        pr_scan_limit: args.pr_scan_limit,
        parallelism: args.parallelism.max(1),
        keep_worktree: args.keep_worktree,
    };

    let report = match run_review(
        options,
        provider.clone(),
        run_logger.clone(),
        progress.clone(),
    )
    .await
    {
        Ok(report) => report,
        Err(error) => {
            progress.info("run", format!("failed: {error}"));
            eprintln!("Run artifacts: {}", run_logger.root().display());
            return Err(error);
        }
    };
    let markdown = render_markdown(&report);

    if let Some(path) = args.output_json {
        let json = serde_json::to_string_pretty(&report)?;
        tokio::fs::write(&path, json)
            .await
            .with_context(|| format!("failed writing {}", path.display()))?;
    }

    if let Some(path) = args.output_markdown {
        tokio::fs::write(&path, &markdown)
            .await
            .with_context(|| format!("failed writing {}", path.display()))?;
    }

    println!("{markdown}");
    println!("Run artifacts: {}", run_logger.root().display());
    progress.info("run", "completed successfully");
    Ok(())
}

impl From<ProviderKind> for provider::ProviderKind {
    fn from(value: ProviderKind) -> Self {
        match value {
            ProviderKind::Codex => provider::ProviderKind::Codex,
            ProviderKind::Claude => provider::ProviderKind::Claude,
        }
    }
}

#[allow(dead_code)]
fn _provider_name(provider: &Arc<dyn Provider>) -> &str {
    provider.kind().as_str()
}

async fn load_prompt_preamble() -> Result<Option<PromptPreamble>> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };

    let path = PathBuf::from(home).join(".reviewer.md");
    if !path.exists() {
        return Ok(None);
    }

    let content = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed reading {}", path.display()))?;

    Ok(Some(PromptPreamble { path, content }))
}

fn parse_extra_args(value: Option<&str>) -> Result<Vec<String>> {
    match value {
        Some(raw) => {
            shlex::split(raw).with_context(|| format!("failed to parse --extra-args value: {raw}"))
        }
        None => Ok(Vec::new()),
    }
}
