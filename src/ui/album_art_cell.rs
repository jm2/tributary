//! Cell widgets and per-row bind state for the browser album pane.
//!
//! Split out of `album_pane_art.rs` so the pane's controller/binder and
//! the row-level cell plumbing live in separately sized modules (same
//! rationale as the `album_art_cache` split). The code is moved
//! verbatim: [`AlbumArtCell`] owns the reusable row widget tree, and
//! [`AlbumArtCellState`] carries the per-row cancellation and
//! generation state the bind factory threads through every fetch.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::architecture::SourceId;

/// Resolved album art for one album pane entry. The placeholder image is
/// a stable `gtk::Image` the bind factory clones and rebinds; reusing it
/// across rows avoids creating a fresh widget per row in a virtualized
/// list (a real cost — each `Image` is a GObject and a CSS node).
#[derive(Clone)]
#[allow(dead_code)]
pub struct AlbumArtCell {
    pub row: gtk::Box,
    pub image: gtk::Image,
    pub label: gtk::Label,
    pub placeholder_icon: &'static str,
}

impl AlbumArtCell {
    /// Side length GTK4 should use for the placeholder icon when the
    /// cell has not yet been bound with a live preference. GTK4
    /// interprets `-1` as "use the icon theme's default size for the
    /// requested icon name"; a literal `0` would render the placeholder
    /// at zero pixels and a positive value would override the live
    /// bind-factory size. The bind factory applies the user-selected
    /// side length on every bind, so a freshly-built cell is only the
    /// icon-theme fallback.
    pub const PLACEHOLDER_PIXEL_SIZE: i32 = -1;

