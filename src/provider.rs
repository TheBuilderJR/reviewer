use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tempfile::tempdir;

use crate::progress::ProgressReporter;
use crate::runlog::RunLogger;
use crate::shell::{capture_command_with_input, run_command};

#[derive(Debug, Clone, Copy)]
pub enum ProviderKind {
    Codex,
    Claude,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn invoke(&self, cwd: &Path, schema: &Value, prompt: &str, label: &str) -> Result<Value>;
}

#[derive(Debug, Clone)]
pub struct PromptPreamble {
    pub path: std::path::PathBuf,
    pub content: String,
}

pub fn build_provider(
    kind: ProviderKind,
    model: Option<String>,
    timeout_secs: u64,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
    prompt_preamble: Option<PromptPreamble>,
    extra_args: Vec<String>,
) -> Arc<dyn Provider> {
    match kind {
        ProviderKind::Codex => Arc::new(CodexProvider {
            model,
            timeout_secs,
            run_logger,
            progress,
            prompt_preamble,
            extra_args,
        }),
        ProviderKind::Claude => Arc::new(ClaudeProvider {
            model,
            timeout_secs,
            run_logger,
            progress,
            prompt_preamble,
            extra_args,
        }),
    }
}

pub async fn invoke_typed<T>(
    provider: &dyn Provider,
    cwd: &Path,
    label: &str,
    prompt: &str,
) -> Result<T>
where
    T: DeserializeOwned + JsonSchema,
{
    let schema = serde_json::to_value(schemars::schema_for!(T))?;
    let value = provider.invoke(cwd, &schema, prompt, label).await?;
    serde_json::from_value(value).with_context(|| {
        format!(
            "failed to deserialize {} response",
            provider.kind().as_str()
        )
    })
}

struct CodexProvider {
    model: Option<String>,
    timeout_secs: u64,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
    prompt_preamble: Option<PromptPreamble>,
    extra_args: Vec<String>,
}

#[async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn invoke(&self, cwd: &Path, schema: &Value, prompt: &str, label: &str) -> Result<Value> {
        let prompt = self.materialize_prompt(prompt);
        let agent = self.progress.begin_agent(label);
        let temp = tempdir().context("failed to create temp dir for codex run")?;
        let schema_path = temp.path().join("schema.json");
        let output_path = temp.path().join("output.json");
        tokio::fs::write(&schema_path, serde_json::to_vec_pretty(schema)?)
            .await
            .context("failed to write codex schema")?;

        let mut args = self.extra_args.clone();
        args.extend(vec![
            "exec".to_string(),
            "--skip-git-repo-check".to_string(),
            "--ephemeral".to_string(),
            "--color".to_string(),
            "never".to_string(),
            "-C".to_string(),
            cwd.display().to_string(),
            "--output-schema".to_string(),
            schema_path.display().to_string(),
            "-o".to_string(),
            output_path.display().to_string(),
            "-".to_string(),
        ]);

        if let Some(model) = &self.model {
            let insert_at = self.extra_args.len() + 1;
            args.splice(
                insert_at..insert_at,
                ["--model".to_string(), model.clone()]
                    .into_iter()
                    .collect::<Vec<_>>(),
            );
        }

        let invocation = self.run_logger.begin(label);
        self.run_logger
            .write_prompt(&invocation, "codex", &args, cwd, schema, &prompt)
            .await?;

        let output =
            capture_command_with_input("codex", &args, cwd, Some(&prompt), self.timeout_secs)
                .await
                .context("codex invocation failed")?;

        let body = tokio::fs::read_to_string(&output_path)
            .await
            .unwrap_or_default();
        let mut parsed = None::<Value>;
        let mut error = None::<String>;

        if output.success {
            match serde_json::from_str::<Value>(&body) {
                Ok(value) => parsed = Some(value),
                Err(parse_error) => {
                    error = Some(format!(
                        "failed to parse codex output as JSON: {parse_error}"
                    ));
                }
            }
        } else {
            error = Some(format!("codex failed with status {:?}", output.status_code));
        }

        self.run_logger
            .write_response(
                &invocation,
                "codex",
                &args,
                cwd,
                &body,
                &output.stdout,
                &output.stderr,
                parsed.as_ref(),
                error.as_deref(),
            )
            .await?;

        if !output.success {
            agent.fail(format!("exit status {:?}", output.status_code));
            bail!(
                "codex failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status_code,
                output.stdout,
                output.stderr
            );
        }

        if let Some(error) = error {
            agent.fail(&error);
            bail!(
                "{error}\nstdout:\n{}\nstderr:\n{}",
                output.stdout,
                output.stderr
            );
        }

        let parsed = parsed.ok_or_else(|| {
            anyhow!(
                "failed to parse codex output as JSON\nstdout:\n{}\nstderr:\n{}",
                output.stdout,
                output.stderr
            )
        })?;

        agent.done();
        Ok(parsed)
    }
}

