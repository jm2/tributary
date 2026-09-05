//! The GTK storage panel: a header quota line plus one accessible row per
//! board entry. Construction requires a GTK display context, so tests only
//! exercise the pure plans in the parent module.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::{plan, quota_text, truncated, OfflineRowAction, OfflineRowPlan};
use crate::offline::engine::{OfflineBoard, OfflineRowSnapshot};

/// The GTK storage panel: a header quota line plus one accessible row per
/// board entry. Construction requires a GTK display context, so tests only
/// exercise the pure plans above.
pub struct OfflineStoragePanel {
    root: gtk::Box,
    quota_label: gtk::Label,
    list: gtk::ListBox,
    action_handler: Rc<RefCell<dyn FnMut(usize, OfflineRowAction)>>,
}

// The application window embeds this panel with the follow-up wiring
// slice (source-lifecycle → engine → panel). Until then the renderer is
// exercised only through construction in downstream slices and tests, so
// the bin-root dead-code lint is silenced locally rather than forcing a
// premature window hookup.
#[allow(dead_code)]
impl OfflineStoragePanel {
    /// Build the panel. `action_handler` receives the board-row index and
    /// the activated action; wiring to the engine lives with the caller.
    #[must_use]
    pub fn new(action_handler: impl FnMut(usize, OfflineRowAction) + 'static) -> Self {
        let action_handler: Rc<RefCell<dyn FnMut(usize, OfflineRowAction)>> =
            Rc::new(RefCell::new(action_handler));
        let quota_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption"])
            .build();
        quota_label.set_accessible_role(gtk::AccessibleRole::Status);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        root.append(&quota_label);
        root.append(&list);
        root.set_accessible_role(gtk::AccessibleRole::Group);
        Self {
            root,
            quota_label,
            list,
            action_handler,
        }
    }

    /// The panel widget for embedding.
    #[must_use]
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Re-render the panel from a board. Rows are rebuilt in board order so
    /// the action handler's index always matches the current board.
    pub fn render(&self, board: &OfflineBoard, locale: &str) {
        self.quota_label.set_text(&quota_text(board, locale));
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for (index, row) in board.rows.iter().enumerate() {
            let row_plan = plan(row, locale);
            let handler = Rc::clone(&self.action_handler);
            let list_row = Self::build_row(&row_plan, row, locale, move |action| {
                (handler.borrow_mut())(index, action);
            });
            self.list.append(&list_row);
        }
    }

    fn build_row(
        row_plan: &OfflineRowPlan,
        row: &OfflineRowSnapshot,
        locale: &str,
        on_action: impl Fn(OfflineRowAction) + 'static,
    ) -> gtk::ListBoxRow {
        let grid = Self::row_grid();
        let title_label = Self::title_label(Self::heading(row));
        grid.append(&title_label);
        grid.append(&Self::status_line(row_plan, locale));
        if let Some(fraction) = row_plan.progress_fraction {
            grid.append(&Self::progress_bar(fraction));
        }
        if let Some(action) = row_plan.primary_action {
            grid.append(&Self::action_button(action, locale, on_action));
        }
        let list_row = gtk::ListBoxRow::builder().child(&grid).build();
        list_row.update_property(&[gtk::accessible::Property::Label(&row_plan.accessible_label)]);
        list_row
    }

    /// The vertical box one row renders into.
    fn row_grid() -> gtk::Box {
        gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build()
    }

    /// "Title — Artist", or title only when the artist label is empty.
    fn heading(row: &OfflineRowSnapshot) -> String {
        if row.labels.artist.is_empty() {
            truncated(&row.labels.title)
        } else {
            format!(
                "{} — {}",
                truncated(&row.labels.title),
                truncated(&row.labels.artist)
            )
        }
    }

    /// The heading label: start-aligned, ellipsized, single line.
    fn title_label(heading: String) -> gtk::Label {
        gtk::Label::builder()
            .label(heading)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .max_width_chars(48)
            .build()
    }

    /// Status strip: presentation-only icon and spinner (when the plan asks
    /// for them) plus the localized status text in an accessible Status role.
    fn status_line(row_plan: &OfflineRowPlan, locale: &str) -> gtk::Box {
        let status_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        if let Some(icon_name) = row_plan.icon_name {
            let icon = gtk::Image::builder().icon_name(icon_name).build();
            icon.set_accessible_role(gtk::AccessibleRole::Presentation);
            status_line.append(&icon);
        }
        if row_plan.show_spinner {
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.set_accessible_role(gtk::AccessibleRole::Presentation);
            status_line.append(&spinner);
        }
        let status_text = row_plan
            .status_key
            .map(|key| rust_i18n::t!(key, locale = locale).into_owned())
            .unwrap_or_default();
        let status_label = gtk::Label::builder()
            .label(status_text)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        status_label.set_accessible_role(gtk::AccessibleRole::Status);
        status_line.append(&status_label);
        status_line
    }

    /// Determinate progress bar in the accessible ProgressBar role.
    fn progress_bar(fraction: f64) -> gtk::ProgressBar {
        let bar = gtk::ProgressBar::builder()
            .fraction(fraction)
            .show_text(false)
            .hexpand(true)
            .build();
        bar.set_accessible_role(gtk::AccessibleRole::ProgressBar);
        bar
    }

    /// Flat action button wired to the row's action closure.
    fn action_button(
        action: OfflineRowAction,
        locale: &str,
        on_action: impl Fn(OfflineRowAction) + 'static,
    ) -> gtk::Button {
        let button = gtk::Button::builder()
            .label(action.label(locale))
            .halign(gtk::Align::Start)
            .css_classes(["flat"])
            .build();
        button.connect_clicked(move |_| on_action(action));
        button
    }
}
