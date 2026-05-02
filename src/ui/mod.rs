//! UI module — GTK4 / libadwaita interface components.

pub mod album_art;
pub mod browser;
pub mod context_menu;
pub mod discovery_handler;
pub mod dummy_data;
pub mod header_bar;
pub mod objects;
pub mod open_files;
pub mod output_dialogs;
pub mod output_switch;
pub mod persistence;
pub mod playback;
pub mod playlist_actions;
pub mod playlist_editor;
pub mod preferences;
pub mod properties_dialog;
pub mod radio;
pub mod server_dialogs;
pub mod sidebar;
pub mod source_connect;
pub mod tracklist;
#[cfg(target_os = "windows")]
pub mod win32_snap;
pub mod window;
pub mod window_state;
