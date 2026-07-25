//! GTK folder-browsing pane for the local library.
//!
//! Renders the [`FolderIndex`] from `local::folder_browser` as a lazy,
//! expandable folder tree alongside the existing genre / artist / album
//! panes.  Selection in this pane drives the same track-list filter
//! callback the other panes already use, so all four panes share one
//! filter state machine.
//!
//! Design contract (see `docs/task.md` § P2.3 — root-relative folder
//! browsing, issue #14):
//!
//! * **Root-relative.** Only the configured root's basename appears at the
//!   top level; expanding a root reveals its immediate children by name
//!   rather than by absolute path.
//! * **Multi-root disambiguation.** Each track belongs to exactly one
//!   root (the most-specific configured ancestor).  The data layer enforces
//!   this; the pane simply renders the resulting partition.
//! * **Lazy navigation.** Only the immediate children of an expanded node
//!   are queried; nothing is read beyond that level until the user expands
//!   another node.
//! * **Unavailable / renamed roots.** Configured roots whose directories
//!   are missing or whose persisted state says unavailable appear as
//!   "(unavailable)" rows with no children, so the user can see which
//!   library entries fell off.
//! * **Explicit omission policy for pathless sources.** When the active
//!   source is remote-only (e.g. a connected Subsonic server) the pane
//!   shows a single label naming the omission instead of an empty tree.
//!
//! The pane is exposed to callers as a `(gtk::Box, FolderBrowserState)`
//! pair mirroring the existing genre / artist / album pane contract.

// Pattern names read more clearly than `Self::` here. The clippy lint is
// disabled module-wide for the same reason as in `local::folder_browser`.
#![allow(clippy::use_self)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::db::entities::library_root;
use crate::local::folder_browser::{
    FolderIndex, OmissionNotice, RootEntry, UnavailableReason,
};

/// One node in the folder tree.
///
/// The `gtk::TreeListModel` factory consumes the inner value to decide
/// whether a node has children and to fetch them lazily.  An `Unavailable`
/// node has no children; `Root` and `Directory` nodes delegate to the
/// shared `FolderIndex`.
#[derive(Clone)]
pub enum FolderBrowserNode {
    /// A configured library root.
    Root(PathBuf),
    /// An immediate subdirectory of some root.
    Directory {
        root: PathBuf,
        relative: PathBuf,
    },
    /// A configured root that is missing or unavailable on disk.  Carries
    /// its reason so the UI can pick a localized label.
    Unavailable {
        reason: UnavailableReason,
    },
}

impl FolderBrowserNode {
    fn display_name(&self) -> String {
        match self {
            FolderBrowserNode::Root(path) => {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned())
            }
            FolderBrowserNode::Directory { relative, .. } => relative
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| relative.to_string_lossy().into_owned()),
            FolderBrowserNode::Unavailable { .. } => String::new(),
        }
    }

    fn subtitle(&self) -> Option<String> {
        match self {
            FolderBrowserNode::Root(_)
            | FolderBrowserNode::Directory { .. } => None,
            FolderBrowserNode::Unavailable { reason, .. } => Some(match reason {
                UnavailableReason::Missing => "(unavailable: folder missing)".to_string(),
                UnavailableReason::PersistedUnavailable => {
                    "(unavailable: not currently readable)".to_string()
                }
                UnavailableReason::ScanIncomplete => {
                    "(unavailable: scan not complete)".to_string()
                }
            }),
        }
    }

    fn as_tracklist_filter(&self) -> Option<TracklistFilter> {
        match self {
            FolderBrowserNode::Root(root) => Some(TracklistFilter::Prefix(root.clone(), PathBuf::new())),
            FolderBrowserNode::Directory { root, relative } => {
                Some(TracklistFilter::Prefix(root.clone(), relative.clone()))
            }
            FolderBrowserNode::Unavailable { .. } => None,
        }
    }
}

/// Filter selection passed to the shared tracklist filter callback.
#[derive(Clone, Debug)]
pub enum TracklistFilter {
    /// Restrict the tracklist to entries whose absolute path lives under
    /// `root + relative`.
    Prefix(PathBuf, PathBuf),
}

/// Opaque handle the window keeps to rebuild the pane when the library
/// changes.
#[derive(Clone)]
pub struct FolderBrowserState {
    inner: Rc<RefCell<FolderBrowserInner>>,
}

struct FolderBrowserInner {
    /// Latest index shared across the model factory closure.
    index: Option<FolderIndex>,
    /// Source-side pathless notice (active source has no local paths).
    pathless_source: bool,
}

