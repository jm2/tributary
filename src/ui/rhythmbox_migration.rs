//! Explicit, preview-before-write Rhythmbox migration UI.
//!
//! GTK receives the bounded local-only report and opaque request produced by
//! the migration planner. It may display escaped source paths and playlist
//! names to the local user, but never logs them or places them in fixed error
//! events. Only the request token crosses back into the serialized library
//! command lane.
//!
//! Cancellation is cooperative while each document is captured in 64 KiB
//! chunks. The streaming parser and database planner are individually
//! non-interruptible once entered; a single-permit worker gate prevents those
//! bounded phases from stacking, and generation checks discard every late
//! result. A command admitted before shutdown is not cancelled: it remains in
//! the FIFO ahead of `Flush`, while shutdown only closes its retained dialog.

use std::cell::{Cell, RefCell};
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use gtk::glib;
use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sea_orm::{DatabaseConnection, SqlxSqliteConnector};
use tracing::warn;
use uuid::Uuid;

use crate::local::engine::LibraryCommand;
use crate::local::rhythmbox_import::{
    parse_rhythmbox_documents, RhythmboxImport, RhythmboxImportLimits,
};
use crate::local::rhythmbox_migration::{
    prepare_rhythmbox_migration, RhythmboxMigrationCompletion, RhythmboxMigrationPolicy,
    RhythmboxMigrationReport, RhythmboxMigrationRequest, RhythmboxMigrationSourceDocument,
    RhythmboxMigrationSummary, RhythmboxParserIssueReason, RhythmboxPlaylistNameConflictReason,
    RhythmboxRatingConflictPolicy, RhythmboxRatingConflictResolution, RhythmboxRootRemap,
    RhythmboxUnsupportedPlaylistReason,
};

use super::library_commands::LibraryCommandAdmission;

const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
static PREVIEW_WORKER_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Default)]
struct MigrationUiState {
    generation: u64,
    pending_request: Option<Uuid>,
    cancellation: Option<Arc<AtomicBool>>,
    applying_dialog: Option<(Uuid, adw::AlertDialog)>,
}

std::thread_local! {
    /// Tributary has one GTK application window. Keeping this state on the GTK
    /// thread makes generation changes and command admission one uninterrupted
    /// producer turn, without a `Send` wrapper around GTK objects.
    static MIGRATION_UI_STATE: RefCell<MigrationUiState> = const {
        RefCell::new(MigrationUiState {
            generation: 0,
            pending_request: None,
            cancellation: None,
            applying_dialog: None,
        })
    };
}

fn begin_generation() -> Option<u64> {
    let (generation, stale_dialog) = MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.pending_request.is_some() {
            return (None, None);
        }
        if let Some(cancellation) = state.cancellation.take() {
            cancellation.store(true, Ordering::Release);
        }
        let stale_dialog = state.applying_dialog.take().map(|(_, dialog)| dialog);
        state.generation = state.generation.wrapping_add(1);
        state.cancellation = Some(Arc::new(AtomicBool::new(false)));
        (Some(state.generation), stale_dialog)
    });
    if let Some(dialog) = stale_dialog {
        dialog.force_close();
    }
    generation
}

fn generation_is_current(generation: u64) -> bool {
    MIGRATION_UI_STATE.with(|state| state.borrow().generation == generation)
}

fn invalidate_generation(generation: u64) {
    let dialog = MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.generation == generation {
            if let Some(cancellation) = state.cancellation.take() {
                cancellation.store(true, Ordering::Release);
            }
            state.generation = state.generation.wrapping_add(1);
            return state.applying_dialog.take().map(|(_, dialog)| dialog);
        }
        None
    });
    if let Some(dialog) = dialog {
        dialog.force_close();
    }
}

fn generation_cancellation(generation: u64) -> Option<Arc<AtomicBool>> {
    MIGRATION_UI_STATE.with(|state| {
        let state = state.borrow();
        if state.generation == generation {
            state.cancellation.as_ref().map(Arc::clone)
        } else {
            None
        }
    })
}

fn arm_request(generation: u64, request_id: Uuid) -> bool {
    MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.generation != generation || state.pending_request.is_some() {
            return false;
        }
        state.pending_request = Some(request_id);
        true
    })
}

fn retain_applying_dialog(generation: u64, request_id: Uuid, dialog: adw::AlertDialog) -> bool {
    MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.generation != generation || state.pending_request != Some(request_id) {
            return false;
        }
        state.applying_dialog = Some((request_id, dialog));
        true
    })
}

fn disarm_request(generation: u64, request_id: Uuid) {
    MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.generation == generation && state.pending_request == Some(request_id) {
            state.pending_request = None;
        }
    });
}

fn finish_request(request_id: Uuid) -> (bool, Option<adw::AlertDialog>) {
    MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.pending_request != Some(request_id) {
            return (false, None);
        }
        if let Some(cancellation) = state.cancellation.take() {
            cancellation.store(true, Ordering::Release);
        }
        state.pending_request = None;
        state.generation = state.generation.wrapping_add(1);
        let dialog = match state.applying_dialog.take() {
            Some((dialog_request_id, dialog)) if dialog_request_id == request_id => Some(dialog),
            Some(stale) => {
                state.applying_dialog = Some(stale);
                None
            }
            None => None,
        };
        (true, dialog)
    })
}

/// Revoke every chooser and unadmitted preview owned by the closing window.
///
/// Bounded XML capture observes cancellation between chunks; an already
/// entered parser/planner finishes behind the single-worker gate and its late
/// result is discarded. An already admitted command is deliberately not
/// cancelled: it stays ahead of the shutdown `Flush`. Only its inert applying
/// dialog and UI correlation state are removed here.
pub(super) fn cancel_pending() {
    let dialog = MIGRATION_UI_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(cancellation) = state.cancellation.take() {
            cancellation.store(true, Ordering::Release);
        }
        state.pending_request = None;
        state.generation = state.generation.wrapping_add(1);
        state.applying_dialog.take().map(|(_, dialog)| dialog)
    });
    if let Some(dialog) = dialog {
        dialog.force_close();
    }
}

/// Start a one-shot migration wizard.
///
/// Every result-affecting policy is chosen before the profile is read. A later
/// invocation supersedes an earlier unadmitted chooser, worker, or preview. An
/// admitted request owns the applying state until exact completion and blocks
/// a second wizard.
pub(super) fn start(
    parent: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    admission: &LibraryCommandAdmission,
    window_closing: Rc<Cell<bool>>,
) {
    if window_closing.get() {
        return;
    }
    if !admission.is_open() {
        show_fixed_alert(
            parent,
            "rhythmbox_migration.unavailable_heading",
            "rhythmbox_migration.unavailable_body",
        );
        return;
    }

    let Some(generation) = begin_generation() else {
        show_fixed_alert(
            parent,
            "rhythmbox_migration.in_progress_heading",
            "rhythmbox_migration.in_progress_body",
        );
        return;
    };
    present_options(parent, rt_handle, admission, window_closing, generation);
}

