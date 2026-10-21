use crate::model::Level;
use tauri_winrt_notification::Toast;

/// Windows toast. `app_id` must be a registered AUMID (the default is PowerShell's), or
/// Windows drops the toast silently.
pub fn toast(app_id: &str, level: Level, text: &str) {
    let title = match level {
        Level::Alert => "Router alert",
        Level::Warn => "Router warning",
        _ => "Router",
    };
    let _ = Toast::new(app_id).title(title).text1(text).show();
}
