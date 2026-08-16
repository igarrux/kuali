//! Commands exposed by the engine to the interface.
//!
//! Commands return `Result<_, String>` because the frontend only needs a
//! displayable message, not the concrete error type.

use std::collections::BTreeMap;

use kuali_core::{
    ActionItem, EngineStatus, KualiConfig, Meeting, MeetingMeta, MeetingSummary, ModelState,
    ProviderSettings, WebhookSubscription, WhisperModel,
};
use kuali_engine::Engine;
use kuali_llm::{ModelChoice, ProviderInfo, ProviderStatus};
use serde::Serialize;
use tauri::{Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;

fn fail(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// --- configuration --------------------------------------------------------

#[tauri::command]
pub fn get_config(engine: State<'_, Engine>) -> KualiConfig {
    engine.config()
}

#[tauri::command]
pub async fn set_config(
    app: tauri::AppHandle,
    engine: State<'_, Engine>,
    config: KualiConfig,
) -> Result<(), String> {
    engine.update_config(config).await.map_err(fail)?;
    let _ = app.emit(crate::CONFIG_CHANGED_CHANNEL, ());
    Ok(())
}

#[tauri::command]
pub fn app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// User-facing requirements that still prevent Kuali from operating.
#[tauri::command]
pub fn missing_requirements(engine: State<'_, Engine>) -> Vec<String> {
    engine
        .config()
        .missing_requirements()
        .into_iter()
        .map(str::to_string)
        .collect()
}

// --- state ----------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub status: EngineStatus,
    pub model_state: ModelState,
    pub discord_connected: bool,
    pub safe_for_update: bool,
    pub current_meeting: Option<Meeting>,
    pub current_meetings: Vec<Meeting>,
    pub missing: Vec<String>,
    pub web_meetings: WebMeetingsSnapshot,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebMeetingsSnapshot {
    pub enabled: bool,
    pub port: u16,
    pub listening: bool,
}

/// Returns all state at once so initial rendering does not require several calls.
#[tauri::command]
pub fn get_snapshot(engine: State<'_, Engine>) -> Snapshot {
    let config = engine.config();
    let current_meetings = engine.current_meetings();
    let current_meeting = current_meetings
        .iter()
        .max_by_key(|meeting| meeting.meta.started_at)
        .cloned();
    Snapshot {
        status: engine.status(),
        model_state: engine.model_state(),
        discord_connected: engine.discord_connected(),
        safe_for_update: engine.safe_for_update(),
        current_meeting,
        current_meetings,
        missing: config
            .missing_requirements()
            .into_iter()
            .map(str::to_string)
            .collect(),
        web_meetings: WebMeetingsSnapshot {
            enabled: config.meet.enabled,
            port: config.meet.port,
            listening: engine.web_ingest_ready(),
        },
    }
}

#[tauri::command]
pub async fn connect(engine: State<'_, Engine>) -> Result<(), String> {
    engine.connect().await.map_err(fail)
}

#[tauri::command]
pub async fn disconnect(engine: State<'_, Engine>) -> Result<(), String> {
    engine.disconnect().await;
    Ok(())
}

#[tauri::command]
pub async fn leave_call(engine: State<'_, Engine>) -> Result<(), String> {
    engine.leave_call().await.map_err(fail)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookChannelInfo {
    pub guild_id: String,
    pub guild_name: String,
    pub channel_id: String,
    pub channel_name: String,
}

#[tauri::command]
pub fn webhook_channels(engine: State<'_, Engine>) -> Result<Vec<WebhookChannelInfo>, String> {
    let mut channels = BTreeMap::new();
    for meeting in engine.list_meetings().map_err(fail)? {
        channels
            .entry((meeting.guild_id, meeting.channel_id))
            .or_insert((meeting.guild_name, meeting.channel_name));
    }
    Ok(channels
        .into_iter()
        .map(
            |((guild_id, channel_id), (guild_name, channel_name))| WebhookChannelInfo {
                guild_id: guild_id.to_string(),
                guild_name,
                channel_id: channel_id.to_string(),
                channel_name,
            },
        )
        .collect())
}

#[tauri::command]
pub async fn test_webhook(
    engine: State<'_, Engine>,
    webhook: WebhookSubscription,
) -> Result<String, String> {
    engine.test_webhook(&webhook).await.map_err(fail)
}

// --- meetings -------------------------------------------------------------

#[tauri::command]
pub fn list_meetings(engine: State<'_, Engine>) -> Result<Vec<MeetingMeta>, String> {
    engine.list_meetings().map_err(fail)
}

#[tauri::command]
pub async fn search_meetings(
    query: String,
) -> Result<Vec<kuali_store::MeetingSearchResult>, String> {
    tauri::async_runtime::spawn_blocking(move || kuali_store::search(&query))
        .await
        .map_err(fail)?
        .map_err(fail)
}

#[tauri::command]
pub fn load_meeting(engine: State<'_, Engine>, id: String) -> Result<Meeting, String> {
    engine.load_meeting(&id).map_err(fail)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListItem {
    pub meeting_id: String,
    pub meeting_title: String,
    pub guild_name: String,
    pub channel_name: String,
    pub started_at: String,
    /// Kept as text so Discord snowflakes and normalized web-platform IDs never
    /// lose precision in JavaScript.
    pub assignee_id: Option<String>,
    pub assignee_avatar_url: Option<String>,
    pub assignee_color: Option<String>,
    pub task: ActionItem,
}

/// Lightweight index for the global task view. It runs off the UI thread because
/// it scans complete meeting records from disk.
#[tauri::command]
pub async fn list_tasks() -> Result<Vec<TaskListItem>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut tasks = Vec::new();
        for meta in kuali_store::list().map_err(fail)? {
            let meeting = match kuali_store::load(&meta.id) {
                Ok(meeting) => meeting,
                Err(_) => continue,
            };
            let Some(summary) = meeting.summary.as_ref() else {
                continue;
            };
            for task in &summary.action_items {
                let assignee = task.assignee.as_deref().unwrap_or("");
                let speaker = meeting.speakers.iter().find(|speaker| {
                    !speaker.is_bot
                        && (speaker.display_name.eq_ignore_ascii_case(assignee)
                            || speaker.username.eq_ignore_ascii_case(assignee))
                });
                tasks.push(TaskListItem {
                    meeting_id: meta.id.clone(),
                    meeting_title: meta.title(),
                    guild_name: meta.guild_name.clone(),
                    channel_name: meta.channel_name.clone(),
                    started_at: meta.started_at.to_rfc3339(),
                    assignee_id: speaker.map(|speaker| speaker.user_id.to_string()),
                    assignee_avatar_url: speaker.and_then(|speaker| speaker.avatar_url.clone()),
                    assignee_color: speaker.map(|speaker| speaker.color.clone()),
                    task: task.clone(),
                });
            }
        }
        Ok(tasks)
    })
    .await
    .map_err(fail)?
}

/// Replaces a meeting's tags and answers with the sanitized result so the
/// interface shows exactly what was written.
#[tauri::command]
pub async fn set_meeting_tags(id: String, tags: Vec<String>) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || kuali_store::set_tags(&id, tags))
        .await
        .map_err(fail)?
        .map_err(fail)
}