#[allow(clippy::too_many_lines)]
fn present_options(
    parent: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    admission: &LibraryCommandAdmission,
    window_closing: Rc<Cell<bool>>,
    generation: u64,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("rhythmbox_migration.options_heading").as_ref())
        .body(rust_i18n::t!("rhythmbox_migration.options_body").as_ref())
        .close_response("cancel")
        .default_response("continue")
        .build();
    dialog.add_response(
        "cancel",
        rust_i18n::t!("rhythmbox_migration.cancel_action").as_ref(),
    );
    dialog.add_response(
        "continue",
        rust_i18n::t!("rhythmbox_migration.continue_action").as_ref(),
    );
    dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);

    let options = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(8)
        .margin_bottom(4)
        .margin_start(8)
        .margin_end(8)
        .build();

    let ratings = gtk::CheckButton::builder()
        .label(rust_i18n::t!("rhythmbox_migration.import_ratings_label").as_ref())
        .active(true)
        .halign(gtk::Align::Start)
        .build();
    let overwrite_ratings = gtk::CheckButton::builder()
        .label(rust_i18n::t!("rhythmbox_migration.overwrite_ratings_label").as_ref())
        .active(false)
        .halign(gtk::Align::Start)
        .margin_start(24)
        .build();
    let play_counts = gtk::CheckButton::builder()
        .label(rust_i18n::t!("rhythmbox_migration.import_play_counts_label").as_ref())
        .active(true)
        .halign(gtk::Align::Start)
        .build();
    let last_played = gtk::CheckButton::builder()
        .label(rust_i18n::t!("rhythmbox_migration.import_last_played_label").as_ref())
        .active(false)
        .halign(gtk::Align::Start)
        .build();
    options.append(&ratings);
    options.append(&overwrite_ratings);
    options.append(&play_counts);
    options.append(&last_played);

    let remap_description = gtk::Label::builder()
        .label(rust_i18n::t!("rhythmbox_migration.root_remap_description").as_ref())
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(54)
        .margin_top(6)
        .build();
    options.append(&remap_description);

    let remap_grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .hexpand(true)
        .build();
    let old_root = gtk::Entry::builder()
        .placeholder_text(rust_i18n::t!("rhythmbox_migration.old_root_placeholder").as_ref())
        .activates_default(true)
        .hexpand(true)
        .build();
    let current_root = gtk::Entry::builder()
        .placeholder_text(rust_i18n::t!("rhythmbox_migration.current_root_placeholder").as_ref())
        .activates_default(true)
        .hexpand(true)
        .build();
    let old_root_label = gtk::Label::builder()
        .label(rust_i18n::t!("rhythmbox_migration.old_root_label").as_ref())
        .halign(gtk::Align::End)
        .use_underline(true)
        .build();
    let current_root_label = gtk::Label::builder()
        .label(rust_i18n::t!("rhythmbox_migration.current_root_label").as_ref())
        .halign(gtk::Align::End)
        .use_underline(true)
        .build();
    old_root_label.set_mnemonic_widget(Some(&old_root));
    current_root_label.set_mnemonic_widget(Some(&current_root));
    remap_grid.attach(&old_root_label, 0, 0, 1, 1);
    remap_grid.attach(&old_root, 1, 0, 1, 1);
    remap_grid.attach(&current_root_label, 0, 1, 1, 1);
    remap_grid.attach(&current_root, 1, 1, 1, 1);
    options.append(&remap_grid);
    dialog.set_extra_child(Some(&options));

    let overwrite_for_toggle = overwrite_ratings.clone();
    ratings.connect_toggled(move |ratings| {
        overwrite_for_toggle.set_sensitive(ratings.is_active());
        if !ratings.is_active() {
            overwrite_for_toggle.set_active(false);
        }
    });

    let parent = parent.clone();
    let dialog_parent = parent.clone();
    let rt_handle = rt_handle.clone();
    let admission = admission.clone();
    let ratings_for_response = ratings.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "continue" {
            invalidate_generation(generation);
            return;
        }
        if window_closing.get() || !generation_is_current(generation) {
            return;
        }

        let Ok(policy) = policy_from_options(
            ratings_for_response.is_active(),
            overwrite_ratings.is_active(),
            play_counts.is_active(),
            last_played.is_active(),
            old_root.text().as_str(),
            current_root.text().as_str(),
        ) else {
            invalidate_generation(generation);
            show_fixed_alert(
                &parent,
                "rhythmbox_migration.invalid_roots_heading",
                "rhythmbox_migration.invalid_roots_body",
            );
            return;
        };
        present_profile_chooser(
            &parent,
            &rt_handle,
            &admission,
            window_closing.clone(),
            generation,
            policy,
        );
    });
    dialog.present(Some(&dialog_parent));
    let _ = ratings.grab_focus();
}

#[allow(clippy::fn_params_excessive_bools)] // Mirrors four independent user-visible checkboxes.
fn policy_from_options(
    import_ratings: bool,
    overwrite_ratings: bool,
    import_play_counts: bool,
    import_last_played: bool,
    old_root: &str,
    current_root: &str,
) -> Result<RhythmboxMigrationPolicy, ()> {
    let mut policy = RhythmboxMigrationPolicy::default();
    policy.import_ratings = import_ratings;
    policy.rating_conflicts = if import_ratings && overwrite_ratings {
        RhythmboxRatingConflictPolicy::UseRhythmbox
    } else {
        RhythmboxRatingConflictPolicy::KeepTributary
    };
    policy.import_play_counts = import_play_counts;
    policy.import_last_played = import_last_played;

    match (old_root.is_empty(), current_root.is_empty()) {
        (true, true) => {}
        (false, false) => {
            let remap =
                RhythmboxRootRemap::new(PathBuf::from(old_root), PathBuf::from(current_root))
                    .map_err(|_| ())?;
            policy = policy.with_root_remap(remap);
        }
        _ => return Err(()),
    }
    Ok(policy)
}

