//! Providers backed by tools already installed and authenticated on the local
//! machine. Kuali can reuse an existing Claude Code session without requesting
//! another key.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::confine;
use crate::provider::{CompletionRequest, LlmError, LlmProvider, ProviderInfo, ProviderKind};

/// An executable and the complete search path it needs when launched from a
/// desktop process that did not inherit the user's shell environment.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedCommand {
    name: String,
    executable: PathBuf,
    search_path: OsString,
}

impl ResolvedCommand {
    /// The command with an emptied environment, holding only what the CLI needs
    /// to authenticate and reach the network. See [`crate::confine`].
    pub(crate) fn process(&self) -> Command {
        let mut command = Command::new(&self.executable);
        confine::apply_environment(&mut command, &self.search_path);
        command
    }

    /// The same command under a kernel sandbox, or an error when this platform
    /// has none to offer.
    fn confined(&self) -> Result<(Command, confine::Guard), LlmError> {
        confine::launch(&self.name, &self.executable, &self.search_path).map_err(|message| {
            LlmError::Provider {
                provider: self.name.clone(),
                message,
            }
        })
    }

    pub(crate) fn label(&self) -> String {
        self.executable.display().to_string()
    }
}

/// Resolves CLIs from both the inherited `PATH` and locations used by package
/// and Node version managers. Finder and the Dock provide only a minimal PATH.
pub(crate) fn resolve_command(program: &str) -> Option<ResolvedCommand> {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    resolve_from(
        program,
        std::env::var_os("PATH").as_deref(),
        home.as_deref(),
    )
}

fn resolve_from(
    program: &str,
    inherited_path: Option<&OsStr>,
    home: Option<&Path>,
) -> Option<ResolvedCommand> {
    let directories = search_directories(inherited_path, home);
    let executable = directories
        .iter()
        .flat_map(|directory| executable_candidates(directory, program))
        .find(|candidate| is_executable(candidate))?;
    let search_path = std::env::join_paths(&directories).ok()?;
    Some(ResolvedCommand {
        name: program.to_string(),
        executable,
        search_path,
    })
}

fn search_directories(inherited_path: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(path) = inherited_path {
        for directory in std::env::split_paths(path) {
            push_unique(&mut directories, directory);
        }
    }

    if let Some(home) = home {
        for relative in [
            ".local/bin",
            ".cargo/bin",
            ".bun/bin",
            ".volta/bin",
            ".asdf/shims",
            ".local/share/mise/shims",
            ".local/share/pnpm",
            ".npm-global/bin",
            "Library/pnpm",
            ".local/share/fnm/aliases/default/bin",
            "Library/Application Support/fnm/aliases/default/bin",
        ] {
            push_unique(&mut directories, home.join(relative));
        }
        append_versioned_bins(&mut directories, &home.join(".nvm/versions/node"), "bin");
        append_versioned_bins(
            &mut directories,
            &home.join(".local/share/fnm/node-versions"),
            "installation/bin",
        );
        append_versioned_bins(
            &mut directories,
            &home.join("Library/Application Support/fnm/node-versions"),
            "installation/bin",
        );
    }

    for directory in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        push_unique(&mut directories, PathBuf::from(directory));
    }
    directories
}

fn push_unique(directories: &mut Vec<PathBuf>, directory: PathBuf) {
    if directory.is_absolute() && !directories.contains(&directory) {
        directories.push(directory);
    }
}

fn append_versioned_bins(directories: &mut Vec<PathBuf>, root: &Path, suffix: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut versions: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    versions.sort_by_key(|path| std::cmp::Reverse(version_key(path)));
    for version in versions {
        push_unique(directories, version.join(suffix));
    }
}

fn version_key(path: &Path) -> Vec<u64> {
    path.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or_default())
        .collect()
}

#[cfg(not(windows))]
fn executable_candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
    vec![directory.join(program)]
}