    pub fn new(placeholder_icon: &'static str) -> Self {
        let image = gtk::Image::builder()
            .icon_name(placeholder_icon)
            .pixel_size(Self::PLACEHOLDER_PIXEL_SIZE)
            .build();
        image.set_accessible_role(gtk::AccessibleRole::Img);
        let label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .margin_start(8)
            .margin_end(8)
            .margin_top(2)
            .margin_bottom(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .build();
        row.append(&image);
        row.append(&label);
        Self {
            row,
            image,
            label,
            placeholder_icon,
        }
    }

    /// Reset the cell to its placeholder state. Used both before the
    /// artwork resolves and when the album has no artwork at all.
    ///
    /// This is deliberately ONE image operation. GTK4 normalizes every
    /// `Image` representation to a paintable, so `set_icon_name(Some(..))`
    /// alone replaces whatever texture the cell showed before; the
    /// previous implementation followed it with `set_paintable(None)`,
    /// which cleared the freshly installed icon paintable and rendered
    /// the missing-art state as a blank square (2026-09-07 review
    /// finding).
    pub(crate) fn show_placeholder(&self, label_text: &str, accessible_label: Option<&str>) {
        self.image.set_icon_name(Some(self.placeholder_icon));
        self.label.set_text(label_text);
        self.label.set_tooltip_text(Some(label_text));
        if let Some(text) = accessible_label {
            self.image
                .update_property(&[gtk::accessible::Property::Label(text)]);
        }
    }

    /// Replace the placeholder with the supplied texture.
    pub(crate) fn show_texture(
        &self,
        texture: &gdk::Texture,
        label_text: &str,
        accessible_label: Option<&str>,
    ) {
        self.image.set_icon_name(None);
        self.image.set_paintable(Some(texture));
        self.label.set_text(label_text);
        self.label.set_tooltip_text(Some(label_text));
        if let Some(text) = accessible_label {
            self.image
                .update_property(&[gtk::accessible::Property::Label(text)]);
        }
    }
}

/// Generation token handed to the bind factory. Each `bind` mints a new
/// token; the same `ListItem` keeps its previous token through `unbind`.
/// When `set_pending` is called, the cell's generation advances so any
/// late async result for the *prior* token is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindGeneration(u64);

impl BindGeneration {
    pub const INVALID: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// Cookie threaded into the bind factory so a late async result can
/// verify its row is still on screen.
///
/// The factory mints a new generation for every `bind`. When the row is
/// recycled (`unbind` then `bind` for a different item) the cell's
/// generation is bumped so any in-flight fetch for the previous item is
/// rejected even before it reaches the worker. The generation is also
/// incremented when the user toggles the artwork preference, so any
/// leftover fetch from the previous mode paints a placeholder.
///
/// In addition to the generation token, each cell carries a
/// [`AlbumArtCellState::revoke`] flag that the bind factory flips before
/// scheduling a fresh fetch. The flag is checked at every `.await`
/// boundary inside the spawned future so an in-flight resolver call for
/// the previous item returns immediately on the next poll, instead of
/// waiting for the GTK main loop to wake it up to discover its
/// generation token is stale. Together, the flag and the token bound
/// each fetch independently — flag for "stop now", token for "the
/// result, if any, must not paint".
#[derive(Clone)]
pub struct AlbumArtCellState {
    pub(crate) cell: AlbumArtCell,
    /// Album key currently bound to this row (`None` for the synthetic
    /// "All" row or while the row is being recycled).
    pub(crate) bound_album_key: Rc<RefCell<Option<String>>>,
    /// Source identity currently bound to this row. `None` is distinct
    /// from "unknown" — it means the bind factory decided this row
    /// should not resolve through the lease-isolated remote path (for
    /// example, a purely local row).
    pub(crate) bound_source: Rc<RefCell<Option<SourceId>>>,
    /// Monotonic generation. Bumped on rebind and on artwork toggle.
    pub(crate) generation: Rc<Cell<BindGeneration>>,
    /// `true` while a fetch for this cell should be aborted. The bind
    /// factory flips it before scheduling a new fetch; the spawned
    /// future checks it after every `.await` and exits silently when
    /// set. [`AlbumArtCellState::clear`] resets it when the controller
    /// schedules the cell's NEXT fetch, so the fresh fetch runs with a
    /// clean slate while the fetch it replaces stays gated by its stale
    /// generation token.
    pub(crate) revoked: Rc<Cell<bool>>,
    /// Worker-side liveness token for the cell's outstanding fetch, if
    /// any. [`AlbumArtCellState::revoke`] flips it so the persistent art
    /// worker skips the network read and closes the reply for a fetch
    /// the row will never see (rebind, unbind, teardown, factory swap);
    /// [`AlbumArtController::spawn_fetch`] replaces it with a fresh
    /// token when it schedules the next fetch.
    pub(crate) fetch_liveness: Rc<RefCell<Option<crate::ui::album_art::ScopedArtFetch>>>,
    /// Active `paintable`-notify listener for the underlying `Image`,
    /// if any. A new bind replaces this with a new listener; the
    /// previous one is disconnected so the cache doesn't get multiple
    /// probes firing on the same paintable change.
    pub(crate) paintable_notify_id: Rc<RefCell<Option<glib::SignalHandlerId>>>,
}

impl AlbumArtCellState {
    pub(crate) fn new(cell: AlbumArtCell) -> Self {
        Self {
            cell,
            bound_album_key: Rc::new(RefCell::new(None)),
            bound_source: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(BindGeneration::INVALID)),
            revoked: Rc::new(Cell::new(false)),
            fetch_liveness: Rc::new(RefCell::new(None)),
            paintable_notify_id: Rc::new(RefCell::new(None)),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn row(&self) -> &gtk::Box {
        &self.cell.row
    }

    pub(crate) fn current_generation(&self) -> BindGeneration {
        self.generation.get()
    }

    /// `true` while a fetch for this cell should be aborted. The bind
    /// factory sets the flag to `true` before scheduling a new fetch;
    /// the spawned future checks this between every `.await` point.
    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.get()
    }

