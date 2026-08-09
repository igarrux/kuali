//! Live model discovery from each provider.
//!
//! Hard-coded lists become stale as providers release and retire models. API
//! providers expose catalogs, so Kuali displays their current data directly.

use std::time::Duration;

use kuali_core::LlmConfig;

use crate::catalog::{self, ProviderDescriptor};
use crate::cli::ResolvedCommand;
use crate::provider::LlmError;

/// Model as published by its provider.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
}

const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=100";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const GEMINI_MODELS_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/models?pageSize=200";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// Requests a provider catalog while preserving useful credential and server errors.
pub async fn list_models(config: &LlmConfig, id: &str) -> Result<Vec<ModelChoice>, LlmError> {
    let descriptor =
        catalog::descriptor(id).ok_or_else(|| LlmError::Unavailable(id.to_string()))?;
    let settings = config.provider(id);
    let api_key = catalog::resolve_api_key(descriptor, settings.api_key());

    match id {
        "anthropic-api" => anthropic(descriptor, api_key).await,
        "openai-api" | "openai-compatible" => {
            openai_compatible(descriptor, api_key, settings.base_url()).await
        }
        "gemini-api" => gemini(descriptor, api_key).await,
        // Each CLI exposes models differently. Help text is the common fallback.
        "claude-cli" => match claude_cli(descriptor).await {
            Ok(models) if !models.is_empty() => Ok(models),
            _ => from_cli_help(descriptor).await,
        },
        "codex-cli" => match codex_cli(descriptor).await {
            Ok(models) if !models.is_empty() => Ok(models),
            _ => from_cli_help(descriptor).await,
        },
        _ => from_cli_help(descriptor).await,
    }
}

/// Claude Code resolves `/model` locally without model inference and immediately
/// returns names accepted by the installed version.
async fn claude_cli(descriptor: &ProviderDescriptor) -> Result<Vec<ModelChoice>, LlmError> {
    let command = require_binary(descriptor)?;
    let output = run_briefly(&command, &["--print", "/model"], Duration::from_secs(30)).await?;

    // Example: `Usage: /model <name>. Available: sonnet, opus, …, or a full model ID.`
    let Some(listed) = output.split("Available:").nth(1) else {
        return Ok(Vec::new());
    };
    let listed = listed.split(['.', '\n']).next().unwrap_or(listed);

    Ok(listed
        .split(',')
        .map(|name| name.trim().trim_start_matches("or "))
        .filter(|name| is_model_identifier(name))
        .map(|name| ModelChoice {
            id: name.to_string(),
            label: describe_claude_alias(name),
        })
        .collect())
}

fn describe_claude_alias(alias: &str) -> String {
    let note = match alias.trim_end_matches("[1m]") {
        "sonnet" => "equilibrado, el recomendado para resumir",
        "opus" => "más caro, para reuniones enrevesadas",
        "haiku" => "el más rápido",
        "fable" => "el más capaz",
        "best" => "el mejor disponible en tu cuenta",
        "default" => "el que tengas configurado",
        "opusplan" => "Opus para planificar, Sonnet para el resto",
        _ => "",
    };
    let window = if alias.ends_with("[1m]") {
        " · ventana de 1M"
    } else {
        ""
    };
    match (note, window) {
        ("", "") => alias.to_string(),
        ("", w) => format!("{alias} —{w}"),
        (n, w) => format!("{alias} — {n}{w}"),
    }
}