#[cfg(windows)]
fn executable_candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
    let path = Path::new(program);
    if path.extension().is_some() {
        return vec![directory.join(path)];
    }
    std::env::var_os("PATHEXT")
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .to_string_lossy()
        .split(';')
        .map(|extension| directory.join(format!("{program}{extension}")))
        .collect()
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
async fn run(
    program: &ResolvedCommand,
    args: &[String],
    stdin_text: &str,
) -> Result<String, LlmError> {
    // Launch from a neutral directory so tools that discover CLAUDE.md, Git, or
    // project configuration cannot inherit unrelated user-project context.
    let cwd = std::env::temp_dir();

    // The profile has to outlive the process that is running under it.
    let (mut command, _profile) = program.confined()?;
    let mut child = command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| LlmError::Spawn {
            command: program.label(),
            source,
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_text.as_bytes())
            .await
            .map_err(|source| LlmError::Spawn {
                command: program.label(),
                source,
            })?;
        stdin.shutdown().await.ok();
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|source| LlmError::Spawn {
            command: program.label(),
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
            provider: program.name.clone(),
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
        resolve_command("claude").is_some()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let mut args: Vec<String> = [
            "--print",
            "--output-format",
            "json",
            "--model",
            &self.model,
            "--system-prompt",
            &request.system,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(confine::tool_restrictions("claude"));

        let command =
            resolve_command("claude").ok_or_else(|| LlmError::Unavailable("claude-cli".into()))?;
        let stdout = run(&command, &args, &request.prompt).await?;

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
        resolve_command("codex").is_some()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let command =
            resolve_command("codex").ok_or_else(|| LlmError::Unavailable("codex-cli".into()))?;
        // Unique temporary names support concurrent summaries.
        let stamp = uuid::Uuid::new_v4();
        let answer_path = std::env::temp_dir().join(format!("kuali-codex-{stamp}.txt"));
        let schema_path = std::env::temp_dir().join(format!("kuali-codex-{stamp}.schema.json"));

        let mut args = vec![
            "exec".to_string(),
            // A neutral directory is intentionally not a repository, so bypass
            // the trusted-repository check explicitly.
            "--skip-git-repo-check".to_string(),
            // Request clean output without Codex's banner and token counter.
            "--output-last-message".to_string(),
            answer_path.display().to_string(),
        ];
        args.extend(confine::tool_restrictions("codex"));

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
        let stdout = run(&command, &args, &prompt).await;

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

/// Gemini CLI. Unlike the others, its flags come from the published reference
/// rather than a binary Kuali has run, and it offers no way to drop its tools
/// from the command line — the kernel sandbox is what confines it.
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
        resolve_command("gemini").is_some()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let mut args = confine::tool_restrictions("gemini");
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        let prompt = format!("{}\n\n---\n\n{}", request.system, request.prompt);
        let command =
            resolve_command("gemini").ok_or_else(|| LlmError::Unavailable("gemini-cli".into()))?;
        run(&command, &args, &prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing reaches a transcript unconfined. A provider whose executable is
    /// missing from the confinement table cannot be launched at all, which is
    /// what keeps a fourth CLI from arriving with the hardening left out.
    #[tokio::test]
    async fn a_cli_kuali_cannot_confine_is_refused_before_it_runs() {
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let command = resolve_command(shell).unwrap();
        let error = run(&command, &[], "hola").await.unwrap_err();
        assert!(
            error.to_string().contains(shell),
            "se ejecutó una CLI sin confinar: {error}"
        );
    }

    /// Confinement is worthless if it also breaks the provider. Needs an
    /// authenticated `claude` on this machine, so it is not part of the suite.
    #[tokio::test]
    #[ignore = "requiere una sesión de Claude Code iniciada"]
    async fn a_confined_claude_still_answers() {
        let provider = ClaudeCliProvider::new(None);
        let answer = provider
            .complete(&CompletionRequest {
                system: "Responde solo con la palabra que se te pida.".into(),
                prompt: "Di exactamente: listo".into(),
                json_schema: None,
                max_tokens: 64,
            })
            .await
            .unwrap();
        assert!(answer.to_lowercase().contains("listo"), "{answer}");
    }

    #[test]
    fn every_cli_provider_asks_its_binary_to_refuse_tools() {
        for program in ["claude", "codex", "gemini"] {
            assert!(
                !confine::tool_restrictions(program).is_empty(),
                "`{program}` se lanzaría sin restricciones"
            );
        }
    }

    #[test]
    fn a_binary_that_cannot_exist_is_not_found() {
        assert!(resolve_command("kuali-no-existe-de-verdad-42").is_none());
    }

    #[test]
    fn a_binary_that_always_exists_is_found() {
        assert!(resolve_command(if cfg!(windows) { "cmd" } else { "sh" }).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dock_style_path_finds_user_and_nvm_commands_and_passes_their_environment() {
        use std::os::unix::fs::PermissionsExt;

        let home = std::env::temp_dir().join(format!("kuali-cli-path-{}", uuid::Uuid::new_v4()));
        let local_bin = home.join(".local/bin");
        let nvm_bin = home.join(".nvm/versions/node/v25.7.0/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::create_dir_all(&nvm_bin).unwrap();
        for command in [local_bin.join("codex"), nvm_bin.join("claude")] {
            std::fs::write(&command, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let minimal_path = OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin");
        let codex = resolve_from("codex", Some(minimal_path), Some(&home)).unwrap();
        let claude = resolve_from("claude", Some(minimal_path), Some(&home)).unwrap();
        assert_eq!(codex.executable, local_bin.join("codex"));
        assert_eq!(claude.executable, nvm_bin.join("claude"));
        assert!(std::env::split_paths(&claude.search_path).any(|path| path == nvm_bin));
        let output = claude.process().output().await.unwrap();
        assert!(output.status.success());

        std::fs::remove_dir_all(home).unwrap();
    }
}
