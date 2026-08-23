// Release builds should not open a console behind the window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod factory_reset;

use kuali_engine::Engine;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Listener, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::MacosLauncher;

/// Carries every engine event to the interface.
const EVENT_CHANNEL: &str = "kuali://event";
const NAVIGATION_CHANNEL: &str = "kuali://navigate";
const CONFIG_CHANGED_CHANNEL: &str = "kuali://config-changed";
/// Panel shown under the menu bar icon.
const PANEL_WINDOW: &str = "panel";
const PANEL_SHOWN_CHANNEL: &str = "kuali://panel-shown";
const PANEL_WIDTH: f64 = 360.0;
const PANEL_HEIGHT: f64 = 428.0;

fn reveal_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn show_main_window(app: &tauri::AppHandle, destination: &str) {
    hide_tray_panel(app);
    reveal_main_window(app);
    let _ = app.emit(NAVIGATION_CHANNEL, destination);
}

/// Horizontal placement of the panel under the tray icon, clamped so it stays
/// on the screen the icon lives on. Returned separately from the window call to
/// keep the arithmetic testable.
fn panel_origin(
    icon_center_x: f64,
    icon_bottom_y: f64,
    panel_width: f64,
    monitor_left: f64,
    monitor_right: f64,
) -> (f64, f64) {
    const MARGIN: f64 = 8.0;
    let unclamped = icon_center_x - panel_width / 2.0;
    let max_x = (monitor_right - panel_width - MARGIN).max(monitor_left + MARGIN);
    (
        unclamped.clamp(monitor_left + MARGIN, max_x),
        icon_bottom_y + MARGIN,
    )
}

fn hide_tray_panel(app: &tauri::AppHandle) {
    if let Some(panel) = app.get_webview_window(PANEL_WINDOW) {
        let _ = panel.hide();
    }
}

/// Opens the panel under the icon that was clicked, or closes it when it is
/// already open, the way a menu-bar app behaves.
fn toggle_tray_panel(app: &tauri::AppHandle, rect: tauri::Rect) {
    let Some(panel) = app.get_webview_window(PANEL_WINDOW) else {
        return;
    };
    if panel.is_visible().unwrap_or(false) {
        let _ = panel.hide();
        return;
    }

    let scale = panel.scale_factor().unwrap_or(1.0);
    let icon_position = rect.position.to_physical::<f64>(scale);
    let icon_size = rect.size.to_physical::<f64>(scale);
    let panel_size = panel.outer_size().ok();
    let panel_width = panel_size
        .map(|size| size.width as f64)
        .unwrap_or(PANEL_WIDTH * scale);

    let icon_center_x = icon_position.x + icon_size.width / 2.0;
    let icon_bottom_y = icon_position.y + icon_size.height;

    let (monitor_left, monitor_right) = app
        .monitor_from_point(icon_center_x, icon_bottom_y)
        .ok()
        .flatten()
        .map(|monitor| {
            let left = monitor.position().x as f64;
            (left, left + monitor.size().width as f64)
        })
        .unwrap_or((icon_center_x - panel_width, icon_center_x + panel_width));

    let (x, y) = panel_origin(
        icon_center_x,
        icon_bottom_y,
        panel_width,
        monitor_left,
        monitor_right,
    );

    let _ = panel.set_position(PhysicalPosition::new(x, y));
    let _ = panel.show();
    let _ = panel.set_focus();
    // The panel may have been hidden for hours; ask it to refresh what it shows.
    let _ = app.emit(PANEL_SHOWN_CHANNEL, ());
}

fn tray_follow_copy(enabled: bool, language: &str) -> (&'static str, &'static str) {
    match (enabled, language == "en") {
        (true, true) => ("🟢 Discord · Following enabled", "Pause Discord following"),
        (false, true) => ("🟠 Discord · Following paused", "Enable Discord following"),
        (true, false) => (
            "🟢 Discord · Seguimiento activo",
            "Pausar seguimiento de Discord",
        ),
        (false, false) => (
            "🟠 Discord · Seguimiento pausado",
            "Activar seguimiento de Discord",
        ),
    }
}

fn sync_tray_follow_items<R: tauri::Runtime>(
    status_item: &MenuItem<R>,
    action_item: &MenuItem<R>,
    enabled: bool,
    language: &str,
) {
    let (status, action) = tray_follow_copy(enabled, language);
    let _ = status_item.set_text(status);
    let _ = action_item.set_text(action);
}

/// The tray panel counts as a visible window for the operating system, so the
/// dock icon must look at the main window rather than at any window.
#[cfg(any(target_os = "macos", test))]
fn should_restore_main_window(main_window_visible: bool) -> bool {
    !main_window_visible
}

