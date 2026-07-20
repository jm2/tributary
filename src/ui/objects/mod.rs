//! GObject wrappers for GTK4 list models.
//!
//! `gio::ListStore` requires `glib::Object` subclasses. These wrappers
//! bridge our plain Rust data structs to the GObject type system.

mod browser_item;
mod source_object;
mod track_object;

pub use browser_item::BrowserItem;
pub use source_object::{HeaderKind, PlaylistSidebarEntry, PlaylistSidebarKind, SourceObject};
pub use track_object::TrackObject;
pub use track_object::{
    PlaylistOccurrenceBinding, PlaylistOccurrenceState, PlaylistRowUnavailableReason,
};