/// Codex embeds a JSON-RPC server exposing `model/list`, the same source used by
/// its own selector and therefore the authoritative installed catalog.
async fn codex_cli(descriptor: &ProviderDescriptor) -> Result<Vec<ModelChoice>, LlmError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let command = require_binary(descriptor)?;
    let mut process = command.process();
    let mut child = process
        .arg("app-server")
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| LlmError::Spawn {
            command: command.label(),
            source,
        })?;

    const LIST_ID: i64 = 2;
    let handshake = serde_json::json!({
        "id": 1,
        "method": "initialize",
        "params": { "clientInfo": { "name": "kuali", "version": env!("CARGO_PKG_VERSION") } }
    });
    let list = serde_json::json!({ "id": LIST_ID, "method": "model/list", "params": {} });

    // Keep stdin open while awaiting a response; closing it terminates the server
    // conversation before a reply can arrive.
    let mut stdin = child.stdin.take();
    if let Some(pipe) = stdin.as_mut() {
        pipe.write_all(format!("{handshake}\n{list}\n").as_bytes())
            .await
            .ok();
        pipe.flush().await.ok();
    }

    let conversation = async {
        let stdout = child.stdout.take()?;
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if message["id"].as_i64() != Some(LIST_ID) {
                continue;
            }
            return Some(
                entries(&message["result"]["data"])
                    .filter_map(|entry| {
                        let id = entry["id"].as_str()?;
                        let name = entry["displayName"].as_str().unwrap_or(id);
                        let detail = entry["description"].as_str().unwrap_or_default();
                        Some(ModelChoice {
                            id: id.to_string(),
                            label: if detail.is_empty() {
                                name.to_string()
                            } else {
                                format!("{name} — {detail}")
                            },
                        })
                    })
                    .collect::<Vec<_>>(),
            );
        }
        None
    };

    // The server continues listening after replying, so bound its lifetime and terminate it.
    let models = tokio::time::timeout(Duration::from_secs(45), conversation).await;
    drop(stdin);
    let _ = child.start_kill();

    Ok(models.ok().flatten().unwrap_or_default())
}

fn require_binary(descriptor: &ProviderDescriptor) -> Result<ResolvedCommand, LlmError> {
    let command = descriptor
        .command
        .ok_or_else(|| LlmError::Unavailable(descriptor.id.to_string()))?;
    crate::cli::resolve_command(command)
        .ok_or_else(|| LlmError::Unavailable(descriptor.id.to_string()))
}

/// Runs a short-lived command and returns its output.
async fn run_briefly(
    command: &ResolvedCommand,
    args: &[&str],
    limit: Duration,
) -> Result<String, LlmError> {
    let mut process = command.process();
    let output = process
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(limit, output)
        .await
        .map_err(|_| LlmError::Provider {
            provider: command.label(),
            message: format!("«{} {}» tardó demasiado", command.label(), args.join(" ")),
        })?
        .map_err(|source| LlmError::Spawn {
            command: command.label(),
            source,
        })?;

    // Some tools write to stderr; either stream is acceptable here.
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(text)
}

/// Models documented by the tool's own help output.
///
/// When no dedicated catalog command exists, every CLI still documents
/// `--model` and accepted aliases. Reading installed help allows new aliases to
/// appear without Kuali code changes.
async fn from_cli_help(descriptor: &ProviderDescriptor) -> Result<Vec<ModelChoice>, LlmError> {
    let command = require_binary(descriptor)?;
    let help = run_briefly(&command, &["--help"], Duration::from_secs(15)).await?;
    let mut models: Vec<ModelChoice> = model_aliases_in_help(&help)
        .into_iter()
        .map(|id| ModelChoice {
            label: format!("{id} — alias, siempre el más reciente"),
            id,
        })
        .collect();

    // Preserve known aliases even when help lists only a subset as examples.
    for known in descriptor.models {
        if !models.iter().any(|model| model.id == known.id) {
            models.push(ModelChoice {
                id: known.id.to_string(),
                label: known.label.to_string(),
            });
        }
    }

    if models.is_empty() {
        return Err(LlmError::Provider {
            provider: descriptor.id.to_string(),
            message: "esta herramienta no dice qué modelos acepta; escribe el identificador a mano"
                .into(),
        });
    }
    Ok(models)
}

/// Extracts quoted identifiers from the `--model` description.
///
/// Only this block is inspected because other help sections quote paths, flags,
/// and configuration examples that are not models.
fn model_aliases_in_help(help: &str) -> Vec<String> {
    let mut block = String::new();
    let mut inside = false;

    for line in help.lines() {
        let is_option = line.starts_with(char::is_whitespace) && line.trim_start().starts_with('-');
        if is_option {
            // An option description ends when the next option begins.
            if inside {
                break;
            }
            inside = mentions_model_option(line);
            if !inside {
                continue;
            }
        }
        if inside {
            block.push_str(line);
            block.push(' ');
        }
    }

    let mut found = Vec::new();
    for candidate in block.split('\'').skip(1).step_by(2) {
        if is_model_identifier(candidate) && !found.iter().any(|f| f == candidate) {
            found.push(candidate.to_string());
        }
    }
    found
}

fn mentions_model_option(line: &str) -> bool {
    line.split(|c: char| c.is_whitespace() || c == ',')
        .any(|token| token == "--model" || token.starts_with("--model="))
}