/// Servers Kuali knows about, with their Discord icons.
#[tauri::command]
pub fn list_guilds() -> Vec<kuali_core::GuildInfo> {
    kuali_store::guilds()
}

/// Folders the user has created, including empty ones.
#[tauri::command]
pub async fn list_folders() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(kuali_store::list_folders)
        .await
        .map_err(fail)?
        .map_err(fail)
}

#[tauri::command]
pub async fn create_folder(name: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || kuali_store::create_folder(&name))
        .await
        .map_err(fail)?
        .map_err(fail)
}

#[tauri::command]
pub async fn rename_folder(from: String, to: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || kuali_store::rename_folder(&from, &to))
        .await
        .map_err(fail)?
        .map_err(fail)
}

/// Removes the folder without touching the meetings it held.
#[tauri::command]
pub async fn delete_folder(name: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || kuali_store::delete_folder(&name))
        .await
        .map_err(fail)?
        .map_err(fail)
}

/// Moves meetings into a folder, or out of every folder when `folder` is null.
#[tauri::command]
pub async fn set_meeting_folder(ids: Vec<String>, folder: Option<String>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || kuali_store::set_folder(&ids, folder.as_deref()))
        .await
        .map_err(fail)?
        .map_err(fail)
}

/// Tags already in use, for suggesting instead of retyping.
#[tauri::command]
pub async fn list_tags() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(kuali_store::all_tags)
        .await
        .map_err(fail)?
        .map_err(fail)
}

#[tauri::command]
pub fn delete_meeting(engine: State<'_, Engine>, id: String) -> Result<(), String> {
    engine.delete_meeting(&id).map_err(fail)
}

