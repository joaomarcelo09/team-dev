//! jg-team-dev-desktop: the jg-team-dev chat window.

mod app;
mod board;
mod hub;
mod icons;
mod style;
mod view;

use jgt_core::{HubClient, api::HealthResponse};

const MAX_LOG_SIZE_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

/// Truncates the log file if its size exceeds `max_bytes`.
pub(crate) fn rotate_if_needed(path: &std::path::Path, max_bytes: u64) {
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.len() > max_bytes
    {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path);
    }
}

/// Installs a tracing subscriber appending to `~/.local/share/jg-team-dev/logs/desktop.log`.
pub(crate) fn init_logging() {
    let log_dir = jgt_core::paths::data_dir().join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("Failed to create log directory {}: {e}", log_dir.display());
    }

    let log_file_path = log_dir.join("desktop.log");
    rotate_if_needed(&log_file_path, MAX_LOG_SIZE_BYTES);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("jgt_desktop=info,jgt_voice=debug"));

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
    {
        Ok(file) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(file)
                .with_ansi(false)
                .try_init();
        }
        Err(e) => {
            eprintln!("Failed to open log file {}: {e}", log_file_path.display());
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .try_init();
        }
    }
}

fn main() -> iced::Result {
    let mut args = std::env::args().skip(1);
    let mut session = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => session = args.next(),
            "--version" | "-V" => {
                println!("jg-team-dev-desktop {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => {}
        }
    }

    init_logging();
    tracing::info!("jg-team-dev-desktop starting (version {})", env!("CARGO_PKG_VERSION"));

    if focus_running_instance(session.clone()) {
        return Ok(());
    }

    iced::application(move || app::App::new(session.clone()), app::App::update, view::view)
        .font(include_bytes!("../assets/material-symbols-outlined.ttf").as_slice())
        .default_font(style::ui_font())
        .title(app::App::title)
        .theme(app::App::theme)
        .scale_factor(app::App::scale_factor)
        .subscription(app::App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1180.0, 780.0),
            min_size: Some(iced::Size::new(760.0, 480.0)),
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "jg-team-dev".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}

/// Only one window: when the app is already open, bring it to the front instead.
fn focus_running_instance(session: Option<String>) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return false };
    rt.block_on(async {
        let client = HubClient::default().with_timeout(std::time::Duration::from_secs(2));
        match client.get::<HealthResponse>("/v1/health").await {
            Ok(health) if health.ui_connected => client
                .post::<_, serde_json::Value>("/v1/ui/focus", &serde_json::json!({ "session_id": session }))
                .await
                .is_ok(),
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rotate_if_needed_truncates_large_file() {
        let temp_dir = std::env::temp_dir().join(format!("jgt-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let log_path = temp_dir.join("desktop.log");

        // Write 100 bytes
        std::fs::write(&log_path, vec![b'a'; 100]).unwrap();
        assert_eq!(std::fs::metadata(&log_path).unwrap().len(), 100);

        // Threshold 200: should NOT truncate
        rotate_if_needed(&log_path, 200);
        assert_eq!(std::fs::metadata(&log_path).unwrap().len(), 100);

        // Threshold 50: SHOULD truncate
        rotate_if_needed(&log_path, 50);
        assert_eq!(std::fs::metadata(&log_path).unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