/// Rejects prose captured between quotes or commas. Model IDs are lowercase and
/// contain no spaces; brackets remain valid for Claude Code variants such as
/// `opus[1m]`.
fn is_model_identifier(candidate: &str) -> bool {
    (2..=64).contains(&candidate.len())
        && candidate.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && candidate
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._:[]".contains(c))
}

fn require_key(
    descriptor: &ProviderDescriptor,
    api_key: Option<String>,
) -> Result<String, LlmError> {
    api_key.ok_or_else(|| LlmError::Provider {
        provider: descriptor.id.to_string(),
        message: "hace falta la clave de API para poder consultar los modelos".into(),
    })
}

async fn fetch_json(
    provider: &'static str,
    request: reqwest::RequestBuilder,
) -> Result<serde_json::Value, LlmError> {
    let response = request.send().await.map_err(|source| LlmError::Http {
        provider: provider.into(),
        source,
    })?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(LlmError::Provider {
            provider: provider.into(),
            message: format!("HTTP {status}: {}", detail.trim()),
        });
    }

    response.json().await.map_err(|source| LlmError::Http {
        provider: provider.into(),
        source,
    })
}

async fn anthropic(
    descriptor: &ProviderDescriptor,
    api_key: Option<String>,
) -> Result<Vec<ModelChoice>, LlmError> {
    let key = require_key(descriptor, api_key)?;
    let json = fetch_json(
        descriptor.id,
        client()
            .get(ANTHROPIC_MODELS_URL)
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_API_VERSION),
    )
    .await?;

    // The provider already returns newest-first order, which is most useful here.
    Ok(entries(&json["data"])
        .filter_map(|entry| {
            let id = entry["id"].as_str()?;
            Some(ModelChoice {
                id: id.to_string(),
                label: entry["display_name"].as_str().unwrap_or(id).to_string(),
            })
        })
        .collect())
}

async fn openai_compatible(
    descriptor: &ProviderDescriptor,
    api_key: Option<String>,
    base_url: Option<&str>,
) -> Result<Vec<ModelChoice>, LlmError> {
    if descriptor.needs_api_key {
        require_key(descriptor, api_key.clone())?;
    }
    let base = base_url
        .or(descriptor.default_base_url)
        .unwrap_or("https://api.openai.com/v1")
        .trim_end_matches('/');

    let mut request = client().get(format!("{base}/models"));
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.bearer_auth(key);
    }
    let json = fetch_json(descriptor.id, request).await?;

    let mut models: Vec<(i64, ModelChoice)> = entries(&json["data"])
        .filter_map(|entry| {
            let id = entry["id"].as_str()?;
            Some((
                entry["created"].as_i64().unwrap_or(0),
                ModelChoice {
                    id: id.to_string(),
                    label: id.to_string(),
                },
            ))
        })
        .collect();

    // OpenAI catalogs mix transcription, image, voice, embedding, and chat
    // models. If heuristics remove everything on a differently named compatible
    // server, return the complete list rather than an empty one.
    let conversational: Vec<_> = models
        .iter()
        .filter(|(_, model)| is_conversational(&model.id))
        .cloned()
        .collect();
    if !conversational.is_empty() {
        models = conversational;
    }

    // Sort newest first so recent releases are visible immediately.
    models.sort_by(|(a_created, a), (b_created, b)| {
        b_created.cmp(a_created).then_with(|| a.id.cmp(&b.id))
    });
    Ok(models.into_iter().map(|(_, model)| model).collect())
}

/// Rejects models clearly unsuitable for meeting summaries. This uses names
/// because the API does not expose model capabilities.
fn is_conversational(id: &str) -> bool {
    const NOT_FOR_CHAT: &[&str] = &[
        "embedding",
        "embed",
        "tts",
        "whisper",
        "transcribe",
        "audio",
        "realtime",
        "dall-e",
        "image",
        "moderation",
        "rerank",
        "vision-encoder",
    ];
    let id = id.to_lowercase();
    !NOT_FOR_CHAT.iter().any(|kind| id.contains(kind))
}