#[tauri::command]
pub async fn delete_meetings(engine: State<'_, Engine>, ids: Vec<String>) -> Result<usize, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.delete_meetings(&ids))
        .await
        .map_err(fail)?
        .map_err(fail)
}

#[tauri::command]
pub async fn delete_channel_meetings(
    engine: State<'_, Engine>,
    meeting_id: String,
) -> Result<usize, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.delete_channel_meetings(&meeting_id))
        .await
        .map_err(fail)?
        .map_err(fail)
}

#[tauri::command]
pub fn set_task_done(
    engine: State<'_, Engine>,
    meeting_id: String,
    task_id: String,
    done: bool,
) -> Result<(), String> {
    engine
        .set_task_done(&meeting_id, &task_id, done)
        .map_err(fail)
}

#[tauri::command]
pub async fn resummarize(
    engine: State<'_, Engine>,
    meeting_id: String,
) -> Result<MeetingSummary, String> {
    engine.resummarize(&meeting_id).await.map_err(fail)
}

/// Opens the save dialog and exports, returning `None` when the user cancels.
#[tauri::command]
pub async fn export_meeting(
    app: tauri::AppHandle,
    engine: State<'_, Engine>,
    meeting_id: String,
    format: String,
) -> Result<Option<String>, String> {
    let markdown = format == "markdown";
    let extension = if markdown { "md" } else { "json" };
    let suggested = engine.suggested_filename(&meeting_id, extension);

    let path = app
        .dialog()
        .file()
        .set_file_name(&suggested)
        .add_filter(if markdown { "Markdown" } else { "JSON" }, &[extension])
        .blocking_save_file();

    let Some(path) = path else { return Ok(None) };
    let path = path.into_path().map_err(fail)?;

    engine.export(&meeting_id, &path, markdown).map_err(fail)?;
    Ok(Some(path.display().to_string()))
}

// --- providers and models -------------------------------------------------

#[tauri::command]
pub async fn available_providers(engine: State<'_, Engine>) -> Result<Vec<ProviderInfo>, String> {
    Ok(engine.available_providers().await)
}

/// Every known provider, including unavailable ones and their missing
/// requirements. This drives the settings picker.
#[tauri::command]
pub async fn provider_catalog(engine: State<'_, Engine>) -> Result<Vec<ProviderStatus>, String> {
    Ok(engine.provider_statuses().await)
}

/// Tests a provider with the unsaved settings currently shown in the UI.
/// Provider errors remain verbatim because they usually explain the actual
/// issue, such as an invalid key, missing model, or offline server.
#[tauri::command]
pub async fn test_provider(
    engine: State<'_, Engine>,
    id: String,
    settings: Option<ProviderSettings>,
) -> Result<String, String> {
    engine.test_provider(&id, settings).await.map_err(fail)
}

/// Fetches the provider's live model catalog with the key currently in the UI.
#[tauri::command]
pub async fn provider_models(
    engine: State<'_, Engine>,
    id: String,
    settings: Option<ProviderSettings>,
) -> Result<Vec<ModelChoice>, String> {
    engine.list_models(&id, settings).await.map_err(fail)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub display_name: String,
    pub technical_name: String,
    pub description: String,
    pub approx_bytes: u64,
    pub estimated_ram_bytes: u64,
    pub downloaded: bool,
    pub selectable: bool,
    pub recommended: bool,
}

#[tauri::command]
pub fn whisper_models(engine: State<'_, Engine>) -> Vec<ModelInfo> {
    let models_dir = engine.config().whisper.resolved_models_directory();
    WhisperModel::ALL
        .iter()
        .map(|model| ModelInfo {
            // Match serde's representation so the frontend can write it back
            // unchanged in the configuration.
            id: serde_json::to_value(model)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            label: model.label().to_string(),
            display_name: model.display_name().to_string(),
            technical_name: model.technical_name().to_string(),
            description: model.description().to_string(),
            approx_bytes: model.approx_bytes(),
            estimated_ram_bytes: model.estimated_ram_bytes(),
            downloaded: kuali_stt::is_downloaded(&models_dir, *model),
            selectable: model.is_selectable(),
            recommended: model.is_recommended(),
        })
        .collect()
}

#[tauri::command]
pub fn resolved_models_directory(engine: State<'_, Engine>) -> String {
    engine
        .config()
        .whisper
        .resolved_models_directory()
        .display()
        .to_string()
}

