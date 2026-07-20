//! Local filesystem backend — tag parsing, scanning engine, MediaBackend impl.

pub mod backend;
pub mod engine;
pub mod playback_history;
pub mod playlist_io;
pub mod playlist_manager;
pub mod playlist_sidebar;
pub mod resolver;
mod root_authority;
pub mod server_playlist_runtime;
pub mod smart_rules;
pub mod tag_parser;
pub mod tag_writer;