fn main() {
    let start_hidden = std::env::args().any(|argument| argument == "--hidden");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Songbird and Serenity are excessively verbose at debug level.
                .unwrap_or_else(|_| "kuali=debug,warn".into()),
        )
        .init();

    match factory_reset::apply_pending() {
        Ok(true) => tracing::info!("Kuali reset completed"),
        Ok(false) => {}
        Err(error) => tracing::error!(%error, "no se pudo completar el restablecimiento de Kuali"),
    }

    if let Err(e) = kuali_core::paths::ensure_dirs() {
        tracing::error!(error = %e, "no se pudieron crear los directorios de Kuali");
    }

    let config = kuali_core::paths::load_config().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "configuration is unreadable; starting with defaults");
        Default::default()
    });
    let auto_connect = config.is_ready();
    let (engine, mut events) = Engine::new(config);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(engine.clone())
        .setup(move |app| {
            let tray_config = engine.config();
            let (follow_status, follow_action) = tray_follow_copy(
                tray_config.discord.follow_automatically,
                &tray_config.application.language,
            );
            let open_item = MenuItem::with_id(app, "open", "Abrir Kuali", true, None::<&str>)?;
            let tasks_item = MenuItem::with_id(app, "tasks", "Ver tareas", true, None::<&str>)?;
            let primary_separator = PredefinedMenuItem::separator(app)?;
            let follow_status_item =
                MenuItem::with_id(app, "follow-status", follow_status, false, None::<&str>)?;
            let follow_item =
                MenuItem::with_id(app, "toggle-follow", follow_action, true, None::<&str>)?;
            let secondary_separator = PredefinedMenuItem::separator(app)?;
            let version_item = MenuItem::with_id(
                app,
                "version",
                format!("Kuali {}", app.package_info().version),
                false,
                None::<&str>,
            )?;
            let quit_item = MenuItem::with_id(app, "quit", "Salir de Kuali", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &tasks_item,
                    &primary_separator,
                    &follow_status_item,
                    &follow_item,
                    &secondary_separator,
                    &version_item,
                    &quit_item,
                ],
            )?;

            let status_item_for_config = follow_status_item.clone();
            let action_item_for_config = follow_item.clone();
            let engine_for_config = engine.clone();
            app.listen(CONFIG_CHANGED_CHANNEL, move |_| {
                let config = engine_for_config.config();
                sync_tray_follow_items(
                    &status_item_for_config,
                    &action_item_for_config,
                    config.discord.follow_automatically,
                    &config.application.language,
                );
            });

            // Built hidden and positioned on demand: a menu-bar panel has no
            // meaningful default position.
            let panel =
                WebviewWindowBuilder::new(app, PANEL_WINDOW, WebviewUrl::App("tray.html".into()))
                    .title("Kuali")
                    .inner_size(PANEL_WIDTH, PANEL_HEIGHT)
                    .resizable(false)
                    .decorations(false)
                    .always_on_top(true)
                    .skip_taskbar(true)
                    .visible(false)
                    .build()?;

            let panel_for_blur = panel.clone();
            panel.on_window_event(move |event| {
                // Clicking anywhere else dismisses it, like every other menu.
                if matches!(event, WindowEvent::Focused(false)) {
                    let _ = panel_for_blur.hide();
                }
            });

            TrayIconBuilder::new()
                .tooltip("Kuali")
                .icon(
                    app.default_window_icon()
                        .expect("Kuali necesita un icono")
                        .clone(),
                )
                .icon_as_template(true)
                .menu(&menu)
                // Left click opens Kuali's own panel; the native menu stays on
                // the right button for the operating system's conventions.
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_tray_panel(tray.app_handle(), rect);
                    }
                })
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show_main_window(app, "home"),
                    "tasks" => show_main_window(app, "tasks"),
                    "toggle-follow" => {
                        let engine = app.state::<Engine>().inner().clone();
                        let discord = engine.config().discord;
                        if discord.follow_user_id.is_none()
                            && discord
                                .follow_username
                                .as_deref()
                                .is_none_or(|username| username.trim().is_empty())
                        {
                            show_main_window(app, "guide");
                            return;
                        }
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let mut config = engine.config();
                            config.discord.follow_automatically =
                                !config.discord.follow_automatically;
                            if engine.update_config(config).await.is_ok() {
                                // The window may have been hidden for hours. Notify it so
                                // its controls reflect changes made from the menu bar.
                                let _ = app.emit(CONFIG_CHANGED_CHANNEL, ());
                            }
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            if start_hidden {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            // Engine-to-interface bridge.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(event) = events.recv().await {
                    if let Err(e) = handle.emit(EVENT_CHANNEL, &event) {
                        tracing::warn!(error = %e, "no se pudo entregar un evento a la interfaz");
                    }
                }
            });

            // Meetings recorded before the index existed — or while it was
            // missing — become searchable without the user doing anything. It
            // runs in the background because a large library takes a moment and
            // nothing else waits on it.
            engine.sync_memory();

            // The browser receiver must be ready as soon as the window appears.
            // Preparing or moving models on an external drive must not delay the
            // local port.
            let web_engine = engine.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = web_engine.start_web_ingest().await {
                    tracing::warn!(%error, "no se pudo escuchar audio de reuniones web");
                }
            });

            // Consolidate weights from previous locations and connect Discord.
            // A fresh installation chooses its first model in the guide, so
            // startup must not silently commit it to the default Q5 download.
            // Cross-volume moves must not block the window or browser receiver.
            tauri::async_runtime::spawn(async move {
                if let Err(error) = engine.prepare_model_storage().await {
                    tracing::warn!(%error, "no se pudieron consolidar los modelos de Whisper");
                }

                // A configured instance connects automatically so opening Kuali
                // is enough to make it ready for calls.
                if auto_connect {
                    if let Err(e) = engine.connect().await {
                        tracing::error!(error = %e, "no se pudo conectar con Discord al arrancar");
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::app_version,
            commands::set_config,
            commands::missing_requirements,
            commands::get_snapshot,
            commands::connect,
            commands::disconnect,
            commands::leave_call,
            commands::webhook_channels,
            commands::test_webhook,
            commands::list_meetings,
            commands::search_meetings,
            commands::load_meeting,
            commands::meeting_index_status,
            commands::reindex_meeting,
            commands::ask_meetings,
            commands::questions_status,
            commands::prepare_questions,
            commands::discard_question_data,
            commands::list_tasks,
            commands::open_main_window,
            commands::close_tray_panel,
            commands::quit_app,
            commands::list_guilds,
            commands::list_folders,
            commands::create_folder,
            commands::rename_folder,
            commands::delete_folder,
            commands::set_meeting_folder,
            commands::set_meeting_tags,
            commands::list_tags,
            commands::delete_meeting,
            commands::delete_meetings,
            commands::delete_channel_meetings,
            commands::set_task_done,
            commands::resummarize,
            commands::export_meeting,
            commands::available_providers,
            commands::provider_catalog,
            commands::test_provider,
            commands::provider_models,
            commands::whisper_models,
            commands::resolved_models_directory,
            commands::choose_models_directory,
            commands::download_model,
            commands::cancel_model_download,
            commands::delete_model,
            commands::reveal_data_dir,
            commands::browser_extension_path,
            commands::reveal_browser_extension,
            commands::open_setup_destination,
            commands::open_discord_install,
            commands::open_browser_extension_store,
            commands::open_browser_extensions,
            commands::autostart_enabled,
            commands::set_autostart_enabled,
            commands::check_for_update,
            commands::install_update,
            commands::factory_reset,
            commands::take_factory_reset_completed,
        ])
        .build(tauri::generate_context!())
        .expect("Kuali no pudo arrancar")
        .run(|_app, _event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = _event
            {
                let _ = has_visible_windows;
                let main_visible = _app
                    .get_webview_window("main")
                    .and_then(|window| window.is_visible().ok())
                    .unwrap_or(false);
                if should_restore_main_window(main_visible) {
                    reveal_main_window(_app);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{panel_origin, should_restore_main_window, tray_follow_copy};

    #[test]
    fn dock_reopen_restores_only_when_every_window_is_hidden() {
        assert!(should_restore_main_window(false));
        assert!(!should_restore_main_window(true));
    }

    #[test]
    fn the_panel_stays_on_the_screen_that_holds_the_tray_icon() {
        // Icon comfortably inside the screen: the panel is centered under it.
        let (x, y) = panel_origin(600.0, 24.0, 360.0, 0.0, 1440.0);
        assert_eq!(x, 420.0);
        assert_eq!(y, 32.0);

        // Icon near the right edge: the panel slides in instead of hanging off.
        let (x, _) = panel_origin(1430.0, 24.0, 360.0, 0.0, 1440.0);
        assert_eq!(x, 1072.0);

        // Second monitor to the right keeps its own bounds.
        let (x, _) = panel_origin(1450.0, 24.0, 360.0, 1440.0, 2880.0);
        assert_eq!(x, 1448.0);
    }

    #[test]
    fn tray_following_copy_always_exposes_state_and_next_action() {
        assert_eq!(
            tray_follow_copy(true, "es"),
            (
                "🟢 Discord · Seguimiento activo",
                "Pausar seguimiento de Discord"
            )
        );
        assert_eq!(
            tray_follow_copy(false, "en"),
            ("🟠 Discord · Following paused", "Enable Discord following")
        );
    }
}
