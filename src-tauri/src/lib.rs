mod commands;
mod db;
mod logic;
mod readme;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::list_projects,
            commands::list_tags,
            commands::dev_pick_dir,
            commands::scan_directory,
            commands::add_projects,
            commands::set_project_tags,
            commands::rename_tag,
            commands::delete_tag,
            commands::relink_project,
            commands::remove_project,
            commands::open_in_finder,
            commands::open_in_vscode,
            commands::open_in_iterm,
            commands::open_github,
            commands::read_readme,
            commands::save_readme,
            commands::generate_readme,
            commands::read_project_image,
            commands::open_external,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