fn present_profile_chooser(
    parent: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    admission: &LibraryCommandAdmission,
    window_closing: Rc<Cell<bool>>,
    generation: u64,
    policy: RhythmboxMigrationPolicy,
) {
    let chooser = gtk::FileDialog::builder()
        .title(rust_i18n::t!("rhythmbox_migration.choose_folder_title").as_ref())
        .modal(true)
        .build();
    let parent = parent.clone();
    let rt_handle = rt_handle.clone();
    let admission = admission.clone();
    let chooser_parent = parent.clone();
    chooser.select_folder(
        Some(&chooser_parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if window_closing.get() || !generation_is_current(generation) {
                return;
            }
            let folder = match result {
                Ok(folder) => folder,
                Err(error) if error.matches(gtk::gio::IOErrorEnum::Cancelled) => {
                    invalidate_generation(generation);
                    return;
                }
                Err(_) => {
                    invalidate_generation(generation);
                    show_fixed_alert(
                        &parent,
                        "rhythmbox_migration.chooser_failed_heading",
                        "rhythmbox_migration.chooser_failed_body",
                    );
                    return;
                }
            };
            let Some(folder) = folder.path() else {
                invalidate_generation(generation);
                show_fixed_alert(
                    &parent,
                    "rhythmbox_migration.local_folder_heading",
                    "rhythmbox_migration.local_folder_body",
                );
                return;
            };
            start_preview_worker(
                &parent,
                &rt_handle,
                &admission,
                window_closing.clone(),
                generation,
                folder,
                policy.clone(),
            );
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn start_preview_worker(
    parent: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    admission: &LibraryCommandAdmission,
    window_closing: Rc<Cell<bool>>,
    generation: u64,
    folder: PathBuf,
    policy: RhythmboxMigrationPolicy,
) {
    let Some(cancellation) = generation_cancellation(generation) else {
        return;
    };
    let progress = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("rhythmbox_migration.preparing_heading").as_ref())
        .body(rust_i18n::t!("rhythmbox_migration.preparing_body").as_ref())
        .close_response("cancel")
        .build();
    progress.add_response(
        "cancel",
        rust_i18n::t!("rhythmbox_migration.cancel_action").as_ref(),
    );
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .halign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(4)
        .accessible_role(gtk::AccessibleRole::Status)
        .build();
    progress.set_extra_child(Some(&spinner));
    let closing_progress = Rc::new(Cell::new(false));
    let closing_for_response = closing_progress.clone();
    progress.connect_response(None, move |_, _| {
        if !closing_for_response.get() {
            invalidate_generation(generation);
        }
    });
    progress.present(Some(parent));

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    rt_handle.spawn(async move {
        let Ok(_worker) = PREVIEW_WORKER_GATE.acquire().await else {
            let _ = result_tx.send(Err(PreviewFailure::WorkerStopped));
            return;
        };
        if let Err(error) = check_cancelled(&cancellation) {
            let _ = result_tx.send(Err(error));
            return;
        }
        let cancellation_for_capture = Arc::clone(&cancellation);
        let captured = match tokio::task::spawn_blocking(move || {
            // The async semaphore above admits only one full capture, parser,
            // and planner. Superseded waiters observe cancellation before
            // opening another pair of large documents.
            check_cancelled(&cancellation_for_capture)?;
            let documents = capture_documents(
                &folder,
                RhythmboxImportLimits::default(),
                &cancellation_for_capture,
            )?;
            check_cancelled(&cancellation_for_capture)?;
            let import = parse_rhythmbox_documents(
                &documents.rhythmdb,
                documents.playlists.as_deref(),
                RhythmboxImportLimits::default(),
            )
            .map_err(|_| PreviewFailure::InvalidDocuments)?;
            check_cancelled(&cancellation_for_capture)?;
            Ok(import)
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(PreviewFailure::WorkerStopped),
        };

        let result = match captured {
            Ok(import) => prepare_preview(import, policy, &cancellation).await,
            Err(error) => Err(error),
        };
        let _ = result_tx.send(result);
    });

    let parent = parent.clone();
    let admission = admission.clone();
    glib::MainContext::default().spawn_local(async move {
        let result = result_rx
            .await
            .unwrap_or(Err(PreviewFailure::WorkerStopped));
        if window_closing.get() || !generation_is_current(generation) {
            return;
        }
        closing_progress.set(true);
        progress.force_close();
        match result {
            Ok(request) => {
                present_preview(&parent, &admission, window_closing, generation, request);
            }
            Err(failure) => {
                warn!(?failure, "Rhythmbox migration preview failed");
                invalidate_generation(generation);
                let (heading, body) = failure.localization_keys();
                show_fixed_alert(&parent, heading, body);
            }
        }
    });
}

async fn prepare_preview(
    import: RhythmboxImport,
    policy: RhythmboxMigrationPolicy,
    cancellation: &AtomicBool,
) -> Result<RhythmboxMigrationRequest, PreviewFailure> {
    check_cancelled(cancellation)?;
    let db = open_read_only_database().await?;
    let result = prepare_rhythmbox_migration(&db, import, policy)
        .await
        .map_err(|_| PreviewFailure::PreviewUnavailable);
    let _ = db.close().await;
    check_cancelled(cancellation)?;
    result
}

async fn open_read_only_database() -> Result<DatabaseConnection, PreviewFailure> {
    let data_dir = dirs::data_dir().ok_or(PreviewFailure::DatabaseUnavailable)?;
    let database_path = data_dir.join("tributary").join("library.db");
    if !database_path.is_file() {
        return Err(PreviewFailure::DatabaseUnavailable);
    }
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .read_only(true)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(DATABASE_BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|_| PreviewFailure::DatabaseUnavailable)?;
    Ok(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

fn present_preview(
    parent: &adw::ApplicationWindow,
    admission: &LibraryCommandAdmission,
    window_closing: Rc<Cell<bool>>,
    generation: u64,
    request: RhythmboxMigrationRequest,
) {
    if request.summary().already_applied {
        invalidate_generation(generation);
        show_fixed_alert(
            parent,
            "rhythmbox_migration.already_applied_heading",
            "rhythmbox_migration.already_applied_body",
        );
        return;
    }

    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("rhythmbox_migration.preview_heading").as_ref())
        .body(rust_i18n::t!("rhythmbox_migration.preview_body").as_ref())
        .content_width(760)
        .close_response("cancel")
        .default_response("apply")
        .build();
    dialog.add_response(
        "cancel",
        rust_i18n::t!("rhythmbox_migration.cancel_action").as_ref(),
    );
    dialog.add_response(
        "apply",
        rust_i18n::t!("rhythmbox_migration.apply_action").as_ref(),
    );
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);

    let preview_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(8)
        .margin_bottom(4)
        .margin_start(8)
        .margin_end(8)
        .build();
    for line in preview_lines(request.summary()) {
        let label = gtk::Label::builder()
            .label(&line)
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .max_width_chars(60)
            .build();
        preview_box.append(&label);
    }

    let report_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    for section in report_render_model(request.report()) {
        report_box.append(&report_section_widget(&section));
    }
    let report_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(260)
        .max_content_height(420)
        .propagate_natural_height(true)
        .child(&report_box)
        .build();
    preview_box.append(&report_scroll);

    let requires_acknowledgement = request.requires_acknowledgement();
    let acknowledgement = requires_acknowledgement.then(|| {
        let label = gtk::Label::builder()
            .label(rust_i18n::t!("rhythmbox_migration.acknowledgement_label").as_ref())
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(70)
            .build();
        let checkbox = gtk::CheckButton::builder()
            .child(&label)
            .halign(gtk::Align::Start)
            .build();
        preview_box.append(&checkbox);
        checkbox
    });
    dialog.set_response_enabled("apply", !requires_acknowledgement);
    if let Some(acknowledgement) = &acknowledgement {
        let dialog_for_acknowledgement = dialog.clone();
        acknowledgement.connect_toggled(move |checkbox| {
            dialog_for_acknowledgement.set_response_enabled("apply", checkbox.is_active());
        });
    }
    dialog.set_extra_child(Some(&preview_box));

    let dialog_parent = parent.clone();
    let parent = parent.clone();
    let admission = admission.clone();
    let request = Rc::new(RefCell::new(Some(request)));
    let acknowledgement_for_response = acknowledgement.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            invalidate_generation(generation);
            return;
        }
        if acknowledgement_for_response
            .as_ref()
            .is_some_and(|checkbox| !checkbox.is_active())
        {
            return;
        }
        if window_closing.get() || !generation_is_current(generation) || !admission.is_open() {
            invalidate_generation(generation);
            if !window_closing.get() {
                show_fixed_alert(
                    &parent,
                    "rhythmbox_migration.unavailable_heading",
                    "rhythmbox_migration.unavailable_body",
                );
            }
            return;
        }

        let Some(request) = request.borrow_mut().take() else {
            return;
        };
        let request_id = request.request_id();
        if !arm_request(generation, request_id) {
            return;
        }
        if !admission.try_send(LibraryCommand::ApplyRhythmboxMigration(Box::new(request))) {
            disarm_request(generation, request_id);
            invalidate_generation(generation);
            show_fixed_alert(
                &parent,
                "rhythmbox_migration.unavailable_heading",
                "rhythmbox_migration.unavailable_body",
            );
            return;
        }
        present_applying_dialog(&parent, generation, request_id);
    });
    dialog.present(Some(&dialog_parent));
}

fn preview_lines(summary: &RhythmboxMigrationSummary) -> [String; 5] {
    [
        rust_i18n::t!(
            "rhythmbox_migration.preview_valid_song_rows",
            valid_rows = summary.source_tracks,
            matched = summary.matched_tracks,
            unmatched = summary.unmatched_tracks,
            duplicates = summary.duplicate_track_locations
        )
        .into_owned(),
        rust_i18n::t!(
            "rhythmbox_migration.preview_track_changes",
            ratings = summary.ratings_to_update,
            rating_conflicts_kept = summary.rating_conflicts_kept,
            rating_conflicts_replaced = summary.rating_conflicts_replaced,
            play_counts = summary.play_counts_to_update,
            last_played = summary.last_played_to_update
        )
        .into_owned(),
        rust_i18n::t!(
            "rhythmbox_migration.preview_playlists",
            static_count = summary.static_playlists_to_create,
            automatic_count = summary.automatic_playlists_to_create,
            name_conflicts = summary.playlist_name_conflicts
        )
        .into_owned(),
        rust_i18n::t!(
            "rhythmbox_migration.preview_entries",
            matched = summary.playlist_entries_matched,
            unmatched = summary.playlist_entries_unmatched,
            invalid = summary.playlist_entries_invalid
        )
        .into_owned(),
        rust_i18n::t!(
            "rhythmbox_migration.preview_skipped",
            queues = summary.queues_skipped,
            unsupported = summary.unsupported_playlists,
            parser_issues = summary.parser_issues
        )
        .into_owned(),
    ]
}

