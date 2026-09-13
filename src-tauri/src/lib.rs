pub mod commands;
pub mod domain;
pub mod listing;
pub mod platform;
pub mod runtime;
pub mod scan;
pub mod scheduler;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder
        .setup(commands::native::setup)
        .invoke_handler(tauri::generate_handler![
            commands::native::connect_client,
            commands::native::filesystem_request
        ]);
    builder
        .plugin(tauri_plugin_dialog::init())
        .run(tauri::generate_context!())
        .expect("error while running Directory UI Viewer");
}
