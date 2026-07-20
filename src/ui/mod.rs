//! UI module — GTK4 / libadwaita interface components.

pub mod album_art;
pub mod browser;
pub mod context_menu;
pub mod discovery_handler;
pub mod dummy_data;
pub mod header_bar;
mod library_commands;
pub mod objects;
pub mod open_files;
pub mod output_dialogs;
pub mod output_switch;
pub mod persistence;
pub mod playback;
pub mod playlist_actions;
pub mod playlist_editor;
pub mod playlist_projection;
pub mod preferences;
pub mod properties_dialog;
pub mod radio;
pub mod removable_media;
mod rhythmbox_migration;
pub mod root_trust;
pub mod server_dialogs;
mod server_playlist_recovery;
mod server_playlists;
pub mod sidebar;
pub mod source_connect;
pub mod source_navigation;
pub mod tracklist;
#[cfg(target_os = "windows")]
pub mod win32_snap;
pub mod window;
pub mod window_state;