fn present_applying_dialog(parent: &adw::ApplicationWindow, generation: u64, request_id: Uuid) {
    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("rhythmbox_migration.applying_heading").as_ref())
        .body(rust_i18n::t!("rhythmbox_migration.applying_body").as_ref())
        .can_close(false)
        .build();
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .halign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(4)
        .accessible_role(gtk::AccessibleRole::Status)
        .build();
    dialog.set_extra_child(Some(&spinner));
    if retain_applying_dialog(generation, request_id, dialog.clone()) {
        dialog.present(Some(parent));
    } else {
        dialog.force_close();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReportSectionKind {
    ParserIssues,
    UnmatchedTracks,
    DuplicateLocations,
    RatingConflicts,
    PlaylistNameConflicts,
    Queues,
    UnsupportedPlaylists,
    InvalidStaticOccurrences,
    UnmatchedPlaylistOccurrences,
}

const REPORT_SECTION_ORDER: [ReportSectionKind; 9] = [
    ReportSectionKind::ParserIssues,
    ReportSectionKind::UnmatchedTracks,
    ReportSectionKind::DuplicateLocations,
    ReportSectionKind::RatingConflicts,
    ReportSectionKind::PlaylistNameConflicts,
    ReportSectionKind::Queues,
    ReportSectionKind::UnsupportedPlaylists,
    ReportSectionKind::InvalidStaticOccurrences,
    ReportSectionKind::UnmatchedPlaylistOccurrences,
];

#[derive(Eq, PartialEq)]
struct ReportSectionModel {
    kind: ReportSectionKind,
    heading: String,
    rows: Vec<String>,
    omitted: usize,
}

// Heading and row strings can contain source names and paths; omitting them is
// the privacy boundary, not an incomplete diagnostic implementation.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for ReportSectionModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReportSectionModel")
            .field("kind", &self.kind)
            .field("row_count", &self.rows.len())
            .field("omitted", &self.omitted)
            .finish()
    }
}

#[allow(clippy::too_many_lines)] // Keep the nine contract-ordered report mappings contiguous.
fn report_render_model(report: &RhythmboxMigrationReport) -> Vec<ReportSectionModel> {
    let mut sections = Vec::with_capacity(REPORT_SECTION_ORDER.len());

    let parser = report.parser_issues();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::ParserIssues,
        heading: rust_i18n::t!("rhythmbox_migration.report.parser_issues_heading").into_owned(),
        rows: parser
            .details()
            .iter()
            .map(|detail| {
                let document = localized_parser_document(detail.document());
                let reason = rust_i18n::t!(parser_issue_reason_key(detail.reason()));
                detail.entry_ordinal().map_or_else(
                    || {
                        rust_i18n::t!(
                            "rhythmbox_migration.report.parser_issue_item",
                            document = document,
                            item = detail.item_ordinal(),
                            reason = reason
                        )
                        .into_owned()
                    },
                    |entry| {
                        rust_i18n::t!(
                            "rhythmbox_migration.report.parser_issue_entry",
                            document = document,
                            item = detail.item_ordinal(),
                            entry = entry,
                            reason = reason
                        )
                        .into_owned()
                    },
                )
            })
            .collect(),
        omitted: parser.omitted(),
    });

    let unmatched = report.unmatched_tracks();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::UnmatchedTracks,
        heading: rust_i18n::t!("rhythmbox_migration.report.unmatched_tracks_heading").into_owned(),
        rows: unmatched
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.unmatched_track",
                    ordinal = detail.source_ordinal(),
                    path = escaped_path(detail.path()),
                    reason = rust_i18n::t!("rhythmbox_migration.report.reason.unmatched_track")
                )
                .into_owned()
            })
            .collect(),
        omitted: unmatched.omitted(),
    });

    let duplicates = report.duplicate_locations();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::DuplicateLocations,
        heading: rust_i18n::t!("rhythmbox_migration.report.duplicate_locations_heading")
            .into_owned(),
        rows: duplicates
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.duplicate_location",
                    path = escaped_path(detail.path()),
                    count = detail.source_count(),
                    reason = rust_i18n::t!("rhythmbox_migration.report.reason.duplicate_location")
                )
                .into_owned()
            })
            .collect(),
        omitted: duplicates.omitted(),
    });

    let ratings = report.rating_conflicts();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::RatingConflicts,
        heading: rust_i18n::t!("rhythmbox_migration.report.rating_conflicts_heading").into_owned(),
        rows: ratings
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.rating_conflict",
                    path = escaped_path(detail.path()),
                    resolution = rust_i18n::t!(rating_resolution_key(detail.resolution()))
                )
                .into_owned()
            })
            .collect(),
        omitted: ratings.omitted(),
    });

    let name_conflicts = report.playlist_name_conflicts();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::PlaylistNameConflicts,
        heading: rust_i18n::t!("rhythmbox_migration.report.playlist_name_conflicts_heading")
            .into_owned(),
        rows: name_conflicts
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.playlist_name_conflict",
                    ordinal = detail.source_ordinal(),
                    name = escaped_name(detail.name()),
                    reason = rust_i18n::t!(playlist_name_conflict_reason_key(detail.reason()))
                )
                .into_owned()
            })
            .collect(),
        omitted: name_conflicts.omitted(),
    });

    let queues = report.queues();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::Queues,
        heading: rust_i18n::t!("rhythmbox_migration.report.queues_heading").into_owned(),
        rows: queues
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.queue",
                    ordinal = detail.source_ordinal(),
                    name = escaped_name(detail.name()),
                    entries = detail.entry_count(),
                    reason = rust_i18n::t!("rhythmbox_migration.report.reason.queue_skipped")
                )
                .into_owned()
            })
            .collect(),
        omitted: queues.omitted(),
    });

    let unsupported = report.unsupported_playlists();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::UnsupportedPlaylists,
        heading: rust_i18n::t!("rhythmbox_migration.report.unsupported_playlists_heading")
            .into_owned(),
        rows: unsupported
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.unsupported_playlist",
                    ordinal = detail.source_ordinal(),
                    name = escaped_name(detail.name()),
                    reason = rust_i18n::t!(unsupported_playlist_reason_key(detail.reason()))
                )
                .into_owned()
            })
            .collect(),
        omitted: unsupported.omitted(),
    });

    let invalid_occurrences = report.invalid_static_occurrences();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::InvalidStaticOccurrences,
        heading: rust_i18n::t!("rhythmbox_migration.report.invalid_occurrences_heading")
            .into_owned(),
        rows: invalid_occurrences
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.invalid_occurrence",
                    name = escaped_name(detail.playlist_name()),
                    entry = detail.entry_ordinal(),
                    reason = rust_i18n::t!("rhythmbox_migration.report.reason.invalid_occurrence")
                )
                .into_owned()
            })
            .collect(),
        omitted: invalid_occurrences.omitted(),
    });

    let unmatched_occurrences = report.unmatched_playlist_occurrences();
    sections.push(ReportSectionModel {
        kind: ReportSectionKind::UnmatchedPlaylistOccurrences,
        heading: rust_i18n::t!("rhythmbox_migration.report.unmatched_occurrences_heading")
            .into_owned(),
        rows: unmatched_occurrences
            .details()
            .iter()
            .map(|detail| {
                rust_i18n::t!(
                    "rhythmbox_migration.report.unmatched_occurrence",
                    name = escaped_name(detail.playlist_name()),
                    entry = detail.entry_ordinal(),
                    path = escaped_path(detail.path()),
                    reason =
                        rust_i18n::t!("rhythmbox_migration.report.reason.unmatched_occurrence")
                )
                .into_owned()
            })
            .collect(),
        omitted: unmatched_occurrences.omitted(),
    });

    debug_assert_eq!(
        sections
            .iter()
            .map(|section| section.kind)
            .collect::<Vec<_>>(),
        REPORT_SECTION_ORDER
    );
    sections
}

