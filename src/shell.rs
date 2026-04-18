use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub status_code: Option<i32>,
    pub success: bool,
}

pub async fn run_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_secs: u64,
) -> Result<CmdOutput> {
    run_command_with_input(program, args, cwd, None, timeout_secs).await
}

pub async fn run_command_with_input(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    timeout_secs: u64,
) -> Result<CmdOutput> {
    let output = capture_command_with_input(program, args, cwd, stdin_text, timeout_secs).await?;

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

pub async fn capture_command_with_input(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    timeout_secs: u64,
) -> Result<CmdOutput> {
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

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    if let Some(input) = stdin_text {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("stdin unavailable for {program}"))?;
        stdin
            .write_all(input.as_bytes())
            .await
            .with_context(|| format!("failed writing stdin to {program}"))?;
        stdin
            .shutdown()
            .await
            .with_context(|| format!("failed closing stdin for {program}"))?;
    }

    let output = timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .with_context(|| format!("{program} timed out after {timeout_secs}s"))?
        .with_context(|| format!("failed waiting for {program}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(CmdOutput {
        stdout,
        stderr,
        status_code: output.status.code(),
        success: output.status.success(),
    })
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