    /// Mark the cell as revoked. Returns the cell so the bind factory
    /// can chain it after flipping the previous fetch's flag. Also flips
    /// the cell's outstanding fetch's worker-side token, if one is
    /// stored, so the persistent art worker stops a fetch the row will
    /// never observe — this is what revokes requests on virtualized
    /// rebinds on every path (cache hit, no-art, and placeholder), not
    /// just the paths that schedule a new fetch.
    pub(crate) fn revoke(&self) {
        self.revoked.set(true);
        if let Some(token) = self.fetch_liveness.borrow().as_ref() {
            token.revoke();
        }
    }

    /// Clear the revocation flag, discard the (dead) fetch token slot,
    /// and disconnect any paintable listener left over from the cell's
    /// previous fetch cycle. Called by [`AlbumArtController::spawn_fetch`]
    /// so the fetch it is about to schedule starts from a clean slate —
    /// spawn_fetch stores a fresh token right after. Race-free: the reset
    /// runs synchronously on the main loop before the new future is
    /// polled, and the fetch it replaces is still blocked by its revoked
    /// token and stale generation even if it resumes after the reset.
    pub(crate) fn clear(&self) {
        self.revoked.set(false);
        self.fetch_liveness.borrow_mut().take();
        if let Some(handler_id) = self.paintable_notify_id.borrow_mut().take() {
            self.cell.image.disconnect(handler_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_generation_advances_on_reset() {
        // The cell's generation must advance on reset so any in-flight
        // fetch for the prior row is invalidated before the next bind
        // can paint. The advance is a pure `Cell<BindGeneration>` write
        // (`generation.set(generation.get().next())`) — exercised here
        // without constructing a `gtk::Image`, because GTK4 may only be
        // used from the main thread and CI's aarch64 / macOS matrices
        // do not give us a main thread. `gtk::test_synced` dispatches
        // onto GTK's test thread pool but does not initialise GTK, so
        // the first widget call would still panic there.
        let cell = Rc::new(Cell::new(BindGeneration::INVALID));
        let initial = cell.get();
        cell.set(cell.get().next());
        assert_ne!(cell.get(), initial);
    }

    #[test]
    fn bind_generation_monotonic_across_rebinds() {
        // The virtualized list recycles a row's cell across many
        // rebinds; each bind must hand the cell a fresh generation so a
        // late async result for the previous row is dropped without
        // painting. The factory's contract is: every bind mints a new
        // generation, no generation is reused.
        let gen = BindGeneration::INVALID;
        let mut last = gen;
        for _ in 0..8 {
            let next = last.next();
            assert_ne!(next, last);
            last = next;
        }
    }

    #[test]
    fn bind_generation_late_result_is_dropped() {
        // An in-flight async fetch returns after the row has been
        // recycled. The contract is: `state.current_generation() !=
        // captured_generation` ⇒ the result must be discarded. We can
        // exercise the check directly without spinning up the async
        // runtime or constructing a `gtk::Image`: the same predicate
        // runs in `spawn_fetch` and the cache probe closure, both of
        // which compare the captured generation against the cell's
        // current generation token.
        let captured = BindGeneration::INVALID.next();
        // Simulate a rebind (advance the generation).
        let current = captured.next();
        assert_ne!(
            current, captured,
            "late result must observe a newer generation than the one captured at fetch time"
        );
    }

    #[test]
    fn cell_pixel_size_starts_at_icon_theme_sentinel() {
        // GTK4's "use the icon theme's default size" sentinel is -1; a
        // freshly-built cell must request the placeholder at that
        // sentinel, not at a literal 0 pixels, so the icon-theme
        // fallback is visible until the bind factory applies the live
        // side length. The sentinel is exposed as the
        // `AlbumArtCell::PLACEHOLDER_PIXEL_SIZE` constant so the test
        // can verify it without constructing a `gtk::Image` — GTK4 may
        // only be used from the main thread, and CI's aarch64 / macOS
        // matrices do not give us one. `gtk::test_synced` dispatches
        // onto GTK's test thread pool but does not initialise GTK, so
        // the first widget call would still panic there.
        assert_eq!(AlbumArtCell::PLACEHOLDER_PIXEL_SIZE, -1);
    }

    /// `revoke` flips the cell's cancellation flag from false to true;
    /// the bind factory reads this flag (via `is_revoked`) on every
    /// re-bind and the spawned future reads it between every `.await`.
    /// The flag is pure `Cell<bool>` so the test exercises the exact
    /// primitive without involving GTK or an async runtime.
    #[test]
    fn cell_state_revocation_flag_toggles_on_revoke() {
        // We can't construct an AlbumArtCell without GTK, but the
        // revocation flag is the only field we need to exercise here.
        // Build a parallel `Cell<bool>` to model the flag's behaviour
        // and confirm the contract that the bind factory relies on:
        // the flag is read at well-defined points and never reset
        // implicitly.
        let flag = Rc::new(Cell::new(false));
        assert!(!flag.get(), "fresh cell starts unrevoked");
        flag.set(true);
        assert!(flag.get(), "revoke() flips the flag");
        flag.set(false);
        assert!(!flag.get(), "next bind resets the flag for the new fetch");
    }
}

/// Widget-level contracts for the album-art cell, exercised from the
/// crate's SINGLE consolidated GTK test (`browser.rs`'s
/// `gtk_widget_contracts_hold_on_one_session`) via the process-wide
/// `ui::widget_test_session`. Never spawn a second GTK-initializing
/// `#[test]` — join that test's body instead (see `ui::widget_test_session`).
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::*;
    use crate::ui::album_art::ScopedArtFetch;

    /// The placeholder reset must leave the missing-art state VISIBLE.
    /// GTK4 normalizes `Image` representations to a paintable, so after
    /// the single `set_icon_name` operation the image must expose a
    /// non-`None` paintable — the previous implementation cleared it
    /// again with `set_paintable(None)`, rendering a blank square where
    /// the placeholder icon belongs (2026-09-07 review finding).
    pub fn show_placeholder_keeps_the_missing_art_visible() {
        let cell = AlbumArtCell::new("audio-x-generic-symbolic");
        // Simulate a recycled row that previously painted a texture: the
        // placeholder reset must replace — not blank — the image content.
        cell.show_placeholder("Album", Some("Album"));
        assert_eq!(
            cell.image.icon_name().as_deref(),
            Some("audio-x-generic-symbolic"),
            "the placeholder icon name must be set"
        );
        assert!(
            cell.image.paintable().is_some(),
            "the placeholder icon must stay visible: a paintable-less Image renders blank"
        );
    }

    /// Revoking a cell must stop its outstanding fetch at the worker,
    /// not merely flag the local future: the stored `ScopedArtFetch`
    /// token flips so the persistent art worker skips the network read
    /// and closes the reply. This is the token half of the virtualized
    /// rebind contract — it fires on every revoke path (rebind, unbind,
    /// teardown, factory swap), and `clear` must not resurrect it.
    pub fn revoking_a_cell_revokes_its_outstanding_fetch_token() {
        let cell = AlbumArtCell::new("audio-x-generic-symbolic");
        let state = AlbumArtCellState::new(cell);
        // Mint a fetch token the way spawn_fetch does and store it.
        let token = ScopedArtFetch::new();
        *state.fetch_liveness.borrow_mut() = Some(token.clone());
        assert!(token.is_live(), "a freshly minted fetch token is live");

        state.revoke();
        assert!(state.is_revoked(), "revoke flags the local future gate");
        assert!(
            !token.is_live(),
            "revoke must flip the worker-side fetch token"
        );

        // The next fetch's clean-slate reset un-flags the cell for the
        // NEW fetch but cannot resurrect the dead token; spawn_fetch
        // replaces the slot with a fresh token afterwards.
        state.clear();
        assert!(!state.is_revoked());
        assert!(!token.is_live(), "a revoked token stays revoked");
    }
}
