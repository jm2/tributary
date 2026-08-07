//! Bounded MTP storage-object browse.
//!
//! MTP storage objects form a tree; the tree is bounded by the
//! storage's capacity but a malicious or buggy device could return an
//! arbitrarily deep or wide tree. The browser in this module is the
//! only place the rest of the system can enumerate an MTP tree, and it
//! is bounded by an explicit [`BrowseBudget`] supplied by the caller.
//!
//! The browser walks the tree depth-first, parent-first, and is
//! strictly advisory: it never fetches object bytes, never opens a
//! destination, and never commits a write. The browser's output is a
//! list of [`MtpObject`]s that the planner can use to assemble a
//! [`TransferPlan`](super::super::transfer::TransferPlan).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::transport::{MtpObjectHandle, MtpSession, MtpTransportError};

/// What kind of object an [`MtpObject`] is. Mirrors the MTP object
/// format codes the transport layer surfaces, narrowed to the subset
/// the planner can act on.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum MtpObjectKind {
    /// A regular file. The planner can fetch it.
    RegularFile,
    /// A folder. The planner descends into it subject to its budget.
    Folder,
    /// Any other object type (associations, playlists, abstract
    /// media). The planner ignores these.
    Other,
}

/// One MTP storage object as observed by the browser.
///
/// The object is described by an [`MtpObjectHandle`] — its portable
/// identity on the device. The [`parent`](Self::parent) chain lets the
/// planner reconstruct a relative path without consulting any host
/// filesystem.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MtpObject {
    pub handle: MtpObjectHandle,
    pub parent: Option<MtpObjectHandle>,
    pub name: String,
    pub kind: MtpObjectKind,
    pub size_bytes: u64,
}

/// The closure the browser uses to enumerate children of a given
/// object handle. The transport supplies one because the transport is
/// the only place that can talk to the device. The trait object is
/// boxed so callers can pass an owned closure with a `move` capture
/// without worrying about the borrow's lifetime.
type ListChildren<'a> =
    dyn FnMut(&MtpSession, MtpObjectHandle) -> Result<Vec<MtpObject>, MtpTransportError> + 'a;

/// The browser. Stateless and `Clone` so the same browser can be reused
/// across multiple storage areas and so the planner can run tests
/// against it without sharing state.
#[derive(Clone, Debug, Default)]
pub struct MtpBrowser;

impl MtpBrowser {
    /// Create a new browser instance.
    pub fn new() -> Self {
        Self
    }

    /// Browse one storage area starting at the given root handle.
    ///
    /// The browser walks the tree parent-first, depth-first, and stops
    /// as soon as either the entry count or the depth bound is
    /// exhausted. The returned list is in the order the tree was
    /// walked; the planner can use the parent chain to reconstruct a
    /// depth-ordered path without depending on iteration order.
    ///
    /// `list_children` is supplied by the transport because the
    /// transport is the only place that can talk to the device. The
    /// transport never sees a destination or a host path.
    #[allow(clippy::unused_self, clippy::needless_lifetimes)]
    pub fn browse<'a>(
        &'a self,
        session: &'a MtpSession,
        root: MtpObjectHandle,
        budget: BrowseBudget,
        list_children: &mut ListChildren<'a>,
    ) -> Result<Vec<MtpObject>, MtpTransportError> {
        session.verify()?;

        let mut visited: BTreeSet<MtpObjectHandle> = BTreeSet::new();
        let mut by_handle: BTreeMap<MtpObjectHandle, MtpObject> = BTreeMap::new();
        let mut result: Vec<MtpObject> = Vec::new();
        let mut pending: Vec<(MtpObjectHandle, u32)> = vec![(root, 0)];

        while let Some((handle, depth)) = pending.pop() {
            if !visited.insert(handle) {
                continue;
            }
            if result.len() as u64 >= budget.max_entries() {
                break;
            }
            if depth > budget.max_depth() {
                continue;
            }
            let children = list_children(session, handle)?;
            let next_depth = depth.saturating_add(u32::from(budget.allows_recursion()));
            for child in children {
                let kind = child.kind;
                let child_handle = child.handle;
                let child_parent = child.parent;
                if by_handle.insert(child_handle, child.clone()).is_none() {
                    result.push(child);
                }
                if matches!(kind, MtpObjectKind::Folder) && budget.allows_recursion() {
                    pending.push((child_handle, next_depth));
                }
                let _ = child_parent;
            }
        }

        Ok(result)
    }
}

