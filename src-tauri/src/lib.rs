pub mod domain;
pub mod listing;
pub mod platform;
pub mod runtime;
pub mod scan;
pub mod scheduler;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .run(tauri::generate_context!())
        .expect("error while running Directory UI Viewer");
}