#[tauri::command]
pub async fn choose_models_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    // Synchronous Tauri commands run on the main thread. Opening a blocking
    // picker there prevents macOS from rendering and servicing its own dialog.
    let (reply, response) = tokio::sync::oneshot::channel();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Elige dónde guardar los modelos de Whisper");

    // Parenting keeps the picker above Kuali instead of behind the application.
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }

    dialog.pick_folder(move |selected| {
        let selected = selected
            .map(|path| path.into_path().map(|path| path.display().to_string()))
            .transpose()
            .map_err(fail);
        let _ = reply.send(selected);
    });

    response
        .await
        .map_err(|_| "El selector de carpetas se cerró inesperadamente".to_string())?
}

#[tauri::command]
pub async fn download_model(engine: State<'_, Engine>, model: String) -> Result<(), String> {
    let model: WhisperModel =
        serde_json::from_value(serde_json::Value::String(model)).map_err(fail)?;
    engine.download_model(model).await.map_err(fail)
}

#[tauri::command]
pub fn cancel_model_download(engine: State<'_, Engine>) -> bool {
    engine.cancel_model_download()
}

#[tauri::command]
pub async fn delete_model(engine: State<'_, Engine>, model: String) -> Result<u64, String> {
    let model: WhisperModel =
        serde_json::from_value(serde_json::Value::String(model)).map_err(fail)?;
    engine.delete_model(model).await.map_err(fail)
}

/// Opens the directory containing Kuali meeting records.
#[tauri::command]
pub fn reveal_data_dir(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = kuali_core::paths::meetings_dir();
    std::fs::create_dir_all(&dir).map_err(fail)?;
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(fail)
}

fn browser_extension_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let resource_dir = app.path().resource_dir().map_err(fail)?;
    let candidates = [
        resource_dir.join("browser-extension"),
        resource_dir.join("_up_").join("browser-extension"),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("browser-extension"),
    ];
    candidates
        .into_iter()
        .find(|path| path.join("manifest.json").is_file())
        .ok_or_else(|| "No encontré la carpeta empaquetada de la extensión de Kuali".to_string())
}

#[tauri::command]
pub fn browser_extension_path(app: tauri::AppHandle) -> Result<String, String> {
    Ok(browser_extension_dir(&app)?.display().to_string())
}

#[tauri::command]
pub fn reveal_browser_extension(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = browser_extension_dir(&app)?;
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(fail)
}

#[tauri::command]
pub fn open_setup_destination(app: tauri::AppHandle, destination: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = match destination.as_str() {
        "discord-developers" => "https://discord.com/developers/applications",
        "chrome-extensions" => "chrome://extensions",
        _ => return Err("Ese destino no forma parte de la guía de Kuali".into()),
    };
    app.opener().open_url(url, None::<&str>).map_err(fail)
}

/// Validates the bot token and opens Discord's official server authorization.
#[tauri::command]
pub async fn open_discord_install(app: tauri::AppHandle, bot_token: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = kuali_discord::installation_url(&bot_token)
        .await
        .map_err(fail)?;
    app.opener().open_url(url, None::<&str>).map_err(fail)
}

const CHROME_WEB_STORE_URL: &str =
    "https://chromewebstore.google.com/detail/kuali/cgojkmdggflcggedmapamcmkelgaahhp";

fn browser_destination(browser: &str, store: bool) -> Result<(&'static str, &'static str), String> {
    let application = match browser {
        "chrome" => "Google Chrome",
        "edge" => "Microsoft Edge",
        "brave" => "Brave Browser",
        "arc" => "Arc",
        _ => return Err("Ese navegador no forma parte de la guía de Kuali".into()),
    };
    let url = if store {
        CHROME_WEB_STORE_URL
    } else {
        match browser {
            "chrome" | "arc" => "chrome://extensions",
            "edge" => "edge://extensions",
            "brave" => "brave://extensions",
            _ => unreachable!("the browser was validated above"),
        }
    };
    Ok((application, url))
}

fn open_browser_destination(
    app: tauri::AppHandle,
    browser: &str,
    store: bool,
) -> Result<(), String> {
    let (application, url) = browser_destination(browser, store)?;

    #[cfg(target_os = "macos")]
    {
        let _ = app;
        let status = std::process::Command::new("open")
            .args(["-a", application, url])
            .status()
            .map_err(fail)?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("No encontré {application} en esta Mac"))
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        use tauri_plugin_opener::OpenerExt;
        let _ = application;
        app.opener().open_url(url, None::<&str>).map_err(fail)
    }
}