struct ClaudeProvider {
    model: Option<String>,
    timeout_secs: u64,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
    prompt_preamble: Option<PromptPreamble>,
    extra_args: Vec<String>,
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn invoke(&self, cwd: &Path, schema: &Value, prompt: &str, label: &str) -> Result<Value> {
        let prompt = self.materialize_prompt(prompt);
        let agent = self.progress.begin_agent(label);
        let mut args = self.extra_args.clone();
        args.extend(vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--json-schema".to_string(),
            serde_json::to_string(schema)?,
            "--permission-mode".to_string(),
            "dontAsk".to_string(),
            "--no-session-persistence".to_string(),
            "--add-dir".to_string(),
            cwd.display().to_string(),
            "-".to_string(),
        ]);

        if let Some(model) = &self.model {
            args.splice(
                self.extra_args.len()..self.extra_args.len(),
                ["--model".to_string(), model.clone()]
                    .into_iter()
                    .collect::<Vec<_>>(),
            );
        }

        let invocation = self.run_logger.begin(label);
        self.run_logger
            .write_prompt(&invocation, "claude", &args, cwd, schema, &prompt)
            .await?;

        let output =
            capture_command_with_input("claude", &args, cwd, Some(&prompt), self.timeout_secs)
                .await
                .context("claude invocation failed")?;

        let mut parsed = None::<Value>;
        let mut error = None::<String>;

        if output.success {
            match serde_json::from_str::<Value>(output.stdout.trim()) {
                Ok(raw) => {
                    if raw
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        let message = raw
                            .get("result")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown claude error");
                        error = Some(message.to_string());
                    } else {
                        match extract_claude_payload(raw) {
                            Ok(value) => parsed = Some(value),
                            Err(parse_error) => {
                                error = Some(format!(
                                    "failed to extract claude payload: {parse_error}"
                                ));
                            }
                        }
                    }
                }
                Err(parse_error) => {
                    error = Some(format!(
                        "failed to parse claude wrapper output: {parse_error}"
                    ));
                }
            }
        } else {
            error = Some(format!(
                "claude failed with status {:?}",
                output.status_code
            ));
        }

        self.run_logger
            .write_response(
                &invocation,
                "claude",
                &args,
                cwd,
                &output.stdout,
                &output.stdout,
                &output.stderr,
                parsed.as_ref(),
                error.as_deref(),
            )
            .await?;

        if !output.success {
            agent.fail(format!("exit status {:?}", output.status_code));
            bail!(
                "claude failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status_code,
                output.stdout,
                output.stderr
            );
        }

        if let Some(error) = error {
            agent.fail(&error);
            bail!(
                "{error}\nstdout:\n{}\nstderr:\n{}",
                output.stdout,
                output.stderr
            );
        }

        let parsed = parsed.ok_or_else(|| {
            anyhow!(
                "failed to parse claude wrapper output\nstdout:\n{}\nstderr:\n{}",
                output.stdout,
                output.stderr
            )
        })?;

        agent.done();
        Ok(parsed)
    }
}

impl CodexProvider {
    fn materialize_prompt(&self, prompt: &str) -> String {
        merge_prompt(self.prompt_preamble.as_ref(), prompt)
    }
}

impl ClaudeProvider {
    fn materialize_prompt(&self, prompt: &str) -> String {
        merge_prompt(self.prompt_preamble.as_ref(), prompt)
    }
}

fn merge_prompt(prompt_preamble: Option<&PromptPreamble>, prompt: &str) -> String {
    match prompt_preamble {
        Some(preamble) => format!(
            "Global reviewer instructions loaded from `{}`:\n\
             ```md\n{}\n```\n\n\
             Task:\n{}\n",
            preamble.path.display(),
            preamble.content.trim(),
            prompt
        ),
        None => prompt.to_string(),
    }
}

fn extract_claude_payload(raw: Value) -> Result<Value> {
    match raw.get("result") {
        Some(Value::String(value)) => {
            serde_json::from_str(value).with_context(|| anyhow!("claude result was not valid JSON"))
        }
        Some(Value::Object(_)) | Some(Value::Array(_)) => Ok(raw["result"].clone()),
        Some(_) => bail!("unsupported claude result payload"),
        None => Ok(raw),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::extract_claude_payload;

    #[test]
    fn extracts_json_string_payload() {
        let raw = json!({
            "type": "result",
            "result": "{\"ok\":true}"
        });

        let parsed = extract_claude_payload(raw).expect("payload should parse");
        assert_eq!(parsed, json!({"ok": true}));
    }

    #[test]
    fn extracts_object_payload() {
        let raw = json!({
            "result": {
                "ok": true
            }
        });

        let parsed = extract_claude_payload(raw).expect("payload should parse");
        assert_eq!(parsed, json!({"ok": true}));
    }
}

#[allow(dead_code)]
pub async fn check_codex_login(cwd: &Path) -> Result<String> {
    let args = vec!["login".to_string(), "status".to_string()];
    Ok(run_command("codex", &args, cwd, 30).await?.stdout)
}

#[allow(dead_code)]
pub async fn check_claude_login(cwd: &Path) -> Result<String> {
    let args = vec!["auth".to_string(), "status".to_string()];
    Ok(run_command("claude", &args, cwd, 30).await?.stdout)
}
