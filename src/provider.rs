use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
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
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
    prompt_preamble: Option<PromptPreamble>,
    extra_args: Vec<String>,
) -> Arc<dyn Provider> {
    match kind {
        ProviderKind::Codex => Arc::new(CodexProvider {
            model,
            run_logger,
            progress,
            prompt_preamble,
            extra_args,
        }),
        ProviderKind::Claude => Arc::new(ClaudeProvider {
            model,
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
        let prompt_path = self
            .run_logger
            .write_prompt(&invocation, "codex", &args, cwd, schema, &prompt)
            .await?;
        let agent = self
            .progress
            .begin_agent(render_agent_label(label, &prompt_path));

        let output = capture_command_with_input("codex", &args, cwd, Some(&prompt))
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
        let base_prompt = self.materialize_prompt(prompt);
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
            "--add-dir".to_string(),
            self.run_logger.root().display().to_string(),
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
        let structured_output_path =
            self.run_logger
                .artifact_path(&invocation, "structured-response", "json");
        let prompt = add_json_output_contract(&base_prompt, &structured_output_path);
        let prompt_path = self
            .run_logger
            .write_prompt(&invocation, "claude", &args, cwd, schema, &prompt)
            .await?;
        let agent = self
            .progress
            .begin_agent(render_agent_label(label, &prompt_path));

        let output = capture_command_with_input("claude", &args, cwd, Some(&prompt))
            .await
            .context("claude invocation failed")?;

        let mut parsed = None::<Value>;
        let mut error = None::<String>;
        let mut raw_response = output.stdout.clone();

        if output.success {
            if let Some(file_body) = tokio::fs::read_to_string(&structured_output_path)
                .await
                .ok()
            {
                raw_response = file_body.clone();
                match serde_json::from_str::<Value>(&file_body) {
                    Ok(value) => parsed = Some(value),
                    Err(parse_error) => {
                        error = Some(format!(
                            "failed to parse structured output file {} as JSON: {parse_error}",
                            structured_output_path.display()
                        ));
                    }
                }
            }

            if parsed.is_none() {
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
                                Ok(value) => {
                                    parsed = Some(value);
                                    error = None;
                                }
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
                &raw_response,
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

fn render_agent_label(label: &str, prompt_path: &Path) -> String {
    format!("{label} ({})", prompt_path.display())
}

fn add_json_output_contract(prompt: &str, output_path: &Path) -> String {
    format!(
        "{prompt}\n\n\
         Structured output contract:\n\
         - Write the final result JSON to this exact path: `{path}`\n\
         - The directory already exists and is writable.\n\
         - The file must contain a single raw JSON value matching the required schema exactly.\n\
         - Do not wrap the file contents in markdown fences.\n\
         - After writing the file, print only a short confirmation such as `WROTE_JSON_FILE: {path}`.\n\
         - If the file write fails, print the raw JSON directly with no prose before or after it.\n",
        path = output_path.display()
    )
}

fn extract_claude_payload(raw: Value) -> Result<Value> {
    match raw.get("result") {
        Some(Value::String(value)) => extract_json_from_text(value),
        Some(Value::Object(_)) | Some(Value::Array(_)) => Ok(raw["result"].clone()),
        Some(_) => bail!("unsupported claude result payload"),
        None => Ok(raw),
    }
}

fn extract_json_from_text(value: &str) -> Result<Value> {
    let trimmed = value.trim();
    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
        return Ok(parsed);
    }

    if let Some(fenced) = extract_fenced_json_block(trimmed) {
        if let Ok(parsed) = serde_json::from_str::<Value>(fenced) {
            return Ok(parsed);
        }
    }

    extract_first_json_value(trimmed)
}

fn extract_fenced_json_block(value: &str) -> Option<&str> {
    for marker in ["```json", "```JSON", "```"] {
        let Some(start) = value.find(marker) else {
            continue;
        };
        let remaining = &value[start + marker.len()..];
        let Some(end) = remaining.find("```") else {
            continue;
        };
        let block = remaining[..end].trim();
        if !block.is_empty() {
            return Some(block);
        }
    }
    None
}

fn extract_first_json_value(value: &str) -> Result<Value> {
    for (index, ch) in value.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }

        let mut deserializer = serde_json::Deserializer::from_str(&value[index..]);
        if let Ok(parsed) = Value::deserialize(&mut deserializer) {
            return Ok(parsed);
        }
    }

    bail!("claude result was not valid JSON")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{add_json_output_contract, extract_claude_payload, render_agent_label};

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

    #[test]
    fn renders_agent_label_with_prompt_path() {
        let label = render_agent_label("build repo 180747", Path::new("/tmp/run_x/prompt.txt"));
        assert_eq!(label, "build repo 180747 (/tmp/run_x/prompt.txt)");
    }

    #[test]
    fn extracts_fenced_json_payload() {
        let raw = json!({
            "type": "result",
            "result": "some explanation\n\n```json\n{\"ok\":true}\n```"
        });

        let parsed = extract_claude_payload(raw).expect("payload should parse");
        assert_eq!(parsed, json!({"ok": true}));
    }

    #[test]
    fn adds_structured_output_contract() {
        let prompt = add_json_output_contract("Review this PR.", Path::new("/tmp/run_x/out.json"));
        assert!(prompt.contains("/tmp/run_x/out.json"));
        assert!(prompt.contains("Structured output contract"));
    }
}

#[allow(dead_code)]
pub async fn check_codex_login(cwd: &Path) -> Result<String> {
    let args = vec!["login".to_string(), "status".to_string()];
    Ok(run_command("codex", &args, cwd).await?.stdout)
}

#[allow(dead_code)]
pub async fn check_claude_login(cwd: &Path) -> Result<String> {
    let args = vec!["auth".to_string(), "status".to_string()];
    Ok(run_command("claude", &args, cwd).await?.stdout)
}