fn report_section_widget(section: &ReportSectionModel) -> gtk::Expander {
    let label = rust_i18n::t!(
        "rhythmbox_migration.report.section_label",
        section = section.heading.as_str(),
        shown = section.rows.len(),
        omitted = section.omitted
    );
    let expander = gtk::Expander::builder()
        .label(label.as_ref())
        .expanded(!section.rows.is_empty() || section.omitted != 0)
        .build();
    let rows = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(4)
        .margin_bottom(6)
        .margin_start(18)
        .build();
    if section.rows.is_empty() {
        rows.append(&report_row_label(
            rust_i18n::t!("rhythmbox_migration.report.none").as_ref(),
        ));
    } else {
        for row in &section.rows {
            rows.append(&report_row_label(row));
        }
    }
    if section.omitted != 0 {
        rows.append(&report_row_label(
            rust_i18n::t!(
                "rhythmbox_migration.report.omitted",
                count = section.omitted
            )
            .as_ref(),
        ));
    }
    expander.set_child(Some(&rows));
    expander
}

fn report_row_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .max_width_chars(86)
        .build()
}

fn localized_parser_document(document: RhythmboxMigrationSourceDocument) -> String {
    match document {
        RhythmboxMigrationSourceDocument::RhythmDb => {
            rust_i18n::t!("rhythmbox_migration.report.document.rhythmdb").into_owned()
        }
        RhythmboxMigrationSourceDocument::Playlists => {
            rust_i18n::t!("rhythmbox_migration.report.document.playlists").into_owned()
        }
    }
}

const fn parser_issue_reason_key(reason: RhythmboxParserIssueReason) -> &'static str {
    match reason {
        RhythmboxParserIssueReason::MissingLocation => {
            "rhythmbox_migration.report.reason.missing_location"
        }
        RhythmboxParserIssueReason::MalformedLocation => {
            "rhythmbox_migration.report.reason.malformed_location"
        }
        RhythmboxParserIssueReason::NonFileLocation => {
            "rhythmbox_migration.report.reason.non_file_location"
        }
        RhythmboxParserIssueReason::RemoteLocation => {
            "rhythmbox_migration.report.reason.remote_location"
        }
        RhythmboxParserIssueReason::LocationCredentials => {
            "rhythmbox_migration.report.reason.location_credentials"
        }
        RhythmboxParserIssueReason::LocationPort => {
            "rhythmbox_migration.report.reason.location_port"
        }
        RhythmboxParserIssueReason::LocationQuery => {
            "rhythmbox_migration.report.reason.location_query"
        }
        RhythmboxParserIssueReason::LocationFragment => {
            "rhythmbox_migration.report.reason.location_fragment"
        }
        RhythmboxParserIssueReason::NonAbsoluteLocation => {
            "rhythmbox_migration.report.reason.non_absolute_location"
        }
        RhythmboxParserIssueReason::NonUnicodeLocation => {
            "rhythmbox_migration.report.reason.non_unicode_location"
        }
        RhythmboxParserIssueReason::LocationContainsNul => {
            "rhythmbox_migration.report.reason.location_contains_nul"
        }
        RhythmboxParserIssueReason::LocationParentTraversal => {
            "rhythmbox_migration.report.reason.location_parent_traversal"
        }
        RhythmboxParserIssueReason::InvalidRating => {
            "rhythmbox_migration.report.reason.invalid_rating"
        }
        RhythmboxParserIssueReason::InvalidPlayCount => {
            "rhythmbox_migration.report.reason.invalid_play_count"
        }
        RhythmboxParserIssueReason::InvalidLastPlayed => {
            "rhythmbox_migration.report.reason.invalid_last_played"
        }
        RhythmboxParserIssueReason::UnsupportedEntryType => {
            "rhythmbox_migration.report.reason.unsupported_entry_type"
        }
        RhythmboxParserIssueReason::UnsupportedPlaylistType => {
            "rhythmbox_migration.report.reason.unsupported_playlist_type"
        }
    }
}

const fn rating_resolution_key(resolution: RhythmboxRatingConflictResolution) -> &'static str {
    match resolution {
        RhythmboxRatingConflictResolution::KeptTributary => {
            "rhythmbox_migration.report.reason.rating_kept"
        }
        RhythmboxRatingConflictResolution::ReplacedWithRhythmbox => {
            "rhythmbox_migration.report.reason.rating_replaced"
        }
    }
}

const fn playlist_name_conflict_reason_key(
    reason: RhythmboxPlaylistNameConflictReason,
) -> &'static str {
    match reason {
        RhythmboxPlaylistNameConflictReason::Empty => {
            "rhythmbox_migration.report.reason.playlist_name_empty"
        }
        RhythmboxPlaylistNameConflictReason::AlreadyExists => {
            "rhythmbox_migration.report.reason.playlist_name_exists"
        }
        RhythmboxPlaylistNameConflictReason::DuplicateInSource => {
            "rhythmbox_migration.report.reason.playlist_name_duplicate"
        }
    }
}

