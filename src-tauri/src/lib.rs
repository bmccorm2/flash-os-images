pub mod diskutil;
pub mod flash;
pub mod image;
pub mod writer;

use diskutil::Disk;
use flash::FlashState;
use image::ImageInfo;
use std::path::PathBuf;
use tauri::{AppHandle, State};

fn str_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tauri::command]
async fn list_disks() -> Result<Vec<Disk>, String> {
    tauri::async_runtime::spawn_blocking(diskutil::list_external_disks)
        .await
        .map_err(str_err)?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn image_info(path: String) -> Result<ImageInfo, String> {
    ImageInfo::from_path(&PathBuf::from(path)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn start_flash(
    app: AppHandle,
    state: State<'_, FlashState>,
    image_path: String,
    disk_id: String,
) -> Result<(), String> {
    // Early, friendlier check; the helper re-checks as root.
    let id = disk_id.clone();
    let allowed = tauri::async_runtime::spawn_blocking(move || diskutil::is_allowed_target(&id))
        .await
        .map_err(str_err)?
        .map_err(|e| format!("{e:#}"))?;
    if !allowed {
        return Err(format!("{disk_id} is not an allowed target"));
    }
    flash::start(app, &state, &image_path, &disk_id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn cancel_flash(state: State<'_, FlashState>) -> Result<(), String> {
    flash::cancel(&state).map_err(|e| format!("{e:#}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(FlashState::default())
        .invoke_handler(tauri::generate_handler![
            list_disks,
            image_info,
            start_flash,
            cancel_flash
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
