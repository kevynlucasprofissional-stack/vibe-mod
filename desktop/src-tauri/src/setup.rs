use crate::{
    cli::{self, is_cli_detected},
    config::STORE_FILENAME,
    diagnostics::{get_issue_url, DiagnosticsState},
    error::LogError,
    sona::SonaProcess,
};
use eyre::eyre;
use once_cell::sync::Lazy;
use std::fs;
use tauri::{App, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_store::StoreExt;
use tokio::sync::Mutex;

pub static STATIC_APP: Lazy<std::sync::Mutex<Option<tauri::AppHandle>>> = Lazy::new(|| std::sync::Mutex::new(None));

pub struct SonaState {
    pub process: Option<SonaProcess>,
    /// Engine reported by Sona metadata for the model currently loaded through
    /// the desktop GUI. `None` is kept for unknown/custom models and is treated
    /// conservatively as Whisper-compatible by the chunk-protection layer.
    pub model_engine: Option<String>,
}

pub fn setup(app: &App) -> Result<(), Box<dyn std::error::Error>> {
    let local_app_data_dir = app.path().app_local_data_dir()?;
    let app_config_dir = app.path().app_config_dir()?;
    fs::create_dir_all(&local_app_data_dir)
        .unwrap_or_else(|_| panic!("cant create local app data directory at {}", local_app_data_dir.display()));
    fs::create_dir_all(&app_config_dir)
        .unwrap_or_else(|_| panic!("cant create app config directory at {}", app_config_dir.display()));

    app.manage(Mutex::new(SonaState {
        process: None,
        model_engine: None,
    }));
    app.manage(crate::dictation_indicator::DictationIndicatorRuntime::default());

    let store = app.store(STORE_FILENAME)?;

    {
        let mut app_handle = STATIC_APP.lock().expect("lock");
        *app_handle = Some(app.handle().clone());
    }
    crate::logging::setup_logging(app.handle(), store).unwrap();

    // Structured diagnostics are initialized after tracing so every diagnostic
    // report can point back to the raw log for complementary evidence.
    app.manage(DiagnosticsState::new(app.handle())?);

    crate::cleaner::clean_old_logs(app.handle()).log_error();
    crate::cleaner::clean_old_diagnostics(app.handle()).log_error();
    crate::cleaner::clean_old_files().log_error();
    crate::cleaner::clean_updater_files().log_error();
    tracing::debug!("Vibe App Running");

    // Rust panics are recorded into every active diagnostic run before the
    // normal panic hook continues. try_lock is used inside diagnostics so a
    // panic while diagnostics itself is writing cannot deadlock the process.
    let previous_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = format!("Rust panic: {panic_info}");
        if let Ok(app_guard) = STATIC_APP.lock() {
            if let Some(app_handle) = app_guard.as_ref() {
                app_handle.state::<DiagnosticsState>().record_crash_best_effort(&message);
            }
        }
        previous_panic_hook(panic_info);
    }));

    let _handler = crash_handler::CrashHandler::attach(unsafe {
        crash_handler::make_crash_event(move |cc: &crash_handler::CrashContext| {
            #[cfg(windows)]
            let info = cc.exception_code;

            #[cfg(target_os = "macos")]
            let info = cc.exception;

            #[cfg(target_os = "linux")]
            let info = cc.siginfo;

            tracing::error!("Crash context: {:?}", info);

            if let Ok(app_guard) = STATIC_APP.lock() {
                if let Some(app_handle) = app_guard.as_ref() {
                    app_handle
                        .state::<DiagnosticsState>()
                        .record_crash_best_effort(&format!("Native crash context: {info:?}"));
                    app_handle
                        .dialog()
                        .message("App crashed. A diagnostic report was saved. Click Report to open the issue form.")
                        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                        .title("Vibe Crashed")
                        .buttons(MessageDialogButtons::OkCustom("Report".into()))
                        .show(|_| {});
                    let _ = tauri_plugin_opener::open_url(get_issue_url(format!("{info:?}")), None::<&str>);
                }
            }

            crash_handler::CrashEventResult::Handled(true)
        })
    });

    if let Ok(version) = tauri::webview_version() {
        tracing::debug!("webview version: {}", version);
    }

    #[cfg(windows)]
    {
        if let Err(error) = crate::custom_protocol::register() {
            tracing::error!("{:?}", error);
        }
    }

    tracing::debug!("AVX2: {}", crate::cmd::app::is_avx2_enabled());
    tracing::debug!("Executable Architecture: {}", std::env::consts::ARCH);
    tracing::debug!("APP VERSION: {}", app.package_info().version.to_string());
    tracing::debug!("COMMIT HASH: {}", env!("COMMIT_HASH"));
    tracing::debug!("App Info: {}", crate::diagnostics::get_app_info());

    let app_handle = app.app_handle().clone();
    if is_cli_detected() {
        tracing::debug!("CLI mode");
        tauri::async_runtime::spawn(async move {
            cli::run(&app_handle).await.map_err(|e| eyre!("{:?}", e)).log_error();
        });
    } else {
        tracing::debug!("Non CLI mode");
        let result = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
            .inner_size(800.0, 700.0)
            .min_inner_size(800.0, 700.0)
            .center()
            .title("Vibe")
            .resizable(true)
            .focused(true)
            .shadow(true)
            .visible(true)
            .build();
        if let Err(error) = result {
            tracing::error!("{:?}", error);
        }
        crate::dictation_indicator::initialize(app.handle());
    }
    Ok(())
}