async fn gemini(
    descriptor: &ProviderDescriptor,
    api_key: Option<String>,
) -> Result<Vec<ModelChoice>, LlmError> {
    let key = require_key(descriptor, api_key)?;
    let json = fetch_json(
        descriptor.id,
        client()
            .get(GEMINI_MODELS_URL)
            .header("x-goog-api-key", key),
    )
    .await?;

    Ok(entries(&json["models"])
        .filter(|entry| {
            // Gemini also publishes embedding and image models. Summary-capable
            // entries support `generateContent`.
            entry["supportedGenerationMethods"]
                .as_array()
                .map(|methods| {
                    methods
                        .iter()
                        .any(|m| m.as_str() == Some("generateContent"))
                })
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            // Catalog names use `models/gemini-…`; generation expects the suffix.
            let name = entry["name"].as_str()?;
            let id = name.rsplit('/').next().unwrap_or(name);
            Some(ModelChoice {
                id: id.to_string(),
                label: entry["displayName"].as_str().unwrap_or(id).to_string(),
            })
        })
        .collect())
}

fn entries(value: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    value.as_array().map(|a| a.as_slice()).unwrap_or(&[]).iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeddings_and_audio_models_are_not_offered_for_summaries() {
        assert!(is_conversational("gpt-5"));
        assert!(is_conversational("llama3.1:8b"));
        assert!(!is_conversational("text-embedding-3-large"));
        assert!(!is_conversational("whisper-1"));
        assert!(!is_conversational("gpt-4o-realtime-preview"));
        assert!(!is_conversational("dall-e-3"));
    }

    /// Verbatim `claude --help` excerpt, including line breaks that parsing must handle.
    const CLAUDE_HELP: &str = "\
  --mcp-config <configs...>             Load MCP servers from JSON files or
                                        strings (space-separated)
  --model <model>                       Model for the current session. Provide
                                        an alias for the latest model (e.g.
                                        'fable', 'opus', or 'sonnet') or a
                                        model's full name (e.g.
                                        'claude-fable-5').
  -n, --name <name>                     Set a display name for this session
";

    #[test]
    fn reads_the_aliases_out_of_the_tools_own_help() {
        assert_eq!(
            model_aliases_in_help(CLAUDE_HELP),
            vec!["fable", "opus", "sonnet"]
        );
    }

    #[test]
    fn only_the_model_option_is_read_not_the_rest_of_the_help() {
        // `configs` and `space-separated` are quoted by unrelated options and
        // must not become models.
        let aliases = model_aliases_in_help(CLAUDE_HELP);
        assert!(!aliases.iter().any(|a| a.contains("separated")));
    }

    #[test]
    fn a_help_without_examples_yields_nothing_rather_than_prose() {
        // Codex help describes the option without naming any models.
        let codex = "  -m, --model <MODEL>\n          Model the agent should use\n";
        assert!(model_aliases_in_help(codex).is_empty());
    }

    /// Verbatim output from `claude --print "/model"`.
    const CLAUDE_MODEL_COMMAND: &str = "Current model: Opus 5\nUsage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], fable[1m], opusplan, default, or a full model ID.\n";

    #[test]
    fn reads_every_name_that_claude_code_says_it_accepts() {
        let listed = CLAUDE_MODEL_COMMAND.split("Available:").nth(1).unwrap();
        let listed = listed.split(['.', '\n']).next().unwrap();
        let names: Vec<_> = listed
            .split(',')
            .map(|name| name.trim().trim_start_matches("or "))
            .filter(|name| is_model_identifier(name))
            .collect();

        assert_eq!(
            names,
            vec![
                "sonnet",
                "opus",
                "haiku",
                "fable",
                "best",
                "sonnet[1m]",
                "opus[1m]",
                "fable[1m]",
                "opusplan",
                "default"
            ]
        );
        // `a full model ID` is prose rather than a model.
        assert!(!names.iter().any(|n| n.contains(' ')));
    }

    #[test]
    fn the_one_million_variants_are_labelled_as_such() {
        assert!(describe_claude_alias("opus[1m]").contains("1M"));
        assert!(describe_claude_alias("sonnet").contains("resumir"));
        // Display unknown aliases verbatim without inventing metadata.
        assert_eq!(describe_claude_alias("gpt-9"), "gpt-9");
    }

    #[test]
    fn quoted_prose_is_not_mistaken_for_a_model() {
        assert!(is_model_identifier("claude-opus-4-8"));
        assert!(is_model_identifier("gpt-5.6-sol"));
        assert!(is_model_identifier("llama3.1:8b"));
        assert!(!is_model_identifier("s full name (e.g. "));
        assert!(!is_model_identifier("Model for the current session"));
    }

    #[tokio::test]
    async fn asking_an_api_without_a_key_explains_what_is_missing() {
        let error = list_models(&LlmConfig::default(), "anthropic-api")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("clave de API"));
    }
}
