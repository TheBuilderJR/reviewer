use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug)]
pub struct RunLogger {
    root: PathBuf,
    sequence: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct InvocationLog {
    timestamp_secs: u64,
    sequence: u64,
    metadata: String,
}

impl RunLogger {
    pub async fn create() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("run_{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.with_context(|| {
            format!("failed to create run artifact directory {}", root.display())
        })?;

        Ok(Self {
            root,
            sequence: AtomicU64::new(1),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn begin(&self, metadata: &str) -> InvocationLog {
        InvocationLog {
            timestamp_secs: unix_timestamp_secs(),
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            metadata: sanitize_metadata(metadata),
        }
    }

    pub async fn write_prompt(
        &self,
        invocation: &InvocationLog,
        provider: &str,
        args: &[String],
        cwd: &Path,
        schema: &Value,
        prompt: &str,
    ) -> Result<PathBuf> {
        let body = format!(
            "provider: {provider}\n\
             args_json: {args_json}\n\
             cwd: {cwd}\n\
             metadata: {metadata}\n\n\
             schema:\n{schema}\n\n\
             prompt:\n{prompt}\n",
            args_json = serde_json::to_string_pretty(args)?,
            cwd = cwd.display(),
            metadata = invocation.metadata,
            schema = serde_json::to_string_pretty(schema)?
        );
        self.write_stage(invocation, "initial-prompt", &body).await
    }

    pub async fn write_response(
        &self,
        invocation: &InvocationLog,
        provider: &str,
        args: &[String],
        cwd: &Path,
        raw_response: &str,
        stdout: &str,
        stderr: &str,
        parsed_json: Option<&Value>,
        error: Option<&str>,
    ) -> Result<PathBuf> {
        let parsed_text = match parsed_json {
            Some(value) => serde_json::to_string_pretty(value)?,
            None => String::new(),
        };
        let error_text = error.unwrap_or("");

        let body = format!(
            "provider: {provider}\n\
             args_json: {args_json}\n\
             cwd: {cwd}\n\
             metadata: {metadata}\n\
             error: {error_text}\n\n\
             raw_response:\n{raw_response}\n\n\
             stdout:\n{stdout}\n\n\
             stderr:\n{stderr}\n\n\
             parsed_json:\n{parsed_text}\n",
            args_json = serde_json::to_string_pretty(args)?,
            cwd = cwd.display(),
            metadata = invocation.metadata,
        );

        self.write_stage(invocation, "response", &body).await
    }

    pub async fn write_text(
        &self,
        invocation: &InvocationLog,
        run_type: &str,
        body: &str,
    ) -> Result<PathBuf> {
        self.write_stage(invocation, run_type, body).await
    }

    pub fn artifact_path(
        &self,
        invocation: &InvocationLog,
        run_type: &str,
        extension: &str,
    ) -> PathBuf {
        let extension = extension.trim_start_matches('.');
        self.root.join(format!(
            "{}_{}_{}_{}.{}",
            invocation.timestamp_secs,
            run_type,
            invocation.metadata,
            invocation.sequence,
            extension
        ))
    }

    async fn write_stage(
        &self,
        invocation: &InvocationLog,
        run_type: &str,
        body: &str,
    ) -> Result<PathBuf> {
        let path = self.artifact_path(invocation, run_type, "txt");
        tokio::fs::write(&path, body)
            .await
            .with_context(|| format!("failed writing run artifact {}", path.display()))?;
        Ok(path)
    }
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sanitize_metadata(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch.to_ascii_lowercase(),
            '-' | '_' => '-',
            _ => '-',
        })
        .collect();

    let collapsed = sanitized
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    let truncated: String = collapsed.chars().take(96).collect();
    let base = if truncated.is_empty() {
        "run".to_string()
    } else {
        truncated
    };

    format!("{base}-{:08x}", short_hash(value))
}

fn short_hash(value: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    (hasher.finish() & 0xffff_ffff) as u32
}

#[cfg(test)]
mod tests {
    use super::sanitize_metadata;

    #[test]
    fn sanitizes_and_hashes_metadata() {
        let value = sanitize_metadata("review_src/foo.bar");
        assert!(value.starts_with("review-src-foo-bar-"));
        assert!(value.len() > "review-src-foo-bar-".len());
    }
}
