//! Providers backed by tools already installed and authenticated on the local
//! machine. Kuali can reuse an existing Claude Code session without requesting
//! another key.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::provider::{CompletionRequest, LlmError, LlmProvider, ProviderInfo, ProviderKind};

/// Checks whether an executable is on `PATH` without spawning `which` for every probe.
pub fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Runs a binary with the prompt on stdin. Multi-hour transcripts can exceed
/// argument-size limits, making argv unsuitable.
async fn run(program: &str, args: &[String], stdin_text: &str) -> Result<String, LlmError> {
    // Launch from a neutral directory so tools that discover CLAUDE.md, Git, or
    // project configuration cannot inherit unrelated user-project context.
    let cwd = std::env::temp_dir();

    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| LlmError::Spawn {
            command: program.to_string(),
            source,
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_text.as_bytes())
            .await
            .map_err(|source| LlmError::Spawn {
                command: program.to_string(),
                source,
            })?;
        stdin.shutdown().await.ok();
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|source| LlmError::Spawn {
            command: program.to_string(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if stderr.trim().is_empty() {
            format!("terminó con {}", output.status)
        } else {
            stderr.trim().to_string()
        };
        return Err(LlmError::Provider {
            provider: program.to_string(),
            message,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

/// Uses the authenticated `claude` CLI session without `ANTHROPIC_API_KEY`.
pub struct ClaudeCliProvider {
    model: String,
}

impl ClaudeCliProvider {
    /// `sonnet` suits extraction-focused summaries and responds quickly.
    pub const DEFAULT_MODEL: &'static str = "sonnet";

    pub fn new(model: Option<String>) -> Self {
        Self {
            model: model.unwrap_or_else(|| Self::DEFAULT_MODEL.to_string()),
        }
    }
}

#[async_trait]
impl LlmProvider for ClaudeCliProvider {
    fn id(&self) -> &'static str {
        "claude-cli"
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.id().to_string(),
            label: "Claude Code (sesión local)".to_string(),
            model: self.model.clone(),
            kind: ProviderKind::LocalCli,
            structured_output: false,
        }
    }

    async fn is_available(&self) -> bool {
        find_in_path("claude").is_some()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let args: Vec<String> = [
            "--print",
            "--output-format",
            "json",
            "--model",
            &self.model,
            "--system-prompt",
            &request.system,
            // Kuali only needs text. Disabling tools and MCP prevents filesystem,
            // network, or unrelated configured-server access.
            "--strict-mcp-config",
            "--disallowed-tools",
            "Bash,Edit,Write,Read,Glob,Grep,WebFetch,WebSearch,Task",
            // Never wait for interactive approval in this unattended process.
            "--permission-mode",
            "dontAsk",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let stdout = run("claude", &args, &request.prompt).await?;

        // `--output-format json` places text in `result`. Fall back to raw stdout
        // if the wrapper changes.
        let parsed: serde_json::Value = match serde_json::from_str(stdout.trim()) {
            Ok(v) => v,
            Err(_) => return Ok(stdout),
        };

        if parsed.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            return Err(LlmError::Provider {
                provider: "claude-cli".into(),
                message: parsed
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error sin detalle")
                    .to_string(),
            });
        }

        Ok(parsed
            .get("result")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or(stdout))
    }
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// OpenAI Codex CLI with flags verified against v0.146.
///
/// Codex requires a trusted-repository override, surrounds responses with a
/// banner and token count, and can follow a JSON schema supplied as a file.
pub struct CodexCliProvider {
    model: Option<String>,
}

impl CodexCliProvider {
    pub fn new(model: Option<String>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl LlmProvider for CodexCliProvider {
    fn id(&self) -> &'static str {
        "codex-cli"
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.id().to_string(),
            label: "Codex CLI (sesión local)".to_string(),
            model: self.model.clone().unwrap_or_else(|| "por defecto".into()),
            kind: ProviderKind::LocalCli,
            structured_output: true,
        }
    }

    async fn is_available(&self) -> bool {
        find_in_path("codex").is_some()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        // Unique temporary names support concurrent summaries.
        let stamp = uuid::Uuid::new_v4();
        let answer_path = std::env::temp_dir().join(format!("kuali-codex-{stamp}.txt"));
        let schema_path = std::env::temp_dir().join(format!("kuali-codex-{stamp}.schema.json"));

        let mut args = vec![
            "exec".to_string(),
            // A neutral directory is intentionally not a repository, so bypass
            // the trusted-repository check explicitly.
            "--skip-git-repo-check".to_string(),
            // Read-only mode prevents filesystem mutation when Kuali only needs text.
            "--sandbox".to_string(),
            "read-only".to_string(),
            // Request clean output without Codex's banner and token counter.
            "--output-last-message".to_string(),
            answer_path.display().to_string(),
        ];

        // Continue without a schema if the file cannot be written; the parser can
        // recover JSON surrounded by prose.
        let schema_written = request.json_schema.as_ref().is_some_and(|schema| {
            serde_json::to_string(schema)
                .ok()
                .and_then(|text| std::fs::write(&schema_path, text).ok())
                .is_some()
        });
        if schema_written {
            args.push("--output-schema".to_string());
            args.push(schema_path.display().to_string());
        }

        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }

        // Codex exposes no separate system prompt, so prepend it to the request.
        let prompt = format!("{}\n\n---\n\n{}", request.system, request.prompt);
        let stdout = run("codex", &args, &prompt).await;

        let answer = std::fs::read_to_string(&answer_path).ok();
        let _ = std::fs::remove_file(&answer_path);
        if schema_written {
            let _ = std::fs::remove_file(&schema_path);
        }

        let stdout = stdout?;
        // If the output file was not written, retain stdout and let parsing strip
        // the surrounding banner and counter.
        Ok(answer
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(stdout))
    }
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

/// Gemini CLI. Like Codex, this path has not been verified against the binary.
pub struct GeminiCliProvider {
    model: Option<String>,
}

impl GeminiCliProvider {
    pub fn new(model: Option<String>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl LlmProvider for GeminiCliProvider {
    fn id(&self) -> &'static str {
        "gemini-cli"
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.id().to_string(),
            label: "Gemini CLI (sesión local)".to_string(),
            model: self.model.clone().unwrap_or_else(|| "por defecto".into()),
            kind: ProviderKind::LocalCli,
            structured_output: false,
        }
    }

    async fn is_available(&self) -> bool {
        find_in_path("gemini").is_some()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let mut args = vec![];
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        let prompt = format!("{}\n\n---\n\n{}", request.system, request.prompt);
        run("gemini", &args, &prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_that_cannot_exist_is_not_found() {
        assert!(find_in_path("kuali-no-existe-de-verdad-42").is_none());
    }

    #[test]
    fn a_binary_that_always_exists_is_found() {
        // `sh` is available on every supported Unix PATH.
        assert!(find_in_path("sh").is_some());
    }
}