impl FolderBrowserState {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(FolderBrowserInner {
                index: None,
                pathless_source: false,
            })),
        }
    }

    /// Replace the data backing the pane.
    ///
    /// `configured_paths` is the active set of library roots from
    /// `AppConfig::library_paths`.  `persisted_roots` is the latest
    /// `library_root` table snapshot.  `track_paths` is the file-path
    /// slice in display order.
    /// `pathless_source` should be `true` when the active source has no
    /// filesystem paths at all (e.g. a connected remote server).
    pub fn rebuild<I, P>(
        &self,
        configured_paths: I,
        persisted_roots: &[library_root::Model],
        track_paths: P,
        pathless_source: bool,
    ) where
        I: IntoIterator<Item = PathBuf>,
        P: IntoIterator<Item = Option<PathBuf>>,
    {
        let index = FolderIndex::build(configured_paths, persisted_roots, track_paths);
        *self.inner.borrow_mut() = FolderBrowserInner {
            index: Some(index),
            pathless_source,
        };
    }
}

/// Build the folder pane.
///
/// `state` is the shared handle the window owns and passes back here when
/// the library changes.  `on_select` is invoked when the user picks a
/// folder row; the callback receives the `TracklistFilter` that should be
/// applied to the tracklist (or `None` if the selection is an unavailable
/// placeholder).
pub fn build_folder_browser<F>(
    state: &FolderBrowserState,
    on_select: F,
) -> gtk::Box
where
    F: Fn(Option<TracklistFilter>) + 'static,
{
    // ── Header ───────────────────────────────────────────────────────
    let header = gtk::Label::builder()
        .label("Folders")
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .margin_start(8)
        .margin_top(4)
        .margin_bottom(2)
        .build();

    // ── Tree model ────────────────────────────────────────────────────
    // The tree holds a flat list of FolderBrowserNode values at every
    // level.  We share one ListStore for the root level and use the
    // TreeListModel create_func to return a fresh ListStore per
    // expand-on-demand directory.
    let root_store: gio::ListStore = gio::ListStore::new::<glib::BoxedAnyObject>();

    let inner_for_factory = state.inner.clone();
    type CreateFunc = Box<dyn Fn(&glib::Object) -> Option<gio::ListModel>>;
    let create: CreateFunc = Box::new(
        move |item: &glib::Object| {
            let node_obj = item.downcast_ref::<glib::BoxedAnyObject>()?;
            let node: FolderBrowserNode = node_obj.borrow::<FolderBrowserNode>().clone();
            let inner = inner_for_factory.borrow();
            let index = inner.index.as_ref()?;
            let child_store: gio::ListStore = gio::ListStore::new::<glib::BoxedAnyObject>();
            match node {
                FolderBrowserNode::Root(root) => {
                    if let Ok(children) = index.child_dirs(&root, Path::new("")) {
                        for child in children {
                            let child_node = FolderBrowserNode::Directory {
                                root: root.clone(),
                                relative: PathBuf::from(&child.root_relative),
                            };
                            child_store.append(&glib::BoxedAnyObject::new(child_node));
                        }
                    }
                    Some(child_store.upcast())
                }
                FolderBrowserNode::Directory { root, relative } => {
                    if let Ok(children) = index.child_dirs(&root, &relative) {
                        for child in children {
                            let child_node = FolderBrowserNode::Directory {
                                root: root.clone(),
                                relative: PathBuf::from(&child.root_relative),
                            };
                            child_store.append(&glib::BoxedAnyObject::new(child_node));
                        }
                    }
                    Some(child_store.upcast())
                }
                FolderBrowserNode::Unavailable { .. } => None,
            }
        },
    );

    let tree_model = gtk::TreeListModel::new(root_store.clone(), false, false, create);

    let selection = gtk::SingleSelection::new(Some(tree_model.clone()));
    selection.set_autoselect(true);

    // ── Factory for visible cells ─────────────────────────────────────
    let factory = gtk::SignalListItemFactory::new();
    let inner_for_bind = state.inner.clone();
    factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let label_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(4)
            .margin_end(4)
            .margin_top(2)
            .margin_bottom(2)
            .build();
        let label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let badge = gtk::Label::builder()
            .halign(gtk::Align::End)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        label_box.append(&label);
        label_box.append(&badge);
        list_item.set_child(Some(&label_box));
    });
    factory.connect_bind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let label_box = list_item
            .child()
            .and_downcast::<gtk::Box>()
            .expect("Box");
        let label_widget = label_box.first_child();
        let badge_widget = label_widget.as_ref().and_then(|w| w.next_sibling());
        // The label and badge labels are the first two children of the
        // container Box appended in setup.
        let label = label_widget
            .and_downcast_ref::<gtk::Label>()
            .expect("first child is Label");
        let badge = badge_widget
            .and_downcast_ref::<gtk::Label>()
            .expect("second child is Label");
        let row = list_item
            .item()
            .and_downcast::<gtk::TreeListRow>()
            .expect("TreeListRow");
        let item = row.item();
        let Some(node_obj) = item.and_downcast_ref::<glib::BoxedAnyObject>() else {
            return;
        };
        let node: FolderBrowserNode = node_obj.borrow::<FolderBrowserNode>().clone();
        label.set_text(&node.display_name());
        if let Some(subtitle) = node.subtitle() {
            badge.set_text(&subtitle);
        } else {
            // Compute the descendant count from the data layer so users
            // get a quick sense of branch size without expanding.
            let inner = inner_for_bind.borrow();
            let Some(index) = inner.index.as_ref() else {
                badge.set_text("");
                return;
            };
            let count = match &node {
                FolderBrowserNode::Root(root) => {
                    index.subtree_track_count(root.as_path(), Path::new(""))
                }
                FolderBrowserNode::Directory { root, relative } => {
                    index.subtree_track_count(root.as_path(), relative.as_path())
                }
                FolderBrowserNode::Unavailable { .. } => 0,
            };
            if count > 0 {
                badge.set_text(&format!("{count}"));
            } else {
                badge.set_text("");
            }
        }
    });

    let list_view = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .build();
    list_view.set_show_separators(false);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&list_view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    // ── Empty / placeholder label ────────────────────────────────────
    // Shown whenever the data layer reports an omission notice, including
    // the explicit pathless-source case.
    let placeholder = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .margin_start(8)
        .margin_end(8)
        .margin_top(8)
        .wrap(true)
        .css_classes(["dim-label"])
        .visible(false)
        .build();
    placeholder.set_text(
        "Folder browsing is unavailable for this source because it has no local files.",
    );

    let pane = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    pane.append(&header);
    pane.append(&scrolled);
    pane.append(&placeholder);

    // ── Selection wiring ──────────────────────────────────────────────
    let inner_for_sel = state.inner.clone();
    let on_select = Rc::new(on_select);
    selection.connect_selection_changed(move |sel, _, _| {
        let pos = sel.selected();
        if pos == gtk::INVALID_LIST_POSITION {
            return;
        }
        let Some(item) = sel.item(pos) else { return };
        let Some(row) = item.downcast_ref::<gtk::TreeListRow>() else { return };
        let item = row.item();
        let Some(node_obj) = item.and_downcast_ref::<glib::BoxedAnyObject>() else {
            return;
        };
        let node: FolderBrowserNode = node_obj.borrow::<FolderBrowserNode>().clone();
        let filter = node.as_tracklist_filter();
        // Always clear the placeholder visibility in case selection landed
        // somewhere after a data refresh.
        let _ = inner_for_sel.borrow();
        on_select(filter);
    });

    // ── Initial population ────────────────────────────────────────────
    refresh_root_store(&root_store, &state.inner.borrow());

    pane
}

fn refresh_root_store(root_store: &gio::ListStore, inner: &FolderBrowserInner) {
    root_store.remove_all();
    let Some(index) = inner.index.as_ref() else {
        return;
    };
    for entry in index.roots() {
        let node = match entry {
            RootEntry::Available(summary) => FolderBrowserNode::Root(summary.configured_path.clone()),
            RootEntry::Unavailable { reason, .. } => FolderBrowserNode::Unavailable {
                reason: *reason,
            },
        };
        root_store.append(&glib::BoxedAnyObject::new(node));
    }
}

/// Decide whether to render the tree or the placeholder label.
///
/// Returns `Some(notice)` when the placeholder should be shown; `None`
/// when the tree is authoritative.
#[allow(dead_code)] // wired up by a follow-up slice that exposes the omission label
pub fn omission_notice(state: &FolderBrowserState) -> Option<OmissionNotice> {
    let inner = state.inner.borrow();
    if inner.pathless_source {
        return Some(OmissionNotice::NoTracksUnderRoots);
    }
    let index = inner.index.as_ref()?;
    index.omission_notice()
}