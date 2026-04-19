use std::sync::Arc;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::progress::ProgressReporter;

const COMMAND_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub status_code: Option<i32>,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct CommandProgress {
    reporter: Arc<ProgressReporter>,
    label: String,
}

impl CommandProgress {
    pub fn new(reporter: Arc<ProgressReporter>, label: impl Into<String>) -> Self {
        Self {
            reporter,
            label: label.into(),
        }
    }
}

pub async fn run_command(program: &str, args: &[String], cwd: &Path) -> Result<CmdOutput> {
    run_command_with_input_reported(program, args, cwd, None, None).await
}

pub async fn run_command_reported(
    program: &str,
    args: &[String],
    cwd: &Path,
    progress: CommandProgress,
) -> Result<CmdOutput> {
    run_command_with_input_reported(program, args, cwd, None, Some(progress)).await
}

pub async fn run_command_with_input_reported(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    progress: Option<CommandProgress>,
) -> Result<CmdOutput> {
    let output =
        capture_command_with_input_reported(program, args, cwd, stdin_text, progress).await?;

    if !output.success {
        bail!(
            "{program} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status_code,
            trim_for_error(&output.stdout),
            trim_for_error(&output.stderr)
        );
    }

    Ok(output)
}

pub async fn capture_command_with_input_reported(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    progress: Option<CommandProgress>,
) -> Result<CmdOutput> {
    let active = begin_command_progress(progress);

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {program}"));

    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
    };

    if let Some(input) = stdin_text {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("stdin unavailable for {program}"));
        let mut stdin = match stdin {
            Ok(stdin) => stdin,
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(error);
            }
        };
        let write_result = stdin
            .write_all(input.as_bytes())
            .await
            .with_context(|| format!("failed writing stdin to {program}"));
        if let Err(error) = write_result {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
        let shutdown_result = stdin
            .shutdown()
            .await
            .with_context(|| format!("failed closing stdin for {program}"));
        if let Err(error) = shutdown_result {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
    }

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("failed waiting for {program}"));
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let result = CmdOutput {
        stdout,
        stderr,
        status_code: output.status.code(),
        success: output.status.success(),
    };
    finish_command_result(active, &result);
    Ok(result)
}

fn trim_for_error(value: &str) -> String {
    let limit = 6_000;
    if value.chars().count() <= limit {
        return value.trim().to_string();
    }

    let head: String = value.chars().take(limit / 2).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(limit / 2)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    format!("{head}\n...\n{tail}")
}

struct ActiveCommand {
    progress: CommandProgress,
    started_at: Instant,
    ticker: tokio::task::JoinHandle<()>,
}

fn begin_command_progress(progress: Option<CommandProgress>) -> Option<ActiveCommand> {
    progress.map(|progress| {
        progress.reporter.command_start(&progress.label);
        let started_at = Instant::now();
        let heartbeat_started_at = started_at;
        let reporter = progress.reporter.clone();
        let label = progress.label.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(COMMAND_HEARTBEAT_INTERVAL).await;
                reporter.command_heartbeat(&label, heartbeat_started_at.elapsed().as_secs_f32());
            }
        });

        ActiveCommand {
            progress,
            started_at,
            ticker,
        }
    })
}

fn finish_command_result(active: Option<ActiveCommand>, output: &CmdOutput) {
    let Some(active) = active else {
        return;
    };
    active.ticker.abort();
    let elapsed_secs = active.started_at.elapsed().as_secs_f32();
    if output.success {
        active.progress.reporter.command_done(
            &active.progress.label,
            elapsed_secs,
            format!("exit {}", output.status_code.unwrap_or(0)),
        );
    } else {
        active.progress.reporter.command_fail(
            &active.progress.label,
            elapsed_secs,
            format!("exit {:?}", output.status_code),
        );
    }
}

fn finish_command_error(active: Option<ActiveCommand>, error: &str) {
    let Some(active) = active else {
        return;
    };
    active.ticker.abort();
    active.progress.reporter.command_fail(
        &active.progress.label,
        active.started_at.elapsed().as_secs_f32(),
        error,
    );
}
