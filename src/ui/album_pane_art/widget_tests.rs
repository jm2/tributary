//! Widget-level contracts for the album-artwork browser row.
//!
//! Exposed as `pub mod widget_tests` so the crate's SINGLE consolidated
//! GTK test (`browser.rs`'s `gtk_widget_contracts_hold_on_one_session`)
//! can join these bodies. Never spawn a second GTK-initializing `#[test]`.
use super::*;

/// The album-artwork row must publish its combined album/count
/// accessible name on the `GtkListItem` — the list-row boundary —
/// and keep the thumbnail and text label presentational so the row
/// is announced as one utterance rather than duplicate child
/// announcements (2026-09-12 review finding, matching
/// `bind_browser_row` / `unbind_browser_row`).
pub fn album_art_row_publishes_combined_accessible_name() {
    let cell = AlbumArtCell::new(FALLBACK_PLACEHOLDER_ICON);
    let list_item: gtk::ListItem = glib::Object::new();
    list_item.set_child(Some(&cell.row));

    assert_eq!(
        cell.label.accessible_role(),
        gtk::AccessibleRole::Presentation,
        "the album text label must be presentational"
    );
    assert_eq!(
        cell.image.accessible_role(),
        gtk::AccessibleRole::Presentation,
        "the album thumbnail must be presentational"
    );

    let item = BrowserItem::new("Kind of Blue", 9);
    bind_album_row_accessibility(&list_item, &item);
    assert_eq!(
        list_item.accessible_label(),
        "Kind of Blue, (9)",
        "the combined album/count name belongs on the GtkListItem boundary"
    );

    unbind_album_row_accessibility(&list_item);
    assert_eq!(
        list_item.accessible_label(),
        "",
        "unbind must clear the accessible name so a recycled row is not stale"
    );
}

/// A zero-count album row (only the synthetic "All" row when the
/// library is empty) announces the bare label, never a meaningless
/// "(0)"; mirrors the zero-count browser-row contract.
pub fn album_art_row_zero_count_announces_bare_label() {
    let list_item: gtk::ListItem = glib::Object::new();
    let item = BrowserItem::all_row(0);
    bind_album_row_accessibility(&list_item, &item);
    assert_eq!(list_item.accessible_label(), "All");
}