const fn unsupported_playlist_reason_key(
    reason: RhythmboxUnsupportedPlaylistReason,
) -> &'static str {
    match reason {
        RhythmboxUnsupportedPlaylistReason::UnsupportedSourceType => {
            "rhythmbox_migration.report.reason.unsupported_source_type"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticAttributes => {
            "rhythmbox_migration.report.reason.automatic_attributes"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticLimit => {
            "rhythmbox_migration.report.reason.automatic_limit"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticSort => {
            "rhythmbox_migration.report.reason.automatic_sort"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticQueryShape => {
            "rhythmbox_migration.report.reason.automatic_query_shape"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticBooleanShape => {
            "rhythmbox_migration.report.reason.automatic_boolean_shape"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticPredicate => {
            "rhythmbox_migration.report.reason.automatic_predicate"
        }
        RhythmboxUnsupportedPlaylistReason::AutomaticRatingSemantics => {
            "rhythmbox_migration.report.reason.automatic_rating_semantics"
        }
    }
}

fn escaped_name(name: &str) -> String {
    if name.is_empty() {
        rust_i18n::t!("rhythmbox_migration.report.empty_name").into_owned()
    } else {
        escape_private_text(name)
    }
}

fn escaped_path(path: &Path) -> String {
    path.to_str().map_or_else(
        || rust_i18n::t!("rhythmbox_migration.report.non_unicode_path").into_owned(),
        escape_private_text,
    )
}

fn escape_private_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() || is_bidi_control(character) {
            use std::fmt::Write as _;

            write!(escaped, "\\u{{{:04X}}}", u32::from(character))
                .expect("writing to a String cannot fail");
        } else {
            escaped.push(character);
        }
    }
    escaped
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            // Include the six deprecated directional/shaping controls after
            // the isolate range as well; they remain invisible and should not
            // be able to alter surrounding report text.
            | '\u{2066}'..='\u{206F}'
    )
}

/// Present a closed completion state only for the most recently admitted
/// request. Stale events from an older invocation are intentionally ignored.
pub(super) fn handle_finished(
    parent: &adw::ApplicationWindow,
    request_id: Uuid,
    outcome: RhythmboxMigrationCompletion,
    summary: &RhythmboxMigrationSummary,
) {
    let (matched, applying_dialog) = finish_request(request_id);
    if !matched {
        return;
    }
    if let Some(dialog) = applying_dialog {
        dialog.force_close();
    }

    let (heading, body) = match outcome {
        RhythmboxMigrationCompletion::Applied => (
            rust_i18n::t!("rhythmbox_migration.completed_heading").into_owned(),
            rust_i18n::t!(
                "rhythmbox_migration.completed_body",
                matched = summary.matched_tracks,
                ratings = summary.ratings_to_update,
                play_counts = summary.play_counts_to_update,
                last_played = summary.last_played_to_update,
                playlists = summary
                    .static_playlists_to_create
                    .saturating_add(summary.automatic_playlists_to_create)
            )
            .into_owned(),
        ),
        RhythmboxMigrationCompletion::AppliedRefreshFailed => (
            rust_i18n::t!("rhythmbox_migration.completed_heading").into_owned(),
            rust_i18n::t!("rhythmbox_migration.completed_refresh_failed_body").into_owned(),
        ),
        RhythmboxMigrationCompletion::AlreadyApplied => (
            rust_i18n::t!("rhythmbox_migration.already_applied_heading").into_owned(),
            rust_i18n::t!("rhythmbox_migration.already_applied_body").into_owned(),
        ),
        RhythmboxMigrationCompletion::Stale => (
            rust_i18n::t!("rhythmbox_migration.stale_heading").into_owned(),
            rust_i18n::t!("rhythmbox_migration.stale_body").into_owned(),
        ),
        RhythmboxMigrationCompletion::Failed => (
            rust_i18n::t!("rhythmbox_migration.failed_heading").into_owned(),
            rust_i18n::t!("rhythmbox_migration.failed_body").into_owned(),
        ),
    };
    show_alert(parent, &heading, &body);
}

fn show_fixed_alert(parent: &adw::ApplicationWindow, heading_key: &str, body_key: &str) {
    let heading = rust_i18n::t!(heading_key);
    let body = rust_i18n::t!(body_key);
    show_alert(parent, heading.as_ref(), body.as_ref());
}

fn show_alert(parent: &adw::ApplicationWindow, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .close_response("ok")
        .default_response("ok")
        .build();
    dialog.add_response(
        "ok",
        rust_i18n::t!("rhythmbox_migration.ok_action").as_ref(),
    );
    dialog.present(Some(parent));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewFailure {
    RequiredDocumentMissing,
    DocumentUnavailable,
    DocumentTooLarge,
    DocumentChanged,
    InvalidDocuments,
    DatabaseUnavailable,
    PreviewUnavailable,
    WorkerStopped,
}

impl PreviewFailure {
    const fn localization_keys(self) -> (&'static str, &'static str) {
        match self {
            Self::RequiredDocumentMissing => (
                "rhythmbox_migration.missing_document_heading",
                "rhythmbox_migration.missing_document_body",
            ),
            Self::DocumentUnavailable => (
                "rhythmbox_migration.documents_unavailable_heading",
                "rhythmbox_migration.documents_unavailable_body",
            ),
            Self::DocumentTooLarge => (
                "rhythmbox_migration.documents_too_large_heading",
                "rhythmbox_migration.documents_too_large_body",
            ),
            Self::DocumentChanged => (
                "rhythmbox_migration.documents_changed_heading",
                "rhythmbox_migration.documents_changed_body",
            ),
            Self::InvalidDocuments => (
                "rhythmbox_migration.invalid_documents_heading",
                "rhythmbox_migration.invalid_documents_body",
            ),
            Self::DatabaseUnavailable => (
                "rhythmbox_migration.database_unavailable_heading",
                "rhythmbox_migration.database_unavailable_body",
            ),
            Self::PreviewUnavailable => (
                "rhythmbox_migration.preview_failed_heading",
                "rhythmbox_migration.preview_failed_body",
            ),
            Self::WorkerStopped => (
                "rhythmbox_migration.worker_failed_heading",
                "rhythmbox_migration.worker_failed_body",
            ),
        }
    }
}

struct CapturedDocuments {
    rhythmdb: Vec<u8>,
    playlists: Option<Vec<u8>>,
}

fn capture_documents(
    folder: &Path,
    limits: RhythmboxImportLimits,
    cancellation: &AtomicBool,
) -> Result<CapturedDocuments, PreviewFailure> {
    check_cancelled(cancellation)?;
    let folder_stamp = profile_folder_stamp(folder)?;
    let mut rhythmdb = open_document(folder.join("rhythmdb.xml"), limits.max_rhythmdb_bytes, true)?
        .ok_or(PreviewFailure::RequiredDocumentMissing)?;
    let playlists_path = folder.join("playlists.xml");
    let mut playlists = open_document(playlists_path.clone(), limits.max_playlists_bytes, false)?;
    revalidate_profile_folder(folder, &folder_stamp)?;

    // The non-link directory and both direct children are stamped before
    // either child is accepted. Reading through retained file handles and
    // revalidating the directory plus both names prevents ordinary parent or
    // child replacement from silently combining source generations.
    let rhythmdb_bytes = read_open_document(&mut rhythmdb, cancellation)?;
    let playlists_bytes = playlists
        .as_mut()
        .map(|document| read_open_document(document, cancellation))
        .transpose()?;
    revalidate_open_document(&rhythmdb)?;
    if let Some(playlists) = &playlists {
        revalidate_open_document(playlists)?;
    } else {
        revalidate_absence(&playlists_path)?;
    }
    revalidate_profile_folder(folder, &folder_stamp)?;
    check_cancelled(cancellation)?;

    Ok(CapturedDocuments {
        rhythmdb: rhythmdb_bytes,
        playlists: playlists_bytes,
    })
}

struct OpenDocument {
    path: PathBuf,
    file: File,
    stamp: FileStamp,
    max_bytes: usize,
}

fn open_document(
    path: PathBuf,
    max_bytes: usize,
    required: bool,
) -> Result<Option<OpenDocument>, PreviewFailure> {
    let path_metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(PreviewFailure::RequiredDocumentMissing)
        }
        Err(_) => return Err(PreviewFailure::DocumentUnavailable),
    };
    validate_path_metadata(&path_metadata)?;
    enforce_length(path_metadata.len(), max_bytes)?;

    let file = File::open(&path).map_err(|_| PreviewFailure::DocumentUnavailable)?;
    let stamp = file_stamp(&file)?;
    enforce_length(stamp.length, max_bytes)?;
    if stamp.length != path_metadata.len() || Some(stamp.modified) != path_metadata.modified().ok()
    {
        return Err(PreviewFailure::DocumentChanged);
    }
    Ok(Some(OpenDocument {
        path,
        file,
        stamp,
        max_bytes,
    }))
}

fn read_open_document(
    document: &mut OpenDocument,
    cancellation: &AtomicBool,
) -> Result<Vec<u8>, PreviewFailure> {
    let initial_capacity = usize::try_from(document.stamp.length)
        .unwrap_or(document.max_bytes)
        .min(64 * 1024);
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut chunk = vec![0u8; 64 * 1024].into_boxed_slice();
    loop {
        check_cancelled(cancellation)?;
        let remaining = document
            .max_bytes
            .saturating_add(1)
            .saturating_sub(bytes.len());
        if remaining == 0 {
            return Err(PreviewFailure::DocumentTooLarge);
        }
        let chunk_length = remaining.min(chunk.len());
        let read = document
            .file
            .read(&mut chunk[..chunk_length])
            .map_err(|_| PreviewFailure::DocumentUnavailable)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > document.max_bytes {
            return Err(PreviewFailure::DocumentTooLarge);
        }
    }

    let after = file_stamp(&document.file)?;
    if after != document.stamp || u64::try_from(bytes.len()).ok() != Some(document.stamp.length) {
        return Err(PreviewFailure::DocumentChanged);
    }
    Ok(bytes)
}

fn revalidate_open_document(document: &OpenDocument) -> Result<(), PreviewFailure> {
    let final_path_metadata =
        std::fs::symlink_metadata(&document.path).map_err(|_| PreviewFailure::DocumentChanged)?;
    validate_path_metadata(&final_path_metadata).map_err(|_| PreviewFailure::DocumentChanged)?;
    if final_path_metadata.len() != document.stamp.length
        || final_path_metadata.modified().ok() != Some(document.stamp.modified)
    {
        return Err(PreviewFailure::DocumentChanged);
    }
    let final_file = File::open(&document.path).map_err(|_| PreviewFailure::DocumentChanged)?;
    if file_stamp(&final_file).map_err(|_| PreviewFailure::DocumentChanged)? != document.stamp {
        return Err(PreviewFailure::DocumentChanged);
    }
    Ok(())
}

fn revalidate_absence(path: &Path) -> Result<(), PreviewFailure> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(PreviewFailure::DocumentChanged),
    }
}