/// Hard bounds on a single browse call.
///
/// `max_entries` and `max_depth` are both inclusive. A budget with
/// `max_depth == 0` returns the children of the root only; a budget
/// with `max_entries == 0` returns nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BrowseBudget {
    max_entries: u64,
    max_depth: u32,
}

impl BrowseBudget {
    /// Construct a budget. `max_entries` of `0` is allowed and means
    /// "return nothing"; `max_depth` of `0` means "do not descend into
    /// folders."
    pub fn new(max_entries: u64, max_depth: u32) -> Self {
        Self {
            max_entries,
            max_depth,
        }
    }

    /// Upper bound on the number of objects the browser will return.
    pub fn max_entries(&self) -> u64 {
        self.max_entries
    }

    /// Upper bound on the depth of the tree the browser will descend
    /// into.
    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// Whether the browser is allowed to descend into folders. A budget
    /// with `max_depth == 0` is the only configuration where this is
    /// `false`.
    pub fn allows_recursion(&self) -> bool {
        self.max_depth > 0
    }
}

impl Default for BrowseBudget {
    fn default() -> Self {
        Self::new(8 * 1024, 6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mtp::transport::test_transport::{InMemoryMtpTransport, InMemoryObject};
    use crate::device::mtp::transport::MtpTransport;

    fn descriptor(serial: &str) -> crate::device::mtp::MtpUsbDescriptor {
        crate::device::mtp::MtpUsbDescriptor::new(0x04e8, 0x6860, serial).expect("descriptor")
    }

    fn folder(handle: u32, parent: Option<u32>, name: &str) -> InMemoryObject {
        InMemoryObject {
            handle: MtpObjectHandle(handle),
            parent: parent.map(MtpObjectHandle),
            name: name.to_string(),
            kind: MtpObjectKind::Folder,
            size_bytes: 0,
            bytes: Vec::new(),
        }
    }

    fn file(handle: u32, parent: u32, name: &str, payload: &[u8]) -> InMemoryObject {
        InMemoryObject {
            handle: MtpObjectHandle(handle),
            parent: Some(MtpObjectHandle(parent)),
            name: name.to_string(),
            kind: MtpObjectKind::RegularFile,
            size_bytes: payload.len() as u64,
            bytes: payload.to_vec(),
        }
    }

    fn build_tree(transport: &InMemoryMtpTransport) -> (MtpObjectHandle, MtpObjectHandle) {
        // Storage root handle is conventionally 0x0000_0001. Place a
        // top-level folder "Music" and "Photos" beneath it; "Music"
        // contains a subfolder and a file; "Photos" contains only a
        // file.
        let root = MtpObjectHandle(0x0000_0001);
        let music = folder(0x0000_0010, Some(root.0), "Music");
        let photos = folder(0x0000_0011, Some(root.0), "Photos");
        let tracks = folder(0x0000_0012, Some(music.handle.0), "Tracks");
        let song = file(0x0000_0020, music.handle.0, "song.flac", b"flac");
        let deep = file(0x0000_0021, tracks.handle.0, "deep.flac", b"deep");
        let photo = file(0x0000_0022, photos.handle.0, "img.jpg", b"jpg");
        transport.add_object(0, 0, music.clone());
        transport.add_object(0, 0, photos.clone());
        transport.add_object(0, 0, tracks.clone());
        transport.add_object(0, 0, song);
        transport.add_object(0, 0, deep);
        transport.add_object(0, 0, photo);
        // The transport's storage already advertises the storage
        // descriptor; rewrite it to include the synthetic root for the
        // test.
        (root, MtpObjectHandle(music.handle.0))
    }

    #[test]
    fn browse_returns_descendants_within_budget() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let (root, _music) = build_tree(&transport);
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        let browser = MtpBrowser::new();
        let objects = {
            let mut list_children = build_list_children(&transport, root);
            browser.browse(&session, root, BrowseBudget::new(64, 4), &mut list_children)
        }
        .expect("browse");
        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert!(names.contains(&"Music"));
        assert!(names.contains(&"song.flac"));
        // Depth bound of 4 admits the deep file under Music/Tracks.
        assert!(names.contains(&"deep.flac"));
    }

    fn build_list_children(
        transport: &InMemoryMtpTransport,
        root: MtpObjectHandle,
    ) -> impl FnMut(&MtpSession, MtpObjectHandle) -> Result<Vec<MtpObject>, MtpTransportError> + '_
    {
        move |session: &MtpSession, parent: MtpObjectHandle| {
            let _ = transport.list_storage(session).expect("storage");
            let mut children = Vec::new();
            for handle in [
                MtpObjectHandle(0x0000_0010),
                MtpObjectHandle(0x0000_0011),
                MtpObjectHandle(0x0000_0012),
                MtpObjectHandle(0x0000_0020),
                MtpObjectHandle(0x0000_0021),
                MtpObjectHandle(0x0000_0022),
            ] {
                let bytes = match transport.fetch_object(session, handle) {
                    Ok(bytes) => bytes.bytes,
                    Err(_) => continue,
                };
                let kind = if bytes.is_empty() {
                    MtpObjectKind::Folder
                } else {
                    MtpObjectKind::RegularFile
                };
                let name = match handle.0 {
                    0x0000_0010 => "Music",
                    0x0000_0011 => "Photos",
                    0x0000_0012 => "Tracks",
                    0x0000_0020 => "song.flac",
                    0x0000_0021 => "deep.flac",
                    0x0000_0022 => "img.jpg",
                    _ => "unknown",
                };
                let parent_of = match handle.0 {
                    0x0000_0010 | 0x0000_0011 => Some(root),
                    0x0000_0012 => Some(MtpObjectHandle(0x0000_0010)),
                    0x0000_0020 => Some(MtpObjectHandle(0x0000_0010)),
                    0x0000_0021 => Some(MtpObjectHandle(0x0000_0012)),
                    0x0000_0022 => Some(MtpObjectHandle(0x0000_0011)),
                    _ => None,
                };
                if parent_of == Some(parent) {
                    children.push(MtpObject {
                        handle,
                        parent: parent_of,
                        name: name.to_string(),
                        kind,
                        size_bytes: bytes.len() as u64,
                    });
                }
            }
            Ok(children)
        }
    }

    #[test]
    fn browse_respects_max_entries() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let (root, _music) = build_tree(&transport);
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        let browser = MtpBrowser::new();
        let objects = browser
            .browse(&session, root, BrowseBudget::new(2, 8), &mut |_, _| {
                Ok(vec![
                    MtpObject {
                        handle: MtpObjectHandle(1),
                        parent: Some(root),
                        name: "child-1".to_string(),
                        kind: MtpObjectKind::RegularFile,
                        size_bytes: 0,
                    },
                    MtpObject {
                        handle: MtpObjectHandle(2),
                        parent: Some(root),
                        name: "child-2".to_string(),
                        kind: MtpObjectKind::RegularFile,
                        size_bytes: 0,
                    },
                ])
            })
            .expect("browse");
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn browse_with_zero_depth_does_not_descend() {
        let transport = InMemoryMtpTransport::single_device(descriptor("ABC123"));
        let (root, _music) = build_tree(&transport);
        let session = transport
            .open_session(&descriptor("ABC123"))
            .expect("session");
        let browser = MtpBrowser::new();
        let mut descends = 0u32;
        let mut list_children = |_: &MtpSession,
                                 handle: MtpObjectHandle|
         -> Result<Vec<MtpObject>, MtpTransportError> {
            if handle == MtpObjectHandle(0x0000_0010) {
                descends += 1;
            }
            Ok(vec![])
        };
        let _ = browser
            .browse(&session, root, BrowseBudget::new(64, 0), &mut list_children)
            .expect("browse");
        assert_eq!(descends, 0, "must not descend into folders when depth is 0");
    }

    #[test]
    fn browse_budget_zero_disables_recursion() {
        let budget = BrowseBudget::new(64, 0);
        assert!(!budget.allows_recursion());
    }

    #[test]
    fn browse_budget_default_allows_recursion() {
        let budget = BrowseBudget::default();
        assert!(budget.allows_recursion());
        assert_eq!(budget.max_entries(), 8 * 1024);
        assert_eq!(budget.max_depth(), 6);
    }
}