/// Opens Kuali's Chrome Web Store listing in the browser selected by the user.
#[tauri::command]
pub fn open_browser_extension_store(app: tauri::AppHandle, browser: String) -> Result<(), String> {
    open_browser_destination(app, &browser, true)
}

/// Opens the extension manager for manual and development installations.
#[tauri::command]
pub fn open_browser_extensions(app: tauri::AppHandle, browser: String) -> Result<(), String> {
    open_browser_destination(app, &browser, false)
}

#[cfg(test)]
mod browser_destination_tests {
    use super::{browser_destination, CHROME_WEB_STORE_URL};

    #[test]
    fn every_supported_browser_uses_the_same_verified_store_listing() {
        for browser in ["chrome", "edge", "brave", "arc"] {
            assert_eq!(
                browser_destination(browser, true).unwrap().1,
                CHROME_WEB_STORE_URL
            );
        }
    }

    #[test]
    fn manual_installation_keeps_each_browsers_internal_manager() {
        assert_eq!(
            browser_destination("chrome", false).unwrap().1,
            "chrome://extensions"
        );
        assert_eq!(
            browser_destination("edge", false).unwrap().1,
            "edge://extensions"
        );
        assert_eq!(
            browser_destination("brave", false).unwrap().1,
            "brave://extensions"
        );
        assert_eq!(
            browser_destination("arc", false).unwrap().1,
            "chrome://extensions"
        );
        assert!(browser_destination("safari", true).is_err());
    }
}

#[tauri::command]
pub fn autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(fail)
}

#[tauri::command]
pub fn set_autostart_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        app.autolaunch().enable().map_err(fail)
    } else {
        app.autolaunch().disable().map_err(fail)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInfo {
    pub current_version: String,
    pub version: String,
    pub notes: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateProgress {
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
}

/// Checks GitHub's signed updater manifest without downloading the application.
#[tauri::command]
pub async fn check_for_update(app: tauri::AppHandle) -> Result<Option<AppUpdateInfo>, String> {
    let current_version = app.package_info().version.to_string();
    let update = app.updater().map_err(fail)?.check().await.map_err(fail)?;
    Ok(update.map(|update| AppUpdateInfo {
        current_version,
        version: update.version.to_string(),
        notes: update.body.clone(),
    }))
}

/// Downloads, verifies, and installs the newest signed release before restarting.
#[tauri::command]
pub async fn install_update(
    app: tauri::AppHandle,
    engine: State<'_, Engine>,
) -> Result<bool, String> {
    if !engine.safe_for_update() {
        return Err(
            "Kuali terminará primero la reunión, transcripción y resumen en curso".to_string(),
        );
    }
    let Some(update) = app.updater().map_err(fail)?.check().await.map_err(fail)? else {
        return Ok(false);
    };

    let progress_app = app.clone();
    let mut downloaded_bytes = 0u64;
    let package = update
        .download(
            move |chunk_length, total_bytes| {
                downloaded_bytes = downloaded_bytes.saturating_add(chunk_length as u64);
                let _ = progress_app.emit(
                    "kuali://update-progress",
                    AppUpdateProgress {
                        downloaded_bytes,
                        total_bytes,
                    },
                );
            },
            || {},
        )
        .await
        .map_err(fail)?;

    // A call may begin while the package is downloading. Keep the verified
    // package in memory and defer installation instead of interrupting capture,
    // final transcription, summaries or their completion work.
    if !engine.safe_for_update() {
        let _ = app.emit("kuali://update-waiting", ());
    }
    while !engine.safe_for_update() {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    update.install(package).map_err(fail)?;
    app.restart();
}

/// Schedules a reset for the next launch and restarts. Deleting in the new
/// process prevents downloads, meetings, or summaries from writing concurrently.
#[tauri::command]
pub async fn factory_reset(
    app: tauri::AppHandle,
    engine: State<'_, Engine>,
    confirmation: String,
) -> Result<(), String> {
    if !crate::factory_reset::confirmation_matches(&confirmation) {
        return Err("La frase de confirmación no coincide exactamente".into());
    }

    if app.autolaunch().is_enabled().map_err(fail)? {
        app.autolaunch().disable().map_err(fail)?;
    }
    crate::factory_reset::schedule(&engine.config())?;

    // Give the WebView enough time to clear localStorage and render the final
    // state before Tauri terminates this process.
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        app.restart();
    });
    Ok(())
}

#[tauri::command]
pub fn take_factory_reset_completed() -> Result<bool, String> {
    crate::factory_reset::take_completed()
}