fn check_cancelled(cancellation: &AtomicBool) -> Result<(), PreviewFailure> {
    if cancellation.load(Ordering::Acquire) {
        Err(PreviewFailure::WorkerStopped)
    } else {
        Ok(())
    }
}

fn enforce_length(length: u64, max_bytes: usize) -> Result<(), PreviewFailure> {
    if length > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        Err(PreviewFailure::DocumentTooLarge)
    } else {
        Ok(())
    }
}

fn validate_path_metadata(metadata: &Metadata) -> Result<(), PreviewFailure> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PreviewFailure::DocumentUnavailable);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(PreviewFailure::DocumentUnavailable);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProfileFolderStamp {
    identity: FileIdentity,
}

fn profile_folder_stamp(path: &Path) -> Result<ProfileFolderStamp, PreviewFailure> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| PreviewFailure::DocumentUnavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PreviewFailure::DocumentUnavailable);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(PreviewFailure::DocumentUnavailable);
        }
    }
    Ok(ProfileFolderStamp {
        identity: profile_folder_identity(path, &metadata)?,
    })
}

fn revalidate_profile_folder(
    path: &Path,
    expected: &ProfileFolderStamp,
) -> Result<(), PreviewFailure> {
    let current = profile_folder_stamp(path).map_err(|_| PreviewFailure::DocumentChanged)?;
    if &current == expected {
        Ok(())
    } else {
        Err(PreviewFailure::DocumentChanged)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    identity: FileIdentity,
    length: u64,
    modified: SystemTime,
}

fn file_stamp(file: &File) -> Result<FileStamp, PreviewFailure> {
    let metadata = file
        .metadata()
        .map_err(|_| PreviewFailure::DocumentUnavailable)?;
    if !metadata.is_file() {
        return Err(PreviewFailure::DocumentUnavailable);
    }
    Ok(FileStamp {
        identity: file_identity(file)?,
        length: metadata.len(),
        modified: metadata
            .modified()
            .map_err(|_| PreviewFailure::DocumentUnavailable)?,
    })
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<FileIdentity, PreviewFailure> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|_| PreviewFailure::DocumentUnavailable)?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Matches the fallible Windows identity boundary.
fn profile_folder_identity(
    _path: &Path,
    metadata: &Metadata,
) -> Result<FileIdentity, PreviewFailure> {
    use std::os::unix::fs::MetadataExt;

    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    volume: u64,
    id: WindowsFileId,
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum WindowsFileId {
    Extended([u8; 16]),
    Legacy(u64),
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<FileIdentity, PreviewFailure> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO,
    };

    let handle = file.as_raw_handle() as HANDLE;
    let mut extended = MaybeUninit::<FILE_ID_INFO>::zeroed();
    // SAFETY: `file` owns a live handle and the output buffer has the exact
    // size and alignment required by `GetFileInformationByHandleEx`.
    let result = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            extended.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result != 0 {
        // SAFETY: the successful API call initialized the complete structure.
        let extended = unsafe { extended.assume_init() };
        if extended.VolumeSerialNumber != 0
            && !extended.FileId.Identifier.iter().all(|byte| *byte == 0)
        {
            return Ok(FileIdentity {
                volume: extended.VolumeSerialNumber,
                id: WindowsFileId::Extended(extended.FileId.Identifier),
            });
        }
    }

    // `FileIdInfo` is unavailable on some older Windows/filesystem combinations
    // and some providers return an unusable all-zero identity. Fall back to the
    // legacy stable identity before failing closed.
    let mut legacy = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a live handle and the output buffer is correctly
    // sized and aligned for the API to initialize it completely.
    let result = unsafe { GetFileInformationByHandle(handle, legacy.as_mut_ptr()) };
    if result == 0 {
        return Err(PreviewFailure::DocumentUnavailable);
    }
    // SAFETY: the successful API call initialized the complete structure.
    let legacy = unsafe { legacy.assume_init() };
    let id = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
    if legacy.dwVolumeSerialNumber == 0 || id == 0 {
        return Err(PreviewFailure::DocumentUnavailable);
    }
    Ok(FileIdentity {
        volume: u64::from(legacy.dwVolumeSerialNumber),
        id: WindowsFileId::Legacy(id),
    })
}

#[cfg(windows)]
fn profile_folder_identity(
    path: &Path,
    _metadata: &Metadata,
) -> Result<FileIdentity, PreviewFailure> {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let directory = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| PreviewFailure::DocumentUnavailable)?;
    file_identity(&directory)
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity;

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> Result<FileIdentity, PreviewFailure> {
    // Fail closed on a target where Tributary has no stable file-identity
    // implementation rather than weakening capture replacement detection.
    Err(PreviewFailure::DocumentUnavailable)
}

#[cfg(not(any(unix, windows)))]
fn profile_folder_identity(
    _path: &Path,
    _metadata: &Metadata,
) -> Result<FileIdentity, PreviewFailure> {
    Err(PreviewFailure::DocumentUnavailable)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    fn code_localization_keys() -> BTreeSet<String> {
        let prefix = concat!("rhythmbox", "_migration.");
        [
            include_str!("rhythmbox_migration.rs"),
            include_str!("header_bar.rs"),
        ]
        .into_iter()
        .flat_map(|source| {
            source.match_indices(prefix).filter_map(move |(start, _)| {
                let key: String = source[start..]
                    .chars()
                    .take_while(|character| {
                        character.is_ascii_lowercase()
                            || character.is_ascii_digit()
                            || matches!(character, '_' | '.')
                    })
                    .collect();
                (key.len() > prefix.len()
                    && !key.ends_with('.')
                    && key != concat!("rhythmbox_migration", ".rs"))
                .then_some(key)
            })
        })
        .collect()
    }

    fn flatten_catalog(
        prefix: &str,
        value: &serde_yaml::Value,
        output: &mut BTreeMap<String, String>,
        locale: &str,
    ) {
        match value {
            serde_yaml::Value::Mapping(mapping) => {
                for (key, value) in mapping {
                    let key = key
                        .as_str()
                        .unwrap_or_else(|| panic!("{locale} migration catalog key is not text"));
                    let key = if prefix.is_empty() {
                        key.to_owned()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    flatten_catalog(&key, value, output, locale);
                }
            }
            serde_yaml::Value::String(text) => {
                assert!(output.insert(prefix.to_owned(), text.clone()).is_none());
            }
            _ => panic!("{locale} migration catalog value for {prefix} is not text or a map"),
        }
    }

    fn migration_catalog(locale: &str) -> BTreeMap<String, String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("locales")
            .join(format!("{locale}.yml"));
        let yaml = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let root: serde_yaml::Value = serde_yaml::from_str(&yaml)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let section = root
            .get("rhythmbox_migration")
            .unwrap_or_else(|| panic!("{} has no migration catalog", path.display()));
        let mut catalog = BTreeMap::new();
        flatten_catalog("", section, &mut catalog, locale);
        catalog
            .into_iter()
            .map(|(key, value)| (format!("rhythmbox_migration.{key}"), value))
            .collect()
    }

    fn placeholders(text: &str) -> BTreeSet<&str> {
        let mut result = BTreeSet::new();
        let mut remainder = text;
        while let Some(start) = remainder.find("%{") {
            remainder = &remainder[start + 2..];
            let Some(end) = remainder.find('}') else {
                break;
            };
            result.insert(&remainder[..end]);
            remainder = &remainder[end + 1..];
        }
        result
    }

    #[test]
    fn root_remap_requires_both_absolute_distinct_roots() {
        #[cfg(unix)]
        let (old, new) = ("/old", "/new");
        #[cfg(windows)]
        let (old, new) = (r"C:\\old", r"C:\\new");

        assert!(policy_from_options(true, false, true, false, "", "").is_ok());
        assert!(policy_from_options(true, false, true, false, old, "").is_err());
        assert!(policy_from_options(true, false, true, false, old, old).is_err());
        assert!(policy_from_options(true, false, true, false, old, new).is_ok());
        assert!(policy_from_options(true, false, true, false, &format!(" {old}"), new).is_err());

        let old_with_space = format!("{old} ");
        let new_with_space = format!("{new} ");
        let exact = policy_from_options(true, false, true, false, &old_with_space, &new_with_space)
            .expect("valid roots retain intentional trailing spaces");
        assert_eq!(
            exact.root_remap().expect("exact remap").from(),
            Path::new(&old_with_space)
        );
    }

    #[test]
    fn capture_requires_the_primary_direct_child() {
        let folder = tempfile::tempdir().expect("temporary profile folder");
        assert_eq!(
            capture_documents(
                folder.path(),
                RhythmboxImportLimits::default(),
                &AtomicBool::new(false),
            )
            .err(),
            Some(PreviewFailure::RequiredDocumentMissing)
        );
    }

    #[test]
    fn capture_rejects_a_non_directory_or_replaced_profile_folder() {
        let parent = tempfile::tempdir().expect("temporary profile parent");
        let file = parent.path().join("not-a-folder");
        std::fs::write(&file, b"not a directory").expect("write non-directory fixture");
        assert_eq!(
            capture_documents(
                &file,
                RhythmboxImportLimits::default(),
                &AtomicBool::new(false),
            )
            .err(),
            Some(PreviewFailure::DocumentUnavailable)
        );

        let folder = parent.path().join("profile");
        std::fs::create_dir(&folder).expect("create original profile folder");
        let stamp = profile_folder_stamp(&folder).expect("stamp original profile folder");
        std::fs::rename(&folder, parent.path().join("old-profile"))
            .expect("move original profile folder");
        std::fs::create_dir(&folder).expect("replace profile folder");
        assert_eq!(
            revalidate_profile_folder(&folder, &stamp),
            Err(PreviewFailure::DocumentChanged)
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_rejects_a_symlinked_profile_folder() {
        use std::os::unix::fs::symlink;

        let profile = tempfile::tempdir().expect("temporary real profile folder");
        std::fs::write(
            profile.path().join("rhythmdb.xml"),
            b"<rhythmdb version=\"2.0\"/>",
        )
        .expect("write primary fixture");
        let parent = tempfile::tempdir().expect("temporary link parent");
        let linked = parent.path().join("profile-link");
        symlink(profile.path(), &linked).expect("create profile directory symlink");
        assert_eq!(
            capture_documents(
                &linked,
                RhythmboxImportLimits::default(),
                &AtomicBool::new(false),
            )
            .err(),
            Some(PreviewFailure::DocumentUnavailable)
        );
    }

    #[test]
    fn capture_applies_the_independent_document_byte_ceilings() {
        let folder = tempfile::tempdir().expect("temporary profile folder");
        std::fs::write(folder.path().join("rhythmdb.xml"), b"12345")
            .expect("write primary document");
        let limits = RhythmboxImportLimits {
            max_rhythmdb_bytes: 4,
            ..RhythmboxImportLimits::default()
        };
        assert_eq!(
            capture_documents(folder.path(), limits, &AtomicBool::new(false)).err(),
            Some(PreviewFailure::DocumentTooLarge)
        );
    }

    #[test]
    fn private_display_text_escapes_controls_and_bidi_without_altering_unicode() {
        let input = "Música\n\t\u{0000}\u{061C}\u{200E}\u{202E}\u{2067}\u{206F}終";
        assert_eq!(
            escape_private_text(input),
            "Música\\u{000A}\\u{0009}\\u{0000}\\u{061C}\\u{200E}\\u{202E}\\u{2067}\\u{206F}終"
        );
        assert_eq!(escape_private_text("Björk/東京.flac"), "Björk/東京.flac");
    }

    #[test]
    fn report_render_model_debug_is_count_only() {
        let private = "private/path/playlist";
        let model = ReportSectionModel {
            kind: ReportSectionKind::UnmatchedTracks,
            heading: private.to_string(),
            rows: vec![private.to_string()],
            omitted: 7,
        };
        let diagnostics = format!("{model:?}");
        assert!(diagnostics.contains("row_count: 1"));
        assert!(diagnostics.contains("omitted: 7"));
        assert!(!diagnostics.contains(private));
    }

    #[test]
    fn migration_catalogs_exactly_cover_code_keys_and_preserve_placeholders() {
        let expected = code_localization_keys();
        assert_eq!(
            expected.len(),
            125,
            "migration localization inventory changed"
        );
        let english = migration_catalog("en");
        assert_eq!(english.keys().cloned().collect::<BTreeSet<_>>(), expected);
        let language_neutral = BTreeSet::from([
            "rhythmbox_migration.ok_action",
            "rhythmbox_migration.old_root_placeholder",
            "rhythmbox_migration.current_root_placeholder",
            "rhythmbox_migration.report.document.rhythmdb",
            "rhythmbox_migration.report.document.playlists",
            "rhythmbox_migration.report.rating_conflict",
        ]);
        let locale_neutral = BTreeSet::from([
            ("nl", "rhythmbox_migration.report.parser_issue_item"),
            ("pt-BR", "rhythmbox_migration.report.parser_issue_item"),
            ("pt-BR", "rhythmbox_migration.report.playlist_name_conflict"),
            ("pt-BR", "rhythmbox_migration.report.unsupported_playlist"),
        ]);

        for locale in rust_i18n::available_locales!() {
            let catalog = migration_catalog(locale.as_ref());
            assert_eq!(
                catalog.keys().cloned().collect::<BTreeSet<_>>(),
                expected,
                "locale {locale}"
            );
            for key in &expected {
                let value = &catalog[key];
                assert!(!value.trim().is_empty(), "{locale}.{key} is empty");
                assert_eq!(
                    placeholders(value),
                    placeholders(&english[key]),
                    "{locale}.{key} placeholder mismatch"
                );
                if locale.as_ref() != "en"
                    && !language_neutral.contains(key.as_str())
                    && !locale_neutral.contains(&(locale.as_ref(), key.as_str()))
                {
                    assert_ne!(
                        value, &english[key],
                        "{locale}.{key} unexpectedly duplicates English"
                    );
                }
            }
        }
    }

    #[test]
    fn report_section_order_is_complete_and_contract_stable() {
        assert_eq!(REPORT_SECTION_ORDER.len(), 9);
        assert_eq!(
            REPORT_SECTION_ORDER,
            [
                ReportSectionKind::ParserIssues,
                ReportSectionKind::UnmatchedTracks,
                ReportSectionKind::DuplicateLocations,
                ReportSectionKind::RatingConflicts,
                ReportSectionKind::PlaylistNameConflicts,
                ReportSectionKind::Queues,
                ReportSectionKind::UnsupportedPlaylists,
                ReportSectionKind::InvalidStaticOccurrences,
                ReportSectionKind::UnmatchedPlaylistOccurrences,
            ]
        );
    }

    #[test]
    fn admitted_request_blocks_a_superseding_generation_until_completion() {
        cancel_pending();
        let generation = begin_generation().expect("start first generation");
        let request_id = Uuid::new_v4();
        assert!(arm_request(generation, request_id));
        assert!(begin_generation().is_none());

        let (matched, dialog) = finish_request(request_id);
        assert!(matched);
        assert!(dialog.is_none());
        assert!(begin_generation().is_some());
        cancel_pending();
    }
}
